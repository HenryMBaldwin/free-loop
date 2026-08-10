//! The capture ring between the input and output callbacks.
//!
//! The input side converts and pushes; the output side pops and runs the engine. On one
//! device both callbacks share a hardware clock, so the ring level is stable and the
//! cushion is a fixed, known latency rather than drift.

use cpal::FromSample;
use rtrb::{Consumer, Producer, RingBuffer};

/// Largest run of frames handled in one pass. Bounds the scratch buffers.
pub const MAX_BLOCK_FRAMES: usize = 8_192;

/// Which device channels the engine listens to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputSource {
    /// Device channels in order. Extra device channels are dropped; if the device has
    /// fewer than the engine wants, channel 0 fills the rest.
    #[default]
    Direct,
    /// One device channel spread across every engine channel.
    ///
    /// An interface with several inputs reports them all whether or not anything is
    /// plugged in, so a single instrument on input 2 arrives as silence-left.
    Mono(usize),
}

/// Maps device channels onto engine channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelMap {
    device: usize,
    engine: usize,
    source: InputSource,
}

impl ChannelMap {
    /// Builds a map between the two layouts.
    pub fn new(device: usize, engine: usize, source: InputSource) -> Self {
        Self {
            device,
            engine,
            source,
        }
    }

    /// A map that takes device channels in order.
    pub fn direct(device: usize, engine: usize) -> Self {
        Self::new(device, engine, InputSource::Direct)
    }

    /// The device channel feeding an engine channel, clamped to what the device has.
    fn source_channel(self, engine_channel: usize) -> usize {
        let last = self.device.saturating_sub(1);
        match self.source {
            InputSource::Direct => engine_channel.min(last),
            InputSource::Mono(channel) => channel.min(last),
        }
    }

    /// Device channel count.
    pub fn device(self) -> usize {
        self.device
    }

    /// Engine channel count.
    pub fn engine(self) -> usize {
        self.engine
    }

    /// Converts `src` from device layout into `dst` in engine layout.
    ///
    /// Returns the frames written, which is limited by the shorter of the two slices.
    pub fn map<T>(self, src: &[T], dst: &mut [f32]) -> usize
    where
        T: Copy,
        f32: FromSample<T>,
    {
        if self.device == 0 || self.engine == 0 {
            return 0;
        }
        let frames = (src.len() / self.device).min(dst.len() / self.engine);

        for frame in 0..frames {
            let input = &src[frame * self.device..][..self.device];
            let output = &mut dst[frame * self.engine..][..self.engine];
            for (channel, sample) in output.iter_mut().enumerate() {
                *sample = f32::from_sample_(input[self.source_channel(channel)]);
            }
        }
        frames
    }
}

/// The input side of the capture ring.
#[derive(Debug)]
pub struct CaptureWriter {
    producer: Producer<f32>,
    map: ChannelMap,
    scratch: Vec<f32>,
    dropped: u64,
}

impl CaptureWriter {
    /// Converts, maps and pushes one device buffer.
    ///
    /// Returns the frames that did not fit, which happens when the output callback has
    /// stalled.
    pub fn write<T>(&mut self, device_samples: &[T]) -> usize
    where
        T: Copy,
        f32: FromSample<T>,
    {
        if self.map.device == 0 {
            return 0;
        }

        let frames = device_samples.len() / self.map.device;
        let mut done = 0;
        let mut dropped = 0;

        while done < frames {
            let run = (frames - done).min(MAX_BLOCK_FRAMES);
            let src = &device_samples[done * self.map.device..][..run * self.map.device];
            let dst = &mut self.scratch[..run * self.map.engine];
            self.map.map(src, dst);

            let (_, rejected) = self.producer.push_partial_slice(dst);
            dropped += rejected.len() / self.map.engine;
            done += run;
        }

        self.dropped += dropped as u64;
        dropped
    }

    /// Frames dropped since the stream started.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// The output side of the capture ring.
#[derive(Debug)]
pub struct CaptureReader {
    consumer: Consumer<f32>,
    channels: usize,
    cushion_samples: usize,
    primed: bool,
}

impl CaptureReader {
    /// Fills as much of `dst` as the ring holds, in whole frames.
    ///
    /// Returns zero until the cushion has built up. After that a short read means the
    /// device under-delivered.
    pub fn read(&mut self, dst: &mut [f32]) -> usize {
        if self.channels == 0 {
            return 0;
        }
        if !self.primed {
            if self.consumer.slots() < self.cushion_samples {
                return 0;
            }
            self.primed = true;
        }

        let frames = (dst.len() / self.channels).min(self.consumer.slots() / self.channels);
        let samples = frames * self.channels;
        let (taken, _) = self.consumer.pop_partial_slice(&mut dst[..samples]);
        taken.len()
    }

    /// Whether the cushion has built up and reads have started.
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Frames waiting to be read.
    pub fn available_frames(&self) -> usize {
        self.consumer
            .slots()
            .checked_div(self.channels)
            .unwrap_or(0)
    }
}

/// Builds a capture ring.
///
/// `capacity_frames` is rounded up to hold at least the cushion plus one full block.
pub fn capture_ring(
    capacity_frames: usize,
    cushion_frames: usize,
    map: ChannelMap,
) -> (CaptureWriter, CaptureReader) {
    let channels = map.engine();
    let frames = capacity_frames.max(cushion_frames + MAX_BLOCK_FRAMES.min(4_096));
    let (producer, consumer) = RingBuffer::new(frames * channels.max(1));

    (
        CaptureWriter {
            producer,
            map,
            scratch: vec![0.0; MAX_BLOCK_FRAMES * channels.max(1)],
            dropped: 0,
        },
        CaptureReader {
            consumer,
            channels,
            cushion_samples: cushion_frames * channels,
            primed: false,
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        clippy::useless_vec,
        reason = "sample values are exact, and slices keep the fixtures readable"
    )]

    use super::*;

    #[test]
    fn stereo_passes_straight_through() {
        let map = ChannelMap::direct(2, 2);
        let mut dst = [0.0; 4];
        assert_eq!(map.map(&[1.0_f32, 2.0, 3.0, 4.0], &mut dst), 2);
        assert_eq!(dst, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_mono_input_fills_both_sides() {
        let map = ChannelMap::direct(1, 2);
        let mut dst = [0.0; 4];
        assert_eq!(map.map(&[1.0_f32, 2.0], &mut dst), 2);
        assert_eq!(dst, [1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn extra_device_channels_are_dropped() {
        let map = ChannelMap::direct(4, 2);
        let mut dst = [0.0; 4];
        assert_eq!(
            map.map(&[1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], &mut dst),
            2
        );
        assert_eq!(dst, [1.0, 2.0, 5.0, 6.0]);
    }

    #[test]
    fn a_named_mono_channel_reaches_both_sides() {
        // A bass on the Scarlett Solo's instrument jack arrives on device channel 1.
        let map = ChannelMap::new(2, 2, InputSource::Mono(1));
        let mut dst = [0.0; 4];
        assert_eq!(map.map(&[0.0_f32, 1.0, 0.0, 2.0], &mut dst), 2);
        assert_eq!(dst, [1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn direct_leaves_a_mono_source_on_one_side() {
        // The behaviour `InputSource::Mono` exists to correct.
        let map = ChannelMap::direct(2, 2);
        let mut dst = [0.0; 2];
        map.map(&[0.0_f32, 1.0], &mut dst);
        assert_eq!(dst, [0.0, 1.0]);
    }

    #[test]
    fn a_mono_channel_past_the_device_clamps() {
        let map = ChannelMap::new(2, 2, InputSource::Mono(7));
        let mut dst = [0.0; 2];
        map.map(&[3.0_f32, 4.0], &mut dst);
        assert_eq!(dst, [4.0, 4.0]);
    }

    #[test]
    fn integer_samples_are_converted() {
        let map = ChannelMap::direct(2, 2);
        let mut dst = [0.0; 2];
        map.map(&[i16::MAX, 0_i16], &mut dst);
        assert!((dst[0] - 1.0).abs() < 1e-4, "got {}", dst[0]);
        assert_eq!(dst[1], 0.0);
    }

    #[test]
    fn mapping_stops_at_the_shorter_slice() {
        let map = ChannelMap::direct(2, 2);
        let mut dst = [0.0; 2];
        assert_eq!(map.map(&[1.0_f32, 2.0, 3.0, 4.0], &mut dst), 1);
    }

    #[test]
    fn reads_return_nothing_until_the_cushion_is_built() {
        let map = ChannelMap::direct(2, 2);
        let (mut writer, mut reader) = capture_ring(8_192, 128, map);

        writer.write(&vec![0.5_f32; 64 * 2]);
        let mut dst = [0.0; 64 * 2];
        assert_eq!(reader.read(&mut dst), 0, "64 frames is under the cushion");
        assert!(!reader.is_primed());

        writer.write(&vec![0.5_f32; 64 * 2]);
        assert_eq!(reader.read(&mut dst), 128);
        assert!(reader.is_primed());
    }

    #[test]
    fn once_primed_a_short_ring_gives_a_short_read() {
        let map = ChannelMap::direct(2, 2);
        let (mut writer, mut reader) = capture_ring(8_192, 32, map);

        writer.write(&vec![1.0_f32; 40 * 2]);
        let mut dst = [0.0; 64 * 2];
        assert_eq!(reader.read(&mut dst), 40 * 2);
        assert_eq!(reader.read(&mut dst), 0, "the ring is empty, not unprimed");
        assert!(reader.is_primed());
    }

    #[test]
    fn reads_are_whole_frames() {
        let map = ChannelMap::direct(2, 2);
        let (mut writer, mut reader) = capture_ring(8_192, 0, map);

        writer.write(&vec![1.0_f32; 3 * 2]);
        let mut dst = [0.0; 8];
        assert_eq!(reader.read(&mut dst) % 2, 0);
    }

    #[test]
    fn a_stalled_reader_makes_the_writer_drop() {
        let map = ChannelMap::direct(2, 2);
        let (mut writer, reader) = capture_ring(256, 0, map);
        drop(reader);

        let capacity = 4_096 + 256;
        let dropped = writer.write(&vec![1.0_f32; (capacity + 100) * 2]);
        assert!(dropped > 0, "the ring should have overflowed");
        assert_eq!(writer.dropped(), dropped as u64);
    }

    #[test]
    fn a_block_larger_than_the_scratch_is_written_in_runs() {
        let map = ChannelMap::direct(2, 2);
        let frames = MAX_BLOCK_FRAMES + 500;
        let (mut writer, reader) = capture_ring(frames * 2, 0, map);

        assert_eq!(writer.write(&vec![0.25_f32; frames * 2]), 0);
        assert_eq!(reader.available_frames(), frames);
    }
}
