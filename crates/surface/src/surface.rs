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
pub trait ControlSurface {
    /// Lets time pass. Call every pass of the control loop.
    ///
    /// Devices that need no upkeep ignore it.
    fn tick(&mut self, _now: core::time::Duration) {}

    /// Whether a device is attached. Surfaces that cannot lose one are always attached.
    fn is_connected(&self) -> bool {
        true
    }

    /// Whether the device is still there, checked without writing to it.
    ///
    /// A write that fails is not the only way a device goes away: sends to a port that
    /// has vanished can report success, and nothing is written at all while the transport
    /// is stopped and the grid unchanged.
    fn is_present(&self) -> bool {
        true
    }

    /// Appends everything the performer has done since the last call.
    ///
    /// Never blocks.
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

    /// Shows a short string, if the device can.
    ///
    /// Devices with nothing to show it on ignore it. Text takes the grid over while it
    /// runs, so callers should stop it before expecting a frame to be visible.
    ///
    /// # Errors
    ///
    /// [`SurfaceError`] if the device rejected the update.
    fn show_text(&mut self, text: &str) -> Result<(), SurfaceError> {
        let _ = text;
        Ok(())
    }

    /// Passes on MIDI clock ticks, if the device uses them.
    ///
    /// Devices that animate to their own clock ignore them.
    ///
    /// # Errors
    ///
    /// [`SurfaceError`] if the device rejected the update.
    fn send_clock(&mut self, ticks: u32) -> Result<(), SurfaceError> {
        let _ = ticks;
        Ok(())
    }

    /// Stops any text and gives the grid back.
    ///
    /// # Errors
    ///
    /// [`SurfaceError`] if the device rejected the update.
    fn stop_text(&mut self) -> Result<(), SurfaceError> {
        Ok(())
    }
}
