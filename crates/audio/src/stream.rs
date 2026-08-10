//! Opening devices and running the engine from the output callback.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, FromSample, Host, InputCallbackInfo, OutputCallbackInfo, SampleFormat, SizedSample,
    Stream, StreamConfig,
};
use free_loop_core::{Command, Event, Frames};
use free_loop_engine::{Engine, EventSink};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::config::{
    ASSUMED_BLOCK_FRAMES, AudioConfig, Negotiated, buffer_size, choose, cushion_frames, frames_in,
};
use crate::error::AudioError;
use crate::ring::{CaptureReader, CaptureWriter, ChannelMap, MAX_BLOCK_FRAMES, capture_ring};

/// Commands the control thread can queue before the audio thread drains them.
const COMMAND_SLOTS: usize = 256;
/// Reports the audio thread can queue before the control thread drains them.
const EVENT_SLOTS: usize = 4_096;

/// Devices the host can see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceList {
    /// Names of devices that can capture.
    pub inputs: Vec<String>,
    /// Names of devices that can play back.
    pub outputs: Vec<String>,
}

/// Lists the devices on the default host.
///
/// # Errors
///
/// [`AudioError::Cpal`] if the host cannot be enumerated.
pub fn list_devices() -> Result<DeviceList, AudioError> {
    let host = cpal::default_host();
    let mut list = DeviceList::default();

    for device in host.devices()? {
        let Ok(description) = device.description() else {
            continue;
        };
        let name = description.name().to_owned();
        if device.supports_input() {
            list.inputs.push(name.clone());
        }
        if device.supports_output() {
            list.outputs.push(name);
        }
    }
    Ok(list)
}

fn device_name(device: &Device) -> String {
    device
        .description()
        .map_or_else(|_| "<unnamed>".to_owned(), |d| d.name().to_owned())
}

fn find_device(host: &Host, wanted: Option<&str>, output: bool) -> Result<Device, AudioError> {
    let Some(wanted) = wanted else {
        let device = if output {
            host.default_output_device()
        } else {
            host.default_input_device()
        };
        return device.ok_or(AudioError::NoDevice(if output {
            "output"
        } else {
            "input"
        }));
    };

    let needle = wanted.to_lowercase();
    for device in host.devices()? {
        let usable = if output {
            device.supports_output()
        } else {
            device.supports_input()
        };
        if usable && device_name(&device).to_lowercase().contains(&needle) {
            return Ok(device);
        }
    }
    Err(AudioError::DeviceNotFound(wanted.to_owned()))
}

/// Devices chosen and configurations agreed, but not yet running.
///
/// Two phases because the engine has to be built for the negotiated sample rate and
/// channel count, which are only known once the devices have been inspected.
pub struct Opened {
    input: Device,
    output: Device,
    input_config: StreamConfig,
    output_config: StreamConfig,
    negotiated: Negotiated,
}

impl core::fmt::Debug for Opened {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Opened")
            .field("input", &device_name(&self.input))
            .field("output", &device_name(&self.output))
            .field("negotiated", &self.negotiated)
            .finish_non_exhaustive()
    }
}

/// Picks devices and settles on a configuration.
///
/// # Errors
///
/// [`AudioError`] if a named device is missing, a device offers nothing usable, or the
/// two devices cannot agree on a sample rate.
pub fn open(config: &AudioConfig) -> Result<Opened, AudioError> {
    let host = cpal::default_host();
    let output = find_device(&host, config.output_device.as_deref(), true)?;
    let input = find_device(&host, config.input_device.as_deref(), false)?;

    let output_ranges: Vec<_> = output.supported_output_configs()?.collect();
    let output_supported = choose(&output_ranges, config.sample_rate, config.channels)
        .ok_or(AudioError::NoUsableConfig("output"))?;

    // The input has to follow the output's rate; two rates cannot share one transport.
    let input_ranges: Vec<_> = input.supported_input_configs()?.collect();
    let input_supported = choose(&input_ranges, Some(output_supported.sample_rate()), None)
        .ok_or(AudioError::NoUsableConfig("input"))?;

    if input_supported.sample_rate() != output_supported.sample_rate() {
        return Err(AudioError::SampleRateMismatch {
            input: input_supported.sample_rate(),
            output: output_supported.sample_rate(),
        });
    }

    let negotiated = Negotiated {
        sample_rate: output_supported.sample_rate(),
        channels: usize::from(output_supported.channels()),
        input_channels: usize::from(input_supported.channels()),
        input_format: input_supported.sample_format(),
        output_format: output_supported.sample_format(),
        input_source: config.input_source,
        buffer_frames: config.buffer_frames,
        cushion_frames: cushion_frames(config.buffer_frames, config.cushion_blocks),
        capture_offset: config.capture_offset,
    };

    Ok(Opened {
        input_config: StreamConfig {
            channels: input_supported.channels(),
            sample_rate: negotiated.sample_rate,
            buffer_size: buffer_size(input_supported.buffer_size(), config.buffer_frames),
        },
        output_config: StreamConfig {
            channels: output_supported.channels(),
            sample_rate: negotiated.sample_rate,
            buffer_size: buffer_size(output_supported.buffer_size(), config.buffer_frames),
        },
        negotiated,
        input,
        output,
    })
}

impl Opened {
    /// What the two devices agreed on. Build the engine to match.
    pub fn negotiated(&self) -> Negotiated {
        self.negotiated
    }

    /// The name of the device being captured from.
    pub fn input_name(&self) -> String {
        device_name(&self.input)
    }

    /// The name of the device being played to.
    pub fn output_name(&self) -> String {
        device_name(&self.output)
    }

    /// Starts both streams, moving `engine` into the output callback.
    ///
    /// # Errors
    ///
    /// [`AudioError`] if either stream cannot be built or started.
    pub fn start(self, engine: Engine) -> Result<AudioIo, AudioError> {
        let negotiated = self.negotiated;
        let map = ChannelMap::new(
            negotiated.input_channels,
            negotiated.channels,
            negotiated.input_source,
        );

        let block = usize::try_from(negotiated.buffer_frames.unwrap_or(ASSUMED_BLOCK_FRAMES))
            .unwrap_or(usize::from(u16::MAX));
        let (writer, reader) = capture_ring(
            negotiated.cushion_frames + 4 * block,
            negotiated.cushion_frames,
            map,
        );

        let (commands, command_rx) = RingBuffer::new(COMMAND_SLOTS);
        let (event_tx, events) = RingBuffer::new(EVENT_SLOTS);

        let errors = Arc::new(AtomicU64::new(0));
        let input_latency = Arc::new(AtomicU32::new(0));
        let capture_offset = Arc::new(AtomicU32::new(0));

        let callback = Render {
            input_latency: Arc::clone(&input_latency),
            capture_offset: Arc::clone(&capture_offset),
            cushion: u32::try_from(negotiated.cushion_frames).unwrap_or(u32::MAX),
            offset_override: negotiated.capture_offset,
            sample_rate: negotiated.sample_rate,
            engine,
            reader,
            commands: command_rx,
            events: event_tx,
            captured: vec![0.0; MAX_BLOCK_FRAMES * negotiated.channels],
            rendered: vec![0.0; MAX_BLOCK_FRAMES * negotiated.channels],
            channels: negotiated.channels,
            dropped_events: 0,
        };

        let input = build_input(
            &self.input,
            self.input_config,
            negotiated.input_format,
            writer,
            Arc::clone(&errors),
            Arc::clone(&input_latency),
            negotiated.sample_rate,
        )?;
        let output = build_output(
            &self.output,
            self.output_config,
            negotiated.output_format,
            callback,
            Arc::clone(&errors),
        )?;

        input.play()?;
        output.play()?;

        Ok(AudioIo {
            input,
            output,
            commands,
            events,
            negotiated,
            errors,
            capture_offset,
        })
    }
}

/// Running streams.
///
/// `cpal::Stream` is not `Send` on every platform, so keep this on the thread that
/// started it. Dropping it stops both streams.
pub struct AudioIo {
    input: Stream,
    output: Stream,
    commands: Producer<Command>,
    events: Consumer<Event>,
    negotiated: Negotiated,
    errors: Arc<AtomicU64>,
    capture_offset: Arc<AtomicU32>,
}

impl core::fmt::Debug for AudioIo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioIo")
            .field("negotiated", &self.negotiated)
            .field("capture_offset_frames", &self.capture_offset_frames())
            .field("device_errors", &self.device_errors())
            .finish_non_exhaustive()
    }
}

impl AudioIo {
    /// What the two devices agreed on.
    pub fn negotiated(&self) -> Negotiated {
        self.negotiated
    }

    /// Queues a command for the engine.
    ///
    /// # Errors
    ///
    /// Returns the command back if the queue is full, which means the audio thread has
    /// stalled.
    pub fn send(&mut self, command: Command) -> Result<(), Command> {
        self.commands
            .push(command)
            .map_err(|rtrb::PushError::Full(c)| c)
    }

    /// Hands every queued report to `handler`.
    pub fn drain_events(&mut self, mut handler: impl FnMut(Event)) {
        while let Ok(event) = self.events.pop() {
            handler(event);
        }
    }

    /// The round-trip latency currently being compensated for, in frames.
    ///
    /// Zero until the first output callback has run, since it comes from what the driver
    /// reports rather than from a guess.
    pub fn capture_offset_frames(&self) -> u32 {
        self.capture_offset.load(Ordering::Relaxed)
    }

    /// Errors either device has reported since the streams started.
    pub fn device_errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Stops both streams without dropping them.
    ///
    /// # Errors
    ///
    /// [`AudioError::Cpal`] if a device refuses.
    pub fn pause(&self) -> Result<(), AudioError> {
        self.input.pause()?;
        self.output.pause()?;
        Ok(())
    }

    /// Restarts both streams after [`AudioIo::pause`].
    ///
    /// # Errors
    ///
    /// [`AudioError::Cpal`] if a device refuses.
    pub fn play(&self) -> Result<(), AudioError> {
        self.input.play()?;
        self.output.play()?;
        Ok(())
    }
}

/// Pushes engine reports onto the event ring, counting any that do not fit.
struct RingSink<'a> {
    events: &'a mut Producer<Event>,
    dropped: &'a mut u64,
}

impl EventSink for RingSink<'_> {
    fn event(&mut self, event: Event) {
        if self.events.push(event).is_err() {
            *self.dropped += 1;
        }
    }
}

/// The output callback's state.
struct Render {
    input_latency: Arc<AtomicU32>,
    capture_offset: Arc<AtomicU32>,
    /// Frames of capture buffered before this callback started consuming.
    cushion: u32,
    /// A latency the caller pinned, used instead of measuring.
    offset_override: Option<u32>,
    sample_rate: u32,
    engine: Engine,
    reader: CaptureReader,
    commands: Consumer<Command>,
    events: Producer<Event>,
    captured: Vec<f32>,
    rendered: Vec<f32>,
    channels: usize,
    dropped_events: u64,
}

impl Render {
    /// Tells the engine how far behind captured audio is running.
    ///
    /// The driver reports how long its own buffering adds on each side; the cushion
    /// between the two callbacks is ours and known exactly. Together they are the round
    /// trip between playing a note and the engine seeing it.
    fn update_capture_offset(&mut self, output_latency: core::time::Duration) {
        let total = self.offset_override.unwrap_or_else(|| {
            frames_in(output_latency, self.sample_rate)
                .saturating_add(self.input_latency.load(Ordering::Relaxed))
                .saturating_add(self.cushion)
        });

        self.capture_offset.store(total, Ordering::Relaxed);
        self.engine.set_capture_offset(Frames(u64::from(total)));
    }

    fn fill<T: FromSample<f32> + Copy>(&mut self, out: &mut [T]) {
        let Self {
            engine,
            reader,
            commands,
            events,
            captured,
            rendered,
            channels,
            dropped_events,
            ..
        } = self;

        let mut sink = RingSink {
            events,
            dropped: dropped_events,
        };

        while let Ok(command) = commands.pop() {
            engine.handle(command, &mut sink);
        }

        let frames = out.len() / *channels;
        let mut done = 0;

        while done < frames {
            let run = (frames - done).min(MAX_BLOCK_FRAMES);
            let samples = run * *channels;

            let filled = reader.read(&mut captured[..samples]);
            engine.process(&captured[..filled], &mut rendered[..samples], &mut sink);

            let target = &mut out[done * *channels..][..samples];
            for (slot, sample) in target.iter_mut().zip(&rendered[..samples]) {
                *slot = T::from_sample_(*sample);
            }
            done += run;
        }
    }
}

fn build_input(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    writer: CaptureWriter,
    errors: Arc<AtomicU64>,
    latency: Arc<AtomicU32>,
    sample_rate: u32,
) -> Result<Stream, AudioError> {
    match format {
        SampleFormat::F32 => {
            input_stream::<f32>(device, config, writer, errors, latency, sample_rate)
        }
        SampleFormat::I16 => {
            input_stream::<i16>(device, config, writer, errors, latency, sample_rate)
        }
        SampleFormat::I32 => {
            input_stream::<i32>(device, config, writer, errors, latency, sample_rate)
        }
        SampleFormat::U16 => {
            input_stream::<u16>(device, config, writer, errors, latency, sample_rate)
        }
        other => Err(AudioError::UnsupportedFormat(other)),
    }
}

fn input_stream<T>(
    device: &Device,
    config: StreamConfig,
    mut writer: CaptureWriter,
    errors: Arc<AtomicU64>,
    latency: Arc<AtomicU32>,
    sample_rate: u32,
) -> Result<Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data, info: &InputCallbackInfo| {
            // How long the driver held these frames between the ADC and this callback.
            let stamp = info.timestamp();
            let held = stamp.callback.saturating_duration_since(stamp.capture);
            latency.store(frames_in(held, sample_rate), Ordering::Relaxed);

            writer.write(data);
        },
        move |_| {
            errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}

fn build_output(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    render: Render,
    errors: Arc<AtomicU64>,
) -> Result<Stream, AudioError> {
    match format {
        SampleFormat::F32 => output_stream::<f32>(device, config, render, errors),
        SampleFormat::I16 => output_stream::<i16>(device, config, render, errors),
        SampleFormat::I32 => output_stream::<i32>(device, config, render, errors),
        SampleFormat::U16 => output_stream::<u16>(device, config, render, errors),
        other => Err(AudioError::UnsupportedFormat(other)),
    }
}

fn output_stream<T>(
    device: &Device,
    config: StreamConfig,
    mut render: Render,
    errors: Arc<AtomicU64>,
) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |data, info: &OutputCallbackInfo| {
            // How long the driver will hold these frames between this callback and the DAC.
            let stamp = info.timestamp();
            render.update_capture_offset(stamp.playback.saturating_duration_since(stamp.callback));

            render.fill(data);
        },
        move |_| {
            errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}
