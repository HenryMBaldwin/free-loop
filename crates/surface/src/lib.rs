//! Control surface for Free Loop.
//!
//! Rows are tracks and columns are slots, so a track's eight loops sit side by side. The
//! right-hand column is a separate strip of eight buttons, not part of the grid. The top
//! row carries the transport and session controls, with the beat indicator sharing its
//! first four buttons.
//!
//! - [`led`]: what to show, in device-neutral terms.
//! - [`paint`]: turning session state into a frame. All the colour policy lives here.
//! - [`event`]: what the performer did.
//! - [`mock`]: a surface with no hardware, for tests and headless runs.
//! - [`launchpad`]: the Launchpad X.

pub mod event;
pub mod launchpad;
pub mod led;
pub mod mock;
pub mod paint;
pub mod surface;

pub use event::{Control, SurfaceEvent};
pub use launchpad::LaunchpadX;
pub use led::{
    BEAT_LEDS, CONTROL_COUNT, FIRST_BEAT_LED, Led, LedColor, LedFrame, LedStyle, SIDE_COUNT,
};
pub use mock::MockSurface;
pub use paint::{
    Axis, Chrome, MUTE_SIDE, MUTED, NEW_SIDE, PAUSE_SIDE, SELECTED, SOLO_SIDE, SOLOED, VOLUME_SIDE,
};
pub use surface::{ControlSurface, SurfaceError};
