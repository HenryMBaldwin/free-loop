//! Opening devices and running the engine from the output callback.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Arc, Mutex};

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

/// How long to wait between attempts to reopen a device that went away.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Seconds of capture delivering nothing before the input counts as gone.
///
/// A device that is unplugged stops delivering without always reporting an error, so this
/// is the other way the streams learn they have to be rebuilt.
const STARVED_SECONDS: u32 = 1;

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
    /// What was asked for, kept so the same request can be made again.
    config: AudioConfig,
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
        config: config.clone(),
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
        let (commands, command_rx) = RingBuffer::new(COMMAND_SLOTS);
        let (event_tx, events) = RingBuffer::new(EVENT_SLOTS);

        let health = Health {
            errors: Arc::new(AtomicU64::new(0)),
            lost: Arc::new(AtomicBool::new(false)),
            starved: Arc::new(AtomicU32::new(0)),
        };
        let input_latency = Arc::new(AtomicU32::new(0));
        let capture_offset = Arc::new(AtomicU32::new(0));

        let shared = Arc::new(Mutex::new(Shared {
            engine,
            commands: command_rx,
            events: event_tx,
            reader: None,
            captured: vec![0.0; MAX_BLOCK_FRAMES * negotiated.channels],
            rendered: vec![0.0; MAX_BLOCK_FRAMES * negotiated.channels],
            channels: negotiated.channels,
            dropped_events: 0,
        }));

        let mut io = AudioIo {
            streams: None,
            shared,
            commands,
            events,
            negotiated,
            config: self.config.clone(),
            health,
            capture_offset,
            input_latency,
            retry_at: None,
            check_at: RETRY_INTERVAL,
            open_names: (String::new(), String::new()),
            refusal: None,
        };
        io.spawn(&self)?;
        Ok(io)
    }
}

/// What happened to the devices on one pass of the control loop.
#[derive(Debug)]
pub enum DeviceChange {
    /// A device went away. Nothing sounds and the transport is frozen where it was.
    Lost,
    /// The devices are running again.
    Back,
    /// A device was there but could not be used. Reported once per reason.
    Refused(AudioError),
}

/// Running streams.
///
/// `cpal::Stream` is not `Send` on every platform, so keep this on the thread that
/// started it. Dropping it stops both streams.
pub struct AudioIo {
    /// The running streams, absent while no device is attached.
    streams: Option<Streams>,
    /// The engine and its buffers, which outlive any one pair of streams.
    shared: Arc<Mutex<Shared>>,
    commands: Producer<Command>,
    events: Consumer<Event>,
    negotiated: Negotiated,
    /// What was asked for, so the same request can be made again.
    config: AudioConfig,
    health: Health,
    capture_offset: Arc<AtomicU32>,
    input_latency: Arc<AtomicU32>,
    /// Time of the next attempt, or `None` while the devices are running.
    retry_at: Option<Duration>,
    /// Time of the next check that the open devices are still there.
    check_at: Duration,
    /// Names the running streams were opened under. Checked rather than the configured
    /// names, which may be absent and leave nothing to check.
    open_names: (String, String),
    /// The last reason a reopen was refused, so it is reported once.
    refusal: Option<String>,
}

/// A running pair. Dropping it stops both.
struct Streams {
    input: Stream,
    output: Stream,
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

    /// Whether the devices are running.
    pub fn is_running(&self) -> bool {
        self.streams.is_some()
    }

    /// The shared state.
    ///
    /// Only call this with the streams stopped: the audio callback holds the same lock.
    fn locked(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Notices a device going away and puts it back when it returns.
    ///
    /// Call every pass of the control loop. Returns what changed, if anything.
    pub fn tick(&mut self, now: Duration) -> Option<DeviceChange> {
        if self.streams.is_some() {
            // A device that has been unplugged does not always report an error, but it
            // does stop delivering.
            let starved = self.health.starved.load(Ordering::Relaxed);
            if starved_out(starved, self.negotiated.sample_rate) {
                self.health.lost.store(true, Ordering::Relaxed);
            }

            if now >= self.check_at {
                self.check_at = now + RETRY_INTERVAL;
                let host = cpal::default_host();
                let (input, output) = &self.open_names;
                if !still_listed(&host, input, false) || !still_listed(&host, output, true) {
                    self.health.lost.store(true, Ordering::Relaxed);
                }
            }

            if !self.health.lost.swap(false, Ordering::Relaxed) {
                return None;
            }
            // Dropping stops both, which also hands the device back to the system.
            self.streams = None;
            self.retry_at = Some(now + RETRY_INTERVAL);
            return Some(DeviceChange::Lost);
        }

        // Nothing is draining the command ring, so it would otherwise fill and start
        // refusing presses.
        self.locked().drain_commands();

        if self.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        self.retry_at = Some(now + RETRY_INTERVAL);

        match self.reopen() {
            Ok(()) => {
                self.retry_at = None;
                self.refusal = None;
                self.health.lost.store(false, Ordering::Relaxed);
                Some(DeviceChange::Back)
            }
            Err(error) => {
                // Reported once per reason, or an absent device would say so every
                // interval for as long as it is gone.
                let reason = error.to_string();
                if self.refusal.as_ref() == Some(&reason) {
                    return None;
                }
                self.refusal = Some(reason);
                Some(DeviceChange::Refused(error))
            }
        }
    }

    /// Opens the devices again and starts them, if they offer what the engine expects.
    fn reopen(&mut self) -> Result<(), AudioError> {
        let opened = open(&self.config)?;
        let found = opened.negotiated();

        compatible(self.negotiated, found)?;
        self.spawn(&opened)
    }

    /// Builds a capture ring and a pair of streams, and starts them.
    fn spawn(&mut self, opened: &Opened) -> Result<(), AudioError> {
        let negotiated = opened.negotiated;
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

        // Whatever the previous device left is audio from before it went away.
        self.locked().reader = Some(reader);

        let callback = Render {
            shared: Arc::clone(&self.shared),
            health: self.health.clone(),
            input_latency: Arc::clone(&self.input_latency),
            capture_offset: Arc::clone(&self.capture_offset),
            cushion: u32::try_from(negotiated.cushion_frames).unwrap_or(u32::MAX),
            offset_override: negotiated.capture_offset,
            sample_rate: negotiated.sample_rate,
        };

        let input = build_input(
            &opened.input,
            opened.input_config,
            negotiated.input_format,
            writer,
            self.health.clone(),
            Arc::clone(&self.input_latency),
            negotiated.sample_rate,
        )?;
        let output = build_output(
            &opened.output,
            opened.output_config,
            negotiated.output_format,
            callback,
            self.health.clone(),
        )?;

        input.play()?;
        output.play()?;

        self.health.starved.store(0, Ordering::Relaxed);
        self.open_names = (opened.input_name(), opened.output_name());
        self.negotiated = negotiated;
        self.streams = Some(Streams { input, output });
        Ok(())
    }

    /// Queues a command for the engine.
    ///
    /// # Errors
    ///
    /// Returns the command back if the queue is full.
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
    /// Zero until the first output callback has run.
    pub fn capture_offset_frames(&self) -> u32 {
        self.capture_offset.load(Ordering::Relaxed)
    }

    /// Errors either device has reported since the streams started.
    pub fn device_errors(&self) -> u64 {
        self.health.errors.load(Ordering::Relaxed)
    }

    /// Stops both streams without dropping them.
    ///
    /// # Errors
    ///
    /// [`AudioError::Cpal`] if a device refuses.
    pub fn pause(&self) -> Result<(), AudioError> {
        if let Some(streams) = self.streams.as_ref() {
            streams.input.pause()?;
            streams.output.pause()?;
        }
        Ok(())
    }

    /// Restarts both streams after [`AudioIo::pause`].
    ///
    /// # Errors
    ///
    /// [`AudioError::Cpal`] if a device refuses.
    pub fn play(&self) -> Result<(), AudioError> {
        if let Some(streams) = self.streams.as_ref() {
            streams.input.play()?;
            streams.output.play()?;
        }
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

/// Everything the output callback works on.
///
/// Behind a lock rather than owned by the callback, so it outlives the stream that was
/// running it. The callback never waits for the lock.
struct Shared {
    engine: Engine,
    commands: Consumer<Command>,
    events: Producer<Event>,
    /// Set when a stream starts. Without one there is no capture, so input reads silent.
    reader: Option<CaptureReader>,
    captured: Vec<f32>,
    rendered: Vec<f32>,
    channels: usize,
    dropped_events: u64,
}

/// The output callback's state.
struct Render {
    shared: Arc<Mutex<Shared>>,
    health: Health,
    input_latency: Arc<AtomicU32>,
    capture_offset: Arc<AtomicU32>,
    /// Frames of capture buffered before this callback started consuming.
    cushion: u32,
    /// A latency the caller pinned, used instead of measuring.
    offset_override: Option<u32>,
    sample_rate: u32,
}

impl Render {
    /// Tells the engine how far behind captured audio is running.
    ///
    /// The driver reports what its own buffering adds on each side; the cushion between
    /// the two callbacks is known exactly. Together they are the round trip between
    /// playing a note and the engine seeing it.
    fn update_capture_offset(&mut self, output_latency: core::time::Duration) {
        let total = self.offset_override.unwrap_or_else(|| {
            frames_in(output_latency, self.sample_rate)
                .saturating_add(self.input_latency.load(Ordering::Relaxed))
                .saturating_add(self.cushion)
        });

        self.capture_offset.store(total, Ordering::Relaxed);
        if let Ok(mut shared) = self.shared.try_lock() {
            shared.engine.set_capture_offset(Frames(u64::from(total)));
        }
    }

    /// Renders one block, or silence if the state is being handed over.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let Ok(mut shared) = self.shared.try_lock() else {
            // Held only while the streams are stopped, so this is a block either side of
            // a device going away.
            out.fill(T::EQUILIBRIUM);
            return;
        };

        let wanted = shared.frames(out);
        let filled = shared.fill(out);
        if filled == 0 && wanted > 0 {
            self.health
                .starved
                .fetch_add(u32::try_from(wanted).unwrap_or(u32::MAX), Ordering::Relaxed);
        } else {
            self.health.starved.store(0, Ordering::Relaxed);
        }
    }
}

impl Shared {
    /// Applies everything the control thread has queued.
    fn drain_commands(&mut self) {
        let Self {
            engine,
            commands,
            events,
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
    }

    /// Frames one block of `out` holds.
    fn frames<T>(&self, out: &[T]) -> usize {
        out.len() / self.channels
    }

    /// Renders one block, returning how many frames of capture it had to work with.
    fn fill<T: FromSample<f32> + Copy>(&mut self, out: &mut [T]) -> usize {
        self.drain_commands();

        let Self {
            engine,
            reader,
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

        let frames = out.len() / *channels;
        let mut done = 0;
        let mut captured_frames = 0;

        while done < frames {
            let run = (frames - done).min(MAX_BLOCK_FRAMES);
            let samples = run * *channels;

            let filled = reader
                .as_mut()
                .map_or(0, |reader| reader.read(&mut captured[..samples]));
            captured_frames += filled / *channels;
            engine.process(&captured[..filled], &mut rendered[..samples], &mut sink);

            let target = &mut out[done * *channels..][..samples];
            for (slot, sample) in target.iter_mut().zip(&rendered[..samples]) {
                *slot = T::from_sample_(*sample);
            }
            done += run;
        }

        captured_frames
    }
}

fn build_input(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    writer: CaptureWriter,
    health: Health,
    latency: Arc<AtomicU32>,
    sample_rate: u32,
) -> Result<Stream, AudioError> {
    match format {
        SampleFormat::F32 => {
            input_stream::<f32>(device, config, writer, health, latency, sample_rate)
        }
        SampleFormat::I16 => {
            input_stream::<i16>(device, config, writer, health, latency, sample_rate)
        }
        SampleFormat::I32 => {
            input_stream::<i32>(device, config, writer, health, latency, sample_rate)
        }
        SampleFormat::U16 => {
            input_stream::<u16>(device, config, writer, health, latency, sample_rate)
        }
        other => Err(AudioError::UnsupportedFormat(other)),
    }
}

fn input_stream<T>(
    device: &Device,
    config: StreamConfig,
    mut writer: CaptureWriter,
    health: Health,
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
        move |error| note_error(&error, &health),
        None,
    )?;
    Ok(stream)
}

/// Whether a device of this name is still listed.
///
/// A device that is unplugged can be replaced by the host rather than reported as an
/// error, which leaves the streams running against something else entirely. The name it
/// was opened under no longer being listed is the only sign of that.
fn still_listed(host: &Host, name: &str, output: bool) -> bool {
    find_device(host, Some(name), output).is_ok()
}

/// Whether the capture has delivered nothing for long enough to count as gone.
fn starved_out(frames: u32, sample_rate: u32) -> bool {
    frames >= sample_rate.saturating_mul(STARVED_SECONDS)
}

/// Whether a device that has come back can carry on with a session built for `wanted`.
///
/// Every clip length and phase is a frame count at one rate, and the engine interleaves
/// for one channel count. Anything else about the device may differ.
fn compatible(wanted: Negotiated, found: Negotiated) -> Result<(), AudioError> {
    if wanted.sample_rate == found.sample_rate && wanted.channels == found.channels {
        return Ok(());
    }
    Err(AudioError::ConfigurationChanged {
        wanted_rate: wanted.sample_rate,
        found_rate: found.sample_rate,
        wanted_channels: wanted.channels,
        found_channels: found.channels,
    })
}

/// What both streams report their trouble into.
#[derive(Debug, Clone)]
struct Health {
    /// Every error either device has reported.
    errors: Arc<AtomicU64>,
    /// Set when the streams have to be rebuilt.
    lost: Arc<AtomicBool>,
    /// Frames the capture has delivered nothing for, in a row.
    starved: Arc<AtomicU32>,
}

/// Counts a stream error, and flags the ones that mean the stream has to be rebuilt.
///
/// A reroute leaves the stream working, so it is counted and otherwise left alone.
fn note_error(error: &cpal::Error, health: &Health) {
    health.errors.fetch_add(1, Ordering::Relaxed);
    if matches!(
        error.kind(),
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
    ) {
        health.lost.store(true, Ordering::Relaxed);
    }
}

fn build_output(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    render: Render,
    health: Health,
) -> Result<Stream, AudioError> {
    match format {
        SampleFormat::F32 => output_stream::<f32>(device, config, render, health),
        SampleFormat::I16 => output_stream::<i16>(device, config, render, health),
        SampleFormat::I32 => output_stream::<i32>(device, config, render, health),
        SampleFormat::U16 => output_stream::<u16>(device, config, render, health),
        other => Err(AudioError::UnsupportedFormat(other)),
    }
}

fn output_stream<T>(
    device: &Device,
    config: StreamConfig,
    mut render: Render,
    health: Health,
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
        move |error| note_error(&error, &health),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::InputSource;

    fn negotiated(sample_rate: u32, channels: usize) -> Negotiated {
        Negotiated {
            sample_rate,
            channels,
            input_channels: 2,
            input_format: SampleFormat::F32,
            output_format: SampleFormat::F32,
            input_source: InputSource::Direct,
            buffer_frames: None,
            cushion_frames: 512,
            capture_offset: None,
        }
    }

    fn health() -> Health {
        Health {
            errors: Arc::new(AtomicU64::new(0)),
            lost: Arc::new(AtomicBool::new(false)),
            starved: Arc::new(AtomicU32::new(0)),
        }
    }

    #[test]
    fn the_same_configuration_carries_on() {
        assert!(compatible(negotiated(48_000, 2), negotiated(48_000, 2)).is_ok());
    }

    #[test]
    fn another_rate_is_refused() {
        let error = compatible(negotiated(48_000, 2), negotiated(44_100, 2));
        assert!(matches!(
            error,
            Err(AudioError::ConfigurationChanged {
                wanted_rate: 48_000,
                found_rate: 44_100,
                ..
            })
        ));
    }

    #[test]
    fn another_channel_count_is_refused() {
        let error = compatible(negotiated(48_000, 2), negotiated(48_000, 4));
        assert!(matches!(
            error,
            Err(AudioError::ConfigurationChanged {
                wanted_channels: 2,
                found_channels: 4,
                ..
            })
        ));
    }

    #[test]
    fn the_rest_of_the_configuration_may_differ() {
        let mut found = negotiated(48_000, 2);
        found.input_channels = 8;
        found.input_format = SampleFormat::I16;
        found.cushion_frames = 1_024;
        assert!(
            compatible(negotiated(48_000, 2), found).is_ok(),
            "only the rate and the engine's channel count are fixed"
        );
    }

    #[test]
    fn a_name_matching_nothing_is_a_loss() {
        let host = cpal::default_host();
        assert!(!still_listed(&host, "no such device anywhere", true));
        assert!(!still_listed(&host, "no such device anywhere", false));
    }

    #[test]
    fn a_device_the_host_lists_is_not_a_loss() {
        let host = cpal::default_host();
        let Ok(devices) = list_devices() else { return };
        if let Some(name) = devices.outputs.first() {
            assert!(still_listed(&host, name, true), "{name} is right there");
        }
    }

    #[test]
    fn a_short_gap_in_the_capture_is_not_a_loss() {
        assert!(!starved_out(0, 48_000));
        assert!(
            !starved_out(47_999, 48_000),
            "a hiccup, not an unplugged jack"
        );
    }

    #[test]
    fn capture_silent_for_a_whole_second_is_a_loss() {
        assert!(starved_out(48_000, 48_000));
        assert!(starved_out(96_000, 48_000));
    }

    #[test]
    fn a_missing_device_asks_for_a_rebuild() {
        let health = health();
        note_error(
            &cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable),
            &health,
        );
        assert!(health.lost.load(Ordering::Relaxed));
        assert_eq!(health.errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_invalidated_stream_asks_for_a_rebuild() {
        let health = health();
        note_error(
            &cpal::Error::new(cpal::ErrorKind::StreamInvalidated),
            &health,
        );
        assert!(health.lost.load(Ordering::Relaxed));
    }

    #[test]
    fn a_reroute_is_counted_and_left_alone() {
        let health = health();
        note_error(&cpal::Error::new(cpal::ErrorKind::DeviceChanged), &health);
        assert!(
            !health.lost.load(Ordering::Relaxed),
            "the stream is still working"
        );
        assert_eq!(health.errors.load(Ordering::Relaxed), 1);
    }
}
