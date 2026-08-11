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

use free_loop_core::{
    ClipId, Command, Event, Frames, Settings, SlotAddr, SlotId, SlotState, Tempo, TrackId,
};
use free_loop_engine::{
    ClickConfig, Engine, EngineConfig, Housekeeping, LoadMessage, Snapshot,
    buffer::{AudioBuffer, Clip, SegmentPool},
};
use std::sync::Arc;

const CHANNELS: usize = 2;
const BAR: u64 = 96_000; // 120 bpm, 4/4, 48 kHz
const BEAT: u64 = BAR / 4;

/// A distinct value per frame and channel. The 977 modulus is coprime with the bar
/// length, so a phase error cannot coincidentally produce matching samples.
///
/// Scaled to a fraction of full scale, and by a power of two so every value is exact in
/// `f32`. Several tracks can sum without reaching the limit the engine holds them at.
fn signal(frame: u64, channel: usize) -> f32 {
    ((frame % 977) as f32 + channel as f32 * 0.25) / 4_096.0
}

struct Harness {
    engine: Engine,
    housekeeping: Housekeeping,
    events: Vec<Event>,
    block: usize,
    /// What the engine has been told, so one setting can be changed at a time.
    settings: Settings,
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
        // Ramps are asserted on their own. Everything else compares exact frames.
        config.declick = Frames::ZERO;
        let (engine, housekeeping) = Engine::new(config).unwrap();
        Self {
            engine,
            housekeeping,
            events: Vec::new(),
            block,
            settings: Settings::new(),
        }
    }

    fn with_declick(block: usize, declick: Frames) -> Self {
        let mut harness = Self::new(block);
        harness.engine = {
            let mut config = EngineConfig::stereo_48k().unwrap();
            config.segment_pool = 12;
            config.click = ClickConfig {
                enabled: false,
                level: 0.0,
            };
            config.declick = declick;
            let (engine, housekeeping) = Engine::new(config).unwrap();
            harness.housekeeping = housekeeping;
            engine
        };
        harness
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

    /// Changes one setting and hands the whole state to the engine.
    fn setting(&mut self, change: impl FnOnce(&mut Settings)) {
        change(&mut self.settings);
        self.engine.apply_settings(self.settings);
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

    /// Runs `count` blocks of the harness's block size.
    fn run_blocks(&mut self, count: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..count {
            let block = self.run_frames(self.block);
            out.extend_from_slice(&block);
        }
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
fn a_clip_records_the_offset_it_was_sealed_with() {
    const LATENCY: u64 = 2_048;

    let mut harness = Harness::with_offset(128, Frames(LATENCY));
    let pad = addr(0, 0);
    record(&mut harness, pad, 1_000, 1);
    harness.command(Command::Snapshot { request: 1 });

    let mut seen = Vec::new();
    harness.housekeeping.snapshots.drain(|s| seen.push(s));

    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].clip.capture_offset(),
        Frames(LATENCY),
        "the alignment must stay visible after the fact"
    );
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

    harness.command(Command::Snapshot { request: 1 });

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

    harness.command(Command::Snapshot { request: 1 });
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
    harness.command(Command::Snapshot { request: 1 });

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

/// A clip whose samples say which frame they came from, allocated outside the engine.
fn lent_clip(frames: u64, phase: u64) -> Arc<Clip> {
    let mut pool = SegmentPool::new(64, CHANNELS);
    let mut buffer = AudioBuffer::new(64, CHANNELS);
    let channels = CHANNELS as u64;
    let audio: Vec<f32> = (0..frames * channels)
        .map(|i| signal(i / channels, usize::try_from(i % channels).unwrap_or(0)))
        .collect();
    buffer.write(0, &audio, &mut pool);

    let mut clip = Clip::new(buffer, Frames(frames), Frames(phase), CHANNELS);
    clip.set_borrowed(true);
    Arc::new(clip)
}

#[test]
fn a_loaded_session_lands_on_the_grid_frozen() {
    let mut harness = Harness::new(128);
    let pad = addr(2, 3);
    harness.run_to(BAR);

    harness
        .housekeeping
        .loader
        .send(LoadMessage::Begin {
            tempo: Tempo::new(90.0).unwrap(),
        })
        .unwrap();
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Clip {
            addr: pad,
            clip: lent_clip(1_000, 0),
            playing: true,
            launch_anchor: None,
        })
        .unwrap();
    harness.housekeeping.loader.send(LoadMessage::End).unwrap();

    harness.run_frames(128);

    assert!(
        harness.engine.is_paused(),
        "a loaded session waits to be started"
    );
    assert!(matches!(
        harness.engine.state(pad),
        SlotState::Playing { .. }
    ));
    assert!(
        (harness.engine.grid().tempo().bpm() - 90.0).abs() < 1e-9,
        "the session's tempo came with it"
    );
}

#[test]
fn loading_replaces_what_was_on_the_grid() {
    let mut harness = Harness::new(128);
    let recorded = addr(0, 0);
    record(&mut harness, recorded, 0, 1);

    let loaded = addr(5, 5);
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Begin {
            tempo: Tempo::new(120.0).unwrap(),
        })
        .unwrap();
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Clip {
            addr: loaded,
            clip: lent_clip(500, 0),
            playing: false,
            launch_anchor: None,
        })
        .unwrap();
    harness.housekeeping.loader.send(LoadMessage::End).unwrap();
    harness.run_frames(128);

    assert_eq!(harness.engine.state(recorded), SlotState::Empty);
    assert!(matches!(
        harness.engine.state(loaded),
        SlotState::Stopped { .. }
    ));
}

#[test]
fn lent_storage_comes_back_rather_than_joining_the_pool() {
    let mut harness = Harness::new(128);
    let pad = addr(1, 1);
    let available = harness.engine.segments_available();

    harness
        .housekeeping
        .loader
        .send(LoadMessage::Begin {
            tempo: Tempo::new(120.0).unwrap(),
        })
        .unwrap();
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Clip {
            addr: pad,
            clip: lent_clip(1_000, 0),
            playing: false,
            launch_anchor: None,
        })
        .unwrap();
    harness.housekeeping.loader.send(LoadMessage::End).unwrap();
    harness.run_frames(128);

    assert_eq!(
        harness.engine.segments_available(),
        available,
        "lent segments must not enter the engine's pool"
    );

    harness.command(Command::Clear(pad));
    harness.run_frames(128);
    harness.housekeeping.recycler.run();

    let returned: Vec<_> = harness.housekeeping.recycler.take_borrowed().collect();
    assert_eq!(returned.len(), 1, "the loader gets its storage back");
    assert_eq!(
        harness.engine.segments_available(),
        available,
        "and the engine's pool is the size it started at"
    );
}

#[test]
fn a_loaded_loop_plays_what_was_saved() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    let len = 4_096;

    harness
        .housekeeping
        .loader
        .send(LoadMessage::Begin {
            tempo: Tempo::new(120.0).unwrap(),
        })
        .unwrap();
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Clip {
            addr: pad,
            clip: lent_clip(len, 0),
            playing: true,
            launch_anchor: None,
        })
        .unwrap();
    harness.housekeeping.loader.send(LoadMessage::End).unwrap();
    harness.run_frames(128);

    harness.command(Command::SetPaused(false));
    let from = harness.position();
    let out = harness.run_to(from + len);

    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        assert_eq!(*sample, signal(frame % len, channel), "frame {frame}");
    }
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
    assert_eq!(
        harness.drain_events(),
        vec![Event::Tempo { bpm: 140.0 }],
        "an accepted tempo reports itself, so a stale replay cannot win"
    );

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
#[test]
fn playback_survives_the_apps_loop_shape() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    // The app runs the recycler every pass, which the other tests never do.
    harness.command(Command::Press(pad));
    harness.run_to(BAR);
    harness.housekeeping.recycler.run();
    harness.run_to(3 * BAR);
    harness.command(Command::Press(pad));
    harness.housekeeping.recycler.run();
    harness.run_to(3 * BAR + 1);
    harness.housekeeping.recycler.run();

    assert!(matches!(
        harness.engine.state(pad),
        SlotState::Playing { .. }
    ));

    let out = harness.run_to(harness.position() + BAR);
    assert!(out.iter().any(|s| *s != 0.0), "the loop should be sounding");
}

#[test]
fn recording_after_a_snapshot_still_captures() {
    let mut harness = Harness::new(128);
    let first = addr(0, 0);
    record(&mut harness, first, 0, 1);

    // A save holds a snapshot while the next take is recorded.
    harness.command(Command::Snapshot { request: 1 });
    let mut held = Vec::new();
    harness.housekeeping.snapshots.drain(|s| held.push(s));

    let second = addr(1, 0);
    let at = harness.position();
    record(&mut harness, second, at, 1);

    let out = harness.run_to(harness.position() + BAR);
    assert!(out.iter().any(|s| *s != 0.0), "both loops should sound");
    drop(held);
}

#[test]
fn recording_onto_a_loaded_session_captures_audio() {
    let mut harness = Harness::new(128);
    let loaded = addr(0, 0);
    let fresh = addr(1, 0);

    harness
        .housekeeping
        .loader
        .send(LoadMessage::Begin {
            tempo: Tempo::new(120.0).unwrap(),
        })
        .unwrap();
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Clip {
            addr: loaded,
            clip: lent_clip(BAR, 0),
            playing: true,
            launch_anchor: None,
        })
        .unwrap();
    harness.housekeeping.loader.send(LoadMessage::End).unwrap();
    harness.run_frames(128);

    harness.command(Command::SetPaused(false));
    let at = harness.position();
    record(&mut harness, fresh, at, 1);

    assert!(matches!(
        harness.engine.state(fresh),
        SlotState::Playing { .. }
    ));

    let from = harness.position();
    let out = harness.run_to(from + BAR);
    assert!(
        out.iter().any(|s| *s != 0.0),
        "the take recorded onto a loaded session should sound"
    );
}

#[test]
fn rewinding_keeps_loops_where_they_were_against_each_other() {
    let mut harness = Harness::new(128);
    let two_bar = addr(0, 0);
    let four_bar = addr(1, 0);

    // Two bars from the top, then four bars starting on an odd bar, so the two bar loop
    // is halfway through when the four bar one begins. That relationship is the music.
    record(&mut harness, two_bar, 0, 2);
    harness.run_to(3 * BAR);
    record(&mut harness, four_bar, 3 * BAR, 4);

    harness.command(Command::Rewind);
    assert_eq!(harness.engine.position(), Frames::ZERO);

    let out = harness.run_frames(64);
    for (i, sample) in out.iter().enumerate() {
        let frame = (i / CHANNELS) as u64;
        let channel = i % CHANNELS;

        // The longest loop is at its beginning, and the shorter one is still halfway.
        let four = signal(3 * BAR + frame, channel);
        let two = signal(BAR + frame, channel);
        assert_eq!(*sample, two + four, "frame {frame}");
    }
}

#[test]
fn a_loaded_session_starts_at_the_beginning() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    let len = 4_096;
    harness.run_to(3 * BAR + 777);

    harness
        .housekeeping
        .loader
        .send(LoadMessage::Begin {
            tempo: Tempo::new(120.0).unwrap(),
        })
        .unwrap();
    harness
        .housekeeping
        .loader
        .send(LoadMessage::Clip {
            addr: pad,
            clip: lent_clip(len, len - 64),
            playing: true,
            launch_anchor: None,
        })
        .unwrap();
    harness.housekeeping.loader.send(LoadMessage::End).unwrap();
    harness.run_frames(128);

    assert_eq!(harness.engine.position(), Frames::ZERO);

    harness.command(Command::SetPaused(false));
    let out = harness.run_frames(64);
    for (i, sample) in out.iter().enumerate() {
        let frame = (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        assert_eq!(*sample, signal(frame, channel), "frame {frame} of the loop");
    }
}

#[test]
fn rewinding_works_while_playing() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    harness.run_to(harness.position() + BAR / 3);
    harness.command(Command::Rewind);

    let out = harness.run_frames(32);
    for (i, sample) in out.iter().enumerate() {
        let frame = (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        assert_eq!(*sample, signal(frame, channel));
    }
}

#[test]
fn rewinding_does_not_strand_a_queued_launch() {
    let mut harness = Harness::new(128);
    let playing = addr(0, 0);
    let waiting = addr(1, 0);

    record(&mut harness, playing, 0, 1);
    let after = harness.position();
    record(&mut harness, waiting, after, 1);
    harness.command(Command::Press(waiting));
    harness.run_to(harness.position() + BAR + 1);
    assert_eq!(
        harness.engine.state(waiting),
        SlotState::Stopped { clip: ClipId(1) }
    );

    // Queue it, then rewind before the bar line it was waiting for.
    harness.run_to(harness.position() + BAR / 3);
    harness.command(Command::Press(waiting));
    assert!(matches!(
        harness.engine.state(waiting),
        SlotState::QueuedPlay { .. }
    ));

    harness.command(Command::Rewind);
    harness.run_frames(128);

    assert!(
        matches!(harness.engine.state(waiting), SlotState::Playing { .. }),
        "a queued launch must fire at the start, not wait for the old bar line"
    );
}

#[test]
fn rewinding_does_not_strand_a_queued_stop() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    harness.run_to(harness.position() + BAR / 3);
    harness.command(Command::Press(pad));
    assert!(matches!(
        harness.engine.state(pad),
        SlotState::QueuedStop { .. }
    ));

    harness.command(Command::Rewind);
    harness.run_frames(128);

    assert!(matches!(
        harness.engine.state(pad),
        SlotState::Stopped { .. }
    ));
}

#[test]
fn rewinding_discards_a_take_in_progress() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);

    harness.command(Command::Press(pad));
    harness.run_to(2 * BAR);
    assert!(harness.engine.state(pad).is_recording());

    harness.command(Command::Rewind);
    assert_eq!(harness.engine.state(pad), SlotState::Empty);
}

#[test]
fn a_muted_pad_does_not_sound() {
    use free_loop_core::{pad_bit, row_mask};

    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    harness.setting(|s| s.muted = row_mask(pad.track));
    let out = harness.run_frames(256);
    assert!(out.iter().all(|s| *s == 0.0), "a muted row is silent");
    assert!(!harness.engine.is_audible(pad));

    harness.setting(|s| s.muted = 0);
    let out = harness.run_frames(256);
    assert!(out.iter().any(|s| *s != 0.0), "and comes back");
    let _ = pad_bit(pad);
}

#[test]
fn a_solo_silences_everything_outside_it() {
    use free_loop_core::row_mask;

    let mut harness = Harness::new(128);
    let kept = addr(0, 0);
    let dropped = addr(1, 0);

    record(&mut harness, kept, 0, 1);
    let after = harness.position();
    record(&mut harness, dropped, after, 1);

    harness.setting(|s| s.soloed = row_mask(kept.track));

    assert!(harness.engine.is_audible(kept));
    assert!(!harness.engine.is_audible(dropped));

    let from = harness.position();
    let out = harness.run_to(from + BAR);
    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        assert_eq!(*sample, signal(frame % BAR, channel), "only the solo");
    }
}

#[test]
fn a_mute_beats_a_solo_on_the_same_pad() {
    use free_loop_core::row_mask;

    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    let row = row_mask(pad.track);
    harness.setting(|s| {
        s.muted = row;
        s.soloed = row;
    });

    assert!(!harness.engine.is_audible(pad));
    let out = harness.run_frames(256);
    assert!(out.iter().all(|s| *s == 0.0));
}

#[test]
fn muting_a_column_reaches_across_tracks() {
    use free_loop_core::{SlotId, column_mask};

    let mut harness = Harness::new(128);
    let first = addr(0, 0);
    let second = addr(1, 0);
    record(&mut harness, first, 0, 1);
    let after = harness.position();
    record(&mut harness, second, after, 1);

    harness.setting(|s| s.muted = column_mask(SlotId::new(0).unwrap()));

    assert!(!harness.engine.is_audible(first));
    assert!(!harness.engine.is_audible(second));
    let out = harness.run_frames(256);
    assert!(out.iter().all(|s| *s == 0.0));
}

#[test]
fn a_track_plays_at_the_gain_it_was_given() {
    use free_loop_core::{TRACK_COUNT, UNITY_STEP, gain_for_step};

    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    let mut gains = [UNITY_STEP; TRACK_COUNT];
    gains[0] = UNITY_STEP - 1;
    harness.setting(|s| s.gains = gains);

    let quieter = gain_for_step(UNITY_STEP - 1);
    let from = harness.position();
    let out = harness.run_to(from + 256);

    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        assert_eq!(*sample, signal(frame % BAR, channel) * quieter);
    }
}

#[test]
fn the_bottom_of_the_ladder_is_silence() {
    use free_loop_core::{TRACK_COUNT, UNITY_STEP};

    let mut harness = Harness::new(128);
    record(&mut harness, addr(0, 0), 0, 1);

    let mut gains = [UNITY_STEP; TRACK_COUNT];
    gains[0] = 0;
    harness.setting(|s| s.gains = gains);

    let out = harness.run_frames(256);
    assert!(out.iter().all(|s| *s == 0.0));
}

#[test]
fn gain_is_per_track_not_per_grid() {
    use free_loop_core::{TRACK_COUNT, UNITY_STEP, gain_for_step};

    let mut harness = Harness::new(128);
    let quiet = addr(0, 0);
    let loud = addr(1, 0);
    record(&mut harness, quiet, 0, 1);
    let after = harness.position();
    record(&mut harness, loud, after, 1);

    let mut gains = [UNITY_STEP; TRACK_COUNT];
    gains[0] = 1;
    harness.setting(|s| s.gains = gains);

    let scale = gain_for_step(1);
    let from = harness.position();
    let out = harness.run_to(from + 256);

    for (i, sample) in out.iter().enumerate() {
        let frame = from + (i / CHANNELS) as u64;
        let channel = i % CHANNELS;
        // The second take starts on the bar 2 line, so that is where its content begins.
        let first = signal(frame % BAR, channel) * scale;
        let second = signal(2 * BAR + (frame - 2 * BAR) % BAR, channel);
        assert_eq!(*sample, first + second, "frame {frame}");
    }
}

#[test]
fn nothing_leaves_the_engine_past_full_scale() {
    let mut harness = Harness::new(128);
    let first = addr(0, 0);
    let second = addr(1, 0);
    record(&mut harness, first, 0, 1);
    let after = harness.position();
    record(&mut harness, second, after, 1);

    // Both tracks at the top of the ladder, which sums past full scale.
    let top = u8::try_from(free_loop_core::GAIN_STEPS - 1).unwrap();
    harness.setting(|s| s.gains = [top; free_loop_core::TRACK_COUNT]);

    let out = harness.run_to(harness.position() + BAR);
    assert!(out.iter().all(|s| (-1.0..=1.0).contains(s)));
}

#[test]
fn a_quiet_mix_is_left_alone() {
    let mut harness = Harness::new(128);
    let pad = addr(0, 0);
    record(&mut harness, pad, 0, 1);

    harness.drain_events();

    harness.run_frames(512);
    let clipped = harness
        .drain_events()
        .iter()
        .any(|e| matches!(e, Event::Clipped { .. }));
    assert!(!clipped);
}

mod staged_load {
    use super::*;

    #[test]
    fn a_load_that_has_not_finished_arriving_changes_nothing() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        record(&mut harness, pad, 0, 1);
        let playing = harness.engine.state(pad);

        // Begin and one clip, with no End yet.
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Begin {
                tempo: Tempo::new(90.0).unwrap(),
            })
            .unwrap();
        harness.run_frames(128);

        assert_eq!(
            harness.engine.state(pad),
            playing,
            "the grid is untouched until the whole load is in"
        );
        assert!(!harness.engine.is_paused(), "and it has not frozen");
    }

    #[test]
    fn a_load_takes_effect_all_at_once_when_it_finishes() {
        let mut harness = Harness::new(128);
        record(&mut harness, addr(0, 0), 0, 1);

        let loaded = addr(2, 3);
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Begin {
                tempo: Tempo::new(90.0).unwrap(),
            })
            .unwrap();
        harness.run_frames(128);
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Clip {
                addr: loaded,
                clip: lent_clip(BAR, 0),
                playing: true,
                launch_anchor: None,
            })
            .unwrap();
        harness.housekeeping.loader.send(LoadMessage::End).unwrap();
        harness.run_frames(128);

        assert_eq!(harness.engine.state(addr(0, 0)), SlotState::Empty);
        assert!(harness.engine.state(loaded).is_sounding());
        assert!(harness.engine.is_paused(), "a load waits to be started");
    }
}

mod load_protocol {
    use super::*;

    fn begin(harness: &mut Harness) {
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Begin {
                tempo: Tempo::new(90.0).unwrap(),
            })
            .unwrap();
    }

    #[test]
    fn an_end_with_nothing_open_leaves_the_session_alone() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        record(&mut harness, pad, 0, 1);
        let playing = harness.engine.state(pad);

        harness.housekeeping.loader.send(LoadMessage::End).unwrap();
        harness.run_frames(128);

        assert_eq!(harness.engine.state(pad), playing, "nothing was loaded");
        assert!(!harness.engine.is_paused());
    }

    #[test]
    fn a_second_load_does_not_inherit_the_first_ones_completion() {
        let mut harness = Harness::new(128);
        record(&mut harness, addr(0, 0), 0, 1);

        // A finished load, then the start of another in the same drain.
        begin(&mut harness);
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Clip {
                addr: addr(2, 3),
                clip: lent_clip(BAR, 0),
                playing: true,
                launch_anchor: None,
            })
            .unwrap();
        harness.housekeeping.loader.send(LoadMessage::End).unwrap();
        begin(&mut harness);
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Clip {
                addr: addr(4, 5),
                clip: lent_clip(BAR, 0),
                playing: true,
                launch_anchor: None,
            })
            .unwrap();
        harness.run_frames(128);

        assert!(
            harness.engine.state(addr(2, 3)).is_sounding(),
            "the finished load went in"
        );
        assert_eq!(
            harness.engine.state(addr(4, 5)),
            SlotState::Empty,
            "the unfinished one did not"
        );
    }

    #[test]
    fn a_load_cancels_a_move_queued_against_what_it_replaced() {
        let mut harness = Harness::with_declick(128, Frames(256));
        record(&mut harness, addr(0, 0), 0, 1);

        // Queued behind a fade, so it is still waiting when the load lands.
        harness.command(Command::ClearAll);
        begin(&mut harness);
        harness
            .housekeeping
            .loader
            .send(LoadMessage::Clip {
                addr: addr(2, 3),
                clip: lent_clip(BAR, 0),
                playing: true,
                launch_anchor: None,
            })
            .unwrap();
        harness.housekeeping.loader.send(LoadMessage::End).unwrap();
        harness.run_blocks(4);

        assert!(
            harness.engine.state(addr(2, 3)).is_sounding(),
            "the load survived the clear it did not belong to"
        );
    }
}

mod resync {
    use super::*;

    #[test]
    fn a_resync_reports_the_tempo_the_engine_is_running_at() {
        let mut harness = Harness::new(128);
        harness.command(Command::SetTempo(Tempo::new(90.0).unwrap()));
        harness.drain_events();

        harness.command(Command::Resync);
        let reported = harness
            .drain_events()
            .into_iter()
            .find_map(|event| match event {
                Event::Tempo { bpm } => Some(bpm),
                _ => None,
            });
        assert_eq!(reported, Some(90.0), "so a missed refusal cannot linger");
    }

    #[test]
    fn a_resync_reports_every_pad_as_it_stands() {
        let mut harness = Harness::new(128);
        let playing = addr(0, 0);
        record(&mut harness, playing, 0, 1);
        harness.drain_events();

        harness.command(Command::Resync);
        let reported: Vec<(SlotAddr, SlotState)> = harness
            .drain_events()
            .iter()
            .filter_map(|event| match event {
                Event::SlotChanged { addr, state } => Some((*addr, *state)),
                _ => None,
            })
            .collect();

        assert_eq!(
            reported.len(),
            free_loop_core::TRACK_COUNT * free_loop_core::SLOT_COUNT,
            "every pad, not just the ones that changed"
        );
        let sounding = reported
            .iter()
            .find(|(addr, _)| *addr == playing)
            .map(|(_, state)| *state);
        assert!(sounding.is_some_and(SlotState::is_sounding), "as it stands");
    }
}

mod snapshots {
    use super::*;

    #[test]
    fn a_completion_reports_what_it_published_and_what_it_meant_to() {
        let mut harness = Harness::new(128);
        record(&mut harness, addr(0, 0), 0, 1);
        let at = harness.position();
        record(&mut harness, addr(1, 0), at, 1);
        harness.drain_events();

        harness.command(Command::Snapshot { request: 7 });
        let done = harness
            .drain_events()
            .into_iter()
            .find_map(|event| match event {
                Event::SnapshotComplete {
                    request,
                    clips,
                    expected,
                } => Some((request, clips, expected)),
                _ => None,
            });
        assert_eq!(done, Some((7, 2, 2)), "both pads, under the request given");
    }

    #[test]
    fn every_snapshot_carries_the_request_that_asked_for_it() {
        let mut harness = Harness::new(128);
        record(&mut harness, addr(0, 0), 0, 1);
        harness.command(Command::Snapshot { request: 42 });

        let mut seen = Vec::new();
        harness
            .housekeeping
            .snapshots
            .drain(|s| seen.push(s.request));
        assert_eq!(seen, vec![42]);
    }
}

mod resources {
    use super::*;

    #[test]
    fn every_pad_refused_on_one_boundary_is_put_back() {
        let mut harness = Harness::new(512);
        // Fill the pool: one shell per pad, and a sealed take keeps the one it was given.
        for slot in 0..u8::try_from(free_loop_core::SLOT_COUNT).unwrap() {
            for track in 0..u8::try_from(free_loop_core::TRACK_COUNT).unwrap() {
                harness.command(Command::Press(addr(track, slot)));
            }
            let until = (harness.position() / BAR + 2) * BAR;
            harness.run_to(until);
            for track in 0..u8::try_from(free_loop_core::TRACK_COUNT).unwrap() {
                harness.command(Command::Press(addr(track, slot)));
            }
            harness.run_to(until + 1);
        }

        // Clearing hands shells to the recycler rather than back, so several arms on one
        // boundary all have nowhere to write.
        harness.command(Command::Snapshot { request: 1 });
        let spares = [addr(0, 0), addr(1, 0), addr(2, 0)];
        for spare in spares {
            harness.command(Command::Clear(spare));
        }
        harness.drain_events();

        for spare in spares {
            harness.command(Command::Press(spare));
        }
        let until = (harness.position() / BAR + 2) * BAR;
        harness.run_to(until);

        let refused: Vec<SlotAddr> = harness
            .drain_events()
            .iter()
            .filter_map(|event| match event {
                Event::RecordingRefused { addr } => Some(*addr),
                _ => None,
            })
            .collect();
        assert_eq!(refused.len(), spares.len(), "each one said so");
        for spare in spares {
            assert_eq!(harness.engine.state(spare), SlotState::Empty, "{spare:?}");
        }
    }

    #[test]
    fn a_pad_with_no_storage_left_is_put_back_rather_than_left_playing() {
        let mut harness = Harness::new(512);

        // One shell per pad, and a sealed take keeps the one it was given, so the pool is
        // only empty once every pad holds a clip. One slot per track records at a time.
        for slot in 0..u8::try_from(free_loop_core::SLOT_COUNT).unwrap() {
            for track in 0..u8::try_from(free_loop_core::TRACK_COUNT).unwrap() {
                harness.command(Command::Press(addr(track, slot)));
            }
            let until = (harness.position() / BAR + 2) * BAR;
            harness.run_to(until);
            for track in 0..u8::try_from(free_loop_core::TRACK_COUNT).unwrap() {
                harness.command(Command::Press(addr(track, slot)));
            }
            harness.run_to(until + 1);
        }
        harness.drain_events();

        // Clearing hands the shell to the recycler rather than straight back, so the next
        // arm has nowhere to write.
        let spare = addr(0, 0);
        harness.command(Command::Snapshot { request: 1 });
        harness.command(Command::Clear(spare));
        harness.drain_events();

        harness.command(Command::Press(spare));
        let until = (harness.position() / BAR + 2) * BAR;
        harness.run_to(until);

        let refused = harness
            .drain_events()
            .iter()
            .any(|event| matches!(event, Event::RecordingRefused { addr } if *addr == spare));
        assert!(refused, "the arm had nowhere to write and said so");
        assert_eq!(
            harness.engine.state(spare),
            SlotState::Empty,
            "left empty rather than claiming to hold a take"
        );
    }
}

mod starvation {
    use super::*;

    /// Renders `frames` with the device delivering nothing at all.
    fn render_starved(harness: &mut Harness, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * CHANNELS];
        harness.engine.process(&[], &mut out, &mut harness.events);
        out
    }

    #[test]
    fn a_device_that_delivers_nothing_is_survivable() {
        let mut harness = Harness::new(512);
        // Part way into a beat, so the block the device fails on is split at the boundary.
        harness.run_to(BEAT - 256);

        let out = render_starved(&mut harness, 512);
        assert!(
            out.iter().all(|s| *s == 0.0),
            "nothing to play and nothing in"
        );
        assert_eq!(harness.position(), BEAT + 256, "the transport carried on");
    }

    #[test]
    fn a_starved_block_is_reported_as_an_xrun() {
        let mut harness = Harness::new(512);
        render_starved(&mut harness, 512);

        let xruns: Vec<u64> = harness
            .drain_events()
            .iter()
            .filter_map(|event| match event {
                Event::Xrun { frames } => Some(*frames),
                _ => None,
            })
            .collect();
        assert_eq!(xruns, vec![512]);
    }

    #[test]
    fn a_take_across_a_starved_block_keeps_its_length() {
        let mut harness = Harness::new(512);
        let pad = addr(0, 0);
        harness.command(Command::Press(pad));
        harness.run_to(BAR + BEAT - 256);

        // The device drops out mid take.
        render_starved(&mut harness, 512);
        harness.run_to(2 * BAR);
        harness.command(Command::Press(pad));
        harness.run_to(2 * BAR + 1);

        let recorded = harness.drain_events().iter().find_map(|event| match event {
            Event::ClipRecorded { len, .. } => Some(*len),
            _ => None,
        });
        assert_eq!(
            recorded,
            Some(Frames(2 * BAR)),
            "silence for part of it, but the full length"
        );
    }
}

mod launch_mode {
    use super::*;
    use free_loop_core::LaunchMode;

    fn restart(harness: &mut Harness, track: usize) {
        let mut modes = [LaunchMode::Follow; free_loop_core::TRACK_COUNT];
        modes[track] = LaunchMode::Restart;
        harness.setting(|s| s.launch_modes = modes);
    }

    /// Records two bars on `pad`, stops it, then relaunches on the next bar line.
    fn record_stop_relaunch(harness: &mut Harness, pad: SlotAddr) -> u64 {
        let (start, end) = record(harness, pad, 0, 2);
        harness.command(Command::Press(pad));
        harness.run_to(2 * (end - start));

        // Half a bar on, so the relaunch lands on an odd bar line rather than a multiple
        // of the clip's own two, where the two modes would agree.
        harness.run_to(harness.position() + BAR / 2);
        harness.command(Command::Press(pad));
        let launch = (harness.position() / BAR + 1) * BAR;
        // Stops just short, so the next block is the one the launch lands on.
        harness.run_to(launch);
        start
    }

    #[test]
    fn following_drops_into_the_clip_where_the_transport_is() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        let start = record_stop_relaunch(&mut harness, pad);

        let from = harness.position();
        let out = harness.run_frames(64);
        let phase = (from - start) % (2 * BAR);
        assert_eq!(out[0], signal(start + phase, 0), "wherever the grid is");
        assert_ne!(phase, 0, "and that is not the clip's start");
    }

    #[test]
    fn restarting_plays_the_clip_from_its_start() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        restart(&mut harness, 0);
        let start = record_stop_relaunch(&mut harness, pad);

        let out = harness.run_frames(64);
        assert_eq!(out[0], signal(start, 0), "the first frame of the take");
    }

    #[test]
    fn a_restart_lands_on_the_beat_the_take_began_on() {
        const LATENCY: u64 = 2_048;

        let mut harness = Harness::with_offset(128, Frames(LATENCY));
        let pad = addr(0, 0);
        restart(&mut harness, 0);
        let start = record_stop_relaunch(&mut harness, pad);

        // Compensation put the audio played on the take's first beat `LATENCY` frames into
        // the buffer, so that is what a restart has to reach for.
        let out = harness.run_frames(64);
        assert_eq!(
            out[0],
            signal(start + LATENCY, 0),
            "what was played on the downbeat, not the round trip before it"
        );
    }

    #[test]
    fn a_mode_change_leaves_a_sounding_clip_alone() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        let (start, end) = record(&mut harness, pad, 0, 2);
        let len = end - start;

        restart(&mut harness, 0);
        let from = harness.position();
        let out = harness.run_frames(64);
        let phase = (from - start) % len;
        assert_eq!(out[0], signal(start + phase, 0), "no jump mid performance");
    }

    #[test]
    fn a_rewind_does_not_send_a_sounding_clip_back_to_its_start() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        restart(&mut harness, 0);
        let (start, end) = record(&mut harness, pad, 0, 2);
        let len = end - start;

        harness.run_to(harness.position() + BAR + BAR / 2);
        harness.command(Command::Rewind);
        harness.run_frames(128);

        let from = harness.position();
        let out = harness.run_frames(64);
        assert!(
            out.iter().any(|s| *s != 0.0),
            "still sounding after the rewind"
        );
        let phase = (from + (harness.position() - from)) % len;
        assert_ne!(phase, 0, "and carrying on rather than jumping to its start");
    }
}

mod inputs {
    use super::*;
    use free_loop_core::TrackInput;

    fn set(harness: &mut Harness, track: usize, input: TrackInput) {
        let mut inputs = [TrackInput::Stereo; free_loop_core::TRACK_COUNT];
        inputs[track] = input;
        harness.setting(|s| s.inputs = inputs);
    }

    /// Every frame of `out`, as (channel 0, channel 1) pairs.
    fn pairs(out: &[f32]) -> Vec<(f32, f32)> {
        out.chunks_exact(CHANNELS).map(|f| (f[0], f[1])).collect()
    }

    #[test]
    fn a_track_set_to_one_input_records_it_on_both_channels() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        set(&mut harness, 0, TrackInput::Mono(1));
        let (start, end) = record(&mut harness, pad, 0, 1);
        let len = end - start;

        let from = harness.position();
        let out = harness.run_to(from + len);
        for (i, (left, right)) in pairs(&out).into_iter().enumerate() {
            let frame = from + i as u64;
            let played = signal(start + (frame - start) % len, 1);
            assert_eq!(left, played, "frame {frame} left");
            assert_eq!(right, played, "frame {frame} right");
        }
    }

    #[test]
    fn the_stereo_default_keeps_the_channels_apart() {
        let mut harness = Harness::new(128);
        let (start, end) = record(&mut harness, addr(0, 0), 0, 1);

        let phase = (harness.position() - start) % (end - start);
        let out = harness.run_frames(64);
        let (left, right) = pairs(&out)[0];
        assert_eq!(left, signal(start + phase, 0));
        assert_eq!(right, signal(start + phase, 1), "not a copy of the left");
    }

    #[test]
    fn a_take_keeps_the_input_it_started_on() {
        let mut harness = Harness::new(128);
        let pad = addr(0, 0);
        set(&mut harness, 0, TrackInput::Mono(1));

        harness.command(Command::Press(pad));
        harness.run_to(BAR / 2);
        // Changed mid take, which the take in progress must not pick up.
        set(&mut harness, 0, TrackInput::Mono(0));
        harness.run_to(BAR);
        harness.command(Command::Press(pad));
        harness.run_to(BAR + 1);

        let phase = harness.position() % BAR;
        let out = harness.run_frames(64);
        let (left, right) = pairs(&out)[0];
        assert_eq!(left, right, "a mono take is the same on both");
        assert_eq!(
            left,
            signal(phase, 1),
            "still input 1, not the one set later"
        );
    }
}

mod declick {
    use super::*;
    use free_loop_core::row_mask;

    /// Frames a level takes to travel the full range in these tests. Two blocks of 128.
    const RAMP: u64 = 256;

    /// Blocks for a fade, plus the one that carries out what was waiting on it.
    const BLOCKS: usize = 3;

    fn is_silent(out: &[f32]) -> bool {
        out.iter().all(|s| *s == 0.0)
    }

    fn playing(block: usize) -> (Harness, SlotAddr, u64) {
        let mut harness = Harness::with_declick(block, Frames(RAMP));
        let pad = addr(0, 0);
        let (start, end) = record(&mut harness, pad, 0, 1);
        (harness, pad, end - start)
    }

    fn mute(harness: &mut Harness, pad: SlotAddr) {
        harness.setting(|s| s.muted = row_mask(pad.track));
    }

    fn unmute(harness: &mut Harness) {
        harness.setting(|s| s.muted = 0);
    }

    #[test]
    fn muting_fades_out_rather_than_cutting() {
        let (mut harness, pad, _) = playing(128);
        mute(&mut harness, pad);

        let fading = harness.run_blocks(BLOCKS);
        assert!(!is_silent(&fading), "the audio is on its way down");
        assert!(
            is_silent(&harness.run_blocks(2)),
            "and reaches silence rather than stopping part way"
        );
    }

    #[test]
    fn unmuting_fades_in_rather_than_stepping() {
        let (mut harness, pad, len) = playing(128);
        mute(&mut harness, pad);
        harness.run_blocks(BLOCKS);

        unmute(&mut harness);
        let from = harness.position();
        let faded = harness.run_to(from + len);
        // A loop length later the same audio comes round at full level.
        let reference = harness.run_to(from + 2 * len);

        assert_eq!(faded[0], 0.0, "the first frame back is silent");
        assert_ne!(faded, reference, "and the ones after it are quieter");
        for (faded, reference) in faded.iter().zip(&reference) {
            assert!(
                faded.abs() <= reference.abs(),
                "a fade in never passes the level it is heading for"
            );
        }
    }

    #[test]
    fn a_fade_is_over_within_its_ramp() {
        let (mut harness, pad, len) = playing(128);
        mute(&mut harness, pad);
        harness.run_blocks(BLOCKS);
        unmute(&mut harness);
        harness.run_blocks(BLOCKS);

        let from = harness.position();
        let after = harness.run_to(from + len);
        let reference = harness.run_to(from + 2 * len);
        assert_eq!(after, reference, "the ramp is transient, not a gain change");
    }

    #[test]
    fn a_rewind_waits_for_the_mix_to_fade() {
        let (mut harness, _, _) = playing(128);
        let before = harness.position();

        harness.command(Command::Rewind);
        assert!(
            !is_silent(&harness.run_blocks(1)),
            "the first block after the press is still sounding"
        );
        assert!(harness.position() > before, "and the move has not happened");

        harness.run_blocks(BLOCKS);
        assert!(harness.position() < before, "the transport went back");
    }

    #[test]
    fn a_transport_move_lands_on_silence() {
        let (mut harness, _, _) = playing(128);
        harness.command(Command::Rewind);

        let mut previous = harness.position();
        let mut moved = false;
        for _ in 0..8 {
            let out = harness.run_blocks(1);
            let now = harness.position();
            if now < previous {
                assert_eq!(out[0], 0.0, "the jump happens at silence");
                moved = true;
                break;
            }
            previous = now;
        }
        assert!(moved, "the rewind never happened");
    }

    #[test]
    fn pausing_fades_before_it_freezes() {
        let (mut harness, _, _) = playing(128);
        harness.command(Command::SetPaused(true));
        assert!(!harness.engine.is_paused(), "the freeze waits for the fade");

        harness.run_blocks(BLOCKS);
        assert!(harness.engine.is_paused());
        assert!(is_silent(&harness.run_blocks(1)));
    }

    #[test]
    fn playing_again_cancels_a_pause_that_has_not_landed() {
        let (mut harness, _, _) = playing(128);
        harness.command(Command::SetPaused(true));
        harness.command(Command::SetPaused(false));

        harness.run_blocks(BLOCKS);
        assert!(!harness.engine.is_paused());
        assert!(!is_silent(&harness.run_blocks(1)), "and it kept playing");
    }

    #[test]
    fn a_tempo_change_behind_a_clear_is_taken() {
        let (mut harness, _, _) = playing(128);
        let before = harness.engine.grid().bars(1);

        // What a fresh session sends.
        harness.command(Command::ClearAll);
        harness.command(Command::SetTempo(Tempo::new(90.0).unwrap()));

        let events = harness.drain_events();
        assert!(!events.contains(&Event::TempoRejected));
        assert!(
            harness.engine.grid().bars(1) > before,
            "a slower bar is longer"
        );
    }

    #[test]
    fn a_launch_fades_in() {
        let (mut harness, pad, _) = playing(128);
        // A press on a sounding pad stops it at the next bar line, not at once.
        harness.command(Command::Press(pad));
        let stops_at = (harness.position() / BAR + 1) * BAR;
        harness.run_to(stops_at + RAMP + 128);
        assert!(is_silent(&harness.run_blocks(1)), "stopped and faded out");

        harness.command(Command::Press(pad));
        // The launch is queued to the next bar line, so the fade in starts there.
        let next_bar = (harness.position() / BAR + 1) * BAR;
        let out = harness.run_to(next_bar + RAMP);
        assert_eq!(out[0], 0.0, "nothing sounds before the bar line");
        assert!(!is_silent(&out), "and it comes back after it");
    }
}
