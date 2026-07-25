//! Core building blocks for stt-linux: configuration, XDG paths, audio device
//! discovery and Wayland capability probing. Capture, VAD and ASR engines land
//! here in later milestones.

pub mod audio;
pub mod capture;
pub mod config;
pub mod engine;
pub mod model;
pub mod paths;
pub mod probe;
pub mod resample;
pub mod wav;
pub mod wayland;

pub use config::Config;
pub use engine::{Engine, Transcript};
pub use probe::Availability;
