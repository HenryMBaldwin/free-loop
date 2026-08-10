//! Musical time.
//!
//! The transport free-runs from process start at a fixed tempo. All positions are frame
//! counts from that origin.
//!
//! Frames per bar is rounded to an integer once; everything else derives from it with
//! integer math. Bar `n` starts at exactly `n * frames_per_bar`, so there is no
//! accumulating drift, only a fixed tempo error of under half a frame per bar.

use core::ops::{Add, AddAssign, Sub, SubAssign};

/// An invalid musical-time value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    /// Sample rate was zero.
    #[error("sample rate must be greater than zero")]
    ZeroSampleRate,
    /// Tempo outside the supported range.
    #[error("tempo must be between {MIN_BPM} and {MAX_BPM} bpm")]
    TempoOutOfRange,
    /// Beats per bar was zero, or the beat unit was not a power of two.
    #[error("time signature must have at least one beat per bar and a power-of-two beat unit")]
    InvalidTimeSignature,
    /// Bar length was under one frame or over `u32::MAX` frames.
    #[error("the resulting bar length is not representable")]
    BarLengthUnrepresentable,
}

/// MIDI clock ticks in a quarter note.
pub const CLOCK_TICKS_PER_QUARTER: u64 = 24;

/// Lowest supported tempo, in beats per minute.
pub const MIN_BPM: f64 = 20.0;
/// Highest supported tempo, in beats per minute.
pub const MAX_BPM: f64 = 300.0;

/// A position or duration in sample frames, counted from the transport origin.
///
/// One frame is one sample per channel, so this is channel-count independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Frames(pub u64);

impl Frames {
    /// The transport origin.
    pub const ZERO: Self = Self(0);

    /// Converts a duration in milliseconds at the given sample rate.
    pub fn from_millis(millis: u64, sample_rate: SampleRate) -> Self {
        Self(millis.saturating_mul(u64::from(sample_rate.hz())) / 1000)
    }

    /// Subtraction that saturates at the origin.
    #[must_use]
    pub fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl Add for Frames {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Frames {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for Frames {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl SubAssign for Frames {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

/// A sample rate in hertz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SampleRate(u32);

impl SampleRate {
    /// # Errors
    ///
    /// [`TimeError::ZeroSampleRate`] if `hz` is zero.
    pub fn new(hz: u32) -> Result<Self, TimeError> {
        if hz == 0 {
            return Err(TimeError::ZeroSampleRate);
        }
        Ok(Self(hz))
    }

    /// The rate in hertz.
    pub fn hz(self) -> u32 {
        self.0
    }
}

/// A tempo in beats per minute.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Tempo(f64);

impl Tempo {
    /// # Errors
    ///
    /// [`TimeError::TempoOutOfRange`] if `bpm` is not finite or falls outside
    /// [`MIN_BPM`]..=[`MAX_BPM`].
    pub fn new(bpm: f64) -> Result<Self, TimeError> {
        if !bpm.is_finite() || !(MIN_BPM..=MAX_BPM).contains(&bpm) {
            return Err(TimeError::TempoOutOfRange);
        }
        Ok(Self(bpm))
    }

    /// The tempo in beats per minute.
    pub fn bpm(self) -> f64 {
        self.0
    }
}

/// A time signature. The grid math is generic, though only
/// [`TimeSignature::FOUR_FOUR`] is used today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeSignature {
    beats_per_bar: u32,
    beat_unit: u32,
}

impl TimeSignature {
    /// Common time.
    pub const FOUR_FOUR: Self = Self {
        beats_per_bar: 4,
        beat_unit: 4,
    };

    /// # Errors
    ///
    /// [`TimeError::InvalidTimeSignature`] if `beats_per_bar` is zero, or `beat_unit`
    /// is zero or not a power of two.
    pub fn new(beats_per_bar: u32, beat_unit: u32) -> Result<Self, TimeError> {
        if beats_per_bar == 0 || beat_unit == 0 || !beat_unit.is_power_of_two() {
            return Err(TimeError::InvalidTimeSignature);
        }
        Ok(Self {
            beats_per_bar,
            beat_unit,
        })
    }

    /// Beats in one bar. The numerator.
    pub fn beats_per_bar(self) -> u32 {
        self.beats_per_bar
    }

    /// Note value that gets the beat. The denominator.
    pub fn beat_unit(self) -> u32 {
        self.beat_unit
    }
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self::FOUR_FOUR
    }
}

/// Maps frame positions to musical time. `Copy` and stateless.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarGrid {
    sample_rate: SampleRate,
    tempo: Tempo,
    time_signature: TimeSignature,
    frames_per_bar: u64,
}

impl BarGrid {
    /// Builds a grid, rounding the bar length to a whole number of frames.
    ///
    /// # Errors
    ///
    /// [`TimeError::BarLengthUnrepresentable`] if the resulting bar is shorter than one
    /// frame or longer than `u32::MAX` frames.
    pub fn new(
        sample_rate: SampleRate,
        tempo: Tempo,
        time_signature: TimeSignature,
    ) -> Result<Self, TimeError> {
        let quarters_per_bar =
            f64::from(time_signature.beats_per_bar()) * 4.0 / f64::from(time_signature.beat_unit());
        let seconds_per_bar = 60.0 / tempo.bpm() * quarters_per_bar;
        let frames_per_bar = (f64::from(sample_rate.hz()) * seconds_per_bar).round();

        // Bounded by u32 so `frames_per_bar * bars` cannot overflow a u64 downstream.
        if !frames_per_bar.is_finite()
            || frames_per_bar < 1.0
            || frames_per_bar > f64::from(u32::MAX)
        {
            return Err(TimeError::BarLengthUnrepresentable);
        }

        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "range checked above"
        )]
        let frames_per_bar = frames_per_bar as u64;

        Ok(Self {
            sample_rate,
            tempo,
            time_signature,
            frames_per_bar,
        })
    }

    /// The sample rate this grid was built for.
    pub fn sample_rate(self) -> SampleRate {
        self.sample_rate
    }

    /// The tempo this grid was built for.
    pub fn tempo(self) -> Tempo {
        self.tempo
    }

    /// The time signature this grid was built for.
    pub fn time_signature(self) -> TimeSignature {
        self.time_signature
    }

    /// Length of one bar.
    pub fn frames_per_bar(self) -> Frames {
        Frames(self.frames_per_bar)
    }

    /// Length of `bars` bars.
    pub fn bars(self, bars: u32) -> Frames {
        Frames(self.frames_per_bar * u64::from(bars))
    }

    /// Which bar `pos` falls in, counting from zero at the origin.
    pub fn bar_of(self, pos: Frames) -> u64 {
        pos.0 / self.frames_per_bar
    }

    /// Where `bar` starts.
    pub fn bar_start(self, bar: u64) -> Frames {
        Frames(bar * self.frames_per_bar)
    }

    /// The bar boundary at or after `pos`.
    pub fn next_boundary(self, pos: Frames) -> Frames {
        let into_bar = pos.0 % self.frames_per_bar;
        if into_bar == 0 {
            pos
        } else {
            Frames(pos.0 - into_bar + self.frames_per_bar)
        }
    }

    /// The bar boundary at or before `pos`.
    pub fn previous_boundary(self, pos: Frames) -> Frames {
        Frames(pos.0 - pos.0 % self.frames_per_bar)
    }

    /// Offset of beat `beat` from the start of its bar.
    pub fn beat_offset(self, beat: u32) -> Frames {
        let beats = u64::from(self.time_signature.beats_per_bar());
        Frames(self.frames_per_bar * u64::from(beat) / beats)
    }

    /// The bar and beat index `pos` falls in.
    pub fn beat_of(self, pos: Frames) -> (u64, u32) {
        let bar = self.bar_of(pos);
        let into_bar = pos.0 - bar * self.frames_per_bar;
        let beats = u64::from(self.time_signature.beats_per_bar());
        // Exact inverse of `beat_offset`. Dividing `into_bar * beats` directly would
        // floor a second time and land a frame short of the reported beat.
        let beat = ((into_bar + 1) * beats - 1) / self.frames_per_bar;

        #[expect(
            clippy::cast_possible_truncation,
            reason = "beat < beats_per_bar, a u32"
        )]
        let beat = beat as u32;
        (bar, beat)
    }

    /// The next beat boundary strictly after `pos`.
    pub fn next_beat_boundary(self, pos: Frames) -> Frames {
        let (bar, beat) = self.beat_of(pos);
        let beats = self.time_signature.beats_per_bar();
        if beat + 1 < beats {
            self.bar_start(bar) + self.beat_offset(beat + 1)
        } else {
            self.bar_start(bar + 1)
        }
    }

    /// How many MIDI clock ticks have passed by `pos`.
    ///
    /// Ticks are counted per quarter note rather than per beat, so a bar of 7/8 carries
    /// half as many as a bar of 7/4.
    pub fn clock_ticks_at(self, pos: Frames) -> u64 {
        let quarters_per_bar = u128::from(self.time_signature.beats_per_bar()) * 4;
        let per_bar = u128::from(CLOCK_TICKS_PER_QUARTER) * quarters_per_bar;
        let divisor = u128::from(self.time_signature.beat_unit()) * u128::from(self.frames_per_bar);

        let ticks = u128::from(pos.0) * per_bar / divisor;
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }

    /// The position under `target` that sits at the same musical place as `position`
    /// does under `self`.
    ///
    /// A tempo change rebuilds the grid, so the same frame count lands on a different
    /// bar and beat. Moving the transport with the grid keeps the bar number and the
    /// fraction through it, so the next beat arrives one new-tempo interval later
    /// instead of the click jumping.
    pub fn rebase_onto(self, position: Frames, target: Self) -> Frames {
        let bar = self.bar_of(position);
        let into_bar = position.0 - self.bar_start(bar).0;
        let scaled = into_bar * target.frames_per_bar / self.frames_per_bar;
        Frames(target.bar_start(bar).0 + scaled)
    }

    /// Where a recording begun at `start` ends, given stop was pressed at `pressed_at`.
    ///
    /// Rounds back to the bar line that just passed, so a take is whatever whole bars
    /// were finished before the press. A bar in progress is discarded rather than
    /// completed. Press after the bar you want, not during it.
    ///
    /// Result is at least one bar. `start` must be on a boundary.
    pub fn quantize_record_end(self, start: Frames, pressed_at: Frames) -> Frames {
        let end = self.previous_boundary(pressed_at);
        let min_end = Frames(start.0 + self.frames_per_bar);
        if end < min_end { min_end } else { end }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;

    fn grid(bpm: f64) -> BarGrid {
        BarGrid::new(
            SampleRate::new(48_000).unwrap(),
            Tempo::new(bpm).unwrap(),
            TimeSignature::FOUR_FOUR,
        )
        .unwrap()
    }

    #[test]
    fn bar_length_at_common_tempos() {
        assert_eq!(grid(120.0).frames_per_bar(), Frames(96_000));
        assert_eq!(grid(60.0).frames_per_bar(), Frames(192_000));
        // A bar that is not a whole number of frames rounds cleanly.
        assert_eq!(grid(123.0).frames_per_bar(), Frames(93_659));
    }

    #[test]
    fn bars_do_not_drift_at_awkward_tempos() {
        let g = grid(123.0);
        assert_eq!(g.bar_start(1000), Frames(g.frames_per_bar().0 * 1000));
        assert_eq!(g.bar_of(g.bar_start(1000)), 1000);
        assert_eq!(g.bar_of(g.bar_start(1000).saturating_sub(Frames(1))), 999);
    }

    #[test]
    fn tempo_and_signature_are_validated() {
        assert_eq!(Tempo::new(0.0), Err(TimeError::TempoOutOfRange));
        assert_eq!(Tempo::new(f64::NAN), Err(TimeError::TempoOutOfRange));
        assert_eq!(Tempo::new(1000.0), Err(TimeError::TempoOutOfRange));
        assert_eq!(SampleRate::new(0), Err(TimeError::ZeroSampleRate));
        assert_eq!(
            TimeSignature::new(4, 3),
            Err(TimeError::InvalidTimeSignature)
        );
        assert_eq!(
            TimeSignature::new(0, 4),
            Err(TimeError::InvalidTimeSignature)
        );
        assert!(TimeSignature::new(7, 8).is_ok());
    }

    #[test]
    fn odd_meter_bar_length() {
        // 7/8 at 120 bpm = 3.5 quarters = 1.75 s.
        let g = BarGrid::new(
            SampleRate::new(48_000).unwrap(),
            Tempo::new(120.0).unwrap(),
            TimeSignature::new(7, 8).unwrap(),
        )
        .unwrap();
        assert_eq!(g.frames_per_bar(), Frames(84_000));
    }

    #[test]
    fn next_boundary_is_inclusive_of_an_exact_hit() {
        let g = grid(120.0);
        assert_eq!(g.next_boundary(Frames(0)), Frames(0));
        assert_eq!(g.next_boundary(Frames(1)), Frames(96_000));
        assert_eq!(g.next_boundary(Frames(96_000)), Frames(96_000));
        assert_eq!(g.next_boundary(Frames(96_001)), Frames(192_000));
    }

    #[test]
    fn beats_partition_the_bar_exactly() {
        let g = grid(123.0);
        let fpb = g.frames_per_bar().0;
        assert_eq!(g.beat_offset(0), Frames(0));
        assert_eq!(g.beat_offset(4), Frames(fpb));

        for beat in 0..4 {
            let start = g.beat_offset(beat).0;
            let end = g.beat_offset(beat + 1).0;
            assert!(start < end);
            assert_eq!(g.beat_of(Frames(start)), (0, beat));
            assert_eq!(g.beat_of(Frames(end - 1)), (0, beat));
        }
    }

    #[test]
    fn next_beat_boundary_rolls_into_the_next_bar() {
        let g = grid(120.0);
        assert_eq!(g.next_beat_boundary(Frames(0)), Frames(24_000));
        assert_eq!(g.next_beat_boundary(Frames(23_999)), Frames(24_000));
        assert_eq!(g.next_beat_boundary(Frames(72_001)), Frames(96_000));
    }

    #[test]
    fn a_bar_of_four_four_carries_ninety_six_clock_ticks() {
        let g = grid(120.0);
        assert_eq!(g.clock_ticks_at(Frames::ZERO), 0);
        assert_eq!(g.clock_ticks_at(g.frames_per_bar()), 96);
        assert_eq!(g.clock_ticks_at(g.bar_start(4)), 4 * 96);
    }

    #[test]
    fn clock_ticks_land_on_the_beats() {
        let g = grid(120.0);
        for beat in 0..4 {
            assert_eq!(
                g.clock_ticks_at(g.beat_offset(beat)),
                u64::from(beat) * CLOCK_TICKS_PER_QUARTER
            );
        }
    }

    #[test]
    fn clock_ticks_count_quarters_not_beats() {
        // 7/8 is three and a half quarters, so 84 ticks rather than 7 beats worth.
        let g = BarGrid::new(
            SampleRate::new(48_000).unwrap(),
            Tempo::new(120.0).unwrap(),
            TimeSignature::new(7, 8).unwrap(),
        )
        .unwrap();
        assert_eq!(g.clock_ticks_at(g.frames_per_bar()), 84);
    }

    #[test]
    fn clock_ticks_never_go_backwards() {
        let g = grid(123.0);
        let mut previous = 0;
        for step in 0..2_000 {
            let now = g.clock_ticks_at(Frames(step * 137));
            assert!(now >= previous);
            previous = now;
        }
    }

    #[test]
    fn a_faster_tempo_ticks_more_often() {
        let slow = grid(60.0).clock_ticks_at(Frames(48_000));
        let fast = grid(120.0).clock_ticks_at(Frames(48_000));
        assert_eq!(fast, slow * 2);
    }

    #[test]
    fn rebasing_keeps_the_bar_and_the_fraction_through_it() {
        let slow = grid(60.0);
        let fast = grid(120.0);

        // Half way through bar 3 at 60 bpm.
        let position = slow.bar_start(3) + Frames(slow.frames_per_bar().0 / 2);
        let moved = slow.rebase_onto(position, fast);

        assert_eq!(fast.bar_of(moved), 3);
        assert_eq!(
            moved,
            fast.bar_start(3) + Frames(fast.frames_per_bar().0 / 2)
        );
    }

    #[test]
    fn rebasing_holds_the_beat_within_the_bar() {
        let from = grid(120.0);
        let to = grid(140.0);

        for beat in 0..4 {
            let position = from.bar_start(2) + from.beat_offset(beat);
            let moved = from.rebase_onto(position, to);
            assert_eq!(to.beat_of(moved), (2, beat), "beat {beat}");
        }
    }

    #[test]
    fn rebasing_onto_the_same_grid_changes_nothing() {
        let g = grid(123.0);
        let position = Frames(1_234_567);
        assert_eq!(g.rebase_onto(position, g), position);
    }

    #[test]
    fn record_end_takes_the_bars_that_finished() {
        let g = grid(120.0);
        let start = Frames(96_000);

        // Stop pressed just after the bar 3 line: the two finished bars.
        let end = g.quantize_record_end(start, Frames(288_000 + 5_000));
        assert_eq!(end, Frames(288_000));
        assert_eq!((end - start).0 / g.frames_per_bar().0, 2);
    }

    #[test]
    fn a_bar_still_in_progress_is_discarded() {
        let g = grid(120.0);
        let start = Frames(96_000);

        // Anywhere inside bar 3 gives the same answer: bars 1 and 2.
        for into_bar in [1, 24_000, 48_000, 95_999] {
            assert_eq!(
                g.quantize_record_end(start, Frames(288_000 + into_bar)),
                Frames(288_000),
                "{into_bar} frames into the bar"
            );
        }
    }

    #[test]
    fn a_press_on_the_line_keeps_the_bar_that_just_ended() {
        let g = grid(120.0);
        assert_eq!(
            g.quantize_record_end(Frames(96_000), Frames(288_000)),
            Frames(288_000)
        );
    }

    #[test]
    fn record_end_is_never_shorter_than_one_bar() {
        let g = grid(120.0);
        // Stop pressed in the very first bar, which has not finished.
        let end = g.quantize_record_end(Frames(96_000), Frames(96_100));
        assert_eq!(end, Frames(192_000));
    }
}
