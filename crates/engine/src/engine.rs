//! The realtime engine.
//!
//! [`Engine::process`] is the callback body. It never allocates: all audio memory comes
//! from pools built in [`Engine::new`], and retired clips return their segments to those
//! pools rather than being dropped.
//!
//! Blocks are split at beat boundaries. When a boundary falls inside the block the run
//! is cut there, the transition is applied, and rendering resumes — so a launch lands on
//! the exact frame it was scheduled for rather than at the next block edge.

use free_loop_core::{
    BarGrid, ClipId, Command, Ctx, Effect, Event, Frames, MIN_BPM, SLOT_COUNT, SampleRate,
    SessionModel, SlotAddr, SlotState, TRACK_COUNT, Tempo, TimeError, TimeSignature,
};

use crate::buffer::{AudioBuffer, Clip, SEGMENT_FRAMES, SegmentPool};
use crate::click::{Click, ClickConfig};

/// Saturating `u64` to `usize`.
fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Widening `usize` to `u64`.
fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The engine could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The configured musical time was not valid.
    #[error(transparent)]
    Time(#[from] TimeError),
    /// Channel count was zero.
    #[error("channel count must be greater than zero")]
    ZeroChannels,
    /// Recordings were configured to be zero bars long.
    #[error("maximum recording length must be at least one bar")]
    ZeroMaxBars,
    /// The segment pool held no segments.
    #[error("the segment pool must hold at least one segment")]
    EmptyPool,
}

/// How the engine is set up. Fixed for the life of the engine except for the tempo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// Sample rate of the audio device.
    pub sample_rate: SampleRate,
    /// Starting tempo.
    pub tempo: Tempo,
    /// Time signature.
    pub time_signature: TimeSignature,
    /// Channels in the interleaved input and output buffers.
    pub channels: usize,
    /// Longest recording allowed, in bars.
    pub max_bars: u32,
    /// Segments to allocate. This is the ceiling on total recorded audio.
    pub segment_pool: usize,
    /// Click settings.
    pub click: ClickConfig,
}

impl EngineConfig {
    /// A stereo 48 kHz setup at 120 bpm, with roughly 90 seconds of recording space.
    ///
    /// # Errors
    ///
    /// [`EngineError::Time`] never occurs for these values, but the constructors are
    /// fallible so the result is propagated.
    pub fn stereo_48k() -> Result<Self, EngineError> {
        Ok(Self {
            sample_rate: SampleRate::new(48_000)?,
            tempo: Tempo::new(120.0)?,
            time_signature: TimeSignature::FOUR_FOUR,
            channels: 2,
            max_bars: 32,
            segment_pool: 64,
            click: ClickConfig::default(),
        })
    }
}

/// A destination for engine reports.
///
/// Implemented for any `FnMut(Event)` and for `Vec<Event>`, so tests can collect and the
/// audio thread can push straight onto a ring.
pub trait EventSink {
    /// Records one event.
    fn event(&mut self, event: Event);
}

impl<F: FnMut(Event)> EventSink for F {
    fn event(&mut self, event: Event) {
        self(event);
    }
}

impl EventSink for Vec<Event> {
    fn event(&mut self, event: Event) {
        self.push(event);
    }
}

/// Capture in progress.
#[derive(Debug)]
struct Recording {
    buffer: AudioBuffer,
    /// Frames of transport elapsed since capture began, written or not.
    frames: u64,
    /// Whether the segment pool ran dry part way through.
    starved: bool,
}

/// Everything that owns audio memory.
///
/// Split out from [`Engine`] so effects can be applied while [`SessionModel`] is
/// mutably borrowed.
#[derive(Debug)]
struct Audio {
    clips: [[Option<Clip>; SLOT_COUNT]; TRACK_COUNT],
    recordings: [[Option<Recording>; SLOT_COUNT]; TRACK_COUNT],
    shells: Vec<AudioBuffer>,
    segments: SegmentPool,
    next_clip_id: ClipId,
    channels: usize,
}

impl Audio {
    fn clip(&self, addr: SlotAddr) -> Option<&Clip> {
        self.clips[addr.track.index()][addr.slot.index()].as_ref()
    }

    fn recycle(&mut self, mut buffer: AudioBuffer) {
        buffer.drain_into(&mut self.segments);
        self.shells.push(buffer);
    }

    fn apply(&mut self, addr: SlotAddr, effect: Effect, sink: &mut impl EventSink) {
        match effect {
            Effect::StartCapture { .. } => {
                // The shell pool holds one buffer per pad, so this cannot come up empty.
                if let Some(buffer) = self.shells.pop() {
                    self.recordings[addr.track.index()][addr.slot.index()] = Some(Recording {
                        buffer,
                        frames: 0,
                        starved: false,
                    });
                }
            }

            Effect::FinishCapture {
                clip,
                started_at,
                at,
            } => {
                let Some(recording) = self.recordings[addr.track.index()][addr.slot.index()].take()
                else {
                    return;
                };
                let len = at.saturating_sub(started_at);
                self.clips[addr.track.index()][addr.slot.index()] =
                    Some(Clip::new(recording.buffer, len, started_at, self.channels));
                self.next_clip_id = clip.next();
                sink.event(Event::ClipRecorded { addr, clip, len });
            }

            Effect::CancelCapture => {
                if let Some(recording) =
                    self.recordings[addr.track.index()][addr.slot.index()].take()
                {
                    self.recycle(recording.buffer);
                }
            }

            Effect::ReleaseClip { clip } => {
                if let Some(held) = self.clips[addr.track.index()][addr.slot.index()].take() {
                    let buffer = held.into_buffer(&mut self.segments);
                    self.shells.push(buffer);
                }
                sink.event(Event::ClipReleased { clip });
            }

            // Playback is a function of slot state and transport position, so there is
            // no voice to start or stop.
            Effect::StartPlayback { .. } | Effect::StopPlayback { .. } => {}
        }
    }
}

/// The realtime engine.
#[derive(Debug)]
pub struct Engine {
    grid: BarGrid,
    position: Frames,
    session: SessionModel,
    audio: Audio,
    click: Click,
    channels: usize,
    max_bars: u32,
    sample_rate: SampleRate,
    time_signature: TimeSignature,
}

impl Engine {
    /// Builds an engine and allocates its pools.
    ///
    /// # Errors
    ///
    /// [`EngineError`] if the configuration is not usable.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        if config.channels == 0 {
            return Err(EngineError::ZeroChannels);
        }
        if config.max_bars == 0 {
            return Err(EngineError::ZeroMaxBars);
        }
        if config.segment_pool == 0 {
            return Err(EngineError::EmptyPool);
        }

        let grid = BarGrid::new(config.sample_rate, config.tempo, config.time_signature)?;

        // Size the segment arrays for the slowest tempo the transport accepts, so a
        // later tempo change can never outgrow a buffer that is already allocated.
        let slowest = BarGrid::new(
            config.sample_rate,
            Tempo::new(MIN_BPM)?,
            config.time_signature,
        )?;
        let longest = slowest.bars(config.max_bars).0;
        let max_segments = as_usize(longest.div_ceil(as_u64(SEGMENT_FRAMES))).max(1);

        let shells = (0..TRACK_COUNT * SLOT_COUNT)
            .map(|_| AudioBuffer::new(max_segments, config.channels))
            .collect();

        Ok(Self {
            grid,
            position: Frames::ZERO,
            session: SessionModel::new(),
            audio: Audio {
                clips: core::array::from_fn(|_| core::array::from_fn(|_| None)),
                recordings: core::array::from_fn(|_| core::array::from_fn(|_| None)),
                shells,
                segments: SegmentPool::new(config.segment_pool, config.channels),
                next_clip_id: ClipId(0),
                channels: config.channels,
            },
            click: Click::new(config.click, config.sample_rate),
            channels: config.channels,
            max_bars: config.max_bars,
            sample_rate: config.sample_rate,
            time_signature: config.time_signature,
        })
    }

    /// The transport position, in frames from the origin.
    pub fn position(&self) -> Frames {
        self.position
    }

    /// The current bar grid.
    pub fn grid(&self) -> BarGrid {
        self.grid
    }

    /// What a pad is doing.
    pub fn state(&self, addr: SlotAddr) -> SlotState {
        self.session.state(addr)
    }

    /// Segments still available for recording.
    pub fn segments_available(&self) -> usize {
        self.audio.segments.available()
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            now: self.position,
            grid: self.grid,
            max_bars: self.max_bars,
            next_clip_id: self.audio.next_clip_id,
        }
    }

    /// Applies a control instruction.
    ///
    /// Instructions take effect at the position the transport has reached, so they land
    /// at the start of the block they are drained in. That granularity only matters for
    /// a press within one block of a bar line.
    pub fn handle(&mut self, command: Command, sink: &mut impl EventSink) {
        let before = self.session;
        let ctx = self.ctx();

        match command {
            Command::Press(addr) => self.with_session(|session, audio| {
                session.press(addr, &ctx, &mut |a, e| audio.apply(a, e, sink));
            }),
            Command::Clear(addr) => self.with_session(|session, audio| {
                session.clear(addr, &ctx, &mut |a, e| audio.apply(a, e, sink));
            }),
            Command::StopTrack(track) => self.with_session(|session, audio| {
                session.stop_track(track, &ctx, &mut |a, e| audio.apply(a, e, sink));
            }),
            Command::StopAll => self.with_session(|session, audio| {
                session.stop_all(&ctx, &mut |a, e| audio.apply(a, e, sink));
            }),
            Command::SetClickEnabled(enabled) => self.click.set_enabled(enabled),
            Command::SetClickLevel(level) => self.click.set_level(level),
            Command::SetTempo(tempo) => self.set_tempo(tempo, sink),
        }

        self.emit_changes(&before, sink);
    }

    fn with_session(&mut self, apply: impl FnOnce(&mut SessionModel, &mut Audio)) {
        let Self { session, audio, .. } = self;
        apply(session, audio);
    }

    fn set_tempo(&mut self, tempo: Tempo, sink: &mut impl EventSink) {
        if self.session.has_any_clip() {
            sink.event(Event::TempoRejected);
            return;
        }
        match BarGrid::new(self.sample_rate, tempo, self.time_signature) {
            Ok(grid) => self.grid = grid,
            Err(_) => sink.event(Event::TempoRejected),
        }
    }

    /// Renders one block.
    ///
    /// `output` is interleaved and fully overwritten. `input` is interleaved with the
    /// same channel count; if it is short, the missing frames are treated as silence and
    /// an [`Event::Xrun`] is reported.
    pub fn process(&mut self, input: &[f32], output: &mut [f32], sink: &mut impl EventSink) {
        output.fill(0.0);

        let frames = output.len() / self.channels;
        let input_frames = input.len() / self.channels;
        if input_frames < frames {
            sink.event(Event::Xrun {
                frames: as_u64(frames - input_frames),
            });
        }

        let mut done = 0;
        while done < frames {
            self.reach_boundary(sink);

            let next = self.grid.next_beat_boundary(self.position);
            let run = as_usize(next.saturating_sub(self.position).0).min(frames - done);
            self.render(input, output, done, run, input_frames, sink);

            self.position += Frames(as_u64(run));
            done += run;
        }
    }

    /// Fires anything scheduled for the exact frame the transport has reached.
    fn reach_boundary(&mut self, sink: &mut impl EventSink) {
        let (bar, beat) = self.grid.beat_of(self.position);
        let on_beat = self.grid.bar_start(bar) + self.grid.beat_offset(beat) == self.position;
        if !on_beat {
            return;
        }

        if beat == 0 {
            sink.event(Event::Bar { bar });
        }
        sink.event(Event::Beat { bar, beat });
        self.click.trigger(beat == 0);

        let before = self.session;
        let ctx = self.ctx();
        self.with_session(|session, audio| {
            session.advance(&ctx, &mut |a, e| audio.apply(a, e, sink));
        });
        self.emit_changes(&before, sink);
    }

    fn render(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        offset: usize,
        run: usize,
        input_frames: usize,
        sink: &mut impl EventSink,
    ) {
        let out = &mut output[offset * self.channels..(offset + run) * self.channels];

        for addr in SlotAddr::all() {
            // A queued stop keeps sounding until its boundary arrives.
            let sounding = matches!(
                self.session.state(addr),
                SlotState::Playing { .. } | SlotState::QueuedStop { .. }
            );
            if !sounding {
                continue;
            }
            if let Some(clip) = self.audio.clip(addr) {
                clip.mix_into(self.position, out);
            }
        }

        // Frames the device did not deliver stay unwritten, which reads back as silence,
        // while the write position still advances so the loop keeps its length.
        let available = input_frames.saturating_sub(offset).min(run);
        let captured = &input[offset * self.channels..(offset + available) * self.channels];

        for addr in SlotAddr::all() {
            let segments = &mut self.audio.segments;
            let Some(recording) =
                self.audio.recordings[addr.track.index()][addr.slot.index()].as_mut()
            else {
                continue;
            };
            if available > 0 {
                let written = recording.buffer.write(recording.frames, captured, segments);
                if written < available && !recording.starved {
                    recording.starved = true;
                    sink.event(Event::RecordBufferLow { addr });
                }
            }
            recording.frames += as_u64(run);
        }

        self.click.add_into(out, self.channels);
    }

    fn emit_changes(&self, before: &SessionModel, sink: &mut impl EventSink) {
        for addr in SlotAddr::all() {
            let state = self.session.state(addr);
            if before.state(addr) != state {
                sink.event(Event::SlotChanged { addr, state });
            }
        }
    }
}
