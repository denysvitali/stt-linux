//! Activation backends: how the user tells the daemon to start listening.
//!
//! The control socket always works; the portal and evdev backends are
//! conveniences layered on top for sessions that support them.

pub mod probe;

pub use probe::{Probe, probe_all};
