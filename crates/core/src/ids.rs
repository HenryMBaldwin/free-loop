//! Identifiers for the fixed 8×8 grid and for recorded clips.

/// Number of tracks. Columns on the Launchpad grid.
pub const TRACK_COUNT: usize = 8;
/// Number of slots per track. Rows on the Launchpad grid.
pub const SLOT_COUNT: usize = 8;

/// Input channels a track can choose between, one per column of the input page.
pub const INPUT_CHANNELS: usize = SLOT_COUNT;

/// [`TRACK_COUNT`] as the index type, so range checks need no cast.
const TRACK_LIMIT: u8 = 8;
/// [`SLOT_COUNT`] as the index type, so range checks need no cast.
const SLOT_LIMIT: u8 = 8;

const _: () = assert!(TRACK_LIMIT as usize == TRACK_COUNT);
const _: () = assert!(SLOT_LIMIT as usize == SLOT_COUNT);

/// An out-of-range track or slot index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("index {index} is out of range 0..{limit}")]
pub struct IndexOutOfRange {
    /// The offending index.
    pub index: u8,
    /// The exclusive upper bound.
    pub limit: u8,
}

/// A track index in `0..TRACK_COUNT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(u8);

impl TrackId {
    /// # Errors
    ///
    /// [`IndexOutOfRange`] if `index` is not below [`TRACK_COUNT`].
    pub fn new(index: u8) -> Result<Self, IndexOutOfRange> {
        if index >= TRACK_LIMIT {
            return Err(IndexOutOfRange {
                index,
                limit: TRACK_LIMIT,
            });
        }
        Ok(Self(index))
    }

    /// The raw index.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    /// Every track, in order.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..TRACK_LIMIT).map(Self)
    }
}

/// A slot index in `0..SLOT_COUNT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(u8);

impl SlotId {
    /// # Errors
    ///
    /// [`IndexOutOfRange`] if `index` is not below [`SLOT_COUNT`].
    pub fn new(index: u8) -> Result<Self, IndexOutOfRange> {
        if index >= SLOT_LIMIT {
            return Err(IndexOutOfRange {
                index,
                limit: SLOT_LIMIT,
            });
        }
        Ok(Self(index))
    }

    /// The raw index.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    /// Every slot, in order.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..SLOT_LIMIT).map(Self)
    }
}

/// A cell on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotAddr {
    /// Which track. The grid row.
    pub track: TrackId,
    /// Which slot on that track. The grid column.
    pub slot: SlotId,
}

impl SlotAddr {
    pub fn new(track: TrackId, slot: SlotId) -> Self {
        Self { track, slot }
    }

    /// Every cell, track-major.
    pub fn all() -> impl Iterator<Item = Self> {
        TrackId::all().flat_map(|track| SlotId::all().map(move |slot| Self::new(track, slot)))
    }
}

/// A set of pads, one bit each, track-major.
///
/// Kept to one word so it can cross the realtime boundary in a command.
pub type PadMask = u64;

/// The bit for a pad in a [`PadMask`].
pub fn pad_bit(addr: SlotAddr) -> PadMask {
    1 << (addr.track.index() * SLOT_COUNT + addr.slot.index())
}

/// Every pad in a track's row.
pub fn row_mask(track: TrackId) -> PadMask {
    SlotId::all()
        .map(|slot| pad_bit(SlotAddr::new(track, slot)))
        .fold(0, |mask, bit| mask | bit)
}

/// Every pad in a slot's column.
pub fn column_mask(slot: SlotId) -> PadMask {
    TrackId::all()
        .map(|track| pad_bit(SlotAddr::new(track, slot)))
        .fold(0, |mask, bit| mask | bit)
}

/// Identifies a recorded clip. Supplied via [`crate::slot::Ctx`], which keeps
/// [`crate::slot::step`] pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ClipId(pub u32);

impl ClipId {
    /// The id that follows this one.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;

    #[test]
    fn a_pair_puts_the_lower_channel_on_the_left() {
        assert_eq!(TrackInput::pair(3, 1), TrackInput::Pair(1, 3));
        assert_eq!(TrackInput::pair(1, 3), TrackInput::Pair(1, 3));
        assert_eq!(
            TrackInput::pair(2, 2),
            TrackInput::Mono(2),
            "one channel twice is one channel"
        );
    }

    #[test]
    fn an_input_lists_the_channels_it_takes() {
        assert_eq!(TrackInput::Mono(5).channels().as_slice(), &[5]);
        assert_eq!(TrackInput::Pair(1, 3).channels().as_slice(), &[1, 3]);
    }

    #[test]
    fn ids_reject_out_of_range() {
        assert!(TrackId::new(7).is_ok());
        assert!(TrackId::new(8).is_err());
        assert!(SlotId::new(7).is_ok());
        assert!(SlotId::new(8).is_err());
    }

    #[test]
    fn iterators_cover_the_whole_grid() {
        assert_eq!(TrackId::all().count(), TRACK_COUNT);
        assert_eq!(SlotId::all().count(), SLOT_COUNT);
        assert_eq!(SlotAddr::all().count(), TRACK_COUNT * SLOT_COUNT);
    }

    #[test]
    fn every_pad_has_its_own_bit() {
        let mut seen = 0;
        for addr in SlotAddr::all() {
            let bit = pad_bit(addr);
            assert_eq!(seen & bit, 0, "{addr:?} collides");
            seen |= bit;
        }
        assert_eq!(seen.count_ones(), 64);
    }

    #[test]
    fn a_row_and_a_column_meet_at_one_pad() {
        let track = TrackId::new(3).unwrap();
        let slot = SlotId::new(5).unwrap();

        let row = row_mask(track);
        let column = column_mask(slot);
        assert_eq!(row.count_ones(), 8);
        assert_eq!(column.count_ones(), 8);
        assert_eq!(row & column, pad_bit(SlotAddr::new(track, slot)));
    }

    #[test]
    fn rows_cover_the_grid_and_do_not_overlap() {
        let all = TrackId::all().fold(0_u64, |mask, t| mask | row_mask(t));
        assert_eq!(all, u64::MAX);

        let summed: u32 = TrackId::all().map(|t| row_mask(t).count_ones()).sum();
        assert_eq!(summed, 64, "rows overlap");
    }

    #[test]
    fn addresses_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for addr in SlotAddr::all() {
            assert!(seen.insert(addr), "duplicate address {addr:?}");
        }
    }
}

/// The capture channels an input takes, in the order they land on the clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picks {
    channels: [u8; 2],
    count: usize,
}

impl Picks {
    /// The channels, lower first.
    pub fn as_slice(&self) -> &[u8] {
        &self.channels[..self.count]
    }
}

/// Which capture channels a track records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackInput {
    /// One channel, on every channel of the clip.
    Mono(u8),
    /// Two channels as left and right. The lower channel is left.
    Pair(u8, u8),
}

impl Default for TrackInput {
    /// The first two channels, which is a stereo source on any interface.
    fn default() -> Self {
        Self::Pair(0, 1)
    }
}

impl TrackInput {
    /// Two channels as a pair, lower on the left. The same channel twice is mono.
    pub fn pair(one: u8, two: u8) -> Self {
        if one == two {
            return Self::Mono(one);
        }
        Self::Pair(one.min(two), one.max(two))
    }

    /// The channels this takes, lower first.
    pub fn channels(self) -> Picks {
        match self {
            Self::Mono(channel) => Picks {
                channels: [channel, 0],
                count: 1,
            },
            Self::Pair(left, right) => Picks {
                channels: [left, right],
                count: 2,
            },
        }
    }

    /// Whether this takes `channel`.
    pub fn takes(self, channel: u8) -> bool {
        self.channels().as_slice().contains(&channel)
    }
}

/// When a clip's playback position is decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LaunchMode {
    /// The clip sits where it was recorded, so launching drops into whatever part of it
    /// the transport has reached.
    #[default]
    Follow,
    /// The clip is anchored where it is launched, so it plays from its start every time.
    Restart,
}

impl LaunchMode {
    /// The other mode.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Follow => Self::Restart,
            Self::Restart => Self::Follow,
        }
    }

    /// Whether a launch re-anchors the clip.
    pub fn restarts(self) -> bool {
        matches!(self, Self::Restart)
    }
}
