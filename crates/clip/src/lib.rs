//! Audio storage for recorded clips.
//!
//! Memory is allocated when the pools are built and recycled through them afterwards.
//! Nothing here allocates or frees once audio is running: buffers hand their segments
//! back to the pool they came from instead of dropping them.

use free_loop_core::{Frames, Pan};

/// Frames in one segment. At 48 kHz this is about 1.4 s.
pub const SEGMENT_FRAMES: usize = 65_536;

const SEGMENT_FRAMES_U64: u64 = SEGMENT_FRAMES as u64;

/// Saturating `u64` to `usize`. Only reached with frame counts that already exceed
/// addressable memory.
fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// A gain that may move across a block, rather than switching between blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ramp {
    start: f32,
    step: f32,
}

impl Ramp {
    /// Unity for the whole block.
    pub const UNITY: Self = Self {
        start: 1.0,
        step: 0.0,
    };

    /// A gain that holds for the whole block.
    pub const fn constant(gain: f32) -> Self {
        Self {
            start: gain,
            step: 0.0,
        }
    }

    /// A gain that travels from `start` to `end` over `frames`.
    #[expect(
        clippy::cast_precision_loss,
        reason = "block lengths are far below f32's exact range"
    )]
    pub fn new(start: f32, end: f32, frames: usize) -> Self {
        if frames == 0 {
            return Self::constant(end);
        }
        Self {
            start,
            step: (end - start) / frames as f32,
        }
    }

    /// Whether it would add nothing.
    pub fn is_silent(self) -> bool {
        self.start == 0.0 && self.step == 0.0
    }

    /// The gain at frame `frame` of the block.
    #[expect(
        clippy::cast_precision_loss,
        reason = "block lengths are far below f32's exact range"
    )]
    fn at(self, frame: usize) -> f32 {
        self.start + self.step * frame as f32
    }

    /// The same ramp as seen from `frames` in, for mixing a block in pieces.
    fn from(self, frames: usize) -> Self {
        Self {
            start: self.at(frames),
            step: self.step,
        }
    }
}

/// Channels a pan can place a source across.
const STEREO: usize = 2;

/// A pan that may move across a block, rather than switching between blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanRamp {
    left: Ramp,
    right: Ramp,
    width: Ramp,
}

impl PanRamp {
    /// Dead centre for the whole block.
    pub const CENTRE: Self = Self::constant(Pan::CENTRE);

    /// A pan that holds for the whole block.
    pub const fn constant(pan: Pan) -> Self {
        Self {
            left: Ramp::constant(pan.left()),
            right: Ramp::constant(pan.right()),
            width: Ramp::constant(pan.width()),
        }
    }

    /// A pan that travels from `start` to `end` over `frames`.
    pub fn new(start: Pan, end: Pan, frames: usize) -> Self {
        Self {
            left: Ramp::new(start.left(), end.left(), frames),
            right: Ramp::new(start.right(), end.right(), frames),
            width: Ramp::new(start.width(), end.width(), frames),
        }
    }

    /// The gains at frame `frame` of the block, as left, right and width.
    fn at(self, frame: usize) -> (f32, f32, f32) {
        (
            self.left.at(frame),
            self.right.at(frame),
            self.width.at(frame),
        )
    }

    /// The same pan as seen from `frames` in, for mixing a block in pieces.
    fn from(self, frames: usize) -> Self {
        Self {
            left: self.left.from(frames),
            right: self.right.from(frames),
            width: self.width.from(frames),
        }
    }
}

/// Segments `frames` of audio occupies, rounded up the way storage is allocated.
///
/// The one definition of that rounding, so recording, saving and loading agree.
pub fn segments_for(frames: Frames) -> usize {
    as_usize(frames.0.div_ceil(SEGMENT_FRAMES_U64))
}

/// Splits a frame position into a segment index and an offset within it.
fn split(frame: u64) -> (usize, usize) {
    (
        as_usize(frame / SEGMENT_FRAMES_U64),
        as_usize(frame % SEGMENT_FRAMES_U64),
    )
}

/// A fixed block of interleaved audio.
#[derive(Debug)]
pub struct Segment {
    data: Box<[f32]>,
}

impl Segment {
    /// Allocates a zeroed segment. Call off the audio thread.
    pub fn new(channels: usize) -> Self {
        Self {
            data: vec![0.0; SEGMENT_FRAMES * channels].into_boxed_slice(),
        }
    }
}

/// A pool of segments, drawn from and returned to without allocating.
///
/// Capacity can be held back for audio the pool did not allocate, so it counts against
/// the same ceiling.
#[derive(Debug)]
pub struct SegmentPool {
    free: Vec<Segment>,
    channels: usize,
    /// Segments allocated up front, which `free` never grows past.
    capacity: usize,
    /// Segments accounted to storage held elsewhere, which are never handed out.
    reserved: usize,
}

impl SegmentPool {
    /// Allocates `count` segments up front.
    pub fn new(count: usize, channels: usize) -> Self {
        Self {
            free: (0..count).map(|_| Segment::new(channels)).collect(),
            channels,
            capacity: count,
            reserved: 0,
        }
    }

    /// Segments currently available, which is what is free less what is reserved.
    pub fn available(&self) -> usize {
        self.free.len().saturating_sub(self.reserved)
    }

    /// Holds `count` segments back for audio stored outside the pool.
    ///
    /// Exactly undone by [`SegmentPool::release`] of the same count. The reservation may
    /// exceed what is free, which reads as nothing available until enough comes back.
    pub fn reserve(&mut self, count: usize) {
        self.reserved = self.reserved.saturating_add(count);
    }

    /// Gives `count` reserved segments back.
    pub fn release(&mut self, count: usize) {
        self.reserved = self.reserved.saturating_sub(count);
    }

    /// Segments held back for storage the pool did not allocate.
    pub fn reserved(&self) -> usize {
        self.reserved
    }

    /// Segments the pool was built with, which is the ceiling it holds everything to.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Channel count every segment in this pool was allocated for.
    pub fn channels(&self) -> usize {
        self.channels
    }

    fn take(&mut self) -> Option<Segment> {
        if self.free.len() <= self.reserved {
            return None;
        }
        self.free.pop()
    }

    fn give(&mut self, segment: Segment) {
        self.free.push(segment);
    }
}

/// Sparse audio storage addressed by frame.
///
/// The segment array is sized up front for the longest allowed recording, but segments
/// are only drawn from the pool as the write position reaches them, so a short take
/// costs only the pointer array.
#[derive(Debug)]
pub struct AudioBuffer {
    segments: Box<[Option<Segment>]>,
    channels: usize,
}

impl AudioBuffer {
    /// Allocates the segment array. Call off the audio thread.
    pub fn new(max_segments: usize, channels: usize) -> Self {
        let mut segments = Vec::new();
        segments.resize_with(max_segments, || None);
        Self {
            segments: segments.into_boxed_slice(),
            channels,
        }
    }

    /// Frames this buffer can address if fully populated.
    pub fn capacity(&self) -> u64 {
        self.segments.len() as u64 * SEGMENT_FRAMES_U64
    }

    /// Returns every segment to `pool`, leaving the buffer empty and reusable.
    pub fn drain_into(&mut self, pool: &mut SegmentPool) {
        for slot in &mut self.segments {
            if let Some(segment) = slot.take() {
                pool.give(segment);
            }
        }
    }

    /// Writes interleaved frames at `frame`, drawing segments from `pool` as needed.
    ///
    /// Returns how many frames were written, which is short of `src` only when the pool
    /// ran dry or the buffer's capacity was reached.
    pub fn write(&mut self, frame: u64, src: &[f32], pool: &mut SegmentPool) -> usize {
        let total = src.len() / self.channels;
        let mut done = 0;

        while done < total {
            let (index, offset) = split(frame + done as u64);
            let Some(slot) = self.segments.get_mut(index) else {
                break;
            };
            if slot.is_none() {
                let Some(segment) = pool.take() else { break };
                *slot = Some(segment);
            }
            let Some(segment) = slot.as_mut() else { break };

            let run = (total - done).min(SEGMENT_FRAMES - offset);
            let dst = &mut segment.data[offset * self.channels..(offset + run) * self.channels];
            dst.copy_from_slice(&src[done * self.channels..(done + run) * self.channels]);
            done += run;
        }

        done
    }

    /// Writes interleaved frames at `frame`, taking channels from `src` as `picks` says.
    ///
    /// `src` is `src_channels` wide. Channel `c` of the buffer takes source channel
    /// `picks[c % picks.len()]`, clamped to what `src` holds. An empty `picks` writes
    /// nothing.
    ///
    /// Returns how many frames were written.
    pub fn write_picked(
        &mut self,
        frame: u64,
        src: &[f32],
        src_channels: usize,
        picks: &[u8],
        pool: &mut SegmentPool,
    ) -> usize {
        if src_channels == 0 || picks.is_empty() {
            return 0;
        }
        let last = src_channels - 1;
        let total = src.len() / src_channels;
        let mut done = 0;

        while done < total {
            let (index, offset) = split(frame + done as u64);
            let Some(slot) = self.segments.get_mut(index) else {
                break;
            };
            if slot.is_none() {
                let Some(segment) = pool.take() else { break };
                *slot = Some(segment);
            }
            let Some(segment) = slot.as_mut() else { break };

            let run = (total - done).min(SEGMENT_FRAMES - offset);
            let dst = &mut segment.data[offset * self.channels..(offset + run) * self.channels];
            for (frame_index, out) in dst.chunks_exact_mut(self.channels).enumerate() {
                let from = &src[(done + frame_index) * src_channels..][..src_channels];
                for (channel, sample) in out.iter_mut().enumerate() {
                    let pick = usize::from(picks[channel % picks.len()]).min(last);
                    *sample = from[pick];
                }
            }
            done += run;
        }

        done
    }

    /// Backs `run` frames at `frame` with silence, drawing segments as needed.
    ///
    /// For frames a recording covered but the device never delivered. Recycled segments
    /// still hold the last take's audio, so the range is zeroed.
    ///
    /// Returns how many frames are now backed, short of `run` only when the pool ran dry
    /// or the buffer's capacity was reached.
    pub fn silence(&mut self, frame: u64, run: usize, pool: &mut SegmentPool) -> usize {
        let mut done = 0;

        while done < run {
            let (index, offset) = split(frame + done as u64);
            let Some(slot) = self.segments.get_mut(index) else {
                break;
            };
            if slot.is_none() {
                let Some(segment) = pool.take() else { break };
                *slot = Some(segment);
            }
            let Some(segment) = slot.as_mut() else { break };

            let span = (run - done).min(SEGMENT_FRAMES - offset);
            segment.data[offset * self.channels..(offset + span) * self.channels].fill(0.0);
            done += span;
        }

        done
    }

    /// Copies `run` frames starting at `frame` into `dst`, without wrapping.
    ///
    /// Segments that were never written read as silence.
    fn copy_into(&self, frame: u64, dst: &mut [f32], run: usize) {
        let (index, offset) = split(frame);
        let Some(Some(segment)) = self.segments.get(index) else {
            dst[..run * self.channels].fill(0.0);
            return;
        };
        let src = &segment.data[offset * self.channels..(offset + run) * self.channels];
        dst[..run * self.channels].copy_from_slice(src);
    }

    /// Adds `run` frames starting at `frame` into `dst`.
    ///
    /// Segments that were never written read as silence.
    fn add_into(&self, frame: u64, dst: &mut [f32], run: usize, ramp: Ramp, pan: PanRamp) {
        let (index, offset) = split(frame);
        let Some(Some(segment)) = self.segments.get(index) else {
            return;
        };
        let src = &segment.data[offset * self.channels..(offset + run) * self.channels];

        // A centred pan takes the plain path: the mid/side round trip is only exact in
        // real arithmetic, and every track sits here until one is moved.
        if pan != PanRamp::CENTRE {
            let pairs = self.channels / STEREO;
            let odd = self.channels % STEREO == 1;
            for (position, (out, sample)) in dst
                .chunks_exact_mut(self.channels)
                .zip(src.chunks_exact(self.channels))
                .enumerate()
            {
                let gain = ramp.at(position);
                let (left, right, width) = pan.at(position);
                // Each stereo pair carries the field. A width the device does not pair up
                // has no side to spread across.
                for pair in 0..pairs {
                    let (l, r) = (sample[pair * STEREO], sample[pair * STEREO + 1]);
                    let mid = (l + r) * 0.5;
                    let side = (l - r) * 0.5 * width;
                    out[pair * STEREO] += (mid + side) * gain * left;
                    out[pair * STEREO + 1] += (mid - side) * gain * right;
                }
                if odd {
                    let last = self.channels - 1;
                    out[last] += sample[last] * gain;
                }
            }
            return;
        }

        for (position, (out, sample)) in dst
            .chunks_exact_mut(self.channels)
            .zip(src.chunks_exact(self.channels))
            .enumerate()
        {
            // Recomputed per frame rather than accumulated, which would drift.
            let gain = ramp.at(position);
            for (out, sample) in out.iter_mut().zip(sample) {
                *out += sample * gain;
            }
        }
    }
}

/// A finished recording.
#[derive(Debug)]
pub struct Clip {
    buffer: AudioBuffer,
    len: Frames,
    recorded_at: Frames,
    channels: usize,
    capture_offset: Frames,
    /// Audio captured past the loop, held at `[len, len + tail)`.
    tail: Frames,
    borrowed: bool,
}

impl Clip {
    /// Seals a buffer into a clip of `len` frames that was captured starting at
    /// `recorded_at`.
    pub fn new(buffer: AudioBuffer, len: Frames, recorded_at: Frames, channels: usize) -> Self {
        Self {
            buffer,
            len,
            recorded_at,
            channels,
            capture_offset: Frames::ZERO,
            tail: Frames::ZERO,
            borrowed: false,
        }
    }

    /// Length of the loop.
    pub fn len(&self) -> Frames {
        self.len
    }

    /// Segments this clip's audio costs, wherever its storage came from.
    ///
    /// Counts the tail as well as the loop.
    pub fn segments(&self) -> usize {
        segments_for(Frames(self.len.0.saturating_add(self.tail.0)))
    }

    /// Audio held past the loop, for a pickup to come from.
    pub fn tail(&self) -> Frames {
        self.tail
    }

    /// Records how much audio was kept past the loop.
    pub fn set_tail(&mut self, tail: Frames) {
        self.tail = tail;
    }

    /// The transport position capture began at. Fixes the loop's phase against the grid.
    pub fn recorded_at(&self) -> Frames {
        self.recorded_at
    }

    /// The round trip that was compensated for when this was sealed.
    ///
    /// Already folded into [`Clip::recorded_at`] and not read back when playing.
    pub fn capture_offset(&self) -> Frames {
        self.capture_offset
    }

    /// Records the round trip compensated for.
    pub fn set_capture_offset(&mut self, offset: Frames) {
        self.capture_offset = offset;
    }

    /// Whether this clip's storage belongs to whoever handed it to the engine.
    ///
    /// The engine returns borrowed storage instead of absorbing it, so its pools keep
    /// their size.
    pub fn is_borrowed(&self) -> bool {
        self.borrowed
    }

    /// Marks the storage as belonging to someone else.
    pub fn set_borrowed(&mut self, borrowed: bool) {
        self.borrowed = borrowed;
    }

    /// An empty clip sized for `max_segments`, ready to be recorded into.
    pub fn empty(max_segments: usize, channels: usize) -> Self {
        Self::new(
            AudioBuffer::new(max_segments, channels),
            Frames::ZERO,
            Frames::ZERO,
            channels,
        )
    }

    /// Clears the clip for reuse, keeping its storage.
    pub fn reset(&mut self) {
        self.len = Frames::ZERO;
        self.recorded_at = Frames::ZERO;
        self.capture_offset = Frames::ZERO;
        self.tail = Frames::ZERO;
    }

    /// Sets the loop length.
    pub fn set_len(&mut self, len: Frames) {
        self.len = len;
    }

    /// Sets the transport position the loop is aligned to.
    pub fn set_recorded_at(&mut self, at: Frames) {
        self.recorded_at = at;
    }

    /// Writes interleaved frames at `frame`, drawing segments from `pool` as needed.
    ///
    /// Returns how many frames were written.
    pub fn write(&mut self, frame: u64, src: &[f32], pool: &mut SegmentPool) -> usize {
        self.buffer.write(frame, src, pool)
    }

    /// Writes interleaved frames at `frame`, taking channels from `src` as `picks` says.
    ///
    /// Returns how many frames were written.
    pub fn write_picked(
        &mut self,
        frame: u64,
        src: &[f32],
        src_channels: usize,
        picks: &[u8],
        pool: &mut SegmentPool,
    ) -> usize {
        self.buffer
            .write_picked(frame, src, src_channels, picks, pool)
    }

    /// Backs `run` frames at `frame` with silence, drawing segments as needed.
    ///
    /// Returns how many frames are now backed.
    pub fn silence(&mut self, frame: u64, run: usize, pool: &mut SegmentPool) -> usize {
        self.buffer.silence(frame, run, pool)
    }

    /// Copies the clip's stored audio at `from` into `dst`, loop and tail alike.
    ///
    /// For writing a clip out as it was captured. Reads past what is stored give
    /// silence.
    pub fn copy_into(&self, from: Frames, dst: &mut [f32]) {
        let total = dst.len() / self.channels;
        let mut done = 0;
        while done < total {
            let at = from.0 + done as u64;
            let run = (total - done).min(SEGMENT_FRAMES - as_usize(at % SEGMENT_FRAMES_U64));
            let slice = &mut dst[done * self.channels..(done + run) * self.channels];
            self.buffer.copy_into(at, slice, run);
            done += run;
        }
    }

    /// Returns the clip's segments to `pool`, leaving it empty and reusable.
    pub fn release_segments(&mut self, pool: &mut SegmentPool) {
        self.buffer.drain_into(pool);
    }

    /// Adds the loop into `dst` at `ramp`, from the phase this clip has at `position`.
    ///
    /// `dst` is interleaved and its length must be a multiple of the channel count.
    pub fn mix_into(&self, position: Frames, dst: &mut [f32], ramp: Ramp) {
        self.mix_from(self.recorded_at, position, dst, ramp);
    }

    /// Adds the loop into `dst` as though it were recorded at `anchor`.
    ///
    /// For a clip whose playback position is decided when it is launched rather than when
    /// it was recorded. Otherwise as [`Clip::mix_into`].
    pub fn mix_from(&self, anchor: Frames, position: Frames, dst: &mut [f32], ramp: Ramp) {
        self.mix_pickup(anchor, position, dst, ramp, Frames::ZERO, PanRamp::CENTRE);
    }

    /// Adds the loop into `dst`, opening its first `pickup` frames from the tail.
    ///
    /// Clamped to what the tail holds; the rest of the loop plays as recorded.
    pub fn mix_pickup(
        &self,
        anchor: Frames,
        position: Frames,
        dst: &mut [f32],
        ramp: Ramp,
        pickup: Frames,
        pan: PanRamp,
    ) {
        let len = self.len.0;
        if len == 0 || ramp.is_silent() {
            return;
        }
        let opening = pickup.0.min(self.tail.0).min(len);

        let total = dst.len() / self.channels;
        // Modular rather than saturating: a clip loaded from a session can sit ahead of
        // the transport, and clamping to zero replays one fragment until it catches up.
        let mut phase = (position.0 % len + len - anchor.0 % len) % len;
        let mut done = 0;

        while done < total {
            // Runs never straddle the point the tail stops standing in.
            let standing_in = phase < opening;
            let source = if standing_in { len + phase } else { phase };
            let limit = if standing_in { opening } else { len };

            let run = (total - done)
                .min(SEGMENT_FRAMES - as_usize(source % SEGMENT_FRAMES_U64))
                .min(as_usize(limit - phase));

            let slice = &mut dst[done * self.channels..(done + run) * self.channels];
            self.buffer
                .add_into(source, slice, run, ramp.from(done), pan.from(done));

            done += run;
            phase += run as u64;
            if phase >= len {
                phase = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::useless_vec,
        reason = "tests should fail loudly, and compare exact sample values"
    )]

    use super::*;
    use free_loop_core::{CENTRE_STEP, PAN_STEPS, pan_for_step};

    const CH: usize = 2;

    fn ramp(frames: usize, start: usize) -> Vec<f32> {
        (0..frames * CH)
            .map(|i| (start * CH + i) as f32)
            .collect::<Vec<_>>()
    }

    #[test]
    fn a_constant_ramp_holds_its_gain() {
        let ramp = Ramp::constant(0.5);
        assert_eq!(ramp.at(0), 0.5);
        assert_eq!(ramp.at(1_000), 0.5);
    }

    #[test]
    fn a_ramp_starts_where_it_was_told_and_ends_where_it_was_aimed() {
        let ramp = Ramp::new(0.0, 1.0, 256);
        assert_eq!(ramp.at(0), 0.0);
        assert_eq!(ramp.at(128), 0.5);
        assert_eq!(ramp.at(256), 1.0);
    }

    #[test]
    fn a_ramp_seen_from_part_way_lines_up_with_the_whole() {
        let whole = Ramp::new(0.25, 0.75, 512);
        let rest = whole.from(200);
        assert_eq!(rest.at(0), whole.at(200));
        assert_eq!(rest.at(312), whole.at(512));
    }

    #[test]
    fn a_ramp_over_no_frames_is_its_destination() {
        assert_eq!(Ramp::new(0.0, 0.5, 0), Ramp::constant(0.5));
    }

    #[test]
    fn only_a_flat_zero_ramp_is_silent() {
        assert!(Ramp::constant(0.0).is_silent());
        assert!(!Ramp::UNITY.is_silent());
        assert!(
            !Ramp::new(0.0, 1.0, 64).is_silent(),
            "a fade in starts at zero but does not stay there"
        );
    }

    #[test]
    fn mixing_at_a_ramp_scales_each_frame_by_its_own_gain() {
        let mut pool = SegmentPool::new(1, 1);
        let mut buffer = AudioBuffer::new(1, 1);
        buffer.write(0, &[1.0, 1.0, 1.0, 1.0], &mut pool);
        let clip = Clip::new(buffer, Frames(4), Frames(0), 1);

        let mut out = vec![0.0; 4];
        clip.mix_into(Frames(0), &mut out, Ramp::new(0.0, 1.0, 4));
        assert_eq!(out, vec![0.0, 0.25, 0.5, 0.75]);
    }

    /// A two-frame stereo clip with different content on each channel.
    fn lopsided() -> Clip {
        let mut pool = SegmentPool::new(1, CH);
        let mut buffer = AudioBuffer::new(1, CH);
        buffer.write(0, &[1.0, 0.0, 1.0, 0.0], &mut pool);
        Clip::new(buffer, Frames(2), Frames(0), CH)
    }

    #[test]
    fn a_centred_pan_leaves_the_source_untouched() {
        let clip = lopsided();
        let mut out = vec![0.0; 2 * CH];
        clip.mix_pickup(
            Frames(0),
            Frames(0),
            &mut out,
            Ramp::UNITY,
            Frames::ZERO,
            PanRamp::CENTRE,
        );
        assert_eq!(out, vec![1.0, 0.0, 1.0, 0.0], "left stays left");
    }

    #[test]
    fn a_hard_pan_sums_both_channels_onto_the_side_it_lands_on() {
        let clip = lopsided();
        let mut out = vec![0.0; 2 * CH];
        let hard_right = PanRamp::constant(pan_for_step(6));
        clip.mix_pickup(
            Frames(0),
            Frames(0),
            &mut out,
            Ramp::UNITY,
            Frames::ZERO,
            hard_right,
        );

        assert_eq!(out[0], 0.0, "nothing is left on the left");
        // The source summed to mono is 0.5, at the far end of a constant-power sweep.
        let hard = 0.5 * std::f32::consts::SQRT_2;
        assert!((out[1] - hard).abs() < 1e-6, "got {}", out[1]);
    }

    #[test]
    fn a_hard_pan_keeps_the_power_a_centred_one_had() {
        let mut pool = SegmentPool::new(1, CH);
        let mut buffer = AudioBuffer::new(1, CH);
        // The same signal on both channels, which is what a mono input records.
        buffer.write(0, &[1.0, 1.0], &mut pool);
        let clip = Clip::new(buffer, Frames(1), Frames(0), CH);

        let power = |step: u8| {
            let mut out = vec![0.0; CH];
            clip.mix_pickup(
                Frames(0),
                Frames(0),
                &mut out,
                Ramp::UNITY,
                Frames::ZERO,
                PanRamp::constant(pan_for_step(step)),
            );
            out[0].mul_add(out[0], out[1] * out[1])
        };

        let centre = power(CENTRE_STEP);
        assert_eq!(
            centre, 2.0,
            "a centred track plays as recorded on both sides"
        );
        for step in 0..u8::try_from(PAN_STEPS).unwrap() {
            assert!(
                (power(step) - centre).abs() < 1e-5,
                "step {step} changes level"
            );
        }
    }

    #[test]
    fn a_pan_carries_across_every_stereo_pair_of_a_wider_output() {
        const WIDE: usize = 4;
        let mut pool = SegmentPool::new(1, WIDE);
        let mut buffer = AudioBuffer::new(1, WIDE);
        buffer.write(0, &[1.0, 1.0, 1.0, 1.0], &mut pool);
        let clip = Clip::new(buffer, Frames(1), Frames(0), WIDE);

        let mut out = vec![0.0; WIDE];
        clip.mix_pickup(
            Frames(0),
            Frames(0),
            &mut out,
            Ramp::UNITY,
            Frames::ZERO,
            PanRamp::constant(pan_for_step(6)),
        );

        assert_eq!(out[0], 0.0, "the first pair moved over");
        assert!(out[1] > 0.0);
        assert_eq!(out[2], 0.0, "and so did the second");
        assert!(out[3] > 0.0);
    }

    #[test]
    fn a_mono_output_has_nowhere_to_pan_and_keeps_its_level() {
        let mut pool = SegmentPool::new(1, 1);
        let mut buffer = AudioBuffer::new(1, 1);
        buffer.write(0, &[1.0], &mut pool);
        let clip = Clip::new(buffer, Frames(1), Frames(0), 1);

        let mut out = vec![0.0; 1];
        clip.mix_pickup(
            Frames(0),
            Frames(0),
            &mut out,
            Ramp::UNITY,
            Frames::ZERO,
            PanRamp::constant(pan_for_step(0)),
        );
        assert_eq!(out[0], 1.0, "one channel is played as it was recorded");
    }

    #[test]
    fn a_moving_pan_travels_across_the_block() {
        let mut pool = SegmentPool::new(1, CH);
        let mut buffer = AudioBuffer::new(1, CH);
        buffer.write(0, &[1.0, 1.0, 1.0, 1.0], &mut pool);
        let clip = Clip::new(buffer, Frames(2), Frames(0), CH);

        let mut out = vec![0.0; 2 * CH];
        clip.mix_pickup(
            Frames(0),
            Frames(0),
            &mut out,
            Ramp::UNITY,
            Frames::ZERO,
            PanRamp::new(Pan::CENTRE, pan_for_step(6), 2),
        );

        assert_eq!(out[0], 1.0, "the first frame is still centred");
        assert!(out[2] < out[0], "and the left has begun to give way");
        assert!(out[3] > out[1], "as the right takes over");
    }

    #[test]
    fn a_buffer_round_trips_across_a_segment_boundary() {
        let mut pool = SegmentPool::new(4, CH);
        let mut buffer = AudioBuffer::new(4, CH);

        // Straddle the first segment boundary.
        let at = SEGMENT_FRAMES as u64 - 100;
        let src = ramp(200, 0);
        assert_eq!(buffer.write(at, &src, &mut pool), 200);
        assert_eq!(pool.available(), 2, "two segments should be in use");

        let clip = Clip::new(buffer, Frames(at + 200), Frames(0), CH);
        let mut out = vec![0.0; 200 * CH];
        clip.mix_into(Frames(at), &mut out, Ramp::UNITY);
        assert_eq!(out, src);
    }

    #[test]
    fn a_reservation_takes_segments_out_of_reach() {
        let mut pool = SegmentPool::new(4, CH);
        pool.reserve(3);
        assert_eq!(pool.available(), 1);
        assert_eq!(pool.reserved(), 3);

        let mut buffer = AudioBuffer::new(4, CH);
        let src = ramp(SEGMENT_FRAMES + 10, 0);
        assert_eq!(
            buffer.write(0, &src, &mut pool),
            SEGMENT_FRAMES,
            "one segment, then the reservation stops it"
        );
        assert_eq!(pool.available(), 0);
    }

    #[test]
    fn releasing_a_reservation_puts_the_segments_back() {
        let mut pool = SegmentPool::new(4, CH);
        pool.reserve(4);
        assert_eq!(pool.available(), 0);

        pool.release(4);
        assert_eq!(pool.available(), 4);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn a_reservation_survives_being_made_before_the_one_it_replaces_is_released() {
        let mut pool = SegmentPool::new(4, CH);
        pool.reserve(4);

        // A replacement reserves the incoming clip before retiring the outgoing one, so
        // the two overlap.
        pool.reserve(4);
        pool.release(4);

        assert_eq!(
            pool.reserved(),
            4,
            "the incoming clip is still accounted for"
        );
        assert_eq!(pool.available(), 0);
    }

    #[test]
    fn a_reservation_past_the_pool_leaves_nothing_available() {
        let mut pool = SegmentPool::new(2, CH);
        pool.reserve(100);
        assert_eq!(pool.available(), 0);
        assert_eq!(pool.reserved(), 100, "the count stays exact");

        pool.release(100);
        assert_eq!(pool.available(), 2, "and is exactly undone");
    }

    #[test]
    fn segments_out_on_loan_do_not_shrink_a_reservation() {
        let mut pool = SegmentPool::new(4, CH);
        let mut held = AudioBuffer::new(2, CH);
        held.write(0, &ramp(SEGMENT_FRAMES + 1, 0), &mut pool);
        assert_eq!(pool.available(), 2, "two segments are out");

        // A load arriving while a snapshot still holds owned storage.
        pool.reserve(2);
        assert_eq!(pool.available(), 0);

        held.drain_into(&mut pool);
        assert_eq!(pool.available(), 2, "what came back is not the reservation");
    }

    #[test]
    fn a_clip_costs_whole_segments() {
        assert_eq!(segments_for(Frames(0)), 0);
        assert_eq!(segments_for(Frames(1)), 1);
        assert_eq!(segments_for(Frames(SEGMENT_FRAMES as u64)), 1);
        assert_eq!(segments_for(Frames(SEGMENT_FRAMES as u64 + 1)), 2);
    }

    #[test]
    fn writing_stops_when_the_pool_runs_dry() {
        let mut pool = SegmentPool::new(1, CH);
        let mut buffer = AudioBuffer::new(4, CH);

        let src = ramp(SEGMENT_FRAMES + 500, 0);
        let written = buffer.write(0, &src, &mut pool);
        assert_eq!(written, SEGMENT_FRAMES);
        assert_eq!(pool.available(), 0);
    }

    #[test]
    fn segments_return_to_the_pool() {
        let mut pool = SegmentPool::new(4, CH);
        let mut buffer = AudioBuffer::new(4, CH);
        buffer.write(0, &ramp(10, 0), &mut pool);
        assert_eq!(pool.available(), 3);

        let mut clip = Clip::new(buffer, Frames(10), Frames(0), CH);
        clip.release_segments(&mut pool);
        assert_eq!(pool.available(), 4);
    }

    #[test]
    fn playback_wraps_at_the_loop_length() {
        let mut pool = SegmentPool::new(2, CH);
        let mut buffer = AudioBuffer::new(2, CH);
        let src = ramp(4, 0);
        buffer.write(0, &src, &mut pool);

        let clip = Clip::new(buffer, Frames(4), Frames(0), CH);
        let mut out = vec![0.0; 10 * CH];
        clip.mix_into(Frames(0), &mut out, Ramp::UNITY);

        let expected: Vec<f32> = (0..10)
            .flat_map(|f| [(f % 4 * CH) as f32, (f % 4 * CH + 1) as f32])
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn phase_follows_the_transport_not_the_launch() {
        let mut pool = SegmentPool::new(2, CH);
        let mut buffer = AudioBuffer::new(2, CH);
        buffer.write(0, &ramp(4, 0), &mut pool);

        // Recorded starting at frame 100, so frame 106 is phase 2.
        let clip = Clip::new(buffer, Frames(4), Frames(100), CH);
        let mut out = vec![0.0; 2 * CH];
        clip.mix_into(Frames(106), &mut out, Ramp::UNITY);
        assert_eq!(out, vec![4.0, 5.0, 6.0, 7.0]);
    }

    #[test]
    fn a_clip_ahead_of_the_transport_still_plays_its_phase() {
        let mut pool = SegmentPool::new(2, CH);
        let mut buffer = AudioBuffer::new(2, CH);
        buffer.write(0, &ramp(4, 0), &mut pool);

        // Saved with a phase near the end of the loop, then played from a transport that
        // has only just started.
        let clip = Clip::new(buffer, Frames(4), Frames(3), CH);

        let mut out = vec![0.0; 2 * CH];
        clip.mix_into(Frames(0), &mut out, Ramp::UNITY);
        assert_eq!(out, vec![2.0, 3.0, 4.0, 5.0], "phase 1, not phase 0");

        let mut later = vec![0.0; 2 * CH];
        clip.mix_into(Frames(1), &mut later, Ramp::UNITY);
        assert_eq!(later, vec![4.0, 5.0, 6.0, 7.0], "and it advances");
    }

    #[test]
    fn phase_is_the_same_a_whole_number_of_loops_apart() {
        let mut pool = SegmentPool::new(2, CH);
        let mut buffer = AudioBuffer::new(2, CH);
        buffer.write(0, &ramp(4, 0), &mut pool);
        let clip = Clip::new(buffer, Frames(4), Frames(3), CH);

        let mut early = vec![0.0; 4 * CH];
        let mut late = vec![0.0; 4 * CH];
        clip.mix_into(Frames(1), &mut early, Ramp::UNITY);
        clip.mix_into(Frames(1 + 4 * 1_000), &mut late, Ramp::UNITY);
        assert_eq!(early, late);
    }

    #[test]
    fn unwritten_segments_read_as_silence() {
        let mut pool = SegmentPool::new(2, CH);
        let buffer = AudioBuffer::new(2, CH);
        let clip = Clip::new(buffer, Frames(8), Frames(0), CH);

        let mut out = vec![1.0; 8 * CH];
        clip.mix_into(Frames(0), &mut out, Ramp::UNITY);
        assert_eq!(
            out,
            vec![1.0; 8 * CH],
            "mixing adds, so silence leaves dst alone"
        );

        let mut clip = clip;
        clip.release_segments(&mut pool);
        assert_eq!(pool.available(), 2, "an unwritten clip holds no segments");
    }

    #[test]
    fn a_reset_clip_keeps_its_storage() {
        let mut pool = SegmentPool::new(4, CH);
        let mut clip = Clip::empty(4, CH);
        clip.write(0, &ramp(10, 0), &mut pool);
        clip.set_len(Frames(10));
        assert_eq!(pool.available(), 3);

        clip.reset();
        assert_eq!(clip.len(), Frames::ZERO);
        assert_eq!(
            pool.available(),
            3,
            "reset keeps the segments; release_segments hands them back"
        );

        clip.release_segments(&mut pool);
        assert_eq!(pool.available(), 4);
    }

    #[test]
    fn mixing_adds_rather_than_overwrites() {
        let mut pool = SegmentPool::new(1, CH);
        let mut buffer = AudioBuffer::new(1, CH);
        buffer.write(0, &vec![0.5; 4 * CH], &mut pool);

        let clip = Clip::new(buffer, Frames(4), Frames(0), CH);
        let mut out = vec![0.25; 4 * CH];
        clip.mix_into(Frames(0), &mut out, Ramp::UNITY);
        assert_eq!(out, vec![0.75; 4 * CH]);
    }
}
