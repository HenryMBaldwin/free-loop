//! What a session records besides its audio.

use free_loop_core::{IndexOutOfRange, SlotAddr, SlotId, TrackId};
use serde::{Deserialize, Serialize};

/// The file inside a session directory.
pub const MANIFEST: &str = "session.toml";

/// One pad's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipEntry {
    /// Row on the grid.
    pub track: u8,
    /// Column on the grid.
    pub slot: u8,
    /// Audio file, relative to the session directory.
    pub file: String,
    /// Loop length in frames.
    pub len_frames: u64,
    /// Where the loop sits against the bar grid, in frames from a bar line.
    ///
    /// Stored rather than the absolute position it was recorded at, which means nothing
    /// once the transport has been restarted.
    pub phase_frames: u64,
    /// Whether the pad was sounding.
    pub playing: bool,
}

impl ClipEntry {
    /// The pad this entry belongs to.
    ///
    /// # Errors
    ///
    /// [`IndexOutOfRange`] if the file names a pad outside the grid.
    pub fn addr(&self) -> Result<SlotAddr, IndexOutOfRange> {
        Ok(SlotAddr::new(
            TrackId::new(self.track)?,
            SlotId::new(self.slot)?,
        ))
    }
}

/// Everything a session holds apart from the audio itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Beats per minute.
    pub tempo: f64,
    /// Beats in a bar.
    pub beats_per_bar: u32,
    /// Note value that gets the beat.
    pub beat_unit: u32,
    /// Rate the audio was recorded at.
    pub sample_rate: u32,
    /// Channels in each audio file.
    pub channels: u16,
    /// The pads that hold something.
    #[serde(default)]
    pub clips: Vec<ClipEntry>,
}

impl Manifest {
    /// The audio file name for a pad.
    pub fn file_name(addr: SlotAddr) -> String {
        format!("t{}s{}.wav", addr.track.index(), addr.slot.index())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        reason = "tests should fail loudly, and compare exact stored values"
    )]

    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            tempo: 120.0,
            beats_per_bar: 4,
            beat_unit: 4,
            sample_rate: 48_000,
            channels: 2,
            clips: vec![ClipEntry {
                track: 1,
                slot: 3,
                file: "t1s3.wav".to_owned(),
                len_frames: 96_000,
                phase_frames: 1_234,
                playing: true,
            }],
        }
    }

    #[test]
    fn a_manifest_round_trips_through_toml() {
        let written = toml::to_string(&manifest()).unwrap();
        let read: Manifest = toml::from_str(&written).unwrap();
        assert_eq!(read, manifest());
    }

    #[test]
    fn an_entry_maps_back_to_its_pad() {
        let addr = manifest().clips[0].addr().unwrap();
        assert_eq!(addr.track.index(), 1);
        assert_eq!(addr.slot.index(), 3);
    }

    #[test]
    fn an_entry_off_the_grid_is_refused() {
        let mut entry = manifest().clips[0].clone();
        entry.track = 9;
        assert!(entry.addr().is_err());
    }

    #[test]
    fn a_session_with_no_clips_parses() {
        let text =
            "tempo = 120.0\nbeats_per_bar = 4\nbeat_unit = 4\nsample_rate = 48000\nchannels = 2\n";
        let read: Manifest = toml::from_str(text).unwrap();
        assert!(read.clips.is_empty());
    }

    #[test]
    fn file_names_are_unique_per_pad() {
        let mut seen = std::collections::HashSet::new();
        for addr in SlotAddr::all() {
            assert!(seen.insert(Manifest::file_name(addr)));
        }
        assert_eq!(seen.len(), 64);
    }
}
