//! Identifiers for the fixed 8×8 grid and for recorded clips.

/// Number of tracks. Columns on the Launchpad grid.
pub const TRACK_COUNT: usize = 8;
/// Number of slots per track. Rows on the Launchpad grid.
pub const SLOT_COUNT: usize = 8;

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
    /// Which track — the grid column.
    pub track: TrackId,
    /// Which slot on that track — the grid row.
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

/// Identifies a recorded clip.
///
/// Supplied via [`crate::slot::Ctx`] rather than generated internally, which keeps
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
    use super::*;

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
    fn addresses_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for addr in SlotAddr::all() {
            assert!(seen.insert(addr), "duplicate address {addr:?}");
        }
    }
}
