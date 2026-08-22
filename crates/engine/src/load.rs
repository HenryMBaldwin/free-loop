//! Putting a saved session back into the engine.
//!
//! Loaded audio arrives as storage the loader owns, marked borrowed so the engine returns
//! it rather than absorbing it. One ordered channel carries every step, so clearing the
//! grid, filling it and freezing the transport keep their order.

use std::sync::Arc;

use free_loop_core::{BarGrid, Frames, SlotAddr};
use rtrb::{Consumer, Producer, PushError, RingBuffer};

use free_loop_clip::Clip;

/// Messages in flight before the engine drains any. One per pad plus the two markers.
const SLOTS: usize = 80;

/// One step of a load.
#[derive(Debug)]
pub enum LoadMessage {
    /// Empty the grid and take the session's musical time.
    Begin {
        /// The session's grid. Only exists if the engine can measure it.
        grid: BarGrid,
    },
    /// Put a clip on a pad.
    Clip {
        /// Which pad.
        addr: SlotAddr,
        /// The audio.
        clip: Arc<Clip>,
        /// Whether the pad was sounding when the session was saved.
        playing: bool,
        /// Where the launch that was playing it put it, if there was one.
        launch_anchor: Option<Frames>,
    },
    /// Nothing more is coming. Freezes the transport.
    End,
}

/// A load that could not be handed over.
#[derive(Debug, thiserror::Error)]
#[error("the engine has not drained the load queue")]
pub struct LoadFull(pub LoadMessage);

/// The loader's side.
#[derive(Debug)]
pub struct Loader {
    out: Producer<LoadMessage>,
}

impl Loader {
    /// Queues one step.
    ///
    /// # Errors
    ///
    /// [`LoadFull`] with the message back if the engine has not drained the queue.
    pub fn send(&mut self, message: LoadMessage) -> Result<(), LoadFull> {
        self.out
            .push(message)
            .map_err(|PushError::Full(m)| LoadFull(m))
    }

    /// Whether the queue has room for a whole session.
    pub fn ready(&self) -> bool {
        self.out.slots() >= SLOTS
    }
}

/// The engine's side.
#[derive(Debug)]
pub struct LoadInbox {
    inbox: Consumer<LoadMessage>,
}

impl LoadInbox {
    /// Takes everything queued.
    pub fn drain(&mut self, mut take: impl FnMut(LoadMessage)) {
        while let Some(message) = self.pop() {
            take(message);
        }
    }

    /// Takes the next message, if there is one.
    ///
    /// For a reader that has to stop part way, such as one applying a load before it starts
    /// receiving the next.
    pub fn pop(&mut self) -> Option<LoadMessage> {
        self.inbox.pop().ok()
    }
}

/// Builds both sides.
pub fn channel() -> (Loader, LoadInbox) {
    let (out, inbox) = RingBuffer::new(SLOTS);
    (Loader { out }, LoadInbox { inbox })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use free_loop_clip::AudioBuffer;
    use free_loop_core::{Frames, SampleRate, SlotId, Tempo, TimeSignature, TrackId};

    fn clip() -> Arc<Clip> {
        Arc::new(Clip::new(AudioBuffer::new(1, 2), Frames(64), Frames(0), 2))
    }

    fn addr(track: u8, slot: u8) -> SlotAddr {
        SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
    }

    #[test]
    fn messages_arrive_in_the_order_they_were_sent() {
        let (mut loader, mut inbox) = channel();
        loader
            .send(LoadMessage::Begin {
                grid: BarGrid::new(
                    SampleRate::new(48_000).unwrap(),
                    Tempo::new(120.0).unwrap(),
                    TimeSignature::FOUR_FOUR,
                )
                .unwrap(),
            })
            .unwrap();
        loader
            .send(LoadMessage::Clip {
                addr: addr(1, 2),
                clip: clip(),
                playing: true,
                launch_anchor: None,
            })
            .unwrap();
        loader.send(LoadMessage::End).unwrap();

        let mut seen = Vec::new();
        inbox.drain(|m| {
            seen.push(match m {
                LoadMessage::Begin { .. } => "begin",
                LoadMessage::Clip { .. } => "clip",
                LoadMessage::End => "end",
            });
        });
        assert_eq!(seen, vec!["begin", "clip", "end"]);
    }

    #[test]
    fn a_full_queue_hands_the_message_back() {
        let (mut loader, _inbox) = channel();
        for _ in 0..SLOTS {
            loader.send(LoadMessage::End).unwrap();
        }
        assert!(!loader.ready());
        assert!(loader.send(LoadMessage::End).is_err());
    }

    #[test]
    fn a_drained_queue_has_room_for_a_whole_session() {
        let (mut loader, mut inbox) = channel();
        loader.send(LoadMessage::End).unwrap();
        assert!(!loader.ready());

        inbox.drain(|_| {});
        assert!(loader.ready());
    }
}
