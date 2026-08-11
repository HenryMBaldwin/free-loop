//! Control instructions for the realtime side. `Copy` and allocation-free.

use crate::gain::GAIN_STEPS;
use crate::ids::{LaunchMode, PadMask, SlotAddr, TRACK_COUNT, TrackId, TrackInput};
use crate::time::Tempo;

const _: () = assert!(GAIN_STEPS == 8);

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
    /// Set how loud each track plays, as a step on the gain ladder. Sent whole, like the
    /// mute masks.
    SetGains([u8; TRACK_COUNT]),
    /// Set which input each track records.
    SetInputs([TrackInput; TRACK_COUNT]),
    /// Set where each track's clips are anchored when launched.
    SetLaunchModes([LaunchMode; TRACK_COUNT]),
    /// Publish a reference to every pad that holds a clip.
    /// Report every pad's state again, for a reader that has missed some.
    Resync,
    /// Publish every pad holding a clip, tagged with `request` so a reader can tell one
    /// answer from another.
    Snapshot {
        /// Identifies this request.
        request: u32,
    },
    /// Set which pads are silenced and which are soloed. Sent whole, not as toggles.
    SetMutes {
        /// Pads that do not sound.
        muted: PadMask,
        /// Pads that sound to the exclusion of the rest. Empty means no solo.
        soloed: PadMask,
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
    /// Change the tempo. Rejected once any clip exists, with
    /// [`crate::event::Event::TempoRejected`].
    SetTempo(Tempo),
}
