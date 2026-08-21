//! The slot state machine.
//!
//! [`step`] is pure and allocation-free; the transport position comes from the [`Ctx`]
//! it is handed rather than a clock.
//!
//! Queued transitions store the frame they are due at. [`SlotInput::Advance`] fires them
//! once due, so every [`Effect`] means "do this at exactly this frame".

use crate::ids::ClipId;
use crate::time::{BarGrid, Frames};

/// What a slot is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlotState {
    /// Nothing recorded here.
    #[default]
    Empty,
    /// Recording will begin at the stored bar boundary.
    QueuedRecord {
        /// When capture starts.
        at: Frames,
    },
    /// Capturing input.
    Recording {
        /// The bar boundary capture began on.
        started_at: Frames,
        /// The bar boundary capture will end on, once the user has asked it to stop.
        ends_at: Option<Frames>,
    },
    /// Holds a clip that is not sounding.
    Stopped {
        /// The clip held here.
        clip: ClipId,
    },
    /// Playback will begin at the stored bar boundary.
    QueuedPlay {
        /// The clip that will play.
        clip: ClipId,
        /// When playback starts.
        at: Frames,
    },
    /// Sounding.
    Playing {
        /// The clip that is sounding.
        clip: ClipId,
    },
    /// Playback will end at the stored bar boundary.
    QueuedStop {
        /// The clip that is sounding.
        clip: ClipId,
        /// When playback stops.
        at: Frames,
    },
}

impl SlotState {
    /// The clip held here, if any. A slot still recording has none yet.
    pub fn clip(self) -> Option<ClipId> {
        match self {
            Self::Empty | Self::QueuedRecord { .. } | Self::Recording { .. } => None,
            Self::Stopped { clip }
            | Self::QueuedPlay { clip, .. }
            | Self::Playing { clip }
            | Self::QueuedStop { clip, .. } => Some(clip),
        }
    }

    /// Whether this slot is currently producing or consuming audio.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Recording { .. } | Self::Playing { .. } | Self::QueuedStop { .. }
        )
    }

    /// Whether this slot is putting audio out.
    ///
    /// A queued stop is still sounding until its boundary arrives.
    pub fn is_sounding(self) -> bool {
        matches!(self, Self::Playing { .. } | Self::QueuedStop { .. })
    }

    /// Whether this slot is waiting on a bar boundary.
    pub fn is_pending(self) -> bool {
        match self {
            Self::QueuedRecord { .. } | Self::QueuedPlay { .. } | Self::QueuedStop { .. } => true,
            Self::Recording { ends_at, .. } => ends_at.is_some(),
            Self::Empty | Self::Stopped { .. } | Self::Playing { .. } => false,
        }
    }

    /// Whether this slot is recording or about to.
    pub fn is_recording(self) -> bool {
        matches!(self, Self::QueuedRecord { .. } | Self::Recording { .. })
    }
}

/// Something that happened to a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotInput {
    /// The user pressed this pad.
    Press,
    /// The transport reached [`Ctx::now`]; fire anything that has come due.
    Advance,
    /// Another slot on this track is taking over at the given boundary.
    Yield {
        /// When the takeover happens.
        at: Frames,
    },
    /// Stop immediately, without quantisation. Discards an in-progress recording.
    Stop,
    /// Forget the clip held here.
    Clear,
}

/// Work to carry out at an exact frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Begin capturing input.
    StartCapture {
        /// The frame capture begins on.
        at: Frames,
    },
    /// Seal the captured input into a clip.
    FinishCapture {
        /// The id to give the new clip, from [`Ctx::next_clip_id`].
        clip: ClipId,
        /// The frame capture began on; fixes the clip's loop phase against the grid.
        started_at: Frames,
        /// The frame capture ends on.
        at: Frames,
    },
    /// Discard the in-progress recording.
    CancelCapture,
    /// Begin playing a clip.
    StartPlayback {
        /// The clip to play.
        clip: ClipId,
        /// The frame playback begins on.
        at: Frames,
    },
    /// Stop playing whatever this slot was playing.
    StopPlayback {
        /// The frame playback ends on.
        at: Frames,
    },
    /// Drop this slot's reference to a clip.
    ReleaseClip {
        /// The clip being let go.
        clip: ClipId,
    },
}

/// The most effects a single [`step`] can produce.
const MAX_EFFECTS: usize = 3;

/// A fixed-capacity effect list, inline so [`step`] never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effects {
    buf: [Option<Effect>; MAX_EFFECTS],
    len: usize,
}

impl Effects {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, effect: Effect) {
        debug_assert!(self.len < MAX_EFFECTS, "MAX_EFFECTS is too small");
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = Some(effect);
            self.len += 1;
        }
    }

    /// How many effects were produced.
    pub fn len(self) -> usize {
        self.len
    }

    /// Whether the transition produced no effects.
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The effects, in the order they should be applied.
    pub fn iter(&self) -> impl Iterator<Item = Effect> + '_ {
        self.buf.iter().take(self.len).copied().flatten()
    }
}

impl FromIterator<Effect> for Effects {
    fn from_iter<I: IntoIterator<Item = Effect>>(iter: I) -> Self {
        let mut effects = Self::new();
        for effect in iter {
            effects.push(effect);
        }
        effects
    }
}

/// Everything [`step`] needs to know about the world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ctx {
    /// The transport position this step is being evaluated at.
    pub now: Frames,
    /// The bar grid.
    pub grid: BarGrid,
    /// The id to give the next clip that finishes recording.
    pub next_clip_id: ClipId,
}

/// Advances one slot. Pure and allocation-free.
pub fn step(state: SlotState, input: SlotInput, ctx: &Ctx) -> (SlotState, Effects) {
    match input {
        SlotInput::Press => press(state, ctx),
        SlotInput::Advance => advance(state, ctx),
        SlotInput::Yield { at } => yield_to_sibling(state, at),
        SlotInput::Stop => stop(state, ctx),
        SlotInput::Clear => clear(state, ctx),
    }
}

fn press(state: SlotState, ctx: &Ctx) -> (SlotState, Effects) {
    let next_boundary = ctx.grid.next_boundary(ctx.now);
    match state {
        // Arm for the next bar line.
        SlotState::Empty => (
            SlotState::QueuedRecord { at: next_boundary },
            Effects::new(),
        ),
        // Disarm; nothing had started.
        SlotState::QueuedRecord { .. } => (SlotState::Empty, Effects::new()),
        // Queue the record stop, rounded to whole bars.
        SlotState::Recording {
            started_at,
            ends_at: None,
        } => (
            SlotState::Recording {
                started_at,
                ends_at: Some(ctx.grid.quantize_record_end(started_at, ctx.now)),
            },
            Effects::new(),
        ),
        // Cancel the queued stop, keep recording.
        SlotState::Recording {
            started_at,
            ends_at: Some(_),
        } => (
            SlotState::Recording {
                started_at,
                ends_at: None,
            },
            Effects::new(),
        ),
        SlotState::Stopped { clip } => (
            SlotState::QueuedPlay {
                clip,
                at: next_boundary,
            },
            Effects::new(),
        ),
        SlotState::QueuedPlay { clip, .. } => (SlotState::Stopped { clip }, Effects::new()),
        SlotState::Playing { clip } => (
            SlotState::QueuedStop {
                clip,
                at: next_boundary,
            },
            Effects::new(),
        ),
        SlotState::QueuedStop { clip, .. } => (SlotState::Playing { clip }, Effects::new()),
    }
}

fn advance(state: SlotState, ctx: &Ctx) -> (SlotState, Effects) {
    match state {
        SlotState::QueuedRecord { at } if ctx.now >= at => (
            SlotState::Recording {
                started_at: at,
                ends_at: None,
            },
            Effects::from_iter([Effect::StartCapture { at }]),
        ),

        SlotState::Recording {
            started_at,
            ends_at,
        } => {
            // A recording nobody has stopped runs until the storage does.
            let Some(due) = ends_at else {
                return (state, Effects::new());
            };
            if ctx.now < due {
                return (state, Effects::new());
            }
            let clip = ctx.next_clip_id;
            (
                // Capture rolls straight into playback.
                SlotState::Playing { clip },
                Effects::from_iter([
                    Effect::FinishCapture {
                        clip,
                        started_at,
                        at: due,
                    },
                    Effect::StartPlayback { clip, at: due },
                ]),
            )
        }

        SlotState::QueuedPlay { clip, at } if ctx.now >= at => (
            SlotState::Playing { clip },
            Effects::from_iter([Effect::StartPlayback { clip, at }]),
        ),

        SlotState::QueuedStop { clip, at } if ctx.now >= at => (
            SlotState::Stopped { clip },
            Effects::from_iter([Effect::StopPlayback { at }]),
        ),

        _ => (state, Effects::new()),
    }
}

fn yield_to_sibling(state: SlotState, at: Frames) -> (SlotState, Effects) {
    match state {
        // Hand over on the boundary the newcomer starts on.
        SlotState::Playing { clip } => (SlotState::QueuedStop { clip, at }, Effects::new()),
        // Never started, so nothing to stop.
        SlotState::QueuedPlay { clip, .. } => (SlotState::Stopped { clip }, Effects::new()),
        _ => (state, Effects::new()),
    }
}

fn stop(state: SlotState, ctx: &Ctx) -> (SlotState, Effects) {
    match state {
        SlotState::Playing { clip } | SlotState::QueuedStop { clip, .. } => (
            SlotState::Stopped { clip },
            Effects::from_iter([Effect::StopPlayback { at: ctx.now }]),
        ),
        SlotState::QueuedPlay { clip, .. } => (SlotState::Stopped { clip }, Effects::new()),
        SlotState::Recording { .. } => (
            SlotState::Empty,
            Effects::from_iter([Effect::CancelCapture]),
        ),
        SlotState::QueuedRecord { .. } => (SlotState::Empty, Effects::new()),
        SlotState::Empty | SlotState::Stopped { .. } => (state, Effects::new()),
    }
}

fn clear(state: SlotState, ctx: &Ctx) -> (SlotState, Effects) {
    match state {
        SlotState::Playing { clip } | SlotState::QueuedStop { clip, .. } => (
            SlotState::Empty,
            Effects::from_iter([
                Effect::StopPlayback { at: ctx.now },
                Effect::ReleaseClip { clip },
            ]),
        ),
        SlotState::Stopped { clip } | SlotState::QueuedPlay { clip, .. } => (
            SlotState::Empty,
            Effects::from_iter([Effect::ReleaseClip { clip }]),
        ),
        SlotState::Recording { .. } => (
            SlotState::Empty,
            Effects::from_iter([Effect::CancelCapture]),
        ),
        SlotState::Empty | SlotState::QueuedRecord { .. } => (SlotState::Empty, Effects::new()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::time::{SampleRate, Tempo, TimeSignature};

    const BAR: u64 = 96_000; // 120 bpm, 4/4, 48 kHz

    fn ctx(now: u64) -> Ctx {
        Ctx {
            now: Frames(now),
            grid: BarGrid::new(
                SampleRate::new(48_000).unwrap(),
                Tempo::new(120.0).unwrap(),
                TimeSignature::FOUR_FOUR,
            )
            .unwrap(),
            next_clip_id: ClipId(7),
        }
    }

    fn effects(e: Effects) -> Vec<Effect> {
        e.iter().collect()
    }

    #[test]
    fn press_on_empty_arms_for_the_next_bar() {
        let (state, fx) = step(SlotState::Empty, SlotInput::Press, &ctx(BAR + 1_000));
        assert_eq!(
            state,
            SlotState::QueuedRecord {
                at: Frames(2 * BAR)
            }
        );
        assert!(fx.is_empty());
    }

    #[test]
    fn press_on_an_exact_boundary_arms_for_that_boundary() {
        let (state, _) = step(SlotState::Empty, SlotInput::Press, &ctx(2 * BAR));
        assert_eq!(
            state,
            SlotState::QueuedRecord {
                at: Frames(2 * BAR)
            }
        );
    }

    #[test]
    fn second_press_disarms() {
        let armed = SlotState::QueuedRecord {
            at: Frames(2 * BAR),
        };
        let (state, fx) = step(armed, SlotInput::Press, &ctx(BAR + 2_000));
        assert_eq!(state, SlotState::Empty);
        assert!(fx.is_empty());
    }

    #[test]
    fn advance_does_nothing_before_the_boundary() {
        let armed = SlotState::QueuedRecord {
            at: Frames(2 * BAR),
        };
        let (state, fx) = step(armed, SlotInput::Advance, &ctx(2 * BAR - 1));
        assert_eq!(state, armed);
        assert!(fx.is_empty());
    }

    #[test]
    fn advance_starts_capture_on_the_boundary() {
        let armed = SlotState::QueuedRecord {
            at: Frames(2 * BAR),
        };
        let (state, fx) = step(armed, SlotInput::Advance, &ctx(2 * BAR));
        assert_eq!(
            state,
            SlotState::Recording {
                started_at: Frames(2 * BAR),
                ends_at: None,
            }
        );
        assert_eq!(
            effects(fx),
            vec![Effect::StartCapture {
                at: Frames(2 * BAR)
            }]
        );
    }

    /// Arm midway through bar 2, stop just past bar 7: a four-bar loop, already playing.
    #[test]
    fn arm_mid_bar_stop_slightly_late_yields_a_whole_number_of_bars() {
        let (state, _) = step(SlotState::Empty, SlotInput::Press, &ctx(2 * BAR + BAR / 2));
        assert_eq!(
            state,
            SlotState::QueuedRecord {
                at: Frames(3 * BAR)
            }
        );

        let (state, _) = step(state, SlotInput::Advance, &ctx(3 * BAR));
        let started_at = Frames(3 * BAR);
        assert_eq!(
            state,
            SlotState::Recording {
                started_at,
                ends_at: None
            }
        );

        // Just past the bar 7 line, so bars 3 to 6 are what finished.
        let late = 7 * BAR + 12_000;
        let (state, fx) = step(state, SlotInput::Press, &ctx(late));
        assert_eq!(
            state,
            SlotState::Recording {
                started_at,
                ends_at: Some(Frames(7 * BAR)),
            }
        );
        assert!(fx.is_empty());

        let (state, fx) = step(state, SlotInput::Advance, &ctx(7 * BAR));
        assert_eq!(state, SlotState::Playing { clip: ClipId(7) });
        assert_eq!(
            effects(fx),
            vec![
                Effect::FinishCapture {
                    clip: ClipId(7),
                    started_at,
                    at: Frames(7 * BAR),
                },
                Effect::StartPlayback {
                    clip: ClipId(7),
                    at: Frames(7 * BAR),
                },
            ]
        );

        let len = 7 * BAR - started_at.0;
        assert_eq!(len / BAR, 4);
        assert_eq!(len % BAR, 0);
    }

    #[test]
    fn stop_pressed_mid_bar_discards_that_bar() {
        let started_at = Frames(BAR);
        let recording = SlotState::Recording {
            started_at,
            ends_at: None,
        };
        let (state, _) = step(recording, SlotInput::Press, &ctx(3 * BAR + BAR / 2));
        assert_eq!(
            state,
            SlotState::Recording {
                started_at,
                ends_at: Some(Frames(3 * BAR)),
            }
        );
    }

    #[test]
    fn pressing_again_cancels_a_queued_record_stop() {
        let started_at = Frames(BAR);
        let queued = SlotState::Recording {
            started_at,
            ends_at: Some(Frames(4 * BAR)),
        };
        let (state, _) = step(queued, SlotInput::Press, &ctx(3 * BAR + 1_000));
        assert_eq!(
            state,
            SlotState::Recording {
                started_at,
                ends_at: None
            }
        );
    }

    #[test]
    fn a_recording_nobody_stops_carries_on() {
        let started_at = Frames(BAR);
        let recording = SlotState::Recording {
            started_at,
            ends_at: None,
        };
        let ctx = ctx(BAR + 400 * BAR);

        // Nothing but a stop or the storage running out ends a take.
        let (state, fx) = step(recording, SlotInput::Advance, &ctx);
        assert_eq!(state, recording);
        assert!(effects(fx).is_empty());
    }

    #[test]
    fn a_recording_ends_where_it_was_told_to() {
        let started_at = Frames(BAR);
        let recording = SlotState::Recording {
            started_at,
            ends_at: Some(Frames(3 * BAR)),
        };
        let ctx = ctx(3 * BAR);

        let (state, fx) = step(recording, SlotInput::Advance, &ctx);
        assert_eq!(state, SlotState::Playing { clip: ClipId(7) });
        assert_eq!(
            effects(fx).first(),
            Some(&Effect::FinishCapture {
                clip: ClipId(7),
                started_at,
                at: Frames(3 * BAR),
            })
        );
    }

    #[test]
    fn launch_and_stop_are_quantised_and_cancellable() {
        let clip = ClipId(3);

        let (state, _) = step(
            SlotState::Stopped { clip },
            SlotInput::Press,
            &ctx(BAR + 500),
        );
        assert_eq!(
            state,
            SlotState::QueuedPlay {
                clip,
                at: Frames(2 * BAR)
            }
        );

        let (cancelled, fx) = step(state, SlotInput::Press, &ctx(BAR + 600));
        assert_eq!(cancelled, SlotState::Stopped { clip });
        assert!(fx.is_empty());

        let (state, fx) = step(state, SlotInput::Advance, &ctx(2 * BAR));
        assert_eq!(state, SlotState::Playing { clip });
        assert_eq!(
            effects(fx),
            vec![Effect::StartPlayback {
                clip,
                at: Frames(2 * BAR)
            }]
        );

        let (state, _) = step(state, SlotInput::Press, &ctx(2 * BAR + 10));
        assert_eq!(
            state,
            SlotState::QueuedStop {
                clip,
                at: Frames(3 * BAR)
            }
        );

        let (state, fx) = step(state, SlotInput::Advance, &ctx(3 * BAR));
        assert_eq!(state, SlotState::Stopped { clip });
        assert_eq!(
            effects(fx),
            vec![Effect::StopPlayback {
                at: Frames(3 * BAR)
            }]
        );
    }

    #[test]
    fn yield_hands_over_on_the_newcomers_boundary() {
        let clip = ClipId(1);
        let at = Frames(4 * BAR);

        let (state, fx) = step(
            SlotState::Playing { clip },
            SlotInput::Yield { at },
            &ctx(0),
        );
        assert_eq!(state, SlotState::QueuedStop { clip, at });
        assert!(fx.is_empty(), "handover fires on Advance");

        let (state, fx) = step(
            SlotState::QueuedPlay {
                clip,
                at: Frames(4 * BAR),
            },
            SlotInput::Yield { at },
            &ctx(0),
        );
        assert_eq!(state, SlotState::Stopped { clip });
        assert!(fx.is_empty());
    }

    #[test]
    fn yield_leaves_unrelated_states_alone() {
        for state in [
            SlotState::Empty,
            SlotState::Stopped { clip: ClipId(1) },
            SlotState::Recording {
                started_at: Frames(0),
                ends_at: None,
            },
        ] {
            let (next, fx) = step(state, SlotInput::Yield { at: Frames(BAR) }, &ctx(0));
            assert_eq!(next, state);
            assert!(fx.is_empty());
        }
    }

    #[test]
    fn stop_is_immediate_and_discards_a_recording() {
        let (state, fx) = step(
            SlotState::Recording {
                started_at: Frames(0),
                ends_at: None,
            },
            SlotInput::Stop,
            &ctx(BAR + 1),
        );
        assert_eq!(state, SlotState::Empty);
        assert_eq!(effects(fx), vec![Effect::CancelCapture]);

        let (state, fx) = step(
            SlotState::Playing { clip: ClipId(2) },
            SlotInput::Stop,
            &ctx(BAR + 1),
        );
        assert_eq!(state, SlotState::Stopped { clip: ClipId(2) });
        assert_eq!(
            effects(fx),
            vec![Effect::StopPlayback {
                at: Frames(BAR + 1)
            }]
        );
    }

    #[test]
    fn clear_releases_the_clip() {
        let clip = ClipId(9);
        let (state, fx) = step(SlotState::Playing { clip }, SlotInput::Clear, &ctx(BAR));
        assert_eq!(state, SlotState::Empty);
        assert_eq!(
            effects(fx),
            vec![
                Effect::StopPlayback { at: Frames(BAR) },
                Effect::ReleaseClip { clip },
            ]
        );

        let (state, fx) = step(SlotState::Stopped { clip }, SlotInput::Clear, &ctx(BAR));
        assert_eq!(state, SlotState::Empty);
        assert_eq!(effects(fx), vec![Effect::ReleaseClip { clip }]);

        let (state, fx) = step(SlotState::Empty, SlotInput::Clear, &ctx(BAR));
        assert_eq!(state, SlotState::Empty);
        assert!(fx.is_empty());
    }

    #[test]
    fn advance_is_idempotent_once_a_transition_has_fired() {
        let armed = SlotState::QueuedRecord { at: Frames(BAR) };
        let (state, _) = step(armed, SlotInput::Advance, &ctx(BAR));
        let (again, fx) = step(state, SlotInput::Advance, &ctx(BAR));
        assert_eq!(again, state);
        assert!(fx.is_empty(), "capture must not restart every block");
    }
}
