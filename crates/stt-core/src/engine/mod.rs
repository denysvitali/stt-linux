//! ASR engine abstraction.
//!
//! Everything above this layer deals in 16 kHz mono `f32` and gets back text.
//! Which model produces that text — Parakeet today, Whisper behind a feature
//! flag — is not the pipeline's concern.

pub mod parakeet;

use anyhow::Result;

use crate::config::{EngineBackend, EngineConfig};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// Detected language, when the model reports one.
    pub language: Option<String>,
}

pub trait Engine: Send {
    /// Transcribe 16 kHz mono samples normalized to `[-1, 1]`.
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Transcript>;

    /// Longest audio this engine handles in one call.
    ///
    /// Parakeet TDT degrades past roughly 4–5 minutes, so the pipeline splits
    /// longer recordings at silence boundaries rather than letting the model
    /// quietly truncate them.
    fn max_segment_secs(&self) -> f32;

    fn name(&self) -> &str;
}

/// Construct the engine named by `config`.
pub fn load(config: &EngineConfig) -> Result<Box<dyn Engine>> {
    match config.backend {
        EngineBackend::Parakeet => {
            Ok(Box::new(parakeet::ParakeetEngine::load(config)?) as Box<dyn Engine>)
        }
        EngineBackend::Whisper => {
            anyhow::bail!(
                "the whisper backend is not built in yet (milestone M8); \
                 set engine.backend = \"parakeet\""
            )
        }
    }
}
