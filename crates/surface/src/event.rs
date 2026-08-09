//! What the performer did.

use free_loop_core::{SlotAddr, SlotId};

/// Top-row buttons that do something.
///
/// The remaining top-row buttons are the beat indicator and are output only, so a press
/// on one produces no event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Control {
    /// Turn the click on or off.
    ClickToggle,
    /// Slow down.
    TempoDown,
    /// Speed up.
    TempoUp,
    /// Stop everything.
    StopAll,
}

impl Control {
    /// The top-row button this control sits on.
    pub fn index(self) -> usize {
        match self {
            Self::ClickToggle => 0,
            Self::TempoDown => 1,
            Self::TempoUp => 2,
            Self::StopAll => 3,
        }
    }

    /// The control on a top-row button, if that button carries one.
    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::ClickToggle),
            1 => Some(Self::TempoDown),
            2 => Some(Self::TempoUp),
            3 => Some(Self::StopAll),
            _ => None,
        }
    }

    /// Every control, in top-row order.
    pub fn all() -> impl Iterator<Item = Self> {
        [
            Self::ClickToggle,
            Self::TempoDown,
            Self::TempoUp,
            Self::StopAll,
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
    ScenePressed {
        /// Which row.
        slot: SlotId,
    },
    /// A right-hand column button was released.
    SceneReleased {
        /// Which row.
        slot: SlotId,
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
    fn the_beat_indicator_buttons_carry_no_control() {
        for index in 4..8 {
            assert_eq!(Control::from_index(index), None);
        }
        assert_eq!(Control::from_index(99), None);
    }
}
