//! The config file: which jack the instrument is in, what the interface is called, how
//! big its blocks are.

use std::path::Path;

use free_loop_audio::{AudioConfig, InputSource};
use free_loop_core::{LaunchMode, SampleRate, Tempo, TimeError, TimeSignature, TrackInput};
use free_loop_engine::{ClickConfig, EngineConfig, EngineError};
use serde::Deserialize;

/// The config file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("could not read {path}: {source}")]
    Read {
        /// The file that failed.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or has fields this version does not know.
    #[error("could not parse {path}: {source}")]
    Parse {
        /// The file that failed.
        path: String,
        /// Why.
        source: toml::de::Error,
    },
    /// A musical value was out of range.
    #[error(transparent)]
    Time(#[from] TimeError),
    /// The engine refused the settings.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Audio device settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// Substring of the input device name. Empty uses the default device.
    pub input_device: Option<String>,
    /// Substring of the output device name. Empty uses the default device.
    pub output_device: Option<String>,
    /// Rate to request. Empty takes the device's preference.
    pub sample_rate: Option<u32>,
    /// Block size to request. Empty leaves it to the device.
    pub buffer_frames: Option<u32>,
    /// Channels to request. Empty asks for stereo.
    pub channels: Option<u16>,
    /// Input channel every track starts recording, changeable from the grid.
    ///
    /// Empty starts them on the whole input, which is right for a stereo source. Set it
    /// when one instrument is in one jack of a multi-input interface.
    pub input_channel: Option<usize>,
    /// Blocks of capture buffered before the output starts consuming.
    pub cushion_blocks: u32,
    /// Round-trip latency to compensate for, in frames.
    ///
    /// Empty measures it from the driver.
    pub capture_offset_frames: Option<u32>,
    /// Whether losing a device freezes the transport.
    ///
    /// Off carries on where it left off as soon as the device is back.
    pub pause_on_disconnect: bool,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            sample_rate: None,
            buffer_frames: None,
            channels: None,
            input_channel: None,
            cushion_blocks: 2,
            capture_offset_frames: None,
            pause_on_disconnect: true,
        }
    }
}

/// Musical time.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Transport {
    /// Beats per minute. Locked once a clip exists.
    pub tempo: f64,
    /// Beats in a bar.
    pub beats_per_bar: u32,
    /// Note value that gets the beat.
    pub beat_unit: u32,
    /// Longest recording allowed, in bars.
    pub max_bars: u32,
    /// Whether launching a clip plays it from its start rather than from wherever the
    /// transport has reached.
    pub restart_clips: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            tempo: 120.0,
            beats_per_bar: 4,
            beat_unit: 4,
            max_bars: 32,
            restart_clips: false,
        }
    }
}

/// Engine sizing.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Engine {
    /// Segments to allocate. The ceiling on total recorded audio, about 1.4 s and half a
    /// megabyte each, shared across every pad.
    pub segment_pool: usize,
    /// Frames a level takes to travel the full gain range. 5 ms at 48 kHz.
    pub declick_frames: u64,
    /// Segments a loaded session may hold, about 1.4 s and half a megabyte each.
    ///
    /// Separate from `segment_pool`: a load brings its own storage rather than drawing on the
    /// recording pool, so a session built up over several record and load passes is normally
    /// larger than the pool ever held at once. A ceiling rather than a reservation, so it only
    /// stops a file claiming more than is sensible.
    pub load_segments: usize,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            segment_pool: 2_048,
            declick_frames: free_loop_engine::DEFAULT_DECLICK.0,
            load_segments: 8_192,
        }
    }
}

/// Click settings.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Click {
    /// Whether the click sounds at startup.
    pub enabled: bool,
    /// Peak amplitude, 0.0 to 1.0.
    pub level: f32,
}

impl Default for Click {
    fn default() -> Self {
        let defaults = ClickConfig::default();
        Self {
            enabled: defaults.enabled,
            level: defaults.level,
        }
    }
}

/// The whole config file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Audio device settings.
    pub audio: Audio,
    /// Musical time.
    pub transport: Transport,
    /// Engine sizing.
    pub engine: Engine,
    /// Click settings.
    pub click: Click,
}

impl Config {
    /// Reads a config file, falling back to defaults when it is not there.
    ///
    /// A missing file is not an error; a malformed one is.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        Self::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Parses config text.
    ///
    /// # Errors
    ///
    /// The underlying TOML error if the text is not valid.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// The device settings this config asks for.
    pub fn audio(&self) -> AudioConfig {
        AudioConfig {
            input_device: self.audio.input_device.clone(),
            output_device: self.audio.output_device.clone(),
            sample_rate: self.audio.sample_rate,
            buffer_frames: self.audio.buffer_frames,
            channels: self.audio.channels,
            // Every input channel reaches the engine, which picks one per track.
            input_source: InputSource::Direct,
            cushion_blocks: self.audio.cushion_blocks,
            capture_offset: self.audio.capture_offset_frames,
        }
    }

    /// The most segments a loaded session may hold.
    ///
    /// Never below `segment_pool`: a session can be recorded up to the pool's size, and one
    /// that cannot be loaded back is worse than one that was never allowed.
    ///
    /// Counted the way the loader allocates: one buffer per clip, each rounded up to whole
    /// segments.
    pub fn load_budget(&self) -> usize {
        self.engine.load_segments.max(self.engine.segment_pool)
    }

    /// Where every track's clips start out being anchored.
    pub fn launch_mode(&self) -> LaunchMode {
        if self.transport.restart_clips {
            LaunchMode::Restart
        } else {
            LaunchMode::Follow
        }
    }

    /// The input every track starts on.
    pub fn track_input(&self) -> TrackInput {
        match self.audio.input_channel {
            Some(channel) => TrackInput::Mono(u8::try_from(channel).unwrap_or(0)),
            None => TrackInput::Stereo,
        }
    }

    /// The time signature this config asks for.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Time`] if the signature is not musically meaningful.
    pub fn time_signature(&self) -> Result<TimeSignature, ConfigError> {
        Ok(TimeSignature::new(
            self.transport.beats_per_bar,
            self.transport.beat_unit,
        )?)
    }

    /// The engine settings, given what the devices agreed on.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] if a musical value is out of range.
    pub fn engine(&self, sample_rate: u32, channels: usize) -> Result<EngineConfig, ConfigError> {
        Ok(EngineConfig {
            sample_rate: SampleRate::new(sample_rate)?,
            tempo: Tempo::new(self.transport.tempo)?,
            time_signature: self.time_signature()?,
            channels,
            max_bars: self.transport.max_bars,
            segment_pool: self.engine.segment_pool,
            declick: free_loop_core::Frames(self.engine.declick_frames),
            input: self.track_input(),
            launch_mode: self.launch_mode(),
            // Replaced by what the driver reports once the streams are running.
            capture_offset: free_loop_core::Frames::ZERO,
            click: ClickConfig {
                enabled: self.click.enabled,
                level: self.click.level,
            },
        })
    }
}

/// A config file showing every setting at its default.
pub const EXAMPLE: &str = r#"# Free Loop configuration. Every setting shown at its default.

[audio]
# Substrings of the device names. Omit to use the system default devices.
# input_device = "Scarlett"
# output_device = "Scarlett"

# Omit to take the device's preference.
# sample_rate = 48000
# buffer_frames = 256
# channels = 2

# The device input channel your instrument is in. Omit for a stereo source.
# A Scarlett Solo's instrument jack is channel 1.
# input_channel = 1

# Blocks of capture buffered before playback starts consuming. More is safer, later.
cushion_blocks = 2

# Round-trip latency to compensate for, in frames. Omit to measure it from the driver,
# which is right for almost every rig. Set it only if a driver reports badly.
# capture_offset_frames = 512

# Whether losing a device freezes the transport. Off carries on as soon as it is back.
pause_on_disconnect = true

[transport]
tempo = 120.0
beats_per_bar = 4
beat_unit = 4
max_bars = 32
# Whether launching a clip plays it from its start. Off drops into whatever part of it the
# transport has reached, which keeps every loop locked to the same grid.
restart_clips = false

[engine]
# About 1.4 s of stereo audio and half a megabyte each, shared across every pad. Allocated at
# startup, so this is resident memory: 2048 is roughly 46 minutes for just over a gigabyte.
segment_pool = 2048
# Frames a level takes to travel the full gain range. 5 ms at 48 kHz.
declick_frames = 240
# The most segments a saved session may load. A load brings its own storage, so this is a
# ceiling on a file rather than a draw on the pool; never applied below segment_pool.
load_segments = 8192

[click]
enabled = true
level = 0.35
"#;

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::float_cmp,
        reason = "tests should fail loudly, and compare exact configured values"
    )]

    use super::*;

    #[test]
    fn the_load_budget_never_falls_below_the_recording_pool() {
        let config = Config::parse("[engine]\nsegment_pool = 4096\nload_segments = 8\n").unwrap();
        assert_eq!(
            config.load_budget(),
            4096,
            "a session recorded to the pool's size has to load back"
        );
    }

    #[test]
    fn the_load_budget_follows_the_recording_pool() {
        let config = Config::parse("[engine]\nsegment_pool = 4\n").unwrap();
        assert_eq!(
            config.load_budget(),
            8_192,
            "a load is bounded on its own, not by the recording pool"
        );
    }

    #[test]
    fn losing_a_device_pauses_unless_told_otherwise() {
        assert!(Config::default().audio.pause_on_disconnect);
        let config = Config::parse("[audio]\npause_on_disconnect = false\n").unwrap();
        assert!(!config.audio.pause_on_disconnect);
    }

    #[test]
    fn an_empty_file_gives_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.transport.tempo, 120.0);
        assert_eq!(config.audio.cushion_blocks, 2);
        assert!(config.click.enabled);
        assert!(config.audio.input_device.is_none());
    }

    #[test]
    fn a_partial_file_leaves_the_rest_alone() {
        let config = Config::parse("[transport]\ntempo = 90.0\n").unwrap();
        assert_eq!(config.transport.tempo, 90.0);
        assert_eq!(
            config.transport.beats_per_bar, 4,
            "untouched keys keep defaults"
        );
        assert_eq!(config.engine.segment_pool, 2_048);
    }

    #[test]
    fn the_example_file_parses_and_matches_the_defaults() {
        let config = Config::parse(EXAMPLE).unwrap();
        let defaults = Config::default();
        assert_eq!(config.transport.tempo, defaults.transport.tempo);
        assert_eq!(config.transport.max_bars, defaults.transport.max_bars);
        assert_eq!(config.engine.segment_pool, defaults.engine.segment_pool);
        assert_eq!(config.engine.load_segments, defaults.engine.load_segments);
        assert_eq!(config.click.level, defaults.click.level);
        assert_eq!(config.audio.cushion_blocks, defaults.audio.cushion_blocks);
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_a_silent_default() {
        let error = Config::parse("[transport]\ntemp0 = 90.0\n").unwrap_err();
        assert!(error.to_string().contains("temp0"), "{error}");
    }

    #[test]
    fn an_input_channel_becomes_a_mono_source() {
        let config = Config::parse("[audio]\ninput_channel = 1\n").unwrap();
        assert_eq!(config.track_input(), TrackInput::Mono(1));

        let config = Config::parse("").unwrap();
        assert_eq!(config.track_input(), TrackInput::Stereo);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let config = Config::load(Path::new("/nonexistent/free-loop.toml")).unwrap();
        assert_eq!(config.transport.tempo, 120.0);
    }

    #[test]
    fn an_impossible_tempo_is_refused() {
        let config = Config::parse("[transport]\ntempo = 5000.0\n").unwrap();
        assert!(config.engine(48_000, 2).is_err());
    }

    #[test]
    fn an_impossible_time_signature_is_refused() {
        let config = Config::parse("[transport]\nbeat_unit = 3\n").unwrap();
        assert!(config.time_signature().is_err());
    }
}
