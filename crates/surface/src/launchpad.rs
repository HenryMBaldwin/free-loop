//! The Launchpad X.
//!
//! Layout: columns are tracks, rows are slots, matching Session view. The right-hand
//! column is reserved for scene launch. The top row carries the transport controls on
//! the left and the beat indicator on the right.
//!
//! The device must be in Programmer layout for the full 9×9 grid to address, which
//! [`LaunchpadX::connect`] sets.

use free_loop_core::{SlotAddr, SlotId, TrackId};
use launchy::x;
use launchy::{InputDevice as _, MsgPollingWrapper as _, OutputDevice as _};

use crate::event::{Control, SurfaceEvent};
use crate::led::{CONTROL_COUNT, Led, LedColor, LedFrame, LedStyle};
use crate::surface::{ControlSurface, SurfaceError};

/// Buttons the device accepts in one update.
const MAX_BUTTONS: usize = 80;

/// Fraction of full brightness used for [`LedStyle::Dim`].
const DIM_DIVISOR: u16 = 4;

/// Highest value a Launchpad X RGB channel takes.
const RGB_MAX: u16 = 127;

fn convert(error: launchy::MidiError) -> SurfaceError {
    match error {
        launchy::MidiError::NoPortFound { .. } => SurfaceError::NotFound,
        other => SurfaceError::Device(other.to_string()),
    }
}

/// What a physical button stands for.
enum Target {
    Pad(SlotAddr),
    Scene(SlotId),
    Control(Control),
}

fn target(button: x::Button) -> Option<Target> {
    match button {
        // Launchy counts the right-hand column as grid column 8.
        x::Button::GridButton { x: 8, y } => SlotId::new(y).ok().map(Target::Scene),
        x::Button::GridButton { x, y } => {
            let track = TrackId::new(x).ok()?;
            let slot = SlotId::new(y).ok()?;
            Some(Target::Pad(SlotAddr::new(track, slot)))
        }
        x::Button::ControlButton { index } => {
            Control::from_index(usize::from(index)).map(Target::Control)
        }
    }
}

fn pad_button(addr: SlotAddr) -> x::Button {
    x::Button::GridButton {
        x: u8::try_from(addr.track.index()).unwrap_or(0),
        y: u8::try_from(addr.slot.index()).unwrap_or(0),
    }
}

fn scene_button(slot: SlotId) -> x::Button {
    x::Button::GridButton {
        x: 8,
        y: u8::try_from(slot.index()).unwrap_or(0),
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
    }
}

/// Scales a 0–255 channel onto the device's 0–127 range at reduced brightness.
fn dim_channel(value: u8) -> u8 {
    let scaled = u16::from(value) * RGB_MAX / 255 / DIM_DIVISOR;
    u8::try_from(scaled).unwrap_or(0)
}

fn style(led: Led) -> x::ButtonStyle {
    match led.style {
        LedStyle::Solid => x::ButtonStyle::palette(palette(led.color)),
        LedStyle::Flash => x::ButtonStyle::flash(palette(led.color)),
        LedStyle::Pulse => x::ButtonStyle::pulse(palette(led.color)),
        // The device only flashes and pulses palette entries, so dimming has to go
        // through RGB, where brightness is ours to pick.
        LedStyle::Dim => {
            let (r, g, b) = led.color.rgb();
            x::ButtonStyle::rgb(x::RgbColor::new(
                dim_channel(r),
                dim_channel(g),
                dim_channel(b),
            ))
        }
    }
}

/// A connected Launchpad X.
pub struct LaunchpadX {
    output: x::Output,
    input: launchy::InputDeviceHandlerPolling<x::Message>,
    /// What the device is currently showing, so renders can send only what changed.
    shown: LedFrame,
    changes: Vec<(x::Button, x::ButtonStyle)>,
}

impl core::fmt::Debug for LaunchpadX {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LaunchpadX").finish_non_exhaustive()
    }
}

impl LaunchpadX {
    /// Finds a Launchpad X, puts it in Programmer layout and darkens it.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::NotFound`] if no device is attached, [`SurfaceError::Device`] if
    /// one is but will not talk.
    pub fn connect() -> Result<Self, SurfaceError> {
        let mut output = x::Output::guess().map_err(convert)?;
        output
            .change_layout(x::Layout::Programmer)
            .map_err(convert)?;
        output.clear().map_err(convert)?;

        let input = x::Input::guess_polling().map_err(convert)?;

        Ok(Self {
            output,
            input,
            shown: LedFrame::new(),
            changes: Vec::with_capacity(MAX_BUTTONS),
        })
    }

    fn collect_changes(&mut self, frame: &LedFrame) {
        self.changes.clear();

        for addr in SlotAddr::all() {
            let led = frame.pad(addr);
            if led != self.shown.pad(addr) {
                self.changes.push((pad_button(addr), style(led)));
            }
        }
        for slot in SlotId::all() {
            let led = frame.scene(slot);
            if led != self.shown.scene(slot) {
                self.changes.push((scene_button(slot), style(led)));
            }
        }
        for index in 0..CONTROL_COUNT {
            let led = frame.control(index);
            if led != self.shown.control(index) {
                self.changes.push((control_button(index), style(led)));
            }
        }
    }
}

impl ControlSurface for LaunchpadX {
    fn poll(&mut self, events: &mut Vec<SurfaceEvent>) {
        while let Some(message) = self.input.try_recv() {
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
                (Target::Scene(slot), true) => SurfaceEvent::ScenePressed { slot },
                (Target::Scene(slot), false) => SurfaceEvent::SceneReleased { slot },
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
        Ok(())
    }

    fn clear(&mut self) -> Result<(), SurfaceError> {
        self.output.clear().map_err(convert)?;
        self.shown = LedFrame::new();
        Ok(())
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
    fn pads_map_columns_to_tracks_and_rows_to_slots() {
        // Track 3, slot 5 is the fourth column, sixth row from the top.
        assert_eq!(pad_button(addr(3, 5)), x::Button::GridButton { x: 3, y: 5 });
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
    fn the_right_hand_column_is_a_scene_not_a_pad() {
        for expected in SlotId::all() {
            match target(scene_button(expected)) {
                Some(Target::Scene(actual)) => assert_eq!(actual, expected),
                _ => panic!("{expected:?} did not come back as a scene"),
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
        // The rest are the beat indicator, which is output only.
        for index in 4..CONTROL_COUNT {
            assert!(target(control_button(index)).is_none());
        }
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
        assert_eq!(dim_channel(0), 0);
        let full = dim_channel(255);
        assert!(full > 0 && full < u8::try_from(RGB_MAX).unwrap());
    }
}
