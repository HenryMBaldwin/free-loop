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
use free_loop_audio::{AudioIo, DeviceChange, Negotiated, open};
use free_loop_core::{Command, Event};
use free_loop_engine::{Engine, Housekeeping, LoadMessage, Loader, Snapshot};
use free_loop_session::{SavedClip, SessionData, SessionStore};
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
        Some(channel) => println!("capturing device input channel {channel} onto every side"),
        None => println!("capturing device input channels in order"),
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
    controller.set_sessions(store.index());

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
    let mut pending_save: Option<free_loop_core::SlotAddr> = None;
    let mut clipping = ClipReport::default();
    let started = Instant::now();
    // Only known once the driver has run a callback and said how much it buffers.
    let mut reported_latency = false;
    let mut connected = surface.is_connected();

    while running.load(Ordering::Relaxed) {
        let now = started.elapsed();

        connected = watch_surface(surface, now, connected);
        watch_devices(io, now);

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
                    pending_save = Some(addr);
                    snapshots.clear();
                    if io.send(Command::Snapshot).is_err() {
                        eprintln!("could not ask for a snapshot");
                        pending_save = None;
                    }
                }
                Request::LoadSession(addr) => {
                    load_session(
                        store,
                        addr,
                        &mut housekeeping.loader,
                        &negotiated,
                        controller,
                    );
                }
            }
        }

        // Storage the engine has finished with comes back here to be dropped.
        drop(housekeeping.recycler.take_borrowed().collect::<Vec<_>>());

        for command in controller.drain_commands() {
            if io.send(command).is_err() {
                eprintln!("audio thread is not keeping up; dropped {command:?}");
            }
        }

        let mut snapshot_ready = false;
        let mut clock_ticks = 0;
        let mut clipped = 0_u32;
        io.drain_events(|event| {
            match event {
                Event::SnapshotComplete { .. } => snapshot_ready = true,
                Event::Clock { ticks } => clock_ticks += ticks,
                Event::Clipped { samples } => clipped += samples,
                _ => {}
            }
            report(event);
            controller.on_engine(event);
        });
        clipping.note(clipped, now);

        // Keeps the device's flash and pulse animations on the transport's tempo.
        if clock_ticks > 0
            && let Err(error) = surface.send_clock(clock_ticks)
        {
            eprintln!("surface: {error}");
        }
        housekeeping
            .snapshots
            .drain(|snapshot| snapshots.push(snapshot));

        if snapshot_ready && let Some(addr) = pending_save.take() {
            write_session(store, addr, config, &negotiated, &snapshots, controller);
            snapshots.clear();
        }

        repaint(surface, controller);

        reported_latency |= report_latency(io, &negotiated, reported_latency);

        std::thread::sleep(TICK);
    }
}

/// How long to gather clipping before saying anything.
const CLIP_REPORT_EVERY: Duration = Duration::from_secs(2);

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
) {
    match load(store, addr, loader, negotiated) {
        Ok(gains) => {
            println!("loaded session {}{}", addr.track.index(), addr.slot.index());
            controller.set_gains(gains);
            controller.session_loaded(addr, true);
        }
        Err(error) => {
            eprintln!("load failed: {error}");
            controller.cancel_picker();
        }
    }
}

/// Writes the snapshotted clips out under `addr`.
fn save(
    store: &SessionStore,
    addr: free_loop_core::SlotAddr,
    config: &Config,
    negotiated: &free_loop_audio::Negotiated,
    snapshots: &[Snapshot],
    gains: [u8; free_loop_core::TRACK_COUNT],
) -> Result<(), free_loop_session::SessionError> {
    let clips = snapshots
        .iter()
        .map(|snapshot| SavedClip {
            addr: snapshot.addr,
            gain_step: gains[snapshot.addr.track.index()],
            playing: matches!(
                snapshot.state,
                free_loop_core::SlotState::Playing { .. }
                    | free_loop_core::SlotState::QueuedStop { .. }
            ),
            clip: &snapshot.clip,
        })
        .collect();

    store.save(
        addr,
        &SessionData {
            tempo: config.transport.tempo,
            beats_per_bar: config.transport.beats_per_bar,
            beat_unit: config.transport.beat_unit,
            sample_rate: negotiated.sample_rate,
            channels: u16::try_from(negotiated.channels).unwrap_or(2),
            clips,
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
) -> Result<[u8; free_loop_core::TRACK_COUNT], Box<dyn Error>> {
    let channels = u16::try_from(negotiated.channels).unwrap_or(2);
    let session = store.load(addr, negotiated.sample_rate, channels)?;
    let tempo = free_loop_core::Tempo::new(session.manifest.tempo)?;
    let gains = session.gains();

    if !loader.ready() {
        return Err("the audio thread has not drained the load queue".into());
    }
    loader.send(LoadMessage::Begin { tempo })?;
    for loaded in session.clips {
        loader.send(LoadMessage::Clip {
            addr: loaded.addr,
            clip: std::sync::Arc::new(loaded.clip),
            playing: loaded.playing,
        })?;
    }
    loader.send(LoadMessage::End)?;
    Ok(gains)
}

/// Writes a snapshot to a pad and tells the controller it landed.
fn write_session(
    store: &SessionStore,
    addr: free_loop_core::SlotAddr,
    config: &Config,
    negotiated: &Negotiated,
    snapshots: &[Snapshot],
    controller: &mut Controller,
) {
    match save(
        store,
        addr,
        config,
        negotiated,
        snapshots,
        controller.gains(),
    ) {
        Ok(()) => {
            println!("saved session {}{}", addr.track.index(), addr.slot.index());
            controller.session_saved(addr);
        }
        Err(error) => eprintln!("save failed: {error}"),
    }
}

/// Lets the audio devices come back after being unplugged, reporting what changed.
fn watch_devices(io: &mut AudioIo, now: Duration) {
    match io.tick(now) {
        Some(DeviceChange::Lost) => {
            eprintln!("audio: device gone. the set is held where it stopped");
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
        Event::Xrun { frames } => eprintln!("xrun: {frames} frames"),
        Event::RecordBufferLow { addr } => {
            eprintln!(
                "out of recording space on track {} slot {}",
                addr.track.index(),
                addr.slot.index()
            );
        }
        Event::TempoRejected => eprintln!("tempo is locked while clips exist"),
        // Clipping reports per block, too often to print. `ClipReport` throttles it.
        Event::Clipped { .. }
        | Event::SnapshotComplete { .. }
        | Event::Clock { .. }
        | Event::Bar { .. }
        | Event::Beat { .. }
        | Event::SlotChanged { .. }
        | Event::ClipReleased { .. } => {}
    }
}
