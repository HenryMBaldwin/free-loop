//! The control loop's state.
//!
//! Sits between the surface and the engine: gestures in, commands out, reports in, a
//! frame out. Owns no I/O and takes the time as an argument rather than reading a clock.
//!
//! The session model here is a mirror of the engine's, kept to paint the grid.

use core::time::Duration;

use free_loop_core::{
    Command, Event, LaunchMode, MAX_BPM, MIN_BPM, SLOT_COUNT, SessionModel, SlotAddr, TRACK_COUNT,
    Tempo, TrackInput, UNITY_STEP, column_mask, pad_bit, row_mask,
};
use free_loop_surface::{
    Axis, Chrome, Control, INPUT_SIDE, Led, LedColor, LedFrame, MUTE_SIDE, NEW_SIDE, PAUSE_SIDE,
    RESTART_COLUMN, SELECTED, SETTINGS_SIDE, SOLO_SIDE, SurfaceEvent, VOLUME_SIDE, paint,
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
/// The device says nothing when its scroll finishes, so the time is waited out.
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
    /// Which pads are silenced.
    Mute,
    /// Which pads are soloed.
    Solo,
    /// How loud each track plays.
    Volume,
    /// Which input each track records.
    Input,
    /// One row of settings per track.
    Settings,
}

impl Mode {
    /// The button that opens a picker, if this mode is one.
    fn button(self) -> Option<Control> {
        match self {
            Self::SavePicker => Some(Control::SaveSession),
            Self::LoadPicker => Some(Control::LoadSession),
            // Mute and solo open from the side column, not the top row.
            Self::Perform
            | Self::Mute
            | Self::Solo
            | Self::Volume
            | Self::Input
            | Self::Settings => None,
        }
    }
}

/// A tempo button being held down.
#[derive(Debug, Clone, Copy)]
struct TempoHold {
    /// The button being held, so it can show that it is.
    button: Control,
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
/// from the start of a hold.
#[derive(Debug)]
pub struct Controller {
    session: SessionModel,
    chrome: Chrome,
    tempo: f64,
    /// What the config asked for, which a fresh session goes back to.
    default_tempo: f64,
    /// The input a fresh session puts every track on.
    default_input: TrackInput,
    /// The launch mode a fresh session puts every track on.
    default_launch_mode: LaunchMode,
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
    /// Whether text is on the grid now, as opposed to merely queued.
    text_running: bool,
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
            inputs: [TrackInput::Stereo; TRACK_COUNT],
            launch_modes: [LaunchMode::Follow; TRACK_COUNT],
            input_count: 2,
            beat: 0,
            beats_per_bar,
            click_enabled,
            paused: false,
            axis: Axis::Row,
            muted: 0,
            soloed: 0,
            gains: [UNITY_STEP; TRACK_COUNT],
        };
        let session = SessionModel::new();
        Self {
            frame: paint::frame(&session, chrome),
            session,
            chrome,
            tempo,
            default_tempo: tempo,
            default_input: TrackInput::Stereo,
            default_launch_mode: LaunchMode::Follow,
            tempo_before_request: tempo,
            held: [[None; SLOT_COUNT]; TRACK_COUNT],
            warning: 0,
            commands: Vec::new(),
            requests: Vec::new(),
            tempo_hold: None,
            text: None,
            text_until: None,
            text_running: false,
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

    /// Freezes the transport without a press, for something the performer did not ask for.
    pub fn pause(&mut self) {
        if self.chrome.paused {
            return;
        }
        self.chrome.paused = true;
        self.commands.push(Command::SetPaused(true));
        self.dirty = true;
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

    /// The level each track plays at.
    pub fn gains(&self) -> [u8; TRACK_COUNT] {
        self.chrome.gains
    }

    /// Sets a track's level, taken from the column pressed.
    fn set_level(&mut self, addr: SlotAddr) {
        let step = u8::try_from(addr.slot.index()).unwrap_or(UNITY_STEP);
        self.chrome.gains[addr.track.index()] = step;
        self.commands.push(Command::SetGains(self.chrome.gains));
        self.dirty = true;
    }

    /// Sets which input the pad's track records, if the device offers it.
    fn set_input(&mut self, addr: SlotAddr) {
        let column = addr.slot.index();
        if column > self.chrome.input_count {
            return;
        }
        self.chrome.inputs[addr.track.index()] = TrackInput::from_column(column);
        self.commands.push(Command::SetInputs(self.chrome.inputs));
        self.dirty = true;
    }

    /// Flips the setting the pad's column stands for.
    fn toggle_setting(&mut self, addr: SlotAddr) {
        if addr.slot.index() != RESTART_COLUMN {
            return;
        }
        let track = addr.track.index();
        self.chrome.launch_modes[track] = self.chrome.launch_modes[track].toggled();
        self.commands
            .push(Command::SetLaunchModes(self.chrome.launch_modes));
        self.dirty = true;
    }

    /// Where each track's clips are anchored when launched.
    pub fn launch_modes(&self) -> [LaunchMode; TRACK_COUNT] {
        self.chrome.launch_modes
    }

    /// Takes the modes a loaded session came with.
    pub fn set_launch_modes(&mut self, modes: [LaunchMode; TRACK_COUNT]) {
        self.chrome.launch_modes = modes;
        self.commands.push(Command::SetLaunchModes(modes));
        self.dirty = true;
    }

    /// Tells the grid how many inputs the device offers.
    pub fn set_input_count(&mut self, count: usize) {
        self.chrome.input_count = count;
        self.dirty = true;
    }

    /// Which input each track records.
    pub fn inputs(&self) -> [TrackInput; TRACK_COUNT] {
        self.chrome.inputs
    }

    /// The input a fresh session starts every track on.
    pub fn set_default_input(&mut self, input: TrackInput) {
        self.default_input = input;
    }

    /// The launch mode a fresh session starts every track on.
    pub fn set_default_launch_mode(&mut self, mode: LaunchMode) {
        self.default_launch_mode = mode;
    }

    /// Takes the inputs a loaded session came with.
    pub fn set_inputs(&mut self, inputs: [TrackInput; TRACK_COUNT]) {
        self.chrome.inputs = inputs;
        self.commands.push(Command::SetInputs(inputs));
        self.dirty = true;
    }

    /// Takes the levels a loaded session came with.
    pub fn set_gains(&mut self, gains: [u8; TRACK_COUNT]) {
        self.chrome.gains = gains;
        self.commands.push(Command::SetGains(gains));
        self.dirty = true;
    }

    /// Silences or frees the row or column a pad sits in.
    fn toggle_group(&mut self, addr: SlotAddr) {
        let group = match self.chrome.axis {
            Axis::Row => row_mask(addr.track),
            Axis::Column => column_mask(addr.slot),
        };

        let marks = if self.mode == Mode::Solo {
            &mut self.chrome.soloed
        } else {
            &mut self.chrome.muted
        };
        // The pad pressed decides, so a part-set group turns fully on rather than
        // toggling each pad separately.
        if *marks & pad_bit(addr) == 0 {
            *marks |= group;
        } else {
            *marks &= !group;
        }

        self.commands.push(Command::SetMutes {
            muted: self.chrome.muted,
            soloed: self.chrome.soloed,
        });
        self.dirty = true;
    }

    /// Empties everything and leaves the picker on a session with no name.
    fn start_fresh(&mut self) {
        self.session = SessionModel::new();
        self.chrome.muted = 0;
        self.chrome.soloed = 0;
        self.chrome.paused = false;
        self.current = None;
        self.mode = Mode::Perform;

        self.chrome.gains = [UNITY_STEP; TRACK_COUNT];
        self.commands.push(Command::SetGains(self.chrome.gains));
        self.chrome.inputs = [self.default_input; TRACK_COUNT];
        self.commands.push(Command::SetInputs(self.chrome.inputs));
        self.chrome.launch_modes = [self.default_launch_mode; TRACK_COUNT];
        self.commands
            .push(Command::SetLaunchModes(self.chrome.launch_modes));
        self.commands.push(Command::ClearAll);
        self.commands.push(Command::SetMutes {
            muted: 0,
            soloed: 0,
        });
        self.commands.push(Command::SetPaused(false));

        // After the clear, so the engine is no longer holding clips to protect and takes
        // the change rather than refusing it.
        self.tempo = self.default_tempo;
        self.tempo_before_request = self.default_tempo;
        if let Ok(tempo) = Tempo::new(self.default_tempo) {
            self.commands.push(Command::SetTempo(tempo));
        }
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
    /// Takes the tempo a loaded session came with.
    ///
    /// The engine took it from the load itself, so this only brings the display in step.
    pub fn set_loaded_tempo(&mut self, bpm: f64) {
        self.tempo = bpm;
        self.tempo_before_request = bpm;
        self.dirty = true;
    }

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
        let update = self.text.take()?;
        self.text_running = matches!(update, TextUpdate::Show(_));
        Some(update)
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
            SurfaceEvent::PadPressed { addr, .. }
                if matches!(self.mode, Mode::Mute | Mode::Solo) =>
            {
                self.toggle_group(addr);
            }
            SurfaceEvent::PadPressed { addr, .. } if self.mode == Mode::Volume => {
                self.set_level(addr);
            }
            SurfaceEvent::PadPressed { addr, .. } if self.mode == Mode::Input => {
                self.set_input(addr);
            }
            SurfaceEvent::PadPressed { addr, .. } if self.mode == Mode::Settings => {
                self.toggle_setting(addr);
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
            SurfaceEvent::ControlPressed(Control::Axis) => {
                self.chrome.axis = self.chrome.axis.flipped();
                self.dirty = true;
            }
            SurfaceEvent::SidePressed { index }
                if usize::from(index) == NEW_SIDE && self.mode == Mode::LoadPicker =>
            {
                self.start_fresh();
            }
            SurfaceEvent::SidePressed { index } if usize::from(index) == VOLUME_SIDE => {
                self.set_mode(Mode::Volume);
            }
            SurfaceEvent::SidePressed { index } if usize::from(index) == INPUT_SIDE => {
                self.set_mode(Mode::Input);
            }
            SurfaceEvent::SidePressed { index } if usize::from(index) == SETTINGS_SIDE => {
                self.set_mode(Mode::Settings);
            }
            SurfaceEvent::SidePressed { index } if usize::from(index) == MUTE_SIDE => {
                self.set_mode(Mode::Mute);
            }
            SurfaceEvent::SidePressed { index } if usize::from(index) == SOLO_SIDE => {
                self.set_mode(Mode::Solo);
            }
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
    /// Call every pass of the control loop, or a hold completes only when another event
    /// arrives.
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
    /// The tempo cannot move once a clip exists.
    fn press_tempo(&mut self, direction: f64, now: Duration) {
        if self.session.has_any_clip() {
            self.show_tempo(now);
            return;
        }

        let before = self.tempo;
        self.nudge_tempo(direction * TEMPO_STEP);
        self.dirty = true;
        self.tempo_hold = Some(TempoHold {
            button: if direction > 0.0 {
                Control::TempoUp
            } else {
                Control::TempoDown
            },
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
    /// Shown on release, since each update restarts the scroll from the edge. A press that
    /// moved nothing says nothing.
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
            // The engine's own value wins: the display may have been left optimistic by a
            // refusal that never arrived.
            Event::Tempo { bpm } => {
                self.tempo = bpm;
                self.tempo_before_request = bpm;
                self.dirty = true;
            }
            Event::Bar { .. }
            | Event::ClipRecorded { .. }
            | Event::ClipReleased { .. }
            | Event::RecordBufferLow { .. }
            | Event::RecordingRefused { .. }
            | Event::Xrun { .. }
            | Event::Clipped { .. }
            | Event::SnapshotComplete { .. }
            // The clock goes straight to the surface rather than through the grid.
            | Event::Clock { .. } => {}
        }
    }

    /// Takes everything queued for the engine.
    pub fn drain_commands(&mut self) -> std::vec::Drain<'_, Command> {
        self.commands.drain(..)
    }

    /// Marks whatever is waiting on the next press.
    ///
    /// Applied to every screen, so a held button looks the same on any of them.
    fn overlay(&mut self) {
        match self.mode {
            Mode::Volume => self.frame.set_side(VOLUME_SIDE, Led::flash(SELECTED)),
            Mode::Input => self.frame.set_side(INPUT_SIDE, Led::flash(SELECTED)),
            Mode::Settings => self.frame.set_side(SETTINGS_SIDE, Led::flash(SELECTED)),
            Mode::Mute => self.frame.set_side(MUTE_SIDE, Led::flash(SELECTED)),
            Mode::Solo => self.frame.set_side(SOLO_SIDE, Led::flash(SELECTED)),
            _ => {}
        }

        if let Some(hold) = self.tempo_hold {
            // Steady while pressed, blinking once it starts repeating, so the button
            // says which of the two is happening.
            let led = if self.tempo_repeating() {
                Led::flash(SELECTED)
            } else {
                Led::solid(SELECTED)
            };
            self.frame.set_control(hold.button.index(), led);
        }

        // A hold about to empty a pad says so before the audio disappears.
        for addr in SlotAddr::all() {
            if self.warning & bit(addr) != 0 {
                self.frame.set_pad(addr, Led::flash(LedColor::White));
            }
        }
    }

    /// The frame to show, if anything changed since it was last taken.
    pub fn take_frame(&mut self) -> Option<&LedFrame> {
        // A frame cuts off text that is on the grid. Queued text is not on it yet, so the
        // frame that goes with it still gets out.
        if !self.dirty || self.text_running {
            return None;
        }

        self.frame = if self.mode == Mode::Volume {
            paint::volumes(self.chrome)
        } else if self.mode == Mode::Input {
            paint::inputs(self.chrome)
        } else if self.mode == Mode::Settings {
            paint::settings(self.chrome)
        } else if self.tempo_repeating() {
            // A number cannot track a tempo that is still moving, so the grid shows it
            // instead until the button is let go.
            paint::tempo_gauge(self.tempo, self.chrome)
        } else if let Some(button) = self.mode.button() {
            paint::picker(self.sessions, self.current, self.chrome, button)
        } else {
            paint::frame(&self.session, self.chrome)
        };

        self.overlay();
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
    use free_loop_core::{ClipId, Frames, SlotId, SlotState, TrackId, column_mask, row_mask};

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

    fn side(index: usize) -> SurfaceEvent {
        SurfaceEvent::SidePressed {
            index: u8::try_from(index).unwrap(),
        }
    }

    #[test]
    fn muting_silences_the_whole_row() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(2, 0), T0);

        assert_eq!(
            commands(&mut controller),
            vec![Command::SetMutes {
                muted: row_mask(TrackId::new(2).unwrap()),
                soloed: 0,
            }]
        );
    }

    #[test]
    fn the_axis_button_switches_to_columns() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::Axis), T0);
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(2, 5), T0);

        assert_eq!(
            commands(&mut controller),
            vec![Command::SetMutes {
                muted: column_mask(SlotId::new(5).unwrap()),
                soloed: 0,
            }]
        );
    }

    #[test]
    fn pressing_a_silenced_group_frees_it() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(2, 0), T0);
        commands(&mut controller);

        press(&mut controller, addr(2, 4), T0);
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetMutes {
                muted: 0,
                soloed: 0,
            }],
            "any pad in the group frees the group"
        );
    }

    #[test]
    fn mute_and_solo_are_kept_apart() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);
        commands(&mut controller);

        controller.on_surface(side(SOLO_SIDE), T0);
        press(&mut controller, addr(1, 0), T0);

        assert_eq!(
            commands(&mut controller),
            vec![Command::SetMutes {
                muted: row_mask(TrackId::new(0).unwrap()),
                soloed: row_mask(TrackId::new(1).unwrap()),
            }]
        );
    }

    #[test]
    fn the_grid_keeps_showing_the_loops_while_choosing_a_group() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Playing { clip: ClipId(0) },
        });
        let playing = controller.take_frame().unwrap().pad(addr(0, 0));

        controller.on_surface(side(MUTE_SIDE), T0);
        let frame = controller.take_frame().expect("the mode changed");
        assert_eq!(
            frame.pad(addr(0, 0)),
            playing,
            "opening mute must not blank what the loops are doing"
        );
        assert_eq!(
            frame.side(MUTE_SIDE).style,
            free_loop_surface::LedStyle::Flash
        );
    }

    #[test]
    fn a_silenced_row_shows_on_the_grid_without_the_mode_open() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Playing { clip: ClipId(0) },
        });

        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);
        controller.on_surface(side(MUTE_SIDE), T0);
        assert_eq!(controller.mode(), Mode::Perform);

        let frame = controller.take_frame().unwrap();
        assert_eq!(
            frame.pad(addr(0, 0)),
            Led::pulse(free_loop_surface::MUTED),
            "still playing, still silenced, and both are visible"
        );
        assert_eq!(
            frame.pad(addr(0, 7)),
            Led::dim(free_loop_surface::MUTED),
            "and the rest of the row is lit so the group reads as one"
        );
    }

    #[test]
    fn a_pad_in_the_mute_screen_does_not_touch_the_loops() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);

        let sent = commands(&mut controller);
        assert!(!sent.iter().any(|c| matches!(c, Command::Press(_))));

        controller.tick(millis(2_000));
        assert!(
            commands(&mut controller).is_empty(),
            "and cannot empty a pad by holding"
        );
    }

    #[test]
    fn the_mute_screen_closes_on_a_second_press() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        assert_eq!(controller.mode(), Mode::Mute);

        controller.on_surface(side(MUTE_SIDE), T0);
        assert_eq!(controller.mode(), Mode::Perform);
    }

    #[test]
    fn a_column_sets_the_level_for_its_row() {
        let mut controller = controller();
        controller.on_surface(side(VOLUME_SIDE), T0);
        press(&mut controller, addr(2, 6), T0);

        let mut expected = [UNITY_STEP; TRACK_COUNT];
        expected[2] = 6;
        assert_eq!(controller.gains(), expected);
        assert_eq!(commands(&mut controller), vec![Command::SetGains(expected)]);
    }

    #[test]
    fn a_level_press_does_not_launch_the_pad() {
        let mut controller = controller();
        controller.on_surface(side(VOLUME_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);

        let sent = commands(&mut controller);
        assert!(!sent.iter().any(|c| matches!(c, Command::Press(_))));

        controller.tick(millis(2_000));
        assert!(commands(&mut controller).is_empty(), "and cannot empty it");
    }

    #[test]
    fn the_levels_show_as_a_bar_per_row() {
        let mut controller = controller();
        controller.on_surface(side(VOLUME_SIDE), T0);
        press(&mut controller, addr(1, 5), T0);

        let frame = controller.take_frame().unwrap();
        assert!(frame.pad(addr(1, 0)).is_lit(), "the bottom of the bar");
        assert!(frame.pad(addr(1, 5)).is_lit(), "up to the level");
        assert!(!frame.pad(addr(1, 6)).is_lit(), "and no further");
    }

    #[test]
    fn a_fresh_session_puts_every_level_back() {
        let mut controller = controller();
        controller.on_surface(side(VOLUME_SIDE), T0);
        press(&mut controller, addr(0, 1), T0);
        controller.on_surface(side(VOLUME_SIDE), T0);
        commands(&mut controller);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.on_surface(side(NEW_SIDE), T0);

        assert_eq!(controller.gains(), [UNITY_STEP; TRACK_COUNT]);
    }

    #[test]
    fn a_loaded_session_brings_its_levels() {
        let mut controller = controller();
        let mut gains = [UNITY_STEP; TRACK_COUNT];
        gains[4] = 1;

        controller.set_gains(gains);
        assert_eq!(controller.gains(), gains);
        assert_eq!(commands(&mut controller), vec![Command::SetGains(gains)]);
    }

    #[test]
    fn a_fresh_session_empties_everything() {
        let mut controller = controller();
        controller.on_engine(Event::SlotChanged {
            addr: addr(0, 0),
            state: SlotState::Playing { clip: ClipId(0) },
        });
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);
        controller.on_surface(side(MUTE_SIDE), T0);
        controller.session_loaded(addr(2, 2), true);
        commands(&mut controller);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.on_surface(side(NEW_SIDE), T0);

        assert_eq!(controller.mode(), Mode::Perform);
        assert_eq!(controller.current_session(), None, "nothing to save over");
        assert!(!controller.paused(), "ready to record straight away");

        let sent = commands(&mut controller);
        assert!(sent.contains(&Command::ClearAll));
        assert!(sent.contains(&Command::SetMutes {
            muted: 0,
            soloed: 0,
        }));

        let frame = controller.take_frame().unwrap();
        assert!(
            SlotAddr::all().all(|a| !frame.pad(a).is_lit()),
            "an empty grid"
        );
    }

    #[test]
    fn a_fresh_session_goes_back_to_the_configured_tempo() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(50));
        assert_eq!(controller.tempo(), 121.0);
        commands(&mut controller);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.on_surface(side(NEW_SIDE), T0);

        assert_eq!(controller.tempo(), 120.0);

        let sent = commands(&mut controller);
        let clear = sent.iter().position(|c| *c == Command::ClearAll);
        let tempo = sent.iter().position(|c| matches!(c, Command::SetTempo(_)));
        assert!(
            clear < tempo,
            "the tempo is locked until the clips are gone"
        );
    }

    #[test]
    fn a_fresh_session_is_only_offered_from_the_load_picker() {
        let mut controller = controller();
        controller.on_surface(side(NEW_SIDE), T0);
        assert!(commands(&mut controller).is_empty(), "not while playing");

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        controller.on_surface(side(NEW_SIDE), T0);
        assert!(
            commands(&mut controller).is_empty(),
            "and not from the save picker"
        );
    }

    #[test]
    fn everything_waiting_on_a_press_flashes_the_same_colour() {
        let mut controller = controller();

        controller.on_surface(side(SOLO_SIDE), T0);
        assert_eq!(
            controller.take_frame().unwrap().side(SOLO_SIDE),
            Led::flash(SELECTED)
        );
        controller.on_surface(side(SOLO_SIDE), T0);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        assert_eq!(
            controller
                .take_frame()
                .unwrap()
                .control(Control::SaveSession.index()),
            Led::flash(SELECTED)
        );
    }

    #[test]
    fn a_held_tempo_button_shows_that_it_is_held() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);

        let frame = controller.take_frame().expect("the button changed");
        assert_eq!(
            frame.control(Control::TempoUp.index()),
            Led::solid(SELECTED),
            "steady while simply pressed"
        );
        assert_ne!(frame.control(Control::TempoDown.index()).color, SELECTED);

        controller.tick(millis(400));
        let frame = controller.take_frame().expect("it started repeating");
        assert_eq!(
            frame.control(Control::TempoUp.index()),
            Led::flash(SELECTED),
            "and blinks once it repeats"
        );

        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), millis(500));

        // The frame that clears the button goes out before the number takes the grid.
        let frame = controller.take_frame().expect("the button was let go");
        assert_ne!(
            frame.control(Control::TempoUp.index()).color,
            SELECTED,
            "the button stops showing as held the moment it is released"
        );

        controller.take_text();
        controller.tick(millis(500) + TEXT_DURATION);
        controller.take_text();
        assert!(controller.take_frame().is_some(), "and the grid comes back");
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
    fn a_device_loss_freezes_the_transport() {
        let mut controller = controller();
        assert!(!controller.paused());

        controller.pause();
        assert!(controller.paused(), "and the grid says so");
        assert!(commands(&mut controller).contains(&Command::SetPaused(true)));
    }

    #[test]
    fn a_device_loss_while_already_paused_changes_nothing() {
        let mut controller = controller();
        controller.pause();
        let _ = commands(&mut controller);

        controller.pause();
        assert!(
            commands(&mut controller).is_empty(),
            "no second pause to send"
        );
    }

    #[test]
    fn a_loaded_tempo_reaches_the_display_without_a_second_command() {
        let mut controller = controller();
        let _ = commands(&mut controller);

        controller.set_loaded_tempo(90.0);
        assert_eq!(controller.tempo(), 90.0);
        assert!(
            commands(&mut controller).is_empty(),
            "the engine took it from the load itself"
        );
    }

    #[test]
    fn a_fresh_session_puts_every_track_back_to_its_defaults() {
        let mut controller = controller();
        let mut modes = [LaunchMode::Follow; TRACK_COUNT];
        modes[7] = LaunchMode::Restart;
        controller.set_launch_modes(modes);
        let mut inputs = [TrackInput::Stereo; TRACK_COUNT];
        inputs[7] = TrackInput::Mono(1);
        controller.set_inputs(inputs);
        let _ = commands(&mut controller);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.on_surface(side(NEW_SIDE), T0);

        assert_eq!(controller.launch_modes(), [LaunchMode::Follow; TRACK_COUNT]);
        assert_eq!(controller.inputs(), [TrackInput::Stereo; TRACK_COUNT]);
    }

    #[test]
    fn the_settings_button_toggles_a_track_between_the_modes() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::SidePressed { index: 3 }, T0);
        let _ = commands(&mut controller);

        let pad = SlotAddr::new(TrackId::new(2).unwrap(), SlotId::new(0).unwrap());
        let press = SurfaceEvent::PadPressed {
            addr: pad,
            velocity: 127,
        };
        controller.on_surface(press, T0);

        let mut wanted = [LaunchMode::Follow; TRACK_COUNT];
        wanted[2] = LaunchMode::Restart;
        assert_eq!(controller.launch_modes(), wanted);
        assert!(commands(&mut controller).contains(&Command::SetLaunchModes(wanted)));

        controller.on_surface(press, T0);
        assert_eq!(
            controller.launch_modes(),
            [LaunchMode::Follow; TRACK_COUNT],
            "and back again"
        );
    }

    #[test]
    fn a_settings_column_that_does_nothing_yet_is_ignored() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::SidePressed { index: 3 }, T0);
        let _ = commands(&mut controller);

        let pad = SlotAddr::new(TrackId::new(0).unwrap(), SlotId::new(4).unwrap());
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: pad,
                velocity: 127,
            },
            T0,
        );
        assert!(commands(&mut controller).is_empty());
    }

    #[test]
    fn the_input_button_opens_a_row_per_track() {
        let mut controller = controller();
        controller.set_input_count(2);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);

        let frame = controller.take_frame().unwrap();
        let row = SlotAddr::new(TrackId::new(0).unwrap(), SlotId::new(0).unwrap());
        assert!(frame.pad(row).is_lit(), "stereo is offered on column zero");
    }

    #[test]
    fn picking_a_column_sets_that_tracks_input() {
        let mut controller = controller();
        controller.set_input_count(2);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);
        let _ = commands(&mut controller);

        let pad = SlotAddr::new(TrackId::new(3).unwrap(), SlotId::new(2).unwrap());
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: pad,
                velocity: 127,
            },
            T0,
        );

        let mut wanted = [TrackInput::Stereo; TRACK_COUNT];
        wanted[3] = TrackInput::Mono(1);
        assert_eq!(controller.inputs(), wanted, "column two is input one");
        assert!(commands(&mut controller).contains(&Command::SetInputs(wanted)));
    }

    #[test]
    fn a_column_the_device_cannot_offer_is_ignored() {
        let mut controller = controller();
        controller.set_input_count(2);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);
        let _ = commands(&mut controller);

        let pad = SlotAddr::new(TrackId::new(0).unwrap(), SlotId::new(5).unwrap());
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: pad,
                velocity: 127,
            },
            T0,
        );

        assert_eq!(controller.inputs(), [TrackInput::Stereo; TRACK_COUNT]);
        assert!(commands(&mut controller).is_empty());
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
        controller.take_frame();
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
