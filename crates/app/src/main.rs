//! The Free Loop binary.
//!
//! One thread does the control work: poll the surface, queue commands, drain reports,
//! repaint. The audio callbacks run on their own threads inside the device, and the ring
//! buffers are the only thing between them.
//!
//! ```text
//! free-loop [config path]      # defaults to ./free-loop.toml
//! free-loop --print-config     # a config file with every default filled in
//! free-loop --log-surface      # print every gesture the surface reports
//! ```

use core::sync::atomic::{AtomicBool, Ordering};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use free_loop::config::{self, Config};
use free_loop::control::{Controller, Mode, Request, TextUpdate, Work};
use free_loop::gui;
use free_loop_audio::{AudioIo, DeviceChange, DroppedEvents, Negotiated, open};
use free_loop_core::{Command, Event, TimeSignature, TrackInput};
use free_loop_engine::{Engine, Housekeeping, Loader, Snapshot};
use free_loop_session::{SavedClip, SessionData, SessionStore, TrackSettings};
use free_loop_surface::{ControlSurface, LaunchpadX, Reconnecting, SurfaceEvent};
use launchpad_emulator_ui::Console;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// How often the control loop runs.
const TICK: Duration = Duration::from_millis(2);

/// Where the config lives unless told otherwise.
const DEFAULT_CONFIG: &str = "free-loop.toml";

/// What the command line asked for.
struct Args {
    path: PathBuf,
    log_surface: bool,
}

/// Parses the command line. `None` means the request was answered already.
fn parse_args() -> Result<Option<Args>, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        path: PathBuf::from(DEFAULT_CONFIG),
        log_surface: false,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--log-surface" => parsed.log_surface = true,
            "--print-config" => {
                print!("{}", config::EXAMPLE);
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("free-loop [config path]");
                println!("free-loop --print-config");
                println!("free-loop --log-surface");
                return Ok(None);
            }
            "--config" => {
                parsed.path = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or("--config needs a path")?;
            }
            other => parsed.path = PathBuf::from(other),
        }
    }
    Ok(Some(parsed))
}

/// How a track's input reads on the startup line.
fn describe_input(input: TrackInput) -> String {
    match input {
        TrackInput::Mono(channel) => format!("input channel {channel}"),
        TrackInput::Pair(left, right) => format!("input channels {left} and {right}"),
    }
}

/// Sends traces to stderr at `info`, or whatever `RUST_LOG` asks for. `log_surface`
/// raises this crate to `debug`, where surface gestures report.
fn init_tracing(log_surface: bool, console: Option<Console>) {
    subscriber(log_surface, console).init();
}

/// The subscriber [`init_tracing`] installs, logging to the window's pane when there is
/// a window.
fn subscriber(
    log_surface: bool,
    console: Option<Console>,
) -> impl tracing::Subscriber + Send + Sync {
    let fallback = if log_surface {
        "info,free_loop=debug"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    // Stderr keeps the traces after the window closes, and outlives it either way.
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(console.map(|console| console.layer()))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(Args { path, log_surface }) = parse_args()? else {
        return Ok(());
    };
    // Read before the traces are set up, since it says whether they need a console.
    let config = Config::load(&path)?;
    let console = config.gui.enabled.then(Console::new);
    init_tracing(log_surface, console.clone());
    // A missing config falls back to the default devices, which sounds like broken
    // recording rather than like a missing file unless it says so here.
    if path.exists() {
        tracing::info!("config: {}", path.display());
    } else {
        tracing::info!("config: {} not found, using defaults", path.display());
    }

    let running = Arc::new(AtomicBool::new(true));
    // The window runs on this thread and the looper on another, so the screen showing has
    // to be published for the labels to follow it.
    let showing = Arc::new(Mutex::new(Mode::Perform));
    ctrlc::set_handler({
        let running = Arc::clone(&running);
        move || running.store(false, Ordering::Relaxed)
    })?;

    let Some(console) = console else {
        return play(&path, &config, None, &running, None);
    };

    // The window has to hold the main thread, so the looper takes one of its own. Its
    // ports go up first, or the surface would spend a retry looking for them.
    let emulator = gui::open()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let worker = std::thread::spawn({
        let (path, config) = (path.clone(), config.clone());
        let (running, stopped) = (Arc::clone(&running), Arc::clone(&stopped));
        let showing = Arc::clone(&showing);
        move || {
            // On drop rather than after the call, so a panic still closes the window.
            let _stopping = Stopping {
                running: Arc::clone(&running),
                stopped,
            };
            // Reduced to its text, which a `Box<dyn Error>` cannot cross a thread to give.
            play(
                &path,
                &config,
                Some(gui::PORT_NAME),
                &running,
                Some(showing),
            )
            .map_err(|error| {
                // Reported here as well, or the window would close saying nothing.
                tracing::error!("{error}");
                error.to_string()
            })
        }
    });

    let shown = gui::run(emulator, console, Arc::clone(&running), stopped, showing);
    running.store(false, Ordering::Relaxed);
    // The looper's own failure is why the window closed, so it is the one to report.
    match worker.join() {
        Ok(outcome) => outcome?,
        Err(_) => return Err("the looper thread stopped without saying why".into()),
    }
    shown?;
    Ok(())
}

/// Tells the window the looper has finished, however its thread ended.
struct Stopping {
    running: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Drop for Stopping {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.stopped.store(true, Ordering::Relaxed);
    }
}

/// Opens the devices and runs the control loop until `running` clears, taking the surface
/// from the exactly named `port` when one is given.
fn play(
    path: &Path,
    config: &Config,
    port: Option<&str>,
    running: &AtomicBool,
    showing: Option<Arc<Mutex<Mode>>>,
) -> Result<(), Box<dyn Error>> {
    let opened = open(&config.audio())?;
    let negotiated = opened.negotiated();
    tracing::info!("input:  {}", opened.input_name());
    tracing::info!("output: {}", opened.output_name());
    tracing::info!(
        "audio: {} Hz, {} channels in / {} out, {} frames of cushion",
        negotiated.sample_rate,
        negotiated.input_channels,
        negotiated.channels,
        negotiated.cushion_frames
    );
    let input = config.track_input(negotiated.capture_channels);
    tracing::info!("tracks start on {}", describe_input(input));

    let (engine, mut housekeeping) = Engine::new(config.engine(
        negotiated.sample_rate,
        negotiated.channels,
        negotiated.capture_channels,
    )?)?;
    let mut io = opened.start(engine)?;

    let mut surface = connect_surface(port);
    let mut controller = Controller::new(
        config.transport.tempo,
        config.time_signature()?,
        config.click.enabled,
    );

    let store = SessionStore::new(path.parent().unwrap_or(Path::new(".")).join("sessions"));
    // A save interrupted between its two renames leaves a session under another name.
    for trouble in store.recover() {
        tracing::warn!("sessions: {trouble}");
    }
    controller.set_sessions(store.index());
    controller.set_input_count(negotiated.capture_channels);
    controller.set_default_input(input);
    controller.set_default_launch_mode(config.launch_mode());
    controller.set_default_time_signature(config.time_signature()?);
    controller.set_inputs([input; free_loop_core::TRACK_COUNT]);
    controller.set_launch_modes([config.launch_mode(); free_loop_core::TRACK_COUNT]);

    tracing::info!(
        "transport: {:.1} bpm, {}/{}",
        controller.tempo(),
        config.transport.beats_per_bar,
        config.transport.beat_unit
    );
    tracing::info!("running. ctrl-c to stop.");

    run(Session {
        io: &mut io,
        surface: surface.as_mut(),
        controller: &mut controller,
        housekeeping: &mut housekeeping,
        store: &store,
        config,
        negotiated,
        running,
        showing,
    });

    // Leaving the grid lit after the process is gone looks like it is still running.
    if let Err(error) = surface.clear() {
        tracing::warn!("surface: {error}");
    }
    tracing::info!("stopped. device errors: {}", io.device_errors());
    Ok(())
}

/// What the control loop needs from the audio side.
///
/// The loop is written against this rather than against a device, so a pass can be driven
/// without one.
trait Audio {
    /// Queues a command for the engine. `Err` if the ring is full.
    fn send(&mut self, command: Command) -> Result<(), Command>;

    /// Hands every report the engine has made to `handler`.
    fn drain_events(&mut self, handler: impl FnMut(Event));

    /// How many reports of each kind the engine had to throw away.
    fn dropped_events(&self) -> DroppedEvents;

    /// Lets the device be looked for, reporting when one comes or goes.
    fn tick(&mut self, now: Duration) -> Option<DeviceChange>;

    /// The round trip the engine is compensating for, or zero before it is known.
    fn capture_offset_frames(&self) -> u32;
}

impl Audio for AudioIo {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        AudioIo::send(self, command)
    }

    fn drain_events(&mut self, handler: impl FnMut(Event)) {
        AudioIo::drain_events(self, handler);
    }

    fn dropped_events(&self) -> DroppedEvents {
        AudioIo::dropped_events(self)
    }

    fn tick(&mut self, now: Duration) -> Option<DeviceChange> {
        AudioIo::tick(self, now)
    }

    fn capture_offset_frames(&self) -> u32 {
        AudioIo::capture_offset_frames(self)
    }
}

/// Everything the control loop touches.
struct Session<'a, A: Audio> {
    io: &'a mut A,
    surface: &'a mut dyn ControlSurface,
    controller: &'a mut Controller,
    housekeeping: &'a mut Housekeeping,
    store: &'a SessionStore,
    config: &'a Config,
    negotiated: Negotiated,
    running: &'a AtomicBool,
    /// Where to publish the screen showing, for a window that names its buttons.
    showing: Option<Arc<Mutex<Mode>>>,
}

/// What one pass of the control loop leaves for the next.
struct Looping {
    /// Everything the performer has asked for that the engine has not taken, in order.
    queued: Vec<Work>,
    /// Whether a load is still sitting in the loader's channel.
    loading: bool,
    /// A save waiting on the answer to one request.
    pending_save: Option<PendingSave>,
    /// Clips the engine has published towards the save that is waiting.
    snapshots: Vec<Snapshot>,
    /// Tags one snapshot request apart from another.
    next_request: u32,
    clipping: ClipReport,
    xruns: XrunReport,
    /// Reports that never arrived, against the engine's running count.
    missed_reports: DroppedEvents,
    /// Clock ticks the surface has had, against the total the engine reports.
    clock_sent: u64,
    /// Whether the round trip has been reported, which is only known once it runs.
    reported_latency: bool,
    /// Whether a surface was attached last pass.
    connected: bool,
    /// The screen last published to a window.
    published: Option<Mode>,
}

impl Looping {
    fn new(connected: bool) -> Self {
        Self {
            queued: Vec::new(),
            loading: false,
            pending_save: None,
            snapshots: Vec::new(),
            next_request: 0,
            clipping: ClipReport::default(),
            xruns: XrunReport::default(),
            missed_reports: DroppedEvents::default(),
            clock_sent: 0,
            reported_latency: false,
            connected,
            published: None,
        }
    }
}

/// Polls the surface, drives the engine and repaints until asked to stop.
fn run<A: Audio>(mut s: Session<'_, A>) {
    let mut state = Looping::new(s.surface.is_connected());
    let started = Instant::now();

    while s.running.load(Ordering::Relaxed) {
        s.pass(&mut state, started.elapsed());
        std::thread::sleep(TICK);
    }
}

impl<A: Audio> Session<'_, A> {
    /// One turn of the control loop: poll, hand over, drain, repaint.
    ///
    /// Split out from [`run`] so a pass can be driven with a clock of the caller's
    /// choosing, and without a device.
    fn pass(&mut self, state: &mut Looping, now: Duration) {
        let io = &mut *self.io;
        let surface = &mut *self.surface;
        let controller = &mut *self.controller;
        let housekeeping = &mut *self.housekeeping;
        let (store, config, negotiated) = (self.store, self.config, self.negotiated);
        let mut events: Vec<SurfaceEvent> = Vec::new();

        state.connected = watch_surface(surface, now, state.connected);
        watch_devices(io, now, controller, config.audio.pause_on_disconnect);

        events.clear();
        surface.poll(&mut events);
        for event in events.drain(..) {
            tracing::debug!("surface: {event:?}");
            controller.on_surface(event, now);
        }
        controller.tick(now);
        state.published =
            publish_screen(self.showing.as_deref(), controller.mode(), state.published);
        // Returns clips the engine finished with while something else was reading them.
        housekeeping.recycler.run();

        // Storage the engine has finished with comes back here to be dropped.
        housekeeping.recycler.take_borrowed().for_each(drop);

        // Cleared once the engine has taken every step of the load, which it commits in
        // the same callback that empties the channel.
        state.loading &= !housekeeping.loader.ready();

        state.queued.extend(controller.drain_work());

        // Handed over in order, stopping at whatever cannot go yet. A request surfaces
        // only once everything asked for before it has reached the engine.
        while let Some(request) = hand_over(&mut state.queued, state.loading, |command| {
            io.send(command).is_ok()
        }) {
            match request {
                Request::SaveSession(addr) => {
                    state.next_request = state.next_request.wrapping_add(1);
                    state.snapshots.clear();
                    let save = ask_for_snapshot(
                        &mut state.queued,
                        state.next_request,
                        addr,
                        controller,
                        now,
                    );
                    state.pending_save = Some(save);
                }
                Request::LoadSession(addr) => {
                    load_session(
                        store,
                        addr,
                        &mut housekeeping.loader,
                        &negotiated,
                        controller,
                        config,
                        now,
                    );
                    // Nothing more goes out until the engine has taken it. A load that
                    // never reached the channel clears this on the next pass.
                    state.loading = true;
                }
            }
            // Whatever acting on it asked for belongs where the request stood, in front of
            // anything the performer asked for after it.
            let produced: Vec<Work> = controller.drain_work().collect();
            state.queued.splice(0..0, produced);
        }

        let Drained {
            answered,
            clock_total,
            clipped,
            short_frames,
        } = drain_engine(io, controller, now);
        // After draining, so the replay it asks for has somewhere to go.
        state.missed_reports = resync_after_loss(io, &mut state.queued, state.missed_reports);
        state.clipping.note(clipped, now);
        state.xruns.note(short_frames, now);

        state.clock_sent = forward_clock(surface, clock_total, state.clock_sent);
        collect_snapshots(
            &mut housekeeping.snapshots,
            state.pending_save.as_ref(),
            &mut state.snapshots,
        );
        let outcome = resolve_save(&mut state.pending_save, answered, now);
        if carry_out_save(
            outcome,
            store,
            config,
            &negotiated,
            &state.snapshots,
            controller,
            now,
        ) {
            state.snapshots.clear();
        }

        repaint(surface, controller);

        state.reported_latency |= report_latency(io, &negotiated, state.reported_latency);
    }
}

/// How long a save waits for the engine to publish its clips before giving up.
const SAVE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to gather short capture before mentioning it.
const XRUN_REPORT_EVERY: Duration = Duration::from_secs(2);

/// How long to gather clipping before saying anything.
const CLIP_REPORT_EVERY: Duration = Duration::from_secs(2);

/// Collects short capture blocks so a dropout is one line, not one per block.
///
/// A device delivering nothing reports every block, which at any usable block size is
/// fast enough that printing each one holds up the control loop.
#[derive(Debug, Default)]
struct XrunReport {
    frames: u64,
    last: Duration,
}

impl XrunReport {
    fn note(&mut self, frames: u64, now: Duration) {
        self.frames += frames;
        if self.frames == 0 || now.saturating_sub(self.last) < XRUN_REPORT_EVERY {
            return;
        }

        tracing::warn!("capture came up short by {} frames", self.frames);
        self.frames = 0;
        self.last = now;
    }
}

/// Counts clipped samples and mentions them now and then.
#[derive(Default)]
struct ClipReport {
    samples: u64,
    last: Duration,
}

impl ClipReport {
    fn note(&mut self, samples: u32, now: Duration) {
        self.samples += u64::from(samples);
        if self.samples == 0 || now.saturating_sub(self.last) < CLIP_REPORT_EVERY {
            return;
        }

        tracing::warn!(
            "output is clipping ({} samples held); turn tracks down with the volume button",
            self.samples
        );
        self.samples = 0;
        self.last = now;
    }
}

/// Prints the measured round trip once the driver has reported it.
///
/// Returns whether it has now been printed.
fn report_latency<A: Audio>(io: &A, negotiated: &Negotiated, already: bool) -> bool {
    if already {
        return true;
    }
    let frames = io.capture_offset_frames();
    if frames == 0 {
        return false;
    }
    let millis = f64::from(frames) / f64::from(negotiated.sample_rate) * 1000.0;
    tracing::info!("round trip: {frames} frames ({millis:.1} ms), compensated");
    true
}

/// The segments a session needs, when that is why it was refused.
fn too_large(error: &(dyn Error + 'static)) -> Option<String> {
    match error.downcast_ref::<free_loop_session::SessionError>()? {
        free_loop_session::SessionError::TooLarge { wanted, .. } => Some(wanted.to_string()),
        _ => None,
    }
}

/// Reads a session in and tells the controller what came with it.
fn load_session(
    store: &SessionStore,
    addr: free_loop_core::SlotAddr,
    loader: &mut Loader,
    negotiated: &Negotiated,
    controller: &mut Controller,
    config: &Config,
    now: Duration,
) {
    match load(store, addr, loader, negotiated, config) {
        Ok(restored) => {
            tracing::info!("loaded session {}{}", addr.track.index(), addr.slot.index());
            controller.set_gains(core::array::from_fn(|track| {
                restored.tracks[track].gain_step
            }));
            controller.set_loaded_tempo(restored.tempo);
            controller.set_loaded_time_signature(restored.time_signature);
            // A session recorded on a wider interface names channels this one may not
            // have.
            controller.set_inputs(core::array::from_fn(|track| {
                restored.tracks[track]
                    .input
                    .within(negotiated.capture_channels)
            }));
            controller.set_launch_modes(core::array::from_fn(|track| {
                if restored.tracks[track].restart {
                    free_loop_core::LaunchMode::Restart
                } else {
                    free_loop_core::LaunchMode::Follow
                }
            }));
            controller.set_pickups(core::array::from_fn(|track| restored.tracks[track].pickup));
            controller.session_loaded(addr, true);
        }
        Err(error) => {
            tracing::error!(
                "load of session {}{} failed: {error}",
                addr.track.index(),
                addr.slot.index()
            );
            controller.load_failed(now, too_large(error.as_ref()));
        }
    }
}

/// Takes the snapshots belonging to the save that is waiting, dropping stale ones.
fn collect_snapshots(
    reader: &mut free_loop_engine::SnapshotReader,
    pending: Option<&PendingSave>,
    into: &mut Vec<Snapshot>,
) {
    let wanted = pending.map(|save| save.request);
    reader.drain(|snapshot| {
        // An answer to a request that has been superseded is not part of this save.
        if Some(snapshot.request) == wanted {
            into.push(snapshot);
        }
    });
}

/// Acts on what became of a save. Returns whether its snapshots are finished with.
fn carry_out_save(
    outcome: SaveOutcome,
    store: &SessionStore,
    config: &Config,
    negotiated: &Negotiated,
    snapshots: &[Snapshot],
    controller: &mut Controller,
    now: Duration,
) -> bool {
    match outcome {
        SaveOutcome::Waiting => false,
        SaveOutcome::Answered(save) => {
            settle_save(store, &save, config, negotiated, snapshots, controller, now);
            true
        }
        SaveOutcome::Expired(addr) => {
            tracing::warn!(
                "save to {}{} never completed; nothing was written",
                addr.track.index(),
                addr.slot.index()
            );
            controller.save_failed(now);
            true
        }
    }
}

/// What became of the save that was waiting.
#[derive(Debug)]
enum SaveOutcome {
    /// Still waiting for its answer.
    Waiting,
    /// Its answer arrived.
    Answered(Answered),
    /// Its answer never came.
    Expired(free_loop_core::SlotAddr),
}

/// Decides what to do with the save that is waiting.
///
/// An answer that arrived wins over the deadline, even on the pass the deadline falls on:
/// the engine will not send it again.
fn resolve_save(
    pending: &mut Option<PendingSave>,
    answered: Option<(u32, u32, u32)>,
    now: Duration,
) -> SaveOutcome {
    if let Some((request, clips, expected)) = answered
        && pending.as_ref().is_some_and(|save| save.request == request)
        && let Some(save) = pending.take()
    {
        return SaveOutcome::Answered(Answered {
            addr: save.addr,
            settings: save.settings,
            clips,
            expected,
        });
    }
    match pending.take_if(|save| now >= save.deadline) {
        Some(save) => SaveOutcome::Expired(save.addr),
        None => SaveOutcome::Waiting,
    }
}

/// A save whose snapshots have all been accounted for.
#[derive(Debug)]
struct Answered {
    addr: free_loop_core::SlotAddr,
    /// What was set when the snapshot was asked for.
    settings: SaveSettings,
    /// Pads that arrived.
    clips: u32,
    /// Pads there were to send. More than `clips` means some were lost on the way.
    expected: u32,
}

/// Writes a save, or reports that not all of it arrived.
fn settle_save(
    store: &SessionStore,
    save: &Answered,
    config: &Config,
    negotiated: &Negotiated,
    snapshots: &[Snapshot],
    controller: &mut Controller,
    now: Duration,
) {
    if save.clips != save.expected {
        tracing::warn!(
            "save abandoned: {} of {} pads arrived",
            save.clips,
            save.expected
        );
        controller.save_failed(now);
        return;
    }
    write_session(store, save, config, negotiated, snapshots, controller, now);
}

/// A save waiting on the engine to publish its clips.
#[derive(Debug)]
struct PendingSave {
    /// When the answer stops being expected.
    ///
    /// A completion is a reply, not a state, so a lost one cannot be replayed.
    deadline: Duration,
    /// The request the engine will tag its answer with.
    request: u32,
    /// Where the session goes.
    addr: free_loop_core::SlotAddr,
    /// Taken when the snapshot was asked for, so the audio and the settings describe the
    /// same moment.
    settings: SaveSettings,
}

/// What a save records besides the audio.
#[derive(Debug)]
struct SaveSettings {
    /// What the performer has the transport set to, not what the config file says.
    tempo: f64,
    /// The signature the material is in, which a load can have changed.
    time_signature: TimeSignature,
    tracks: [TrackSettings; free_loop_core::TRACK_COUNT],
}

/// What the controller currently has set.
fn save_settings(controller: &Controller) -> SaveSettings {
    let inputs = controller.inputs();
    let modes = controller.launch_modes();
    let pickups = controller.pickups();
    let gains = controller.gains();
    SaveSettings {
        tempo: controller.tempo(),
        time_signature: controller.time_signature(),
        tracks: core::array::from_fn(|track| TrackSettings {
            input: inputs[track],
            restart: modes[track].restarts(),
            pickup: pickups[track],
            gain_step: gains[track],
        }),
    }
}

/// Writes the snapshotted clips out under `addr`.
fn save(
    store: &SessionStore,
    addr: free_loop_core::SlotAddr,
    config: &Config,
    negotiated: &free_loop_audio::Negotiated,
    snapshots: &[Snapshot],
    settings: &SaveSettings,
) -> Result<(), free_loop_session::SessionError> {
    let clips = snapshots
        .iter()
        .map(|snapshot| SavedClip {
            addr: snapshot.addr,
            gain_step: settings.tracks[snapshot.addr.track.index()].gain_step,
            playing: matches!(
                snapshot.state,
                free_loop_core::SlotState::Playing { .. }
                    | free_loop_core::SlotState::QueuedStop { .. }
            ),
            launch_anchor: snapshot.launch_anchor,
            clip: &snapshot.clip,
        })
        .collect();

    store.save(
        addr,
        &SessionData {
            tempo: settings.tempo,
            beats_per_bar: settings.time_signature.beats_per_bar(),
            beat_unit: settings.time_signature.beat_unit(),
            sample_rate: negotiated.sample_rate,
            channels: u16::try_from(negotiated.channels).unwrap_or(2),
            clips,
            tracks: settings.tracks,
        },
        config.load_budget(),
    )
}

/// Shows any frame, then any text.
///
/// Text takes the grid over, so the frame goes first and nothing more is sent until the
/// text finishes.
fn repaint(surface: &mut dyn ControlSurface, controller: &mut Controller) {
    if let Some(frame) = controller.take_frame()
        && let Err(error) = surface.render(frame)
    {
        tracing::warn!("surface: {error}");
    }

    if let Some(update) = controller.take_text() {
        let shown = match update {
            TextUpdate::Show(text) => surface.show_text(&text),
            TextUpdate::Stop => surface.stop_text(),
        };
        if let Err(error) = shown {
            tracing::warn!("surface: {error}");
        }
    }
}

/// Reads a session off disk and hands it to the engine.
fn load(
    store: &SessionStore,
    addr: free_loop_core::SlotAddr,
    loader: &mut Loader,
    negotiated: &Negotiated,
    config: &Config,
) -> Result<Restored, Box<dyn Error>> {
    let channels = u16::try_from(negotiated.channels).unwrap_or(2);

    // Everything a session can be refused for is settled before any of its audio is read,
    // so refusing one costs a single parse.
    let checked =
        store
            .inspect(addr)?
            .accepts(negotiated.sample_rate, channels, config.load_budget())?;

    let manifest = checked.manifest();
    let time_signature =
        free_loop_core::TimeSignature::new(manifest.beats_per_bar, manifest.beat_unit)?;
    let tempo = free_loop_core::Tempo::new(manifest.tempo)?;
    // Discarded: only the loader builds the grid that is sent. Checked here so a bar the
    // engine cannot measure is refused before any audio is read.
    free_loop_core::BarGrid::new(
        free_loop_core::SampleRate::new(negotiated.sample_rate)?,
        tempo,
        time_signature,
    )?;

    let session = checked.materialise()?;
    let restored = Restored {
        tracks: session.tracks(),
        tempo: session.manifest.tempo,
        time_signature,
    };

    if !loader.ready() {
        return Err("the audio thread has not drained the load queue".into());
    }
    // The loader builds the grid, so an unmeasurable one is refused before any clip is
    // read and it carries the rate of the engine it is going to.
    loader.begin(tempo, time_signature)?;
    for loaded in session.clips {
        loader.clip(
            loaded.addr,
            std::sync::Arc::new(loaded.clip),
            loaded.playing,
            loaded.launch_anchor,
        )?;
    }
    loader.end()?;
    Ok(restored)
}

/// What a loaded session sets besides its audio.
struct Restored {
    tracks: [TrackSettings; free_loop_core::TRACK_COUNT],
    /// What the session was recorded at, which the engine has already taken.
    tempo: f64,
    /// What the session was recorded in, which the engine has already taken.
    time_signature: TimeSignature,
}

/// Writes a snapshot to a pad and tells the controller it landed.
fn write_session(
    store: &SessionStore,
    save_to: &Answered,
    config: &Config,
    negotiated: &Negotiated,
    snapshots: &[Snapshot],
    controller: &mut Controller,
    now: Duration,
) {
    let addr = save_to.addr;
    match save(
        store,
        addr,
        config,
        negotiated,
        snapshots,
        &save_to.settings,
    ) {
        Ok(()) => {
            tracing::info!("saved session {}{}", addr.track.index(), addr.slot.index());
            controller.session_saved(addr, now);
        }
        Err(error) => {
            tracing::error!("save failed: {error}");
            controller.save_failed(now);
        }
    }
}

/// Passes on the pulses the device has not had yet, returning the new total sent.
///
/// Keeps the device's flash and pulse animations on the transport's tempo.
fn forward_clock(surface: &mut dyn ControlSurface, total: Option<u64>, sent: u64) -> u64 {
    let Some(total) = total else { return sent };

    let ticks = u32::try_from(total.saturating_sub(sent)).unwrap_or(u32::MAX);
    if ticks > 0
        && let Err(error) = surface.send_clock(ticks)
    {
        tracing::warn!("surface: {error}");
    }
    total
}

/// What one pass of the engine's reports added up to.
struct Drained {
    /// The snapshot completion, if one arrived.
    answered: Option<(u32, u32, u32)>,
    /// The transport's running clock count, if it reported one.
    clock_total: Option<u64>,
    clipped: u32,
    short_frames: u64,
}

/// Takes everything the engine has reported, printing and mirroring as it goes.
fn drain_engine<A: Audio>(io: &mut A, controller: &mut Controller, now: Duration) -> Drained {
    let mut drained = Drained {
        answered: None,
        clock_total: None,
        clipped: 0,
        short_frames: 0,
    };
    io.drain_events(|event| {
        match event {
            Event::SnapshotComplete {
                request,
                clips,
                expected,
            } => drained.answered = Some((request, clips, expected)),
            Event::Clock { total } => drained.clock_total = Some(total),
            Event::Clipped { samples } => drained.clipped += samples,
            Event::Xrun { frames } => drained.short_frames += frames,
            _ => {}
        }
        report(event);
        controller.on_engine(event, now);
    });
    drained
}

/// Asks the engine to report every pad again if a report a resync repairs was lost.
///
/// The controller paints from a mirror kept in step by those reports. Kinds a resync
/// cannot repair are reported and otherwise left.
fn resync_after_loss<A: Audio>(
    io: &A,
    queued: &mut Vec<Work>,
    seen: DroppedEvents,
) -> DroppedEvents {
    let dropped = io.dropped_events();
    if dropped == seen {
        return seen;
    }
    tracing::warn!("missed engine reports: {}", lost_report(&dropped, &seen));

    if !needs_resync(&dropped, &seen) {
        return dropped;
    }
    // Queued rather than sent: a resync answers with the engine's own state, and sending
    // one in front of a load would answer with the session the load is replacing.
    queued.push(Work::Command(Command::Resync));
    dropped
}

/// Tells a window which screen is showing, when it is not the one it was last told.
///
/// Returns what it now knows, which is unchanged if the slot was busy.
fn publish_screen(slot: Option<&Mutex<Mode>>, mode: Mode, published: Option<Mode>) -> Option<Mode> {
    if published == Some(mode) {
        return published;
    }
    let Some(slot) = slot else {
        return Some(mode);
    };
    // Never blocks the loop for a label: the next pass tries again.
    match slot.try_lock() {
        Ok(mut showing) => {
            *showing = mode;
            Some(mode)
        }
        Err(_) => published,
    }
}

/// Hands `queued` to the engine in order, up to the first thing that cannot go yet.
///
/// Returns the request that surfaced, which is the caller's to act on: everything asked
/// for before it has reached the engine by then. Nothing moves while `loading`, since the
/// audio side applies a load after the commands it has already taken.
fn hand_over(
    queued: &mut Vec<Work>,
    loading: bool,
    mut send: impl FnMut(Command) -> bool,
) -> Option<Request> {
    while let Some(work) = queued.first().copied() {
        if loading {
            return None;
        }
        match work {
            Work::Command(command) => {
                if !send(command) {
                    // No room. Order matters, so the rest wait behind it.
                    return None;
                }
                queued.remove(0);
            }
            Work::Request(request) => {
                queued.remove(0);
                return Some(request);
            }
        }
    }
    None
}

/// Asks the engine to publish its clips, and starts the wait for them.
fn ask_for_snapshot(
    queued: &mut Vec<Work>,
    request: u32,
    addr: free_loop_core::SlotAddr,
    controller: &Controller,
    now: Duration,
) -> PendingSave {
    // Queued behind whatever is waiting, so the snapshot sees the pads as they were when
    // the save was asked for. One that never gets out expires on its deadline.
    queued.insert(0, Work::Command(Command::Snapshot { request }));
    PendingSave {
        deadline: now + SAVE_TIMEOUT,
        request,
        addr,
        settings: save_settings(controller),
    }
}

/// Whether asking the engine again would put any of what was lost right.
fn needs_resync(dropped: &DroppedEvents, seen: &DroppedEvents) -> bool {
    dropped.since(seen).any(|(kind, _)| kind.is_replayed())
}

/// Names what was lost between two counts, as `1 slot change, 3 beats`.
fn lost_report(dropped: &DroppedEvents, seen: &DroppedEvents) -> String {
    dropped
        .since(seen)
        .map(|(kind, count)| {
            let plural = if count == 1 { "" } else { "s" };
            format!("{count} {}{plural}", kind.name())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Lets the audio devices come back after being unplugged, reporting what changed.
fn watch_devices<A: Audio>(
    io: &mut A,
    now: Duration,
    controller: &mut Controller,
    pause_on_disconnect: bool,
) {
    match io.tick(now) {
        Some(DeviceChange::Lost(loss)) => {
            controller.device_lost(now);
            if pause_on_disconnect {
                controller.pause();
                tracing::warn!("audio: device gone ({loss}). paused");
            } else {
                tracing::warn!("audio: device gone ({loss}). held where it stopped");
            }
        }
        Some(DeviceChange::Back) => tracing::info!("audio: device back"),
        Some(DeviceChange::Refused(error)) => tracing::error!("audio: {error}"),
        None => {}
    }
}

/// Lets the surface look for its device, reporting when it comes or goes.
///
/// Returns whether one is attached now.
fn watch_surface(surface: &mut dyn ControlSurface, now: Duration, connected: bool) -> bool {
    surface.tick(now);

    let attached = surface.is_connected();
    if attached != connected {
        if attached {
            tracing::info!("surface: Launchpad X back");
        } else {
            tracing::warn!("surface: gone, still looking");
        }
    }
    attached
}

/// Connects a Launchpad, and keeps looking for one as long as the process runs.
///
/// A missing pad is not fatal: the click and the audio path still work.
fn connect_surface(port: Option<&str>) -> Box<dyn ControlSurface> {
    let wanted = port.map(str::to_owned);
    let surface = Reconnecting::new(move || match &wanted {
        Some(name) => LaunchpadX::connect_to(name),
        None => LaunchpadX::connect(),
    });
    if surface.is_connected() {
        tracing::info!("surface: {}", port.unwrap_or("Launchpad X"));
    } else {
        if let Some(name) = port {
            tracing::warn!("surface: no port named \"{name}\", still looking");
        } else {
            tracing::warn!(
                "surface: no port containing \"{}\", still looking",
                LaunchpadX::PORT_KEYWORD
            );
        }
        let ports = free_loop_surface::output_ports();
        if ports.is_empty() {
            tracing::warn!("surface: the host lists no midi outputs at all");
        } else {
            tracing::warn!("surface: midi outputs seen: {}", ports.join(", "));
        }
    }
    Box::new(surface)
}

/// Prints what is worth knowing. Bars, beats and slot changes are on the grid already.
fn report(event: Event) {
    match event {
        Event::ClipRecorded { addr, len, .. } => {
            tracing::info!(
                "recorded track {} slot {}: {} frames",
                addr.track.index(),
                addr.slot.index(),
                len.0
            );
        }

        Event::RecordingRefused { addr } => {
            tracing::warn!(
                "no clip from track {} slot {}; the pad is left empty",
                addr.track.index(),
                addr.slot.index()
            );
        }
        Event::RecordBufferLow { addr } => {
            tracing::warn!(
                "out of recording space on track {} slot {}; the take will be cut short",
                addr.track.index(),
                addr.slot.index()
            );
        }
        Event::LoadRefused { wanted, allowed } => {
            tracing::warn!("session needs {wanted} segments but the pool holds {allowed}");
        }
        Event::TempoRejected => tracing::warn!("tempo is locked while clips exist"),
        Event::TimeSignatureRejected => {
            tracing::warn!("the time signature is locked while clips exist");
        }
        // Clipping and short capture report per block, too often to print. `ClipReport`
        // and `XrunReport` throttle them. The tempo reports on every nudge, which a held
        // button repeats eight times a second; the controller wants it, a reader does not.
        Event::Tempo { .. }
        | Event::TimeSignature { .. }
        | Event::Clipped { .. }
        | Event::Xrun { .. }
        | Event::SnapshotComplete { .. }
        | Event::Clock { .. }
        | Event::Bar { .. }
        | Event::Beat { .. }
        | Event::SlotChanged { .. }
        | Event::ClipReleased { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        reason = "tests should fail loudly, and compare the exact values they set"
    )]

    use super::*;

    const DEADLINE: Duration = Duration::from_secs(2);

    /// The audio side, with the engine the loop is really talking to.
    ///
    /// Commands wait in a ring the way they do on the device, and only reach the engine
    /// when [`FakeAudio::callback`] runs, which drains them before it processes a block.
    /// That is the order the audio thread uses, and it is what a load has to survive.
    struct FakeAudio {
        engine: Engine,
        /// Waiting for the next callback, as they would in the ring.
        ring: std::collections::VecDeque<Command>,
        /// Commands it took, in the order it took them.
        taken: Vec<Command>,
        /// How many more it will take before the ring is full.
        room: usize,
        /// Reports the engine has made and the loop has not drained.
        reports: Vec<Event>,
        dropped: DroppedEvents,
        offset: u32,
    }

    impl FakeAudio {
        fn new(engine: Engine) -> Self {
            Self {
                engine,
                ring: std::collections::VecDeque::new(),
                taken: Vec::new(),
                room: usize::MAX,
                reports: Vec::new(),
                dropped: DroppedEvents::default(),
                offset: 0,
            }
        }

        /// One block of audio: the commands first, then the engine, which is where a load
        /// is applied.
        fn callback(&mut self) {
            let mut sink: Vec<Event> = Vec::new();
            while let Some(command) = self.ring.pop_front() {
                self.engine.handle(command, &mut sink);
                self.room = self.room.saturating_add(1);
            }
            let input = [0.0_f32; 128 * 2];
            let mut output = [0.0_f32; 128 * 2];
            self.engine.process(&input, &mut output, &mut sink);
            self.reports.extend(sink);
        }
    }

    impl Audio for FakeAudio {
        fn send(&mut self, command: Command) -> Result<(), Command> {
            if self.room == 0 {
                return Err(command);
            }
            self.room -= 1;
            self.taken.push(command);
            self.ring.push_back(command);
            Ok(())
        }

        fn drain_events(&mut self, mut handler: impl FnMut(Event)) {
            for event in self.reports.drain(..) {
                handler(event);
            }
        }

        fn dropped_events(&self) -> DroppedEvents {
            self.dropped
        }

        fn tick(&mut self, _now: Duration) -> Option<DeviceChange> {
            None
        }

        fn capture_offset_frames(&self) -> u32 {
            self.offset
        }
    }

    /// A directory that removes itself, so a failed test leaves nothing behind.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "free-loop-{}-{}-{name}",
                std::process::id(),
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Everything a pass needs, held together so a test can drive one.
    struct Harness {
        io: FakeAudio,
        surface: free_loop_surface::MockSurface,
        controller: Controller,
        housekeeping: Housekeeping,
        store: SessionStore,
        config: Config,
        running: AtomicBool,
        state: Looping,
        at: Duration,
        _dir: TempDir,
    }

    impl Harness {
        /// A harness with its own session directory, named for the test using it.
        fn new(named: &str) -> Self {
            let config = Config::parse("").unwrap();
            let mut engine = free_loop_engine::EngineConfig::stereo_48k().unwrap();
            engine.segment_pool = 16;
            let (engine, housekeeping) = Engine::new(engine).unwrap();
            let dir = TempDir::new(named);
            Self {
                io: FakeAudio::new(engine),
                surface: free_loop_surface::MockSurface::new(),
                controller: Controller::new(120.0, TimeSignature::FOUR_FOUR, true),
                housekeeping,
                store: SessionStore::new(dir.0.clone()),
                config,
                running: AtomicBool::new(true),
                state: Looping::new(false),
                at: Duration::ZERO,
                _dir: dir,
            }
        }

        fn negotiated() -> free_loop_audio::Negotiated {
            free_loop_audio::Negotiated {
                sample_rate: 48_000,
                channels: 2,
                input_channels: 2,
                capture_channels: 2,
                input_format: free_loop_audio::SampleFormat::F32,
                output_format: free_loop_audio::SampleFormat::F32,
                buffer_frames: None,
                cushion_frames: 0,
                capture_offset: None,
            }
        }

        /// Writes a one-bar session to `addr` and tells the controller it is there.
        fn put_session(&mut self, addr: free_loop_core::SlotAddr) {
            let frames = free_loop_core::Frames(4_800);
            let mut pool = free_loop_clip::SegmentPool::new(4, 2);
            let mut buffer = free_loop_clip::AudioBuffer::new(4, 2);
            let audio = vec![0.25_f32; 4_800 * 2];
            buffer.write(0, &audio, &mut pool);
            let clip = free_loop_clip::Clip::new(buffer, frames, free_loop_core::Frames::ZERO, 2);

            self.store
                .save(
                    addr,
                    &SessionData {
                        tempo: 120.0,
                        beats_per_bar: 4,
                        beat_unit: 4,
                        sample_rate: 48_000,
                        channels: 2,
                        clips: vec![SavedClip {
                            addr: pad(0, 0),
                            playing: true,
                            gain_step: free_loop_core::UNITY_STEP,
                            launch_anchor: None,
                            clip: &clip,
                        }],
                        tracks: [TrackSettings::default(); free_loop_core::TRACK_COUNT],
                    },
                    self.config.load_budget(),
                )
                .unwrap();
            self.controller.set_sessions(self.store.index());
        }

        /// What the performer did, before the next pass picks it up.
        fn press(&mut self, event: SurfaceEvent) {
            self.surface.press(event);
        }

        /// Runs one pass, a tick later than the last.
        fn pass(&mut self) {
            self.at += TICK;
            let mut session = Session {
                io: &mut self.io,
                surface: &mut self.surface,
                controller: &mut self.controller,
                housekeeping: &mut self.housekeeping,
                store: &self.store,
                config: &self.config,
                negotiated: Self::negotiated(),
                running: &self.running,
                showing: None,
            };
            session.pass(&mut self.state, self.at);
        }

        /// Everything the engine has taken, ignoring the settings it starts with.
        fn taken(&self) -> Vec<Command> {
            self.io
                .taken
                .iter()
                .filter(|command| !matches!(command, Command::SetSettings(_)))
                .copied()
                .collect()
        }
    }

    fn dropped(counts: &[(free_loop_core::EventKind, u64)]) -> DroppedEvents {
        let mut array = [0; free_loop_core::EventKind::COUNT];
        for (kind, count) in counts {
            array[kind.index()] = *count;
        }
        DroppedEvents::from(array)
    }

    /// Drives `repaint` the way the control loop does: frame first, then text.
    fn shown(surface: &mut free_loop_surface::MockSurface, controller: &mut Controller) {
        repaint(surface, controller);
    }

    #[test]
    fn the_startup_line_names_the_channels_the_route_ended_up_on() {
        assert_eq!(describe_input(TrackInput::Mono(0)), "input channel 0");
        assert_eq!(describe_input(TrackInput::Mono(5)), "input channel 5");
        assert_eq!(
            describe_input(TrackInput::Pair(0, 1)),
            "input channels 0 and 1"
        );
    }

    /// One turn of the run loop's handover: sends what fits, and returns the request that
    /// surfaced for the caller to act on.
    fn step(queued: &mut Vec<Work>, loading: bool, room: usize) -> (Vec<Command>, Option<Request>) {
        let mut taken = Vec::new();
        let mut left = room;
        let surfaced = hand_over(queued, loading, |command| {
            if left == 0 {
                return false;
            }
            left -= 1;
            taken.push(command);
            true
        });
        (taken, surfaced)
    }

    #[test]
    fn a_gesture_reaches_the_engine_as_a_command() {
        let mut harness = Harness::new("gesture");
        let pad = pad(2, 3);

        harness.press(SurfaceEvent::PadPressed {
            addr: pad,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: pad });
        harness.pass();

        assert_eq!(harness.taken(), vec![Command::Press(pad)]);
    }

    #[test]
    fn a_full_engine_takes_the_rest_on_a_later_pass_in_order() {
        let mut harness = Harness::new("backpressure");
        let pad = pad(0, 0);
        // The settings the controller starts on go out first, and would take the room.
        harness.pass();
        harness.io.taken.clear();

        // Room for one, so the second waits without letting the third overtake it.
        harness.io.room = 1;
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::Rewind,
        ));
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::StopAll,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: pad,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: pad });
        harness.pass();
        assert_eq!(harness.taken(), vec![Command::Rewind], "one fitted");

        harness.io.room = usize::MAX;
        harness.pass();
        assert_eq!(
            harness.taken(),
            vec![Command::Rewind, Command::StopAll, Command::Press(pad)],
            "the rest follow in the order they were made"
        );
    }

    #[test]
    fn a_setting_reaches_the_engine_between_the_gestures_it_sits_between() {
        let mut harness = Harness::new("interleave");
        let loop_pad = pad(0, 0);
        let level = pad(1, 5);
        let volume = u8::try_from(free_loop::paint::VOLUME_SIDE).unwrap();

        // A press, then a level set on the volume screen, then another press.
        harness.press(SurfaceEvent::PadPressed {
            addr: loop_pad,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: loop_pad });
        harness.press(SurfaceEvent::SidePressed { index: volume });
        harness.press(SurfaceEvent::PadPressed {
            addr: level,
            velocity: 100,
        });
        harness.press(SurfaceEvent::SidePressed { index: volume });
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::Rewind,
        ));
        harness.pass();

        let kinds: Vec<&str> = harness
            .io
            .taken
            .iter()
            .map(|command| match command {
                Command::Press(_) => "press",
                Command::SetSettings(_) => "settings",
                Command::Rewind => "rewind",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["settings", "press", "settings", "rewind"],
            "the level lands between the two gestures, not before both"
        );
    }

    #[test]
    fn a_load_reaches_the_engine_before_the_gesture_made_after_it() {
        let mut harness = Harness::new("load-barrier");
        let session = pad(3, 3);
        harness.put_session(session);
        harness.pass();
        harness.io.callback();
        harness.io.taken.clear();

        // One poll: the load is chosen, then the transport is pressed.
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::LoadSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: session,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: session });
        harness.press(SurfaceEvent::SidePressed {
            index: u8::try_from(free_loop::paint::PAUSE_SIDE).unwrap(),
        });
        harness.pass();

        assert!(harness.state.loading, "the load is in its own channel");
        assert!(
            harness.taken().is_empty(),
            "the transport went in front of a load the engine has not applied"
        );

        // The callback applies the load, which is what frees the channel.
        harness.io.callback();
        harness.pass();
        assert!(!harness.state.loading, "the engine has taken it");
        assert_eq!(
            harness.taken(),
            vec![Command::SetPaused(true)],
            "and the transport follows it"
        );
    }

    #[test]
    fn a_loaded_session_puts_its_clip_on_the_grid() {
        let mut harness = Harness::new("load-lands");
        let session = pad(1, 1);
        harness.put_session(session);
        harness.pass();
        harness.io.callback();

        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::LoadSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: session,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: session });
        harness.pass();
        harness.io.callback();

        // The engine says what it now holds, and the controller paints from that.
        harness.pass();
        assert!(
            matches!(
                harness.controller.session().state(pad(0, 0)),
                free_loop_core::SlotState::Playing { .. }
            ),
            "the clip the session held is on its pad"
        );
        assert_eq!(harness.controller.current_session(), Some(session));
    }

    #[test]
    fn a_save_asks_the_engine_for_its_clips_and_writes_what_comes_back() {
        let mut harness = Harness::new("save-round-trip");
        let source = pad(2, 2);
        let target = pad(4, 4);
        harness.put_session(source);
        harness.pass();
        harness.io.callback();

        // Load one, so there is something on the grid worth saving.
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::LoadSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: source,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: source });
        harness.pass();
        harness.io.callback();
        harness.pass();

        // Save it somewhere else. An empty pad asks for no confirmation.
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::SaveSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: target,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: target });
        harness.pass();
        assert!(
            harness
                .taken()
                .iter()
                .any(|command| matches!(command, Command::Snapshot { .. })),
            "the engine was asked for its clips"
        );

        // The engine publishes them, and the next passes write them out.
        harness.io.callback();
        for _ in 0..4 {
            harness.pass();
        }

        assert!(
            harness.store.index().contains(&target),
            "the session was written to the pad that was chosen"
        );
        assert_eq!(harness.controller.current_session(), Some(target));
    }

    #[test]
    fn what_a_load_sets_reaches_the_engine_before_a_gesture_made_after_it() {
        let mut harness = Harness::new("load-settings-order");
        let session = pad(3, 1);
        harness.put_session(session);
        harness.pass();
        harness.io.callback();
        harness.io.taken.clear();

        // One poll: the load is chosen, then a pad is pressed.
        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::LoadSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: session,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: session });
        // The transport, which is the one gesture that acts from the picker as well.
        harness.press(SurfaceEvent::SidePressed {
            index: u8::try_from(free_loop::paint::PAUSE_SIDE).unwrap(),
        });
        harness.pass();
        harness.io.callback();
        harness.pass();

        // The settings the session came with go in front of the gesture that followed it.
        let order: Vec<&str> = harness
            .io
            .taken
            .iter()
            .filter_map(|command| match command {
                Command::SetSettings(_) => Some("settings"),
                Command::SetPaused(_) => Some("paused"),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            vec!["settings", "paused"],
            "the later gesture was applied before the session's own settings"
        );
    }

    #[test]
    fn a_replay_asked_for_during_a_load_waits_for_it() {
        let mut harness = Harness::new("resync-behind-load");
        let session = pad(2, 6);
        harness.put_session(session);
        harness.pass();
        harness.io.callback();
        harness.io.taken.clear();

        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::LoadSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: session,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: session });
        harness.pass();
        assert!(harness.state.loading);

        // A report went missing, which asks the engine to say everything again.
        harness.io.dropped = dropped(&[(free_loop_core::EventKind::SlotChanged, 1)]);
        harness.pass();
        assert!(
            !harness.taken().contains(&Command::Resync),
            "the replay was answered from in front of the load"
        );

        harness.io.callback();
        harness.pass();
        assert!(
            harness.taken().contains(&Command::Resync),
            "and follows once the engine has taken the load"
        );
    }

    #[test]
    fn a_save_the_engine_never_answers_gives_up_rather_than_waiting() {
        let mut harness = Harness::new("save-timeout");
        let source = pad(2, 2);
        let target = pad(4, 4);
        harness.put_session(source);
        harness.pass();
        harness.io.callback();

        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::LoadSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: source,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: source });
        harness.pass();
        harness.io.callback();
        harness.pass();

        harness.press(SurfaceEvent::ControlPressed(
            free_loop_surface::Control::SaveSession,
        ));
        harness.press(SurfaceEvent::PadPressed {
            addr: target,
            velocity: 100,
        });
        harness.press(SurfaceEvent::PadReleased { addr: target });
        harness.pass();
        assert!(harness.state.pending_save.is_some(), "the save is waiting");

        // No callback: the engine takes the snapshot request and never publishes.
        harness.pass();
        assert!(
            harness.state.pending_save.is_some(),
            "still inside its wait"
        );

        harness.at += SAVE_TIMEOUT;
        harness.pass();
        assert!(harness.state.pending_save.is_none(), "the wait ended");
        assert!(
            !harness.store.index().contains(&target),
            "and wrote nothing"
        );

        let frame = harness.surface.frames().last().unwrap();
        assert!(
            free_loop_core::SlotAddr::all()
                .all(|addr| frame.pad(addr).color == free_loop_surface::LedColor::Red),
            "the grid says so"
        );
    }

    #[test]
    fn a_report_from_the_engine_reaches_the_controller() {
        let mut harness = Harness::new("report");
        harness.io.reports.push(Event::Tempo { bpm: 90.0 });

        harness.pass();
        assert_eq!(harness.controller.tempo(), 90.0);
    }

    #[test]
    fn a_window_is_told_the_screen_only_when_it_changes() {
        let slot = Mutex::new(Mode::Perform);

        let published = publish_screen(Some(&slot), Mode::Mute, None);
        assert_eq!(published, Some(Mode::Mute));
        assert_eq!(*slot.lock().unwrap(), Mode::Mute);

        // Held by the window this pass, so nothing is known to have got through.
        let held = slot.lock().unwrap();
        assert_eq!(
            publish_screen(Some(&slot), Mode::Solo, published),
            published,
            "the next pass tries again"
        );
        drop(held);

        assert_eq!(
            publish_screen(Some(&slot), Mode::Solo, published),
            Some(Mode::Solo)
        );
        assert_eq!(*slot.lock().unwrap(), Mode::Solo);
    }

    #[test]
    fn a_run_with_no_window_still_tracks_the_screen() {
        // Nothing to publish to, so it only remembers, and never asks again for the same.
        assert_eq!(publish_screen(None, Mode::Mute, None), Some(Mode::Mute));
        assert_eq!(
            publish_screen(None, Mode::Mute, Some(Mode::Mute)),
            Some(Mode::Mute)
        );
    }

    #[test]
    fn a_load_waits_for_the_snapshot_of_the_save_before_it() {
        let pad = pad(1, 2);
        let mut queued = vec![
            Work::Request(Request::SaveSession(pad)),
            Work::Request(Request::LoadSession(pad)),
        ];

        // The save surfaces first, and its snapshot takes the place the request had.
        let (_, surfaced) = step(&mut queued, false, 8);
        assert_eq!(surfaced, Some(Request::SaveSession(pad)));
        queued.insert(0, Work::Command(Command::Snapshot { request: 1 }));

        // The load cannot start until that snapshot has gone: the audio side applies a
        // load after the commands it has taken, so it would commit first.
        let (taken, surfaced) = step(&mut queued, false, 0);
        assert!(taken.is_empty(), "no room for the snapshot yet");
        assert_eq!(surfaced, None, "so the load waits behind it");
        assert_eq!(queued.len(), 2, "and neither is lost");

        let (taken, surfaced) = step(&mut queued, false, 8);
        assert_eq!(taken, vec![Command::Snapshot { request: 1 }]);
        assert_eq!(surfaced, Some(Request::LoadSession(pad)));
    }

    #[test]
    fn a_save_waits_for_the_load_in_front_of_it_to_commit() {
        let pad = pad(1, 2);
        let mut queued = vec![
            Work::Request(Request::SaveSession(pad)),
            Work::Command(Command::Press(pad)),
        ];

        // With a load in its channel, nothing behind it moves at all.
        let (taken, surfaced) = step(&mut queued, true, 8);
        assert!(taken.is_empty() && surfaced.is_none());
        assert_eq!(queued.len(), 2);

        let (_, surfaced) = step(&mut queued, false, 8);
        assert_eq!(
            surfaced,
            Some(Request::SaveSession(pad)),
            "once the engine has taken the load"
        );
    }

    #[test]
    fn a_lost_signature_report_asks_the_engine_again() {
        // Both directions: the value and the refusal that takes an optimistic copy back.
        for kind in [
            free_loop_core::EventKind::TimeSignature,
            free_loop_core::EventKind::TimeSignatureRejected,
        ] {
            let lost = dropped(&[(kind, 1)]);
            assert!(
                needs_resync(&lost, &DroppedEvents::default()),
                "{kind:?} should be asked for again"
            );
        }
    }

    #[test]
    fn a_lost_beat_is_not_worth_asking_about() {
        let lost = dropped(&[(free_loop_core::EventKind::Beat, 4)]);
        assert!(!needs_resync(&lost, &DroppedEvents::default()));
    }

    #[test]
    fn a_size_refusal_holds_the_red_before_it_scrolls() {
        let mut surface = free_loop_surface::MockSurface::new();
        let mut controller = Controller::new(120.0, TimeSignature::FOUR_FOUR, true);
        controller.load_failed(Duration::ZERO, Some("2600".to_owned()));

        shown(&mut surface, &mut controller);
        let frame = surface.frames().last().unwrap();
        assert!(
            free_loop_core::SlotAddr::all().all(|a| frame.pad(a).is_lit()),
            "the answer is on the grid"
        );
        assert!(surface.texts().is_empty(), "with nothing scrolling over it");

        controller.tick(free_loop::control::RESULT_FLASH);
        shown(&mut surface, &mut controller);
        assert_eq!(surface.texts(), [Some("2600".to_owned())]);
    }

    #[test]
    fn a_result_cuts_a_scroll_before_taking_the_grid() {
        let mut surface = free_loop_surface::MockSurface::new();
        let mut controller = Controller::new(120.0, TimeSignature::FOUR_FOUR, true);

        controller.on_surface(
            SurfaceEvent::ControlPressed(free_loop_surface::Control::TempoUp),
            Duration::ZERO,
        );
        controller.on_surface(
            SurfaceEvent::ControlReleased(free_loop_surface::Control::TempoUp),
            Duration::ZERO,
        );
        shown(&mut surface, &mut controller);
        assert_eq!(surface.texts().len(), 1, "the bpm is scrolling");

        controller.session_saved(pad(0, 0), Duration::ZERO);
        shown(&mut surface, &mut controller);
        assert_eq!(surface.texts().last(), Some(&None), "the scroll is stopped");

        shown(&mut surface, &mut controller);
        let frame = surface.frames().last().unwrap();
        assert!(
            free_loop_core::SlotAddr::all().all(|a| frame.pad(a).is_lit()),
            "and the answer reaches the grid"
        );
    }

    #[test]
    fn a_session_too_large_reports_the_number_to_raise_the_pool_to() {
        let error: Box<dyn Error> = Box::new(free_loop_session::SessionError::TooLarge {
            allowed: 2_048,
            wanted: 2_600,
        });
        assert_eq!(too_large(error.as_ref()), Some("2600".to_owned()));
    }

    #[test]
    fn another_refusal_has_no_number_worth_scrolling() {
        let error: Box<dyn Error> = Box::new(free_loop_session::SessionError::Mismatch {
            what: "Hz",
            wanted: 48_000,
            found: 44_100,
        });
        assert_eq!(too_large(error.as_ref()), None);
    }

    #[test]
    fn the_device_gets_the_pulses_it_has_not_had() {
        let mut surface = free_loop_surface::MockSurface::new();

        assert_eq!(forward_clock(&mut surface, Some(24), 0), 24);
        assert_eq!(surface.clock(), 24);

        assert_eq!(forward_clock(&mut surface, Some(30), 24), 30, "the delta");
        assert_eq!(surface.clock(), 30);
    }

    #[test]
    fn a_dropped_clock_report_still_delivers_its_pulses() {
        let mut surface = free_loop_surface::MockSurface::new();

        // The report carrying 24 never arrived; the next one carries both.
        assert_eq!(forward_clock(&mut surface, Some(48), 0), 48);
        assert_eq!(surface.clock(), 48);
    }

    #[test]
    fn a_pass_with_no_clock_report_sends_nothing() {
        let mut surface = free_loop_surface::MockSurface::new();

        assert_eq!(forward_clock(&mut surface, None, 17), 17);
        assert_eq!(surface.clock(), 0);
    }

    #[test]
    fn a_lost_beat_does_not_cost_a_replay() {
        use free_loop_core::EventKind;

        let seen = DroppedEvents::default();
        let now = dropped(&[(EventKind::Beat, 4), (EventKind::Clock, 2)]);

        assert!(!needs_resync(&now, &seen), "both correct themselves");
        assert_eq!(lost_report(&now, &seen), "2 clock ticks, 4 beats");
    }

    #[test]
    fn a_lost_slot_change_asks_for_a_replay() {
        use free_loop_core::EventKind;

        let seen = dropped(&[(EventKind::Beat, 4)]);
        let now = dropped(&[(EventKind::Beat, 9), (EventKind::SlotChanged, 1)]);

        assert!(needs_resync(&now, &seen));
        assert_eq!(lost_report(&now, &seen), "1 slot change, 5 beats");
    }

    #[test]
    fn a_lost_tempo_refusal_asks_for_a_replay() {
        use free_loop_core::EventKind;

        let now = dropped(&[(EventKind::TempoRejected, 1)]);
        assert!(
            needs_resync(&now, &DroppedEvents::default()),
            "a resync republishes the tempo the engine is actually at"
        );
    }

    fn pad(track: u8, slot: u8) -> free_loop_core::SlotAddr {
        free_loop_core::SlotAddr::new(
            free_loop_core::TrackId::new(track).unwrap(),
            free_loop_core::SlotId::new(slot).unwrap(),
        )
    }

    fn waiting(request: u32) -> PendingSave {
        PendingSave {
            deadline: DEADLINE,
            request,
            addr: pad(1, 2),
            settings: SaveSettings {
                tempo: 120.0,
                time_signature: TimeSignature::FOUR_FOUR,
                tracks: [TrackSettings::default(); free_loop_core::TRACK_COUNT],
            },
        }
    }

    #[test]
    fn a_save_with_no_answer_yet_keeps_waiting() {
        let mut pending = Some(waiting(1));
        let outcome = resolve_save(&mut pending, None, Duration::from_secs(1));

        assert!(matches!(outcome, SaveOutcome::Waiting));
        assert!(pending.is_some(), "still expecting its answer");
    }

    #[test]
    fn an_answer_to_this_save_finishes_it() {
        let mut pending = Some(waiting(7));
        let outcome = resolve_save(&mut pending, Some((7, 3, 3)), Duration::from_secs(1));

        let answered = match outcome {
            SaveOutcome::Answered(answered) => answered,
            other => unreachable!("expected an answer, got {other:?}"),
        };
        assert_eq!(answered.addr, pad(1, 2));
        assert_eq!((answered.clips, answered.expected), (3, 3));
        assert!(pending.is_none(), "no longer pending");
    }

    #[test]
    fn an_answer_to_a_superseded_save_is_not_this_one() {
        let mut pending = Some(waiting(9));
        let outcome = resolve_save(&mut pending, Some((8, 3, 3)), Duration::from_secs(1));

        assert!(matches!(outcome, SaveOutcome::Waiting));
        assert!(pending.is_some(), "still expecting request nine");
    }

    #[test]
    fn a_save_whose_answer_never_came_expires() {
        let mut pending = Some(waiting(1));
        let outcome = resolve_save(&mut pending, None, DEADLINE);

        assert!(matches!(outcome, SaveOutcome::Expired(addr) if addr == pad(1, 2)));
        assert!(pending.is_none(), "given up on");
    }

    /// The answer cannot be asked for again, so it has to win.
    #[test]
    fn an_answer_arriving_on_the_deadline_still_wins() {
        let mut pending = Some(waiting(4));
        let outcome = resolve_save(&mut pending, Some((4, 2, 2)), DEADLINE);

        assert!(
            matches!(outcome, SaveOutcome::Answered(_)),
            "written, not abandoned"
        );
    }
    #[test]
    fn the_window_log_keeps_what_was_reported() {
        let console = Console::new();
        let subscriber = subscriber(false, Some(console.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("a line for the window");
            tracing::debug!("below the level asked for");
        });

        let lines = console.lines().join("\n");
        assert!(
            lines.contains("a line for the window"),
            "the console had {lines:?}"
        );
        assert!(
            !lines.contains("below the level asked for"),
            "the filter reaches the console too"
        );
    }

    #[test]
    fn a_run_without_a_window_has_nowhere_to_log_but_stderr() {
        // Building it is the assertion: `None` has to satisfy the same layer types.
        let _ = subscriber(true, None);
    }
}
