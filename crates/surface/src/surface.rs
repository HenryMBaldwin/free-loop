//! The device-neutral interface.

use crate::event::SurfaceEvent;
use crate::led::LedFrame;

/// The surface could not be reached.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    /// No matching device was found.
    #[error("no control surface found")]
    NotFound,
    /// The device rejected a message or went away.
    #[error("control surface failed: {0}")]
    Device(String),
}

/// A grid controller.
///
/// The trait exists so the looper can run against a mock with no hardware attached, and
/// so a change in the device library stays inside one implementation.
pub trait ControlSurface {
    /// Appends everything the performer has done since the last call.
    ///
    /// Never blocks. Appending rather than returning lets the caller keep one buffer.
    fn poll(&mut self, events: &mut Vec<SurfaceEvent>);

    /// Shows `frame`.
    ///
    /// Implementations may send only what changed since the previous call, so callers
    /// should pass a complete frame every time rather than a delta.
    ///
    /// # Errors
    ///
    /// [`SurfaceError`] if the device rejected the update.
    fn render(&mut self, frame: &LedFrame) -> Result<(), SurfaceError>;

    /// Darkens every button.
    ///
    /// # Errors
    ///
    /// [`SurfaceError`] if the device rejected the update.
    fn clear(&mut self) -> Result<(), SurfaceError>;
}
