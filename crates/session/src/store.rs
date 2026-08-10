//! Where sessions live on disk.
//!
//! One directory per pad, named for the pad it is saved under, so the grid position is
//! the session's identity. There is nothing else to name them by on a device with no
//! screen.

use std::path::{Path, PathBuf};

use free_loop_core::{Frames, SLOT_COUNT, SlotAddr, TRACK_COUNT};
use free_loop_engine::buffer::{Clip, SEGMENT_FRAMES};

use crate::manifest::{ClipEntry, MANIFEST, Manifest};

/// Something went wrong reading or writing a session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file that failed.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// A manifest could not be parsed.
    #[error("{path}: {source}")]
    Parse {
        /// The file that failed.
        path: String,
        /// Why.
        source: toml::de::Error,
    },
    /// A manifest could not be written.
    #[error("could not encode the manifest: {0}")]
    Encode(#[from] toml::ser::Error),
    /// An audio file could not be read or written.
    #[error("{path}: {source}")]
    Wav {
        /// The file that failed.
        path: String,
        /// Why.
        source: hound::Error,
    },
}

/// One pad's audio, ready to write.
#[derive(Debug)]
pub struct SavedClip<'a> {
    /// Which pad it belongs to.
    pub addr: SlotAddr,
    /// Whether the pad was sounding.
    pub playing: bool,
    /// The audio.
    pub clip: &'a Clip,
}

/// Everything needed to write a session.
#[derive(Debug)]
pub struct SessionData<'a> {
    /// Beats per minute.
    pub tempo: f64,
    /// Beats in a bar.
    pub beats_per_bar: u32,
    /// Note value that gets the beat.
    pub beat_unit: u32,
    /// Rate the audio was recorded at.
    pub sample_rate: u32,
    /// Channels in each clip.
    pub channels: u16,
    /// The pads that hold something.
    pub clips: Vec<SavedClip<'a>>,
}

/// A directory holding up to one session per pad.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Points at a directory. Nothing is created until a session is saved.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where a pad's session lives.
    pub fn dir(&self, addr: SlotAddr) -> PathBuf {
        self.root
            .join(format!("{}{}", addr.track.index(), addr.slot.index()))
    }

    /// Whether a pad holds a session.
    pub fn exists(&self, addr: SlotAddr) -> bool {
        self.dir(addr).join(MANIFEST).is_file()
    }

    /// Every pad that holds a session.
    pub fn index(&self) -> Vec<SlotAddr> {
        SlotAddr::all().filter(|addr| self.exists(*addr)).collect()
    }

    /// Reads a pad's manifest.
    ///
    /// # Errors
    ///
    /// [`SessionError`] if the manifest is missing or malformed.
    pub fn manifest(&self, addr: SlotAddr) -> Result<Manifest, SessionError> {
        let path = self.dir(addr).join(MANIFEST);
        let text = std::fs::read_to_string(&path).map_err(|source| SessionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| SessionError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Writes a session under a pad, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// [`SessionError`] if the directory or any file cannot be written.
    pub fn save(&self, addr: SlotAddr, data: &SessionData<'_>) -> Result<(), SessionError> {
        let dir = self.dir(addr);
        create_dir(&dir)?;
        // Audio from a previous session under this pad would otherwise linger and be
        // listed by a manifest that no longer mentions it.
        remove_wavs(&dir)?;

        let mut entries = Vec::with_capacity(data.clips.len());
        for saved in &data.clips {
            let file = Manifest::file_name(saved.addr);
            write_wav(
                &dir.join(&file),
                saved.clip,
                data.sample_rate,
                data.channels,
            )?;

            let len = saved.clip.len();
            entries.push(ClipEntry {
                track: index_as_u8(saved.addr.track.index()),
                slot: index_as_u8(saved.addr.slot.index()),
                file,
                len_frames: len.0,
                phase_frames: phase_of(saved.clip),
                playing: saved.playing,
                capture_offset_frames: saved.clip.capture_offset().0,
            });
        }

        let manifest = Manifest {
            tempo: data.tempo,
            beats_per_bar: data.beats_per_bar,
            beat_unit: data.beat_unit,
            sample_rate: data.sample_rate,
            channels: data.channels,
            clips: entries,
        };

        let path = dir.join(MANIFEST);
        std::fs::write(&path, toml::to_string_pretty(&manifest)?).map_err(|source| {
            SessionError::Io {
                path: path.display().to_string(),
                source,
            }
        })
    }

    /// Deletes a pad's session.
    ///
    /// # Errors
    ///
    /// [`SessionError`] if the directory exists but cannot be removed.
    pub fn remove(&self, addr: SlotAddr) -> Result<(), SessionError> {
        let dir = self.dir(addr);
        if !dir.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&dir).map_err(|source| SessionError::Io {
            path: dir.display().to_string(),
            source,
        })
    }
}

/// Grid indices are below [`TRACK_COUNT`] and [`SLOT_COUNT`], both far inside a `u8`.
fn index_as_u8(index: usize) -> u8 {
    const _: () = assert!(TRACK_COUNT <= 255 && SLOT_COUNT <= 255);
    u8::try_from(index).unwrap_or(0)
}

/// How far into a bar the loop starts, which is what survives a restart.
fn phase_of(clip: &Clip) -> u64 {
    let len = clip.len().0;
    if len == 0 {
        0
    } else {
        clip.recorded_at().0 % len
    }
}

fn create_dir(dir: &Path) -> Result<(), SessionError> {
    std::fs::create_dir_all(dir).map_err(|source| SessionError::Io {
        path: dir.display().to_string(),
        source,
    })
}

fn remove_wavs(dir: &Path) -> Result<(), SessionError> {
    let listing = std::fs::read_dir(dir).map_err(|source| SessionError::Io {
        path: dir.display().to_string(),
        source,
    })?;

    for entry in listing.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "wav") {
            std::fs::remove_file(&path).map_err(|source| SessionError::Io {
                path: path.display().to_string(),
                source,
            })?;
        }
    }
    Ok(())
}

fn write_wav(
    path: &Path,
    clip: &Clip,
    sample_rate: u32,
    channels: u16,
) -> Result<(), SessionError> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|source| SessionError::Wav {
        path: path.display().to_string(),
        source,
    })?;

    let channels = usize::from(channels);
    let total = clip.len().0;
    let mut chunk = vec![0.0_f32; SEGMENT_FRAMES * channels];
    let mut done = 0;

    while done < total {
        let frames = usize::try_from(total - done)
            .unwrap_or(SEGMENT_FRAMES)
            .min(SEGMENT_FRAMES);
        let slice = &mut chunk[..frames * channels];
        slice.fill(0.0);
        // Reading from the clip's own start gives phase zero, so the file begins where
        // the loop begins.
        clip.mix_into(clip.recorded_at() + Frames(done), slice);

        for sample in slice.iter() {
            writer
                .write_sample(*sample)
                .map_err(|source| SessionError::Wav {
                    path: path.display().to_string(),
                    source,
                })?;
        }
        done += frames as u64;
    }

    writer.finalize().map_err(|source| SessionError::Wav {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::elidable_lifetime_names,
        reason = "tests should fail loudly, and compare exact stored values"
    )]

    use super::*;
    use free_loop_core::{SlotId, TrackId};
    use free_loop_engine::buffer::{AudioBuffer, SegmentPool};

    const CH: u16 = 2;

    /// A directory that removes itself, so a failed test leaves nothing behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "free-loop-{}-{}-{name}",
                std::process::id(),
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    /// A clip whose samples say which frame they came from.
    fn clip(frames: usize, recorded_at: u64) -> Clip {
        let mut pool = SegmentPool::new(4, usize::from(CH));
        let mut buffer = AudioBuffer::new(4, usize::from(CH));
        let audio: Vec<f32> = (0..frames * usize::from(CH))
            .map(|i| i as f32 / 1000.0)
            .collect();
        buffer.write(0, &audio, &mut pool);
        let mut clip = Clip::new(
            buffer,
            Frames(frames as u64),
            Frames(recorded_at),
            usize::from(CH),
        );
        clip.set_capture_offset(Frames(64));
        clip
    }

    fn data<'a>(clips: Vec<SavedClip<'a>>) -> SessionData<'a> {
        SessionData {
            tempo: 120.0,
            beats_per_bar: 4,
            beat_unit: 4,
            sample_rate: 48_000,
            channels: CH,
            clips,
        }
    }

    #[test]
    fn an_empty_store_lists_nothing() {
        let dir = TempDir::new("empty");
        let store = SessionStore::new(&dir.0);
        assert!(store.index().is_empty());
        assert!(!store.exists(addr(0, 0)));
    }

    #[test]
    fn a_saved_session_is_listed_and_reads_back() {
        let dir = TempDir::new("save");
        let store = SessionStore::new(&dir.0);
        let audio = clip(128, 300);
        let under = addr(2, 5);

        store
            .save(
                under,
                &data(vec![SavedClip {
                    addr: addr(1, 0),
                    playing: true,
                    clip: &audio,
                }]),
            )
            .unwrap();

        assert!(store.exists(under));
        assert_eq!(store.index(), vec![under]);

        let manifest = store.manifest(under).unwrap();
        assert_eq!(manifest.tempo, 120.0);
        assert_eq!(manifest.clips.len(), 1);

        let entry = &manifest.clips[0];
        assert_eq!(entry.addr().unwrap(), addr(1, 0));
        assert_eq!(entry.len_frames, 128);
        assert_eq!(entry.phase_frames, 300 % 128, "phase survives a restart");
        assert_eq!(
            entry.capture_offset_frames, 64,
            "the alignment stays visible"
        );
        assert!(entry.playing);
        assert!(dir.0.join("25").join(&entry.file).is_file());
    }

    #[test]
    fn the_audio_written_is_the_audio_held() {
        let dir = TempDir::new("audio");
        let store = SessionStore::new(&dir.0);
        let audio = clip(64, 0);
        let under = addr(0, 0);

        store
            .save(
                under,
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: false,
                    clip: &audio,
                }]),
            )
            .unwrap();

        let path = dir.0.join("00").join("t0s0.wav");
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, CH);
        assert_eq!(reader.spec().sample_rate, 48_000);

        let read: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
        let expected: Vec<f32> = (0..64 * usize::from(CH))
            .map(|i| i as f32 / 1000.0)
            .collect();
        assert_eq!(read, expected);
    }

    #[test]
    fn a_clip_longer_than_one_chunk_is_written_whole() {
        let dir = TempDir::new("long");
        let store = SessionStore::new(&dir.0);
        let frames = SEGMENT_FRAMES + 777;
        let audio = clip(frames, 0);

        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: false,
                    clip: &audio,
                }]),
            )
            .unwrap();

        let path = dir.0.join("00").join("t0s0.wav");
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.len() as usize, frames * usize::from(CH));
    }

    #[test]
    fn saving_again_replaces_what_was_there() {
        let dir = TempDir::new("replace");
        let store = SessionStore::new(&dir.0);
        let audio = clip(64, 0);
        let under = addr(3, 3);

        store
            .save(
                under,
                &data(vec![
                    SavedClip {
                        addr: addr(0, 0),
                        playing: false,
                        clip: &audio,
                    },
                    SavedClip {
                        addr: addr(1, 1),
                        playing: false,
                        clip: &audio,
                    },
                ]),
            )
            .unwrap();
        assert!(dir.0.join("33").join("t1s1.wav").is_file());

        store
            .save(
                under,
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: false,
                    clip: &audio,
                }]),
            )
            .unwrap();

        assert_eq!(store.manifest(under).unwrap().clips.len(), 1);
        assert!(
            !dir.0.join("33").join("t1s1.wav").is_file(),
            "audio the new manifest does not mention must not linger"
        );
    }

    #[test]
    fn a_session_with_no_clips_still_saves() {
        let dir = TempDir::new("bare");
        let store = SessionStore::new(&dir.0);
        store.save(addr(7, 7), &data(Vec::new())).unwrap();

        assert!(store.exists(addr(7, 7)));
        assert!(store.manifest(addr(7, 7)).unwrap().clips.is_empty());
    }

    #[test]
    fn sessions_under_different_pads_do_not_collide() {
        let dir = TempDir::new("pads");
        let store = SessionStore::new(&dir.0);
        let audio = clip(32, 0);

        for under in [addr(0, 1), addr(1, 0)] {
            store
                .save(
                    under,
                    &data(vec![SavedClip {
                        addr: addr(0, 0),
                        playing: false,
                        clip: &audio,
                    }]),
                )
                .unwrap();
        }

        let mut listed = store.index();
        listed.sort_unstable();
        assert_eq!(listed, vec![addr(0, 1), addr(1, 0)]);
    }

    #[test]
    fn removing_a_session_takes_it_out_of_the_index() {
        let dir = TempDir::new("remove");
        let store = SessionStore::new(&dir.0);
        store.save(addr(4, 4), &data(Vec::new())).unwrap();

        store.remove(addr(4, 4)).unwrap();
        assert!(!store.exists(addr(4, 4)));
        store.remove(addr(4, 4)).unwrap();
    }

    #[test]
    fn a_malformed_manifest_is_an_error() {
        let dir = TempDir::new("broken");
        let store = SessionStore::new(&dir.0);
        std::fs::create_dir_all(dir.0.join("00")).unwrap();
        std::fs::write(dir.0.join("00").join(MANIFEST), "tempo = \"fast\"").unwrap();

        assert!(store.manifest(addr(0, 0)).is_err());
    }
}
