//! An on-screen Launchpad X, played with the pointer instead of the hardware.
//!
//! The emulator publishes its own MIDI ports, so the looper reaches it the way it reaches
//! a real pad. Attaching hardware as well mirrors both, leaving the window a view of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{CentralPanel, Frame, Key, Panel, Slider, Ui, ViewportBuilder, ViewportCommand};
use launchpad_emulator::devices::LaunchpadX;
use launchpad_emulator::{Emulator, Interaction};
use launchpad_emulator_ui::{Console, LaunchpadUi, Layout};

use crate::labels;

/// The port the window publishes, distinct from the hardware's so either can be chosen.
pub const PORT_NAME: &str = "Launchpad X LPX MIDI (free-loop)";

/// How often the hardware is looked for.
const HARDWARE_CHECK: Duration = Duration::from_secs(1);

/// Height of the bars above and below the surface.
const CHROME: f32 = 72.0;

/// Width the surface opens at.
const BOARD_SIZE: f32 = 460.0;

/// Smallest the surface is allowed to become.
const MIN_BOARD: f32 = 280.0;

/// Narrowest the window may be, set by the controls rather than the surface.
const MIN_WIDTH: f32 = 520.0;

/// Longest the window waits for the looper to darken the surface as it closes.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Publishes the emulated surface's ports, before anything looks for them.
///
/// # Errors
///
/// If the MIDI backend will not give up a virtual port under [`PORT_NAME`].
pub fn open() -> Result<Emulator<LaunchpadX>, launchpad_emulator::Error> {
    let mut emulator = Emulator::<LaunchpadX>::new(PORT_NAME)?;
    let hardware = emulator.refresh_hardware().unwrap_or(false);
    tracing::info!(port = PORT_NAME, hardware, "surface window");
    Ok(emulator)
}

/// Runs the window until it is closed, holding the main thread.
///
/// # Errors
///
/// If the window cannot be opened.
pub fn run(
    emulator: Emulator<LaunchpadX>,
    console: Console,
    running: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        persist_window: false,
        viewport: ViewportBuilder::default()
            .with_title("Free Loop")
            .with_inner_size([BOARD_SIZE, BOARD_SIZE + CHROME])
            .with_min_inner_size([MIN_WIDTH, MIN_BOARD + CHROME]),
        ..Default::default()
    };
    eframe::run_native(
        "Free Loop",
        options,
        Box::new(move |cc| {
            // A label should appear the moment the pointer arrives
            cc.egui_ctx.all_styles_mut(|style| {
                style.animation_time = 0.0;
                style.interaction.tooltip_delay = 0.0;
                style.interaction.tooltip_grace_time = 0.0;
            });
            Ok(Box::new(Window {
                layout: Layout::for_device::<LaunchpadX>(),
                widget: LaunchpadUi::new().with_labels(labels::fixed()),
                emulator,
                console,
                running,
                stopped,
                hardware: false,
                checked: Instant::now(),
            }))
        }),
    )
}

/// The window's state.
struct Window {
    emulator: Emulator<LaunchpadX>,
    layout: Layout,
    widget: LaunchpadUi,
    console: Console,
    running: Arc<AtomicBool>,
    /// Set once the looper has stopped and darkened the surface.
    stopped: Arc<AtomicBool>,
    hardware: bool,
    /// When the hardware was last looked for.
    checked: Instant,
}

impl Window {
    /// Drains the emulator and shows anything the hardware reported, dropping the
    /// messages themselves: at 24 pulses a quarter note the clock alone would bury the log.
    fn pump(&mut self) {
        drop(self.emulator.poll());
        match self.emulator.pump_hardware() {
            Ok(interactions) => {
                for interaction in interactions {
                    self.widget.apply(interaction);
                }
            }
            Err(error) => tracing::warn!("hardware: {error}"),
        }
    }

    /// Passes what the pointer did back to the looper.
    fn send(&mut self, interactions: Vec<Interaction>) {
        for interaction in interactions {
            if let Err(error) = self.emulator.send(interaction) {
                tracing::warn!("could not report a press: {error}");
            }
        }
    }

    /// Draws the strip describing what the looper has asked for.
    fn status_bar(&mut self, ui: &mut Ui, surface: &launchpad_emulator::Surface) {
        ui.horizontal(|ui| {
            ui.label(if surface.is_programmer_mode() {
                "programmer"
            } else {
                "live"
            });
            ui.separator();
            ui.label(if self.hardware {
                "hardware attached"
            } else {
                "no hardware"
            });
            ui.separator();
            if ui
                .selectable_label(self.console.is_visible(), "log")
                .on_hover_text("Show what the looper is reporting (`)")
                .clicked()
            {
                self.console.toggle();
            }
            if self.console.is_visible() && ui.button("clear").clicked() {
                self.console.clear();
            }
            let mut velocity = self.widget.velocity();
            if ui
                .add(Slider::new(&mut velocity, 1..=127).text("velocity"))
                .changed()
            {
                self.widget.set_velocity(velocity);
            }
        });
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // The looper stopping on its own leaves nothing for the window to drive
        if !self.running.load(Ordering::Relaxed) {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        self.pump();
        let _ = self.emulator.advance();
        if self.checked.elapsed() >= HARDWARE_CHECK {
            self.checked = Instant::now();
            match self.emulator.refresh_hardware() {
                Ok(attached) => self.hardware = attached,
                Err(error) => tracing::warn!("looking for the hardware: {error}"),
            }
        }

        let beats = self.emulator.beats().unwrap_or(0.0);
        let Ok(surface) = self.emulator.surface() else {
            return;
        };

        if ui.ctx().input(|i| i.key_pressed(Key::Backtick)) {
            self.console.toggle();
        }
        Panel::top("status").show(ui, |ui| self.status_bar(ui, &surface));
        if self.console.is_visible() {
            Panel::bottom("log")
                .resizable(true)
                .show(ui, |ui| self.console.show(ui));
        }

        let board = CentralPanel::default()
            .frame(Frame::NONE)
            .show(ui, |ui| self.widget.show(ui, &self.layout, &surface, beats))
            .inner;
        self.send(board.inner);

        // Flashing and pulsing animate between messages
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

/// Stops the looper when the window goes away, and waits for it.
impl Drop for Window {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // The looper darkens the surface on its way out, which only reaches attached
        // hardware while this window still holds the ports it travels through.
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !self.stopped.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                tracing::warn!("the looper did not stop in time; the pad may be left lit");
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
