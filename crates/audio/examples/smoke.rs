//! Manual check against real hardware.
//!
//! Lists devices, opens a pair, and runs a scripted take: one bar of count-in, two bars
//! recorded, then the loop plays for four more with the click running.
//!
//! ```text
//! cargo run -p free-loop-audio --example smoke -- [device substring] [input channel]
//! ```
//!
//! Naming an input channel spreads that one device channel across both sides. An
//! interface reports every input whether or not anything is plugged in, so a single
//! instrument needs to say which channel it is on. A Scarlett Solo's instrument jack is
//! channel 1.

use std::error::Error;
use std::time::{Duration, Instant};

use free_loop_audio::{AudioConfig, InputSource, list_devices, open};
use free_loop_core::{
    Command, Event, Frames, SampleRate, SlotAddr, SlotId, SlotState, Tempo, TimeSignature, TrackId,
};
use free_loop_engine::{ClickConfig, Engine, EngineConfig};

const TEMPO: f64 = 120.0;
/// Bars of click before recording starts.
const COUNT_IN_BARS: u32 = 1;
/// Bars to record.
const RECORD_BARS: u32 = 2;
/// Bars to let the loop run afterwards.
const PLAY_BARS: u32 = 4;

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

    let (engine, _recycler) = Engine::new(EngineConfig {
        sample_rate: SampleRate::new(negotiated.sample_rate)?,
        tempo: Tempo::new(TEMPO)?,
        time_signature: TimeSignature::FOUR_FOUR,
        channels: negotiated.channels,
        max_bars: 32,
        segment_pool: 64,
        capture_offset: Frames::ZERO,
        click: ClickConfig::default(),
    })?;
    let frames_per_bar = engine.grid().frames_per_bar().0;

    let mut io = opened.start(engine)?;
    let pad = SlotAddr::new(TrackId::new(0)?, SlotId::new(0)?);

    // Presses take effect on the following bar line, so each sits one bar ahead of the
    // boundary it acts on. Mid-bar keeps them clear of the line itself, where an arm
    // would race the boundary it is meant to wait for.
    let bar = Duration::from_secs_f64(60.0 / TEMPO * 4.0);
    let arm_at = bar * (COUNT_IN_BARS - 1) + bar / 2;
    // Stop rounds back to the line that just passed, so the press has to land after the
    // last bar wanted rather than inside it.
    let stop_at = bar * (COUNT_IN_BARS + RECORD_BARS) + bar / 4;
    let script = [
        (arm_at, Command::Press(pad)),
        (stop_at, Command::Press(pad)),
    ];
    let mut next = 0;

    let mut recording = false;
    let start = Instant::now();
    let total = bar * (COUNT_IN_BARS + RECORD_BARS + PLAY_BARS);

    while start.elapsed() < total {
        if let Some((at, command)) = script.get(next)
            && start.elapsed() >= *at
        {
            io.send(*command).map_err(|_| "command queue is full")?;
            next += 1;
        }

        io.drain_events(|event| match event {
            Event::Bar { bar } => {
                // Counted from one, the way the click is counted.
                let marker = if recording { "   <<< PLAY NOW" } else { "" };
                println!("bar {}{marker}", bar + 1);
            }
            Event::SlotChanged { state, .. } => match state {
                SlotState::Recording { .. } => {
                    recording = true;
                    println!("=== RECORDING {RECORD_BARS} BARS, PLAY ===");
                }
                SlotState::Playing { .. } => {
                    recording = false;
                    println!("=== LOOPING ===");
                }
                _ => {}
            },
            Event::ClipRecorded { len, .. } => {
                println!(
                    "captured {} bars ({} frames)",
                    len.0 / frames_per_bar,
                    len.0
                );
            }
            Event::Xrun { frames } => println!("xrun: {frames} frames"),
            Event::Beat { .. } => {}
            other => println!("{other:?}"),
        });

        std::thread::sleep(Duration::from_millis(5));
    }

    println!(
        "\nmeasured round trip: {} frames",
        io.capture_offset_frames()
    );
    println!("device errors: {}", io.device_errors());
    Ok(())
}
