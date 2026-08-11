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
use std::sync::Arc;
use std::time::{Duration, Instant};

use free_loop::config::{self, Config};
use free_loop::control::{Controller, Request, TextUpdate};
use free_loop_audio::{AudioIo, DeviceChange, DroppedEvents, Negotiated, open};
use free_loop_core::{Command, Event};
use free_loop_engine::{Engine, Housekeeping, LoadMessage, Loader, Snapshot};
use free_loop_session::{SavedClip, SessionData, SessionStore, TrackSettings};
use free_loop_surface::{ControlSurface, LaunchpadX, Reconnecting, SurfaceEvent};

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

fn main() -> Result<(), Box<dyn Error>> {
    let Some(Args { path, log_surface }) = parse_args()? else {
        return Ok(());
    };

    let config = Config::load(&path)?;
    // A missing config falls back to the default devices, which sounds like broken
    // recording rather than like a missing file unless it says so here.
    if path.exists() {
        println!("config: {}", path.display());
    } else {
        println!("config: {} not found, using defaults", path.display());
    }

    let opened = open(&config.audio())?;
    let negotiated = opened.negotiated();
    println!("input:  {}", opened.input_name());
    println!("output: {}", opened.output_name());
    println!(
        "audio: {} Hz, {} channels in / {} out, {} frames of cushion",
        negotiated.sample_rate,
        negotiated.input_channels,
        negotiated.channels,
        negotiated.cushion_frames
    );
    match config.audio.input_channel {
        Some(channel) => println!("tracks start on input channel {channel}"),
        None => println!("tracks start on the whole input"),
    }

    let (engine, mut housekeeping) =
        Engine::new(config.engine(negotiated.sample_rate, negotiated.channels)?)?;
    let mut io = opened.start(engine)?;

    let mut surface = connect_surface();
    let mut controller = Controller::new(
        config.transport.tempo,
        config.transport.beats_per_bar,
        config.click.enabled,
    );

    let store = SessionStore::new(path.parent().unwrap_or(Path::new(".")).join("sessions"));
    // A save interrupted between its two renames leaves a session under another name.
    for trouble in store.recover() {
        eprintln!("sessions: {trouble}");
    }
    controller.set_sessions(store.index());
    controller.set_input_count(negotiated.channels);
    controller.set_default_input(config.track_input());
    controller.set_default_launch_mode(config.launch_mode());
    controller.set_inputs([config.track_input(); free_loop_core::TRACK_COUNT]);
    controller.set_launch_modes([config.launch_mode(); free_loop_core::TRACK_COUNT]);

    println!(
        "transport: {:.1} bpm, {}/{}",
        controller.tempo(),
        config.transport.beats_per_bar,
        config.transport.beat_unit
    );
    println!("running. ctrl-c to stop.");

    let running = Arc::new(AtomicBool::new(true));
    ctrlc::set_handler({
        let running = Arc::clone(&running);
        move || running.store(false, Ordering::Relaxed)
    })?;

    run(Session {
        io: &mut io,
        surface: surface.as_mut(),
        controller: &mut controller,
        housekeeping: &mut housekeeping,
        store: &store,
        config: &config,
        negotiated,
        log_surface,
        running: &running,
    });

    // Leaving the grid lit after the process is gone looks like it is still running.
    if let Err(error) = surface.clear() {
        eprintln!("surface: {error}");
    }
    println!("\nstopped. device errors: {}", io.device_errors());
    Ok(())
}

/// Everything the control loop touches.
struct Session<'a> {
    io: &'a mut AudioIo,
    surface: &'a mut dyn ControlSurface,
    controller: &'a mut Controller,
    housekeeping: &'a mut Housekeeping,
    store: &'a SessionStore,
    config: &'a Config,
    negotiated: Negotiated,
    log_surface: bool,
    running: &'a AtomicBool,
}

/// Polls the surface, drives the engine and repaints until asked to stop.
fn run(s: Session<'_>) {
    let Session {
        io,
        surface,
        controller,
        housekeeping,
        store,
        config,
        negotiated,
        log_surface,
        running,
    } = s;

    let mut events: Vec<SurfaceEvent> = Vec::new();
    let mut snapshots: Vec<Snapshot> = Vec::new();
    // A save waits on the answer to one request; anything tagged otherwise is stale.
    let mut pending_save: Option<PendingSave> = None;
    let mut next_request = 0_u32;
    let mut clipping = ClipReport::default();
    let mut xruns = XrunReport::default();
    let started = Instant::now();
    // Only known once the driver has run a callback and said how much it buffers.
    let mut reported_latency = false;
    let mut connected = surface.is_connected();
    // Reports that never arrived leave the grid showing what a pad used to be doing.
    let mut missed_reports = DroppedEvents::default();
    // Clock ticks the device has had, against the running total the engine reports.
    let mut clock_sent = 0_u64;

    while running.load(Ordering::Relaxed) {
        let now = started.elapsed();

        connected = watch_surface(surface, now, connected);
        watch_devices(io, now, controller, config.audio.pause_on_disconnect);

        events.clear();
        surface.poll(&mut events);
        for event in events.drain(..) {
            if log_surface {
                println!("surface: {event:?}");
            }
            controller.on_surface(event, now);
        }
        controller.tick(now);
        // Returns clips the engine finished with while something else was reading them.
        housekeeping.recycler.run();

        // Collected first: acting on a request touches the controller again.
        let requests: Vec<Request> = controller.drain_requests().collect();
        for request in requests {
            match request {
                Request::SaveSession(addr) => {
                    next_request = next_request.wrapping_add(1);
                    snapshots.clear();
                    pending_save = ask_for_snapshot(io, next_request, addr, controller, now);
                }
                Request::LoadSession(addr) => {
                    load_session(
                        store,
                        addr,
                        &mut housekeeping.loader,
                        &negotiated,
                        controller,
                        config,
                    );
                }
            }
        }

        // Storage the engine has finished with comes back here to be dropped.
        housekeeping.recycler.take_borrowed().for_each(drop);

        // Ahead of the commands, and never refused.
        if let Some(settings) = controller.take_settings() {
            io.publish_settings(settings);
        }
        for command in controller.drain_commands() {
            if io.send(command).is_err() {
                eprintln!("audio thread is not keeping up; dropped {command:?}");
            }
        }

        let drained = drain_engine(io, controller);
        let Drained {
            answered,
            clock_total,
            clipped,
            short_frames,
        } = drained;
        // After draining, so the replay it asks for has somewhere to go.
        missed_reports = resync_after_loss(io, missed_reports);
        clipping.note(clipped, now);
        xruns.note(short_frames, now);

        // Keeps the device's flash and pulse animations on the transport's tempo.
        let ticks = clock_total.map_or(0, |total| {
            let ticks = total.saturating_sub(clock_sent);
            clock_sent = total;
            u32::try_from(ticks).unwrap_or(u32::MAX)
        });
        if ticks > 0
            && let Err(error) = surface.send_clock(ticks)
        {
            eprintln!("surface: {error}");
        }
        collect_snapshots(
            &mut housekeeping.snapshots,
            pending_save.as_ref(),
            &mut snapshots,
        );
        let outcome = resolve_save(&mut pending_save, answered, now);
        if carry_out_save(outcome, store, config, &negotiated, &snapshots, controller) {
            snapshots.clear();
        }

        repaint(surface, controller);

        reported_latency |= report_latency(io, &negotiated, reported_latency);

        std::thread::sleep(TICK);
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

        eprintln!("capture came up short by {} frames", self.frames);
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

        eprintln!(
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
fn report_latency(io: &AudioIo, negotiated: &Negotiated, already: bool) -> bool {
    if already {
        return true;
    }
    let frames = io.capture_offset_frames();
    if frames == 0 {
        return false;
    }
    let millis = f64::from(frames) / f64::from(negotiated.sample_rate) * 1000.0;
    println!("round trip: {frames} frames ({millis:.1} ms), compensated");
    true
}

/// Reads a session in and tells the controller what came with it.
fn load_session(
    store: &SessionStore,
    addr: free_loop_core::SlotAddr,
    loader: &mut Loader,
    negotiated: &Negotiated,
    controller: &mut Controller,
    config: &Config,
) {
    match load(store, addr, loader, negotiated, config) {
        Ok(restored) => {
            println!("loaded session {}{}", addr.track.index(), addr.slot.index());
            controller.set_gains(restored.gains);
            controller.set_loaded_tempo(restored.tempo);
            controller.set_inputs(core::array::from_fn(|track| {
                free_loop_core::TrackInput::from_column(restored.tracks[track].input)
            }));
            controller.set_launch_modes(core::array::from_fn(|track| {
                if restored.tracks[track].restart {
                    free_loop_core::LaunchMode::Restart
                } else {
                    free_loop_core::LaunchMode::Follow
                }
            }));
            controller.session_loaded(addr, true);
        }
        Err(error) => {
            eprintln!(
                "load of session {}{} failed: {error}",
                addr.track.index(),
                addr.slot.index()
            );
            controller.cancel_picker();
        }
    }
}

/// Asks the engine to publish its clips for a save.
fn ask_for_snapshot(
    io: &mut AudioIo,
    request: u32,
    addr: free_loop_core::SlotAddr,
    controller: &Controller,
    now: Duration,
) -> Option<PendingSave> {
    if io.send(Command::Snapshot { request }).is_err() {
        eprintln!("could not ask for a snapshot");
        return None;
    }
    Some(PendingSave {
        deadline: now + SAVE_TIMEOUT,
        request,
        addr,
        settings: settings(controller),
    })
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
) -> bool {
    match outcome {
        SaveOutcome::Waiting => false,
        SaveOutcome::Answered(save) => {
            settle_save(store, &save, config, negotiated, snapshots, controller);
            true
        }
        SaveOutcome::Expired(addr) => {
            eprintln!(
                "save to {}{} never completed; nothing was written",
                addr.track.index(),
                addr.slot.index()
            );
            controller.cancel_picker();
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
    settings: Settings,
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
) {
    if save.clips != save.expected {
        eprintln!(
            "save abandoned: {} of {} pads arrived",
            save.clips, save.expected
        );
        controller.cancel_picker();
        return;
    }
    write_session(store, save, config, negotiated, snapshots, controller);
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
    settings: Settings,
}

/// What a save records besides the audio.
#[derive(Debug)]
struct Settings {
    /// What the performer has the transport set to, not what the config file says.
    tempo: f64,
    gains: [u8; free_loop_core::TRACK_COUNT],
    tracks: [TrackSettings; free_loop_core::TRACK_COUNT],
}

/// What the controller currently has set.
fn settings(controller: &Controller) -> Settings {
    let inputs = controller.inputs();
    let modes = controller.launch_modes();
    Settings {
        tempo: controller.tempo(),
        gains: controller.gains(),
        tracks: core::array::from_fn(|track| TrackSettings {
            input: inputs[track].column(),
            restart: modes[track].restarts(),
            gain_step: controller.gains()[track],
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
    settings: &Settings,
) -> Result<(), free_loop_session::SessionError> {
    let clips = snapshots
        .iter()
        .map(|snapshot| SavedClip {
            addr: snapshot.addr,
            gain_step: settings.gains[snapshot.addr.track.index()],
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
            beats_per_bar: config.transport.beats_per_bar,
            beat_unit: config.transport.beat_unit,
            sample_rate: negotiated.sample_rate,
            channels: u16::try_from(negotiated.channels).unwrap_or(2),
            clips,
            tracks: settings.tracks,
        },
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
        eprintln!("surface: {error}");
    }

    if let Some(update) = controller.take_text() {
        let shown = match update {
            TextUpdate::Show(text) => surface.show_text(&text),
            TextUpdate::Stop => surface.stop_text(),
        };
        if let Err(error) = shown {
            eprintln!("surface: {error}");
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

    let wanted = config.time_signature()?;
    let manifest = checked.manifest();
    if manifest.beats_per_bar != wanted.beats_per_bar() || manifest.beat_unit != wanted.beat_unit()
    {
        return Err(format!(
            "session is in {}/{} but the transport is {}/{}",
            manifest.beats_per_bar,
            manifest.beat_unit,
            wanted.beats_per_bar(),
            wanted.beat_unit()
        )
        .into());
    }
    let tempo = free_loop_core::Tempo::new(manifest.tempo)?;

    let session = checked.materialise()?;
    let restored = Restored {
        gains: session.gains(),
        tracks: session.tracks(),
        tempo: session.manifest.tempo,
    };

    if !loader.ready() {
        return Err("the audio thread has not drained the load queue".into());
    }
    loader.send(LoadMessage::Begin { tempo })?;
    for loaded in session.clips {
        loader.send(LoadMessage::Clip {
            addr: loaded.addr,
            clip: std::sync::Arc::new(loaded.clip),
            playing: loaded.playing,
            launch_anchor: loaded.launch_anchor,
        })?;
    }
    loader.send(LoadMessage::End)?;
    Ok(restored)
}

/// What a loaded session sets besides its audio.
struct Restored {
    gains: [u8; free_loop_core::TRACK_COUNT],
    tracks: [TrackSettings; free_loop_core::TRACK_COUNT],
    /// What the session was recorded at, which the engine has already taken.
    tempo: f64,
}

/// Writes a snapshot to a pad and tells the controller it landed.
fn write_session(
    store: &SessionStore,
    save_to: &Answered,
    config: &Config,
    negotiated: &Negotiated,
    snapshots: &[Snapshot],
    controller: &mut Controller,
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
            println!("saved session {}{}", addr.track.index(), addr.slot.index());
            controller.session_saved(addr);
        }
        Err(error) => eprintln!("save failed: {error}"),
    }
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
fn drain_engine(io: &mut AudioIo, controller: &mut Controller) -> Drained {
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
        controller.on_engine(event);
    });
    drained
}

/// Asks the engine to report every pad again if a report a resync repairs was lost.
///
/// The controller paints from a mirror kept in step by those reports. Kinds a resync
/// cannot repair are reported and otherwise left.
fn resync_after_loss(io: &mut AudioIo, seen: DroppedEvents) -> DroppedEvents {
    let dropped = io.dropped_events();
    if dropped == seen {
        return seen;
    }
    eprintln!("missed engine reports: {}", lost_report(&dropped, &seen));

    if !needs_resync(&dropped, &seen) {
        return dropped;
    }
    if io.send(Command::Resync).is_err() {
        // The queue is full as well, so leave the count alone and try again next pass.
        return seen;
    }
    dropped
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
fn watch_devices(
    io: &mut AudioIo,
    now: Duration,
    controller: &mut Controller,
    pause_on_disconnect: bool,
) {
    match io.tick(now) {
        Some(DeviceChange::Lost(loss)) => {
            if pause_on_disconnect {
                controller.pause();
                eprintln!("audio: device gone ({loss}). paused");
            } else {
                eprintln!("audio: device gone ({loss}). held where it stopped");
            }
        }
        Some(DeviceChange::Back) => println!("audio: device back"),
        Some(DeviceChange::Refused(error)) => eprintln!("audio: {error}"),
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
            println!("surface: Launchpad X back");
        } else {
            eprintln!("surface: gone, still looking");
        }
    }
    attached
}

/// Connects a Launchpad, and keeps looking for one as long as the process runs.
///
/// A missing pad is not fatal: the click and the audio path still work.
fn connect_surface() -> Box<dyn ControlSurface> {
    let surface = Reconnecting::new(LaunchpadX::connect);
    if surface.is_connected() {
        println!("surface: Launchpad X");
    } else {
        println!(
            "surface: no port containing \"{}\", still looking",
            LaunchpadX::PORT_KEYWORD
        );
        let ports = free_loop_surface::output_ports();
        if ports.is_empty() {
            println!("surface: the host lists no midi outputs at all");
        } else {
            println!("surface: midi outputs seen: {}", ports.join(", "));
        }
    }
    Box::new(surface)
}

/// Prints what is worth knowing. Bars, beats and slot changes are on the grid already.
fn report(event: Event) {
    match event {
        Event::ClipRecorded { addr, len, .. } => {
            println!(
                "recorded track {} slot {}: {} frames",
                addr.track.index(),
                addr.slot.index(),
                len.0
            );
        }

        Event::RecordingRefused { addr } => {
            eprintln!(
                "no recording space free for track {} slot {}; the pad is left empty",
                addr.track.index(),
                addr.slot.index()
            );
        }
        Event::RecordBufferLow { addr } => {
            eprintln!(
                "out of recording space on track {} slot {}",
                addr.track.index(),
                addr.slot.index()
            );
        }
        Event::TempoRejected => eprintln!("tempo is locked while clips exist"),
        // Clipping and short capture report per block, too often to print. `ClipReport`
        // and `XrunReport` throttle them. The tempo reports on every nudge, which a held
        // button repeats eight times a second; the controller wants it, a reader does not.
        Event::Tempo { .. }
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
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;

    const DEADLINE: Duration = Duration::from_secs(2);

    fn dropped(counts: &[(free_loop_core::EventKind, u64)]) -> DroppedEvents {
        let mut array = [0; free_loop_core::EventKind::COUNT];
        for (kind, count) in counts {
            array[kind.index()] = *count;
        }
        DroppedEvents::from(array)
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
            settings: Settings {
                tempo: 120.0,
                gains: [free_loop_core::UNITY_STEP; free_loop_core::TRACK_COUNT],
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
}
