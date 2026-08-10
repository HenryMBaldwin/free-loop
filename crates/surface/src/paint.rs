//! Turning session state into a frame.
//!
//! The one place the looper's colour scheme is decided. Pure, so the whole scheme is
//! testable without a device.

use free_loop_core::{
    MAX_BPM, MIN_BPM, PadMask, SLOT_COUNT, SessionModel, SlotAddr, SlotState, TRACK_COUNT, pad_bit,
};

use crate::event::Control;
use crate::led::{BEAT_LEDS, FIRST_BEAT_LED, Led, LedColor, LedFrame, LedStyle};

/// Surface state that does not come from the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chrome {
    /// Beat within the bar, zero-based.
    pub beat: u32,
    /// Beats in a bar.
    pub beats_per_bar: u32,
    /// Whether the click is sounding.
    pub click_enabled: bool,
    /// Whether the transport is frozen.
    pub paused: bool,
    /// Which way mute and solo group the grid.
    pub axis: Axis,
    /// Pads that do not sound.
    pub muted: PadMask,
    /// Pads that sound to the exclusion of the rest.
    pub soloed: PadMask,
}

/// How mute and solo group the grid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Axis {
    /// A row at a time, so one track.
    #[default]
    Row,
    /// A column at a time, so one slot across every track.
    Column,
}

impl Axis {
    /// The other one.
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Row => Self::Column,
            Self::Column => Self::Row,
        }
    }

    /// The colour that stands for this grouping wherever it is shown.
    pub fn color(self) -> LedColor {
        match self {
            Self::Row => LedColor::Purple,
            Self::Column => LedColor::Blue,
        }
    }
}

impl Chrome {
    /// The colour a pad's whole group has been marked with, if any.
    ///
    /// Each colour matches the side button that sets it, so the grid says which of the
    /// two is in play without a legend. Silence outranks solo, matching the engine, so a
    /// group that is both reads as silenced rather than as heard.
    pub fn mark(&self, addr: SlotAddr) -> Option<LedColor> {
        let bit = pad_bit(addr);
        if self.muted & bit != 0 {
            return Some(MUTED);
        }
        if self.soloed & bit != 0 {
            return Some(SOLOED);
        }
        None
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            beat: 0,
            beats_per_bar: 4,
            click_enabled: true,
            paused: false,
            axis: Axis::Row,
            muted: 0,
            soloed: 0,
        }
    }
}

/// How a pad looks in a given state.
///
/// Flashing means waiting on a bar line, so every queued state flashes and every settled
/// state does not.
pub fn pad(state: SlotState) -> Led {
    match state {
        SlotState::Empty => Led::OFF,
        SlotState::QueuedRecord { .. } => Led::flash(LedColor::Red),
        SlotState::Recording { .. } => Led::solid(LedColor::Red),
        SlotState::Stopped { .. } => Led::dim(LedColor::Amber),
        SlotState::QueuedPlay { .. } => Led::flash(LedColor::Green),
        SlotState::Playing { .. } => Led::pulse(LedColor::Green),
        SlotState::QueuedStop { .. } => Led::flash(LedColor::Amber),
    }
}

/// How a pad looks when its whole group has been marked.
///
/// The group lights end to end, empty pads included, so the row or column reads as one
/// thing. Within it the colour is fixed and the state is carried by brightness and
/// movement: an empty pad sits dim, a clip waiting is steady, one playing pulses, and one
/// waiting on a bar line flashes. What the pad is doing survives the shift.
pub fn marked_pad(state: SlotState, color: LedColor) -> Led {
    let style = match state {
        SlotState::Empty => LedStyle::Dim,
        SlotState::Stopped { .. } | SlotState::Recording { .. } => LedStyle::Solid,
        SlotState::Playing { .. } => LedStyle::Pulse,
        SlotState::QueuedRecord { .. }
        | SlotState::QueuedPlay { .. }
        | SlotState::QueuedStop { .. } => LedStyle::Flash,
    };
    Led { color, style }
}

/// How a top-row control looks.
pub fn control(control: Control, chrome: Chrome) -> Led {
    match control {
        Control::ClickToggle => {
            if chrome.click_enabled {
                Led::solid(LedColor::Blue)
            } else {
                Led::dim(LedColor::Blue)
            }
        }
        Control::TempoDown | Control::TempoUp => Led::dim(LedColor::Blue),
        Control::StopAll => Led::dim(LedColor::Red),
        Control::Rewind => Led::dim(LedColor::Green),
        // Carries the grouping colour, so the axis is readable without pressing anything.
        Control::Axis => Led::solid(chrome.axis.color()),
        Control::LoadSession | Control::SaveSession => Led::dim(LedColor::White),
    }
}

/// Colour a button takes while it is waiting for the press that follows it.
///
/// One colour for every such button, so anything flashing pink means the same thing
/// wherever it is.
pub const SELECTED: LedColor = LedColor::Pink;

/// Colour a silenced group takes, matching its side button.
pub const MUTED: LedColor = LedColor::Red;

/// Colour a soloed group takes, matching its side button.
pub const SOLOED: LedColor = LedColor::Blue;

/// The right-hand column button that runs the transport.
pub const PAUSE_SIDE: usize = 4;

/// The right-hand column button that opens the silenced pads.
pub const MUTE_SIDE: usize = 5;

/// The right-hand column button that opens the soloed pads.
pub const SOLO_SIDE: usize = 6;

/// The right-hand column button that starts a session from nothing.
///
/// Only offered from the load picker, since emptying the grid is not something to have
/// within reach while playing.
pub const NEW_SIDE: usize = 7;

/// How the transport button looks.
pub fn pause_button(chrome: Chrome) -> Led {
    if chrome.paused {
        Led::flash(LedColor::Amber)
    } else {
        Led::dim(LedColor::Green)
    }
}

/// Paints the beat indicator over whatever the shared buttons already show.
///
/// One button lit at a time, beat one in white so the bar line is unmistakable at a
/// glance. Only the current beat is overwritten, so the buttons it shares keep their own
/// colour the rest of the time. Meters wider than the indicator show only the beats that
/// fit.
pub fn beat_indicator(frame: &mut LedFrame, chrome: Chrome) {
    let shown = usize::try_from(chrome.beats_per_bar)
        .unwrap_or(BEAT_LEDS)
        .min(BEAT_LEDS);
    let beat = usize::try_from(chrome.beat).unwrap_or(usize::MAX);
    if beat >= shown {
        return;
    }

    let led = if beat == 0 {
        Led::solid(LedColor::White)
    } else {
        Led::solid(LedColor::Blue)
    };
    frame.set_control(FIRST_BEAT_LED + beat, led);
}

/// Paints the tempo as a fill across the grid.
///
/// Shown while a tempo button is held, where the number cannot be: each text update
/// restarts the scroll. The grid spans [`MIN_BPM`] to [`MAX_BPM`], so one pad is about
/// the size of one step of the hold and the fill rate is the rate.
pub fn tempo_gauge(tempo: f64, chrome: Chrome) -> LedFrame {
    let mut frame = LedFrame::new();
    let total = TRACK_COUNT * SLOT_COUNT;

    let fraction = ((tempo - MIN_BPM) / (MAX_BPM - MIN_BPM)).clamp(0.0, 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a fraction of 64, clamped above"
    )]
    let lit = (fraction * total as f64).round() as usize;

    for (index, addr) in SlotAddr::all().enumerate() {
        let led = if index + 1 == lit {
            Led::solid(LedColor::White)
        } else if index < lit {
            Led::dim(LedColor::Blue)
        } else {
            Led::OFF
        };
        frame.set_pad(addr, led);
    }

    for button in Control::all() {
        frame.set_control(button.index(), control(button, chrome));
    }
    beat_indicator(&mut frame, chrome);
    side_buttons(&mut frame, chrome);

    frame
}

/// Paints the session picker over the grid.
///
/// `existing` has a bit set per pad that holds a session, indexed track-major. `current`
/// is the session in use, shown differently so an overwrite is deliberate. `active` is
/// the button that opened the picker, flashed so the mode is obvious.
pub fn picker(
    existing: u64,
    current: Option<SlotAddr>,
    chrome: Chrome,
    active: Control,
) -> LedFrame {
    let mut frame = LedFrame::new();

    for addr in SlotAddr::all() {
        let bit = 1_u64 << (addr.track.index() * SLOT_COUNT + addr.slot.index());
        let led = if current == Some(addr) {
            Led::solid(LedColor::Green)
        } else if existing & bit != 0 {
            Led::dim(LedColor::White)
        } else {
            Led::OFF
        };
        frame.set_pad(addr, led);
    }

    for button in Control::all() {
        frame.set_control(button.index(), control(button, chrome));
    }
    frame.set_control(active.index(), Led::flash(SELECTED));
    side_buttons(&mut frame, chrome);
    if active == Control::LoadSession {
        frame.set_side(NEW_SIDE, Led::dim(LedColor::White));
    }

    frame
}

/// Paints the whole surface.
pub fn frame(session: &SessionModel, chrome: Chrome) -> LedFrame {
    let mut frame = LedFrame::new();

    for addr in SlotAddr::all() {
        let state = session.state(addr);
        let led = chrome
            .mark(addr)
            .map_or_else(|| pad(state), |color| marked_pad(state, color));
        frame.set_pad(addr, led);
    }
    for button in Control::all() {
        frame.set_control(button.index(), control(button, chrome));
    }
    beat_indicator(&mut frame, chrome);
    side_buttons(&mut frame, chrome);

    frame
}

/// Paints the right-hand column.
fn side_buttons(frame: &mut LedFrame, chrome: Chrome) {
    frame.set_side(PAUSE_SIDE, pause_button(chrome));
    frame.set_side(
        MUTE_SIDE,
        if chrome.muted != 0 {
            Led::solid(LedColor::Red)
        } else {
            Led::dim(LedColor::Red)
        },
    );
    frame.set_side(
        SOLO_SIDE,
        if chrome.soloed != 0 {
            Led::solid(LedColor::Blue)
        } else {
            Led::dim(LedColor::Blue)
        },
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use free_loop_core::{ClipId, Frames, SlotId, TrackId, row_mask};

    use crate::led::LedStyle;

    fn bit(addr: SlotAddr) -> u64 {
        1 << (addr.track.index() * SLOT_COUNT + addr.slot.index())
    }

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    const EVERY_STATE: [SlotState; 7] = [
        SlotState::Empty,
        SlotState::QueuedRecord { at: Frames(0) },
        SlotState::Recording {
            started_at: Frames(0),
            ends_at: None,
        },
        SlotState::Stopped { clip: ClipId(0) },
        SlotState::QueuedPlay {
            clip: ClipId(0),
            at: Frames(0),
        },
        SlotState::Playing { clip: ClipId(0) },
        SlotState::QueuedStop {
            clip: ClipId(0),
            at: Frames(0),
        },
    ];

    #[test]
    fn queued_states_flash_and_settled_states_do_not() {
        for state in EVERY_STATE {
            let flashing = pad(state).style == LedStyle::Flash;
            assert_eq!(
                flashing,
                state.is_pending(),
                "{state:?} should flash iff it is waiting on a bar line"
            );
        }
    }

    #[test]
    fn every_state_is_distinguishable_by_eye() {
        let mut seen = std::collections::HashSet::new();
        for state in EVERY_STATE {
            assert!(
                seen.insert(pad(state)),
                "{state:?} duplicates another state"
            );
        }
    }

    #[test]
    fn an_empty_pad_is_dark_and_everything_else_is_lit() {
        assert!(!pad(SlotState::Empty).is_lit());
        for state in EVERY_STATE.into_iter().filter(|s| *s != SlotState::Empty) {
            assert!(pad(state).is_lit(), "{state:?} should be visible");
        }
    }

    #[test]
    fn recording_is_red_and_playing_is_green() {
        assert_eq!(pad(EVERY_STATE[2]).color, LedColor::Red);
        assert_eq!(pad(EVERY_STATE[5]).color, LedColor::Green);
        assert_eq!(pad(EVERY_STATE[5]).style, LedStyle::Pulse);
    }

    #[test]
    fn the_beat_indicator_lights_one_button_at_a_time() {
        for beat in 0..4 {
            let mut frame = LedFrame::new();
            beat_indicator(
                &mut frame,
                Chrome {
                    beat,
                    ..Chrome::default()
                },
            );

            let lit: Vec<usize> = (FIRST_BEAT_LED..FIRST_BEAT_LED + BEAT_LEDS)
                .filter(|i| frame.control(*i).is_lit())
                .collect();
            assert_eq!(lit, vec![FIRST_BEAT_LED + beat as usize]);
        }
    }

    #[test]
    fn the_buttons_the_indicator_shares_keep_their_own_colour() {
        let painted = frame(&SessionModel::new(), Chrome::default());

        // Beat one is on the tempo up button, so that one is white.
        assert_eq!(
            painted.control(Control::TempoUp.index()).color,
            LedColor::White
        );
        // Tempo down is not the current beat, so it still shows its own state.
        assert_eq!(
            painted.control(Control::TempoDown.index()),
            control(Control::TempoDown, Chrome::default())
        );
    }

    #[test]
    fn every_control_keeps_its_colour_when_no_beat_is_on_it() {
        let chrome = Chrome {
            beat: 3,
            ..Chrome::default()
        };
        let painted = frame(&SessionModel::new(), chrome);

        for button in Control::all().filter(|c| c.index() != 3) {
            assert_eq!(
                painted.control(button.index()),
                control(button, chrome),
                "{button:?} was overwritten"
            );
        }
    }

    #[test]
    fn the_transport_button_shows_whether_it_is_frozen() {
        let running = frame(&SessionModel::new(), Chrome::default());
        let frozen = frame(
            &SessionModel::new(),
            Chrome {
                paused: true,
                ..Chrome::default()
            },
        );

        assert_ne!(running.side(PAUSE_SIDE), frozen.side(PAUSE_SIDE));
        assert!(running.side(PAUSE_SIDE).is_lit());
        assert!(frozen.side(PAUSE_SIDE).is_lit());
    }

    #[test]
    fn beat_one_is_a_different_colour() {
        let mut frame = LedFrame::new();
        beat_indicator(&mut frame, Chrome::default());
        assert_eq!(frame.control(FIRST_BEAT_LED).color, LedColor::White);

        let mut frame = LedFrame::new();
        beat_indicator(
            &mut frame,
            Chrome {
                beat: 1,
                ..Chrome::default()
            },
        );
        assert_eq!(frame.control(FIRST_BEAT_LED + 1).color, LedColor::Blue);
    }

    #[test]
    fn a_wide_meter_shows_only_the_beats_that_fit() {
        let mut frame = LedFrame::new();
        beat_indicator(
            &mut frame,
            Chrome {
                beat: 6,
                beats_per_bar: 7,
                ..Chrome::default()
            },
        );
        // Beat 7 of 7 has no button, so nothing lights rather than wrapping onto a
        // control that means something else.
        assert!((FIRST_BEAT_LED..FIRST_BEAT_LED + BEAT_LEDS).all(|i| !frame.control(i).is_lit()));
    }

    #[test]
    fn the_click_button_shows_whether_it_is_on() {
        let on = control(Control::ClickToggle, Chrome::default());
        let off = control(
            Control::ClickToggle,
            Chrome {
                click_enabled: false,
                ..Chrome::default()
            },
        );
        assert_ne!(on, off);
        assert_eq!(on.style, LedStyle::Solid);
        assert_eq!(off.style, LedStyle::Dim);
    }

    #[test]
    fn a_painted_frame_follows_the_session() {
        let mut session = SessionModel::new();
        let ctx = free_loop_core::Ctx {
            now: Frames(0),
            grid: free_loop_core::BarGrid::new(
                free_loop_core::SampleRate::new(48_000).unwrap(),
                free_loop_core::Tempo::new(120.0).unwrap(),
                free_loop_core::TimeSignature::FOUR_FOUR,
            )
            .unwrap(),
            max_bars: 32,
            next_clip_id: ClipId(0),
        };
        session.press(addr(2, 3), &ctx, &mut |_, _| {});

        let painted = frame(&session, Chrome::default());
        assert_eq!(painted.pad(addr(2, 3)), Led::flash(LedColor::Red));
        assert_eq!(painted.pad(addr(3, 2)), Led::OFF);
    }

    #[test]
    fn the_gauge_fills_with_the_tempo() {
        let low = tempo_gauge(MIN_BPM, Chrome::default());
        let high = tempo_gauge(MAX_BPM, Chrome::default());

        assert!(
            SlotAddr::all().all(|a| !low.pad(a).is_lit()),
            "empty at the bottom"
        );
        assert!(
            SlotAddr::all().all(|a| high.pad(a).is_lit()),
            "full at the top"
        );
    }

    #[test]
    fn the_gauge_never_goes_backwards() {
        let lit = |bpm: f64| {
            let frame = tempo_gauge(bpm, Chrome::default());
            SlotAddr::all().filter(|a| frame.pad(*a).is_lit()).count()
        };

        let mut previous = 0;
        let mut bpm = MIN_BPM;
        while bpm <= MAX_BPM {
            let now = lit(bpm);
            assert!(
                now >= previous,
                "{bpm} lit fewer pads than the tempo below it"
            );
            previous = now;
            bpm += 5.0;
        }
    }

    #[test]
    fn one_step_of_a_hold_moves_the_gauge() {
        let lit = |bpm: f64| {
            let frame = tempo_gauge(bpm, Chrome::default());
            SlotAddr::all().filter(|a| frame.pad(*a).is_lit()).count()
        };
        assert_ne!(lit(120.0), lit(125.0), "a step should be visible");
    }

    #[test]
    fn the_gauge_marks_where_the_tempo_is() {
        let frame = tempo_gauge(120.0, Chrome::default());
        let leading = SlotAddr::all()
            .filter(|a| frame.pad(*a) == Led::solid(LedColor::White))
            .count();
        assert_eq!(leading, 1, "one pad shows the tempo itself");
    }

    #[test]
    fn the_gauge_keeps_the_transport_and_beat() {
        let frame = tempo_gauge(120.0, Chrome::default());
        assert!(frame.side(PAUSE_SIDE).is_lit());
        assert_eq!(frame.control(FIRST_BEAT_LED).color, LedColor::White);
    }

    #[test]
    fn a_marked_group_lights_end_to_end() {
        for state in EVERY_STATE {
            let led = marked_pad(state, MUTED);
            assert!(
                led.is_lit(),
                "{state:?} should light so the group reads as one thing"
            );
            assert_eq!(led.color, MUTED);
        }
    }

    #[test]
    fn a_marked_pad_still_says_what_it_is_doing() {
        // An empty pad sits dim, a clip waiting is steady, one playing pulses.
        assert_eq!(marked_pad(SlotState::Empty, MUTED).style, LedStyle::Dim);
        assert_eq!(
            marked_pad(SlotState::Stopped { clip: ClipId(0) }, MUTED).style,
            LedStyle::Solid
        );
        assert_eq!(
            marked_pad(SlotState::Playing { clip: ClipId(0) }, MUTED).style,
            LedStyle::Pulse
        );
        assert_eq!(
            marked_pad(
                SlotState::QueuedPlay {
                    clip: ClipId(0),
                    at: Frames(0)
                },
                MUTED
            )
            .style,
            LedStyle::Flash
        );
    }

    #[test]
    fn a_marked_pad_keeps_the_movement_it_had() {
        let playing = SlotState::Playing { clip: ClipId(0) };
        let stopped = SlotState::Stopped { clip: ClipId(0) };

        assert_eq!(marked_pad(playing, MUTED).style, pad(playing).style);
        assert_ne!(
            marked_pad(playing, MUTED).style,
            marked_pad(stopped, MUTED).style,
            "a pulsing loop must not look like a parked one"
        );
    }

    #[test]
    fn a_silenced_row_reads_as_a_row() {
        let mut session = SessionModel::new();
        session.mirror(addr(0, 0), SlotState::Playing { clip: ClipId(0) });
        session.mirror(addr(0, 1), SlotState::Stopped { clip: ClipId(1) });

        let painted = frame(
            &session,
            Chrome {
                muted: row_mask(TrackId::new(0).unwrap()),
                ..Chrome::default()
            },
        );

        assert_eq!(painted.pad(addr(0, 0)), Led::pulse(MUTED));
        assert_eq!(painted.pad(addr(0, 1)), Led::solid(MUTED));
        assert_eq!(
            painted.pad(addr(0, 7)),
            Led::dim(MUTED),
            "the empty end of the row lights too"
        );
        assert_eq!(painted.pad(addr(1, 0)), Led::OFF, "and stops at the row");
    }

    #[test]
    fn mute_and_solo_are_told_apart_by_colour() {
        let mut session = SessionModel::new();
        session.mirror(addr(0, 0), SlotState::Playing { clip: ClipId(0) });
        session.mirror(addr(1, 0), SlotState::Playing { clip: ClipId(1) });

        let painted = frame(
            &session,
            Chrome {
                muted: row_mask(TrackId::new(0).unwrap()),
                soloed: row_mask(TrackId::new(1).unwrap()),
                ..Chrome::default()
            },
        );

        assert_eq!(painted.pad(addr(0, 0)).color, MUTED);
        assert_eq!(painted.pad(addr(1, 0)).color, SOLOED);
        assert_ne!(MUTED, SOLOED);
    }

    #[test]
    fn silence_outranks_solo_on_the_same_group() {
        let mut session = SessionModel::new();
        session.mirror(addr(0, 0), SlotState::Playing { clip: ClipId(0) });
        let row = row_mask(TrackId::new(0).unwrap());

        let painted = frame(
            &session,
            Chrome {
                muted: row,
                soloed: row,
                ..Chrome::default()
            },
        );
        assert_eq!(painted.pad(addr(0, 0)).color, MUTED);
    }

    #[test]
    fn the_axis_button_carries_the_grouping_colour() {
        let rows = frame(&SessionModel::new(), Chrome::default());
        let columns = frame(
            &SessionModel::new(),
            Chrome {
                axis: Axis::Column,
                ..Chrome::default()
            },
        );

        let button = Control::Axis.index();
        assert_ne!(rows.control(button), columns.control(button));
        assert_eq!(rows.control(button).color, Axis::Row.color());
        assert_eq!(columns.control(button).color, Axis::Column.color());
    }

    #[test]
    fn an_axis_flips_and_comes_back() {
        assert_eq!(Axis::Row.flipped(), Axis::Column);
        assert_eq!(Axis::Row.flipped().flipped(), Axis::Row);
        assert_ne!(Axis::Row.color(), Axis::Column.color());
    }

    #[test]
    fn the_side_buttons_show_whether_anything_is_set() {
        let quiet = frame(&SessionModel::new(), Chrome::default());
        let active = frame(
            &SessionModel::new(),
            Chrome {
                muted: bit(addr(0, 0)),
                soloed: bit(addr(1, 1)),
                ..Chrome::default()
            },
        );

        assert_eq!(quiet.side(MUTE_SIDE).style, LedStyle::Dim);
        assert_eq!(active.side(MUTE_SIDE).style, LedStyle::Solid);
        assert_eq!(active.side(SOLO_SIDE).style, LedStyle::Solid);
    }

    #[test]
    fn the_picker_marks_saved_pads_and_the_current_one() {
        let saved = addr(0, 1);
        let current = addr(2, 3);
        let bit = |a: SlotAddr| 1_u64 << (a.track.index() * SLOT_COUNT + a.slot.index());

        let painted = picker(
            bit(saved) | bit(current),
            Some(current),
            Chrome::default(),
            Control::SaveSession,
        );

        assert_eq!(painted.pad(saved), Led::dim(LedColor::White));
        assert_eq!(painted.pad(current), Led::solid(LedColor::Green));
        assert_ne!(
            painted.pad(current),
            painted.pad(saved),
            "an overwrite of the session in use must look different"
        );
        assert_eq!(painted.pad(addr(7, 7)), Led::OFF);
    }

    #[test]
    fn the_picker_flashes_the_button_that_opened_it() {
        let painted = picker(0, None, Chrome::default(), Control::SaveSession);
        assert_eq!(
            painted.control(Control::SaveSession.index()),
            Led::flash(SELECTED)
        );
        assert_ne!(
            painted.control(Control::LoadSession.index()),
            Led::flash(SELECTED)
        );
    }

    #[test]
    fn the_picker_keeps_the_transport_button() {
        let painted = picker(0, None, Chrome::default(), Control::LoadSession);
        assert!(painted.side(PAUSE_SIDE).is_lit());
    }

    #[test]
    fn the_unbound_side_buttons_stay_dark() {
        use crate::led::SIDE_COUNT;

        let painted = frame(&SessionModel::new(), Chrome::default());
        let bound = [PAUSE_SIDE, MUTE_SIDE, SOLO_SIDE];
        assert!(
            (0..SIDE_COUNT)
                .filter(|i| !bound.contains(i))
                .all(|i| !painted.side(i).is_lit())
        );
    }
}
