//! Realtime engine for Free Loop.
//!
//! Owns the transport, the mixer, capture, and the click. Nothing here touches an audio
//! device or a MIDI port: [`Engine::process`] takes an input slice and fills an output
//! slice, so it can be driven by a device callback or by a test at any block size.
//!
//! - [`buffer`] — pooled audio storage for recorded clips.
//! - [`click`] — the metronome.

pub mod buffer;
pub mod click;
mod engine;

pub use buffer::{AudioBuffer, Clip, SEGMENT_FRAMES, Segment, SegmentPool};
pub use click::{Click, ClickConfig};
pub use engine::{Engine, EngineConfig, EngineError, EventSink};
