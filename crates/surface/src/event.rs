//! What the performer did.

use free_loop_core::SlotAddr;

/// Top-row buttons that do something.
///
/// The beat indicator shares the first four buttons rather than owning any. The tempo
/// controls are momentary nudges with no state to display, so lighting them with the
/// beat costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Control {
    /// Speed up.
    TempoUp,
    /// Slow down.
    TempoDown,
    /// Send the transport back to the start.
    Rewind,
    /// Switch mute and solo between rows and columns.
    Axis,
    /// Open the session picker to load.
    LoadSession,
    /// Turn the click on or off.
    ClickToggle,
    /// Stop everything.
    StopAll,
    /// Open the session picker to save.
    SaveSession,
}

impl Control {
    /// The top-row button this control sits on.
    pub fn index(self) -> usize {
        match self {
            Self::TempoUp => 0,
            Self::TempoDown => 1,
            Self::Rewind => 2,
            Self::Axis => 3,
            Self::LoadSession => 4,
            Self::ClickToggle => 5,
            Self::StopAll => 6,
            Self::SaveSession => 7,
        }
    }

    /// The control on a top-row button, if that button carries one.
    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::TempoUp),
            1 => Some(Self::TempoDown),
            2 => Some(Self::Rewind),
            3 => Some(Self::Axis),
            4 => Some(Self::LoadSession),
            5 => Some(Self::ClickToggle),
            6 => Some(Self::StopAll),
            7 => Some(Self::SaveSession),
            _ => None,
        }
    }

    /// Every control, in top-row order.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::TempoUp,
            Self::TempoDown,
            Self::Rewind,
            Self::Axis,
            Self::LoadSession,
            Self::ClickToggle,
            Self::StopAll,
            Self::SaveSession,
        ]
        .into_iter()
    }
}

/// A button went down or came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceEvent {
    /// A pad in the 8×8 grid was pressed.
    PadPressed {
        /// Which pad.
        addr: SlotAddr,
        /// How hard, 1–127. Ordinary switches report 127.
        velocity: u8,
    },
    /// A pad in the 8×8 grid was released.
    PadReleased {
        /// Which pad.
        addr: SlotAddr,
    },
    /// A right-hand column button was pressed.
    ///
    /// A plain strip of eight buttons with their own printed labels, not an extension of
    /// the tracks.
    SidePressed {
        /// Which button, top to bottom.
        index: u8,
    },
    /// A right-hand column button was released.
    SideReleased {
        /// Which button, top to bottom.
        index: u8,
    },
    /// A top-row control was pressed.
    ControlPressed(Control),
    /// A top-row control was released.
    ControlReleased(Control),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_round_trip_through_their_index() {
        for control in Control::all() {
            assert_eq!(Control::from_index(control.index()), Some(control));
        }
    }

    #[test]
    fn the_top_row_is_fully_bound() {
        for index in 0..8 {
            assert!(Control::from_index(index).is_some(), "button {index}");
        }
        assert_eq!(Control::from_index(99), None);
    }

    #[test]
    fn no_two_controls_share_a_button() {
        let mut seen = std::collections::HashSet::new();
        for control in Control::all() {
            assert!(seen.insert(control.index()), "{control:?} collides");
        }
    }
}
