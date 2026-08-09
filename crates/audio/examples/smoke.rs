//! Manual check against real hardware.
//!
//! Lists devices, opens the default pair, and runs a scripted take: arm a pad, record two
//! bars, then loop it for eight more with the click running.
//!
//! ```text
//! cargo run -p free-loop-audio --example smoke -- [device substring] [input channel]
//! ```
//!
//! Naming an input channel spreads that one device channel across both sides. An
//! interface reports every input whether or not anything is plugged in, so a single
//! instrument needs to say which channel it is on — a Scarlett Solo's instrument jack
//! is channel 1.

use std::error::Error;
use std::time::{Duration, Instant};

use free_loop_audio::{AudioConfig, InputSource, list_devices, open};
use free_loop_core::{Command, Event, SampleRate, SlotAddr, SlotId, Tempo, TimeSignature, TrackId};
use free_loop_engine::{ClickConfig, Engine, EngineConfig};

const TEMPO: f64 = 120.0;

fn main() -> Result<(), Box<dyn Error>> {
    let device = std::env::args().nth(1);
    let input_source = match std::env::args().nth(2) {
        Some(channel) => InputSource::Mono(channel.parse()?),
        None => InputSource::Direct,
    };

    let devices = list_devices()?;
    println!("inputs:");
    for name in &devices.inputs {
        println!("  {name}");
    }
    println!("outputs:");
    for name in &devices.outputs {
        println!("  {name}");
    }

    let config = AudioConfig {
        input_device: device.clone(),
        output_device: device,
        input_source,
        ..AudioConfig::new()
    };

    let opened = open(&config)?;
    let negotiated = opened.negotiated();
    println!("\n{negotiated:#?}");
    let cushion = negotiated.added_latency_frames();
    let millis = u32::try_from(cushion).unwrap_or(u32::MAX);
    println!(
        "added latency: {cushion} frames ({:.1} ms)\n",
        f64::from(millis) / f64::from(negotiated.sample_rate) * 1000.0
    );

    let engine = Engine::new(EngineConfig {
        sample_rate: SampleRate::new(negotiated.sample_rate)?,
        tempo: Tempo::new(TEMPO)?,
        time_signature: TimeSignature::FOUR_FOUR,
        channels: negotiated.channels,
        max_bars: 32,
        segment_pool: 64,
        click: ClickConfig::default(),
    })?;

    let mut io = opened.start(engine)?;
    let pad = SlotAddr::new(TrackId::new(0)?, SlotId::new(0)?);

    // Arming during bar 1 starts capture on the bar 2 line, so the stop press has to
    // land on the bar 4 line to get two bars.
    let bar = Duration::from_secs_f64(60.0 / TEMPO * 4.0);
    let script = [(bar, Command::Press(pad)), (bar * 4, Command::Press(pad))];
    let mut next = 0;

    println!("click is running. play something during bars 2 and 3.");

    let start = Instant::now();
    while start.elapsed() < bar * 12 {
        if let Some((at, command)) = script.get(next)
            && start.elapsed() >= *at
        {
            io.send(*command).map_err(|_| "command queue is full")?;
            next += 1;
        }

        // Beats are too chatty to print; everything else is worth seeing.
        io.drain_events(|event| {
            if !matches!(event, Event::Beat { .. }) {
                println!("{event:?}");
            }
        });

        std::thread::sleep(Duration::from_millis(5));
    }

    println!("\ndevice errors: {}", io.device_errors());
    Ok(())
}
