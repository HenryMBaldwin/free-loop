//! Whole-state settings the controller holds and the engine follows.

use crate::gain::UNITY_STEP;
use crate::ids::{LaunchMode, PadMask, Polyphony, TRACK_COUNT, TrackInput};
use crate::pan::CENTRE_STEP;

/// Everything the controller sets as a whole value.
///
/// Delivered latest-wins: an unread copy is replaced, not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How loud each track plays, as a step on the gain ladder.
    pub gains: [u8; TRACK_COUNT],
    /// Pads that do not sound.
    pub muted: PadMask,
    /// Pads that sound to the exclusion of the rest. Empty means no solo.
    pub soloed: PadMask,
    /// Which input each track records.
    pub inputs: [TrackInput; TRACK_COUNT],
    /// Where each track's clips are anchored when launched.
    pub launch_modes: [LaunchMode; TRACK_COUNT],
    /// Beats each track opens its loops from the tail for. Zero takes the head.
    pub pickups: [u8; TRACK_COUNT],
    /// How many of each track's loops may sound at once.
    pub polyphony: [Polyphony; TRACK_COUNT],
    /// Where each track sits across the stereo field, as a step on the pan row.
    pub pans: [u8; TRACK_COUNT],
}

impl Settings {
    /// Every track at unity on the default input, with nothing muted or soloed.
    pub fn new() -> Self {
        Self {
            gains: [UNITY_STEP; TRACK_COUNT],
            muted: 0,
            soloed: 0,
            inputs: [TrackInput::default(); TRACK_COUNT],
            launch_modes: [LaunchMode::default(); TRACK_COUNT],
            pickups: [0; TRACK_COUNT],
            polyphony: [Polyphony::default(); TRACK_COUNT],
            pans: [CENTRE_STEP; TRACK_COUNT],
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}
