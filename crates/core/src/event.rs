//! Reports from the realtime side. `Copy` and allocation-free.

use crate::ids::{ClipId, SlotAddr};
use crate::slot::SlotState;
use crate::time::Frames;

/// Something the realtime side observed.
#[derive(Debug, Clone, Copy, PartialEq)]
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
        /// How many the transport has produced since the engine was built.
        ///
        /// Never goes backwards, and a rewind produces none. Send on the difference from
        /// the last total taken.
        total: u64,
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
    /// The tempo the transport is running at.
    ///
    /// Sent whenever it is set or refused, and on a resync.
    Tempo {
        /// Beats per minute.
        bpm: f64,
    },
    /// A take produced no clip, and the pad is left empty.
    ///
    /// Either it never started for want of storage, or it ran out part way and was
    /// discarded.
    RecordingRefused {
        /// Which pad was armed.
        addr: SlotAddr,
    },
    /// A recording could not be given storage for every frame it has covered.
    ///
    /// Capture carries on, but the take is discarded when it finishes. Sent once per take.
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
    /// A load held more audio than the pool allows, and was refused whole.
    LoadRefused {
        /// Segments the session needs.
        wanted: u32,
        /// Segments the pool holds.
        allowed: u32,
    },
    /// Every pad holding a clip has been published. Sent even when none did.
    SnapshotComplete {
        /// Which request this answers.
        request: u32,
        /// How many pads were published and fit.
        clips: u32,
        /// How many pads there were to publish. More than `clips` means some were lost.
        expected: u32,
    },
}

/// Which report an [`Event`] is, without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// [`Event::SlotChanged`].
    SlotChanged,
    /// [`Event::Bar`].
    Bar,
    /// [`Event::Clock`].
    Clock,
    /// [`Event::Beat`].
    Beat,
    /// [`Event::ClipRecorded`].
    ClipRecorded,
    /// [`Event::ClipReleased`].
    ClipReleased,
    /// [`Event::Tempo`].
    Tempo,
    /// [`Event::RecordingRefused`].
    RecordingRefused,
    /// [`Event::RecordBufferLow`].
    RecordBufferLow,
    /// [`Event::Clipped`].
    Clipped,
    /// [`Event::Xrun`].
    Xrun,
    /// [`Event::TempoRejected`].
    TempoRejected,
    /// [`Event::LoadRefused`].
    LoadRefused,
    /// [`Event::SnapshotComplete`].
    SnapshotComplete,
}

impl EventKind {
    /// Every kind, in the order they index a per-kind count.
    pub const ALL: [Self; 14] = [
        Self::SlotChanged,
        Self::Bar,
        Self::Clock,
        Self::Beat,
        Self::ClipRecorded,
        Self::ClipReleased,
        Self::Tempo,
        Self::RecordingRefused,
        Self::RecordBufferLow,
        Self::Clipped,
        Self::Xrun,
        Self::TempoRejected,
        Self::LoadRefused,
        Self::SnapshotComplete,
    ];

    /// How many kinds there are.
    pub const COUNT: usize = Self::ALL.len();

    /// Its place in a per-kind count.
    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether [`crate::Command::Resync`] puts a lost one right.
    ///
    /// A resync republishes every pad's state and the tempo the transport is actually
    /// running at. The rest are transient or carry their own recovery.
    pub fn is_replayed(self) -> bool {
        matches!(self, Self::SlotChanged | Self::Tempo | Self::TempoRejected)
    }

    /// What to call one of these when saying which reports were lost.
    ///
    /// Singular, and takes a plain `s` in the plural.
    pub fn name(self) -> &'static str {
        match self {
            Self::SlotChanged => "slot change",
            Self::Bar => "bar",
            Self::Clock => "clock tick",
            Self::Beat => "beat",
            Self::ClipRecorded => "recording",
            Self::ClipReleased => "clip release",
            Self::Tempo => "tempo report",
            Self::RecordingRefused => "recording refusal",
            Self::RecordBufferLow => "buffer warning",
            Self::Clipped => "clipping report",
            Self::Xrun => "short capture report",
            Self::TempoRejected => "tempo refusal",
            Self::LoadRefused => "load refusal",
            Self::SnapshotComplete => "snapshot completion",
        }
    }
}

impl Event {
    /// Which report this is.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::SlotChanged { .. } => EventKind::SlotChanged,
            Self::Bar { .. } => EventKind::Bar,
            Self::Clock { .. } => EventKind::Clock,
            Self::Beat { .. } => EventKind::Beat,
            Self::ClipRecorded { .. } => EventKind::ClipRecorded,
            Self::ClipReleased { .. } => EventKind::ClipReleased,
            Self::Tempo { .. } => EventKind::Tempo,
            Self::RecordingRefused { .. } => EventKind::RecordingRefused,
            Self::RecordBufferLow { .. } => EventKind::RecordBufferLow,
            Self::Clipped { .. } => EventKind::Clipped,
            Self::Xrun { .. } => EventKind::Xrun,
            Self::TempoRejected => EventKind::TempoRejected,
            Self::LoadRefused { .. } => EventKind::LoadRefused,
            Self::SnapshotComplete { .. } => EventKind::SnapshotComplete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_its_own_index() {
        for (index, kind) in EventKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), index, "{kind:?} is out of place");
        }
    }

    #[test]
    fn a_resync_covers_the_state_the_controller_mirrors() {
        assert!(EventKind::SlotChanged.is_replayed());
        assert!(EventKind::Tempo.is_replayed());
        assert!(EventKind::TempoRejected.is_replayed());
        assert!(!EventKind::Beat.is_replayed(), "the next beat corrects it");
        assert!(!EventKind::Clock.is_replayed(), "the total corrects itself");
    }
}
