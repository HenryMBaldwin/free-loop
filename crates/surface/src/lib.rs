//! Control surface for Free Loop.
//!
//! Columns are tracks and rows are slots, matching Session view. The right-hand column
//! is reserved for scene launch. The top row carries the transport controls on the left
//! and the beat indicator on the right.
//!
//! - [`led`] — what to show, in device-neutral terms.
//! - [`paint`] — turning session state into a frame. All the colour policy lives here.
//! - [`event`] — what the performer did.
//! - [`mock`] — a surface with no hardware, for tests and headless runs.
//! - [`launchpad`] — the Launchpad X.

pub mod event;
pub mod launchpad;
pub mod led;
pub mod mock;
pub mod paint;
pub mod surface;

pub use event::{Control, SurfaceEvent};
pub use launchpad::LaunchpadX;
pub use led::{BEAT_LEDS, CONTROL_COUNT, FIRST_BEAT_LED, Led, LedColor, LedFrame, LedStyle};
pub use mock::MockSurface;
pub use paint::Chrome;
pub use surface::{ControlSurface, SurfaceError};
