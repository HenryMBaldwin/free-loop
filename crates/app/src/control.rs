//! The control loop's state.
//!
//! Sits between the surface and the engine: gestures in, commands out, reports in, a
//! frame out. Owns no I/O and takes the time as an argument rather than reading a clock.
//!
//! The session model here is a mirror of the engine's, kept to paint the grid.

use core::time::Duration;

use free_loop_core::{
    CENTRE_STEP, Command, Event, LaunchMode, MAX_BPM, MIN_BPM, PAN_STEPS, Polyphony, SLOT_COUNT,
    SessionModel, Settings, SlotAddr, Subdivision, TRACK_COUNT, Tempo, TimeSignature, TrackInput,
    UNITY_STEP, column_mask, pad_bit, row_mask,
};
use free_loop_surface::{Control, Led, LedColor, LedFrame, SHADES, SIDE_COUNT, SurfaceEvent};

use crate::paint;
use crate::paint::{
    Axis, Chrome, NO_PAD, PICKUP_COLUMN, POLYPHONY_COLUMN, RESTART_COLUMN, SELECTED, YES_PAD,
};
use crate::screen::{self, Button, Role};

/// Beats per minute one press of the tempo buttons moves.
pub const TEMPO_STEP: f64 = 1.0;

/// How long a pad must be held to empty it.
pub const CLEAR_HOLD: Duration = Duration::from_secs(1);

/// How long into a hold the pad starts warning that it is about to empty.
pub const CLEAR_WARNING: Duration = Duration::from_millis(400);

/// Beats per minute a tempo button moves once it starts repeating.
pub const TEMPO_HOLD_STEP: f64 = 5.0;

/// How long the click button must be held to open its own page.
pub const CLICK_HOLD: Duration = Duration::from_millis(400);

/// How long a tempo button must be held before it starts repeating.
pub const TEMPO_HOLD_DELAY: Duration = Duration::from_millis(400);

/// How often a held tempo button repeats.
pub const TEMPO_HOLD_INTERVAL: Duration = Duration::from_millis(120);

/// How long the grid holds the colour that answers a save or a load.
pub const RESULT_FLASH: Duration = Duration::from_millis(500);

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
    /// Where each track sits across the stereo field.
    Pan,
    /// Which input each track records.
    Input,
    /// One row of settings per track.
    Settings,
    /// Beats to the bar and the note that gets the beat.
    TimeSignature,
    /// How often the click sounds.
    Subdivision,
    /// Waiting for yes or no before saving over the pad it holds.
    ConfirmSave(SlotAddr),
    /// Waiting for yes or no before loading over what is on the grid.
    ConfirmLoad(SlotAddr),
}

impl Mode {
    /// The button that opens a picker, if this mode is one.
    fn button(self) -> Option<Control> {
        match self {
            Self::SavePicker | Self::ConfirmSave(_) => Some(Control::SaveSession),
            Self::LoadPicker | Self::ConfirmLoad(_) => Some(Control::LoadSession),
            // Mute and solo open from the side column, not the top row.
            Self::Perform
            | Self::Mute
            | Self::Solo
            | Self::Volume
            | Self::Pan
            | Self::Input
            | Self::Settings
            | Self::TimeSignature
            | Self::Subdivision => None,
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
    /// The screen it went down on. A hold does not reach across to another.
    screen: Mode,
}

/// The click button being held down.
#[derive(Debug, Clone, Copy)]
struct ClickHold {
    /// When it went down.
    since: Duration,
    /// Whether the hold has already done its work, so letting go does nothing more.
    used: bool,
    /// The screen it went down on.
    screen: Mode,
}

/// The grid lit one colour to answer something the performer asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Flash {
    color: LedColor,
    /// When the grid goes back to showing the loops.
    until: Duration,
    /// Scrolled once the colour has been held.
    then: Option<String>,
}

/// Something for the surface to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextUpdate {
    /// Show this.
    Show(String),
    /// Stop showing anything.
    Stop,
}

/// One thing the performer asked for, in the order it was asked.
///
/// Commands and requests share a queue: a request can start a load, which the audio side
/// applies after the commands it has already taken, so their order has to survive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Work {
    /// For the engine.
    Command(Command),
    /// For the caller, which has the disk and the loader.
    Request(Request),
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
    /// The musical time the loaded material is in.
    time_signature: TimeSignature,
    /// What the config asked for, which a fresh session goes back to.
    default_time_signature: TimeSignature,
    /// The last signature the engine confirmed, which a refusal falls back to.
    confirmed_signature: TimeSignature,
    /// When the click button went down, and whether the hold has already been used.
    click_hold: Option<ClickHold>,
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
    /// A pad held down on the input page, which the next press pairs with.
    input_held: Option<SlotAddr>,
    /// Commands and requests in the order the performer made them.
    work: Vec<Work>,
    /// A tempo button being held down.
    tempo_hold: Option<TempoHold>,
    /// When the beat indicator goes dark again.
    beat_off: Option<Duration>,
    /// A display change the caller has not picked up yet.
    text: Option<TextUpdate>,
    /// When the grid comes back, while text has it.
    text_until: Option<Duration>,
    /// Whether text is on the grid now, as opposed to merely queued.
    text_running: bool,
    /// The screen the frame on the surface was painted for.
    painted: Mode,
    /// A colour answering a save, over the whole grid until it expires.
    flash: Option<Flash>,
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
    pub fn new(tempo: f64, time_signature: TimeSignature, click_enabled: bool) -> Self {
        let chrome = Chrome {
            inputs: [TrackInput::default(); TRACK_COUNT],
            launch_modes: [LaunchMode::Follow; TRACK_COUNT],
            pickups: [0; TRACK_COUNT],
            polyphony: [Polyphony::Single; TRACK_COUNT],
            pans: [CENTRE_STEP; TRACK_COUNT],
            input_count: 2,
            beat: 0,
            beat_lit: true,
            subdivision: Subdivision::default(),
            signature: time_signature,
            click_enabled,
            paused: false,
            axis: Axis::Row,
            muted: 0,
            soloed: 0,
            gains: [UNITY_STEP; TRACK_COUNT],
        };
        let session = SessionModel::new();
        let mut controller = Self {
            frame: paint::frame(&session, chrome),
            session,
            chrome,
            tempo,
            default_tempo: tempo,
            time_signature,
            default_time_signature: time_signature,
            confirmed_signature: time_signature,
            click_hold: None,
            default_input: TrackInput::default(),
            default_launch_mode: LaunchMode::Follow,
            tempo_before_request: tempo,
            held: [[None; SLOT_COUNT]; TRACK_COUNT],
            warning: 0,
            input_held: None,
            work: Vec::new(),
            tempo_hold: None,
            beat_off: None,
            text: None,
            text_until: None,
            text_running: false,
            painted: Mode::Perform,
            flash: None,
            mode: Mode::Perform,
            sessions: 0,
            current: None,
            dirty: true,
        };
        // The engine is told what it starts on, before anything else is asked for.
        controller.mark_settings();
        controller
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

    /// How often the click sounds.
    pub fn subdivision(&self) -> Subdivision {
        self.chrome.subdivision
    }

    /// Whether the transport is believed to be frozen.
    pub fn paused(&self) -> bool {
        self.chrome.paused
    }

    /// Says on the grid that the audio device has gone away.
    pub fn device_lost(&mut self, now: Duration) {
        self.scroll("NO AUDIO".to_owned(), now);
    }

    /// Freezes the transport without a press, for something the performer did not ask for.
    pub fn pause(&mut self) {
        if self.chrome.paused {
            return;
        }
        self.chrome.paused = true;
        self.command(Command::SetPaused(true));
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
        self.mark_settings();
        self.dirty = true;
    }

    /// Moves the pad's track across the stereo field. Pads past the row do nothing.
    fn set_pan(&mut self, addr: SlotAddr) {
        if addr.slot.index() >= PAN_STEPS {
            return;
        }
        let step = u8::try_from(addr.slot.index()).unwrap_or(CENTRE_STEP);
        self.chrome.pans[addr.track.index()] = step;
        self.mark_settings();
        self.dirty = true;
    }

    /// Sets which channels the pad's track records, if the device offers them.
    ///
    /// A press on its own takes that one channel. A press while another pad on the same
    /// row is still down takes both, lower channel on the left.
    fn press_input(&mut self, addr: SlotAddr) {
        let column = addr.slot.index();
        if column >= self.chrome.input_count {
            return;
        }
        let channel = u8::try_from(column).unwrap_or(0);

        self.chrome.inputs[addr.track.index()] = match self.input_held {
            Some(held) if held.track == addr.track && held.slot != addr.slot => {
                let first = u8::try_from(held.slot.index()).unwrap_or(0);
                TrackInput::pair(first, channel)
            }
            _ => {
                self.input_held = Some(addr);
                TrackInput::Mono(channel)
            }
        };
        self.mark_settings();
        self.dirty = true;
    }

    /// Flips the setting the pad's column stands for.
    fn toggle_setting(&mut self, addr: SlotAddr) {
        let track = addr.track.index();
        match addr.slot.index() {
            RESTART_COLUMN => {
                self.chrome.launch_modes[track] = self.chrome.launch_modes[track].toggled();
            }
            // Off, then a degree per beat the tail can stand in for.
            PICKUP_COLUMN => {
                let degrees = self.chrome.signature.beats_per_bar().saturating_sub(1);
                let degrees = u8::try_from(degrees).unwrap_or(u8::MAX).min(SHADES - 1);
                let next = self.chrome.pickups[track] + 1;
                self.chrome.pickups[track] = if next > degrees { 0 } else { next };
            }
            POLYPHONY_COLUMN => {
                self.chrome.polyphony[track] = self.chrome.polyphony[track].toggled();
            }
            _ => return,
        }
        self.mark_settings();
        self.dirty = true;
    }

    /// Beats each track opens its loops from the tail for.
    pub fn pickups(&self) -> [u8; TRACK_COUNT] {
        self.chrome.pickups
    }

    /// Takes the pickup settings a loaded session came with.
    pub fn set_pickups(&mut self, pickups: [u8; TRACK_COUNT]) {
        self.chrome.pickups = pickups;
        self.mark_settings();
        self.dirty = true;
    }

    /// Where each track sits across the stereo field.
    pub fn pans(&self) -> [u8; TRACK_COUNT] {
        self.chrome.pans
    }

    /// Takes the pan settings a loaded session came with.
    pub fn set_pans(&mut self, pans: [u8; TRACK_COUNT]) {
        self.chrome.pans = pans;
        self.mark_settings();
        self.dirty = true;
    }

    /// How many of each track's loops may sound at once.
    pub fn polyphony(&self) -> [Polyphony; TRACK_COUNT] {
        self.chrome.polyphony
    }

    /// Takes the polyphony settings a loaded session came with.
    pub fn set_polyphony(&mut self, polyphony: [Polyphony; TRACK_COUNT]) {
        self.chrome.polyphony = polyphony;
        self.mark_settings();
        self.dirty = true;
    }

    /// Where each track's clips are anchored when launched.
    pub fn launch_modes(&self) -> [LaunchMode; TRACK_COUNT] {
        self.chrome.launch_modes
    }

    /// Takes the modes a loaded session came with.
    pub fn set_launch_modes(&mut self, modes: [LaunchMode; TRACK_COUNT]) {
        self.chrome.launch_modes = modes;
        self.mark_settings();
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
        self.mark_settings();
        self.dirty = true;
    }

    /// Takes the levels a loaded session came with.
    pub fn set_gains(&mut self, gains: [u8; TRACK_COUNT]) {
        self.chrome.gains = gains;
        self.mark_settings();
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

        self.mark_settings();
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
        self.chrome.inputs = [self.default_input; TRACK_COUNT];
        self.chrome.launch_modes = [self.default_launch_mode; TRACK_COUNT];
        self.chrome.pickups = [0; TRACK_COUNT];
        self.chrome.polyphony = [Polyphony::Single; TRACK_COUNT];
        self.chrome.pans = [CENTRE_STEP; TRACK_COUNT];
        self.mark_settings();
        self.command(Command::ClearAll);
        self.command(Command::SetPaused(false));

        // After the clear, so the engine is no longer holding clips to protect and takes
        // the change rather than refusing it.
        self.tempo = self.default_tempo;
        self.tempo_before_request = self.default_tempo;
        if let Ok(tempo) = Tempo::new(self.default_tempo) {
            self.command(Command::SetTempo(tempo));
        }
        self.adopt_signature(self.default_time_signature);
        self.command(Command::SetTimeSignature(self.default_time_signature));
        self.dirty = true;
    }

    /// Saves over the pad, asking first if it holds anything.
    fn press_save(&mut self, addr: SlotAddr) {
        if self.sessions & bit(addr) == 0 {
            self.request(Request::SaveSession(addr));
            return;
        }
        self.mode = Mode::ConfirmSave(addr);
        self.dirty = true;
    }

    /// Loads the pad, asking first if the grid holds anything.
    fn press_load(&mut self, addr: SlotAddr) {
        // Nothing to load from a pad that holds nothing.
        if self.sessions & bit(addr) == 0 {
            return;
        }
        if !self.session.has_any_clip() {
            self.request(Request::LoadSession(addr));
            return;
        }
        self.mode = Mode::ConfirmLoad(addr);
        self.dirty = true;
    }

    /// Takes yes or no to the question the grid is asking. Any other pad is ignored.
    fn answer(&mut self, addr: SlotAddr) {
        let pressed = (addr.track.index(), addr.slot.index());
        if pressed == NO_PAD {
            self.cancel_picker();
            return;
        }
        if pressed != YES_PAD {
            return;
        }
        match self.mode {
            Mode::ConfirmSave(pad) => self.request(Request::SaveSession(pad)),
            Mode::ConfirmLoad(pad) => self.request(Request::LoadSession(pad)),
            _ => return,
        }
        self.mode = Mode::Perform;
        self.dirty = true;
    }

    /// Opens a picker, or closes it if it was already open.
    fn set_mode(&mut self, wanted: Mode) {
        self.input_held = None;
        self.mode = if self.mode == wanted {
            Mode::Perform
        } else {
            wanted
        };
        self.dirty = true;
    }

    /// Takes the tempo a loaded session came with.
    ///
    /// The engine took it from the load itself, so this only brings the display in step.
    pub fn set_loaded_tempo(&mut self, bpm: f64) {
        self.tempo = bpm;
        self.tempo_before_request = bpm;
        self.dirty = true;
    }

    /// Takes the time signature a loaded session was recorded in.
    pub fn set_loaded_time_signature(&mut self, signature: TimeSignature) {
        self.adopt_signature(signature);
    }

    /// The signature a fresh session goes back to.
    pub fn set_default_time_signature(&mut self, signature: TimeSignature) {
        self.default_time_signature = signature;
    }

    /// Shows a signature without touching anything that depends on it.
    ///
    /// Used for a change the engine has not confirmed, so a refusal only has this to undo.
    fn show_signature_state(&mut self, signature: TimeSignature) {
        self.time_signature = signature;
        self.chrome.signature = signature;
        self.dirty = true;
    }

    /// Takes a signature the engine is known to be running, and everything that follows.
    fn adopt_signature(&mut self, signature: TimeSignature) {
        self.show_signature_state(signature);
        self.confirmed_signature = signature;

        // A rate the bar cannot be cut into falls back to one click a beat.
        if !self.chrome.subdivision.fits(signature) {
            let fitting = Subdivision::fitting(signature);
            self.chrome.subdivision = fitting;
            self.command(Command::SetClickSubdivision(fitting));
        }
        // A pickup cannot reach past the bar it opens from.
        let degrees = signature.beats_per_bar().saturating_sub(1);
        let degrees = u8::try_from(degrees).unwrap_or(u8::MAX).min(SHADES - 1);
        let pulled_in = self.chrome.pickups.iter().any(|pickup| *pickup > degrees);
        for pickup in &mut self.chrome.pickups {
            *pickup = (*pickup).min(degrees);
        }
        if pulled_in {
            self.mark_settings();
        }
    }

    /// Changes how often the click sounds. Never locked: no clip depends on it.
    fn press_subdivision(&mut self, subdivision: Subdivision, now: Duration) {
        // A rate the bar cannot be cut into is shown greyed, and does nothing.
        if !subdivision.fits(self.chrome.signature) {
            return;
        }
        if subdivision != self.chrome.subdivision {
            self.chrome.subdivision = subdivision;
            self.command(Command::SetClickSubdivision(subdivision));
            self.dirty = true;
        }
        self.scroll(subdivision.name().to_owned(), now);
    }

    /// Changes one number of the signature, taking effect at once so it can be heard.
    fn press_time_signature(&mut self, addr: SlotAddr, now: Duration) {
        let Some(part) = paint::signature_part(addr) else {
            return;
        };
        let current = self.time_signature;
        let next = match part {
            paint::SignaturePart::Beats(beats) => TimeSignature::new(beats, current.beat_unit()),
            paint::SignaturePart::Unit(unit) => TimeSignature::new(current.beats_per_bar(), unit),
        };
        let Ok(next) = next else {
            return;
        };

        // Locked while clips exist, the same as the tempo, so it only reports.
        if self.session.has_any_clip() {
            self.show_signature(current, now);
            return;
        }
        if next != current {
            // Shown at once so it can be heard, but nothing that depends on it moves
            // until the engine says it took the change. The value to fall back to stays
            // the last one the engine confirmed, however many presses go unanswered.
            self.show_signature_state(next);
            self.command(Command::SetTimeSignature(next));
        }
        self.show_signature(next, now);
    }

    /// Puts a signature on the grid the way the tempo puts its number there.
    fn show_signature(&mut self, signature: TimeSignature, now: Duration) {
        self.scroll(
            format!("{}/{}", signature.beats_per_bar(), signature.beat_unit()),
            now,
        );
    }

    /// Leaves the signature page.
    fn close_time_signature(&mut self) {
        self.mode = Mode::Perform;
        self.dirty = true;
    }

    /// The time signature the transport is in.
    pub fn time_signature(&self) -> TimeSignature {
        self.time_signature
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

    /// Records which session is in use, leaves the picker, and says so on the grid.
    pub fn session_saved(&mut self, addr: SlotAddr, now: Duration) {
        self.sessions |= bit(addr);
        self.current = Some(addr);
        self.mode = Mode::Perform;
        self.show(LedColor::Green, now, None);
    }

    /// Leaves the picker and says on the grid that nothing was written.
    pub fn save_failed(&mut self, now: Duration) {
        self.mode = Mode::Perform;
        self.show(LedColor::Red, now, None);
    }

    /// Leaves the picker and says on the grid that nothing was loaded.
    ///
    /// `code` is scrolled once the colour has been held, for a refusal with a number
    /// worth reading.
    pub fn load_failed(&mut self, now: Duration, code: Option<String>) {
        self.mode = Mode::Perform;
        self.show(LedColor::Red, now, code);
    }

    /// Puts text on the grid for [`TEXT_DURATION`].
    fn scroll(&mut self, text: String, now: Duration) {
        self.text = Some(TextUpdate::Show(text));
        self.text_until = Some(now + TEXT_DURATION);
        self.dirty = true;
    }

    /// Holds the grid at one colour for [`RESULT_FLASH`], then scrolls `then`.
    ///
    /// Text already on the grid is stopped first.
    fn show(&mut self, color: LedColor, now: Duration, then: Option<String>) {
        if self.text_running || self.text_until.is_some() {
            self.text = Some(TextUpdate::Stop);
            self.text_until = None;
        }
        self.flash = Some(Flash {
            color,
            until: now + RESULT_FLASH,
            then,
        });
        self.dirty = true;
    }

    /// Takes a display change, if there is one.
    pub fn take_text(&mut self) -> Option<TextUpdate> {
        let update = self.text.take()?;
        self.text_running = matches!(update, TextUpdate::Show(_));
        Some(update)
    }

    /// Takes everything the performer asked for, in order.
    pub fn drain_work(&mut self) -> std::vec::Drain<'_, Work> {
        self.work.drain(..)
    }

    fn command(&mut self, command: Command) {
        self.work.push(Work::Command(command));
    }

    fn request(&mut self, request: Request) {
        self.work.push(Work::Request(request));
    }

    /// Notes that the settings moved, in its place among everything else asked for.
    ///
    /// They go through the same queue as the rest, so nothing can overtake them. Adjacent
    /// changes coalesce into the later one, which cannot cross anything else.
    fn mark_settings(&mut self) {
        let settings = self.settings();
        if let Some(Work::Command(Command::SetSettings(last))) = self.work.last_mut() {
            *last = settings;
        } else {
            self.command(Command::SetSettings(settings));
        }
    }

    /// Handles something the performer did, at time `now` since the app started.
    ///
    /// What a button does is [`screen::role`]'s answer for the screen showing, so a
    /// button with no part to play on it does nothing.
    pub fn on_surface(&mut self, event: SurfaceEvent, now: Duration) {
        match event {
            SurfaceEvent::PadPressed { addr, .. } => self.press_pad(addr, now),
            SurfaceEvent::PadReleased { addr } => self.release_pad(addr),
            SurfaceEvent::ControlPressed(control) => {
                self.press_button(Button::Top(control), now);
            }
            SurfaceEvent::ControlReleased(control) => {
                self.release_button(Button::Top(control), now);
            }
            SurfaceEvent::SidePressed { index } => {
                self.press_button(Button::Side(usize::from(index)), now);
            }
            SurfaceEvent::SideReleased { .. } => {}
        }
    }

    /// Acts on a grid pad, according to what it means on this screen.
    fn press_pad(&mut self, addr: SlotAddr, now: Duration) {
        match screen::role(self.mode, Button::Grid(addr)) {
            Role::Loop => {
                // Nothing yet: which gesture this is depends on how long it lasts.
                self.held[addr.track.index()][addr.slot.index()] = Some(now);
            }
            Role::Answer => self.answer(addr),
            Role::SaveTo => self.press_save(addr),
            Role::LoadFrom => self.press_load(addr),
            Role::Group => self.toggle_group(addr),
            Role::Level => self.set_level(addr),
            Role::Pan => self.set_pan(addr),
            Role::InputChannel => self.press_input(addr),
            Role::Setting => self.toggle_setting(addr),
            Role::Beats(_) | Role::Unit(_) => self.press_time_signature(addr, now),
            Role::Rate(subdivision) => self.press_subdivision(subdivision, now),
            _ => {}
        }
    }

    fn release_pad(&mut self, addr: SlotAddr) {
        if self.input_held == Some(addr) {
            self.input_held = None;
            return;
        }
        // A hold that completed already emptied the pad and took its entry, so only a
        // release that still has one is a tap.
        if self.held[addr.track.index()][addr.slot.index()]
            .take()
            .is_some()
        {
            self.command(Command::Press(addr));
        }
    }

    /// Acts on a top-row or side button, according to what it means on this screen.
    fn press_button(&mut self, button: Button, now: Duration) {
        match screen::role(self.mode, button) {
            Role::Transport => self.toggle_paused(),
            Role::Tempo(direction) => self.press_tempo(direction, now),
            Role::Click => self.press_click(now),
            Role::StopAll => self.command(Command::StopAll),
            Role::Rewind => self.command(Command::Rewind),
            Role::Axis => {
                self.chrome.axis = self.chrome.axis.flipped();
                self.dirty = true;
            }
            Role::NewSession => self.start_fresh(),
            Role::Open(mode) => self.set_mode(mode),
            _ => {}
        }
    }

    /// Ends a gesture, keyed on the button itself.
    ///
    /// A hold begun on one screen can be let go on another, and has to end either way.
    fn release_button(&mut self, button: Button, now: Duration) {
        match button {
            Button::Top(Control::TempoDown | Control::TempoUp) => self.release_tempo(now),
            Button::Top(Control::ClickToggle) => self.release_click(),
            _ => {}
        }
    }

    /// Freezes or resumes the transport.
    fn toggle_paused(&mut self) {
        self.chrome.paused = !self.chrome.paused;
        self.command(Command::SetPaused(self.chrome.paused));
        self.dirty = true;
    }

    /// Advances anything that depends on time passing rather than on an event.
    ///
    /// Call every pass of the control loop, or a hold completes only when another event
    /// arrives.
    pub fn tick(&mut self, now: Duration) {
        self.repeat_tempo(now);
        self.open_click_page(now);

        if self.beat_off.is_some_and(|until| now >= until) {
            self.beat_off = None;
            self.chrome.beat_lit = false;
            self.dirty = true;
        }

        if let Some(flash) = self.flash.take_if(|flash| now >= flash.until) {
            if let Some(text) = flash.then {
                self.scroll(text, now);
            }
            self.dirty = true;
        }

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
                self.command(Command::Clear(addr));
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
        // Both tempo buttons at once asks for the signature, so the first nudge is undone.
        if let Some(hold) = self.tempo_hold.take() {
            self.undo_nudge(hold.started_at);
            if self.mode == Mode::TimeSignature {
                self.close_time_signature();
            } else {
                self.mode = Mode::TimeSignature;
                self.dirty = true;
            }
            return;
        }
        if self.session.has_any_clip() {
            self.show_tempo(now);
            // The pair is the only way off the signature screen, so the first of them is
            // remembered even though the tempo itself cannot move.
            if self.mode == Mode::TimeSignature {
                self.tempo_hold = Some(self.locked_hold(direction, now));
            }
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
            screen: self.mode,
        });
    }

    /// Puts the tempo back to `was`, in the engine as well as on the display.
    ///
    /// Drops any nudge still queued: one authoritative value goes out instead, since the
    /// earlier one may already have been drained.
    fn undo_nudge(&mut self, was: f64) {
        self.work
            .retain(|work| !matches!(work, Work::Command(Command::SetTempo(_))));
        self.tempo = was;
        self.tempo_before_request = was;
        if let Ok(tempo) = Tempo::new(was) {
            self.command(Command::SetTempo(tempo));
        }
        self.dirty = true;
    }

    /// Whether a held tempo button is climbing on the screen showing.
    ///
    /// A hold from another screen keeps its gauge there: this one paints its own grid.
    fn tempo_repeating(&self) -> bool {
        self.tempo_hold
            .is_some_and(|hold| hold.last > hold.since && self.mode == hold.screen)
    }

    /// Stops the repeat and reports where the tempo landed.
    ///
    /// Shown on release, since each update restarts the scroll from the edge. A press that
    /// moved nothing says nothing.
    fn release_tempo(&mut self, now: Duration) {
        let Some(hold) = self.tempo_hold.take() else {
            return;
        };
        if self.mode != hold.screen {
            // Let go on another screen: the gesture ends, and says nothing here.
            self.dirty = true;
            return;
        }
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

        self.scroll(shown.to_string(), now);
    }

    /// Arms the hold that opens the click's page, or leaves the page if it is open.
    fn press_click(&mut self, now: Duration) {
        if self.mode == Mode::Subdivision {
            self.mode = Mode::Perform;
            self.dirty = true;
            // Used, so letting go does not turn the click off as well.
            self.click_hold = Some(self.holding_click(now, true));
        } else {
            self.click_hold = Some(self.holding_click(now, false));
        }
    }

    /// A tap turns the click on or off. A hold has opened the page already.
    fn release_click(&mut self) {
        let Some(hold) = self.click_hold.take() else {
            return;
        };
        // A tap that went down on another screen has already been answered there, or is
        // no longer this screen's to answer.
        if !hold.used && self.mode == hold.screen {
            self.toggle_click();
        }
    }

    /// The click button going down on the screen showing.
    fn holding_click(&self, now: Duration, used: bool) -> ClickHold {
        ClickHold {
            since: now,
            used,
            screen: self.mode,
        }
    }

    /// Opens the click's page once its button has been held long enough.
    fn open_click_page(&mut self, now: Duration) {
        let Some(hold) = self.click_hold else {
            return;
        };
        if hold.used || self.mode != hold.screen || now.saturating_sub(hold.since) < CLICK_HOLD {
            return;
        }
        self.click_hold = Some(ClickHold { used: true, ..hold });
        self.mode = Mode::Subdivision;
        self.dirty = true;
    }

    /// Turns the click on or off.
    fn toggle_click(&mut self) {
        self.chrome.click_enabled = !self.chrome.click_enabled;
        self.command(Command::SetClickEnabled(self.chrome.click_enabled));
        self.dirty = true;
    }

    /// How long the beat indicator stays lit: half a beat.
    ///
    /// The tempo counts quarter notes, so a beat is `4 / beat_unit` of one.
    fn lit_for(&self) -> Duration {
        let bpm = self.tempo.clamp(MIN_BPM, MAX_BPM);
        let unit = f64::from(self.chrome.signature.beat_unit().max(1));
        Duration::from_secs_f64(120.0 / (bpm * unit))
    }

    /// Moves the tempo again while a button stays down.
    fn repeat_tempo(&mut self, now: Duration) {
        // On the signature screen these buttons are the way out, not the tempo.
        if self.mode == Mode::TimeSignature {
            return;
        }
        let Some(hold) = self.tempo_hold else {
            return;
        };
        // A hold from another screen keeps its place there, and moves nothing here.
        if self.mode != hold.screen {
            return;
        }
        if now.saturating_sub(hold.since) < TEMPO_HOLD_DELAY
            || now.saturating_sub(hold.last) < TEMPO_HOLD_INTERVAL
        {
            return;
        }

        self.nudge_tempo(hold.delta);
        self.tempo_hold = Some(TempoHold { last: now, ..hold });
        self.dirty = true;
    }

    /// A held tempo button that moves nothing, waiting for the other to be pressed.
    fn locked_hold(&self, direction: f64, now: Duration) -> TempoHold {
        TempoHold {
            button: if direction > 0.0 {
                Control::TempoUp
            } else {
                Control::TempoDown
            },
            delta: 0.0,
            since: now,
            last: now,
            started_at: self.tempo,
            screen: self.mode,
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
        self.command(Command::SetTempo(tempo));
    }

    /// Handles something the engine reported.
    pub fn on_engine(&mut self, event: Event, now: Duration) {
        match event {
            Event::SlotChanged { addr, state } => {
                self.session.mirror(addr, state);
                self.dirty = true;
            }
            Event::Beat { beat, .. } => {
                if beat != self.chrome.beat || !self.chrome.beat_lit {
                    self.chrome.beat = beat;
                    self.chrome.beat_lit = true;
                    self.dirty = true;
                }
                self.beat_off = Some(now + self.lit_for());
            }
            // The engine's own value wins, however the change was asked for.
            Event::TimeSignature {
                beats_per_bar,
                beat_unit,
            } => {
                if let Ok(signature) = TimeSignature::new(beats_per_bar, beat_unit) {
                    self.adopt_signature(signature);
                }
            }
            Event::TimeSignatureRejected => {
                self.show_signature_state(self.confirmed_signature);
            }
            Event::TempoRejected => {
                self.tempo = self.tempo_before_request;
                // A number that has just been rolled back is worse than none.
                if self.text_until.take().is_some() {
                    self.text = Some(TextUpdate::Stop);
                    self.dirty = true;
                }
            }
            // The engine's own value wins: the display may have been left optimistic by a
            // refusal that never arrived.
            Event::Tempo { bpm } => {
                self.tempo = bpm;
                self.tempo_before_request = bpm;
                self.dirty = true;
            }
            // A take with nowhere to go says so on the whole grid: the session is full,
            // not that pad.
            Event::RecordingRefused { .. } => self.show(LedColor::Red, now, None),
            // The same answer the store's own refusal gives.
            Event::LoadRefused { wanted, .. } => {
                self.load_failed(now, Some(wanted.to_string()));
            }
            // Bars are already covered by the beat they start with, and the rest are for
            // logging rather than for the grid.
            Event::Bar { .. }
            | Event::ClipRecorded { .. }
            | Event::ClipReleased { .. }
            | Event::RecordBufferLow { .. }
            | Event::Xrun { .. }
            | Event::Clipped { .. }
            | Event::SnapshotComplete { .. }
            // The clock goes straight to the surface rather than through the grid.
            | Event::Clock { .. } => {}
        }
    }

    /// What every track should be set to.
    pub fn settings(&self) -> Settings {
        Settings {
            gains: self.chrome.gains,
            muted: self.chrome.muted,
            soloed: self.chrome.soloed,
            inputs: self.chrome.inputs,
            launch_modes: self.chrome.launch_modes,
            pickups: self.chrome.pickups,
            polyphony: self.chrome.polyphony,
            pans: self.chrome.pans,
        }
    }

    /// Marks whatever is waiting on the next press.
    ///
    /// Applied to every screen, so a held button looks the same on any of them.
    fn overlay(&mut self) {
        // A button with no part to play on this screen shows nothing, so there is no way
        // into another screen to reach for.
        for control in Control::all() {
            if screen::role(self.mode, Button::Top(control)) == Role::Inert {
                self.frame.set_control(control.index(), Led::OFF);
            }
        }
        for index in 0..SIDE_COUNT {
            if screen::role(self.mode, Button::Side(index)) == Role::Inert {
                self.frame.set_side(index, Led::OFF);
            }
        }
        // Shares a top button with a control, and the beat is worth seeing from anywhere.
        paint::beat_indicator(&mut self.frame, self.chrome);

        // Whatever it takes to leave, on whatever screen this is.
        for button in screen::exits(self.mode).into_iter().flatten() {
            let led = Led::flash(SELECTED);
            match button {
                Button::Top(control) => self.frame.set_control(control.index(), led),
                Button::Side(index) => self.frame.set_side(index, led),
                Button::Grid(addr) => self.frame.set_pad(addr, led),
            }
        }

        if let Some(hold) = self.tempo_hold.filter(|hold| self.mode == hold.screen) {
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
        // Text on the grid holds a frame back, but not across a change of screen: the
        // buttons around the text keep offering what the screen behind it did, and they
        // would answer for a screen that is no longer showing.
        if self.text_running && self.painted != self.mode {
            self.text = Some(TextUpdate::Stop);
            self.text_until = None;
        }
        // A frame cuts off text that is on the grid. Queued text is not on it yet, so the
        // frame that goes with it still gets out.
        if !self.dirty || self.text_running {
            return None;
        }
        self.painted = self.mode;

        self.frame = if self.mode == Mode::Volume {
            paint::volumes(self.chrome)
        } else if self.mode == Mode::Pan {
            paint::pans(self.chrome)
        } else if self.mode == Mode::Input {
            paint::inputs(self.chrome)
        } else if self.mode == Mode::Settings {
            paint::settings(self.chrome)
        } else if self.mode == Mode::TimeSignature {
            paint::time_signature(self.time_signature, self.chrome)
        } else if self.mode == Mode::Subdivision {
            paint::subdivisions(self.chrome)
        } else if self.tempo_repeating() {
            // A number cannot track a tempo that is still moving, so the grid shows it
            // instead until the button is let go.
            paint::tempo_gauge(self.tempo, self.chrome)
        } else if matches!(self.mode, Mode::ConfirmSave(_) | Mode::ConfirmLoad(_)) {
            paint::confirm(self.chrome)
        } else if let Some(button) = self.mode.button() {
            paint::picker(self.sessions, self.current, self.chrome, button)
        } else {
            paint::frame(&self.session, self.chrome)
        };

        self.overlay();
        if let Some(flash) = self.flash.as_ref() {
            let led = Led::solid(flash.color);
            for addr in SlotAddr::all() {
                self.frame.set_pad(addr, led);
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
    use crate::paint::{
        BEATS_ROW, INPUT_SIDE, MUTE_SIDE, NEW_SIDE, PAN_SIDE, PAUSE_SIDE, SETTINGS_SIDE, SOLO_SIDE,
        SUBDIVISION_ROW, UNIT_ROW, VOLUME_SIDE,
    };
    use free_loop_core::{ClipId, Frames, SlotId, SlotState, TrackId, column_mask, row_mask};
    use free_loop_surface::FIRST_BEAT_LED;

    const T0: Duration = Duration::ZERO;

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    fn controller() -> Controller {
        Controller::new(120.0, TimeSignature::FOUR_FOUR, true)
    }

    /// The commands the controller has asked for, in order, ignoring any request.
    fn commands(controller: &mut Controller) -> Vec<Command> {
        controller
            .drain_work()
            .filter_map(|work| match work {
                Work::Command(Command::SetSettings(_)) | Work::Request(_) => None,
                Work::Command(command) => Some(command),
            })
            .collect()
    }

    /// The settings the controller has ready for the engine, which must have moved.
    /// The settings the controller has asked for, which must have moved.
    fn settings(controller: &mut Controller) -> Settings {
        offered(controller).expect("the settings moved")
    }

    /// The latest settings in everything asked for, if any moved.
    fn offered(controller: &mut Controller) -> Option<Settings> {
        controller
            .drain_work()
            .filter_map(|work| match work {
                Work::Command(Command::SetSettings(settings)) => Some(settings),
                _ => None,
            })
            .next_back()
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

        let settings = settings(&mut controller);
        assert_eq!(settings.muted, row_mask(TrackId::new(2).unwrap()));
        assert_eq!(settings.soloed, 0);
    }

    #[test]
    fn settings_are_offered_once_until_they_move_again() {
        let mut controller = controller();
        assert!(
            offered(&mut controller).is_some(),
            "the engine is told what it starts on"
        );
        assert_eq!(offered(&mut controller), None);

        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);
        assert!(offered(&mut controller).is_some());
        assert_eq!(offered(&mut controller), None);
    }

    #[test]
    fn only_the_latest_settings_are_offered() {
        let mut controller = controller();
        controller.on_surface(side(VOLUME_SIDE), T0);
        press(&mut controller, addr(1, 2), T0);
        press(&mut controller, addr(1, 6), T0);

        assert_eq!(
            settings(&mut controller).gains[1],
            6,
            "not the level in between"
        );
        assert_eq!(offered(&mut controller), None);
    }

    #[test]
    fn the_grouping_can_be_flipped_from_the_screen_it_groups() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);

        // The axis says what a pad in this screen silences, so it belongs on it.
        controller.on_surface(SurfaceEvent::ControlPressed(Control::Axis), T0);
        press(&mut controller, addr(2, 5), T0);

        let settings = settings(&mut controller);
        assert_eq!(settings.muted, column_mask(SlotId::new(5).unwrap()));
        assert_eq!(controller.mode(), Mode::Mute, "and does not leave it");
    }

    #[test]
    fn the_axis_button_switches_to_columns() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::Axis), T0);
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(2, 5), T0);

        let settings = settings(&mut controller);
        assert_eq!(settings.muted, column_mask(SlotId::new(5).unwrap()));
        assert_eq!(settings.soloed, 0);
    }

    #[test]
    fn pressing_a_silenced_group_frees_it() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(2, 0), T0);
        settings(&mut controller);

        press(&mut controller, addr(2, 4), T0);
        assert_eq!(
            settings(&mut controller).muted,
            0,
            "any pad in the group frees the group"
        );
    }

    #[test]
    fn a_screen_darkens_the_buttons_it_has_no_use_for() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        let frame = controller.take_frame().unwrap();

        // Only the grouping belongs up there, plus the button the beat shares.
        for control in Control::all() {
            if control.index() == FIRST_BEAT_LED || control == Control::Axis {
                continue;
            }
            assert!(
                !frame.control(control.index()).is_lit(),
                "{control:?} is still offering itself"
            );
        }
        assert!(
            frame.control(Control::Axis.index()).is_lit(),
            "grouping stays"
        );
        for index in [VOLUME_SIDE, INPUT_SIDE, SETTINGS_SIDE, SOLO_SIDE, NEW_SIDE] {
            assert!(!frame.side(index).is_lit(), "side {index} is still lit");
        }
        assert!(frame.side(PAUSE_SIDE).is_lit(), "the transport stays");
    }

    #[test]
    fn a_screen_shows_the_way_out_of_it() {
        let mut settings = controller();
        settings.on_surface(side(SETTINGS_SIDE), T0);
        let frame = settings.take_frame().unwrap();
        assert_eq!(frame.side(SETTINGS_SIDE), Led::flash(SELECTED));

        // The signature screen takes both tempo buttons to leave, so both say so.
        let mut signature = controller();
        signature.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        signature.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        let frame = signature.take_frame().unwrap();
        assert_eq!(
            frame.control(Control::TempoDown.index()),
            Led::flash(SELECTED)
        );
    }

    #[test]
    fn the_beat_still_shows_from_another_screen() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        controller.on_engine(Event::Beat { bar: 0, beat: 0 }, T0);

        let frame = controller.take_frame().unwrap();
        assert_eq!(
            frame.control(FIRST_BEAT_LED),
            Led::solid(LedColor::Red),
            "the downbeat, even on a screen that owns nothing else up there"
        );
    }

    #[test]
    fn the_signature_screen_can_be_left_after_a_take_seals() {
        let mut controller = controller();
        open_signature(&mut controller);
        assert_eq!(controller.mode(), Mode::TimeSignature);

        // A take seals while the screen is open, which locks the tempo and the signature.
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Stopped { clip: ClipId(0) },
            },
            T0,
        );

        // The way out must not be lockable: it is the only button that acts here.
        open_signature(&mut controller);
        assert_eq!(
            controller.mode(),
            Mode::Perform,
            "stuck on the signature screen with no way back"
        );
    }

    /// The gesture that opens a screen.
    type Opening = fn(&mut Controller);

    /// Does whatever leaves the screen showing: all of its buttons down, then up.
    ///
    /// All down before any up: a screen taking two of them wants them held together.
    fn press_the_way_out(controller: &mut Controller) {
        let ways: Vec<Button> = crate::screen::exits(controller.mode())
            .into_iter()
            .flatten()
            .collect();
        for button in &ways {
            match button {
                Button::Top(control) => {
                    controller.on_surface(SurfaceEvent::ControlPressed(*control), T0);
                }
                Button::Side(index) => controller.on_surface(side(*index), T0),
                Button::Grid(addr) => press(controller, *addr, T0),
            }
        }
        for button in &ways {
            if let Button::Top(control) = button {
                controller.on_surface(SurfaceEvent::ControlReleased(*control), T0);
            }
        }
    }

    #[test]
    fn every_screen_can_be_left_even_with_a_take_on_the_grid() {
        // A clip locks the tempo and the signature, which must not lock a way out.
        let openings: Vec<(&str, Opening)> = vec![
            ("mute", |c| c.on_surface(side(MUTE_SIDE), T0)),
            ("solo", |c| c.on_surface(side(SOLO_SIDE), T0)),
            ("volume", |c| c.on_surface(side(VOLUME_SIDE), T0)),
            ("input", |c| c.on_surface(side(INPUT_SIDE), T0)),
            ("settings", |c| c.on_surface(side(SETTINGS_SIDE), T0)),
            ("save", |c| {
                c.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
            }),
            ("load", |c| {
                c.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
            }),
            ("signature", open_signature),
            ("click rate", |c| {
                c.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
                c.tick(T0 + CLICK_HOLD);
                c.on_surface(
                    SurfaceEvent::ControlReleased(Control::ClickToggle),
                    T0 + CLICK_HOLD,
                );
            }),
        ];

        for (name, open) in openings {
            let mut controller = controller();
            open(&mut controller);
            assert_ne!(controller.mode(), Mode::Perform, "{name} did not open");

            // The take seals while the screen is open, which is the only way to be on one
            // that the lock could otherwise strand.
            controller.on_engine(
                Event::SlotChanged {
                    addr: addr(0, 0),
                    state: SlotState::Stopped { clip: ClipId(0) },
                },
                T0,
            );

            // Twice over: a confirm screen steps back to its picker before the loops.
            press_the_way_out(&mut controller);
            press_the_way_out(&mut controller);
            assert_eq!(controller.mode(), Mode::Perform, "{name} could not be left");
        }
    }

    #[test]
    fn a_change_of_screen_cuts_text_short_rather_than_waiting_it_out() {
        let mut controller = controller();

        // A tempo tap puts its number on the grid.
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), T0);
        let _ = controller.take_frame();
        assert!(matches!(controller.take_text(), Some(TextUpdate::Show(_))));
        assert!(controller.take_frame().is_none(), "the text has the grid");

        // Moving to another screen must not leave the last one's buttons lit and inert.
        controller.on_surface(side(MUTE_SIDE), T0);
        assert!(
            controller.take_frame().is_none(),
            "still the text this pass"
        );
        assert_eq!(controller.take_text(), Some(TextUpdate::Stop));

        let frame = controller.take_frame().expect("the mute screen goes out");
        assert_eq!(frame.side(MUTE_SIDE), Led::flash(SELECTED));
        assert!(
            !frame.control(Control::StopAll.index()).is_lit(),
            "and the loops' buttons are dark on it"
        );
    }

    #[test]
    fn a_repeating_gauge_does_not_paint_over_the_next_screen() {
        let mut controller = controller();

        // Held long enough to start climbing, and only then does another screen open.
        // Tempo down, since tempo up shares its button with the beat.
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        controller.tick(T0 + TEMPO_HOLD_DELAY + TEMPO_HOLD_INTERVAL);
        assert!(controller.take_frame().is_some(), "the gauge is up");

        controller.on_surface(side(MUTE_SIDE), T0);
        let frame = controller.take_frame().expect("the mute screen goes out");
        assert!(
            !frame.pad(addr(0, 0)).is_lit(),
            "the gauge is still filling a grid whose pads now mute instead"
        );
        assert!(
            !frame.control(Control::TempoDown.index()).is_lit(),
            "and the held button is offering itself on a screen it does nothing on"
        );
    }

    #[test]
    fn a_hold_does_not_outlive_the_screen_it_started_on() {
        let mut controller = controller();

        // Held down, then another finger opens a screen where this button means nothing.
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), T0);

        let settled = controller.tempo();
        controller.tick(T0 + TEMPO_HOLD_DELAY + TEMPO_HOLD_INTERVAL * 4);
        assert_eq!(
            controller.tempo(),
            settled,
            "the tempo ran away after the button was let go"
        );
    }

    #[test]
    fn a_click_hold_does_not_open_its_screen_from_another() {
        let mut controller = controller();

        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        controller.on_surface(side(MUTE_SIDE), T0);
        controller.tick(T0 + CLICK_HOLD);

        assert_eq!(
            controller.mode(),
            Mode::Mute,
            "a hold from the loops reached across into the click screen"
        );
    }

    #[test]
    fn a_screen_cannot_be_left_by_the_button_of_another() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);

        // Volume, settings and solo are all out of reach until mute is left.
        for index in [VOLUME_SIDE, SETTINGS_SIDE, SOLO_SIDE] {
            controller.on_surface(side(index), T0);
            assert_eq!(controller.mode(), Mode::Mute, "side {index} let it out");
        }
        controller.on_surface(side(MUTE_SIDE), T0);
        assert_eq!(controller.mode(), Mode::Perform);
    }

    #[test]
    fn the_transport_answers_from_another_screen() {
        let mut controller = controller();
        controller.on_surface(side(VOLUME_SIDE), T0);
        let _ = commands(&mut controller);

        controller.on_surface(side(PAUSE_SIDE), T0);
        assert_eq!(commands(&mut controller), vec![Command::SetPaused(true)]);
        assert_eq!(controller.mode(), Mode::Volume, "and stays on the screen");
    }

    #[test]
    fn mute_and_solo_are_kept_apart() {
        let mut controller = controller();
        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);
        settings(&mut controller);

        // Out of mute before solo opens: one screen cannot be reached from another.
        controller.on_surface(side(MUTE_SIDE), T0);
        controller.on_surface(side(SOLO_SIDE), T0);
        press(&mut controller, addr(1, 0), T0);

        let settings = settings(&mut controller);
        assert_eq!(settings.muted, row_mask(TrackId::new(0).unwrap()));
        assert_eq!(settings.soloed, row_mask(TrackId::new(1).unwrap()));
    }

    #[test]
    fn the_grid_keeps_showing_the_loops_while_choosing_a_group() {
        let mut controller = controller();
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );
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
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );

        controller.on_surface(side(MUTE_SIDE), T0);
        press(&mut controller, addr(0, 0), T0);
        controller.on_surface(side(MUTE_SIDE), T0);
        assert_eq!(controller.mode(), Mode::Perform);

        let frame = controller.take_frame().unwrap();
        assert_eq!(
            frame.pad(addr(0, 0)),
            Led::pulse(paint::MUTED),
            "still playing, still silenced, and both are visible"
        );
        assert_eq!(
            frame.pad(addr(0, 7)),
            Led::dim(paint::MUTED),
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
        assert_eq!(settings(&mut controller).gains, expected);
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
        assert_eq!(settings(&mut controller).gains, gains);
    }

    #[test]
    fn a_fresh_session_empties_everything() {
        let mut controller = controller();
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );
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

        let (sent, settings, _) = work(&mut controller);
        assert!(sent.contains(&Command::ClearAll));
        assert_eq!(settings, Some(Settings::new()));

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

    /// Everything asked for, split by kind, for a test that wants more than one of them.
    fn work(controller: &mut Controller) -> (Vec<Command>, Option<Settings>, Vec<Request>) {
        let mut commands = Vec::new();
        let mut settings = None;
        let mut requests = Vec::new();
        for item in controller.drain_work() {
            match item {
                Work::Command(Command::SetSettings(latest)) => settings = Some(latest),
                Work::Command(command) => commands.push(command),
                Work::Request(request) => requests.push(request),
            }
        }
        (commands, settings, requests)
    }

    /// The requests the controller has made, in order, ignoring any command.
    fn requests(controller: &mut Controller) -> Vec<Request> {
        controller
            .drain_work()
            .filter_map(|work| match work {
                Work::Request(request) => Some(request),
                Work::Command(_) => None,
            })
            .collect()
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

        let (commands, _, requests) = work(&mut controller);
        assert!(
            commands.is_empty(),
            "a pad in the picker must not touch the loops"
        );
        assert_eq!(requests, vec![Request::SaveSession(addr(2, 3))]);
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
    fn one_picker_is_left_before_the_other_opens() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);

        // The load button is not part of the save picker, so it does nothing there.
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        assert_eq!(controller.mode(), Mode::SavePicker);

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

    /// The pad that answers yes, and the one that answers no.
    fn yes() -> SlotAddr {
        addr(0, 0)
    }
    fn no() -> SlotAddr {
        addr(0, u8::try_from(SLOT_COUNT - 1).unwrap())
    }

    #[test]
    fn saving_over_a_session_asks_first() {
        let mut controller = controller();
        controller.set_sessions([addr(1, 1)]);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        press(&mut controller, addr(1, 1), T0);

        assert!(
            requests(&mut controller).into_iter().next().is_none(),
            "nothing is written until it is answered"
        );
        let frame = controller.take_frame().unwrap();
        assert!(frame.pad(yes()).is_lit() && frame.pad(no()).is_lit());
        assert!(
            SlotAddr::all()
                .filter(|a| *a != yes() && *a != no())
                .all(|a| !frame.pad(a).is_lit()),
            "and nothing else can be pressed by mistake"
        );
    }

    #[test]
    fn yes_writes_over_it_and_no_does_not() {
        let mut controller = controller();
        controller.set_sessions([addr(1, 1)]);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        press(&mut controller, addr(1, 1), T0);
        press(&mut controller, no(), T0);
        assert!(
            requests(&mut controller).into_iter().next().is_none(),
            "no means no"
        );
        assert_eq!(controller.mode(), Mode::Perform);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        press(&mut controller, addr(1, 1), T0);
        press(&mut controller, yes(), T0);
        assert_eq!(
            requests(&mut controller).into_iter().next(),
            Some(Request::SaveSession(addr(1, 1))),
            "and the pad asked about is the pad written"
        );
    }

    #[test]
    fn saving_onto_an_empty_pad_asks_nothing() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        press(&mut controller, addr(3, 3), T0);

        assert_eq!(
            requests(&mut controller).into_iter().next(),
            Some(Request::SaveSession(addr(3, 3))),
            "there is nothing to lose"
        );
    }

    #[test]
    fn loading_over_a_grid_that_holds_something_asks_first() {
        let mut controller = controller();
        controller.set_sessions([addr(1, 1)]);
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        press(&mut controller, addr(1, 1), T0);
        assert!(requests(&mut controller).into_iter().next().is_none());

        press(&mut controller, yes(), T0);
        assert_eq!(
            requests(&mut controller).into_iter().next(),
            Some(Request::LoadSession(addr(1, 1)))
        );
    }

    #[test]
    fn loading_onto_an_empty_grid_asks_nothing() {
        let mut controller = controller();
        controller.set_sessions([addr(1, 1)]);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        press(&mut controller, addr(1, 1), T0);

        assert_eq!(
            requests(&mut controller).into_iter().next(),
            Some(Request::LoadSession(addr(1, 1))),
            "nothing on the grid to lose"
        );
    }

    #[test]
    fn a_completed_save_leaves_the_picker_and_marks_the_session() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        controller.session_saved(addr(1, 1), T0);

        assert_eq!(controller.mode(), Mode::Perform);
        assert_eq!(controller.current_session(), Some(addr(1, 1)));
    }

    #[test]
    fn a_saved_session_turns_the_grid_green() {
        let mut controller = controller();
        controller.session_saved(addr(1, 1), T0);

        let frame = controller.take_frame().unwrap();
        assert!(
            SlotAddr::all().all(|a| frame.pad(a) == Led::solid(LedColor::Green)),
            "the whole grid answers"
        );
    }

    #[test]
    fn a_failed_save_turns_the_grid_red_and_leaves_the_picker() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::SaveSession), T0);
        controller.save_failed(T0);

        assert_eq!(controller.mode(), Mode::Perform);
        assert_eq!(controller.current_session(), None, "nothing was written");

        let frame = controller.take_frame().unwrap();
        assert!(SlotAddr::all().all(|a| frame.pad(a) == Led::solid(LedColor::Red)));
    }

    #[test]
    fn a_failed_load_scrolls_its_code_only_once_the_red_has_been_held() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.load_failed(T0, Some("2600".to_owned()));

        assert_eq!(controller.mode(), Mode::Perform);
        assert_eq!(controller.take_text(), None, "nothing over the answer yet");

        controller.tick(T0 + RESULT_FLASH);
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("2600".to_owned())),
            "the pool size the session needs"
        );
    }

    #[test]
    fn the_pickup_setting_cycles_through_its_degrees() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::SidePressed { index: 3 }, T0);
        let pad = SlotAddr::new(TrackId::new(2).unwrap(), SlotId::new(1).unwrap());
        let press = SurfaceEvent::PadPressed {
            addr: pad,
            velocity: 127,
        };

        // Off, then one degree per beat the tail can stand in for, then back.
        for beats in [1, 2, 3, 0] {
            controller.on_surface(press, T0);
            assert_eq!(controller.pickups()[2], beats);
        }
        assert_eq!(settings(&mut controller).pickups[2], 0);
    }

    #[test]
    fn a_refused_take_turns_the_grid_red() {
        let mut controller = controller();
        controller.on_engine(Event::RecordingRefused { addr: addr(3, 2) }, T0);

        let frame = controller.take_frame().unwrap();
        assert!(
            SlotAddr::all().all(|a| frame.pad(a) == Led::solid(LedColor::Red)),
            "the session is full, not that one pad"
        );
    }

    #[test]
    fn every_pad_refused_on_one_boundary_answers_once() {
        let mut controller = controller();
        for slot in 0..3 {
            controller.on_engine(
                Event::RecordingRefused {
                    addr: addr(0, slot),
                },
                T0,
            );
        }

        controller.take_frame();
        controller.tick(T0 + RESULT_FLASH);
        let frame = controller.take_frame().unwrap();
        assert!(
            SlotAddr::all().all(|a| !frame.pad(a).is_lit()),
            "one answer, not three in a row"
        );
    }

    #[test]
    fn a_lost_device_says_so_without_a_flash_first() {
        let mut controller = controller();
        controller.device_lost(T0);

        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("NO AUDIO".to_owned())),
            "the word is the whole answer"
        );
    }

    #[test]
    fn a_result_cuts_text_that_is_already_scrolling() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), T0);
        assert!(matches!(controller.take_text(), Some(TextUpdate::Show(_))));

        controller.session_saved(addr(0, 0), T0);
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Stop),
            "or the scroll would suppress every frame until it finished"
        );
    }

    #[test]
    fn a_failed_load_with_nothing_to_read_only_flashes() {
        let mut controller = controller();
        controller.load_failed(T0, None);

        assert_eq!(controller.take_text(), None);
        let frame = controller.take_frame().unwrap();
        assert!(SlotAddr::all().all(|a| frame.pad(a) == Led::solid(LedColor::Red)));
    }

    #[test]
    fn the_grid_comes_back_after_the_flash() {
        let mut controller = controller();
        controller.session_saved(addr(1, 1), T0);
        controller.take_frame();

        controller.tick(T0 + RESULT_FLASH / 2);
        assert!(controller.take_frame().is_none(), "still answering");

        controller.tick(T0 + RESULT_FLASH);
        let frame = controller.take_frame().unwrap();
        assert!(SlotAddr::all().all(|a| !frame.pad(a).is_lit()), "the loops");
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
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );
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
        let tap = |controller: &mut Controller| {
            controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
            controller.on_surface(SurfaceEvent::ControlReleased(Control::ClickToggle), T0);
        };

        tap(&mut controller);
        assert!(!controller.click_enabled());
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickEnabled(false)]
        );

        tap(&mut controller);
        assert!(controller.click_enabled());
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickEnabled(true)]
        );
    }

    #[test]
    fn holding_the_click_button_opens_its_page_without_toggling() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        controller.tick(T0 + CLICK_HOLD);

        let subdivision_row = u8::try_from(SUBDIVISION_ROW).unwrap();
        let frame = controller.take_frame().unwrap();
        assert!(
            frame.pad(addr(subdivision_row, 0)).is_lit(),
            "the page opened"
        );

        controller.on_surface(
            SurfaceEvent::ControlReleased(Control::ClickToggle),
            T0 + CLICK_HOLD,
        );
        assert!(controller.click_enabled(), "the click was left alone");
        assert!(commands(&mut controller).is_empty());
    }

    #[test]
    fn the_click_button_closes_its_own_page() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        controller.tick(T0 + CLICK_HOLD);
        controller.on_surface(
            SurfaceEvent::ControlReleased(Control::ClickToggle),
            T0 + CLICK_HOLD,
        );
        let _ = controller.take_frame();

        // A tap on the way out neither toggles the click nor leaves the page behind.
        let later = T0 + CLICK_HOLD * 2;
        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), later);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::ClickToggle), later);
        assert!(controller.click_enabled(), "still on");
        assert!(commands(&mut controller).is_empty());

        // Back on the loops, so a pad is a pad again.
        let pad = addr(0, 0);
        press(&mut controller, pad, later);
        controller.on_surface(SurfaceEvent::PadReleased { addr: pad }, later);
        assert_eq!(commands(&mut controller), vec![Command::Press(pad)]);
    }

    #[test]
    fn a_rate_the_bar_cannot_take_is_not_selectable() {
        let mut controller = controller();
        controller.set_loaded_time_signature(TimeSignature::new(3, 4).unwrap());
        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        controller.tick(T0 + CLICK_HOLD);
        controller.on_surface(
            SurfaceEvent::ControlReleased(Control::ClickToggle),
            T0 + CLICK_HOLD,
        );
        let _ = commands(&mut controller);

        // Halves do not divide a three beat bar.
        let row = u8::try_from(SUBDIVISION_ROW).unwrap();
        press(&mut controller, addr(row, 1), T0);
        assert!(
            commands(&mut controller).is_empty(),
            "nothing was asked for"
        );
        assert_eq!(controller.subdivision(), Subdivision::Quarter);

        // Eighths do.
        press(&mut controller, addr(row, 5), T0);
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickSubdivision(Subdivision::Eighth)]
        );
    }

    #[test]
    fn a_signature_that_breaks_the_rate_falls_back_to_the_quarter() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        controller.tick(T0 + CLICK_HOLD);
        controller.on_surface(
            SurfaceEvent::ControlReleased(Control::ClickToggle),
            T0 + CLICK_HOLD,
        );

        let row = u8::try_from(SUBDIVISION_ROW).unwrap();
        press(&mut controller, addr(row, 2), T0);
        assert_eq!(controller.subdivision(), Subdivision::HalfTriplet);
        let _ = commands(&mut controller);

        // Three beats cannot be cut into three halves.
        controller.set_loaded_time_signature(TimeSignature::new(3, 4).unwrap());
        assert_eq!(controller.subdivision(), Subdivision::Quarter);
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickSubdivision(Subdivision::Quarter)],
            "one click a beat, which a quarter is in 3/4"
        );
    }

    #[test]
    fn the_click_page_works_with_clips_on_the_grid() {
        let mut controller = controller();
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Stopped { clip: ClipId(0) },
            },
            T0,
        );
        controller.on_surface(SurfaceEvent::ControlPressed(Control::ClickToggle), T0);
        controller.tick(T0 + CLICK_HOLD);
        controller.on_surface(
            SurfaceEvent::ControlReleased(Control::ClickToggle),
            T0 + CLICK_HOLD,
        );
        let _ = commands(&mut controller);

        // Unlike the tempo and the signature, nothing here is bound to a recording.
        let subdivision_row = u8::try_from(SUBDIVISION_ROW).unwrap();
        press(&mut controller, addr(subdivision_row, 7), T0);

        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickSubdivision(Subdivision::Sixteenth)]
        );
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("1/16".to_owned()))
        );
    }

    #[test]
    fn tempo_moves_by_one_beat_per_press() {
        let mut controller = controller();
        let tap = |controller: &mut Controller, button| {
            controller.on_surface(SurfaceEvent::ControlPressed(button), T0);
            controller.on_surface(SurfaceEvent::ControlReleased(button), T0);
        };

        tap(&mut controller, Control::TempoUp);
        assert_eq!(controller.tempo(), 121.0);

        tap(&mut controller, Control::TempoDown);
        tap(&mut controller, Control::TempoDown);
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
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Stopped { clip: ClipId(0) },
            },
            T0,
        );

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
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Stopped { clip: ClipId(0) },
            },
            T0,
        );

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        for at in (0..2_000).step_by(20) {
            controller.tick(millis(at));
        }
        assert_eq!(controller.tempo(), 120.0);
    }

    #[test]
    fn a_tap_that_moves_nothing_says_nothing() {
        let mut controller = Controller::new(MAX_BPM, TimeSignature::FOUR_FOUR, true);
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

        controller.on_engine(Event::TempoRejected, T0);
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
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );
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
    fn both_tempo_buttons_open_the_signature_page_and_undo_the_nudge() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        assert_eq!(controller.tempo(), 121.0);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        assert_eq!(controller.tempo(), 120.0, "the nudge is put back");

        // What an engine ends up on, not just what the display says.
        let asked = commands(&mut controller);
        let tempo = asked.iter().rev().find_map(|command| match command {
            Command::SetTempo(tempo) => Some(tempo.bpm()),
            _ => None,
        });
        assert_eq!(
            tempo,
            Some(120.0),
            "the last word is the tempo it started on"
        );

        let frame = controller.take_frame().unwrap();
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        assert!(
            frame.pad(addr(beats_row, 0)).is_lit(),
            "the page is showing"
        );
    }

    #[test]
    fn a_nudge_already_sent_is_corrected_rather_than_left() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        // Drained, so the engine has the nudge and it cannot be taken back.
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetTempo(Tempo::new(121.0).unwrap())]
        );

        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetTempo(Tempo::new(120.0).unwrap())],
            "a correction follows it"
        );
    }

    /// Opens the signature page.
    fn open_signature(controller: &mut Controller) {
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoUp), T0);
        controller.on_surface(SurfaceEvent::ControlReleased(Control::TempoDown), T0);
    }

    #[test]
    fn each_number_takes_effect_as_it_is_pressed() {
        let mut controller = controller();
        open_signature(&mut controller);
        let _ = commands(&mut controller);
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        let unit_row = u8::try_from(UNIT_ROW).unwrap();

        // Three beats to the bar, then an eighth-note beat.
        press(&mut controller, addr(beats_row, 2), T0);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(3, 4).unwrap()
        );
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("3/4".to_owned()))
        );

        press(&mut controller, addr(unit_row, 2), T0);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(3, 8).unwrap()
        );
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("3/8".to_owned()))
        );

        assert_eq!(
            commands(&mut controller),
            vec![
                Command::SetTimeSignature(TimeSignature::new(3, 4).unwrap()),
                Command::SetTimeSignature(TimeSignature::new(3, 8).unwrap()),
            ],
            "each press is heard rather than held back"
        );

        // A quarter click cannot tile three eighths, but nothing moves until the engine
        // confirms which grid it ended up on.
        controller.on_engine(
            Event::TimeSignature {
                beats_per_bar: 3,
                beat_unit: 8,
            },
            T0,
        );
        assert_eq!(
            commands(&mut controller),
            vec![Command::SetClickSubdivision(Subdivision::Eighth)]
        );
    }

    #[test]
    fn the_same_gesture_closes_the_page() {
        let mut controller = controller();
        open_signature(&mut controller);
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 2), T0);

        open_signature(&mut controller);
        let _ = commands(&mut controller);

        // Back on the loops, so a pad is a pad again.
        press(&mut controller, addr(beats_row, 2), T0);
        controller.on_surface(
            SurfaceEvent::PadReleased {
                addr: addr(beats_row, 2),
            },
            T0,
        );
        assert_eq!(
            commands(&mut controller),
            vec![Command::Press(addr(beats_row, 2))]
        );
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(3, 4).unwrap(),
            "what was chosen stands"
        );
    }

    #[test]
    fn a_clip_landing_on_the_open_page_locks_it() {
        let mut controller = controller();
        open_signature(&mut controller);

        // A take sealing while the page is open is the only way to reach this.
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Stopped { clip: ClipId(0) },
            },
            T0,
        );
        let _ = commands(&mut controller);

        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 6), T0);

        assert_eq!(controller.time_signature(), TimeSignature::FOUR_FOUR);
        assert_eq!(
            controller.take_text(),
            Some(TextUpdate::Show("4/4".to_owned()))
        );
        assert!(
            commands(&mut controller).is_empty(),
            "nothing was asked for"
        );
    }

    #[test]
    fn the_page_does_not_open_once_a_clip_exists() {
        let mut controller = controller();
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Stopped { clip: ClipId(0) },
            },
            T0,
        );
        let _ = commands(&mut controller);
        let _ = controller.take_text();

        open_signature(&mut controller);
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 2), T0);
        controller.on_surface(
            SurfaceEvent::PadReleased {
                addr: addr(beats_row, 2),
            },
            T0,
        );

        assert_eq!(
            commands(&mut controller),
            vec![Command::Press(addr(beats_row, 2))],
            "the pad is still a loop, since the locked tempo only reported"
        );
    }

    #[test]
    fn a_dropped_signature_command_is_put_right_by_a_resync() {
        let mut controller = controller();
        open_signature(&mut controller);
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 6), T0);

        // The command never reached the engine, so the two disagree.
        let _ = commands(&mut controller);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(7, 4).unwrap()
        );

        // A resync answers with what the engine is actually running.
        controller.on_engine(
            Event::TimeSignature {
                beats_per_bar: 4,
                beat_unit: 4,
            },
            T0,
        );
        assert_eq!(
            controller.time_signature(),
            TimeSignature::FOUR_FOUR,
            "the engine's own value wins"
        );
    }

    #[test]
    fn a_refusal_that_never_arrives_is_still_put_right() {
        let mut controller = controller();
        controller.set_pickups([3; TRACK_COUNT]);
        open_signature(&mut controller);

        // Asking for a two beat bar would pull every pickup in to one.
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 1), T0);
        assert_eq!(
            controller.pickups(),
            [3; TRACK_COUNT],
            "nothing that depends on the signature moved yet"
        );

        // The refusal is lost, and the resync says the bar never changed.
        controller.on_engine(
            Event::TimeSignature {
                beats_per_bar: 4,
                beat_unit: 4,
            },
            T0,
        );
        assert_eq!(controller.time_signature(), TimeSignature::FOUR_FOUR);
        assert_eq!(
            controller.pickups(),
            [3; TRACK_COUNT],
            "and neither did they"
        );
    }

    #[test]
    fn two_presses_refused_both_land_back_on_the_engines_own_signature() {
        let mut controller = controller();
        open_signature(&mut controller);
        let beats_row = u8::try_from(BEATS_ROW).unwrap();

        // Two presses before either answer arrives.
        press(&mut controller, addr(beats_row, 2), T0);
        press(&mut controller, addr(beats_row, 6), T0);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(7, 4).unwrap()
        );

        // Both refused. The engine never left 4/4, so neither refusal may land on 3/4.
        controller.on_engine(Event::TimeSignatureRejected, T0);
        assert_eq!(controller.time_signature(), TimeSignature::FOUR_FOUR);
        controller.on_engine(Event::TimeSignatureRejected, T0);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::FOUR_FOUR,
            "the fallback is the last confirmed value, not the last requested one"
        );
    }

    #[test]
    fn a_refusal_after_a_narrower_bar_leaves_the_pickups_alone() {
        let mut controller = controller();
        controller.set_pickups([3; TRACK_COUNT]);
        open_signature(&mut controller);

        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 1), T0);
        controller.on_engine(Event::TimeSignatureRejected, T0);

        assert_eq!(controller.time_signature(), TimeSignature::FOUR_FOUR);
        assert_eq!(
            controller.pickups(),
            [3; TRACK_COUNT],
            "a refusal has only the display to undo"
        );
        assert_eq!(controller.subdivision(), Subdivision::Quarter);
    }

    #[test]
    fn a_refused_signature_change_is_rolled_back() {
        let mut controller = controller();
        controller.set_loaded_time_signature(TimeSignature::new(7, 8).unwrap());
        open_signature(&mut controller);
        let beats_row = u8::try_from(BEATS_ROW).unwrap();
        press(&mut controller, addr(beats_row, 2), T0);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(3, 8).unwrap()
        );

        controller.on_engine(Event::TimeSignatureRejected, T0);
        assert_eq!(
            controller.time_signature(),
            TimeSignature::new(7, 8).unwrap(),
            "back to what the engine still has"
        );
    }

    #[test]
    fn a_narrower_bar_pulls_a_pickup_in_with_it() {
        let mut controller = controller();
        controller.set_pickups([3; TRACK_COUNT]);

        controller.set_loaded_time_signature(TimeSignature::new(2, 4).unwrap());
        assert_eq!(
            controller.pickups(),
            [1; TRACK_COUNT],
            "two beats to the bar leaves one to open from"
        );
    }

    #[test]
    fn a_loaded_signature_is_what_a_later_save_records() {
        let mut controller = controller();
        let three_four = TimeSignature::new(3, 4).unwrap();

        controller.set_loaded_time_signature(three_four);
        assert_eq!(controller.time_signature(), three_four);
        assert!(
            commands(&mut controller).is_empty(),
            "the engine took it from the load itself"
        );
    }

    #[test]
    fn the_beat_flash_is_half_a_beat_whatever_the_unit_is() {
        // The tempo counts quarter notes, so at 120 bpm a quarter beat is 500 ms.
        let dark_at = |signature: Option<TimeSignature>, at: u64| {
            let mut c = controller();
            if let Some(signature) = signature {
                c.set_loaded_time_signature(signature);
            }
            c.on_engine(Event::Beat { bar: 0, beat: 1 }, T0);
            c.take_frame();
            c.tick(T0 + millis(at));
            c.take_frame().is_some()
        };

        assert!(!dark_at(None, 249), "still lit in 4/4");
        assert!(dark_at(None, 250), "dark by half a quarter beat");

        // An eighth-note beat is half as long, so its flash is half as long too.
        let six_eight = TimeSignature::new(6, 8).unwrap();
        assert!(!dark_at(Some(six_eight), 124), "still lit in 6/8");
        assert!(dark_at(Some(six_eight), 125), "dark by half an eighth beat");
    }

    #[test]
    fn a_loaded_signature_sets_how_far_a_pickup_can_reach() {
        let mut controller = controller();
        controller.set_loaded_time_signature(TimeSignature::new(3, 4).unwrap());
        controller.on_surface(side(SETTINGS_SIDE), T0);
        let column = u8::try_from(PICKUP_COLUMN).unwrap();
        let pad = SlotAddr::new(TrackId::new(2).unwrap(), SlotId::new(column).unwrap());
        let press = SurfaceEvent::PadPressed {
            addr: pad,
            velocity: 127,
        };

        // Two beats of tail in 3/4, where 4/4 reaches three.
        for beats in [1, 2, 0, 1] {
            controller.on_surface(press, T0);
            assert_eq!(controller.pickups()[2], beats);
        }
    }

    #[test]
    fn the_third_settings_column_toggles_a_track_between_single_and_multiple() {
        let mut controller = controller();
        controller.on_surface(side(SETTINGS_SIDE), T0);
        let column = u8::try_from(POLYPHONY_COLUMN).unwrap();
        let pad = SlotAddr::new(TrackId::new(2).unwrap(), SlotId::new(column).unwrap());
        let press = SurfaceEvent::PadPressed {
            addr: pad,
            velocity: 127,
        };

        assert_eq!(controller.polyphony()[2], Polyphony::Single, "single first");
        controller.on_surface(press, T0);
        assert_eq!(controller.polyphony()[2], Polyphony::Multiple);
        controller.on_surface(press, T0);
        assert_eq!(controller.polyphony()[2], Polyphony::Single);

        assert!(
            controller.polyphony()[3].is_exclusive(),
            "the other tracks are left alone"
        );
    }

    #[test]
    fn the_polyphony_a_track_is_on_reaches_the_engine() {
        let mut controller = controller();
        controller.on_surface(side(SETTINGS_SIDE), T0);
        let column = u8::try_from(POLYPHONY_COLUMN).unwrap();
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: SlotAddr::new(TrackId::new(5).unwrap(), SlotId::new(column).unwrap()),
                velocity: 127,
            },
            T0,
        );

        let settings = controller
            .drain_work()
            .filter_map(|work| match work {
                Work::Command(Command::SetSettings(settings)) => Some(settings),
                _ => None,
            })
            .next_back()
            .expect("the change was published");
        assert_eq!(settings.polyphony[5], Polyphony::Multiple);
    }

    #[test]
    fn a_pan_pad_puts_its_track_where_the_column_says() {
        let mut controller = controller();
        controller.on_surface(side(PAN_SIDE), T0);
        assert_eq!(controller.mode(), Mode::Pan);

        let press = |slot: u8| SurfaceEvent::PadPressed {
            addr: SlotAddr::new(TrackId::new(4).unwrap(), SlotId::new(slot).unwrap()),
            velocity: 127,
        };
        controller.on_surface(press(0), T0);
        assert_eq!(controller.pans()[4], 0, "hard left");
        controller.on_surface(press(6), T0);
        assert_eq!(controller.pans()[4], 6, "hard right");
        controller.on_surface(press(CENTRE_STEP), T0);
        assert_eq!(controller.pans()[4], CENTRE_STEP);
    }

    #[test]
    fn the_pad_past_the_pan_row_does_nothing() {
        let mut controller = controller();
        controller.on_surface(side(PAN_SIDE), T0);
        let last = u8::try_from(SLOT_COUNT - 1).unwrap();
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: SlotAddr::new(TrackId::new(0).unwrap(), SlotId::new(last).unwrap()),
                velocity: 127,
            },
            T0,
        );
        assert_eq!(
            controller.pans()[0],
            CENTRE_STEP,
            "the column past the row is not a pan"
        );
    }

    #[test]
    fn a_pan_reaches_the_engine() {
        let mut controller = controller();
        controller.on_surface(side(PAN_SIDE), T0);
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: SlotAddr::new(TrackId::new(2).unwrap(), SlotId::new(0).unwrap()),
                velocity: 127,
            },
            T0,
        );

        let settings = controller
            .drain_work()
            .filter_map(|work| match work {
                Work::Command(Command::SetSettings(settings)) => Some(settings),
                _ => None,
            })
            .next_back()
            .expect("the change was published");
        assert_eq!(settings.pans[2], 0);
    }

    #[test]
    fn a_hold_does_not_survive_leaving_the_input_page() {
        let mut controller = controller();
        controller.set_input_count(4);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);

        let first = hold_input(&mut controller, 0, 0);
        // Away and back, letting go while the page is not looking.
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);
        controller.on_surface(SurfaceEvent::PadReleased { addr: first }, T0);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);

        hold_input(&mut controller, 0, 2);
        assert_eq!(
            controller.inputs()[0],
            TrackInput::Mono(2),
            "a pad nobody is holding cannot be paired with"
        );
    }

    #[test]
    fn a_fresh_session_puts_every_track_back_to_its_defaults() {
        let mut controller = controller();
        let mut modes = [LaunchMode::Follow; TRACK_COUNT];
        modes[7] = LaunchMode::Restart;
        controller.set_launch_modes(modes);
        let mut inputs = [TrackInput::default(); TRACK_COUNT];
        inputs[7] = TrackInput::Mono(1);
        controller.set_inputs(inputs);
        let mut pickups = [0; TRACK_COUNT];
        pickups[7] = 2;
        controller.set_pickups(pickups);
        let mut polyphony = [Polyphony::Single; TRACK_COUNT];
        polyphony[7] = Polyphony::Multiple;
        controller.set_polyphony(polyphony);
        let mut pans = [CENTRE_STEP; TRACK_COUNT];
        pans[7] = 0;
        controller.set_pans(pans);
        let _ = commands(&mut controller);

        controller.on_surface(SurfaceEvent::ControlPressed(Control::LoadSession), T0);
        controller.on_surface(side(NEW_SIDE), T0);

        assert_eq!(controller.launch_modes(), [LaunchMode::Follow; TRACK_COUNT]);
        assert_eq!(controller.inputs(), [TrackInput::default(); TRACK_COUNT]);
        assert_eq!(controller.pickups(), [0; TRACK_COUNT]);
        assert_eq!(controller.polyphony(), [Polyphony::Single; TRACK_COUNT]);
        assert_eq!(controller.pans(), [CENTRE_STEP; TRACK_COUNT]);
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
        assert_eq!(settings(&mut controller).launch_modes, wanted);

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
        let channel = |c| SlotAddr::new(TrackId::new(0).unwrap(), SlotId::new(c).unwrap());
        assert!(frame.pad(channel(0)).is_lit(), "one column per channel");
        assert!(
            frame.pad(channel(1)).is_lit(),
            "and both of the pair are on"
        );
        assert!(
            !frame.pad(channel(2)).is_lit(),
            "a channel the device lacks is dark"
        );
    }

    #[test]
    fn a_tap_takes_one_channel_as_mono() {
        let mut controller = controller();
        controller.set_input_count(2);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);
        let _ = commands(&mut controller);

        let pad = SlotAddr::new(TrackId::new(3).unwrap(), SlotId::new(1).unwrap());
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: pad,
                velocity: 127,
            },
            T0,
        );

        let mut wanted = [TrackInput::default(); TRACK_COUNT];
        wanted[3] = TrackInput::Mono(1);
        assert_eq!(controller.inputs(), wanted, "a tap takes that one channel");
        assert_eq!(settings(&mut controller).inputs, wanted);
    }

    /// Presses `column` on `track`'s input row, leaving it down.
    fn hold_input(controller: &mut Controller, track: u8, column: u8) -> SlotAddr {
        let pad = SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(column).unwrap());
        controller.on_surface(
            SurfaceEvent::PadPressed {
                addr: pad,
                velocity: 127,
            },
            T0,
        );
        pad
    }

    #[test]
    fn holding_one_channel_and_tapping_another_makes_a_pair() {
        let mut controller = controller();
        controller.set_input_count(4);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);

        let first = hold_input(&mut controller, 3, 3);
        hold_input(&mut controller, 3, 1);

        assert_eq!(
            controller.inputs()[3],
            TrackInput::Pair(1, 3),
            "lower channel on the left, whichever was pressed first"
        );
        controller.on_surface(SurfaceEvent::PadReleased { addr: first }, T0);
    }

    #[test]
    fn a_tap_after_the_hold_is_let_go_replaces_the_pair() {
        let mut controller = controller();
        controller.set_input_count(4);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);

        let first = hold_input(&mut controller, 0, 0);
        hold_input(&mut controller, 0, 2);
        assert_eq!(controller.inputs()[0], TrackInput::Pair(0, 2));

        controller.on_surface(SurfaceEvent::PadReleased { addr: first }, T0);
        let second = hold_input(&mut controller, 0, 2);
        controller.on_surface(SurfaceEvent::PadReleased { addr: second }, T0);

        assert_eq!(
            controller.inputs()[0],
            TrackInput::Mono(2),
            "a press on its own is one channel again"
        );
    }

    #[test]
    fn a_pair_cannot_reach_across_tracks() {
        let mut controller = controller();
        controller.set_input_count(4);
        controller.on_surface(SurfaceEvent::SidePressed { index: 2 }, T0);

        hold_input(&mut controller, 0, 0);
        hold_input(&mut controller, 1, 3);

        assert_eq!(controller.inputs()[0], TrackInput::Mono(0));
        assert_eq!(controller.inputs()[1], TrackInput::Mono(3), "its own row");
    }

    #[test]
    fn a_channel_the_device_lacks_is_ignored() {
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

        assert_eq!(controller.inputs(), [TrackInput::default(); TRACK_COUNT]);
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
        let mut controller = Controller::new(MAX_BPM - 2.0, TimeSignature::FOUR_FOUR, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        for at in (0..2_000).step_by(20) {
            controller.tick(millis(at));
        }
        assert_eq!(controller.tempo(), MAX_BPM);
    }

    #[test]
    fn tempo_stops_at_the_supported_range() {
        let mut controller = Controller::new(MAX_BPM, TimeSignature::FOUR_FOUR, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        assert_eq!(controller.tempo(), MAX_BPM);
        assert!(
            commands(&mut controller).is_empty(),
            "a change that moves nothing should not be sent"
        );

        let mut controller = Controller::new(MIN_BPM, TimeSignature::FOUR_FOUR, true);
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoDown), T0);
        assert_eq!(controller.tempo(), MIN_BPM);
    }

    #[test]
    fn a_refused_tempo_change_is_rolled_back() {
        let mut controller = controller();
        controller.on_surface(SurfaceEvent::ControlPressed(Control::TempoUp), T0);
        assert_eq!(controller.tempo(), 121.0, "assumed to land");

        controller.on_engine(Event::TempoRejected, T0);
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

        controller.on_engine(
            Event::SlotChanged {
                addr: addr(1, 2),
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );

        let frame = controller.take_frame().expect("the grid changed");
        assert_eq!(frame.pad(addr(1, 2)), Led::pulse(LedColor::Green));
    }

    #[test]
    fn the_frame_is_only_produced_when_something_changed() {
        let mut controller = controller();
        assert!(controller.take_frame().is_some(), "the first frame is new");
        assert!(controller.take_frame().is_none());

        controller.on_engine(Event::Xrun { frames: 128 }, T0);
        assert!(
            controller.take_frame().is_none(),
            "an xrun changes nothing on the grid"
        );

        controller.on_engine(Event::Beat { bar: 0, beat: 1 }, T0);
        assert!(controller.take_frame().is_some());
    }

    #[test]
    fn repeating_the_same_beat_does_not_force_a_repaint() {
        let mut controller = controller();
        controller.take_frame();
        controller.on_engine(Event::Beat { bar: 0, beat: 0 }, T0);
        assert!(controller.take_frame().is_none());
    }

    #[test]
    fn the_beat_indicator_follows_the_transport() {
        use free_loop_surface::FIRST_BEAT_LED;

        let mut controller = controller();
        controller.on_engine(Event::Beat { bar: 3, beat: 2 }, T0);

        let frame = controller.take_frame().unwrap();
        assert_eq!(
            frame.control(FIRST_BEAT_LED),
            Led::solid(LedColor::White),
            "off the downbeat"
        );

        controller.on_engine(Event::Beat { bar: 4, beat: 0 }, T0);
        let frame = controller.take_frame().unwrap();
        assert_eq!(
            frame.control(FIRST_BEAT_LED),
            Led::solid(LedColor::Red),
            "the downbeat"
        );
    }

    #[test]
    fn the_beat_goes_dark_between_beats_and_lights_again_on_the_next() {
        use free_loop_surface::FIRST_BEAT_LED;

        let mut controller = controller();
        controller.on_engine(Event::Beat { bar: 0, beat: 1 }, T0);
        controller.take_frame();

        // Half a beat at 120 bpm.
        controller.tick(T0 + millis(249));
        assert!(
            controller.take_frame().is_none(),
            "still lit up to the halfway point"
        );

        controller.tick(T0 + millis(250));
        let dark = controller.take_frame().unwrap();
        assert_eq!(
            dark.control(FIRST_BEAT_LED),
            paint::control(Control::TempoUp, Chrome::default()),
            "the button it shares gets its own colour back"
        );

        controller.on_engine(Event::Beat { bar: 0, beat: 2 }, T0 + millis(500));
        assert_eq!(
            controller.take_frame().unwrap().control(FIRST_BEAT_LED),
            Led::solid(LedColor::White),
            "lit again on the next beat"
        );
    }

    #[test]
    fn a_recording_pad_shows_red_before_any_clip_exists() {
        let mut controller = controller();
        controller.on_engine(
            Event::SlotChanged {
                addr: addr(0, 0),
                state: SlotState::Recording {
                    started_at: Frames(0),
                    ends_at: None,
                },
            },
            T0,
        );

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
        controller.on_engine(
            Event::SlotChanged {
                addr: pad,
                state: SlotState::Playing { clip: ClipId(0) },
            },
            T0,
        );
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
