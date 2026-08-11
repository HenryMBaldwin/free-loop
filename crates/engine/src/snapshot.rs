//! Handing clips out for something off the audio thread to read.
//!
//! A snapshot request publishes a reference to each occupied pad: an atomic increment per
//! clip, no audio copied. Holding one keeps its clip alive until [`crate::recycle`] takes
//! it back.

use std::sync::Arc;

use free_loop_core::{SlotAddr, SlotState};
use rtrb::{Consumer, Producer, PushError, RingBuffer};

use crate::buffer::Clip;

/// Snapshots that can be queued before the reader drains any. One per pad, plus room for
/// a second request arriving before the first is read.
const SLOTS: usize = 128;

/// One pad's contents at the moment of a request.
#[derive(Debug)]
pub struct Snapshot {
    /// Which pad.
    pub addr: SlotAddr,
    /// What the pad was doing.
    pub state: SlotState,
    /// Where the launch that is playing it put it, if the track restarts its clips.
    pub launch_anchor: Option<free_loop_core::Frames>,
    /// The audio it held.
    pub clip: Arc<Clip>,
}

/// The engine's side.
#[derive(Debug)]
pub struct SnapshotWriter {
    out: Producer<Snapshot>,
    dropped: u64,
}

impl SnapshotWriter {
    /// Publishes one pad. Counts the snapshot as dropped if the reader is behind.
    pub fn publish(&mut self, snapshot: Snapshot) {
        if let Err(PushError::Full(_)) = self.out.push(snapshot) {
            self.dropped += 1;
        }
    }

    /// Snapshots that did not fit since the engine started.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// The reader's side. Use it anywhere except the audio thread.
#[derive(Debug)]
pub struct SnapshotReader {
    inbox: Consumer<Snapshot>,
}

impl SnapshotReader {
    /// Takes everything published since the last call.
    pub fn drain(&mut self, mut take: impl FnMut(Snapshot)) {
        while let Ok(snapshot) = self.inbox.pop() {
            take(snapshot);
        }
    }
}

/// Builds both sides.
pub fn channel() -> (SnapshotWriter, SnapshotReader) {
    let (out, inbox) = RingBuffer::new(SLOTS);
    (SnapshotWriter { out, dropped: 0 }, SnapshotReader { inbox })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;
    use crate::buffer::AudioBuffer;
    use free_loop_core::{ClipId, Frames, SlotId, TrackId};

    fn snapshot(track: u8, slot: u8) -> Snapshot {
        Snapshot {
            addr: SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap()),
            state: SlotState::Stopped { clip: ClipId(0) },
            launch_anchor: None,
            clip: Arc::new(Clip::new(AudioBuffer::new(1, 2), Frames(64), Frames(0), 2)),
        }
    }

    #[test]
    fn published_snapshots_arrive_in_order() {
        let (mut writer, mut reader) = channel();
        writer.publish(snapshot(0, 0));
        writer.publish(snapshot(1, 2));

        let mut seen = Vec::new();
        reader.drain(|s| seen.push((s.addr.track.index(), s.addr.slot.index())));
        assert_eq!(seen, vec![(0, 0), (1, 2)]);
    }

    #[test]
    fn draining_twice_does_not_repeat() {
        let (mut writer, mut reader) = channel();
        writer.publish(snapshot(0, 0));

        let mut count = 0;
        reader.drain(|_| count += 1);
        reader.drain(|_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn a_snapshot_keeps_the_clip_alive() {
        let (mut writer, mut reader) = channel();
        let clip = Arc::new(Clip::new(AudioBuffer::new(1, 2), Frames(64), Frames(0), 2));

        let mut published = snapshot(0, 0);
        published.clip = Arc::clone(&clip);
        writer.publish(published);

        let mut held = None;
        reader.drain(|s| held = Some(s.clip));
        assert_eq!(Arc::strong_count(&clip), 2, "the reader has a reference");

        drop(held);
        assert_eq!(Arc::strong_count(&clip), 1);
    }

    #[test]
    fn an_unread_queue_counts_drops_rather_than_blocking() {
        let (mut writer, _reader) = channel();
        for _ in 0..=SLOTS {
            writer.publish(snapshot(0, 0));
        }
        assert_eq!(writer.dropped(), 1);
    }
}
