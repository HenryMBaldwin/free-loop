//! A surface with no hardware behind it.
//!
//! Lets the looper run headless and lets tests drive gestures and inspect what would
//! have been shown.

use std::collections::VecDeque;

use crate::event::SurfaceEvent;
use crate::led::LedFrame;
use crate::surface::{ControlSurface, SurfaceError};

/// A recording stand-in for a real surface.
#[derive(Debug, Default)]
pub struct MockSurface {
    pending: VecDeque<SurfaceEvent>,
    frames: Vec<LedFrame>,
    texts: Vec<Option<String>>,
    clock: u32,
    fail_next_render: bool,
}

impl MockSurface {
    /// A surface with nothing queued and nothing shown.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a gesture for the next [`ControlSurface::poll`].
    pub fn press(&mut self, event: SurfaceEvent) {
        self.pending.push_back(event);
    }

    /// Every frame rendered so far, oldest first.
    pub fn frames(&self) -> &[LedFrame] {
        &self.frames
    }

    /// The most recent frame.
    pub fn last_frame(&self) -> Option<&LedFrame> {
        self.frames.last()
    }

    /// Every text shown or stopped, oldest first. `None` is a stop.
    pub fn texts(&self) -> &[Option<String>] {
        &self.texts
    }

    /// MIDI clock ticks passed on so far.
    pub fn clock(&self) -> u32 {
        self.clock
    }

    /// Makes the next render fail, to exercise the caller's error path.
    pub fn fail_next_render(&mut self) {
        self.fail_next_render = true;
    }
}

impl ControlSurface for MockSurface {
    fn poll(&mut self, events: &mut Vec<SurfaceEvent>) {
        events.extend(self.pending.drain(..));
    }

    fn render(&mut self, frame: &LedFrame) -> Result<(), SurfaceError> {
        if core::mem::take(&mut self.fail_next_render) {
            return Err(SurfaceError::Device("mock failure".to_owned()));
        }
        self.frames.push(*frame);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), SurfaceError> {
        self.frames.push(LedFrame::new());
        Ok(())
    }

    fn send_clock(&mut self, ticks: u32) -> Result<(), SurfaceError> {
        self.clock += ticks;
        Ok(())
    }

    fn show_text(&mut self, text: &str) -> Result<(), SurfaceError> {
        self.texts.push(Some(text.to_owned()));
        Ok(())
    }

    fn stop_text(&mut self) -> Result<(), SurfaceError> {
        self.texts.push(None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::led::{Led, LedColor};
    use free_loop_core::{SlotAddr, SlotId, TrackId};

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    #[test]
    fn queued_gestures_arrive_once_and_in_order() {
        let mut surface = MockSurface::new();
        surface.press(SurfaceEvent::PadPressed {
            addr: addr(0, 0),
            velocity: 100,
        });
        surface.press(SurfaceEvent::PadReleased { addr: addr(0, 0) });

        let mut events = Vec::new();
        surface.poll(&mut events);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            SurfaceEvent::PadPressed {
                addr: addr(0, 0),
                velocity: 100
            }
        );

        surface.poll(&mut events);
        assert_eq!(events.len(), 2, "polling twice must not repeat gestures");
    }

    #[test]
    fn poll_appends_to_the_callers_buffer() {
        let mut surface = MockSurface::new();
        surface.press(SurfaceEvent::PadReleased { addr: addr(1, 1) });

        let mut events = vec![SurfaceEvent::PadReleased { addr: addr(0, 0) }];
        surface.poll(&mut events);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn rendered_frames_are_kept_in_order() {
        let mut surface = MockSurface::new();
        let mut frame = LedFrame::new();
        surface.render(&frame).unwrap();

        frame.set_pad(addr(2, 2), Led::solid(LedColor::Red));
        surface.render(&frame).unwrap();

        assert_eq!(surface.frames().len(), 2);
        assert_eq!(
            surface.last_frame().unwrap().pad(addr(2, 2)),
            Led::solid(LedColor::Red)
        );
    }

    #[test]
    fn text_is_recorded_in_order() {
        let mut surface = MockSurface::new();
        surface.show_text("120").unwrap();
        surface.stop_text().unwrap();

        assert_eq!(surface.texts(), [Some("120".to_owned()), None]);
    }

    #[test]
    fn a_forced_failure_happens_once() {
        let mut surface = MockSurface::new();
        surface.fail_next_render();
        assert!(surface.render(&LedFrame::new()).is_err());
        assert!(surface.render(&LedFrame::new()).is_ok());
    }

    #[test]
    fn clearing_records_a_dark_frame() {
        let mut surface = MockSurface::new();
        let mut frame = LedFrame::new();
        frame.set_pad(addr(0, 0), Led::solid(LedColor::Green));
        surface.render(&frame).unwrap();
        surface.clear().unwrap();

        assert_eq!(surface.last_frame(), Some(&LedFrame::new()));
    }
}
