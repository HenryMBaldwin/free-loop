//! Failures opening or running the audio devices.

use cpal::SampleFormat;

/// Something went wrong reaching the audio hardware.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The host has no default device in this direction.
    #[error("no default {0} device")]
    NoDevice(&'static str),
    /// No device name matched the request.
    #[error("no device matching \"{0}\"")]
    DeviceNotFound(String),
    /// The device offered nothing this crate can use.
    #[error("the {0} device offers no usable configuration")]
    NoUsableConfig(&'static str),
    /// The two devices could not agree on a rate.
    #[error("input runs at {input} Hz but output runs at {output} Hz")]
    SampleRateMismatch {
        /// Rate the input settled on.
        input: u32,
        /// Rate the output settled on.
        output: u32,
    },
    /// The device wants a sample format this crate does not convert.
    #[error("sample format {0:?} is not supported")]
    UnsupportedFormat(SampleFormat),
    /// The device or host reported a failure.
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
}
