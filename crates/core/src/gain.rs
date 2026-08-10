//! Track volume, in the eight steps a row of pads offers.
//!
//! The ladder is uneven: silence, three cuts, unity, three boosts.

/// Steps a row offers.
pub const GAIN_STEPS: usize = 8;

/// The step a take plays at the level it was recorded.
pub const UNITY_STEP: u8 = 4;

/// Amplitude for each step: off, then -18, -12 and -6 dB, unity, then +3, +6 and +9.
const LADDER: [f32; GAIN_STEPS] = [0.0, 0.125, 0.25, 0.5, 1.0, 1.413, 2.0, 2.818];

/// The amplitude a step plays at. Steps past the end read as unity.
pub fn gain_for_step(step: u8) -> f32 {
    LADDER.get(usize::from(step)).copied().unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, reason = "the ladder is a table, not a calculation")]

    use super::*;

    #[test]
    fn the_middle_step_is_the_level_it_was_recorded_at() {
        assert_eq!(gain_for_step(UNITY_STEP), 1.0);
    }

    #[test]
    fn the_bottom_step_is_silence() {
        assert_eq!(gain_for_step(0), 0.0);
    }

    #[test]
    fn the_ladder_only_climbs() {
        for step in 1..GAIN_STEPS {
            let below = gain_for_step(u8::try_from(step - 1).unwrap_or(0));
            let here = gain_for_step(u8::try_from(step).unwrap_or(0));
            assert!(here > below, "step {step} does not rise");
        }
    }

    #[test]
    fn most_of_the_ladder_is_below_unity() {
        let below = usize::from(UNITY_STEP);
        let above = GAIN_STEPS - below - 1;
        assert!(below > above);
    }

    #[test]
    fn a_step_past_the_end_plays_untouched() {
        assert_eq!(gain_for_step(99), 1.0);
    }
}
