//! Control instructions for the realtime side. `Copy` and allocation-free.
//!
//! Only instructions whose effect depends on when they arrive.

use crate::ids::{SlotAddr, TrackId};
use crate::settings::Settings;
use crate::time::{Subdivision, Tempo, TimeSignature};

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
    /// Empty every pad, leaving nothing loaded.
    ClearAll,
    /// Report every pad's state again, for a reader that has missed some.
    Resync,
    /// Publish every pad holding a clip, tagged with `request` so a reader can tell one
    /// answer from another.
    Snapshot {
        /// Identifies this request.
        request: u32,
    },
    /// Send the transport back to the start, with every loop at its beginning.
    Rewind,
    /// Freeze or resume the transport.
    ///
    /// A frozen transport holds its position, so loops keep their phase.
    SetPaused(bool),
    /// Turn the click on or off.
    SetClickEnabled(bool),
    /// Set the click level, `0.0..=1.0`.
    SetClickLevel(f32),
    /// Set how often the click sounds.
    SetClickSubdivision(Subdivision),
    /// Change the tempo. Rejected once any clip exists, with
    /// [`crate::event::Event::TempoRejected`].
    SetTempo(Tempo),
    /// Change the time signature. Rejected once any clip exists, with
    /// [`crate::event::Event::TimeSignatureRejected`].
    SetTimeSignature(TimeSignature),
    /// Take the whole of the track settings.
    SetSettings(Settings),
}
