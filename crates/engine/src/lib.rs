//! Realtime engine for Free Loop.
//!
//! Owns the transport, the mixer, capture, and the click. Nothing here touches an audio
//! device or a MIDI port: [`Engine::process`] takes an input slice and fills an output
//! slice, so it can be driven by a device callback or by a test at any block size.
//!
//! - [`click`]: the metronome.
//! - [`load`]: putting a saved session back in.
//! - [`recycle`]: returning retired clips to the pools without dropping them on the
//!   audio thread.
//! - [`snapshot`]: handing clips out to be read off the audio thread.

pub mod click;
mod engine;
pub mod load;
pub mod recycle;
pub mod snapshot;

pub use click::{Click, ClickConfig};
pub use engine::{DEFAULT_DECLICK, Engine, EngineConfig, EngineError, EventSink, Housekeeping};
pub use load::{LoadMessage, Loader};
pub use recycle::{Recycler, Retirement};
pub use snapshot::{Snapshot, SnapshotReader};
