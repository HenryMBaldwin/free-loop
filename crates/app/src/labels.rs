//! What each button does, for the on-screen surface to name.
//!
//! Read from [`crate::screen`], so a name says what a press will actually do.

use free_loop_core::{SLOT_COUNT, SlotAddr, SlotId, TRACK_COUNT, TrackId};
use free_loop_surface::{Control, SIDE_COUNT};

use crate::control::Mode;
use crate::screen::{self, Button};
use launchpad_emulator::Pad;
use launchpad_emulator_ui::Labels;

/// Where the emulator shows the grid pad on `track` and `slot`, below its row zero of
/// controls.
fn grid_pad(track: usize, slot: usize) -> Pad {
    Pad::new(as_u8(slot), as_u8(track) + 1)
}

/// Where the emulator shows the right-hand button at `index`.
fn side_pad(index: usize) -> Pad {
    Pad::new(as_u8(SLOT_COUNT), as_u8(index) + 1)
}

/// Where the emulator shows the top-row button at `index`.
fn control_pad(index: usize) -> Pad {
    Pad::new(as_u8(index), 0)
}

fn as_u8(value: usize) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// Every button named by what it does on `mode`, with the rest left blank.
pub fn for_mode(mode: Mode) -> Labels {
    let named = |button: Button, pad: Pad| screen::name(mode, button).map(|name| (pad, name));

    let controls = Control::all()
        .filter_map(|control| named(Button::Top(control), control_pad(control.index())));
    let sides = (0..SIDE_COUNT).filter_map(|index| named(Button::Side(index), side_pad(index)));
    let pads = (0..TRACK_COUNT).flat_map(move |track| {
        (0..SLOT_COUNT).filter_map(move |slot| {
            let addr = SlotAddr::new(
                TrackId::new(as_u8(track)).ok()?,
                SlotId::new(as_u8(slot)).ok()?,
            );
            named(Button::Grid(addr), grid_pad(track, slot))
        })
    });

    Labels::none()
        .with_all(controls)
        .with_all(sides)
        .with_all(pads)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::paint::{MUTE_SIDE, PAUSE_SIDE, VOLUME_SIDE};
    use launchpad_emulator::DeviceSpec;
    use launchpad_emulator::devices::LaunchpadX;

    #[test]
    fn the_loops_screen_names_every_button_it_binds() {
        let labels = for_mode(Mode::Perform);
        for track in 0..TRACK_COUNT {
            for slot in 0..SLOT_COUNT {
                assert!(
                    labels.get(grid_pad(track, slot)).is_some(),
                    "track {track} slot {slot}"
                );
            }
        }
        for control in Control::all() {
            assert!(
                labels.get(control_pad(control.index())).is_some(),
                "{control:?}"
            );
        }
    }

    #[test]
    fn a_screen_names_only_what_it_uses() {
        let labels = for_mode(Mode::Mute);

        // The top row belongs to the loops, not to mute, so it goes unnamed.
        for control in Control::all() {
            assert!(
                labels.get(control_pad(control.index())).is_none(),
                "{control:?} named on the mute screen"
            );
        }
        // The way out and the transport are the two it does have.
        assert_eq!(labels.get(side_pad(MUTE_SIDE)), Some("leave mute"));
        assert_eq!(labels.get(side_pad(PAUSE_SIDE)), Some("play / pause"));
        assert!(labels.get(side_pad(VOLUME_SIDE)).is_none(), "out of reach");
    }

    #[test]
    fn a_name_is_only_given_where_a_press_does_something() {
        for mode in [
            Mode::Perform,
            Mode::Mute,
            Mode::Solo,
            Mode::Volume,
            Mode::Input,
            Mode::Settings,
            Mode::TimeSignature,
            Mode::Subdivision,
            Mode::SavePicker,
            Mode::LoadPicker,
        ] {
            let labels = for_mode(mode);
            for index in 0..SIDE_COUNT {
                let named = labels.get(side_pad(index)).is_some();
                let acts = screen::role(mode, Button::Side(index)) != screen::Role::Inert;
                assert_eq!(named, acts, "{mode:?} side {index}");
            }
            for control in Control::all() {
                let named = labels.get(control_pad(control.index())).is_some();
                let acts = screen::role(mode, Button::Top(control)) != screen::Role::Inert;
                assert_eq!(named, acts, "{mode:?} {control:?}");
            }
        }
    }

    #[test]
    fn the_pads_named_are_the_ones_the_device_reports() {
        // A label on a pad the device does not have would never be seen.
        for track in 0..TRACK_COUNT {
            for slot in 0..SLOT_COUNT {
                let pad = grid_pad(track, slot);
                assert!(LaunchpadX::is_button(pad), "grid {track},{slot}");
            }
        }
        for index in 0..SIDE_COUNT {
            assert!(LaunchpadX::is_button(side_pad(index)), "side {index}");
        }
        for control in Control::all() {
            assert!(
                LaunchpadX::is_button(control_pad(control.index())),
                "{control:?}"
            );
        }
    }

    #[test]
    fn a_grid_pad_maps_to_the_note_the_looper_lights() {
        // Programmer layout: the top-left grid pad is note 81, and rows count down.
        assert_eq!(LaunchpadX::pad_to_midi(grid_pad(0, 0)), Some(81));
        assert_eq!(LaunchpadX::pad_to_midi(grid_pad(7, 7)), Some(18));
        assert_eq!(LaunchpadX::pad_to_midi(side_pad(0)), Some(89));
        assert_eq!(LaunchpadX::pad_to_midi(control_pad(0)), Some(91));
    }
}
