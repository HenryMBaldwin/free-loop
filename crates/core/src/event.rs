//! Reports from the realtime side. `Copy` and allocation-free.

use crate::ids::{ClipId, SlotAddr};
use crate::slot::SlotState;
use crate::time::Frames;

/// Something the realtime side observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A slot moved to a new state.
    SlotChanged {
        /// Which pad.
        addr: SlotAddr,
        /// Its new state.
        state: SlotState,
    },
    /// The transport crossed a bar line.
    Bar {
        /// Bar index from the transport origin.
        bar: u64,
    },
    /// MIDI clock ticks have passed.
    Clock {
        /// How many since the last report.
        ticks: u32,
    },
    /// The transport crossed a beat.
    Beat {
        /// Bar index from the transport origin.
        bar: u64,
        /// Beat index within the bar, zero-based.
        beat: u32,
    },
    /// A recording was sealed into a clip.
    ClipRecorded {
        /// Which pad holds it.
        addr: SlotAddr,
        /// The new clip.
        clip: ClipId,
        /// Its length. Always a whole number of bars.
        len: Frames,
    },
    /// A clip is no longer referenced by any slot.
    ClipReleased {
        /// The clip that was let go.
        clip: ClipId,
    },
    /// A recording could not start because no storage was free. The pad is left empty.
    RecordingRefused {
        /// Which pad was armed.
        addr: SlotAddr,
    },
    /// A recording is running out of preallocated buffer. Capture stops early unless
    /// more is supplied.
    RecordBufferLow {
        /// Which pad is recording.
        addr: SlotAddr,
    },
    /// The mix went past full scale and was held at the limit.
    Clipped {
        /// How many samples were held.
        samples: u32,
    },
    /// Captured input was unavailable; silence was substituted.
    Xrun {
        /// How many frames were lost.
        frames: u64,
    },
    /// A tempo change was refused because clips already exist.
    TempoRejected,
    /// Every pad holding a clip has been published. Sent even when none did.
    SnapshotComplete {
        /// How many pads were published.
        clips: u32,
    },
}
