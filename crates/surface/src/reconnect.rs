//! A surface that survives its device being unplugged.
//!
//! Holds the last frame it was given, so a device that comes back shows what it should be
//! showing without waiting for something to change.

use core::time::Duration;

use crate::event::SurfaceEvent;
use crate::led::LedFrame;
use crate::surface::{ControlSurface, SurfaceError};

/// How long to wait between attempts to find a device.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// A surface that reopens its device whenever one is available.
///
/// `open` is called to find a device, and again after every failure.
pub struct Reconnecting<S, F> {
    device: Option<S>,
    open: F,
    /// The frame to restore on a device that has just been opened.
    last: LedFrame,
    /// Time of the next attempt, or `None` while a device is working.
    retry_at: Option<Duration>,
    now: Duration,
}

impl<S, F> core::fmt::Debug for Reconnecting<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reconnecting")
            .field("connected", &self.device.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: ControlSurface, F: FnMut() -> Result<S, SurfaceError>> Reconnecting<S, F> {
    /// Tries to open a device, and keeps trying if there is none.
    ///
    /// The attempt made here counts as the one for the clock's origin, so the next falls
    /// one interval in.
    pub fn new(mut open: F) -> Self {
        let device = open().ok();
        Self {
            retry_at: device.is_none().then_some(RETRY_INTERVAL),
            device,
            open,
            last: LedFrame::new(),
            now: Duration::ZERO,
        }
    }

    /// Drops the device and schedules another attempt.
    fn lost(&mut self) {
        self.device = None;
        self.retry_at = Some(self.now + RETRY_INTERVAL);
    }

    /// Runs `send` on the device, dropping it if it has gone away.
    ///
    /// Reports success while there is no device, since nothing is waiting to be shown.
    fn send(&mut self, send: impl FnOnce(&mut S) -> Result<(), SurfaceError>) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        if send(device).is_err() {
            self.lost();
        }
    }
}

impl<S: ControlSurface, F: FnMut() -> Result<S, SurfaceError>> ControlSurface
    for Reconnecting<S, F>
{
    fn tick(&mut self, now: Duration) {
        self.now = now;
        if let Some(device) = self.device.as_mut() {
            device.tick(now);
            return;
        }

        if self.retry_at.is_some_and(|at| now < at) {
            return;
        }

        match (self.open)() {
            Ok(mut device) => {
                // A device opens dark, so this is what puts the grid back.
                if device.render(&self.last).is_err() {
                    self.retry_at = Some(now + RETRY_INTERVAL);
                    return;
                }
                self.device = Some(device);
                self.retry_at = None;
            }
            Err(_) => self.retry_at = Some(now + RETRY_INTERVAL),
        }
    }

    fn is_connected(&self) -> bool {
        self.device.is_some()
    }

    fn poll(&mut self, events: &mut Vec<SurfaceEvent>) {
        if let Some(device) = self.device.as_mut() {
            device.poll(events);
        }
    }

    fn render(&mut self, frame: &LedFrame) -> Result<(), SurfaceError> {
        self.last = *frame;
        self.send(|device| device.render(frame));
        Ok(())
    }

    fn clear(&mut self) -> Result<(), SurfaceError> {
        self.last = LedFrame::new();
        self.send(ControlSurface::clear);
        Ok(())
    }

    fn send_clock(&mut self, ticks: u32) -> Result<(), SurfaceError> {
        self.send(|device| device.send_clock(ticks));
        Ok(())
    }

    fn show_text(&mut self, text: &str) -> Result<(), SurfaceError> {
        self.send(|device| device.show_text(text));
        Ok(())
    }

    fn stop_text(&mut self) -> Result<(), SurfaceError> {
        self.send(ControlSurface::stop_text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::led::{Led, LedColor};
    use crate::mock::MockSurface;
    use core::cell::RefCell;
    use free_loop_core::{SlotAddr, SlotId, TrackId};
    use std::rc::Rc;

    /// A surface that fails everything once `dead` is set.
    #[derive(Debug)]
    struct Flaky {
        inner: MockSurface,
        dead: Rc<RefCell<bool>>,
    }

    impl ControlSurface for Flaky {
        fn poll(&mut self, events: &mut Vec<SurfaceEvent>) {
            self.inner.poll(events);
        }

        fn render(&mut self, frame: &LedFrame) -> Result<(), SurfaceError> {
            if *self.dead.borrow() {
                return Err(SurfaceError::NotFound);
            }
            self.inner.render(frame)
        }

        fn clear(&mut self) -> Result<(), SurfaceError> {
            self.inner.clear()
        }
    }

    fn addr() -> SlotAddr {
        SlotAddr::new(TrackId::new(0).unwrap(), SlotId::new(0).unwrap())
    }

    fn frame() -> LedFrame {
        let mut frame = LedFrame::new();
        frame.set_pad(addr(), Led::solid(LedColor::Green));
        frame
    }

    #[test]
    fn a_missing_device_is_not_an_error() {
        let mut surface = Reconnecting::new(|| Err::<MockSurface, _>(SurfaceError::NotFound));
        assert!(!surface.is_connected());
        assert!(surface.render(&frame()).is_ok());
        assert!(surface.send_clock(4).is_ok());
    }

    #[test]
    fn a_device_that_appears_later_is_picked_up() {
        let attempts = Rc::new(RefCell::new(0));
        let counter = Rc::clone(&attempts);
        let mut surface = Reconnecting::new(move || {
            *counter.borrow_mut() += 1;
            if *counter.borrow() < 3 {
                Err(SurfaceError::NotFound)
            } else {
                Ok(MockSurface::new())
            }
        });

        assert!(!surface.is_connected());
        surface.tick(RETRY_INTERVAL);
        assert!(!surface.is_connected(), "still nothing attached");
        surface.tick(2 * RETRY_INTERVAL);
        assert!(surface.is_connected());
        assert_eq!(*attempts.borrow(), 3);
    }

    #[test]
    fn attempts_wait_out_the_interval() {
        let attempts = Rc::new(RefCell::new(0));
        let counter = Rc::clone(&attempts);
        let mut surface = Reconnecting::new(move || {
            *counter.borrow_mut() += 1;
            Err::<MockSurface, _>(SurfaceError::NotFound)
        });

        for millis in 0..500 {
            surface.tick(Duration::from_millis(millis));
        }
        assert_eq!(*attempts.borrow(), 1, "only the attempt from the start");
    }

    #[test]
    fn a_device_that_goes_away_is_dropped() {
        let dead = Rc::new(RefCell::new(false));
        let flag = Rc::clone(&dead);
        let mut surface = Reconnecting::new(move || {
            Ok(Flaky {
                inner: MockSurface::new(),
                dead: Rc::clone(&flag),
            })
        });
        assert!(surface.is_connected());

        *dead.borrow_mut() = true;
        surface.render(&frame()).unwrap();
        assert!(
            !surface.is_connected(),
            "the failure took the device with it"
        );

        *dead.borrow_mut() = false;
        surface.tick(RETRY_INTERVAL);
        assert!(surface.is_connected());
    }

    #[test]
    fn a_reopened_device_is_given_the_last_frame() {
        let dead = Rc::new(RefCell::new(false));
        let flag = Rc::clone(&dead);
        let mut surface = Reconnecting::new(move || {
            Ok(Flaky {
                inner: MockSurface::new(),
                dead: Rc::clone(&flag),
            })
        });

        surface.render(&frame()).unwrap();
        *dead.borrow_mut() = true;
        surface.render(&frame()).unwrap();
        assert!(!surface.is_connected());

        *dead.borrow_mut() = false;
        surface.tick(RETRY_INTERVAL);
        assert_eq!(
            surface.device.as_ref().unwrap().inner.last_frame(),
            Some(&frame()),
            "the device was reopened onto the frame it should be showing"
        );
    }

    #[test]
    fn gestures_from_a_missing_device_are_nothing() {
        let mut surface = Reconnecting::new(|| Err::<MockSurface, _>(SurfaceError::NotFound));
        let mut events = Vec::new();
        surface.poll(&mut events);
        assert!(events.is_empty());
    }
}
