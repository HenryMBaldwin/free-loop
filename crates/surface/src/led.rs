//! What the surface should be showing.
//!
//! Device-neutral: a colour and a style, with each device deciding how to produce them.

use free_loop_core::{SLOT_COUNT, SlotAddr, SlotId, TRACK_COUNT};

/// Buttons in the top row.
pub const CONTROL_COUNT: usize = 8;
/// Top-row buttons given over to the beat indicator.
pub const BEAT_LEDS: usize = 4;
/// Index of the first beat indicator button.
pub const FIRST_BEAT_LED: usize = 4;

/// Colours the looper uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LedColor {
    /// Nothing lit.
    #[default]
    Off,
    /// Beat one, and the surface's own chrome.
    White,
    /// Recording.
    Red,
    /// Holds a clip, or is about to stop.
    Amber,
    /// Playing.
    Green,
    /// Transport controls.
    Blue,
}

impl LedColor {
    /// Full-brightness colour, 0–255 per channel.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Off => (0, 0, 0),
            Self::White => (255, 255, 255),
            Self::Red => (255, 0, 0),
            Self::Amber => (255, 140, 0),
            Self::Green => (0, 255, 0),
            Self::Blue => (0, 80, 255),
        }
    }
}

/// How a colour is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LedStyle {
    /// Steady, full brightness.
    #[default]
    Solid,
    /// Steady, low brightness. Marks a pad that holds something but is idle.
    Dim,
    /// Alternating with black. Marks a transition waiting on a bar line.
    Flash,
    /// Breathing. Marks something currently sounding.
    Pulse,
}

/// One button's appearance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Led {
    /// The colour.
    pub color: LedColor,
    /// How it is shown.
    pub style: LedStyle,
}

impl Led {
    /// Unlit.
    pub const OFF: Self = Self {
        color: LedColor::Off,
        style: LedStyle::Solid,
    };

    /// Steady at full brightness.
    pub fn solid(color: LedColor) -> Self {
        Self {
            color,
            style: LedStyle::Solid,
        }
    }

    /// Steady at low brightness.
    pub fn dim(color: LedColor) -> Self {
        Self {
            color,
            style: LedStyle::Dim,
        }
    }

    /// Alternating with black.
    pub fn flash(color: LedColor) -> Self {
        Self {
            color,
            style: LedStyle::Flash,
        }
    }

    /// Breathing.
    pub fn pulse(color: LedColor) -> Self {
        Self {
            color,
            style: LedStyle::Pulse,
        }
    }

    /// Whether this button shows anything.
    pub fn is_lit(self) -> bool {
        self.color != LedColor::Off
    }
}

/// Everything the surface should be showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedFrame {
    pads: [[Led; SLOT_COUNT]; TRACK_COUNT],
    scenes: [Led; SLOT_COUNT],
    controls: [Led; CONTROL_COUNT],
}

impl LedFrame {
    /// A dark surface.
    pub fn new() -> Self {
        Self {
            pads: [[Led::OFF; SLOT_COUNT]; TRACK_COUNT],
            scenes: [Led::OFF; SLOT_COUNT],
            controls: [Led::OFF; CONTROL_COUNT],
        }
    }

    /// A pad in the 8×8 grid.
    pub fn pad(&self, addr: SlotAddr) -> Led {
        self.pads[addr.track.index()][addr.slot.index()]
    }

    /// Sets a pad in the 8×8 grid.
    pub fn set_pad(&mut self, addr: SlotAddr, led: Led) {
        self.pads[addr.track.index()][addr.slot.index()] = led;
    }

    /// A button in the right-hand column.
    pub fn scene(&self, slot: SlotId) -> Led {
        self.scenes[slot.index()]
    }

    /// Sets a button in the right-hand column.
    pub fn set_scene(&mut self, slot: SlotId, led: Led) {
        self.scenes[slot.index()] = led;
    }

    /// A button in the top row. Out-of-range indices read as unlit.
    pub fn control(&self, index: usize) -> Led {
        self.controls.get(index).copied().unwrap_or(Led::OFF)
    }

    /// Sets a button in the top row. Out-of-range indices are ignored.
    pub fn set_control(&mut self, index: usize, led: Led) {
        if let Some(slot) = self.controls.get_mut(index) {
            *slot = led;
        }
    }
}

impl Default for LedFrame {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use free_loop_core::TrackId;

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    #[test]
    fn a_new_frame_is_dark() {
        let frame = LedFrame::new();
        assert!(SlotAddr::all().all(|a| !frame.pad(a).is_lit()));
        assert!(SlotId::all().all(|s| !frame.scene(s).is_lit()));
        assert!((0..CONTROL_COUNT).all(|i| !frame.control(i).is_lit()));
    }

    #[test]
    fn pads_are_addressed_independently() {
        let mut frame = LedFrame::new();
        frame.set_pad(addr(3, 5), Led::pulse(LedColor::Green));

        assert_eq!(frame.pad(addr(3, 5)), Led::pulse(LedColor::Green));
        assert_eq!(
            frame.pad(addr(5, 3)),
            Led::OFF,
            "row and column must not swap"
        );
        assert_eq!(frame.pad(addr(3, 4)), Led::OFF);
    }

    #[test]
    fn out_of_range_controls_are_ignored_rather_than_panicking() {
        let mut frame = LedFrame::new();
        frame.set_control(99, Led::solid(LedColor::White));
        assert_eq!(frame.control(99), Led::OFF);
    }

    #[test]
    fn off_is_never_lit_whatever_the_style() {
        assert!(!Led::pulse(LedColor::Off).is_lit());
        assert!(Led::dim(LedColor::Amber).is_lit());
    }
}
