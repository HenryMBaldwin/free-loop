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
    BarGrid, ClipId, Command, Ctx, Effect, Event, Frames, LaunchMode, PadMask, SLOT_COUNT,
    SampleRate, SessionModel, Settings, SlotAddr, SlotState, Subdivision, TRACK_COUNT, Tempo,
    TimeError, TimeSignature, TrackInput, UNITY_STEP, gain_for_step, pad_bit,
};

use std::sync::Arc;

use crate::click::{Click, ClickConfig, Tone};
use crate::load::{LoadInbox, LoadMessage, Loader};
use crate::recycle::{Recycler, Retirement, channel};
use crate::snapshot::{Snapshot, SnapshotReader, SnapshotWriter};
use free_loop_clip::{Clip, Ramp, SegmentPool, segments_for};

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

/// The anchor a clip recorded from the transport's start would hold.
///
/// Frame zero was captured `capture_offset` after the beat it belongs to, so the anchor
/// sits that far back.
fn anchor_at_start(len: Frames, capture_offset: Frames) -> Frames {
    if len.0 == 0 {
        return Frames::ZERO;
    }
    Frames((len.0 - capture_offset.0 % len.0) % len.0)
}

/// Capture kept past a loop: every beat of the bar after it but the last.
fn tail_frames(grid: BarGrid) -> u64 {
    let beats = grid.time_signature().beats_per_bar().saturating_sub(1);
    grid.beat_offset(beats).0
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
    /// Capture channel count was zero.
    #[error("capture channel count must be greater than zero")]
    ZeroCaptureChannels,
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
    /// Channels in the interleaved output buffer.
    pub channels: usize,
    /// Channels in the interleaved input buffer, which a track picks from.
    pub capture_channels: usize,
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
            capture_channels: 2,
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
    /// Where capture began.
    started_at: Frames,
    /// Frames of the loop backed by storage.
    written: u64,
    /// The input this take is capturing, fixed when it began.
    input: TrackInput,
    /// Whether the segment pool ran dry part way through.
    starved: bool,
    /// Frames of tail backed by storage.
    tail_written: u64,
    /// Frames to keep capturing to once the loop itself is sealed.
    tail_until: Option<u64>,
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
    /// The grid the arriving session was recorded on.
    grid: Option<BarGrid>,
    clips: [[Option<StagedClip>; SLOT_COUNT]; TRACK_COUNT],
}

impl Staging {
    fn new() -> Self {
        Self {
            receiving: false,
            grid: None,
            clips: core::array::from_fn(|_| core::array::from_fn(|_| None)),
        }
    }

    /// Segments the staged clips would cost the pool.
    fn segments(&self) -> usize {
        self.clips
            .iter()
            .flatten()
            .flatten()
            .fold(0_usize, |total, staged| {
                total.saturating_add(staged.clip.segments())
            })
    }

    /// Gives up a load that never finished arriving.
    ///
    /// The storage belongs to whoever sent it, so it goes back through the recycler rather
    /// than being dropped here.
    fn abandon(&mut self, retirement: &mut Retirement) {
        self.receiving = false;
        self.grid = None;
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
    /// Frames in a bar, which is the shortest take.
    bar_frames: u64,
    /// Frames of capture kept past a loop, which is a bar less its final beat.
    tail_frames: u64,
    /// Which input each track records.
    inputs: [TrackInput; TRACK_COUNT],
    /// Where each track's clips are anchored when launched.
    launch_modes: [LaunchMode; TRACK_COUNT],
    /// Beats each track opens its loops from the tail for.
    pickups: [u8; TRACK_COUNT],
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
        if clip.is_borrowed() {
            self.segments.release(clip.segments());
        }
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
            Effect::StartCapture { at } => {
                // A take is at least one bar; less storage than that cannot produce a
                // clip.
                if self.segments.available() < segments_for(Frames(self.bar_frames)) {
                    self.refused |= pad_bit(addr);
                    return;
                }
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
                    started_at: at,
                    written: 0,
                    starved: false,
                    input: self.inputs[addr.track.index()],
                    tail_written: 0,
                    tail_until: None,
                });
            }

            Effect::FinishCapture {
                clip,
                started_at,
                at,
            } => {
                let tail_from = at.0.saturating_sub(started_at.0);
                let Some(recording) = self.recordings[addr.track.index()][addr.slot.index()]
                    .as_mut()
                    .filter(|recording| recording.tail_until.is_none())
                else {
                    return;
                };
                // Capture carries on into the bar after the loop, and the recording is
                // dropped once it has. Anything already written past the loop is held
                // storage and counts as tail.
                recording.tail_written = recording.written.saturating_sub(tail_from);
                recording.tail_until = Some(tail_from.saturating_add(self.tail_frames));
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
    /// How often the click sounds.
    subdivision: Subdivision,
    loads: LoadInbox,
    channels: usize,
    /// Width of the input buffer, which a track picks one or two channels from.
    capture_channels: usize,
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
    /// The clock count the transport position last stood at.
    last_clock: u64,
    /// MIDI clock ticks reported since the engine was built.
    clock_total: u64,
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
        if config.capture_channels == 0 {
            return Err(EngineError::ZeroCaptureChannels);
        }
        if config.segment_pool == 0 {
            return Err(EngineError::EmptyPool);
        }

        let grid = BarGrid::new(config.sample_rate, config.tempo, config.time_signature)?;

        // One take may use the whole pool. The slots are pointers; only the segments
        // written cost memory.
        let max_segments = config.segment_pool.max(1);

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
                bar_frames: grid.bars(1).0,
                tail_frames: tail_frames(grid),
                inputs: [config.input; TRACK_COUNT],
                launch_modes: [config.launch_mode; TRACK_COUNT],
                pickups: [0; TRACK_COUNT],
                anchors: core::array::from_fn(|_| core::array::from_fn(|_| None)),
                refused: 0,
                staged: Staging::new(),
            },
            click: Click::new(config.click, config.sample_rate),
            subdivision: Subdivision::default(),
            loads,
            channels: config.channels,
            capture_channels: config.capture_channels,
            paused: false,
            muted: 0,
            soloed: 0,
            gains: [UNITY_STEP; TRACK_COUNT],
            levels: [[0.0; SLOT_COUNT]; TRACK_COUNT],
            declick: as_usize(config.declick.0),
            pending: None,
            last_boundary: None,
            last_clock: 0,
            clock_total: 0,
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

    /// Audio a pad's clip holds past its loop.
    pub fn clip_tail(&self, addr: SlotAddr) -> Frames {
        self.audio
            .clip(addr)
            .map_or(Frames::ZERO, |clip| clip.tail())
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
            Command::ClearAll => self.defer(Deferred::ClearAll, sink),
            Command::Rewind => self.defer(Deferred::Rewind, sink),
            Command::Resync => {
                sink.event(Event::Tempo {
                    bpm: self.grid.tempo().bpm(),
                });
                self.report_time_signature(sink);
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
            Command::SetClickSubdivision(subdivision) => self.subdivision = subdivision,
            Command::SetTempo(tempo) => self.set_tempo(tempo, sink),
            Command::SetTimeSignature(signature) => self.set_time_signature(signature, sink),
        }

        self.settle_refusals(sink);
        self.emit_changes(&before, sink);
    }

    /// Takes the latest whole-state settings.
    ///
    /// A take in progress keeps the input it started on, and a clip already sounding
    /// keeps the anchor it was launched with.
    pub fn apply_settings(&mut self, settings: Settings) {
        self.gains = settings.gains;
        self.muted = settings.muted;
        self.soloed = settings.soloed;
        self.audio.inputs = settings.inputs;
        self.audio.launch_modes = settings.launch_modes;
        self.audio.pickups = settings.pickups;
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
            let (track, slot) = (addr.track.index(), addr.slot.index());
            let Some(clip) = self.audio.clips[track][slot].as_mut() else {
                self.audio.anchors[track][slot] = None;
                continue;
            };
            let start = anchor_at_start(clip.len(), clip.capture_offset());
            // A clip somebody is reading keeps its old anchor. Rare, and the next rewind
            // catches it.
            if let Some(inner) = Arc::get_mut(clip) {
                inner.set_recorded_at(start);
            }
            // A pad that is not sounding takes a fresh anchor when it is next launched, so
            // there is nothing to keep.
            self.audio.anchors[track][slot] = self.audio.anchors[track][slot]
                .filter(|_| self.session.state(addr).is_sounding())
                .map(|_| start);
        }
    }

    /// Installs anything the loader has queued.
    fn apply_loads(&mut self, sink: &mut impl EventSink) {
        let before = self.session;
        let Self { audio, loads, .. } = self;

        // Staged, not applied as it arrives: the grid changes once, when all of it is in.
        let mut complete = false;
        while let Some(message) = loads.pop() {
            match message {
                LoadMessage::Begin { grid } => {
                    // Anything staged belongs to a load that never finished arriving.
                    audio.staged.abandon(&mut audio.retirement);
                    audio.staged.receiving = true;
                    audio.staged.grid = Some(grid);
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

        // Checked before the grid is replaced.
        let wanted = self.audio.staged.segments();
        let allowed = self.audio.segments.capacity();
        if wanted > allowed {
            let Self { audio, .. } = self;
            audio.staged.abandon(&mut audio.retirement);
            sink.event(Event::LoadRefused {
                wanted: u32::try_from(wanted).unwrap_or(u32::MAX),
                allowed: u32::try_from(allowed).unwrap_or(u32::MAX),
            });
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
            // Loaded audio is stored outside the pool but counts against it, so the grid
            // is bounded by one number however it got there.
            audio.segments.reserve(staged.clip.segments());
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

        if let Some(grid) = self.audio.staged.grid.take() {
            self.adopt_grid(grid, sink);
        }
        // A rewind or clear queued against the session just replaced would run against the
        // new one, and the commit has already zeroed the levels it was waiting on.
        self.pending = None;
        // A load arrives against a grid the performer has not heard yet, so it waits.
        self.paused = true;
        self.levels = [[0.0; SLOT_COUNT]; TRACK_COUNT];
        self.rewind(sink);
    }

    /// Takes a loaded session's grid, without the guard that protects existing clips.
    ///
    /// The grid exists, so there is nothing here that can fail after the clips are in.
    fn adopt_grid(&mut self, grid: BarGrid, sink: &mut impl EventSink) {
        self.time_signature = grid.time_signature();
        self.position = self.grid.rebase_onto(self.position, grid);
        self.set_grid(grid);
        self.resync_clock();
        self.report_time_signature(sink);
    }

    /// Takes a new bar grid, keeping anything measured against it in step.
    fn set_grid(&mut self, grid: BarGrid) {
        self.grid = grid;
        self.audio.bar_frames = grid.bars(1).0;
        self.audio.tail_frames = tail_frames(grid);
    }

    /// Publishes a reference to every pad that holds a clip.
    fn publish_snapshot(&mut self, request: u32, sink: &mut impl EventSink) {
        let mut published = 0;
        let mut expected = 0;
        for addr in SlotAddr::all() {
            // A clone would stop the tail being written, and freeze the clip at whatever
            // it holds now, so the tail is ended before the clip goes out.
            self.settle_tail(addr);
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
        self.set_grid(grid);
        self.resync_clock();
        // Reported on success as well as refusal, so a resync answer queued earlier cannot
        // end up as the last word on the tempo.
        sink.event(Event::Tempo { bpm: tempo.bpm() });
    }

    /// Takes a new time signature, keeping the tempo.
    fn set_time_signature(&mut self, signature: TimeSignature, sink: &mut impl EventSink) {
        // A clear already committed takes every clip with it, so it locks nothing.
        let clearing = self.pending == Some(Deferred::ClearAll);
        if !clearing && self.session.has_any_clip() {
            sink.event(Event::TimeSignatureRejected);
            return;
        }
        let Ok(grid) = BarGrid::new(self.sample_rate, self.grid.tempo(), signature) else {
            sink.event(Event::TimeSignatureRejected);
            return;
        };

        self.time_signature = signature;
        // Move the transport with the grid, or the same frame count would land on a
        // different beat.
        self.position = self.grid.rebase_onto(self.position, grid);
        self.set_grid(grid);
        self.resync_clock();
        // Reported on success as well as refusal, so a resync answer queued earlier cannot
        // end up as the last word on the signature.
        self.report_time_signature(sink);
    }

    /// Says what signature the transport is running at.
    fn report_time_signature(&mut self, sink: &mut impl EventSink) {
        let signature = self.grid.time_signature();
        sink.event(Event::TimeSignature {
            beats_per_bar: signature.beats_per_bar(),
            beat_unit: signature.beat_unit(),
        });
    }

    /// Renders one block.
    ///
    /// `output` is interleaved at the configured channel count and fully overwritten.
    /// `input` is interleaved at the capture channel count, which is independent of it; if
    /// it is short, the missing frames are treated as silence and an [`Event::Xrun`] is
    /// reported.
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
        let input_frames = input.len() / self.capture_channels;
        if input_frames < frames {
            sink.event(Event::Xrun {
                frames: as_u64(frames - input_frames),
            });
        }

        let mut done = 0;
        while done < frames {
            self.reach_boundary(sink);
            self.reach_click();

            let next = self
                .grid
                .next_beat_boundary(self.position)
                .min(self.grid.next_slice(self.position, self.clicks_per_bar()));
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
        self.clock_total = self.clock_total.saturating_add(ticks);
        sink.event(Event::Clock {
            total: self.clock_total,
        });
    }

    /// Takes the clock count from wherever the transport now is.
    ///
    /// Called after anything that moves the transport other than playing. The running
    /// total is left alone.
    fn resync_clock(&mut self) {
        self.last_clock = self.grid.clock_ticks_at(self.position);
    }

    /// Clicks the bar is cut into.
    fn clicks_per_bar(&self) -> u32 {
        self.subdivision.clicks_per_bar(self.grid.time_signature())
    }

    /// Sounds the click if the transport has reached one of its instants.
    fn reach_click(&mut self) {
        if !self.grid.on_slice(self.position, self.clicks_per_bar()) {
            return;
        }
        let (bar, beat) = self.grid.beat_of(self.position);
        let on_beat = self.grid.bar_start(bar) + self.grid.beat_offset(beat) == self.position;
        let tone = match (on_beat, beat) {
            (true, 0) => Tone::Accent,
            (true, _) => Tone::Beat,
            (false, _) => Tone::Sub,
        };
        self.click.trigger(tone);
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
                let pickup = self.grid.beat_offset(u32::from(self.audio.pickups[track]));
                clip.mix_pickup(anchor, self.position, out, ramp, pickup);
            }
        }

        // Frames the device did not deliver stay unwritten, which reads back as silence,
        // while the write position still advances so the loop keeps its length.
        //
        // A device that delivers nothing at all leaves `input` empty, so the range can
        // start past its end.
        let capture_channels = self.capture_channels;
        let available = input_frames.saturating_sub(offset).min(run);
        let captured = input
            .get(offset * capture_channels..(offset + available) * capture_channels)
            .unwrap_or(&[]);

        let mut done_tailing: PadMask = 0;
        let mut out_of_room: PadMask = 0;
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
            // A starved take writes nothing more, keeping its backed frames contiguous.
            let backed = clips[addr.track.index()][addr.slot.index()]
                .as_mut()
                .filter(|_| !recording.starved)
                .and_then(Arc::get_mut)
                .map_or(0, |clip| {
                    let written = if available > 0 {
                        let picks = recording.input.channels();
                        clip.write_picked(
                            recording.frames,
                            captured,
                            capture_channels,
                            picks.as_slice(),
                            segments,
                        )
                    } else {
                        0
                    };
                    // Frames the device never delivered still count in the clip's length,
                    // so they cost the pool the same as recorded ones.
                    let from = recording.frames + as_u64(written);
                    written + clip.silence(from, run - written, segments)
                });

            // A short tail costs the pickup, not the take, and says nothing.
            if recording.tail_until.is_some() {
                recording.tail_written += as_u64(backed);
            } else {
                recording.written += as_u64(backed);
                if recording.starved {
                    // A press can clear `ends_at`, so the end is forced again every block.
                    out_of_room |= pad_bit(addr);
                } else if backed < run {
                    recording.starved = true;
                    out_of_room |= pad_bit(addr);
                    sink.event(Event::RecordBufferLow { addr });
                }
            }
            recording.frames += as_u64(run);

            // A sealed take carries on until its tail is in, then stops being a recording.
            if recording
                .tail_until
                .is_some_and(|until| recording.frames >= until)
            {
                done_tailing |= pad_bit(addr);
            }
        }

        self.settle_tails(done_tailing);
        self.cut_short(out_of_room, sink);
        self.click.add_into(out, self.channels);
    }

    /// Ends the takes that ran out of storage, at the last whole bar each one holds.
    ///
    /// A take with no whole bar behind it goes back.
    fn cut_short(&mut self, out: PadMask, sink: &mut impl EventSink) {
        if out == 0 {
            return;
        }
        let before = self.session;
        let bar = self.audio.bar_frames.max(1);
        for addr in SlotAddr::all().filter(|addr| out & pad_bit(*addr) != 0) {
            let (track, slot) = (addr.track.index(), addr.slot.index());
            let Some(recording) = self.audio.recordings[track][slot].as_ref() else {
                continue;
            };
            let bars = recording.written / bar;
            if bars == 0 {
                // Nothing whole to keep.
                self.audio.recordings[track][slot] = None;
                if let Some(held) = self.audio.clips[track][slot].take() {
                    self.audio.retire(held);
                }
                self.audio.refused |= pad_bit(addr);
                continue;
            }
            let at = Frames(recording.started_at.0 + bars * bar);
            self.session.finish_recording_at(addr, at);
        }
        self.settle_refusals(sink);
        self.emit_changes(&before, sink);
    }

    /// Ends the recordings whose tail is complete, telling each clip how much it holds.
    fn settle_tails(&mut self, done: PadMask) {
        if done == 0 {
            return;
        }
        for addr in SlotAddr::all().filter(|addr| done & pad_bit(*addr) != 0) {
            self.settle_tail(addr);
        }
    }

    /// Gives a pad's clip the tail its recording captured, and ends the recording.
    ///
    /// The recording is kept if the clip cannot be written to, so the tail is never lost
    /// without being recorded.
    fn settle_tail(&mut self, addr: SlotAddr) {
        let (track, slot) = (addr.track.index(), addr.slot.index());
        let Some(recording) = self.audio.recordings[track][slot].as_ref() else {
            return;
        };
        if recording.tail_until.is_none() {
            return;
        }
        let tail = Frames(recording.tail_written);
        let Some(inner) = self.audio.clips[track][slot]
            .as_mut()
            .and_then(Arc::get_mut)
        else {
            return;
        };
        inner.set_tail(tail);
        self.audio.recordings[track][slot] = None;
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
    fn a_start_anchor_puts_the_downbeat_on_the_transport() {
        let len = Frames(1_000);

        // Frame zero was captured 40 frames after the beat it belongs to, so the anchor
        // sits 40 frames back and the beat itself lands on the transport.
        assert_eq!(anchor_at_start(len, Frames(40)), Frames(960));
        assert_eq!(anchor_at_start(len, Frames::ZERO), Frames::ZERO);
    }

    #[test]
    fn a_start_anchor_stays_inside_the_loop() {
        for offset in [0, 1, 999, 1_000, 123_456] {
            let anchor = anchor_at_start(Frames(1_000), Frames(offset));
            assert!(anchor.0 < 1_000, "offset {offset} gave {anchor:?}");
        }
        assert_eq!(anchor_at_start(Frames::ZERO, Frames(7)), Frames::ZERO);
    }
}
