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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use free_loop::{Config, Controller, config};
use free_loop_audio::open;
use free_loop_core::Event;
use free_loop_engine::Engine;
use free_loop_surface::{ControlSurface, LaunchpadX, MockSurface, SurfaceError, SurfaceEvent};

/// How often the control loop runs.
///
/// Fast enough that a press never feels late against the bar it is quantised to, slow
/// enough to leave the machine alone.
const TICK: Duration = Duration::from_millis(2);

/// Where the config lives unless told otherwise.
const DEFAULT_CONFIG: &str = "free-loop.toml";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from(DEFAULT_CONFIG);
    let mut log_surface = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--log-surface" => log_surface = true,
            "--print-config" => {
                print!("{}", config::EXAMPLE);
                return Ok(());
            }
            "--help" | "-h" => {
                println!("free-loop [config path]");
                println!("free-loop --print-config");
                println!("free-loop --log-surface");
                return Ok(());
            }
            "--config" => {
                path = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or("--config needs a path")?;
            }
            other => path = PathBuf::from(other),
        }
    }

    let config = Config::load(&path)?;
    println!("config: {}", path.display());

    let opened = open(&config.audio())?;
    let negotiated = opened.negotiated();
    println!(
        "audio: {} Hz, {} channels in / {} out, {} frames of cushion",
        negotiated.sample_rate,
        negotiated.input_channels,
        negotiated.channels,
        negotiated.cushion_frames
    );

    let engine = Engine::new(config.engine(negotiated.sample_rate, negotiated.channels)?)?;
    let mut io = opened.start(engine)?;

    let mut surface = connect_surface();
    let mut controller = Controller::new(
        config.transport.tempo,
        config.transport.beats_per_bar,
        config.click.enabled,
    );

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

    let mut events: Vec<SurfaceEvent> = Vec::new();
    let started = Instant::now();
    // Only known once the driver has run a callback and said how much it buffers.
    let mut reported_latency = false;

    while running.load(Ordering::Relaxed) {
        let now = started.elapsed();

        events.clear();
        surface.poll(&mut events);
        for event in events.drain(..) {
            if log_surface {
                println!("surface: {event:?}");
            }
            controller.on_surface(event, now);
        }
        controller.tick(now);

        for command in controller.drain_commands() {
            if io.send(command).is_err() {
                eprintln!("audio thread is not keeping up; dropped {command:?}");
            }
        }

        io.drain_events(|event| {
            report(event);
            controller.on_engine(event);
        });

        if let Some(frame) = controller.take_frame()
            && let Err(error) = surface.render(frame)
        {
            eprintln!("surface: {error}");
        }

        if !reported_latency {
            let frames = io.capture_offset_frames();
            if frames > 0 {
                reported_latency = true;
                let millis = f64::from(frames) / f64::from(negotiated.sample_rate) * 1000.0;
                println!("round trip: {frames} frames ({millis:.1} ms), compensated");
            }
        }

        std::thread::sleep(TICK);
    }

    // Leaving the grid lit after the process is gone looks like it is still running.
    if let Err(error) = surface.clear() {
        eprintln!("surface: {error}");
    }
    println!("\nstopped. device errors: {}", io.device_errors());
    Ok(())
}

/// Connects a Launchpad, falling back to a surface with no hardware behind it.
///
/// A missing pad is not fatal: the click and the audio path still work, which is enough
/// to tell whether the rig is right before hunting for the controller.
fn connect_surface() -> Box<dyn ControlSurface> {
    match LaunchpadX::connect() {
        Ok(launchpad) => {
            println!("surface: Launchpad X");
            Box::new(launchpad)
        }
        Err(SurfaceError::NotFound) => {
            println!("surface: none found, running headless");
            Box::new(MockSurface::new())
        }
        Err(error) => {
            eprintln!("surface: {error}; running headless");
            Box::new(MockSurface::new())
        }
    }
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
        Event::Bar { .. }
        | Event::Beat { .. }
        | Event::SlotChanged { .. }
        | Event::ClipReleased { .. } => {}
    }
}
