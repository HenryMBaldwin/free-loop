//! The control loop's state.
//!
//! Sits between the surface and the engine: gestures in, commands out, reports in, a
//! frame out. Owns no I/O, so the whole mapping is testable without a device.
//!
//! The session model here is a mirror. The engine owns the real one, because only it
//! knows where the transport is; this copy exists to paint the grid.

use free_loop_core::{Command, Event, MAX_BPM, MIN_BPM, SessionModel, Tempo};
use free_loop_surface::{Chrome, Control, LedFrame, SurfaceEvent, paint};

/// Beats per minute one press of the tempo buttons moves.
pub const TEMPO_STEP: f64 = 1.0;

/// Turns gestures into commands and reports into a frame.
#[derive(Debug)]
pub struct Controller {
    session: SessionModel,
    chrome: Chrome,
    tempo: f64,
    /// Tempo to fall back to if the engine turns a change down.
    tempo_before_request: f64,
    commands: Vec<Command>,
    frame: LedFrame,
    dirty: bool,
}

impl Controller {
    /// A controller for an empty session.
    pub fn new(tempo: f64, beats_per_bar: u32, click_enabled: bool) -> Self {
        let chrome = Chrome {
            beat: 0,
            beats_per_bar,
            click_enabled,
        };
        let session = SessionModel::new();
        Self {
            frame: paint::frame(&session, chrome),
            session,
            chrome,
            tempo,
            tempo_before_request: tempo,
            commands: Vec::new(),
            dirty: true,
        }
    }

    /// The tempo the engine is believed to be running at.
    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    /// The mirrored session.
    pub fn session(&self) -> &SessionModel {
        &self.session
    }

    /// Whether the click is believed to be sounding.
    pub fn click_enabled(&self) -> bool {
        self.chrome.click_enabled
    }

    /// Handles something the performer did.
    pub fn on_surface(&mut self, event: SurfaceEvent) {
        match event {
            SurfaceEvent::PadPressed { addr, .. } => self.commands.push(Command::Press(addr)),
            SurfaceEvent::ControlPressed(Control::ClickToggle) => {
                self.chrome.click_enabled = !self.chrome.click_enabled;
                self.commands
                    .push(Command::SetClickEnabled(self.chrome.click_enabled));
                self.dirty = true;
            }
            SurfaceEvent::ControlPressed(Control::TempoDown) => self.nudge_tempo(-TEMPO_STEP),
            SurfaceEvent::ControlPressed(Control::TempoUp) => self.nudge_tempo(TEMPO_STEP),
            SurfaceEvent::ControlPressed(Control::StopAll) => self.commands.push(Command::StopAll),

            // Releases carry nothing yet, and the scene column is reserved.
            SurfaceEvent::PadReleased { .. }
            | SurfaceEvent::ControlReleased(_)
            | SurfaceEvent::ScenePressed { .. }
            | SurfaceEvent::SceneReleased { .. } => {}
        }
    }

    fn nudge_tempo(&mut self, delta: f64) {
        let wanted = (self.tempo + delta).clamp(MIN_BPM, MAX_BPM);
        let Ok(tempo) = Tempo::new(wanted) else {
            return;
        };
        // Already against the end of the range, so there is nothing to ask for.
        if (wanted - self.tempo).abs() < f64::EPSILON {
            return;
        }

        // Assume it lands; the engine only speaks up when it does not.
        self.tempo_before_request = self.tempo;
        self.tempo = wanted;
        self.commands.push(Command::SetTempo(tempo));
    }

    /// Handles something the engine reported.
    pub fn on_engine(&mut self, event: Event) {
        match event {
            Event::SlotChanged { addr, state } => {
                self.session.mirror(addr, state);
                self.dirty = true;
            }
            Event::Beat { beat, .. } => {
                if beat != self.chrome.beat {
                    self.chrome.beat = beat;
                    self.dirty = true;
                }
            }
            Event::TempoRejected => {
                self.tempo = self.tempo_before_request;
            }
            // Bars are already covered by the beat they start with, and the rest are for
            // logging rather than for the grid.
            Event::Bar { .. }
            | Event::ClipRecorded { .. }
            | Event::ClipReleased { .. }
            | Event::RecordBufferLow { .. }
            | Event::Xrun { .. } => {}
        }
    }

    /// Takes everything queued for the engine.
    pub fn drain_commands(&mut self) -> std::vec::Drain<'_, Command> {
        self.commands.drain(..)
    }

    /// The frame to show, if anything changed since it was last taken.
    pub fn take_frame(&mut self) -> Option<&LedFrame> {
        if !self.dirty {
            return None;
        }
        self.frame = paint::frame(&self.session, self.chrome);
        self.dirty = false;
        Some(&self.frame)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp,
        reason = "tests should fail loudly, and compare exact configured values"
    )]

    use super::*;
    use free_loop_core::{ClipId, Frames, SlotAddr, SlotId, SlotState, TrackId};
    use free_loop_surface::{Led, LedColor};

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    fn controller() -> Controller {
        Controller::new(120.0, 4, true)
    }

    fn commands(controller: &mut Controller) -> Vec<Command> {
        controller.drain_commands().collect()
    }

    #[test]
    fn a_pad_press_becomes_a_press_command() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::PadPressed {
            addr: addr(2, 3),
            velocity: 100,
        });
        assert_eq!(commands(&mut controller), vec![Command::Press(addr(2, 3))]);
    }

    #[test]
    fn releases_and_scenes_do_nothing_yet() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::PadReleased { addr: addr(0, 0) });
        controller.on_surface(SurfaceEvent::ScenePressed {
            slot: SlotId::new(0).unwrap(),
        });
        controller.on_surface(SurfaceEvent::ControlReleased(Control::StopAll));
        assert!(commands(&mut controller).is_empty());
    }

    #[test]
    fn commands_are_taken_once() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::StopAll));
        assert_eq!(commands(&mut controller), vec![Command::StopAll]);
        assert!(commands(&mut controller).is_empty());
    }

    #[test]
    fn the_click_toggle_tracks_its_own_state() {
        let mut controller = controller();
        assert!(controller.click_enabled());

        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle));
        assert!(!controller.click_enabled());
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickEnabled(false)]
        );

        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle));
        assert!(controller.click_enabled());
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickEnabled(true)]
        );
    }

    #[test]
    fn tempo_moves_by_one_beat_per_press() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp));
        assert_eq!(controller.tempo(), 121.0);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown));
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown));
        assert_eq!(controller.tempo(), 119.0);
        assert_eq!(commands(&mut controller).len(), 3);
    }

    #[test]
    fn tempo_stops_at_the_supported_range() {
        let mut controller = Controller::new(MAX_BPM, 4, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp));
        assert_eq!(controller.tempo(), MAX_BPM);
        assert!(
            commands(&mut controller).is_empty(),
            "a change that moves nothing should not be sent"
        );

        let mut controller = Controller::new(MIN_BPM, 4, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown));
        assert_eq!(controller.tempo(), MIN_BPM);
    }

    #[test]
    fn a_refused_tempo_change_is_rolled_back() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp));
        assert_eq!(controller.tempo(), 121.0, "assumed to land");

        controller.on_engine(Event::TempoRejected);
        assert_eq!(
            controller.tempo(),
            120.0,
            "and rolled back when it does not"
        );
    }

    #[test]
    fn slot_reports_reach_the_grid() {
        let mut controller = controller();
        controller.take_frame();

        controller.on_engine(Event::SlotChanged {
            addr: addr(1, 2),
            state: SlotState::Playing { clip: ClipId(0) },
        });

        let frame = controller.take_frame().expect("the grid changed");
        assert_eq!(frame.pad(addr(1, 2)), Led::pulse(LedColor::Green));
    }

    #[test]
    fn the_frame_is_only_produced_when_something_changed() {
        let mut controller = controller();
        assert!(controller.take_frame().is_some(), "the first frame is new");
        assert!(controller.take_frame().is_none());

        controller.on_engine(Event::Xrun { frames: 128 });
        assert!(
            controller.take_frame().is_none(),
            "an xrun changes nothing on the grid"
        );

        controller.on_engine(Event::Beat { bar: 0, beat: 1 });
        assert!(controller.take_frame().is_some());
    }

    #[test]
    fn repeating_the_same_beat_does_not_force_a_repaint() {
        let mut controller = controller();
        controller.take_frame();
        controller.on_engine(Event::Beat { bar: 0, beat: 0 });
        assert!(controller.take_frame().is_none());
    }

    #[test]
    fn the_beat_indicator_follows_the_transport() {
        use free_loop_surface::FIRST_BEAT_LED;

        let mut controller = controller();
        controller.on_engine(Event::Beat { bar: 3, beat: 2 });

        let frame = controller.take_frame().unwrap();
        assert!(frame.control(FIRST_BEAT_LED + 2).is_lit());
        assert!(!frame.control(FIRST_BEAT_LED).is_lit());
    }

    #[test]
    fn a_recording_pad_shows_red_before_any_clip_exists() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Recording {
                started_at: Frames(0),
                ends_at: None,
            },
        });

        let frame = controller.take_frame().unwrap();
        assert_eq!(frame.pad(addr(0, 0)), Led::solid(LedColor::Red));
    }
}
