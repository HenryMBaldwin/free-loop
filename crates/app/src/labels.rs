//! What each button does, for the on-screen surface to name.

use free_loop_core::{SLOT_COUNT, TRACK_COUNT};
use free_loop_surface::{
    Control, INPUT_SIDE, MUTE_SIDE, NEW_SIDE, PAUSE_SIDE, SETTINGS_SIDE, SIDE_COUNT, SOLO_SIDE,
    VOLUME_SIDE,
};
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

/// What the right-hand button at `index` opens.
fn side_name(index: usize) -> &'static str {
    match index {
        VOLUME_SIDE => "volume",
        INPUT_SIDE => "input",
        SETTINGS_SIDE => "settings",
        PAUSE_SIDE => "play / pause",
        MUTE_SIDE => "mute",
        SOLO_SIDE => "solo",
        NEW_SIDE => "new session",
        _ => "unused",
    }
}

/// What the top-row `control` does.
fn control_name(control: Control) -> &'static str {
    match control {
        Control::TempoUp => "tempo up",
        Control::TempoDown => "tempo down",
        Control::Rewind => "rewind",
        Control::Axis => "mute and solo by row or column",
        Control::LoadSession => "load session",
        Control::ClickToggle => "click",
        Control::StopAll => "stop all",
        Control::SaveSession => "save session",
    }
}

/// Every button named by the job it always has.
pub fn fixed() -> Labels {
    let controls = Control::all().map(|control| {
        (
            control_pad(control.index()),
            control_name(control).to_owned(),
        )
    });
    let sides = (0..SIDE_COUNT).map(|index| (side_pad(index), side_name(index).to_owned()));
    let pads = (0..TRACK_COUNT).flat_map(|track| {
        (0..SLOT_COUNT)
            .map(move |slot| (grid_pad(track, slot), format!("track {track} slot {slot}")))
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
    use launchpad_emulator::DeviceSpec;
    use launchpad_emulator::devices::LaunchpadX;

    #[test]
    fn every_button_the_looper_uses_is_named() {
        let labels = fixed();
        for track in 0..TRACK_COUNT {
            for slot in 0..SLOT_COUNT {
                assert!(
                    labels.get(grid_pad(track, slot)).is_some(),
                    "track {track} slot {slot}"
                );
            }
        }
        for index in 0..SIDE_COUNT {
            assert!(labels.get(side_pad(index)).is_some(), "side {index}");
        }
        for control in Control::all() {
            assert!(
                labels.get(control_pad(control.index())).is_some(),
                "{control:?}"
            );
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
