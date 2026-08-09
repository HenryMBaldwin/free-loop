//! Returning shared clips to the engine after a reader is done with them.
//!
//! Dropping the last reference to a clip deallocates, which the audio thread must not do.
//! The engine reclaims clips nobody else holds directly. Clips someone is still reading
//! go through here instead.
//!
//! Run [`Recycler`] off the audio thread.

use std::sync::Arc;

use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::buffer::Clip;

/// Clips the engine can hand over before the recycler drains any. One buffer exists per
/// pad, so 128 covers the whole population.
const SLOTS: usize = 128;

/// The engine's side of the loop.
#[derive(Debug)]
pub struct Retirement {
    retired: Producer<Arc<Clip>>,
    returned: Consumer<Arc<Clip>>,
    /// Clips the ring could not take. Held rather than dropped, since dropping the last
    /// reference on the audio thread would allocate.
    pending: Vec<Arc<Clip>>,
}

impl Retirement {
    /// Gives up a clip. Never drops it, whatever the queues are doing.
    pub fn retire(&mut self, clip: Arc<Clip>) {
        if let Err(PushError::Full(clip)) = self.retired.push(clip) {
            self.pending.push(clip);
        }
    }

    /// Takes back every clip the recycler has released, and retries anything parked.
    pub fn reclaim(&mut self, mut take: impl FnMut(Arc<Clip>)) {
        while let Some(clip) = self.pending.pop() {
            if let Err(PushError::Full(clip)) = self.retired.push(clip) {
                self.pending.push(clip);
                break;
            }
        }
        while let Ok(clip) = self.returned.pop() {
            take(clip);
        }
    }

    /// Clips handed over but not yet on the ring.
    pub fn parked(&self) -> usize {
        self.pending.len()
    }
}

/// The other side of the loop. Run it anywhere except the audio thread.
#[derive(Debug)]
pub struct Recycler {
    retired: Consumer<Arc<Clip>>,
    returned: Producer<Arc<Clip>>,
    /// Clips nothing has finished reading yet.
    waiting: Vec<Arc<Clip>>,
    /// Swapped with `waiting` each pass so the retry list never reallocates.
    scratch: Vec<Arc<Clip>>,
}

impl Recycler {
    /// Sends back every retired clip nothing is reading any more. Clips still held are
    /// kept and retried, and do not block the ones behind them.
    ///
    /// Returns how many went back to the engine.
    pub fn run(&mut self) -> usize {
        while let Ok(clip) = self.retired.pop() {
            self.waiting.push(clip);
        }

        let mut returned = 0;
        self.scratch.clear();

        for mut candidate in self.waiting.drain(..) {
            // `get_mut` succeeds only on the sole reference. Used as a refcount test;
            // nothing is mutated through it.
            let alone = Arc::get_mut(&mut candidate).is_some();

            if !alone || self.returned.is_full() {
                self.scratch.push(candidate);
                continue;
            }
            if self.returned.push(candidate).is_ok() {
                returned += 1;
            }
        }

        core::mem::swap(&mut self.waiting, &mut self.scratch);
        returned
    }

    /// Clips retired but still being read elsewhere.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
}

/// Builds both ends of the loop.
pub fn channel() -> (Retirement, Recycler) {
    let (retired_tx, retired_rx) = RingBuffer::new(SLOTS);
    let (returned_tx, returned_rx) = RingBuffer::new(SLOTS);

    (
        Retirement {
            retired: retired_tx,
            returned: returned_rx,
            pending: Vec::with_capacity(SLOTS),
        },
        Recycler {
            retired: retired_rx,
            returned: returned_tx,
            waiting: Vec::with_capacity(SLOTS),
            scratch: Vec::with_capacity(SLOTS),
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::buffer::AudioBuffer;
    use free_loop_core::Frames;

    const CH: usize = 2;

    fn clip() -> Arc<Clip> {
        Arc::new(Clip::new(
            AudioBuffer::new(2, CH),
            Frames(64),
            Frames(0),
            CH,
        ))
    }

    #[test]
    fn a_clip_nobody_is_reading_comes_straight_back() {
        let (mut engine, mut recycler) = channel();
        engine.retire(clip());

        assert_eq!(recycler.run(), 1);

        let mut taken = 0;
        engine.reclaim(|_| taken += 1);
        assert_eq!(taken, 1);
    }

    #[test]
    fn a_clip_someone_is_reading_is_held_back() {
        let (mut engine, mut recycler) = channel();
        let held = clip();
        engine.retire(Arc::clone(&held));

        assert_eq!(recycler.run(), 0, "a reader still has it");
        assert_eq!(recycler.waiting(), 1);

        let mut taken = 0;
        engine.reclaim(|_| taken += 1);
        assert_eq!(taken, 0);

        drop(held);
        assert_eq!(recycler.run(), 1);
        assert_eq!(recycler.waiting(), 0);
    }

    #[test]
    fn a_held_clip_does_not_block_the_ones_behind_it() {
        let (mut engine, mut recycler) = channel();
        let held = clip();
        engine.retire(Arc::clone(&held));
        engine.retire(clip());
        engine.retire(clip());

        assert_eq!(recycler.run(), 2, "the free ones go back regardless");
        assert_eq!(recycler.waiting(), 1);

        drop(held);
        assert_eq!(recycler.run(), 1);
    }

    #[test]
    fn a_full_ring_parks_clips_rather_than_dropping_them() {
        let (mut engine, mut recycler) = channel();

        // One more than the ring holds. Dropping the extra would allocate in the audio
        // callback.
        for _ in 0..=SLOTS {
            engine.retire(clip());
        }
        assert_eq!(engine.parked(), 1);

        recycler.run();
        let mut taken = 0;
        engine.reclaim(|_| taken += 1);
        assert_eq!(taken, SLOTS);
        assert_eq!(engine.parked(), 0, "the parked clip made it onto the ring");

        recycler.run();
        engine.reclaim(|_| taken += 1);
        assert_eq!(taken, SLOTS + 1, "every clip came home");
    }

    #[test]
    fn reclaiming_with_nothing_to_do_is_harmless() {
        let (mut engine, mut recycler) = channel();
        assert_eq!(recycler.run(), 0);

        let mut taken = 0;
        engine.reclaim(|_| taken += 1);
        assert_eq!(taken, 0);
    }
}
