//! Realtime engine for Free Loop.
//!
//! Owns the transport, the mixer, capture, and the click. Nothing here touches an audio
//! device or a MIDI port: [`Engine::process`] takes an input slice and fills an output
//! slice, so it can be driven by a device callback or by a test at any block size.
//!
//! - [`click`]: the metronome.
//! - [`Loader`]: putting a saved session back in, a step at a time.
//! - [`recycle`]: returning retired clips to the pools without dropping them on the
//!   audio thread.
//! - [`snapshot`]: handing clips out to be read off the audio thread.

pub mod click;
mod engine;
mod load;
pub mod recycle;
pub mod snapshot;

pub use click::{Click, ClickConfig};
pub use engine::{DEFAULT_DECLICK, Engine, EngineConfig, EngineError, EventSink, Housekeeping};
pub use load::{BeginError, ClipFull, LoadFull, Loader};
pub use recycle::{Recycler, Retirement};
pub use snapshot::{Snapshot, SnapshotReader};
