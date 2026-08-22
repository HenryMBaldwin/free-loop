//! What each button does on the screen the surface is showing.
//!
//! A screen owns every button, not only the grid: a button with no part to play on the
//! screen you are on does nothing, so there is no way to reach solo from mute without
//! leaving mute first. The transport is the one exception, live everywhere.

use free_loop_core::{SlotAddr, Subdivision};
use free_loop_surface::Control;

use crate::control::Mode;
use crate::paint::{
    self, INPUT_SIDE, MUTE_SIDE, NEW_SIDE, NO_PAD, PAUSE_SIDE, PICKUP_COLUMN, RESTART_COLUMN,
    SETTINGS_SIDE, SOLO_SIDE, SignaturePart, VOLUME_SIDE, YES_PAD,
};

/// One button on the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// A pad in the eight by eight grid.
    Grid(SlotAddr),
    /// A button in the top row.
    Top(Control),
    /// A button in the right-hand column, top to bottom.
    Side(usize),
}

/// What pressing a button does on the screen it is on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    /// Nothing on this screen.
    Inert,
    /// Freezes or resumes the transport.
    Transport,
    /// A loop: tap to launch or stop, hold to empty.
    Loop,
    /// Yes or no to what the screen is asking.
    Answer,
    /// The session to save over.
    SaveTo,
    /// The session to load.
    LoadFrom,
    /// Starts again from nothing.
    NewSession,
    /// Silences or soloes a pad's group.
    Group,
    /// How loud a track plays.
    Level,
    /// The input a track records.
    InputChannel,
    /// One of a track's settings.
    Setting,
    /// Beats to the bar.
    Beats(u32),
    /// The note that gets the beat.
    Unit(u32),
    /// How often the click sounds.
    Rate(Subdivision),
    /// Moves the tempo, and opens or closes the signature screen when both are held.
    Tempo(f64),
    /// Turns the click on or off, and opens or closes its own screen when held.
    Click,
    /// Stops everything.
    StopAll,
    /// Sends the transport back to the start.
    Rewind,
    /// Flips whether mute and solo group by row or column.
    Axis,
    /// Opens that screen, or leaves it if it is the one showing.
    Open(Mode),
}

/// What `button` does on `mode`.
pub fn role(mode: Mode, button: Button) -> Role {
    // The one control every screen keeps: the music has to be stoppable from anywhere.
    if button == Button::Side(PAUSE_SIDE) {
        return Role::Transport;
    }
    match button {
        Button::Grid(addr) => grid(mode, addr),
        Button::Top(control) => top(mode, control),
        Button::Side(index) => side(mode, index),
    }
}

/// The buttons that leave `mode`, which paint as waiting so the way out always shows.
///
/// Empty on [`Mode::Perform`], which is the screen there is nothing to leave.
pub fn exits(mode: Mode) -> [Option<Button>; 2] {
    let one = |button| [Some(button), None];
    match mode {
        Mode::Perform => [None, None],
        Mode::Volume => one(Button::Side(VOLUME_SIDE)),
        Mode::Input => one(Button::Side(INPUT_SIDE)),
        Mode::Settings => one(Button::Side(SETTINGS_SIDE)),
        Mode::Mute => one(Button::Side(MUTE_SIDE)),
        Mode::Solo => one(Button::Side(SOLO_SIDE)),
        Mode::SavePicker | Mode::ConfirmSave(_) => one(Button::Top(Control::SaveSession)),
        Mode::LoadPicker | Mode::ConfirmLoad(_) => one(Button::Top(Control::LoadSession)),
        Mode::Subdivision => one(Button::Top(Control::ClickToggle)),
        // Both together, which is what opened it.
        Mode::TimeSignature => [
            Some(Button::Top(Control::TempoDown)),
            Some(Button::Top(Control::TempoUp)),
        ],
    }
}

/// What this screen is called, for naming the way out of it.
pub fn title(mode: Mode) -> &'static str {
    match mode {
        Mode::Perform => "the loops",
        Mode::SavePicker | Mode::ConfirmSave(_) => "save",
        Mode::LoadPicker | Mode::ConfirmLoad(_) => "load",
        Mode::Mute => "mute",
        Mode::Solo => "solo",
        Mode::Volume => "volume",
        Mode::Input => "input",
        Mode::Settings => "settings",
        Mode::TimeSignature => "time signature",
        Mode::Subdivision => "click rate",
    }
}

/// What to call `button` on `mode`, or `None` where it does nothing.
///
/// Read from the same table a press is, so a name cannot promise what a press will not do.
pub fn name(mode: Mode, button: Button) -> Option<String> {
    if exits(mode).into_iter().flatten().any(|way| way == button) {
        return Some(format!("leave {}", title(mode)));
    }
    let pad = |addr: SlotAddr| (addr.track.index(), addr.slot.index());
    Some(match (role(mode, button), button) {
        (Role::Transport, _) => "play / pause".to_owned(),
        (Role::Loop, Button::Grid(addr)) => {
            let (track, slot) = pad(addr);
            format!("loop {track},{slot}")
        }
        (Role::Answer, Button::Grid(addr)) => match pad(addr) {
            YES_PAD => "yes".to_owned(),
            NO_PAD => "no".to_owned(),
            _ => return None,
        },
        (Role::SaveTo, Button::Grid(addr)) => {
            let (track, slot) = pad(addr);
            format!("save over {track}{slot}")
        }
        (Role::LoadFrom, Button::Grid(addr)) => {
            let (track, slot) = pad(addr);
            format!("load {track}{slot}")
        }
        (Role::NewSession, _) => "new session".to_owned(),
        (Role::Group, Button::Grid(addr)) => {
            let (track, slot) = pad(addr);
            format!("{} {track},{slot}", title(mode))
        }
        (Role::Level, Button::Grid(addr)) => {
            let (track, slot) = pad(addr);
            format!("track {track} level {}", slot + 1)
        }
        (Role::InputChannel, Button::Grid(addr)) => {
            let (track, slot) = pad(addr);
            format!("track {track} input {slot}")
        }
        (Role::Setting, Button::Grid(addr)) => match pad(addr) {
            (track, RESTART_COLUMN) => format!("track {track} restart"),
            (track, PICKUP_COLUMN) => format!("track {track} pickup"),
            _ => return None,
        },
        (Role::Beats(beats), _) => format!("{beats} beats to the bar"),
        (Role::Unit(unit), _) => format!("beat is a 1/{unit} note"),
        (Role::Rate(rate), _) => format!("click {}", rate.name()),
        (Role::Tempo(direction), _) => {
            if direction > 0.0 {
                "tempo up".to_owned()
            } else {
                "tempo down".to_owned()
            }
        }
        (Role::Click, _) => "click on or off".to_owned(),
        (Role::StopAll, _) => "stop all".to_owned(),
        (Role::Rewind, _) => "rewind".to_owned(),
        (Role::Axis, _) => "group by row or column".to_owned(),
        (Role::Open(opens), _) => format!("open {}", title(opens)),
        // Inert, and grid roles on something that is not a pad, which cannot arise.
        _ => return None,
    })
}

fn grid(mode: Mode, addr: SlotAddr) -> Role {
    match mode {
        Mode::Perform => Role::Loop,
        Mode::SavePicker => Role::SaveTo,
        Mode::LoadPicker => Role::LoadFrom,
        Mode::ConfirmSave(_) | Mode::ConfirmLoad(_) => Role::Answer,
        Mode::Mute | Mode::Solo => Role::Group,
        Mode::Volume => Role::Level,
        Mode::Input => Role::InputChannel,
        Mode::Settings => Role::Setting,
        Mode::TimeSignature => match paint::signature_part(addr) {
            Some(SignaturePart::Beats(beats)) => Role::Beats(beats),
            Some(SignaturePart::Unit(unit)) => Role::Unit(unit),
            None => Role::Inert,
        },
        Mode::Subdivision => paint::subdivision_at(addr).map_or(Role::Inert, Role::Rate),
    }
}

fn top(mode: Mode, control: Control) -> Role {
    match (mode, control) {
        // On the loops these move the tempo; held together they open the signature
        // screen, and do the same again to leave it.
        (Mode::Perform | Mode::TimeSignature, Control::TempoDown) => Role::Tempo(-1.0),
        (Mode::Perform | Mode::TimeSignature, Control::TempoUp) => Role::Tempo(1.0),
        (Mode::Perform | Mode::Subdivision, Control::ClickToggle) => Role::Click,
        (Mode::Perform, Control::StopAll) => Role::StopAll,
        (Mode::Perform, Control::Rewind) => Role::Rewind,
        (Mode::Perform, Control::Axis) => Role::Axis,
        (Mode::Perform | Mode::SavePicker | Mode::ConfirmSave(_), Control::SaveSession) => {
            Role::Open(Mode::SavePicker)
        }
        (Mode::Perform | Mode::LoadPicker | Mode::ConfirmLoad(_), Control::LoadSession) => {
            Role::Open(Mode::LoadPicker)
        }
        _ => Role::Inert,
    }
}

fn side(mode: Mode, index: usize) -> Role {
    match (mode, index) {
        (Mode::LoadPicker, NEW_SIDE) => Role::NewSession,
        (Mode::Perform | Mode::Volume, VOLUME_SIDE) => Role::Open(Mode::Volume),
        (Mode::Perform | Mode::Input, INPUT_SIDE) => Role::Open(Mode::Input),
        (Mode::Perform | Mode::Settings, SETTINGS_SIDE) => Role::Open(Mode::Settings),
        (Mode::Perform | Mode::Mute, MUTE_SIDE) => Role::Open(Mode::Mute),
        (Mode::Perform | Mode::Solo, SOLO_SIDE) => Role::Open(Mode::Solo),
        _ => Role::Inert,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use free_loop_core::{SLOT_COUNT, SlotId, TRACK_COUNT, TrackId};
    use free_loop_surface::SIDE_COUNT;

    /// Every screen there is, with a pad for the two that carry one.
    fn every_mode() -> Vec<Mode> {
        let pad = addr(0, 0);
        vec![
            Mode::Perform,
            Mode::SavePicker,
            Mode::LoadPicker,
            Mode::Mute,
            Mode::Solo,
            Mode::Volume,
            Mode::Input,
            Mode::Settings,
            Mode::TimeSignature,
            Mode::Subdivision,
            Mode::ConfirmSave(pad),
            Mode::ConfirmLoad(pad),
        ]
    }

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    /// Every button the surface has.
    fn every_button() -> Vec<Button> {
        let mut buttons = Vec::new();
        for track in 0..TRACK_COUNT {
            for slot in 0..SLOT_COUNT {
                let track = u8::try_from(track).unwrap();
                let slot = u8::try_from(slot).unwrap();
                buttons.push(Button::Grid(addr(track, slot)));
            }
        }
        for control in Control::all() {
            buttons.push(Button::Top(control));
        }
        for index in 0..SIDE_COUNT {
            buttons.push(Button::Side(index));
        }
        buttons
    }

    #[test]
    fn every_screen_but_the_loops_says_how_to_leave_it() {
        for mode in every_mode() {
            let ways: Vec<Button> = exits(mode).into_iter().flatten().collect();
            if mode == Mode::Perform {
                assert!(ways.is_empty(), "the loops are not a screen to leave");
            } else {
                assert!(!ways.is_empty(), "{mode:?} has no way out");
            }
        }
    }

    #[test]
    fn a_way_out_is_never_inert() {
        for mode in every_mode() {
            for button in exits(mode).into_iter().flatten() {
                assert_ne!(
                    role(mode, button),
                    Role::Inert,
                    "{mode:?} leaves by a button that does nothing"
                );
            }
        }
    }

    #[test]
    fn the_transport_answers_on_every_screen() {
        for mode in every_mode() {
            assert_eq!(
                role(mode, Button::Side(PAUSE_SIDE)),
                Role::Transport,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn one_screen_cannot_be_reached_from_another() {
        let screens = [
            Button::Side(VOLUME_SIDE),
            Button::Side(INPUT_SIDE),
            Button::Side(SETTINGS_SIDE),
            Button::Side(MUTE_SIDE),
            Button::Side(SOLO_SIDE),
        ];
        for mode in every_mode()
            .into_iter()
            .filter(|mode| *mode != Mode::Perform)
        {
            let ways: Vec<Button> = exits(mode).into_iter().flatten().collect();
            for button in screens {
                if ways.contains(&button) {
                    continue;
                }
                assert_eq!(
                    role(mode, button),
                    Role::Inert,
                    "{mode:?} can still reach another screen by {button:?}"
                );
            }
        }
    }

    #[test]
    fn the_loops_screen_binds_every_button_it_uses() {
        // Nothing on the loops is inert except the side buttons that were never bound.
        let bound = |button| role(Mode::Perform, button) != Role::Inert;
        for control in Control::all() {
            assert!(bound(Button::Top(control)), "{control:?}");
        }
        for index in [
            VOLUME_SIDE,
            INPUT_SIDE,
            SETTINGS_SIDE,
            MUTE_SIDE,
            SOLO_SIDE,
            PAUSE_SIDE,
        ] {
            assert!(bound(Button::Side(index)), "side {index}");
        }
        assert!(bound(Button::Grid(addr(3, 4))));
    }

    #[test]
    fn a_role_is_the_same_however_often_it_is_asked() {
        for mode in every_mode() {
            for button in every_button() {
                assert_eq!(role(mode, button), role(mode, button));
            }
        }
    }
}
