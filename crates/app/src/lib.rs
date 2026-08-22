//! Free Loop: a `DAWless` looper driven by a Launchpad X.
//!
//! - [`config`]: the config file.
//! - [`control`]: gestures to commands, reports to a frame.
//! - [`paint`]: what each screen looks like. All the colour policy lives here.
//! - [`screen`]: what each button does on the screen that is showing.
//! - [`labels`]: what each button does, named for the on-screen surface.
//!
//! The binary wires those to [`free_loop_audio`], [`free_loop_engine`] and
//! [`free_loop_surface`].

pub mod config;
pub mod control;
pub mod gui;
pub mod labels;
pub mod paint;
pub mod screen;

pub use config::{Config, ConfigError};
pub use control::{Controller, Mode, Request, TextUpdate};
