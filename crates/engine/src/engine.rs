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
    BarGrid, ClipId, Command, Ctx, Effect, Event, Frames, LaunchMode, MIN_BPM, PadMask, SLOT_COUNT,
    SampleRate, SessionModel, SlotAddr, SlotState, TRACK_COUNT, Tempo, TimeError, TimeSignature,
    TrackInput, UNITY_STEP, gain_for_step, pad_bit,
};

use std::sync::Arc;

use crate::buffer::{Clip, Ramp, SEGMENT_FRAMES, SegmentPool};
use crate::click::{Click, ClickConfig};
use crate::load::{LoadInbox, LoadMessage, Loader};
use crate::recycle::{Recycler, Retirement, channel};
use crate::snapshot::{Snapshot, SnapshotReader, SnapshotWriter};

/// Frames a level travels the full gain range in by default. 5 ms at 48 kHz.
pub const DEFAULT_DECLICK: Frames = Frames(240);

/// A transport move that waits for the mix to fade out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Deferred {
    /// Back to the start.
    Rewind,
    /// Freeze.
    Pause,
    /// Empty every pad and start over.
    ClearAll,
}

/// Holds the mix at full scale and reports how much was held.
fn limit(output: &mut [f32], sink: &mut impl EventSink) {
    let mut held = 0_u32;
    for sample in output.iter_mut() {
        if *sample > 1.0 {
            *sample = 1.0;
            held += 1;
        } else if *sample < -1.0 {
            *sample = -1.0;
            held += 1;
        }
    }

    if held > 0 {
        sink.event(Event::Clipped { samples: held });
    }
}

/// Moves an anchor back by `shift`, wrapped into the loop rather than clamped.
///
/// Only `recorded_at` modulo the loop length affects playback, so wrapping keeps the
/// phase exact.
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
    /// Input every track starts on.
    pub input: TrackInput,
    /// Where every track's clips start out being anchored.
    pub launch_mode: LaunchMode,
    /// Frames a level takes to travel the full gain range. Zero switches instead.
    pub declick: Frames,
}

impl EngineConfig {
    /// A stereo 48 kHz setup at 120 bpm, with roughly 90 seconds of recording space.
    ///
    /// # Errors
    ///
    /// [`EngineError::Time`], which these values never trigger.
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
            declick: DEFAULT_DECLICK,
            input: TrackInput::default(),
            launch_mode: LaunchMode::default(),
        })
    }
}

/// A destination for engine reports.
///
/// Implemented for any `FnMut(Event)` and for `Vec<Event>`.
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
    /// The input this take is capturing, fixed when it began.
    input: TrackInput,
    /// Whether the segment pool ran dry part way through.
    starved: bool,
}

/// One pad's worth of a load that has not been put in place yet.
#[derive(Debug)]
struct StagedClip {
    clip: Arc<Clip>,
    playing: bool,
    launch_anchor: Option<Frames>,
}

/// A load being assembled.
#[derive(Debug)]
struct Staging {
    /// Whether a `Begin` has arrived and its `End` has not.
    receiving: bool,
    tempo: Option<Tempo>,
    clips: [[Option<StagedClip>; SLOT_COUNT]; TRACK_COUNT],
}

impl Staging {
    fn new() -> Self {
        Self {
            receiving: false,
            tempo: None,
            clips: core::array::from_fn(|_| core::array::from_fn(|_| None)),
        }
    }

    /// Gives up a load that never finished arriving.
    ///
    /// The storage belongs to whoever sent it, so it goes back through the recycler rather
    /// than being dropped here.
    fn abandon(&mut self, retirement: &mut Retirement) {
        self.receiving = false;
        self.tempo = None;
        for row in &mut self.clips {
            for slot in row {
                if let Some(staged) = slot.take() {
                    retirement.retire(staged.clip);
                }
            }
        }
    }
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
    /// The round trip a sealed take is stamped as having started before.
    capture_offset: Frames,
    /// Which input each track records.
    inputs: [TrackInput; TRACK_COUNT],
    /// Where each track's clips are anchored when launched.
    launch_modes: [LaunchMode; TRACK_COUNT],
    /// Where a launch put each pad's clip, for the pads whose track restarts them.
    anchors: [[Option<Frames>; SLOT_COUNT]; TRACK_COUNT],
    /// Pads whose capture could not be given storage, for the engine to put back. Several
    /// can be refused on one boundary.
    refused: PadMask,
    /// A load being assembled, put in place only once all of it has arrived.
    staged: Staging,
}

impl Audio {
    fn clip(&self, addr: SlotAddr) -> Option<&Arc<Clip>> {
        self.clips[addr.track.index()][addr.slot.index()].as_ref()
    }

    /// Where a pad's clip is anchored: where it was launched, or where it was recorded.
    fn anchor(&self, addr: SlotAddr, clip: &Clip) -> Frames {
        self.anchors[addr.track.index()][addr.slot.index()].unwrap_or_else(|| clip.recorded_at())
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
                // the recycler is behind. The slot is put back rather than left claiming
                // to hold a take it has nowhere to write.
                let Some(shell) = self.shells.pop() else {
                    self.refused |= pad_bit(addr);
                    return;
                };
                self.clips[addr.track.index()][addr.slot.index()] = Some(shell);
                self.recordings[addr.track.index()][addr.slot.index()] = Some(Recording {
                    frames: 0,
                    starved: false,
                    input: self.inputs[addr.track.index()],
                });
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

            Effect::StartPlayback { at, .. } => {
                let (track, slot) = (addr.track.index(), addr.slot.index());
                if !self.launch_modes[track].restarts() {
                    self.anchors[track][slot] = None;
                    return;
                }

                // Frame zero of the buffer holds audio from `capture_offset` before the
                // take's first beat, so anchoring on `at` would play that as pre-roll.
                let offset = self.clips[track][slot]
                    .as_ref()
                    .map_or(Frames::ZERO, |clip| clip.capture_offset());
                self.anchors[track][slot] = Some(Frames(at.0.saturating_sub(offset.0)));
            }

            // Playback is otherwise a function of slot state and transport position, so
            // there is no voice to stop.
            Effect::StopPlayback { .. } => {}
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
    /// How loud each track plays, as a step on the gain ladder.
    gains: [u8; TRACK_COUNT],
    /// The gain each pad is mixing at, which slides toward what it should be.
    levels: [[f32; SLOT_COUNT]; TRACK_COUNT],
    /// Frames a level takes to travel the full gain range.
    declick: usize,
    /// A transport move waiting for the mix to fade out.
    pending: Option<Deferred>,
    /// The last position a beat fired on, so a grid change cannot fire it twice.
    last_boundary: Option<Frames>,
    /// MIDI clock ticks already reported.
    last_clock: u64,
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
                inputs: [config.input; TRACK_COUNT],
                launch_modes: [config.launch_mode; TRACK_COUNT],
                anchors: core::array::from_fn(|_| core::array::from_fn(|_| None)),
                refused: 0,
                staged: Staging::new(),
            },
            click: Click::new(config.click, config.sample_rate),
            loads,
            channels: config.channels,
            max_bars: config.max_bars,
            paused: false,
            muted: 0,
            soloed: 0,
            gains: [UNITY_STEP; TRACK_COUNT],
            levels: [[0.0; SLOT_COUNT]; TRACK_COUNT],
            declick: as_usize(config.declick.0),
            pending: None,
            last_boundary: None,
            last_clock: 0,
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

    /// How loud a track plays.
    pub fn gain(&self, track: free_loop_core::TrackId) -> f32 {
        gain_for_step(self.gains[track.index()])
    }

    /// Whether a pad would be heard if it were playing.
    ///
    /// A solo anywhere silences everything outside it.
    pub fn is_audible(&self, addr: SlotAddr) -> bool {
        let bit = pad_bit(addr);
        if self.muted & bit != 0 {
            return false;
        }
        self.soloed == 0 || self.soloed & bit != 0
    }

    /// The gain a pad should be heading for. Zero for anything not being heard.
    fn target_level(&self, addr: SlotAddr) -> f32 {
        // A transport move fades everything out first.
        if self.pending.is_some() {
            return 0.0;
        }
        if !self.session.state(addr).is_sounding() || !self.is_audible(addr) {
            return 0.0;
        }
        self.gain(addr.track)
    }

    /// How a level moves toward `target` over `run` frames, and where it ends up.
    #[expect(
        clippy::cast_precision_loss,
        reason = "block lengths are far below f32's exact range"
    )]
    fn ramp_to(&self, level: f32, target: f32, run: usize) -> (Ramp, f32) {
        // With no ramp the level changes at the block boundary, so the whole block is at
        // the new one.
        if self.declick == 0 {
            return (Ramp::constant(target), target);
        }

        let most = run as f32 / self.declick as f32;
        let reached = if target > level {
            (level + most).min(target)
        } else {
            (level - most).max(target)
        };
        (Ramp::new(level, reached, run), reached)
    }

    /// Whether the mix has reached silence.
    fn is_faded(&self) -> bool {
        self.levels.iter().flatten().all(|level| *level == 0.0)
    }

    /// The round-trip latency being compensated for.
    pub fn capture_offset(&self) -> Frames {
        self.audio.capture_offset
    }

    /// Sets the round-trip latency to compensate for.
    ///
    /// Captured audio arrives this many frames after it was played. The clip is stamped
    /// as having started that much earlier.
    ///
    /// Takes effect on the next recording sealed.
    pub fn set_capture_offset(&mut self, offset: Frames) {
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
    /// at the start of the block they are drained in.
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
            Command::SetGains(gains) => self.gains = gains,
            // Takes in progress keep the input they started on.
            Command::SetInputs(inputs) => self.audio.inputs = inputs,
            // A clip already sounding keeps the anchor it was launched with.
            Command::SetLaunchModes(modes) => self.audio.launch_modes = modes,
            Command::ClearAll => self.defer(Deferred::ClearAll, sink),
            Command::Rewind => self.defer(Deferred::Rewind, sink),
            Command::Resync => {
                for addr in SlotAddr::all() {
                    sink.event(Event::SlotChanged {
                        addr,
                        state: self.session.state(addr),
                    });
                }
            }
            Command::Snapshot { request } => self.publish_snapshot(request, sink),
            Command::SetPaused(paused) => self.set_paused(paused, sink),
            Command::SetClickEnabled(enabled) => self.click.set_enabled(enabled),
            Command::SetClickLevel(level) => self.click.set_level(level),
            Command::SetTempo(tempo) => self.set_tempo(tempo, sink),
        }

        self.settle_refusals(sink);
        self.emit_changes(&before, sink);
    }

    /// Empties any pad that was armed but could not be given storage.
    fn settle_refusals(&mut self, sink: &mut impl EventSink) {
        let refused = core::mem::take(&mut self.audio.refused);
        if refused == 0 {
            return;
        }
        for addr in SlotAddr::all().filter(|addr| refused & pad_bit(*addr) != 0) {
            let ctx = self.ctx();
            self.with_session(|session, audio| {
                session.clear(addr, &ctx, &mut |a, e| audio.apply(a, e, sink));
            });
            sink.event(Event::RecordingRefused { addr });
        }
    }

    /// Queues a transport move behind a fade. With no ramp it lands at once.
    fn defer(&mut self, action: Deferred, sink: &mut impl EventSink) {
        if self.declick == 0 {
            self.apply_deferred(action, sink);
        } else {
            self.pending = Some(action);
        }
    }

    /// Performs a move whose fade has finished.
    fn apply_deferred(&mut self, action: Deferred, sink: &mut impl EventSink) {
        match action {
            Deferred::Rewind => self.rewind(sink),
            Deferred::ClearAll => self.clear_all(sink),
            Deferred::Pause => {
                self.paused = true;
                let ctx = self.ctx();
                self.with_session(|session, audio| {
                    session.cancel_recordings(&ctx, &mut |a, e| audio.apply(a, e, sink));
                });
            }
        }

        // Zero already after a fade. Forcing it covers having had nothing to fade.
        self.levels = [[0.0; SLOT_COUNT]; TRACK_COUNT];
    }

    fn with_session(&mut self, apply: impl FnOnce(&mut SessionModel, &mut Audio)) {
        let Self { session, audio, .. } = self;
        apply(session, audio);
    }

    /// Empties every pad and starts the transport over.
    fn clear_all(&mut self, sink: &mut impl EventSink) {
        let ctx = self.ctx();
        self.with_session(|session, audio| {
            session.clear_all(&ctx, &mut |a, e| audio.apply(a, e, sink));
        });
        self.rewind(sink);
    }

    /// Sends the transport back to the start, with the longest loop at its beginning.
    ///
    /// Every anchor moves by the same amount, which keeps the loops where they were
    /// against each other. The longest loop is the reference.
    fn rewind(&mut self, sink: &mut impl EventSink) {
        let shift = self.reference_anchor();

        // A take spanning a rewind splices two moments together.
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
            // A pad that is not sounding takes a fresh anchor when it is next launched, so
            // there is nothing to keep.
            let (track, slot) = (addr.track.index(), addr.slot.index());
            self.audio.anchors[track][slot] = self.audio.anchors[track][slot]
                .filter(|_| self.session.state(addr).is_sounding())
                .map(|anchor| {
                    let len = self.audio.clips[track][slot]
                        .as_ref()
                        .map_or(Frames::ZERO, |clip| clip.len());
                    shifted_anchor(anchor, len, shift)
                });

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
    /// The longest loop, earliest first, so the choice is stable across rewinds.
    fn reference_anchor(&self) -> Frames {
        // Sounding pads first: a pad that is silent may hold an anchor from a launch that
        // is over, or one it has not taken yet, and neither describes what is playing.
        let sounding = self.longest_anchor(|addr| self.session.state(addr).is_sounding());
        sounding.unwrap_or_else(|| self.longest_anchor(|_| true).unwrap_or(Frames::ZERO))
    }

    /// The anchor of the longest clip on a pad `wanted` accepts, earliest first.
    fn longest_anchor(&self, wanted: impl Fn(SlotAddr) -> bool) -> Option<Frames> {
        SlotAddr::all()
            .filter(|addr| wanted(*addr))
            .filter_map(|addr| self.audio.clip(addr).map(|clip| (addr, clip)))
            .min_by_key(|(addr, clip)| {
                (
                    core::cmp::Reverse(clip.len()),
                    self.audio.anchor(*addr, clip),
                )
            })
            .map(|(addr, clip)| self.audio.anchor(addr, clip))
    }

    /// Installs anything the loader has queued.
    fn apply_loads(&mut self, sink: &mut impl EventSink) {
        let before = self.session;
        let Self { audio, loads, .. } = self;

        // Staged rather than applied as it arrives: the callback would otherwise clear the
        // grid and render whatever subset of a load had turned up so far.
        let mut complete = false;
        while let Some(message) = loads.pop() {
            match message {
                LoadMessage::Begin { tempo } => {
                    // Anything staged belongs to a load that never finished arriving.
                    audio.staged.abandon(&mut audio.retirement);
                    audio.staged.receiving = true;
                    audio.staged.tempo = Some(tempo);
                }
                LoadMessage::Clip {
                    addr,
                    mut clip,
                    playing,
                    launch_anchor,
                } => {
                    // The loader keeps the storage, so mark it before the engine sees it as
                    // one of its own.
                    if let Some(inner) = Arc::get_mut(&mut clip) {
                        inner.set_borrowed(true);
                    }
                    if audio.staged.receiving {
                        audio.staged.clips[addr.track.index()][addr.slot.index()] =
                            Some(StagedClip {
                                clip,
                                playing,
                                launch_anchor,
                            });
                    } else {
                        // No transaction to belong to, so it goes straight back.
                        audio.retirement.retire(clip);
                    }
                }
                // An `End` with nothing open would otherwise commit an empty staging area
                // and clear whatever is loaded. Anything queued behind it is the next
                // load, left for the next block so this one goes in first.
                LoadMessage::End => {
                    complete = audio.staged.receiving;
                    audio.staged.receiving = false;
                    break;
                }
            }
        }

        if !complete {
            return;
        }
        self.commit_load(sink);
        self.emit_changes(&before, sink);
    }

    /// Puts a fully arrived load in place of whatever is loaded now.
    fn commit_load(&mut self, sink: &mut impl EventSink) {
        let Self { session, audio, .. } = self;

        for addr in SlotAddr::all() {
            // A take left running would write live input into whatever the load puts on
            // that pad.
            audio.stop_recording(addr);
            if let Some(held) = audio.take_clip(addr) {
                audio.retire(held);
            }
            audio.anchors[addr.track.index()][addr.slot.index()] = None;
            session.mirror(addr, SlotState::Empty);

            let Some(staged) = audio.staged.clips[addr.track.index()][addr.slot.index()].take()
            else {
                continue;
            };
            let id = audio.next_clip_id;
            audio.next_clip_id = id.next();
            audio.put_clip(addr, staged.clip);
            audio.anchors[addr.track.index()][addr.slot.index()] = staged.launch_anchor;
            session.mirror(
                addr,
                if staged.playing {
                    SlotState::Playing { clip: id }
                } else {
                    SlotState::Stopped { clip: id }
                },
            );
        }

        if let Some(tempo) = self.audio.staged.tempo.take() {
            self.set_tempo_unchecked(tempo);
        }
        // A rewind or clear queued against the session just replaced would run against the
        // new one, and the commit has already zeroed the levels it was waiting on.
        self.pending = None;
        // A load arrives against a grid the performer has not heard yet, so it waits.
        self.paused = true;
        self.levels = [[0.0; SLOT_COUNT]; TRACK_COUNT];
        self.rewind(sink);
    }

    /// Sets the tempo without the guard that protects existing clips.
    ///
    /// Safe during a load, which replaces the grid wholesale.
    fn set_tempo_unchecked(&mut self, tempo: Tempo) {
        if let Ok(grid) = BarGrid::new(self.sample_rate, tempo, self.time_signature) {
            self.position = self.grid.rebase_onto(self.position, grid);
            self.grid = grid;
            self.resync_clock();
        }
    }

    /// Publishes a reference to every pad that holds a clip.
    fn publish_snapshot(&mut self, request: u32, sink: &mut impl EventSink) {
        let mut published = 0;
        let mut expected = 0;
        for addr in SlotAddr::all() {
            let state = self.session.state(addr);
            // A pad still recording has no finished audio to publish.
            if state.is_recording() {
                continue;
            }
            let Some(clip) = self.audio.clip(addr) else {
                continue;
            };
            expected += 1;
            let snapshot = Snapshot {
                request,
                addr,
                state,
                launch_anchor: self.audio.anchors[addr.track.index()][addr.slot.index()],
                clip: Arc::clone(clip),
            };
            if self.audio.snapshots.publish(snapshot) {
                published += 1;
            }
        }
        sink.event(Event::SnapshotComplete {
            request,
            clips: published,
            expected,
        });
    }

    fn set_paused(&mut self, paused: bool, sink: &mut impl EventSink) {
        if paused {
            if self.paused || self.pending == Some(Deferred::Pause) {
                return;
            }
            self.defer(Deferred::Pause, sink);
            return;
        }

        // A second press cancels a pause that has not landed yet.
        if self.pending == Some(Deferred::Pause) {
            self.pending = None;
        }
        self.paused = false;
    }

    fn set_tempo(&mut self, tempo: Tempo, sink: &mut impl EventSink) {
        // A clear already committed takes every clip with it, so it locks nothing.
        let clearing = self.pending == Some(Deferred::ClearAll);
        if !clearing && self.session.has_any_clip() {
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

        // Checked before the pause below, or a move requested while frozen would never
        // come.
        if let Some(action) = self.pending.filter(|_| self.is_faded()) {
            self.pending = None;
            let before = self.session;
            self.apply_deferred(action, sink);
            self.emit_changes(&before, sink);
        }

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

        limit(output, sink);
        self.report_clock(sink);
    }

    /// Reports the MIDI clock ticks crossed by this block.
    ///
    /// A block is shorter than a tick at any usable tempo, so these arrive spaced.
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
    /// Called after anything that moves the transport other than playing.
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
        self.settle_refusals(sink);
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
            let (track, slot) = (addr.track.index(), addr.slot.index());
            let level = self.levels[track][slot];
            let target = self.target_level(addr);
            if level == 0.0 && target == 0.0 {
                continue;
            }

            let (ramp, reached) = self.ramp_to(level, target, run);
            self.levels[track][slot] = reached;
            if let Some(clip) = self.audio.clip(addr) {
                let anchor = self.audio.anchor(addr, clip);
                clip.mix_from(anchor, self.position, out, ramp);
            }
        }

        // Frames the device did not deliver stay unwritten, which reads back as silence,
        // while the write position still advances so the loop keeps its length.
        //
        // A device that delivers nothing at all leaves `input` empty, so the range can
        // start past its end.
        let channels = self.channels;
        let available = input_frames.saturating_sub(offset).min(run);
        let captured = input
            .get(offset * self.channels..(offset + available) * self.channels)
            .unwrap_or(&[]);

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
                .map_or(0, |clip| match recording.input {
                    TrackInput::Stereo => clip.write(recording.frames, captured, segments),
                    TrackInput::Mono(channel) => clip.write_channel(
                        recording.frames,
                        captured,
                        channels,
                        usize::from(channel),
                        segments,
                    ),
                });

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
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        reason = "tests should fail loudly, and compare exact rendered samples"
    )]

    use super::*;

    #[test]
    fn the_mix_is_held_at_full_scale() {
        let mut output = [2.0, -2.0, 0.5, -0.5];
        let mut events = Vec::new();
        limit(&mut output, &mut events);

        assert_eq!(output, [1.0, -1.0, 0.5, -0.5]);
        assert_eq!(events, vec![Event::Clipped { samples: 2 }]);
    }

    #[test]
    fn a_mix_inside_full_scale_is_left_alone() {
        let mut output = [1.0, -1.0, 0.0];
        let mut events = Vec::new();
        limit(&mut output, &mut events);

        assert_eq!(output, [1.0, -1.0, 0.0], "the limit itself is not clipping");
        assert!(events.is_empty());
    }

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
