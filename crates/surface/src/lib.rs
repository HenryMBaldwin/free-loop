//! Control surface for Free Loop.
//!
//! A device: buttons in, lights out. What a button means and what colour it takes are the
//! application's business, not this crate's.
//!
//! Rows are tracks and columns are slots, so a track's eight loops sit side by side. The
//! right-hand column is a separate strip of eight buttons, not part of the grid.
//!
//! - [`led`]: what to show, in device-neutral terms.
//! - [`event`]: what the performer did.
//! - [`mock`]: a surface with no hardware, for tests and headless runs.
//! - [`launchpad`]: the Launchpad X.

pub mod event;
pub mod host;
pub mod launchpad;
pub mod led;
pub mod mock;
pub mod reconnect;
pub mod surface;

pub use event::{Control, SurfaceEvent};
pub use host::HostWatch;
pub use launchpad::{LaunchpadX, output_ports};
pub use led::{
    CONTROL_COUNT, FIRST_BEAT_LED, Led, LedColor, LedFrame, LedStyle, SHADES, SIDE_COUNT,
};
pub use mock::MockSurface;
pub use reconnect::Reconnecting;
pub use surface::{ControlSurface, SurfaceError};
