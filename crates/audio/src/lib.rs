//! Audio device plumbing for Free Loop.
//!
//! cpal has no duplex stream, so the input and output arrive as separate callbacks on
//! every platform. The engine runs in the output callback and reads captured frames from
//! a ring the input callback fills. On a single device both callbacks share a hardware
//! clock, so the ring level is stable and the cushion between them is a fixed, known
//! latency.
//!
//! - [`config`]: picking a configuration from what a device offers.
//! - [`ring`]: the capture ring and channel mapping.
//! - [`stream`]: opening devices and driving the engine.

pub mod config;
pub mod error;
pub mod ring;
pub mod stream;

pub use config::{AudioConfig, Negotiated};
// Named by [`Negotiated`], so a caller can build or match one without depending on cpal.
pub use cpal::SampleFormat;
pub use error::AudioError;
pub use ring::{CaptureReader, CaptureWriter, ChannelMap, MAX_BLOCK_FRAMES};
pub use stream::{
    AudioIo, DeviceChange, DeviceList, DeviceLoss, DroppedEvents, Opened, RETRY_INTERVAL,
    list_devices, open,
};
