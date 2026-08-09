//! Pure domain model for Free Loop.
//!
//! No I/O, no threads, no allocation — only value types and pure functions, so timing
//! behaviour is testable with a fake clock and no hardware.
//!
//! - [`time`] — the bar grid and the quantisation rules.
//! - [`ids`] — the fixed 8×8 grid of pads, and clip identity.
//! - [`slot`] — the per-pad state machine.
//! - [`session`] — the whole grid, and the rules that span more than one pad.
//! - [`command`] / [`event`] — the vocabulary crossing the realtime boundary.

pub mod command;
pub mod event;
pub mod ids;
pub mod session;
pub mod slot;
pub mod time;

pub use command::Command;
pub use event::Event;
pub use ids::{ClipId, IndexOutOfRange, SLOT_COUNT, SlotAddr, SlotId, TRACK_COUNT, TrackId};
pub use session::SessionModel;
pub use slot::{Ctx, Effect, Effects, SlotInput, SlotState, step};
pub use time::{BarGrid, Frames, MAX_BPM, MIN_BPM, SampleRate, Tempo, TimeError, TimeSignature};
