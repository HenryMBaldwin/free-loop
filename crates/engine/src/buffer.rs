//! Audio storage for recorded clips.
//!
//! Memory is allocated when the pools are built and recycled through them afterwards.
//! Nothing in this module allocates or frees once audio is running: buffers hand their
//! segments back to the pool they came from instead of dropping them.

use free_loop_core::Frames;

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
#[derive(Debug)]
pub struct SegmentPool {
    free: Vec<Segment>,
    channels: usize,
}

impl SegmentPool {
    /// Allocates `count` segments up front.
    pub fn new(count: usize, channels: usize) -> Self {
        Self {
            free: (0..count).map(|_| Segment::new(channels)).collect(),
            channels,
        }
    }

    /// Segments currently available.
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Channel count every segment in this pool was allocated for.
    pub fn channels(&self) -> usize {
        self.channels
    }

    fn take(&mut self) -> Option<Segment> {
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

    /// Adds `run` frames starting at `frame` into `dst`.
    ///
    /// Segments that were never written read as silence.
    fn add_into(&self, frame: u64, dst: &mut [f32], run: usize, ramp: Ramp) {
        let (index, offset) = split(frame);
        let Some(Some(segment)) = self.segments.get(index) else {
            return;
        };
        let src = &segment.data[offset * self.channels..(offset + run) * self.channels];
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
            borrowed: false,
        }
    }

    /// Length of the loop.
    pub fn len(&self) -> Frames {
        self.len
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

    /// Returns the clip's segments to `pool`, leaving it empty and reusable.
    pub fn release_segments(&mut self, pool: &mut SegmentPool) {
        self.buffer.drain_into(pool);
    }

    /// Adds the loop into `dst` at `ramp`, from the phase this clip has at `position`.
    ///
    /// `dst` is interleaved and its length must be a multiple of the channel count.
    pub fn mix_into(&self, position: Frames, dst: &mut [f32], ramp: Ramp) {
        let len = self.len.0;
        if len == 0 || ramp.is_silent() {
            return;
        }

        let total = dst.len() / self.channels;
        // Modular rather than saturating: a clip loaded from a session can sit ahead of
        // the transport, and clamping to zero replays one fragment until it catches up.
        let mut phase = (position.0 % len + len - self.recorded_at.0 % len) % len;
        let mut done = 0;

        while done < total {
            let offset = phase % SEGMENT_FRAMES_U64;
            let run = (total - done)
                .min(SEGMENT_FRAMES - as_usize(offset))
                .min(as_usize(len - phase));

            let slice = &mut dst[done * self.channels..(done + run) * self.channels];
            self.buffer.add_into(phase, slice, run, ramp.from(done));

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
