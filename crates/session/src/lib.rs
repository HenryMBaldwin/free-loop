//! Saving and loading Free Loop sessions.
//!
//! A session is a directory per pad: a manifest of the musical settings and which pad
//! holds what, plus one wav per clip.
//!
//! - [`manifest`]: the settings and grid layout.
//! - [`store`]: the directory layout and the file I/O.

pub mod manifest;
pub mod store;

pub use manifest::{ClipEntry, Manifest};
pub use store::{SavedClip, SessionData, SessionError, SessionStore};
