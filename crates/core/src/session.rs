//! The whole grid, and the rules that span more than one slot.
//!
//! Pure and allocation-free like [`crate::slot`]: effects go to a caller-supplied sink
//! rather than a collection.

use crate::ids::{SLOT_COUNT, SlotAddr, SlotId, TRACK_COUNT, TrackId};
use crate::slot::{Ctx, Effect, SlotInput, SlotState, step};

/// The state of all 64 pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionModel {
    slots: [[SlotState; SLOT_COUNT]; TRACK_COUNT],
}

impl SessionModel {
    /// An empty session.
    pub fn new() -> Self {
        Self {
            slots: [[SlotState::Empty; SLOT_COUNT]; TRACK_COUNT],
        }
    }

    /// What a pad is doing.
    pub fn state(&self, addr: SlotAddr) -> SlotState {
        self.slots[addr.track.index()][addr.slot.index()]
    }

    fn set(&mut self, addr: SlotAddr, state: SlotState) {
        self.slots[addr.track.index()][addr.slot.index()] = state;
    }

    /// Overwrites a pad's state without running the machine.
    ///
    /// For a mirror kept in step with a report from elsewhere. Using it on the model
    /// that owns the transitions would skip the rules in [`Self::press`].
    pub fn mirror(&mut self, addr: SlotAddr, state: SlotState) {
        self.set(addr, state);
    }

    /// Whether any pad holds a clip. The tempo is locked once this is true — changing
    /// it would leave existing clips misaligned with the grid.
    pub fn has_any_clip(&self) -> bool {
        SlotAddr::all().any(|addr| self.state(addr).clip().is_some())
    }

    /// Every pad on a track.
    fn track_addrs(track: TrackId) -> impl Iterator<Item = SlotAddr> {
        SlotId::all().map(move |slot| SlotAddr::new(track, slot))
    }

    /// Handles a press, applying the one-slot-per-track rule.
    ///
    /// While a track is recording, presses on its other pads are ignored: arming a
    /// second pad mid-recording would seal, auto-play and stop the first in one
    /// instant, with no useful reading of the gesture.
    pub fn press(&mut self, addr: SlotAddr, ctx: &Ctx, sink: &mut impl FnMut(SlotAddr, Effect)) {
        let busy_elsewhere = Self::track_addrs(addr.track)
            .any(|other| other != addr && self.state(other).is_recording());
        if busy_elsewhere {
            return;
        }

        let (state, effects) = step(self.state(addr), SlotInput::Press, ctx);
        self.set(addr, state);
        for effect in effects.iter() {
            sink(addr, effect);
        }

        // Whatever this pad takes over from hands back on the same boundary.
        let (SlotState::QueuedRecord { at: takeover_at }
        | SlotState::QueuedPlay {
            at: takeover_at, ..
        }) = state
        else {
            return;
        };

        for other in Self::track_addrs(addr.track) {
            if other != addr {
                self.apply(other, SlotInput::Yield { at: takeover_at }, ctx, sink);
            }
        }
    }

    /// Forgets the clip in a pad.
    pub fn clear(&mut self, addr: SlotAddr, ctx: &Ctx, sink: &mut impl FnMut(SlotAddr, Effect)) {
        self.apply(addr, SlotInput::Clear, ctx, sink);
    }

    /// Stops a track immediately.
    pub fn stop_track(
        &mut self,
        track: TrackId,
        ctx: &Ctx,
        sink: &mut impl FnMut(SlotAddr, Effect),
    ) {
        for addr in Self::track_addrs(track) {
            self.apply(addr, SlotInput::Stop, ctx, sink);
        }
    }

    /// Stops everything immediately. Recordings in progress are discarded.
    pub fn stop_all(&mut self, ctx: &Ctx, sink: &mut impl FnMut(SlotAddr, Effect)) {
        for addr in SlotAddr::all() {
            self.apply(addr, SlotInput::Stop, ctx, sink);
        }
    }

    /// Fires every transition due at [`Ctx::now`]. Call it at each bar boundary so
    /// effects land on the exact scheduled frame.
    pub fn advance(&mut self, ctx: &Ctx, sink: &mut impl FnMut(SlotAddr, Effect)) {
        for addr in SlotAddr::all() {
            self.apply(addr, SlotInput::Advance, ctx, sink);
        }
    }

    fn apply(
        &mut self,
        addr: SlotAddr,
        input: SlotInput,
        ctx: &Ctx,
        sink: &mut impl FnMut(SlotAddr, Effect),
    ) {
        let (state, effects) = step(self.state(addr), input, ctx);
        self.set(addr, state);
        for effect in effects.iter() {
            sink(addr, effect);
        }
    }
}

impl Default for SessionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::ids::ClipId;
    use crate::time::{BarGrid, Frames, SampleRate, Tempo, TimeSignature};

    const BAR: u64 = 96_000; // 120 bpm, 4/4, 48 kHz

    fn ctx(now: u64, next_clip_id: u32) -> Ctx {
        Ctx {
            now: Frames(now),
            grid: BarGrid::new(
                SampleRate::new(48_000).unwrap(),
                Tempo::new(120.0).unwrap(),
                TimeSignature::FOUR_FOUR,
            )
            .unwrap(),
            max_bars: 64,
            next_clip_id: ClipId(next_clip_id),
        }
    }

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    fn ignore(_: SlotAddr, _: Effect) {}

    /// Records `bars` bars into `at` from bar `start_bar`, leaving it playing.
    fn record(model: &mut SessionModel, at: SlotAddr, start_bar: u64, bars: u64, clip: u32) {
        model.press(at, &ctx(start_bar * BAR, clip), &mut ignore);
        model.advance(&ctx(start_bar * BAR, clip), &mut ignore);
        let end = (start_bar + bars) * BAR;
        model.press(at, &ctx(end, clip), &mut ignore);
        model.advance(&ctx(end, clip), &mut ignore);
        assert_eq!(model.state(at), SlotState::Playing { clip: ClipId(clip) });
    }

    #[test]
    fn a_mirrored_state_is_taken_verbatim() {
        let mut model = SessionModel::new();
        let target = addr(4, 6);
        model.mirror(target, SlotState::Playing { clip: ClipId(3) });

        assert_eq!(model.state(target), SlotState::Playing { clip: ClipId(3) });
        assert!(model.has_any_clip());
        assert_eq!(model.state(addr(4, 5)), SlotState::Empty);
    }

    #[test]
    fn a_new_session_is_empty() {
        let model = SessionModel::new();
        assert!(!model.has_any_clip());
        assert!(SlotAddr::all().all(|a| model.state(a) == SlotState::Empty));
    }

    #[test]
    fn recording_a_clip_marks_the_session_as_having_clips() {
        let mut model = SessionModel::new();
        assert!(!model.has_any_clip());
        record(&mut model, addr(0, 0), 1, 2, 0);
        assert!(model.has_any_clip(), "tempo must lock once a clip exists");
    }

    #[test]
    fn launching_a_sibling_hands_over_on_the_same_boundary() {
        let mut model = SessionModel::new();
        let first = addr(0, 0);
        let second = addr(0, 1);

        record(&mut model, first, 1, 2, 0);
        record(&mut model, second, 4, 1, 1);
        // The second took over from the first as it armed.
        assert_eq!(model.state(first), SlotState::Stopped { clip: ClipId(0) });

        // Relaunching the first queues the second to stop on the same boundary.
        model.press(first, &ctx(6 * BAR + 1_000, 2), &mut ignore);
        let boundary = Frames(7 * BAR);
        assert_eq!(
            model.state(first),
            SlotState::QueuedPlay {
                clip: ClipId(0),
                at: boundary
            }
        );
        assert_eq!(
            model.state(second),
            SlotState::QueuedStop {
                clip: ClipId(1),
                at: boundary
            }
        );

        model.advance(&ctx(7 * BAR, 2), &mut ignore);
        assert_eq!(model.state(first), SlotState::Playing { clip: ClipId(0) });
        assert_eq!(model.state(second), SlotState::Stopped { clip: ClipId(1) });
    }

    #[test]
    fn only_one_slot_per_track_ever_sounds() {
        let mut model = SessionModel::new();
        record(&mut model, addr(0, 0), 1, 1, 0);
        record(&mut model, addr(0, 1), 3, 1, 1);
        record(&mut model, addr(0, 2), 5, 1, 2);

        let sounding = SlotId::all()
            .filter(|&slot| {
                model
                    .state(SlotAddr::new(TrackId::new(0).unwrap(), slot))
                    .is_active()
            })
            .count();
        assert_eq!(sounding, 1);
    }

    #[test]
    fn tracks_do_not_interfere_with_each_other() {
        let mut model = SessionModel::new();
        record(&mut model, addr(0, 0), 1, 1, 0);
        record(&mut model, addr(1, 0), 3, 1, 1);

        assert_eq!(
            model.state(addr(0, 0)),
            SlotState::Playing { clip: ClipId(0) }
        );
        assert_eq!(
            model.state(addr(1, 0)),
            SlotState::Playing { clip: ClipId(1) }
        );
    }

    #[test]
    fn presses_elsewhere_on_the_track_are_ignored_while_recording() {
        let mut model = SessionModel::new();
        let recording = addr(0, 0);
        let other = addr(0, 1);

        model.press(recording, &ctx(BAR, 0), &mut ignore);
        model.advance(&ctx(BAR, 0), &mut ignore);
        assert!(model.state(recording).is_recording());

        model.press(other, &ctx(BAR + 5_000, 0), &mut ignore);
        assert_eq!(model.state(other), SlotState::Empty);
        assert_eq!(
            model.state(recording),
            SlotState::Recording {
                started_at: Frames(BAR),
                ends_at: None
            }
        );

        // Other tracks are unaffected.
        model.press(addr(1, 0), &ctx(BAR + 5_000, 0), &mut ignore);
        assert_eq!(
            model.state(addr(1, 0)),
            SlotState::QueuedRecord {
                at: Frames(2 * BAR)
            }
        );
    }

    #[test]
    fn the_recording_pad_can_still_stop_itself() {
        let mut model = SessionModel::new();
        let recording = addr(0, 0);
        model.press(recording, &ctx(BAR, 0), &mut ignore);
        model.advance(&ctx(BAR, 0), &mut ignore);

        model.press(recording, &ctx(3 * BAR, 0), &mut ignore);
        assert_eq!(
            model.state(recording),
            SlotState::Recording {
                started_at: Frames(BAR),
                ends_at: Some(Frames(3 * BAR)),
            }
        );
    }

    #[test]
    fn stop_all_silences_everything_and_discards_recordings() {
        let mut model = SessionModel::new();
        record(&mut model, addr(0, 0), 1, 1, 0);
        record(&mut model, addr(1, 0), 1, 1, 1);
        model.press(addr(2, 0), &ctx(3 * BAR, 2), &mut ignore);
        model.advance(&ctx(3 * BAR, 2), &mut ignore);
        assert!(model.state(addr(2, 0)).is_recording());

        let mut cancelled = 0;
        model.stop_all(&ctx(3 * BAR + 1_000, 2), &mut |_, effect| {
            if effect == Effect::CancelCapture {
                cancelled += 1;
            }
        });

        assert_eq!(cancelled, 1);
        assert!(SlotAddr::all().all(|a| !model.state(a).is_active()));
        assert_eq!(
            model.state(addr(0, 0)),
            SlotState::Stopped { clip: ClipId(0) }
        );
        assert_eq!(model.state(addr(2, 0)), SlotState::Empty);
    }

    #[test]
    fn clear_empties_a_pad_and_releases_its_clip() {
        let mut model = SessionModel::new();
        record(&mut model, addr(0, 0), 1, 1, 0);

        let mut released = Vec::new();
        model.clear(addr(0, 0), &ctx(3 * BAR, 1), &mut |_, effect| {
            if let Effect::ReleaseClip { clip } = effect {
                released.push(clip);
            }
        });

        assert_eq!(released, vec![ClipId(0)]);
        assert_eq!(model.state(addr(0, 0)), SlotState::Empty);
        assert!(!model.has_any_clip());
    }

    #[test]
    fn advance_reports_effects_against_the_right_pads() {
        let mut model = SessionModel::new();
        let target = addr(3, 5);
        model.press(target, &ctx(BAR + 1_000, 0), &mut ignore);

        let mut seen = Vec::new();
        model.advance(&ctx(2 * BAR, 0), &mut |a, effect| seen.push((a, effect)));

        assert_eq!(
            seen,
            vec![(
                target,
                Effect::StartCapture {
                    at: Frames(2 * BAR)
                }
            )]
        );
    }
}
