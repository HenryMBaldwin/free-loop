//! Whole-state settings the controller holds and the engine follows.

use crate::gain::UNITY_STEP;
use crate::ids::{LaunchMode, PadMask, TRACK_COUNT, TrackInput};

/// Everything the controller sets outright rather than by increment.
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
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}
