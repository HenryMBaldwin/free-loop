//! Pure domain model for Free Loop.
//!
//! No I/O, no threads, no allocation. Only value types and pure functions.
//!
//! - [`time`]: the bar grid and the quantisation rules.
//! - [`gain`]: track volume in the steps a row of pads offers.
//! - [`ids`]: the fixed 8×8 grid of pads, and clip identity.
//! - [`slot`]: the per-pad state machine.
//! - [`session`]: the whole grid, and the rules that span more than one pad.
//! - [`command`] / [`event`]: the vocabulary crossing the realtime boundary.
//! - [`settings`]: the whole-state half of that vocabulary, which coalesces.

pub mod command;
pub mod event;
pub mod gain;
pub mod ids;
pub mod session;
pub mod settings;
pub mod slot;
pub mod time;

pub use command::Command;
pub use event::{Event, EventKind};
pub use gain::{GAIN_STEPS, UNITY_STEP, gain_for_step};
pub use ids::{
    ClipId, INPUT_CHANNELS, IndexOutOfRange, LaunchMode, PadMask, Picks, SLOT_COUNT, SlotAddr,
    SlotId, TRACK_COUNT, TrackId, TrackInput, column_mask, pad_bit, row_mask,
};
pub use session::SessionModel;
pub use settings::Settings;
pub use slot::{Ctx, Effect, Effects, SlotInput, SlotState, step};
pub use time::{
    BarGrid, CLOCK_TICKS_PER_QUARTER, Frames, MAX_BPM, MIN_BPM, SampleRate, Subdivision, Tempo,
    TimeError, TimeSignature,
};
