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
    /// A device came back offering something the engine was not built for.
    #[error(
        "device returned at {found_rate} Hz with {found_channels} channels, but the \
         session is {wanted_rate} Hz with {wanted_channels}"
    )]
    ConfigurationChanged {
        /// Rate the engine was built for.
        wanted_rate: u32,
        /// Rate the device now offers.
        found_rate: u32,
        /// Channels the engine was built for.
        wanted_channels: usize,
        /// Channels the device now offers.
        found_channels: usize,
    },
    /// The device or host reported a failure.
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
}
