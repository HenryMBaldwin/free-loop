//! Track position across the stereo field, in the steps a row of pads offers.
//!
//! An odd number of steps, so one of them is exactly centre.

use std::f32::consts::{FRAC_1_SQRT_2, SQRT_2};

/// Steps a row offers, one short of its width so centre lands on a pad.
pub const PAN_STEPS: usize = 7;

/// The step that sits dead centre.
pub const CENTRE_STEP: u8 = 3;

/// The step at the right-hand end of the row.
pub const LAST_STEP: u8 = CENTRE_STEP * 2;

const _: () = assert!(PAN_STEPS == CENTRE_STEP as usize * 2 + 1);

/// Where a track sits across the stereo field.
///
/// `width` collapses the source's own stereo image as the track moves off centre, so a
/// hard-panned take keeps both its channels instead of dropping one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pan {
    left: f32,
    right: f32,
    width: f32,
}

impl Pan {
    /// Dead centre, with the source's image untouched.
    pub const CENTRE: Self = LADDER[CENTRE_STEP as usize];

    /// Gain for the left output.
    pub const fn left(self) -> f32 {
        self.left
    }

    /// Gain for the right output.
    pub const fn right(self) -> f32 {
        self.right
    }

    /// How much of the source's own stereo image survives, from one to nothing.
    pub const fn width(self) -> f32 {
        self.width
    }

    /// This pan moved at most `most` of the way toward `target`.
    ///
    /// Every component travels at the same rate, which is the caller's declick length.
    #[must_use]
    pub fn toward(self, target: Self, most: f32) -> Self {
        Self {
            left: step_toward(self.left, target.left, most),
            right: step_toward(self.right, target.right, most),
            width: step_toward(self.width, target.width, most),
        }
    }
}

fn step_toward(from: f32, to: f32, most: f32) -> f32 {
    if to > from {
        (from + most).min(to)
    } else {
        (from - most).max(to)
    }
}

/// Constant power across the sweep, scaled so centre plays at the recorded level.
///
/// The pair squares to two at every step, which is what a centred track already put out
/// before there was anywhere else to put it. `width` falls to nothing at the ends, where
/// both source channels are summed onto the side that is left.
const LADDER: [Pan; PAN_STEPS] = [
    pan(SQRT_2, 0.0, 0.0),
    pan(1.366_025_4, 0.366_025_4, 1.0 / 3.0),
    pan(1.224_744_9, FRAC_1_SQRT_2, 2.0 / 3.0),
    pan(1.0, 1.0, 1.0),
    pan(FRAC_1_SQRT_2, 1.224_744_9, 2.0 / 3.0),
    pan(0.366_025_4, 1.366_025_4, 1.0 / 3.0),
    pan(0.0, SQRT_2, 0.0),
];

const fn pan(left: f32, right: f32, width: f32) -> Pan {
    Pan { left, right, width }
}

/// A track's step nudged by one of its loops, held inside the row.
///
/// Loop pan is an offset from centre, not a position of its own.
pub fn compose_steps(track: u8, loop_step: u8) -> u8 {
    let track = track.min(LAST_STEP);
    let nudge = loop_step.min(LAST_STEP);
    track
        .saturating_add(nudge)
        .saturating_sub(CENTRE_STEP)
        .min(LAST_STEP)
}

/// Where a step sits. Steps past the end read as centre.
pub fn pan_for_step(step: u8) -> Pan {
    LADDER
        .get(usize::from(step))
        .copied()
        .unwrap_or(Pan::CENTRE)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        reason = "tests should fail loudly, and the ladder is a table"
    )]

    use super::*;

    /// The power a centred track put out before there was anywhere else to put it.
    const CENTRE_POWER: f32 = 2.0;

    #[test]
    fn the_middle_step_is_centre() {
        let centre = pan_for_step(CENTRE_STEP);
        assert_eq!(centre, Pan::CENTRE);
        assert_eq!(centre.left(), centre.right());
        assert_eq!(centre.width(), 1.0, "the source's own image is untouched");
    }

    #[test]
    fn a_centred_track_plays_at_the_level_it_was_recorded() {
        assert_eq!(Pan::CENTRE.left(), 1.0);
        assert_eq!(Pan::CENTRE.right(), 1.0);
    }

    #[test]
    fn the_ends_are_one_side_only() {
        let last = u8::try_from(PAN_STEPS - 1).unwrap();
        assert_eq!(pan_for_step(0).right(), 0.0);
        assert_eq!(pan_for_step(last).left(), 0.0);
    }

    #[test]
    fn the_ends_sum_the_source_rather_than_dropping_a_channel() {
        let last = u8::try_from(PAN_STEPS - 1).unwrap();
        assert_eq!(pan_for_step(0).width(), 0.0);
        assert_eq!(pan_for_step(last).width(), 0.0);
    }

    #[test]
    fn the_sweep_holds_its_power() {
        for step in 0..PAN_STEPS {
            let at = pan_for_step(u8::try_from(step).unwrap());
            let power = at.left().mul_add(at.left(), at.right() * at.right());
            assert!(
                (power - CENTRE_POWER).abs() < 1e-6,
                "step {step} is at {power}, not {CENTRE_POWER}"
            );
        }
    }

    #[test]
    fn the_ladder_travels_left_to_right() {
        for step in 1..PAN_STEPS {
            let below = pan_for_step(u8::try_from(step - 1).unwrap());
            let here = pan_for_step(u8::try_from(step).unwrap());
            assert!(here.left() < below.left(), "step {step} does not move");
            assert!(here.right() > below.right(), "step {step} does not move");
        }
    }

    #[test]
    fn the_ladder_is_a_mirror_of_itself() {
        for step in 0..PAN_STEPS {
            let left = pan_for_step(u8::try_from(step).unwrap());
            let right = pan_for_step(u8::try_from(PAN_STEPS - 1 - step).unwrap());
            assert_eq!(left.left(), right.right());
            assert_eq!(left.width(), right.width());
        }
    }

    #[test]
    fn a_step_past_the_end_reads_as_centre() {
        assert_eq!(pan_for_step(99), Pan::CENTRE);
    }

    #[test]
    fn a_loop_at_centre_leaves_its_track_where_it_was() {
        for track in 0..u8::try_from(PAN_STEPS).unwrap() {
            assert_eq!(compose_steps(track, CENTRE_STEP), track);
        }
    }

    #[test]
    fn a_loop_nudges_its_track_across_the_row() {
        assert_eq!(
            compose_steps(CENTRE_STEP, 6),
            6,
            "centre track, hard right loop"
        );
        assert_eq!(
            compose_steps(CENTRE_STEP, 0),
            0,
            "centre track, hard left loop"
        );
        assert_eq!(compose_steps(0, 6), CENTRE_STEP, "they cancel out");
        assert_eq!(compose_steps(2, 4), CENTRE_STEP);
    }

    #[test]
    fn a_nudge_past_the_end_stops_at_the_end() {
        assert_eq!(compose_steps(6, 6), 6);
        assert_eq!(compose_steps(0, 0), 0);
        assert_eq!(compose_steps(5, 6), 6);
    }

    #[test]
    fn a_step_past_the_row_is_held_inside_it() {
        let last = u8::try_from(PAN_STEPS - 1).unwrap();
        assert_eq!(compose_steps(200, CENTRE_STEP), last);
        assert_eq!(compose_steps(CENTRE_STEP, 200), last);
    }

    #[test]
    fn travel_stops_once_it_arrives() {
        let start = pan_for_step(0);
        let target = pan_for_step(6);
        assert_eq!(start.toward(target, 10.0), target, "no overshoot");
        assert_eq!(target.toward(target, 0.5), target);
    }

    #[test]
    fn travel_moves_toward_the_target_rather_than_jumping() {
        let start = pan_for_step(0);
        let target = pan_for_step(6);
        let moved = start.toward(target, 0.25);
        assert!(moved.left() < start.left() && moved.left() > target.left());
        assert!(moved.right() > start.right() && moved.right() < target.right());
    }
}
