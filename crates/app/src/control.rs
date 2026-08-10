//! The control loop's state.
//!
//! Sits between the surface and the engine: gestures in, commands out, reports in, a
//! frame out. Owns no I/O and takes the time as an argument rather than reading a clock,
//! so the mapping and the hold timing are both testable without a device.
//!
//! The session model here is a mirror. The engine owns the real one, because only it
//! knows where the transport is; this copy exists to paint the grid.

use core::time::Duration;

use free_loop_core::{
    Command, Event, MAX_BPM, MIN_BPM, SLOT_COUNT, SessionModel, SlotAddr, TRACK_COUNT, Tempo,
};
use free_loop_surface::{
    Chrome, Control, Led, LedColor, LedFrame, PAUSE_SIDE, SurfaceEvent, paint,
};

/// Beats per minute one press of the tempo buttons moves.
pub const TEMPO_STEP: f64 = 1.0;

/// How long a pad must be held to empty it.
pub const CLEAR_HOLD: Duration = Duration::from_secs(1);

/// How long into a hold the pad starts warning that it is about to empty.
pub const CLEAR_WARNING: Duration = Duration::from_millis(400);

/// Beats per minute a tempo button moves once it starts repeating.
pub const TEMPO_HOLD_STEP: f64 = 5.0;

/// How long a tempo button must be held before it starts repeating.
pub const TEMPO_HOLD_DELAY: Duration = Duration::from_millis(400);

/// How often a held tempo button repeats.
pub const TEMPO_HOLD_INTERVAL: Duration = Duration::from_millis(120);

/// How long the bpm stays up before the grid comes back.
///
/// The device scrolls text across the grid and says nothing when it finishes, so the
/// time it needs is waited out rather than detected.
pub const TEXT_DURATION: Duration = Duration::from_millis(1_400);

/// Bit for a pad in the grid masks.
fn bit(addr: SlotAddr) -> u64 {
    1 << (addr.track.index() * SLOT_COUNT + addr.slot.index())
}

/// What the grid is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The loops.
    Perform,
    /// Sessions, waiting for one to be chosen to save over.
    SavePicker,
    /// Sessions, waiting for one to be chosen to load.
    LoadPicker,
}

impl Mode {
    /// The button that opens a picker, if this mode is one.
    fn button(self) -> Option<Control> {
        match self {
            Self::Perform => None,
            Self::SavePicker => Some(Control::SaveSession),
            Self::LoadPicker => Some(Control::LoadSession),
        }
    }
}

/// A tempo button being held down.
#[derive(Debug, Clone, Copy)]
struct TempoHold {
    /// Which way it moves.
    delta: f64,
    /// When it went down.
    since: Duration,
    /// When it last moved.
    last: Duration,
    /// The tempo before it was pressed.
    started_at: f64,
}

/// Something for the surface to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextUpdate {
    /// Show this.
    Show(String),
    /// Stop showing anything.
    Stop,
}

/// Work for the caller to do, which the controller cannot because it owns no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Write the session under this pad.
    SaveSession(SlotAddr),
    /// Read the session under this pad.
    LoadSession(SlotAddr),
}

/// Turns gestures into commands and reports into a frame.
///
/// A pad's action lands on release rather than on press, because a press cannot be told
/// from the start of a hold. That costs the length of the tap in latency, which only
/// matters if the tap straddles the bar line it was aimed at. Acting on press instead
/// would let a hold arm and start recording before the clear killed it.
#[derive(Debug)]
pub struct Controller {
    session: SessionModel,
    chrome: Chrome,
    tempo: f64,
    /// Tempo to fall back to if the engine turns a change down.
    tempo_before_request: f64,
    /// When each held pad went down.
    held: [[Option<Duration>; SLOT_COUNT]; TRACK_COUNT],
    /// Pads currently warning that they are about to empty.
    warning: u64,
    commands: Vec<Command>,
    requests: Vec<Request>,
    /// A tempo button being held down.
    tempo_hold: Option<TempoHold>,
    /// A display change the caller has not picked up yet.
    text: Option<TextUpdate>,
    /// When the grid comes back, while text has it.
    text_until: Option<Duration>,
    mode: Mode,
    /// A bit per pad that holds a session.
    sessions: u64,
    /// The session in use, if one was loaded or saved this run.
    current: Option<SlotAddr>,
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
            paused: false,
        };
        let session = SessionModel::new();
        Self {
            frame: paint::frame(&session, chrome),
            session,
            chrome,
            tempo,
            tempo_before_request: tempo,
            held: [[None; SLOT_COUNT]; TRACK_COUNT],
            warning: 0,
            commands: Vec::new(),
            requests: Vec::new(),
            tempo_hold: None,
            text: None,
            text_until: None,
            mode: Mode::Perform,
            sessions: 0,
            current: None,
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

    /// Whether the transport is believed to be frozen.
    pub fn paused(&self) -> bool {
        self.chrome.paused
    }

    /// What the grid is showing.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The session in use.
    pub fn current_session(&self) -> Option<SlotAddr> {
        self.current
    }

    /// Tells the controller which pads hold sessions.
    pub fn set_sessions(&mut self, sessions: impl IntoIterator<Item = SlotAddr>) {
        self.sessions = sessions.into_iter().map(bit).fold(0, |mask, b| mask | b);
        self.dirty = true;
    }

    /// Opens a picker, or closes it if it was already open.
    fn set_mode(&mut self, wanted: Mode) {
        self.mode = if self.mode == wanted {
            Mode::Perform
        } else {
            wanted
        };
        self.dirty = true;
    }

    /// Records that a session was loaded, and leaves the picker.
    pub fn session_loaded(&mut self, addr: SlotAddr, paused: bool) {
        self.current = Some(addr);
        self.chrome.paused = paused;
        self.mode = Mode::Perform;
        self.dirty = true;
    }

    /// Leaves the picker without doing anything, after a request failed.
    pub fn cancel_picker(&mut self) {
        self.mode = Mode::Perform;
        self.dirty = true;
    }

    /// Records which session is in use, and leaves the picker.
    pub fn session_saved(&mut self, addr: SlotAddr) {
        self.sessions |= bit(addr);
        self.current = Some(addr);
        self.mode = Mode::Perform;
        self.dirty = true;
    }

    /// Takes a display change, if there is one.
    pub fn take_text(&mut self) -> Option<TextUpdate> {
        self.text.take()
    }

    /// Takes everything the caller needs to act on.
    pub fn drain_requests(&mut self) -> std::vec::Drain<'_, Request> {
        self.requests.drain(..)
    }

    /// Handles something the performer did, at time `now` since the app started.
    pub fn on_surface(&mut self, event: SurfaceEvent, now: Duration) {
        match event {
            SurfaceEvent::PadPressed { addr, .. } if self.mode == Mode::SavePicker => {
                self.requests.push(Request::SaveSession(addr));
            }
            SurfaceEvent::PadPressed { addr, .. } if self.mode == Mode::LoadPicker => {
                // Nothing to load from a pad that holds nothing.
                if self.sessions & bit(addr) != 0 {
                    self.requests.push(Request::LoadSession(addr));
                }
            }
            SurfaceEvent::PadPressed { addr, .. } => {
                // Nothing yet: which gesture this is depends on how long it lasts.
                self.held[addr.track.index()][addr.slot.index()] = Some(now);
            }
            SurfaceEvent::PadReleased { addr } => {
                // A hold that completed already emptied the pad and took its entry, so
                // only a release that still has one is a tap.
                if self.held[addr.track.index()][addr.slot.index()]
                    .take()
                    .is_some()
                {
                    self.commands.push(Command::Press(addr));
                }
            }
            SurfaceEvent::ControlPressed(Control::ClickToggle) => {
                self.chrome.click_enabled = !self.chrome.click_enabled;
                self.commands
                    .push(Command::SetClickEnabled(self.chrome.click_enabled));
                self.dirty = true;
            }
            SurfaceEvent::ControlPressed(Control::TempoDown) => self.press_tempo(-1.0, now),
            SurfaceEvent::ControlPressed(Control::TempoUp) => self.press_tempo(1.0, now),
            SurfaceEvent::ControlReleased(Control::TempoDown | Control::TempoUp) => {
                self.release_tempo(now);
            }
            SurfaceEvent::ControlPressed(Control::StopAll) => self.commands.push(Command::StopAll),
            SurfaceEvent::ControlPressed(Control::Rewind) => self.commands.push(Command::Rewind),
            SurfaceEvent::ControlPressed(Control::SaveSession) => {
                self.set_mode(Mode::SavePicker);
            }
            SurfaceEvent::ControlPressed(Control::LoadSession) => {
                self.set_mode(Mode::LoadPicker);
            }
            SurfaceEvent::SidePressed { index } if usize::from(index) == PAUSE_SIDE => {
                self.chrome.paused = !self.chrome.paused;
                self.commands.push(Command::SetPaused(self.chrome.paused));
                self.dirty = true;
            }

            // The other side buttons are unbound.
            SurfaceEvent::ControlReleased(_)
            | SurfaceEvent::SidePressed { .. }
            | SurfaceEvent::SideReleased { .. } => {}
        }
    }

    /// Advances anything that depends on time passing rather than on an event.
    ///
    /// Call every pass of the control loop, or a hold will only complete when something
    /// else happens to arrive.
    pub fn tick(&mut self, now: Duration) {
        self.repeat_tempo(now);

        if self.text_until.is_some_and(|until| now >= until) {
            self.text_until = None;
            self.text = Some(TextUpdate::Stop);
            self.dirty = true;
        }

        let mut warning = 0;

        for addr in SlotAddr::all() {
            let Some(since) = self.held[addr.track.index()][addr.slot.index()] else {
                continue;
            };
            let holding = now.saturating_sub(since);

            if holding >= CLEAR_HOLD {
                self.commands.push(Command::Clear(addr));
                // Forget the hold rather than wait for the release, so it fires once and
                // a pad still physically down does not empty again every pass.
                self.held[addr.track.index()][addr.slot.index()] = None;
            } else if holding >= CLEAR_WARNING {
                warning |= bit(addr);
            }
        }

        if warning != self.warning {
            self.warning = warning;
            self.dirty = true;
        }
    }

    /// Nudges once and arms the repeat, or just reports the tempo if it is locked.
    ///
    /// The tempo cannot move once a clip exists, so a press is a question rather than an
    /// instruction and gets answered with the number.
    fn press_tempo(&mut self, direction: f64, now: Duration) {
        if self.session.has_any_clip() {
            self.show_tempo(now);
            return;
        }

        let before = self.tempo;
        self.nudge_tempo(direction * TEMPO_STEP);
        self.tempo_hold = Some(TempoHold {
            delta: direction * TEMPO_HOLD_STEP,
            since: now,
            last: now,
            started_at: before,
        });
    }

    /// Whether a held tempo button has started repeating.
    fn tempo_repeating(&self) -> bool {
        self.tempo_hold.is_some_and(|hold| hold.last > hold.since)
    }

    /// Stops the repeat and reports where the tempo landed.
    ///
    /// Shown on release rather than as it moves, because each update restarts the scroll
    /// from the edge and one sent every repeat never gets anywhere. A tap that hit the
    /// end of the range says nothing, since the number would not have changed.
    fn release_tempo(&mut self, now: Duration) {
        let Some(hold) = self.tempo_hold.take() else {
            return;
        };
        // The gauge had the grid; whatever comes next has to redraw it.
        self.dirty = true;
        if (self.tempo - hold.started_at).abs() >= f64::EPSILON {
            self.show_tempo(now);
        }
    }

    /// Puts the tempo on the grid for long enough to read.
    fn show_tempo(&mut self, now: Duration) {
        let bpm = self.tempo.round();
        #[expect(clippy::cast_possible_truncation, reason = "tempo is under 300")]
        let shown = bpm as i32;

        self.text = Some(TextUpdate::Show(shown.to_string()));
        self.text_until = Some(now + TEXT_DURATION);
    }

    /// Moves the tempo again while a button stays down.
    fn repeat_tempo(&mut self, now: Duration) {
        let Some(hold) = self.tempo_hold else {
            return;
        };
        if now.saturating_sub(hold.since) < TEMPO_HOLD_DELAY
            || now.saturating_sub(hold.last) < TEMPO_HOLD_INTERVAL
        {
            return;
        }

        self.nudge_tempo(hold.delta);
        self.tempo_hold = Some(TempoHold { last: now, ..hold });
        self.dirty = true;
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
                // A number that has just been rolled back is worse than none.
                if self.text_until.take().is_some() {
                    self.text = Some(TextUpdate::Stop);
                    self.dirty = true;
                }
            }
            // Bars are already covered by the beat they start with, and the rest are for
            // logging rather than for the grid.
            Event::Bar { .. }
            | Event::ClipRecorded { .. }
            | Event::ClipReleased { .. }
            | Event::RecordBufferLow { .. }
            | Event::Xrun { .. }
            | Event::SnapshotComplete { .. }
            // The clock goes straight to the surface rather than through the grid.
            | Event::Clock { .. } => {}
        }
    }

    /// Takes everything queued for the engine.
    pub fn drain_commands(&mut self) -> std::vec::Drain<'_, Command> {
        self.commands.drain(..)
    }

    /// The frame to show, if anything changed since it was last taken.
    pub fn take_frame(&mut self) -> Option<&LedFrame> {
        // Text has the grid, and a frame sent now would cut it off part way.
        if !self.dirty || self.text_until.is_some() {
            return None;
        }

        // A number cannot track a tempo that is still moving, so the grid shows it
        // instead until the button is let go.
        if self.tempo_repeating() {
            self.frame = paint::tempo_gauge(self.tempo, self.chrome);
            self.dirty = false;
            return Some(&self.frame);
        }

        if let Some(button) = self.mode.button() {
            self.frame = paint::picker(self.sessions, self.current, self.chrome, button);
            self.dirty = false;
            return Some(&self.frame);
        }

        self.frame = paint::frame(&self.session, self.chrome);
        // Painted over the session colour so a hold about to empty a pad says so before
        // the audio disappears.
        if self.warning != 0 {
            for addr in SlotAddr::all() {
                if self.warning & bit(addr) != 0 {
                    self.frame.set_pad(addr, Led::flash(LedColor::White));
                }
            }
        }

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
    use free_loop_core::{ClipId, Frames, SlotId, SlotState, TrackId};

    const T0: Duration = Duration::ZERO;

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    fn controller() -> Controller {
        Controller::new(120.0, 4, true)
    }

    fn commands(controller: &mut Controller) -> Vec<Command> {
        controller.drain_commands().collect()
    }

    fn millis(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn press(controller: &mut Controller, pad: SlotAddr, at: Duration) {
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: pad,
                velocity: 100,
            },
            at,
        );
    }

    #[test]
    fn a_tap_acts_on_release() {
        let mut controller = controller();
        let pad = addr(2, 3);

        press(&mut controller, pad, T0);
        assert!(
            commands(&mut controller).is_empty(),
            "a press could still turn into a hold"
        );

        controller.on_surface(SurfaceEvent::PadReleased { addr: pad }, millis(80));
        assert_eq!(commands(&mut controller), vec![Command::Press(pad)]);
    }

    #[test]
    fn the_rewind_button_sends_the_transport_back() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::Rewind), T0);
        assert_eq!(commands(&mut controller), vec![Command::Rewind]);
    }

    #[test]
    fn the_unbound_side_buttons_do_nothing() {
        let mut controller = controller();
        for index in 0..8 {
            if usize::from(index) == PAUSE_SIDE {
                continue;
            }
            controller.on_surface(SurfaceEvent::SidePressed { index }, T0);
        }
        controller.on_surface(SurfaceEvent::ControlReleased(Control::StopAll), T0);
        assert!(commands(&mut controller).is_empty());
    }

    #[test]
    fn the_transport_button_toggles_the_freeze() {
        let mut controller = controller();
        let button = u8::try_from(PAUSE_SIDE).unwrap();
        assert!(!controller.paused());

        controller.on_surface(SurfaceEvent::SidePressed { index: button }, T0);
        assert!(controller.paused());
        assert_eq!(commands(&mut controller), vec![Command::SetPaused(true)]);

        controller.on_surface(SurfaceEvent::SidePressed { index: button }, T0);
        assert!(!controller.paused());
        assert_eq!(commands(&mut controller), vec![Command::SetPaused(false)]);
    }

    #[test]
    fn the_transport_button_shows_the_freeze() {
        let mut controller = controller();
        controller.take_frame();
        let running = controller.frame.side(PAUSE_SIDE);

        controller.on_surface(
            SurfaceEvent::SidePressed {
                index: u8::try_from(PAUSE_SIDE).unwrap(),
            },
            T0,
        );
        let frozen = controller.take_frame().expect("the button changed");
        assert_ne!(frozen.side(PAUSE_SIDE), running);
    }

    fn requests(controller: &mut Controller) -> Vec<Request> {
        controller.drain_requests().collect()
    }

    #[test]
    fn the_save_button_toggles_the_picker() {
        let mut controller = controller();
        assert_eq!(controller.mode(), Mode::Perform);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        assert_eq!(controller.mode(), Mode::SavePicker);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        assert_eq!(controller.mode(), Mode::Perform, "pressing again backs out");
    }

    #[test]
    fn a_pad_in_the_picker_asks_for_a_save_rather_than_playing() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        press(&mut controller, addr(2, 3), T0);

        assert!(
            commands(&mut controller).is_empty(),
            "a pad in the picker must not touch the loops"
        );
        assert_eq!(
            requests(&mut controller),
            vec![Request::SaveSession(addr(2, 3))]
        );
    }

    #[test]
    fn a_picker_press_cannot_start_a_hold() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        press(&mut controller, addr(0, 0), T0);
        requests(&mut controller);

        controller.tick(millis(2_000));
        assert!(
            commands(&mut controller).is_empty(),
            "choosing where to save must not empty a pad"
        );
    }

    #[test]
    fn the_load_button_opens_its_own_picker() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        assert_eq!(controller.mode(), Mode::LoadPicker);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        assert_eq!(controller.mode(), Mode::Perform);
    }

    #[test]
    fn one_picker_replaces_the_other() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        assert_eq!(controller.mode(), Mode::LoadPicker);
    }

    #[test]
    fn loading_an_empty_pad_asks_for_nothing() {
        let mut controller = controller();
        controller.set_sessions([addr(1, 1)]);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);

        press(&mut controller, addr(0, 0), T0);
        assert!(
            requests(&mut controller).is_empty(),
            "nothing is saved there"
        );

        press(&mut controller, addr(1, 1), T0);
        assert_eq!(
            requests(&mut controller),
            vec![Request::LoadSession(addr(1, 1))]
        );
    }

    #[test]
    fn a_completed_load_leaves_the_picker_frozen() {
        let mut controller = controller();
        controller.set_sessions([addr(2, 2)]);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.session_loaded(addr(2, 2), true);

        assert_eq!(controller.mode(), Mode::Perform);
        assert_eq!(controller.current_session(), Some(addr(2, 2)));
        assert!(controller.paused(), "a loaded session waits to be started");
    }

    #[test]
    fn a_failed_request_still_leaves_the_picker() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.cancel_picker();
        assert_eq!(controller.mode(), Mode::Perform);
    }

    #[test]
    fn a_completed_save_leaves_the_picker_and_marks_the_session() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        controller.session_saved(addr(1, 1));

        assert_eq!(controller.mode(), Mode::Perform);
        assert_eq!(controller.current_session(), Some(addr(1, 1)));
    }

    #[test]
    fn the_picker_shows_the_sessions_it_was_told_about() {
        let mut controller = controller();
        controller.set_sessions([addr(0, 1), addr(3, 4)]);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);

        let frame = controller.take_frame().expect("the picker opened");
        assert!(frame.pad(addr(0, 1)).is_lit());
        assert!(frame.pad(addr(3, 4)).is_lit());
        assert!(!frame.pad(addr(7, 7)).is_lit());
    }

    #[test]
    fn the_picker_hides_the_loops() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Playing { clip: ClipId(0) },
        });
        controller.take_frame();

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        let frame = controller.take_frame().expect("the picker opened");
        assert!(
            !frame.pad(addr(0, 0)).is_lit(),
            "an empty session pad must not look like a playing loop"
        );
    }

    #[test]
    fn commands_are_taken_once() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::StopAll), T0);
        assert_eq!(commands(&mut controller), vec![Command::StopAll]);
        assert!(commands(&mut controller).is_empty());
    }

    #[test]
    fn the_click_toggle_tracks_its_own_state() {
        let mut controller = controller();
        assert!(controller.click_enabled());

        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        assert!(!controller.click_enabled());
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickEnabled(false)]
        );

        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        assert!(controller.click_enabled());
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickEnabled(true)]
        );
    }

    #[test]
    fn tempo_moves_by_one_beat_per_press() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        assert_eq!(controller.tempo(), 121.0);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        assert_eq!(controller.tempo(), 119.0);
        assert_eq!(commands(&mut controller).len(), 3);
    }

    #[test]
    fn a_tap_moves_one_beat_and_shows_it() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(80));

        assert_eq!(controller.tempo(), 121.0);
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("121".to_owned()))
        );
    }

    #[test]
    fn a_locked_tempo_is_reported_rather_than_changed() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Stopped { clip: ClipId(0) },
        });

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);

        assert_eq!(controller.tempo(), 120.0, "locked, so it does not move");
        assert!(
            commands(&mut controller).is_empty(),
            "asking the engine to do what it will refuse is noise"
        );
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("120".to_owned()))
        );
    }

    #[test]
    fn a_locked_tempo_does_not_start_a_repeat() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Stopped { clip: ClipId(0) },
        });

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        for at in (0..2_000).step_by(20) {
            controller.tick(millis(at));
        }
        assert_eq!(controller.tempo(), 120.0);
    }

    #[test]
    fn a_tap_that_moves_nothing_says_nothing() {
        let mut controller = Controller::new(MAX_BPM, 4, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(80));
        assert_eq!(controller.take_text(), None);
    }

    #[test]
    fn a_rolled_back_tempo_takes_its_number_down_with_it() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(80));
        controller.take_text();

        controller.on_engine(Event::TempoRejected);
        assert_eq!(controller.tempo(), 120.0);
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Stop),
            "the number on the grid is no longer the tempo"
        );
    }

    #[test]
    fn a_hold_shows_the_tempo_climbing() {
        let mut controller = controller();
        controller.take_frame();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);

        controller.tick(millis(400));
        let lit_at_first = {
            let frame = controller.take_frame().expect("the gauge appeared");
            SlotAddr::all().filter(|a| frame.pad(*a).is_lit()).count()
        };

        for at in (520..1_200).step_by(120) {
            controller.tick(millis(at));
        }
        let frame = controller.take_frame().expect("the gauge moved");
        let lit_later = SlotAddr::all().filter(|a| frame.pad(*a).is_lit()).count();

        assert!(
            lit_later > lit_at_first,
            "the fill should show the tempo climbing"
        );
    }

    #[test]
    fn the_grid_comes_back_when_the_hold_ends() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Playing { clip: ClipId(0) },
        });
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.tick(millis(400));
        controller.take_frame();

        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(500));
        controller.take_text();
        controller.tick(millis(500) + TEXT_DURATION);
        controller.take_text();

        let frame = controller.take_frame().expect("the loops are back");
        assert_eq!(frame.pad(addr(0, 0)), Led::pulse(LedColor::Green));
    }

    #[test]
    fn a_short_hold_does_not_start_repeating() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        for at in (0..400).step_by(20) {
            controller.tick(millis(at));
        }
        assert_eq!(controller.tempo(), 121.0, "just the tap");
    }

    #[test]
    fn a_held_button_repeats_in_fives() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);

        controller.tick(millis(400));
        assert_eq!(controller.tempo(), 126.0, "the tap plus one repeat");

        controller.tick(millis(520));
        assert_eq!(controller.tempo(), 131.0);
    }

    #[test]
    fn a_held_button_repeats_downwards_too() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        controller.tick(millis(400));
        assert_eq!(controller.tempo(), 114.0);
    }

    #[test]
    fn a_repeat_says_nothing_until_the_button_is_let_go() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);

        for at in (400..1_000).step_by(20) {
            controller.tick(millis(at));
            assert_eq!(
                controller.take_text(),
                None,
                "an update every repeat restarts the scroll and it never finishes"
            );
        }

        controller.on_surface(
            SurfaceEvent::ControlReleased(Control::TempoUp),
            millis(1_000),
        );
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show(controller.tempo().to_string()))
        );
    }

    #[test]
    fn the_grid_comes_back_after_the_bpm_has_been_read() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.tick(millis(400));
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(500));
        controller.take_text();

        controller.tick(millis(600));
        assert!(
            controller.take_frame().is_none(),
            "a frame now would cut the number off part way"
        );

        controller.tick(millis(500) + TEXT_DURATION);
        assert_eq!(controller.take_text(), Some(TextUpdate::Stop));
        assert!(controller.take_frame().is_some(), "and the grid comes back");
    }

    #[test]
    fn releasing_stops_the_repeat() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.tick(millis(400));
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(450));

        let settled = controller.tempo();
        controller.tick(millis(2_000));
        assert_eq!(controller.tempo(), settled);
    }

    #[test]
    fn a_repeat_stops_at_the_supported_range() {
        let mut controller = Controller::new(MAX_BPM - 2.0, 4, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        for at in (0..2_000).step_by(20) {
            controller.tick(millis(at));
        }
        assert_eq!(controller.tempo(), MAX_BPM);
    }

    #[test]
    fn tempo_stops_at_the_supported_range() {
        let mut controller = Controller::new(MAX_BPM, 4, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        assert_eq!(controller.tempo(), MAX_BPM);
        assert!(
            commands(&mut controller).is_empty(),
            "a change that moves nothing should not be sent"
        );

        let mut controller = Controller::new(MIN_BPM, 4, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        assert_eq!(controller.tempo(), MIN_BPM);
    }

    #[test]
    fn a_refused_tempo_change_is_rolled_back() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
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
        assert_eq!(
            frame.control(FIRST_BEAT_LED + 2),
            Led::solid(LedColor::Blue),
            "the current beat"
        );
        // Beat one shares the tempo up button, which keeps its own colour meanwhile.
        assert_ne!(frame.control(FIRST_BEAT_LED), Led::solid(LedColor::White));
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

    #[test]
    fn holding_a_pad_empties_it_without_acting_first() {
        let mut controller = controller();
        let pad = addr(1, 1);

        press(&mut controller, pad, T0);
        controller.tick(millis(999));
        assert!(commands(&mut controller).is_empty(), "not held long enough");

        controller.tick(millis(1_000));
        assert_eq!(
            commands(&mut controller),
            vec![Command::Clear(pad)],
            "a hold must not arm or launch on its way to emptying the pad"
        );
    }

    #[test]
    fn releasing_after_a_hold_completes_does_nothing_more() {
        let mut controller = controller();
        let pad = addr(1, 1);

        press(&mut controller, pad, T0);
        controller.tick(millis(1_000));
        assert_eq!(commands(&mut controller), vec![Command::Clear(pad)]);

        controller.tick(millis(1_500));
        controller.on_surface(SurfaceEvent::PadReleased { addr: pad }, millis(2_000));
        assert!(
            commands(&mut controller).is_empty(),
            "the hold was already spent"
        );
    }

    #[test]
    fn releasing_early_acts_as_a_press_and_leaves_the_clip_alone() {
        let mut controller = controller();
        let pad = addr(1, 1);

        press(&mut controller, pad, T0);
        controller.tick(millis(500));
        controller.on_surface(SurfaceEvent::PadReleased { addr: pad }, millis(600));
        assert_eq!(commands(&mut controller), vec![Command::Press(pad)]);

        controller.tick(millis(2_000));
        assert!(
            commands(&mut controller).is_empty(),
            "the hold was abandoned"
        );
    }

    #[test]
    fn a_hold_warns_before_it_empties() {
        let mut controller = controller();
        let pad = addr(4, 6);
        controller.on_engine(Event::SlotChanged {
            addr: pad,
            state: SlotState::Playing { clip: ClipId(0) },
        });
        controller.take_frame();

        press(&mut controller, pad, T0);
        controller.tick(millis(399));
        assert!(controller.take_frame().is_none(), "too early to warn");

        controller.tick(millis(400));
        let frame = controller.take_frame().expect("the warning appeared");
        assert_eq!(frame.pad(pad), Led::flash(LedColor::White));
        assert_ne!(frame.pad(addr(4, 5)), Led::flash(LedColor::White));
    }

    #[test]
    fn the_warning_clears_when_the_hold_does() {
        let mut controller = controller();
        let pad = addr(0, 0);

        press(&mut controller, pad, T0);
        controller.tick(millis(500));
        controller.take_frame();

        controller.on_surface(SurfaceEvent::PadReleased { addr: pad }, millis(600));
        controller.tick(millis(600));
        let frame = controller.take_frame().expect("the warning went away");
        assert_ne!(frame.pad(pad), Led::flash(LedColor::White));
    }

    #[test]
    fn holds_are_tracked_per_pad() {
        let mut controller = controller();
        let held = addr(2, 2);
        let tapped = addr(3, 3);

        press(&mut controller, held, T0);
        press(&mut controller, tapped, millis(100));
        controller.on_surface(SurfaceEvent::PadReleased { addr: tapped }, millis(200));
        assert_eq!(commands(&mut controller), vec![Command::Press(tapped)]);

        controller.tick(millis(1_000));
        assert_eq!(commands(&mut controller), vec![Command::Clear(held)]);
    }
}
