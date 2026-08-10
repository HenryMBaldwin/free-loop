//! Turning session state into a frame.
//!
//! The one place the looper's colour scheme is decided. Pure, so the whole scheme is
//! testable without a device.

use free_loop_core::{SessionModel, SlotAddr, SlotState};

use crate::event::Control;
use crate::led::{BEAT_LEDS, FIRST_BEAT_LED, Led, LedColor, LedFrame};

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
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            beat: 0,
            beats_per_bar: 4,
            click_enabled: true,
            paused: false,
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
        Control::LoadSession | Control::SaveSession => Led::dim(LedColor::White),
    }
}

/// The right-hand column button that runs the transport.
pub const PAUSE_SIDE: usize = 4;

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

/// Paints the whole surface.
pub fn frame(session: &SessionModel, chrome: Chrome) -> LedFrame {
    let mut frame = LedFrame::new();

    for addr in SlotAddr::all() {
        frame.set_pad(addr, pad(session.state(addr)));
    }
    for button in Control::all() {
        frame.set_control(button.index(), control(button, chrome));
    }
    beat_indicator(&mut frame, chrome);
    frame.set_side(PAUSE_SIDE, pause_button(chrome));

    // The other side buttons are unbound, so they stay dark.
    frame
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use free_loop_core::{ClipId, Frames, SlotId, TrackId};

    use crate::led::LedStyle;

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
    fn the_unbound_side_buttons_stay_dark() {
        use crate::led::SIDE_COUNT;

        let painted = frame(&SessionModel::new(), Chrome::default());
        assert!(
            (0..SIDE_COUNT)
                .filter(|i| *i != PAUSE_SIDE)
                .all(|i| !painted.side(i).is_lit())
        );
    }
}
