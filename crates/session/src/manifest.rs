//! What a session records besides its audio.

use free_loop_core::{IndexOutOfRange, SlotAddr, SlotId, TrackId, UNITY_STEP};
use serde::{Deserialize, Serialize};

/// Level for a clip saved before levels were recorded.
fn unity() -> u8 {
    UNITY_STEP
}

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
    pub phase_frames: u64,
    /// Whether the pad was sounding.
    pub playing: bool,
    /// The step on the gain ladder its track was playing at.
    ///
    /// Stored per clip although volume is set per track.
    #[serde(default = "unity")]
    pub gain_step: u8,
    /// The round trip compensated for when the take was sealed.
    ///
    /// Already folded into `phase_frames`.
    #[serde(default)]
    pub capture_offset_frames: u64,
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

/// One track's settings, for the tracks that are not on their defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackEntry {
    /// Row on the grid.
    pub track: u8,
    /// The column the track's input sits on. Zero is the whole input.
    #[serde(default)]
    pub input: usize,
    /// Whether launching a clip plays it from its start.
    #[serde(default)]
    pub restart: bool,
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
    /// The tracks that are not on their default settings.
    ///
    /// A session saved before track settings existed has none, and loading it puts every
    /// track back to its default rather than leaving the last session's behind.
    #[serde(default)]
    pub tracks: Vec<TrackEntry>,
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
            tracks: Vec::new(),
            clips: vec![ClipEntry {
                track: 1,
                slot: 3,
                file: "t1s3.wav".to_owned(),
                len_frames: 96_000,
                phase_frames: 1_234,
                playing: true,
                capture_offset_frames: 2_348,
                gain_step: 2,
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
    fn a_session_saved_before_track_settings_existed_has_none() {
        let text =
            "tempo = 120.0\nbeats_per_bar = 4\nbeat_unit = 4\nsample_rate = 48000\nchannels = 2\n";
        let read: Manifest = toml::from_str(text).unwrap();
        assert!(read.tracks.is_empty());
    }

    #[test]
    fn a_track_entry_round_trips() {
        let mut manifest = manifest();
        manifest.tracks = vec![TrackEntry {
            track: 7,
            input: 2,
            restart: true,
        }];
        let written = toml::to_string(&manifest).unwrap();
        let read: Manifest = toml::from_str(&written).unwrap();
        assert_eq!(read.tracks, manifest.tracks);
    }

    #[test]
    fn a_session_with_no_clips_parses() {
        let text =
            "tempo = 120.0\nbeats_per_bar = 4\nbeat_unit = 4\nsample_rate = 48000\nchannels = 2\n";
        let read: Manifest = toml::from_str(text).unwrap();
        assert!(read.clips.is_empty());
    }

    #[test]
    fn an_older_session_without_the_offset_still_parses() {
        let text = concat!(
            "tempo = 120.0\nbeats_per_bar = 4\nbeat_unit = 4\n",
            "sample_rate = 48000\nchannels = 2\n",
            "[[clips]]\ntrack = 0\nslot = 0\nfile = \"t0s0.wav\"\n",
            "len_frames = 96000\nphase_frames = 0\nplaying = true\n",
        );
        let read: Manifest = toml::from_str(text).unwrap();
        assert_eq!(read.clips[0].capture_offset_frames, 0);
        assert_eq!(read.clips[0].gain_step, UNITY_STEP, "and plays untouched");
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
