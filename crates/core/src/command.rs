//! Control instructions for the realtime side. `Copy` and allocation-free.

use crate::ids::{SlotAddr, TrackId};
use crate::time::Tempo;

/// An instruction for the realtime side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// A pad was pressed. The meaning depends on what the slot is doing.
    Press(SlotAddr),
    /// Forget the clip in this slot.
    Clear(SlotAddr),
    /// Stop whatever is sounding on this track, immediately.
    StopTrack(TrackId),
    /// Stop everything, immediately. Recordings in progress are discarded.
    StopAll,
    /// Freeze or resume the transport.
    ///
    /// A frozen transport holds its position, so loops keep their phase and pick up
    /// exactly where they stopped.
    SetPaused(bool),
    /// Turn the click on or off.
    SetClickEnabled(bool),
    /// Set the click level, `0.0..=1.0`.
    SetClickLevel(f32),
    /// Change the tempo. Rejected once any clip exists, with
    /// [`crate::event::Event::TempoRejected`].
    SetTempo(Tempo),
}
