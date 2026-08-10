//! The realtime engine.
//!
//! [`Engine::process`] is the callback body. It never allocates: all audio memory comes
//! from pools built in [`Engine::new`], and retired clips return their segments to those
//! pools rather than being dropped.
//!
//! Blocks are split at beat boundaries. When a boundary falls inside the block the run
//! is cut there, the transition is applied, and rendering resumes. A launch lands on the
//! exact frame it was scheduled for rather than at the next block edge.

use free_loop_core::{
    BarGrid, ClipId, Command, Ctx, Effect, Event, Frames, MIN_BPM, PadMask, SLOT_COUNT, SampleRate,
    SessionModel, SlotAddr, SlotState, TRACK_COUNT, Tempo, TimeError, TimeSignature, pad_bit,
};

use std::sync::Arc;

use crate::buffer::{Clip, SEGMENT_FRAMES, SegmentPool};
use crate::click::{Click, ClickConfig};
use crate::load::{LoadInbox, LoadMessage, Loader};
use crate::recycle::{Recycler, Retirement, channel};
use crate::snapshot::{Snapshot, SnapshotReader, SnapshotWriter};

/// Moves an anchor back by `shift`, wrapped into the loop rather than clamped.
///
/// Only `recorded_at` modulo the loop length affects playback, so wrapping keeps the
/// phase exact. Subtracting directly would clamp at zero for anything recorded before
/// the reference, which is the case a uniform shift exists to protect.
fn shifted_anchor(recorded_at: Frames, len: Frames, shift: Frames) -> Frames {
    if len.0 == 0 {
        return Frames::ZERO;
    }
    Frames((recorded_at.0 % len.0 + len.0 - shift.0 % len.0) % len.0)
}

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
    /// Round-trip latency to compensate for, in frames. See
    /// [`Engine::set_capture_offset`].
    pub capture_offset: Frames,
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
            capture_offset: Frames::ZERO,
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

/// Capture in progress. The audio goes straight into the clip the pad will hold.
#[derive(Debug)]
struct Recording {
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
    clips: [[Option<Arc<Clip>>; SLOT_COUNT]; TRACK_COUNT],
    recordings: [[Option<Recording>; SLOT_COUNT]; TRACK_COUNT],
    /// Preallocated clips waiting to be recorded into.
    shells: Vec<Arc<Clip>>,
    segments: SegmentPool,
    retirement: Retirement,
    snapshots: SnapshotWriter,
    next_clip_id: ClipId,
    /// Copy of [`Engine::capture_offset`], since effects are applied from here.
    capture_offset: Frames,
}

impl Audio {
    fn clip(&self, addr: SlotAddr) -> Option<&Arc<Clip>> {
        self.clips[addr.track.index()][addr.slot.index()].as_ref()
    }

    fn take_clip(&mut self, addr: SlotAddr) -> Option<Arc<Clip>> {
        self.clips[addr.track.index()][addr.slot.index()].take()
    }

    fn stop_recording(&mut self, addr: SlotAddr) {
        self.recordings[addr.track.index()][addr.slot.index()] = None;
    }

    fn put_clip(&mut self, addr: SlotAddr, clip: Arc<Clip>) {
        self.clips[addr.track.index()][addr.slot.index()] = Some(clip);
    }

    /// Takes a clip back into the pool, or hands it over.
    ///
    /// Borrowed storage always goes back to whoever supplied it, so repeated loads cannot
    /// grow the engine's pools.
    fn retire(&mut self, mut clip: Arc<Clip>) {
        let mine = !clip.is_borrowed() && self.shells.len() < TRACK_COUNT * SLOT_COUNT;
        match Arc::get_mut(&mut clip).filter(|_| mine) {
            Some(inner) => {
                inner.release_segments(&mut self.segments);
                inner.reset();
                self.shells.push(clip);
            }
            None => self.retirement.retire(clip),
        }
    }

    /// Takes back anything the recycler has released.
    fn reclaim(&mut self) {
        let Self {
            retirement,
            segments,
            shells,
            ..
        } = self;

        // The recycler sends borrowed storage to its own queue, so everything arriving
        // here is the engine's. Filtering for that would drop what it rejected, and
        // dropping the last reference is an allocator call in the callback.
        retirement.reclaim(|mut clip| {
            if let Some(inner) = Arc::get_mut(&mut clip) {
                inner.release_segments(segments);
                inner.reset();
                shells.push(clip);
            }
        });
    }

    fn apply(&mut self, addr: SlotAddr, effect: Effect, sink: &mut impl EventSink) {
        match effect {
            Effect::StartCapture { .. } => {
                // The shell pool holds one clip per pad, so this only comes up empty when
                // the recycler is behind.
                if let Some(shell) = self.shells.pop() {
                    self.clips[addr.track.index()][addr.slot.index()] = Some(shell);
                    self.recordings[addr.track.index()][addr.slot.index()] = Some(Recording {
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
                if self.recordings[addr.track.index()][addr.slot.index()]
                    .take()
                    .is_none()
                {
                    return;
                }
                let Some(held) = self.clips[addr.track.index()][addr.slot.index()].as_mut() else {
                    return;
                };
                let Some(inner) = Arc::get_mut(held) else {
                    return;
                };

                let len = at.saturating_sub(started_at);
                // Stamping the clip as having started `capture_offset` earlier undoes the
                // round trip: frame k holds what was played at `started_at + k - offset`,
                // so that is the grid position it belongs to.
                inner.set_len(len);
                inner.set_recorded_at(started_at.saturating_sub(self.capture_offset));
                inner.set_capture_offset(self.capture_offset);

                self.next_clip_id = clip.next();
                sink.event(Event::ClipRecorded { addr, clip, len });
            }

            Effect::CancelCapture => {
                self.recordings[addr.track.index()][addr.slot.index()] = None;
                if let Some(held) = self.clips[addr.track.index()][addr.slot.index()].take() {
                    self.retire(held);
                }
            }

            Effect::ReleaseClip { clip } => {
                if let Some(held) = self.clips[addr.track.index()][addr.slot.index()].take() {
                    self.retire(held);
                }
                sink.event(Event::ClipReleased { clip });
            }

            // Playback is a function of slot state and transport position, so there is
            // no voice to start or stop.
            Effect::StartPlayback { .. } | Effect::StopPlayback { .. } => {}
        }
    }
}

/// The half of the engine that runs off the audio thread.
#[derive(Debug)]
pub struct Housekeeping {
    /// Returns clips that were being read when the engine finished with them.
    pub recycler: Recycler,
    /// Receives clips the engine publishes on [`Command::Snapshot`].
    pub snapshots: SnapshotReader,
    /// Puts a saved session back into the engine.
    pub loader: Loader,
}

/// The realtime engine.
#[derive(Debug)]
pub struct Engine {
    grid: BarGrid,
    position: Frames,
    session: SessionModel,
    audio: Audio,
    click: Click,
    loads: LoadInbox,
    channels: usize,
    max_bars: u32,
    paused: bool,
    /// Pads that do not sound.
    muted: PadMask,
    /// Pads that sound to the exclusion of the rest.
    soloed: PadMask,
    /// The last position a beat fired on, so a grid change cannot fire it twice.
    last_boundary: Option<Frames>,
    /// MIDI clock ticks already reported.
    last_clock: u64,
    capture_offset: Frames,
    sample_rate: SampleRate,
    time_signature: TimeSignature,
}

impl Engine {
    /// Builds an engine and allocates its pools.
    ///
    /// [`Housekeeping`] is the off-thread half. Run it anywhere except the audio thread.
    ///
    /// # Errors
    ///
    /// [`EngineError`] if the configuration is not usable.
    pub fn new(config: EngineConfig) -> Result<(Self, Housekeeping), EngineError> {
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

        let (retirement, recycler) = channel();
        let (snapshots, snapshot_reader) = crate::snapshot::channel();
        let (loader, loads) = crate::load::channel();
        let shells = (0..TRACK_COUNT * SLOT_COUNT)
            .map(|_| Arc::new(Clip::empty(max_segments, config.channels)))
            .collect();

        let engine = Self {
            grid,
            position: Frames::ZERO,
            session: SessionModel::new(),
            audio: Audio {
                clips: core::array::from_fn(|_| core::array::from_fn(|_| None)),
                recordings: core::array::from_fn(|_| core::array::from_fn(|_| None)),
                shells,
                segments: SegmentPool::new(config.segment_pool, config.channels),
                retirement,
                snapshots,
                next_clip_id: ClipId(0),
                capture_offset: config.capture_offset,
            },
            click: Click::new(config.click, config.sample_rate),
            loads,
            channels: config.channels,
            max_bars: config.max_bars,
            paused: false,
            muted: 0,
            soloed: 0,
            last_boundary: None,
            last_clock: 0,
            capture_offset: config.capture_offset,
            sample_rate: config.sample_rate,
            time_signature: config.time_signature,
        };
        Ok((
            engine,
            Housekeeping {
                recycler,
                snapshots: snapshot_reader,
                loader,
            },
        ))
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

    /// Whether the transport is frozen.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Whether a pad would be heard if it were playing.
    ///
    /// A solo anywhere silences everything outside it, which is what makes solo a way of
    /// hearing one thing rather than a second kind of mute.
    pub fn is_audible(&self, addr: SlotAddr) -> bool {
        let bit = pad_bit(addr);
        if self.muted & bit != 0 {
            return false;
        }
        self.soloed == 0 || self.soloed & bit != 0
    }

    /// The round-trip latency being compensated for.
    pub fn capture_offset(&self) -> Frames {
        self.capture_offset
    }

    /// Sets the round-trip latency to compensate for.
    ///
    /// Captured audio arrives this many frames after it was played, so a clip's frames
    /// describe a moment that has already passed. Rather than shifting the audio, the
    /// clip is stamped as having started that much earlier, which puts every frame back
    /// on the grid position it was played at.
    ///
    /// Takes effect on the next recording sealed; a take already in progress keeps the
    /// value it started with.
    pub fn set_capture_offset(&mut self, offset: Frames) {
        self.capture_offset = offset;
        self.audio.capture_offset = offset;
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
            Command::SetMutes { muted, soloed } => {
                self.muted = muted;
                self.soloed = soloed;
            }
            Command::Rewind => self.rewind(sink),
            Command::Snapshot => self.publish_snapshot(sink),
            Command::SetPaused(paused) => self.set_paused(paused, sink),
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

    /// Sends the transport back to the start, with the longest loop at its beginning.
    ///
    /// Every anchor moves by the same amount rather than to zero. Zeroing them would put
    /// each loop at its own beginning, which shifts loops against each other: a four bar
    /// take recorded while a two bar one was halfway through is meant to stay halfway
    /// through. A uniform shift keeps every one of those relationships and only decides
    /// where the ensemble starts.
    ///
    /// The longest loop is the reference, since it is the one that sets the phrase.
    fn rewind(&mut self, sink: &mut impl EventSink) {
        let shift = self.reference_anchor();

        // A take spanning a rewind would splice two moments together, and its start would
        // sit ahead of the transport.
        let ctx = self.ctx();
        self.with_session(|session, audio| {
            session.cancel_recordings(&ctx, &mut |a, e| audio.apply(a, e, sink));
        });

        self.position = Frames::ZERO;
        self.last_boundary = None;
        self.resync_clock();

        // Anything waiting on a bar line was scheduled against the old position, which is
        // now far ahead. Retargeting fires it at the start instead of stranding it.
        self.session.retarget_pending(Frames::ZERO);

        for addr in SlotAddr::all() {
            let Some(clip) = self.audio.clips[addr.track.index()][addr.slot.index()].as_mut()
            else {
                continue;
            };
            let moved = shifted_anchor(clip.recorded_at(), clip.len(), shift);
            // A clip somebody is reading keeps its old anchor. Rare, and the next rewind
            // catches it.
            if let Some(inner) = Arc::get_mut(clip) {
                inner.set_recorded_at(moved);
            }
        }
    }

    /// The anchor everything is measured against when rewinding.
    ///
    /// The longest loop, and the earliest of those if several share a length, so the
    /// choice does not wander between rewinds.
    fn reference_anchor(&self) -> Frames {
        SlotAddr::all()
            .filter_map(|addr| self.audio.clip(addr))
            .min_by_key(|clip| (core::cmp::Reverse(clip.len()), clip.recorded_at()))
            .map_or(Frames::ZERO, |clip| clip.recorded_at())
    }

    /// Installs anything the loader has queued.
    fn apply_loads(&mut self, sink: &mut impl EventSink) {
        let before = self.session;
        let mut pause = false;
        let mut tempo = None;

        let Self {
            session,
            audio,
            loads,
            ..
        } = self;

        loads.drain(|message| match message {
            LoadMessage::Begin { tempo: wanted } => {
                tempo = Some(wanted);
                for addr in SlotAddr::all() {
                    // A take left running would write live input into whatever the load
                    // puts on that pad.
                    audio.stop_recording(addr);
                    if let Some(held) = audio.take_clip(addr) {
                        audio.retire(held);
                    }
                    session.mirror(addr, SlotState::Empty);
                }
            }
            LoadMessage::Clip {
                addr,
                mut clip,
                playing,
            } => {
                // The loader keeps the storage, so mark it before the engine sees it as
                // one of its own.
                if let Some(inner) = Arc::get_mut(&mut clip) {
                    inner.set_borrowed(true);
                }
                let id = audio.next_clip_id;
                audio.next_clip_id = id.next();
                audio.put_clip(addr, clip);

                let state = if playing {
                    SlotState::Playing { clip: id }
                } else {
                    SlotState::Stopped { clip: id }
                };
                session.mirror(addr, state);
            }
            LoadMessage::End => pause = true,
        });

        if let Some(tempo) = tempo {
            self.set_tempo_unchecked(tempo);
        }
        if pause {
            self.paused = true;
            // Starting a loaded session part way through its loops is never what was
            // wanted, so it begins at the beginning.
            self.rewind(sink);
        }
        self.emit_changes(&before, sink);
    }

    /// Sets the tempo without the guard that protects existing clips.
    ///
    /// A load replaces the grid wholesale, so there is nothing left to fall out of sync.
    fn set_tempo_unchecked(&mut self, tempo: Tempo) {
        if let Ok(grid) = BarGrid::new(self.sample_rate, tempo, self.time_signature) {
            self.position = self.grid.rebase_onto(self.position, grid);
            self.grid = grid;
            self.resync_clock();
        }
    }

    /// Publishes a reference to every pad that holds a clip.
    fn publish_snapshot(&mut self, sink: &mut impl EventSink) {
        let mut published = 0;
        for addr in SlotAddr::all() {
            let state = self.session.state(addr);
            // A pad still recording has no finished audio to publish.
            if state.is_recording() {
                continue;
            }
            let Some(clip) = self.audio.clip(addr) else {
                continue;
            };
            let snapshot = Snapshot {
                addr,
                state,
                clip: Arc::clone(clip),
            };
            self.audio.snapshots.publish(snapshot);
            published += 1;
        }
        sink.event(Event::SnapshotComplete { clips: published });
    }

    fn set_paused(&mut self, paused: bool, sink: &mut impl EventSink) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;

        if paused {
            let ctx = self.ctx();
            self.with_session(|session, audio| {
                session.cancel_recordings(&ctx, &mut |a, e| audio.apply(a, e, sink));
            });
        }
    }

    fn set_tempo(&mut self, tempo: Tempo, sink: &mut impl EventSink) {
        if self.session.has_any_clip() {
            sink.event(Event::TempoRejected);
            return;
        }
        let Ok(grid) = BarGrid::new(self.sample_rate, tempo, self.time_signature) else {
            sink.event(Event::TempoRejected);
            return;
        };

        // Move the transport with the grid, or the same frame count would land on a
        // different beat and the click would jump instead of changing interval.
        self.position = self.grid.rebase_onto(self.position, grid);
        self.grid = grid;
        self.resync_clock();
    }

    /// Renders one block.
    ///
    /// `output` is interleaved and fully overwritten. `input` is interleaved with the
    /// same channel count; if it is short, the missing frames are treated as silence and
    /// an [`Event::Xrun`] is reported.
    pub fn process(&mut self, input: &[f32], output: &mut [f32], sink: &mut impl EventSink) {
        output.fill(0.0);
        self.audio.reclaim();
        self.apply_loads(sink);

        // A frozen transport holds its position, so nothing sounds, nothing is captured
        // and no bar line arrives. Input is dropped rather than buffered: it belongs to a
        // moment the transport is not at.
        if self.paused {
            return;
        }

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

        self.report_clock(sink);
    }

    /// Reports the MIDI clock ticks crossed by this block.
    ///
    /// A block is shorter than a tick at any usable tempo, so these arrive spaced rather
    /// than in bursts, which is what a device deriving tempo from them needs.
    fn report_clock(&mut self, sink: &mut impl EventSink) {
        let now = self.grid.clock_ticks_at(self.position);
        let ticks = now.saturating_sub(self.last_clock);
        if ticks == 0 {
            return;
        }
        self.last_clock = now;
        sink.event(Event::Clock {
            ticks: u32::try_from(ticks).unwrap_or(u32::MAX),
        });
    }

    /// Takes the clock count from wherever the transport now is.
    ///
    /// Called after anything that moves the transport other than playing, so the next
    /// report is a step rather than the whole jump.
    fn resync_clock(&mut self) {
        self.last_clock = self.grid.clock_ticks_at(self.position);
    }

    /// Fires anything scheduled for the exact frame the transport has reached.
    fn reach_boundary(&mut self, sink: &mut impl EventSink) {
        let (bar, beat) = self.grid.beat_of(self.position);
        let on_beat = self.grid.bar_start(bar) + self.grid.beat_offset(beat) == self.position;
        if !on_beat || self.last_boundary == Some(self.position) {
            return;
        }
        self.last_boundary = Some(self.position);

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
            if !sounding || !self.is_audible(addr) {
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
            let Audio {
                clips,
                recordings,
                segments,
                ..
            } = &mut self.audio;

            let Some(recording) = recordings[addr.track.index()][addr.slot.index()].as_mut() else {
                continue;
            };
            let written = clips[addr.track.index()][addr.slot.index()]
                .as_mut()
                .and_then(Arc::get_mut)
                .filter(|_| available > 0)
                .map_or(0, |clip| clip.write(recording.frames, captured, segments));

            if available > 0 && written < available && !recording.starved {
                recording.starved = true;
                sink.event(Event::RecordBufferLow { addr });
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;

    #[test]
    fn an_anchor_before_the_reference_wraps_rather_than_clamping() {
        let len = Frames(1_000);

        // Recorded 300 frames before the reference. Clamping would put it at zero and
        // lose the relationship.
        let moved = shifted_anchor(Frames(700), len, Frames(1_000));
        assert_eq!(moved, Frames(700));

        let reference = shifted_anchor(Frames(1_000), len, Frames(1_000));
        assert_eq!(reference, Frames::ZERO);
    }

    #[test]
    fn a_uniform_shift_keeps_every_gap() {
        let len = Frames(1_000);
        let shift = Frames(2_345);

        let gap = |a: u64, b: u64| {
            let one = shifted_anchor(Frames(a), len, shift).0;
            let two = shifted_anchor(Frames(b), len, shift).0;
            (one + len.0 - two) % len.0
        };

        assert_eq!(gap(700, 400), 300);
        assert_eq!(gap(400, 700), 700);
    }

    #[test]
    fn a_shifted_anchor_stays_inside_the_loop() {
        for recorded_at in [0, 1, 999, 1_000, 123_456] {
            let moved = shifted_anchor(Frames(recorded_at), Frames(1_000), Frames(7_777));
            assert!(moved.0 < 1_000);
        }
    }
}
