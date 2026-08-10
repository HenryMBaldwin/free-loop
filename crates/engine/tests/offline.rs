//! Drives the engine with no audio device.
//!
//! The input signal is a function of absolute transport position, so a recorded clip can
//! be checked against the exact frames that were fed in while it was capturing.

#![allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    reason = "tests should fail loudly, and compare exact sample values"
)]

use free_loop_core::{ClipId, Command, Event, Frames, SlotAddr, SlotId, SlotState, TrackId};
use free_loop_engine::{ClickConfig, Engine, EngineConfig, Housekeeping, Snapshot};

const CHANNELS: usize = 2;
const BAR: u64 = 96_000; // 120 bpm, 4/4, 48 kHz
const BEAT: u64 = BAR / 4;

/// A distinct value per frame and channel. The 977 modulus is coprime with the bar
/// length, so a phase error cannot coincidentally produce matching samples.
fn signal(frame: u64, channel: usize) -> f32 {
    (frame % 977) as f32 + channel as f32 * 0.25
}

struct Harness {
    engine: Engine,
    housekeeping: Housekeeping,
    events: Vec<Event>,
    block: usize,
}

impl Harness {
    fn new(block: usize) -> Self {
        Self::with_offset(block, Frames::ZERO)
    }

    fn with_offset(block: usize, capture_offset: Frames) -> Self {
        let mut config = EngineConfig::stereo_48k().unwrap();
        config.segment_pool = 12;
        config.capture_offset = capture_offset;
        config.click = ClickConfig {
            enabled: false,
            level: 0.0,
        };
        let (engine, housekeeping) = Engine::new(config).unwrap();
        Self {
            engine,
            housekeeping,
            events: Vec::new(),
            block,
        }
    }

    fn with_click(block: usize) -> Self {
        let mut harness = Self::new(block);
        harness.command(Command::SetClickEnabled(true));
        harness.command(Command::SetClickLevel(0.5));
        harness
    }

    fn position(&self) -> u64 {
        self.engine.position().0
    }

    fn command(&mut self, command: Command) {
        self.engine.handle(command, &mut self.events);
    }

    /// Runs until the transport reaches `target`, returning the rendered output.
    fn run_to(&mut self, target: u64) -> Vec<f32> {
        let mut out = Vec::new();
        while self.position() < target {
            let start = self.position();
            let frames = usize::try_from(target - start).unwrap().min(self.block);

            let input: Vec<f32> = (0..frames * CHANNELS)
                .map(|i| signal(start + (i / CHANNELS) as u64, i % CHANNELS))
                .collect();
            let mut block = vec![0.0; frames * CHANNELS];

            self.engine.process(&input, &mut block, &mut self.events);
            out.extend_from_slice(&block);
        }
        out
    }

    /// Runs a fixed number of frames whether or not the transport advances.
    fn run_frames(&mut self, frames: usize) -> Vec<f32> {
        let start = self.position();
        let input: Vec<f32> = (0..frames * CHANNELS)
            .map(|i| signal(start + (i / CHANNELS) as u64, i % CHANNELS))
            .collect();
        let mut out = vec![0.0; frames * CHANNELS];
        self.engine.process(&input, &mut out, &mut self.events);
        out
    }

    fn drain_events(&mut self) -> Vec<Event> {
        core::mem::take(&mut self.events)
    }
}

fn addr(track: u8, slot: u8) -> SlotAddr {
    SlotAddr::new(TrackId::new(track).unwrap(), SlotId::new(slot).unwrap())
}

/// Records `bars` bars into `pad`, starting on the bar boundary after `arm_at`.
/// Leaves the transport at the boundary the clip starts playing on.
fn record(harness: &mut Harness, pad: SlotAddr, arm_at: u64, bars: u64) -> (u64, u64) {
    harness.run_to(arm_at);
    harness.command(Command::Press(pad));

    let start = arm_at.div_ceil(BAR) * BAR;
    let end = start + bars * BAR;

    harness.run_to(end);
    harness.command(Command::Press(pad));
    // The capture is sealed when the transport reaches the boundary, which happens at
    // the top of the next block.
    harness.run_to(end + 1);
    (start, end)
}

#[test]
fn the_transport_reports_bars_and_beats_on_the_grid() {
    let mut harness = Harness::new(128);
    harness.run_to(2 * BAR);

    let events = harness.drain_events();
    let bars: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Bar { bar } => Some(*bar),
            _ => None,
        })
        .collect();
    let beats: Vec<(u64, u32)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Beat { bar, beat } => Some((*bar, *beat)),
            _ => None,
        })
        .collect();

    assert_eq!(bars, vec![0, 1]);
    assert_eq!(
        beats,
        vec![
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 0),
            (1, 1),
            (1, 2),
            (1, 3)
        ]
    );
}

#[test]
fn a_recorded_loop_plays_back_what_was_captured() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    let (start, end) = record(&mut harness, pad, 1_000, 2);

    assert_eq!(start, BAR);
    assert_eq!(end, 3 * BAR);
    assert_eq!(
        harness.engine.state(pad),
        SlotState::Playing { clip: ClipId(0) }
    );

    let len = end - start;
    let from = harness.position();
    let out = harness.run_to(from + len);

    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        let phase = (frame - start) % len;
        assert_eq!(
            *sample,
            signal(start + phase, channel),
            "frame {frame} channel {channel}"
        );
    }
}

/// With no compensation a note played at grid position `P` comes back late by the round
/// trip. Stamping the clip as having started earlier puts it back where it was played.
#[test]
fn compensation_puts_playback_back_on_the_grid() {
    const LATENCY: u64 = 2_048;

    let mut harness = Harness::with_offset(128, Frames(LATENCY));
    let pad = addr(0, 0);
    let (start, end) = record(&mut harness, pad, 1_000, 2);
    let len = end - start;

    let from = harness.position();
    let out = harness.run_to(from + len);

    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        // Frame k of the capture holds what arrived at `start + k`, which was played at
        // `start + k - LATENCY`. Playback should emit that at its played position.
        let captured_at = start + (frame + LATENCY - start) % len;
        assert_eq!(
            *sample,
            signal(captured_at, channel),
            "frame {frame} channel {channel}"
        );
    }
}

#[test]
fn without_compensation_playback_sits_late_by_the_round_trip() {
    const LATENCY: u64 = 2_048;

    let mut compensated = Harness::with_offset(128, Frames(LATENCY));
    let mut raw = Harness::new(128);
    let pad = addr(0, 0);

    let (start, end) = record(&mut compensated, pad, 1_000, 2);
    record(&mut raw, pad, 1_000, 2);
    let len = end - start;

    let from = compensated.position();
    let with = compensated.run_to(from + len);
    let without = raw.run_to(from + len);

    assert_ne!(with, without, "compensation must actually move the audio");

    // The uncompensated render is the compensated one delayed by the round trip.
    let shift = usize::try_from(LATENCY).unwrap() * CHANNELS;
    assert_eq!(without[shift..], with[..with.len() - shift]);
}

#[test]
fn pausing_freezes_the_transport_and_silences_it() {
    let mut harness = Harness::with_click(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 1_000, 1);

    let at = harness.position();
    harness.command(Command::SetPaused(true));

    let out = harness.run_frames(512);
    assert_eq!(
        harness.position(),
        at,
        "the transport must hold its position"
    );
    assert!(out.iter().all(|s| *s == 0.0), "nothing sounds while frozen");
    assert!(harness.engine.is_paused());
}

#[test]
fn resuming_picks_up_the_phase_it_stopped_on() {
    let pad = addr(0, 0);

    // Render a stretch straight through.
    let mut straight = Harness::new(128);
    let (start, end) = record(&mut straight, pad, 0, 1);
    let from = straight.position();
    let reference = straight.run_to(from + 2 * BAR);

    // The same stretch with a freeze part way through.
    let mut paused = Harness::new(128);
    record(&mut paused, pad, 0, 1);
    let first = paused.run_to(from + BAR);
    paused.command(Command::SetPaused(true));
    paused.run_frames(4_096);
    paused.command(Command::SetPaused(false));
    let second = paused.run_to(from + 2 * BAR);

    let mut rejoined = first;
    rejoined.extend(second);
    assert_eq!(
        rejoined, reference,
        "a freeze must not shift the loop's phase"
    );
    assert_eq!(end - start, BAR);
}

#[test]
fn pausing_discards_a_take_in_progress() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    harness.command(Command::Press(pad));
    harness.run_to(2 * BAR);
    assert!(harness.engine.state(pad).is_recording());

    harness.command(Command::SetPaused(true));
    assert_eq!(
        harness.engine.state(pad),
        SlotState::Empty,
        "a take spanning a freeze would splice two moments together"
    );
}

#[test]
fn pausing_leaves_playing_clips_ready_to_resume() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    harness.command(Command::SetPaused(true));
    assert_eq!(
        harness.engine.state(pad),
        SlotState::Playing { clip: ClipId(0) }
    );

    harness.command(Command::SetPaused(false));
    let out = harness.run_frames(256);
    assert!(out.iter().any(|s| *s != 0.0), "playback resumes");
}

#[test]
fn a_loop_repeats_at_its_length() {
    let mut harness = Harness::new(256);
    let pad = addr(0, 0);
    let (start, end) = record(&mut harness, pad, 0, 1);
    let len = end - start;

    let from = harness.position();
    let first = harness.run_to(from + len);
    let second = harness.run_to(from + 2 * len);
    assert_eq!(first, second);
}

#[test]
fn the_recorded_length_is_a_whole_number_of_bars() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    harness.run_to(BAR / 3);
    harness.command(Command::Press(pad));
    harness.run_to(BAR);
    // Stop pressed most of the way through bar 3, so bar 3 is discarded.
    harness.run_to(3 * BAR + 3 * BEAT);
    harness.command(Command::Press(pad));
    harness.run_to(3 * BAR + 3 * BEAT + 1);

    let recorded: Vec<Frames> = harness
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            Event::ClipRecorded { len, .. } => Some(*len),
            _ => None,
        })
        .collect();
    assert_eq!(recorded, vec![Frames(2 * BAR)]);
}

#[test]
fn loops_of_different_lengths_stay_phase_locked() {
    let mut harness = Harness::new(128);
    let short = addr(0, 0);
    let long = addr(1, 0);

    let (short_start, _) = record(&mut harness, short, 0, 1);
    let after_short = harness.position();
    let (long_start, long_end) = record(&mut harness, long, after_short, 2);

    let from = harness.position();
    let out = harness.run_to(from + 4 * BAR);

    let short_len = BAR;
    let long_len = long_end - long_start;

    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        let a = signal(short_start + (frame - short_start) % short_len, channel);
        let b = signal(long_start + (frame - long_start) % long_len, channel);
        assert_eq!(*sample, a + b, "frame {frame}");
    }
}

#[test]
fn launching_a_sibling_swaps_on_the_bar_line() {
    let mut harness = Harness::new(64);
    let first = addr(0, 0);
    let second = addr(0, 1);

    let (first_start, _) = record(&mut harness, first, 0, 1);
    let after_first = harness.position();
    let (second_start, second_end) = record(&mut harness, second, after_first, 1);

    // The first pad handed over as the second armed.
    assert_eq!(
        harness.engine.state(first),
        SlotState::Stopped { clip: ClipId(0) }
    );

    // Relaunch the first part way through a bar; the swap happens on the next line.
    let mid = second_end + BAR + BEAT;
    harness.run_to(mid);
    harness.command(Command::Press(first));

    let boundary = second_end + 2 * BAR;
    let before = harness.run_to(boundary);
    let after = harness.run_to(boundary + 64);

    let second_len = second_end - second_start;
    let frame_before = boundary - 1;
    assert_eq!(
        before[before.len() - CHANNELS],
        signal(second_start + (frame_before - second_start) % second_len, 0),
        "the second pad still owns the frame before the line"
    );
    assert_eq!(
        after[0],
        signal(first_start + (boundary - first_start) % BAR, 0),
        "the first pad owns the line itself"
    );
}

#[test]
fn rendering_is_identical_across_block_sizes() {
    fn render(block: usize) -> Vec<f32> {
        let mut harness = Harness::with_click(block);
        let pad = addr(2, 3);
        record(&mut harness, pad, BEAT, 1);
        let from = harness.position();
        harness.run_to(from + 2 * BAR)
    }

    let reference = render(64);
    for block in [1, 17, 128, 512, 4_096] {
        assert_eq!(render(block), reference, "block size {block} diverged");
    }
}

#[test]
fn the_click_sounds_on_every_beat() {
    let mut harness = Harness::with_click(128);
    let out = harness.run_to(BAR);

    // Each beat should start a blip and each blip should die out before the next beat.
    for beat in 0..4u64 {
        let at = usize::try_from(beat * BEAT).unwrap() * CHANNELS;
        let onset = &out[at..at + 200 * CHANNELS];
        assert!(
            onset.iter().any(|s| s.abs() > 0.0),
            "beat {beat} produced no click"
        );

        let gap_start = at + usize::try_from(BEAT).unwrap() * CHANNELS - 400 * CHANNELS;
        let gap = &out[gap_start..gap_start + 200 * CHANNELS];
        assert!(
            gap.iter().all(|s| *s == 0.0),
            "the blip on beat {beat} did not decay before the next"
        );
    }
}

#[test]
fn a_snapshot_publishes_every_pad_that_holds_audio() {
    let mut harness = Harness::new(128);
    let playing = addr(0, 0);
    let stopped = addr(1, 1);

    record(&mut harness, playing, 0, 1);
    let after = harness.position();
    record(&mut harness, stopped, after, 1);
    // Recording the second pad on another track leaves the first playing.
    harness.command(Command::Press(stopped));
    harness.run_to(harness.position() + BAR + 1);

    harness.command(Command::Snapshot);

    let mut seen: Vec<Snapshot> = Vec::new();
    harness.housekeeping.snapshots.drain(|s| seen.push(s));

    assert_eq!(seen.len(), 2, "both pads hold audio");
    assert!(seen.iter().all(|s| s.clip.len() == Frames(BAR)));
    assert!(seen.iter().any(|s| s.addr == playing));
    assert!(seen.iter().any(|s| s.addr == stopped));
}

#[test]
fn a_pad_still_recording_is_not_published() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    harness.command(Command::Press(pad));
    harness.run_to(2 * BAR);
    assert!(harness.engine.state(pad).is_recording());

    harness.command(Command::Snapshot);
    let mut count = 0;
    harness.housekeeping.snapshots.drain(|_| count += 1);
    assert_eq!(count, 0, "an unfinished take has no length yet");
}

#[test]
fn a_held_snapshot_delays_reclaiming_the_pad() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    let available = harness.engine.segments_available();
    record(&mut harness, pad, 0, 1);
    harness.command(Command::Snapshot);

    let mut held = Vec::new();
    harness.housekeeping.snapshots.drain(|s| held.push(s));
    assert_eq!(held.len(), 1);

    harness.command(Command::Clear(pad));
    assert!(
        harness.engine.segments_available() < available,
        "the reader still has the audio"
    );

    drop(held);
    harness.housekeeping.recycler.run();
    harness.run_frames(64);
    assert_eq!(harness.engine.segments_available(), available);
}

#[test]
fn a_cleared_pad_returns_its_segments_to_the_pool() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    let available = harness.engine.segments_available();
    record(&mut harness, pad, 0, 1);
    assert!(harness.engine.segments_available() < available);

    harness.command(Command::Clear(pad));
    assert_eq!(harness.engine.segments_available(), available);
    assert_eq!(harness.engine.state(pad), SlotState::Empty);

    let out = harness.run_to(harness.position() + BAR);
    assert!(
        out.iter().all(|s| *s == 0.0),
        "a cleared pad must be silent"
    );
}

#[test]
fn a_cancelled_recording_leaves_nothing_behind() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    let available = harness.engine.segments_available();
    harness.command(Command::Press(pad));
    harness.run_to(2 * BAR);
    assert!(harness.engine.segments_available() < available);

    harness.command(Command::StopAll);
    assert_eq!(harness.engine.state(pad), SlotState::Empty);
    assert_eq!(harness.engine.segments_available(), available);
}

#[test]
fn short_input_is_reported_and_recorded_as_silence() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    harness.command(Command::Press(pad));
    harness.run_to(BAR);

    // One block with nothing from the device.
    let mut out = vec![0.0; 128 * CHANNELS];
    harness.engine.process(&[], &mut out, &mut harness.events);

    let xruns: Vec<u64> = harness
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            Event::Xrun { frames } => Some(*frames),
            _ => None,
        })
        .collect();
    assert_eq!(xruns, vec![128]);
}

#[test]
fn changing_tempo_keeps_the_click_running_evenly() {
    use free_loop_core::Tempo;

    let mut harness = Harness::new(128);
    let slow = harness.engine.grid();

    // Half way between beats, the worst place for a jump to show.
    harness.run_to(BEAT + BEAT / 2);
    harness.command(Command::SetTempo(Tempo::new(180.0).unwrap()));
    let fast = harness.engine.grid();

    // The transport moved with the grid rather than staying put.
    assert_eq!(fast.beat_of(harness.engine.position()), (0, 1));
    assert_ne!(slow.frames_per_bar(), fast.frames_per_bar());

    harness.drain_events();
    let out = harness.run_to(harness.position() + fast.frames_per_bar().0);

    let beats: Vec<(u64, u32)> = harness
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            Event::Beat { bar, beat } => Some((*bar, *beat)),
            _ => None,
        })
        .collect();

    // Beat 1 already fired before the change and must not fire again on the new grid.
    assert_eq!(beats, vec![(0, 2), (0, 3), (1, 0), (1, 1)]);
    assert!(!out.is_empty());
}

#[test]
fn the_tempo_locks_once_a_clip_exists() {
    use free_loop_core::Tempo;

    let mut harness = Harness::new(128);
    harness.command(Command::SetTempo(Tempo::new(140.0).unwrap()));
    assert_eq!(harness.engine.grid().tempo().bpm(), 140.0);
    assert!(harness.drain_events().is_empty());

    let pad = addr(0, 0);
    let bar = harness.engine.grid().frames_per_bar().0;
    harness.run_to(bar / 2);
    harness.command(Command::Press(pad));
    harness.run_to(3 * bar);
    harness.command(Command::Press(pad));
    harness.run_to(3 * bar + 1);
    harness.drain_events();

    harness.command(Command::SetTempo(Tempo::new(90.0).unwrap()));
    assert!(harness.drain_events().contains(&Event::TempoRejected));
    assert_eq!(harness.engine.grid().tempo().bpm(), 140.0);
}
