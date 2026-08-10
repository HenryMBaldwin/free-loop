//! Free Loop: a `DAWless` looper driven by a Launchpad X.
//!
//! - [`config`]: the config file.
//! - [`control`]: gestures to commands, reports to a frame.
//!
//! The binary wires those to [`free_loop_audio`], [`free_loop_engine`] and
//! [`free_loop_surface`].

pub mod config;
pub mod control;

pub use config::{Config, ConfigError};
pub use control::{Controller, Mode, Request};
