//! Where sessions live on disk.
//!
//! One directory per pad, named for the pad it is saved under, so the grid position is
//! the session's identity. There is nothing else to name them by on a device with no
//! screen.

use std::path::{Path, PathBuf};

use free_loop_core::{Frames, SLOT_COUNT, SlotAddr, TRACK_COUNT, UNITY_STEP};
use free_loop_engine::buffer::{AudioBuffer, Clip, Ramp, SEGMENT_FRAMES, SegmentPool};

use crate::manifest::{ClipEntry, MANIFEST, Manifest, TrackEntry};

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
    /// The session was recorded for a different setup.
    #[error("session was recorded at {found} {what}, but the device is running {wanted}")]
    Mismatch {
        /// What differs.
        what: &'static str,
        /// What the device is running.
        wanted: u32,
        /// What the session holds.
        found: u32,
    },
    /// The session file describes something that cannot be loaded.
    #[error("session is not loadable: {0}")]
    Invalid(&'static str),
    /// A manifest named a pad outside the grid.
    #[error("{0}")]
    OffGrid(#[from] free_loop_core::IndexOutOfRange),
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
    /// The step on the gain ladder its track was playing at.
    pub gain_step: u8,
    /// Where the launch that is playing it put it, if the track restarts its clips.
    pub launch_anchor: Option<Frames>,
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
    /// What each track's input and launch mode are set to.
    pub tracks: [TrackSettings; TRACK_COUNT],
}

/// One track's settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackSettings {
    /// The column the track's input sits on. Zero is the whole input.
    pub input: usize,
    /// Whether launching a clip plays it from its start.
    pub restart: bool,
}

/// One pad's audio, read back.
#[derive(Debug)]
pub struct LoadedClip {
    /// Which pad it belongs to.
    pub addr: SlotAddr,
    /// Whether the pad was sounding.
    pub playing: bool,
    /// The step on the gain ladder its track should play at.
    pub gain_step: u8,
    /// Where the launch that was playing it put it, if there was one.
    pub launch_anchor: Option<Frames>,
    /// The audio, with storage owned by the caller.
    pub clip: Clip,
}

/// A session read back off disk.
#[derive(Debug)]
pub struct LoadedSession {
    /// The settings it was saved with.
    pub manifest: Manifest,
    /// The pads that hold something.
    pub clips: Vec<LoadedClip>,
}

impl LoadedSession {
    /// What each track's settings should be, defaulted where the session says nothing.
    pub fn tracks(&self) -> [TrackSettings; TRACK_COUNT] {
        let mut tracks = [TrackSettings::default(); TRACK_COUNT];
        for entry in &self.manifest.tracks {
            if let Some(slot) = tracks.get_mut(usize::from(entry.track)) {
                *slot = TrackSettings {
                    input: entry.input,
                    restart: entry.restart,
                };
            }
        }
        tracks
    }

    /// The level each track should play at, taken from the clips it holds.
    pub fn gains(&self) -> [u8; TRACK_COUNT] {
        let mut gains = [UNITY_STEP; TRACK_COUNT];
        for loaded in &self.clips {
            gains[loaded.addr.track.index()] = loaded.gain_step;
        }
        gains
    }
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

    /// Puts back any session left mid-swap by an interrupted save.
    ///
    /// A swap moves the old directory aside and the new one into place. A process that dies
    /// between those two leaves the pad with nothing where a complete session is sitting
    /// under another name. Also clears staging left by an interruption during writing,
    /// which holds disk space until that pad is saved again. Call once at startup.
    ///
    /// Returns what could not be put right, so a pad that is still missing can be said so
    /// rather than looking empty.
    pub fn recover(&self) -> Vec<SessionError> {
        let mut trouble = Vec::new();
        for addr in SlotAddr::all() {
            let dir = self.dir(addr);
            let previous = self.previous(addr);

            if dir.is_dir() {
                // Whatever is here is the finished session; anything aside is what it
                // replaced.
                if let Err(error) = remove_dir(&previous) {
                    trouble.push(error);
                }
            } else if previous.is_dir()
                && let Err(error) = rename(&previous, &dir)
            {
                trouble.push(error);
            }

            if let Err(error) = remove_dir(&self.staging(addr)) {
                trouble.push(error);
            }
        }
        trouble
    }

    /// Where a pad's outgoing session waits during a swap.
    fn previous(&self, addr: SlotAddr) -> PathBuf {
        self.root.join(format!(
            ".{}{}.previous",
            addr.track.index(),
            addr.slot.index()
        ))
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
        // Written beside the real directory and swapped in once every file is on disk, so
        // a failure part way through leaves the previous session as it was.
        let staging = self.staging(addr);
        let _ = std::fs::remove_dir_all(&staging);
        let dir = staging.clone();
        create_dir(&dir)?;

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
                launch_phase_frames: saved
                    .launch_anchor
                    .map(|anchor| phase_in(anchor, saved.clip.len())),
                playing: saved.playing,
                capture_offset_frames: saved.clip.capture_offset().0,
                gain_step: saved.gain_step,
            });
        }

        let manifest = Manifest {
            tempo: data.tempo,
            beats_per_bar: data.beats_per_bar,
            beat_unit: data.beat_unit,
            sample_rate: data.sample_rate,
            channels: data.channels,
            clips: entries,
            // Only what differs, so a session file stays readable.
            tracks: data
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, track)| **track != TrackSettings::default())
                .map(|(index, track)| TrackEntry {
                    track: index_as_u8(index),
                    input: track.input,
                    restart: track.restart,
                })
                .collect(),
        };

        let path = dir.join(MANIFEST);
        std::fs::write(&path, toml::to_string_pretty(&manifest)?).map_err(|source| {
            SessionError::Io {
                path: path.display().to_string(),
                source,
            }
        })?;

        self.swap_in(addr, &staging)
    }

    /// Where a save is built before it replaces what is there.
    fn staging(&self, addr: SlotAddr) -> PathBuf {
        self.root.join(format!(
            ".{}{}.saving",
            addr.track.index(),
            addr.slot.index()
        ))
    }

    /// Replaces the pad's session with a finished staging directory.
    ///
    /// The old directory is moved aside first, so the window where neither is in place is
    /// two renames rather than a whole session's worth of writing.
    fn swap_in(&self, addr: SlotAddr, staging: &Path) -> Result<(), SessionError> {
        let dir = self.dir(addr);
        let previous = self.previous(addr);
        let _ = std::fs::remove_dir_all(&previous);

        let had_previous = dir.is_dir();
        if had_previous {
            rename(&dir, &previous)?;
        }
        if let Err(error) = rename(staging, &dir) {
            // Put back what was there rather than leaving the pad with nothing. If even
            // that fails, `recover` finds it at the next startup.
            if had_previous {
                let _ = std::fs::rename(&previous, &dir);
            }
            return Err(error);
        }

        let _ = std::fs::remove_dir_all(&previous);
        Ok(())
    }

    /// Reads a session back.
    ///
    /// The audio comes back in freshly allocated storage, which the caller owns. Lengths
    /// and phases are frame counts, so a session recorded at another rate is refused.
    ///
    /// # Errors
    ///
    /// [`SessionError`] if the session is missing, malformed, or was recorded at a
    /// different sample rate or channel count.
    pub fn load(
        &self,
        addr: SlotAddr,
        sample_rate: u32,
        channels: u16,
    ) -> Result<LoadedSession, SessionError> {
        let manifest = self.manifest(addr)?;
        if manifest.sample_rate != sample_rate {
            return Err(SessionError::Mismatch {
                what: "Hz",
                wanted: sample_rate,
                found: manifest.sample_rate,
            });
        }
        if manifest.channels != channels {
            return Err(SessionError::Mismatch {
                what: "channels",
                wanted: u32::from(channels),
                found: u32::from(manifest.channels),
            });
        }

        manifest
            .validate(max_frames(sample_rate))
            .map_err(SessionError::Invalid)?;

        let dir = self.dir(addr);
        let mut clips = Vec::with_capacity(manifest.clips.len());
        for entry in &manifest.clips {
            let pad = entry.addr()?;
            clips.push(LoadedClip {
                addr: pad,
                playing: entry.playing,
                gain_step: entry.gain_step,
                launch_anchor: entry.launch_phase_frames.map(Frames),
                // Generated from the pad rather than taken from the file, which could name
                // anything, including a path out of the session directory.
                clip: read_wav(
                    &dir.join(Manifest::file_name(pad)),
                    entry,
                    channels,
                    sample_rate,
                )?,
            });
        }

        Ok(LoadedSession { manifest, clips })
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
    phase_in(clip.recorded_at(), clip.len())
}

/// Where `anchor` sits inside a loop of `len`.
fn phase_in(anchor: Frames, len: Frames) -> u64 {
    if len.0 == 0 { 0 } else { anchor.0 % len.0 }
}

/// Removes a directory if it is there, reporting anything other than its absence.
fn remove_dir(dir: &Path) -> Result<(), SessionError> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionError::Io {
            path: dir.display().to_string(),
            source,
        }),
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), SessionError> {
    std::fs::rename(from, to).map_err(|source| SessionError::Io {
        path: to.display().to_string(),
        source,
    })
}

fn create_dir(dir: &Path) -> Result<(), SessionError> {
    std::fs::create_dir_all(dir).map_err(|source| SessionError::Io {
        path: dir.display().to_string(),
        source,
    })
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
        clip.mix_into(clip.recorded_at() + Frames(done), slice, Ramp::UNITY);

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

/// The most audio a session may claim in total, at half an hour.
///
/// The lengths in the file decide how much is allocated before a byte is read, so they need
/// a ceiling. Half an hour across the whole grid is far more than the engine's own pools
/// hold, so this only stops a file asking for the impossible.
fn max_frames(sample_rate: u32) -> u64 {
    u64::from(sample_rate) * 60 * 30
}

/// Reads one clip back into storage the caller owns.
fn read_wav(
    path: &Path,
    entry: &ClipEntry,
    channels: u16,
    sample_rate: u32,
) -> Result<Clip, SessionError> {
    let mut reader = hound::WavReader::open(path).map_err(|source| SessionError::Wav {
        path: path.display().to_string(),
        source,
    })?;

    // The manifest is checked against the device before this, so an audio file that
    // disagrees with the manifest would be read into the wrong shape.
    let spec = reader.spec();
    if spec.channels != channels {
        return Err(SessionError::Mismatch {
            what: "channels in an audio file",
            wanted: u32::from(channels),
            found: u32::from(spec.channels),
        });
    }
    if spec.sample_rate != sample_rate {
        return Err(SessionError::Invalid("an audio file is at another rate"));
    }
    // Read as `f32` below, which an integer file would only fail on at the first sample,
    // by which point the whole clip has been allocated.
    if spec.sample_format != hound::SampleFormat::Float || spec.bits_per_sample != 32 {
        return Err(SessionError::Invalid("an audio file is not 32-bit float"));
    }
    if reader.len() % u32::from(spec.channels.max(1)) != 0 {
        return Err(SessionError::Invalid("an audio file ends mid frame"));
    }
    // A file shorter than the manifest claims would become silence-padded, and a longer one
    // would be cut off without a word.
    let held = reader.len() / u32::from(spec.channels.max(1));
    if u64::from(held) != entry.len_frames {
        return Err(SessionError::Mismatch {
            what: "frames in an audio file",
            wanted: u32::try_from(entry.len_frames).unwrap_or(u32::MAX),
            found: held,
        });
    }

    let channels = usize::from(channels);
    let frames = entry.len_frames;
    let segments = usize::try_from(frames.div_ceil(SEGMENT_FRAMES as u64)).unwrap_or(1);
    let mut pool = SegmentPool::new(segments.max(1), channels);
    let mut buffer = AudioBuffer::new(segments.max(1), channels);

    let mut chunk = Vec::with_capacity(SEGMENT_FRAMES * channels);
    let mut written = 0_u64;

    for sample in reader.samples::<f32>() {
        chunk.push(sample.map_err(|source| SessionError::Wav {
            path: path.display().to_string(),
            source,
        })?);

        if chunk.len() == chunk.capacity() {
            written += buffer.write(written, &chunk, &mut pool) as u64;
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        buffer.write(written, &chunk, &mut pool);
    }

    let mut clip = Clip::new(buffer, Frames(frames), Frames(entry.phase_frames), channels);
    clip.set_capture_offset(Frames(entry.capture_offset_frames));
    clip.set_borrowed(true);
    Ok(clip)
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
        // Sized from the frame count. A fixed pool silently truncates the fixture, which
        // then looks like a round trip losing data.
        let segments = frames.div_ceil(SEGMENT_FRAMES).max(1);
        let mut pool = SegmentPool::new(segments, usize::from(CH));
        let mut buffer = AudioBuffer::new(segments, usize::from(CH));
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
            tracks: [TrackSettings::default(); TRACK_COUNT],
            tempo: 120.0,
            beats_per_bar: 4,
            beat_unit: 4,
            sample_rate: 48_000,
            channels: CH,
            clips,
        }
    }

    #[test]
    fn a_failed_save_leaves_the_previous_session_in_place() {
        let dir = TempDir::new("atomic-save");
        let store = SessionStore::new(&dir.0);
        let good = clip(128, 0);
        let saved = |addr| {
            vec![SavedClip {
                addr,
                playing: true,
                gain_step: UNITY_STEP,
                launch_anchor: None,
                clip: &good,
            }]
        };

        store.save(addr(0, 0), &data(saved(addr(0, 0)))).unwrap();

        // A file where the staging directory has to go makes the save fail before it can
        // touch what is already saved.
        std::fs::write(dir.0.join(".00.saving"), b"in the way").unwrap();
        assert!(store.save(addr(0, 0), &data(saved(addr(0, 1)))).is_err());

        let read = store.load(addr(0, 0), 48_000, 2).unwrap();
        assert_eq!(read.clips.len(), 1, "the first save is still there");
        assert_eq!(read.clips[0].addr, addr(0, 0), "and it is the same clip");
    }

    /// Writes a wav of `frames` in `spec`, over whatever the pad's audio file is.
    fn overwrite_audio(dir: &Path, pad: SlotAddr, frames: u32, spec: hound::WavSpec) {
        let path = dir.join(Manifest::file_name(pad));
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..frames * u32::from(spec.channels) {
            match spec.sample_format {
                hound::SampleFormat::Float => writer.write_sample(0.25_f32).unwrap(),
                hound::SampleFormat::Int => writer.write_sample(0_i16).unwrap(),
            }
        }
        writer.finalize().unwrap();
    }

    fn float_spec(channels: u16) -> hound::WavSpec {
        hound::WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        }
    }

    /// A saved session whose audio file can then be replaced.
    fn saved_one(dir: &TempDir) -> (SessionStore, SlotAddr) {
        let store = SessionStore::new(&dir.0);
        let held = clip(128, 0);
        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: true,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &held,
                }]),
            )
            .unwrap();
        (store, addr(0, 0))
    }

    #[test]
    fn an_audio_file_shorter_than_the_manifest_is_refused() {
        let dir = TempDir::new("short-wav");
        let (store, pad) = saved_one(&dir);
        overwrite_audio(&store.dir(pad), pad, 64, float_spec(CH));

        assert!(
            store.load(pad, 48_000, CH).is_err(),
            "would have become silence padded"
        );
    }

    #[test]
    fn an_audio_file_longer_than_the_manifest_is_refused() {
        let dir = TempDir::new("long-wav");
        let (store, pad) = saved_one(&dir);
        overwrite_audio(&store.dir(pad), pad, 256, float_spec(CH));

        assert!(store.load(pad, 48_000, CH).is_err(), "would have been cut");
    }

    #[test]
    fn an_integer_audio_file_is_refused_before_anything_is_allocated() {
        let dir = TempDir::new("int-wav");
        let (store, pad) = saved_one(&dir);
        overwrite_audio(
            &store.dir(pad),
            pad,
            128,
            hound::WavSpec {
                channels: CH,
                sample_rate: 48_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        );

        let refused = store.load(pad, 48_000, CH);
        assert!(
            matches!(refused, Err(SessionError::Invalid(_))),
            "not a read failure"
        );
    }

    #[test]
    fn an_audio_file_with_the_wrong_channel_count_is_refused() {
        let dir = TempDir::new("mono-wav");
        let (store, pad) = saved_one(&dir);
        overwrite_audio(&store.dir(pad), pad, 128, float_spec(1));

        assert!(store.load(pad, 48_000, CH).is_err());
    }

    #[test]
    fn a_session_left_mid_swap_is_put_back() {
        let dir = TempDir::new("recover");
        let store = SessionStore::new(&dir.0);
        let held = clip(128, 0);
        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: true,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &held,
                }]),
            )
            .unwrap();

        // What an interruption between the two renames leaves behind.
        std::fs::rename(store.dir(addr(0, 0)), dir.0.join(".00.previous")).unwrap();
        assert!(!store.exists(addr(0, 0)), "invisible until recovered");

        store.recover();
        assert!(store.exists(addr(0, 0)), "back where it belongs");
        assert_eq!(store.load(addr(0, 0), 48_000, 2).unwrap().clips.len(), 1);
    }

    #[test]
    fn recovery_clears_staging_left_by_an_interrupted_write() {
        let dir = TempDir::new("recover-staging");
        let store = SessionStore::new(&dir.0);
        let abandoned = dir.0.join(".34.saving");
        std::fs::create_dir_all(abandoned.join("junk")).unwrap();

        assert!(store.recover().is_empty(), "nothing went wrong");
        assert!(!abandoned.exists(), "the space is given back");
    }

    #[test]
    fn recovery_leaves_a_finished_swap_alone() {
        let dir = TempDir::new("recover-noop");
        let store = SessionStore::new(&dir.0);
        let held = clip(128, 0);
        let save = |addr_of| {
            vec![SavedClip {
                addr: addr_of,
                playing: true,
                gain_step: UNITY_STEP,
                launch_anchor: None,
                clip: &held,
            }]
        };
        store.save(addr(0, 0), &data(save(addr(0, 0)))).unwrap();
        // A leftover from a swap that did finish.
        std::fs::create_dir_all(dir.0.join(".00.previous")).unwrap();

        store.recover();
        assert_eq!(store.load(addr(0, 0), 48_000, 2).unwrap().clips.len(), 1);
        assert!(!dir.0.join(".00.previous").exists(), "and it is tidied up");
    }

    #[test]
    fn no_staging_directory_is_left_behind() {
        let dir = TempDir::new("staging");
        let store = SessionStore::new(&dir.0);
        let held = clip(128, 0);

        store
            .save(
                addr(1, 1),
                &data(vec![SavedClip {
                    addr: addr(1, 1),
                    playing: false,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &held,
                }]),
            )
            .unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "found {leftovers:?}");
    }

    #[test]
    fn a_launched_anchor_survives_a_round_trip() {
        let dir = TempDir::new("launch-anchor");
        let store = SessionStore::new(&dir.0);
        let clip = clip(128, 300);

        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: true,
                    gain_step: UNITY_STEP,
                    // A launch put it a bar off the phase it was recorded at.
                    launch_anchor: Some(Frames(365)),
                    clip: &clip,
                }]),
            )
            .unwrap();

        let read = store.load(addr(0, 0), 48_000, 2).unwrap();
        assert_eq!(
            read.clips[0].launch_anchor,
            Some(Frames(365 % 128)),
            "where the launch put it, not where it was recorded"
        );
    }

    #[test]
    fn a_clip_with_no_launch_anchor_plays_where_it_was_recorded() {
        let dir = TempDir::new("no-launch-anchor");
        let store = SessionStore::new(&dir.0);
        let clip = clip(128, 300);

        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: true,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &clip,
                }]),
            )
            .unwrap();

        let read = store.load(addr(0, 0), 48_000, 2).unwrap();
        assert_eq!(read.clips[0].launch_anchor, None);
        assert_eq!(read.clips[0].clip.recorded_at(), Frames(300 % 128));
    }

    #[test]
    fn a_session_that_says_nothing_about_a_track_defaults_it() {
        let loaded = LoadedSession {
            manifest: Manifest {
                tempo: 120.0,
                beats_per_bar: 4,
                beat_unit: 4,
                sample_rate: 48_000,
                channels: 2,
                clips: Vec::new(),
                tracks: Vec::new(),
            },
            clips: Vec::new(),
        };
        assert_eq!(
            loaded.tracks(),
            [TrackSettings::default(); TRACK_COUNT],
            "an older session puts every track back rather than leaving the last one's"
        );
    }

    #[test]
    fn a_saved_track_setting_comes_back() {
        let loaded = LoadedSession {
            manifest: Manifest {
                tempo: 120.0,
                beats_per_bar: 4,
                beat_unit: 4,
                sample_rate: 48_000,
                channels: 2,
                clips: Vec::new(),
                tracks: vec![TrackEntry {
                    track: 7,
                    input: 2,
                    restart: true,
                }],
            },
            clips: Vec::new(),
        };

        let tracks = loaded.tracks();
        assert_eq!(tracks[7].input, 2);
        assert!(tracks[7].restart);
        assert_eq!(tracks[0], TrackSettings::default(), "and only that track");
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
                    gain_step: 2,
                    launch_anchor: None,
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
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
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
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
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
                        gain_step: UNITY_STEP,
                        launch_anchor: None,
                        clip: &audio,
                    },
                    SavedClip {
                        addr: addr(1, 1),
                        playing: false,
                        gain_step: UNITY_STEP,
                        launch_anchor: None,
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
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
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
                        gain_step: UNITY_STEP,
                        launch_anchor: None,
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
    fn a_saved_session_reads_back_with_its_audio() {
        let dir = TempDir::new("roundtrip");
        let store = SessionStore::new(&dir.0);
        let audio = clip(200, 3_000);
        let under = addr(1, 2);

        store
            .save(
                under,
                &data(vec![SavedClip {
                    addr: addr(4, 5),
                    playing: true,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &audio,
                }]),
            )
            .unwrap();

        let loaded = store.load(under, 48_000, CH).unwrap();
        assert_eq!(loaded.clips.len(), 1);

        let read = &loaded.clips[0];
        assert_eq!(read.addr, addr(4, 5));
        assert!(read.playing);
        assert_eq!(read.clip.len(), Frames(200));
        assert_eq!(read.clip.recorded_at(), Frames(3_000 % 200));
        assert_eq!(read.clip.capture_offset(), Frames(64));
        assert!(read.clip.is_borrowed(), "the caller owns the storage");

        let mut out = vec![0.0_f32; 200 * usize::from(CH)];
        read.clip
            .mix_into(read.clip.recorded_at(), &mut out, Ramp::UNITY);
        let expected: Vec<f32> = (0..200 * usize::from(CH))
            .map(|i| i as f32 / 1000.0)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn a_clip_longer_than_one_chunk_reads_back_whole() {
        let dir = TempDir::new("longread");
        let store = SessionStore::new(&dir.0);
        let frames = SEGMENT_FRAMES + 777;
        let audio = clip(frames, 0);

        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: false,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &audio,
                }]),
            )
            .unwrap();

        let loaded = store.load(addr(0, 0), 48_000, CH).unwrap();
        assert_eq!(loaded.clips[0].clip.len(), Frames(frames as u64));

        // The tail is the part a chunking bug would lose.
        let mut out = vec![0.0_f32; 4 * usize::from(CH)];
        loaded.clips[0]
            .clip
            .mix_into(Frames(frames as u64 - 4), &mut out, Ramp::UNITY);
        let base = (frames - 4) * usize::from(CH);
        let expected: Vec<f32> = (base..base + 4 * usize::from(CH))
            .map(|i| i as f32 / 1000.0)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn every_frame_of_a_multi_segment_clip_survives_the_round_trip() {
        let dir = TempDir::new("everyframe");
        let store = SessionStore::new(&dir.0);
        // Several segments plus a partial one, like a real four bar take.
        let frames = SEGMENT_FRAMES * 5 + 4_321;
        let audio = clip(frames, 0);

        store
            .save(
                addr(0, 0),
                &data(vec![SavedClip {
                    addr: addr(0, 0),
                    playing: false,
                    gain_step: UNITY_STEP,
                    launch_anchor: None,
                    clip: &audio,
                }]),
            )
            .unwrap();

        let loaded = store.load(addr(0, 0), 48_000, CH).unwrap();
        let read = &loaded.clips[0].clip;
        assert_eq!(read.len(), Frames(frames as u64));

        let mut out = vec![0.0_f32; frames * usize::from(CH)];
        read.mix_into(read.recorded_at(), &mut out, Ramp::UNITY);

        let expected: Vec<f32> = (0..frames * usize::from(CH))
            .map(|i| i as f32 / 1000.0)
            .collect();

        let wrong = out
            .iter()
            .zip(&expected)
            .position(|(got, want)| got != want);
        assert_eq!(wrong, None, "first sample that differs");
    }

    #[test]
    fn a_level_travels_with_the_clip_it_was_set_on() {
        let dir = TempDir::new("levels");
        let store = SessionStore::new(&dir.0);
        let audio = clip(64, 0);

        store
            .save(
                addr(0, 0),
                &data(vec![
                    SavedClip {
                        addr: addr(1, 0),
                        playing: false,
                        gain_step: 2,
                        launch_anchor: None,
                        clip: &audio,
                    },
                    SavedClip {
                        addr: addr(3, 0),
                        playing: false,
                        gain_step: 6,
                        launch_anchor: None,
                        clip: &audio,
                    },
                ]),
            )
            .unwrap();

        let loaded = store.load(addr(0, 0), 48_000, CH).unwrap();
        let gains = loaded.gains();

        assert_eq!(gains[1], 2);
        assert_eq!(gains[3], 6);
        assert_eq!(
            gains[7], UNITY_STEP,
            "a track with nothing on it is untouched"
        );
    }

    #[test]
    fn a_session_recorded_for_another_setup_is_refused() {
        let dir = TempDir::new("mismatch");
        let store = SessionStore::new(&dir.0);
        store.save(addr(0, 0), &data(Vec::new())).unwrap();

        assert!(store.load(addr(0, 0), 44_100, CH).is_err());
        assert!(store.load(addr(0, 0), 48_000, 1).is_err());
        assert!(store.load(addr(0, 0), 48_000, CH).is_ok());
    }

    #[test]
    fn loading_a_pad_with_no_session_is_an_error() {
        let dir = TempDir::new("missing");
        let store = SessionStore::new(&dir.0);
        assert!(store.load(addr(6, 6), 48_000, CH).is_err());
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
