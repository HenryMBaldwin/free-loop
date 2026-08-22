//! The Launchpad X.
//!
//! Layout: rows are tracks, columns are slots. The right-hand column is a separate strip
//! of eight buttons with their own printed labels, not part of the grid. The top row
//! carries the transport and session controls, with the beat indicator sharing its first
//! four buttons.
//!
//! The device must be in Programmer layout for the full 9×9 grid to address, which
//! [`LaunchpadX::connect`] sets.

use core::time::Duration;

use free_loop_core::{SlotAddr, SlotId, TrackId};
use launchy::x;
use launchy::{InputDevice as _, MsgPollingWrapper as _, OutputDevice as _};

use crate::event::{Control, SurfaceEvent};
use crate::led::{CONTROL_COUNT, Led, LedColor, LedFrame, LedStyle, SHADES, SIDE_COUNT};
use crate::surface::{ControlSurface, SurfaceError};

/// Buttons the device accepts in one update.
/// Name the port probe registers under. Never connects, so nothing sees it.
const PROBE_NAME: &str = "free-loop probe";

/// Name the connections register under.
const CLIENT_NAME: &str = "free-loop";

/// How long a device gets to answer the inquiry sent when opening it.
const HANDSHAKE: Duration = Duration::from_millis(300);

/// How often the device is asked to identify itself once open.
const HEARTBEAT: Duration = Duration::from_millis(500);

/// Unanswered inquiries before the device counts as gone.
const MISSES_ALLOWED: u32 = 3;

const MAX_BUTTONS: usize = 80;

/// Scroll speed the device accepts, 0 to 127. Fast enough to read a three digit number
/// without waiting for it.
const TEXT_SPEED: u8 = 24;

/// Highest value a Launchpad X RGB channel takes.
const RGB_MAX: u16 = 127;

/// Anything the host refused, other than there being nothing to connect to.
fn device(error: impl core::fmt::Display) -> SurfaceError {
    SurfaceError::Device(error.to_string())
}

fn convert(error: launchy::MidiError) -> SurfaceError {
    match error {
        launchy::MidiError::NoPortFound { .. } => SurfaceError::NotFound,
        other => SurfaceError::Device(other.to_string()),
    }
}

/// What a physical button stands for.
enum Target {
    Pad(SlotAddr),
    Side(u8),
    Control(Control),
}

fn target(button: x::Button) -> Option<Target> {
    match button {
        // Launchy counts the right-hand column as grid column 8.
        x::Button::GridButton { x: 8, y } => Some(Target::Side(y)),
        x::Button::GridButton { x, y } => {
            let track = TrackId::new(y).ok()?;
            let slot = SlotId::new(x).ok()?;
            Some(Target::Pad(SlotAddr::new(track, slot)))
        }
        x::Button::ControlButton { index } => {
            Control::from_index(usize::from(index)).map(Target::Control)
        }
    }
}

fn pad_button(addr: SlotAddr) -> x::Button {
    x::Button::GridButton {
        x: u8::try_from(addr.slot.index()).unwrap_or(0),
        y: u8::try_from(addr.track.index()).unwrap_or(0),
    }
}

fn side_button(index: usize) -> x::Button {
    x::Button::GridButton {
        x: 8,
        y: u8::try_from(index).unwrap_or(0),
    }
}

fn control_button(index: usize) -> x::Button {
    x::Button::ControlButton {
        index: u8::try_from(index).unwrap_or(0),
    }
}

fn palette(color: LedColor) -> x::PaletteColor {
    match color {
        LedColor::Off => x::PaletteColor::BLACK,
        LedColor::White => x::PaletteColor::WHITE,
        LedColor::Red => x::PaletteColor::RED,
        LedColor::Amber => x::PaletteColor::ORANGE,
        LedColor::Green => x::PaletteColor::GREEN,
        LedColor::Blue => x::PaletteColor::BLUE,
        LedColor::Purple => x::PaletteColor::PURPLE,
        LedColor::Pink => x::PaletteColor::PINK,
    }
}

/// Scales a 0–255 channel onto the device's 0–127 range at step `step` of [`SHADES`].
fn shade_channel(value: u8, step: u8) -> u8 {
    let step = u16::from(step.clamp(1, SHADES));
    let scaled = u16::from(value) * RGB_MAX / 255 * step / u16::from(SHADES);
    u8::try_from(scaled).unwrap_or(0)
}

/// A colour at step `step` of [`SHADES`], clamped to the range the device takes.
fn shaded(color: LedColor, step: u8) -> x::ButtonStyle {
    let (r, g, b) = color.rgb();
    x::ButtonStyle::rgb(x::RgbColor::new(
        shade_channel(r, step),
        shade_channel(g, step),
        shade_channel(b, step),
    ))
}

fn style(led: Led) -> x::ButtonStyle {
    match led.style {
        LedStyle::Solid => x::ButtonStyle::palette(palette(led.color)),
        LedStyle::Flash => x::ButtonStyle::flash(palette(led.color)),
        LedStyle::Pulse => x::ButtonStyle::pulse(palette(led.color)),
        // The device only flashes and pulses palette entries, so anything below full
        // brightness has to go through RGB, where it can be set directly.
        LedStyle::Dim => shaded(led.color, 1),
        LedStyle::Shade(step) => shaded(led.color, step),
    }
}

/// A connected Launchpad X.
pub struct LaunchpadX {
    output: x::Output,
    input: launchy::InputDeviceHandlerPolling<x::Message>,
    /// What the device is currently showing, so renders can send only what changed.
    shown: LedFrame,
    /// Set when something other than a render has touched the device, so the next one
    /// sends every button rather than trusting `shown`.
    stale: bool,
    /// Gestures taken off the input while looking for a reply.
    pending: Vec<x::Message>,
    /// Inquiries sent with no answer yet. Reset by any reply.
    unanswered: u32,
    /// When to ask again.
    next_ask: Duration,
    changes: Vec<(x::Button, x::ButtonStyle)>,
}

impl core::fmt::Debug for LaunchpadX {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LaunchpadX").finish_non_exhaustive()
    }
}

/// Every listed port `wanted` accepts, in the order listed.
fn matching_outputs(
    midi: &midir::MidiOutput,
    wanted: &dyn Fn(&str) -> bool,
) -> Vec<midir::MidiOutputPort> {
    midi.ports()
        .into_iter()
        .filter(|port| midi.port_name(port).ok().is_some_and(|name| wanted(&name)))
        .collect()
}

/// The input counterpart of [`matching_outputs`].
fn matching_inputs(
    midi: &midir::MidiInput,
    wanted: &dyn Fn(&str) -> bool,
) -> Vec<midir::MidiInputPort> {
    midi.ports()
        .into_iter()
        .filter(|port| midi.port_name(port).ok().is_some_and(|name| wanted(&name)))
        .collect()
}

/// Every MIDI output port the host can see, for working out why none matched.
pub fn output_ports() -> Vec<String> {
    let Ok(midi) = midir::MidiOutput::new(PROBE_NAME) else {
        return Vec::new();
    };
    midi.ports()
        .iter()
        .filter_map(|port| midi.port_name(port).ok())
        .collect()
}

impl LaunchpadX {
    /// The name fragment a port has to contain to be taken for a Launchpad X.
    pub const PORT_KEYWORD: &'static str = x::Output::MIDI_DEVICE_KEYWORD;

    /// Finds a Launchpad X, puts it in Programmer layout and darkens it.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::NotFound`] if no device is attached, [`SurfaceError::Device`] if
    /// one is but will not talk.
    pub fn connect() -> Result<Self, SurfaceError> {
        Self::connect_matching(&|name| name.contains(Self::PORT_KEYWORD))
    }

    /// Finds a Launchpad X on the port named exactly `name`, which an emulator needs:
    /// it publishes a port a keyword match cannot tell from the hardware's.
    ///
    /// # Errors
    ///
    /// As [`LaunchpadX::connect`].
    pub fn connect_to(name: &str) -> Result<Self, SurfaceError> {
        Self::connect_matching(&|found| found == name)
    }

    fn connect_matching(wanted: &dyn Fn(&str) -> bool) -> Result<Self, SurfaceError> {
        let midi_out = midir::MidiOutput::new(CLIENT_NAME).map_err(device)?;
        let midi_in = midir::MidiInput::new(CLIENT_NAME).map_err(device)?;
        let outputs = matching_outputs(&midi_out, wanted);
        let inputs = matching_inputs(&midi_in, wanted);

        if outputs.is_empty() || inputs.is_empty() {
            return Err(SurfaceError::NotFound);
        }

        // Newest first: a port left behind by a host that exited without disposing it
        // stays listed and accepts writes, so the only way past one is to ask.
        let mut last = SurfaceError::NotFound;
        for (out_port, in_port) in outputs.iter().zip(&inputs).rev() {
            match Self::open(out_port, in_port) {
                Ok(device) => return Ok(device),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    /// Opens one port pair and requires the device to identify itself.
    fn open(
        out_port: &midir::MidiOutputPort,
        in_port: &midir::MidiInputPort,
    ) -> Result<Self, SurfaceError> {
        let midi_out = midir::MidiOutput::new(CLIENT_NAME).map_err(device)?;
        let midi_in = midir::MidiInput::new(CLIENT_NAME).map_err(device)?;

        let connection = midi_out.connect(out_port, CLIENT_NAME).map_err(device)?;
        let output = x::Output::from_connection(connection).map_err(convert)?;
        let input = x::Input::from_port_polling(midi_in, in_port).map_err(convert)?;

        let mut opened = Self {
            output,
            input,
            pending: Vec::new(),
            unanswered: 0,
            next_ask: Duration::ZERO,
            shown: LedFrame::new(),
            stale: false,
            changes: Vec::with_capacity(MAX_BUTTONS),
        };
        opened.handshake()?;

        opened
            .output
            .change_layout(x::Layout::Programmer)
            .map_err(convert)?;
        opened.output.clear().map_err(convert)?;
        Ok(opened)
    }

    /// Asks the device to identify itself and waits for the answer.
    ///
    /// A port whose host has gone accepts the question without error and never answers, so
    /// this is what separates a device from one that is only still listed.
    fn handshake(&mut self) -> Result<(), SurfaceError> {
        self.output
            .request_device_inquiry(x::DeviceIdQuery::Any)
            .map_err(convert)?;

        let deadline = std::time::Instant::now() + HANDSHAKE;
        while std::time::Instant::now() < deadline {
            if self.take_replies() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(SurfaceError::NotFound)
    }

    /// Drains the input, reporting whether the device identified itself.
    ///
    /// Gestures are kept for the next [`ControlSurface::poll`].
    fn take_replies(&mut self) -> bool {
        let mut answered = false;
        while let Some(message) = self.input.try_recv() {
            match message {
                x::Message::ApplicationVersion(_) | x::Message::BootloaderVersion(_) => {
                    answered = true;
                }
                other => self.pending.push(other),
            }
        }
        if answered {
            self.unanswered = 0;
        }
        answered
    }

    /// Whether the device has answered recently enough.
    fn answering(&self) -> bool {
        self.unanswered < MISSES_ALLOWED
    }

    fn collect_changes(&mut self, frame: &LedFrame) {
        diff(&self.shown, frame, self.stale, &mut self.changes);
    }
}

/// Collects the buttons that need sending.
///
/// `stale` sends every button, which darkens anything left behind by a write outside a
/// render.
fn diff(
    shown: &LedFrame,
    frame: &LedFrame,
    stale: bool,
    out: &mut Vec<(x::Button, x::ButtonStyle)>,
) {
    out.clear();

    for addr in SlotAddr::all() {
        let led = frame.pad(addr);
        if stale || led != shown.pad(addr) {
            out.push((pad_button(addr), style(led)));
        }
    }
    for index in 0..SIDE_COUNT {
        let led = frame.side(index);
        if stale || led != shown.side(index) {
            out.push((side_button(index), style(led)));
        }
    }
    for index in 0..CONTROL_COUNT {
        let led = frame.control(index);
        if stale || led != shown.control(index) {
            out.push((control_button(index), style(led)));
        }
    }
}

impl ControlSurface for LaunchpadX {
    fn tick(&mut self, now: Duration) {
        self.take_replies();
        if now < self.next_ask {
            return;
        }
        self.next_ask = now + HEARTBEAT;

        // A send that fails is its own answer.
        if self
            .output
            .request_device_inquiry(x::DeviceIdQuery::Any)
            .is_err()
        {
            self.unanswered = MISSES_ALLOWED;
            return;
        }
        self.unanswered += 1;
    }

    fn is_present(&self) -> bool {
        self.answering()
    }

    fn poll(&mut self, events: &mut Vec<SurfaceEvent>) {
        self.take_replies();
        for message in core::mem::take(&mut self.pending) {
            let (button, pressed, velocity) = match message {
                x::Message::Press { button, velocity } => (button, true, velocity),
                x::Message::Release { button } => (button, false, 0),
                // Aftertouch and the device's replies to queries are not gestures.
                _ => continue,
            };

            let Some(target) = target(button) else {
                continue;
            };
            events.push(match (target, pressed) {
                (Target::Pad(addr), true) => SurfaceEvent::PadPressed { addr, velocity },
                (Target::Pad(addr), false) => SurfaceEvent::PadReleased { addr },
                (Target::Side(index), true) => SurfaceEvent::SidePressed { index },
                (Target::Side(index), false) => SurfaceEvent::SideReleased { index },
                (Target::Control(control), true) => SurfaceEvent::ControlPressed(control),
                (Target::Control(control), false) => SurfaceEvent::ControlReleased(control),
            });
        }
    }

    fn render(&mut self, frame: &LedFrame) -> Result<(), SurfaceError> {
        self.collect_changes(frame);
        if self.changes.is_empty() {
            return Ok(());
        }

        self.output.set_buttons(&self.changes).map_err(convert)?;
        self.shown = *frame;
        self.stale = false;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), SurfaceError> {
        self.output.clear().map_err(convert)?;
        self.shown = LedFrame::new();
        self.stale = false;
        Ok(())
    }

    fn send_clock(&mut self, ticks: u32) -> Result<(), SurfaceError> {
        // The device locks its flash and pulse animations to this. Without it they run
        // at whatever it last assumed, which is only right by accident.
        for _ in 0..ticks {
            self.output.send_clock_tick().map_err(convert)?;
        }
        Ok(())
    }

    fn show_text(&mut self, text: &str) -> Result<(), SurfaceError> {
        // Text takes the grid over, so nothing `shown` says about it holds any more.
        self.stale = true;
        self.output
            .scroll_text(text.as_bytes(), palette(LedColor::White), TEXT_SPEED, false)
            .map_err(convert)
    }

    fn stop_text(&mut self) -> Result<(), SurfaceError> {
        self.stale = true;
        self.output.stop_scroll().map_err(convert)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "tests should fail loudly"
    )]

    use super::*;

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    #[test]
    fn pads_map_rows_to_tracks_and_columns_to_slots() {
        // Track 3, slot 5 is the sixth column of the fourth row from the top.
        assert_eq!(pad_button(addr(3, 5)), x::Button::GridButton { x: 5, y: 3 });
    }

    #[test]
    fn every_pad_round_trips_through_the_device_layout() {
        for expected in SlotAddr::all() {
            match target(pad_button(expected)) {
                Some(Target::Pad(actual)) => assert_eq!(actual, expected),
                _ => panic!("{expected:?} did not come back as a pad"),
            }
        }
    }

    #[test]
    fn the_right_hand_column_is_a_side_button_not_a_pad() {
        for expected in 0..SIDE_COUNT {
            match target(side_button(expected)) {
                Some(Target::Side(actual)) => {
                    assert_eq!(usize::from(actual), expected);
                }
                _ => panic!("side {expected} did not come back as a side button"),
            }
        }
    }

    #[test]
    fn only_the_first_four_top_row_buttons_are_controls() {
        for control in Control::all() {
            match target(control_button(control.index())) {
                Some(Target::Control(actual)) => assert_eq!(actual, control),
                _ => panic!("{control:?} did not come back as a control"),
            }
        }
    }

    #[test]
    fn a_diff_sends_only_what_moved() {
        let shown = LedFrame::new();
        let mut frame = LedFrame::new();
        frame.set_pad(addr(2, 2), Led::solid(LedColor::Red));

        let mut out = Vec::new();
        diff(&shown, &frame, false, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn an_unchanged_frame_sends_nothing() {
        let frame = LedFrame::new();
        let mut out = Vec::new();
        diff(&frame, &frame, false, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_stale_device_is_sent_every_button() {
        let frame = LedFrame::new();
        let mut out = Vec::new();

        // Identical frames, so without the flag nothing would be sent and whatever the
        // device is actually showing would stay there.
        diff(&frame, &frame, true, &mut out);
        assert_eq!(out.len(), MAX_BUTTONS);
    }

    #[test]
    fn dim_uses_rgb_because_the_device_only_pulses_palette_entries() {
        assert!(matches!(
            style(Led::dim(LedColor::Amber)),
            x::ButtonStyle::Rgb { .. }
        ));
        assert!(matches!(
            style(Led::pulse(LedColor::Green)),
            x::ButtonStyle::Pulse { .. }
        ));
        assert!(matches!(
            style(Led::flash(LedColor::Red)),
            x::ButtonStyle::Flash { .. }
        ));
        assert!(matches!(
            style(Led::solid(LedColor::White)),
            x::ButtonStyle::Palette { .. }
        ));
    }

    #[test]
    fn every_style_the_device_is_sent_is_one_it_accepts() {
        for color in [
            LedColor::Off,
            LedColor::White,
            LedColor::Red,
            LedColor::Amber,
            LedColor::Green,
            LedColor::Blue,
            LedColor::Purple,
            LedColor::Pink,
        ] {
            for make in [Led::solid, Led::dim, Led::flash, Led::pulse] {
                assert!(
                    style(make(color)).is_valid(),
                    "{color:?} produced a style the device would reject"
                );
            }
        }
    }

    #[test]
    fn dimming_stays_inside_the_devices_range_and_below_full() {
        assert_eq!(shade_channel(0, 1), 0);
        let full = shade_channel(255, 1);
        assert!(full > 0 && full < u8::try_from(RGB_MAX).unwrap());
    }

    #[test]
    fn listing_ports_works_wherever_it_runs() {
        // A host with no midi at all must be a value, not a panic.
        println!("midi outputs: {:?}", output_ports());
    }

    #[test]
    fn repeated_connects() {
        for i in 0..5 {
            match LaunchpadX::connect() {
                Ok(device) => {
                    println!("{i}: ok, present={}", device.answering());
                    drop(device);
                }
                Err(error) => println!("{i}: FAILED {error}"),
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}
