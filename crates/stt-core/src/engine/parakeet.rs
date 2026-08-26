//! Parakeet TDT v3 via `parakeet-rs` (ONNX Runtime).
//!
//! Chosen as the default because it is fast on CPU — which is the only option
//! on hardware without CUDA — and because, being a transducer, it emits
//! nothing during silence instead of hallucinating text the way Whisper's
//! decoder does.

use anyhow::{Context, Result};
use parakeet_rs::{ExecutionConfig, ParakeetTDT, Transcriber};

use super::{Engine, Transcript};
use crate::audio::TARGET_SAMPLE_RATE;
use crate::config::{EngineConfig, ExecutionProvider};

/// Parakeet TDT is trained on 4–5 minute windows; beyond that quality falls
/// away. The pipeline uses this to decide where to split.
const MAX_SEGMENT_SECS: f32 = 240.0;

pub struct ParakeetEngine {
    inner: ParakeetTDT,
    label: String,
}

impl ParakeetEngine {
    pub fn load(config: &EngineConfig) -> Result<Self> {
        let dir = config.resolved_model_dir()?;
        anyhow::ensure!(
            dir.is_dir(),
            "model directory {} does not exist — run `stt model download`",
            dir.display()
        );

        let exec =
            ExecutionConfig::new().with_execution_provider(map_provider(config.execution_provider));

        let started = std::time::Instant::now();
        let inner = ParakeetTDT::from_pretrained(&dir, Some(exec))
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("loading the Parakeet model from {}", dir.display()))?;

        let label = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("parakeet-tdt")
            .to_string();

        tracing::info!(
            model = %label,
            elapsed_ms = started.elapsed().as_millis() as u64,
            weights = %describe_weights(&dir),
            "loaded ASR model"
        );

        Ok(Self { inner, label })
    }
}

/// Report which weight files were actually picked up.
///
/// `parakeet-rs` probes `encoder-model.onnx` before `encoder-model.int8.onnx`,
/// so a stray fp32 file silently overrides an int8 download. Logging the
/// resolved choice makes that visible instead of mysterious.
fn describe_weights(dir: &std::path::Path) -> String {
    for (file, kind) in [
        ("encoder-model.onnx", "fp32"),
        ("encoder.onnx", "fp32"),
        ("encoder-model.int8.onnx", "int8"),
    ] {
        if dir.join(file).exists() {
            return format!("{kind} ({file})");
        }
    }
    "unknown".into()
}

/// Map our config enum onto whichever providers were compiled in.
///
/// The `parakeet-rs` variants are cargo-feature gated, so asking for OpenVINO
/// in a build without that feature must degrade to CPU with a warning rather
/// than fail to compile or silently mislead.
fn map_provider(requested: ExecutionProvider) -> parakeet_rs::ExecutionProvider {
    match requested {
        ExecutionProvider::Cpu => parakeet_rs::ExecutionProvider::Cpu,

        #[cfg(feature = "openvino")]
        ExecutionProvider::OpenVino => parakeet_rs::ExecutionProvider::OpenVINO,
        #[cfg(not(feature = "openvino"))]
        ExecutionProvider::OpenVino => {
            tracing::warn!(
                "execution_provider = \"openvino\" but this build lacks the \
                 `openvino` feature; falling back to CPU"
            );
            parakeet_rs::ExecutionProvider::Cpu
        }

        #[cfg(feature = "webgpu")]
        ExecutionProvider::WebGpu => parakeet_rs::ExecutionProvider::WebGPU,
        #[cfg(not(feature = "webgpu"))]
        ExecutionProvider::WebGpu => {
            tracing::warn!(
                "execution_provider = \"webgpu\" but this build lacks the \
                 `webgpu` feature; falling back to CPU"
            );
            parakeet_rs::ExecutionProvider::Cpu
        }

        #[cfg(feature = "cuda")]
        ExecutionProvider::Cuda => parakeet_rs::ExecutionProvider::Cuda,
        #[cfg(not(feature = "cuda"))]
        ExecutionProvider::Cuda => {
            tracing::warn!(
                "execution_provider = \"cuda\" but this build lacks the `cuda` \
                 feature; falling back to CPU"
            );
            parakeet_rs::ExecutionProvider::Cpu
        }
    }
}

impl Engine for ParakeetEngine {
    fn transcribe(&mut self, pcm: &[f32]) -> Result<Transcript> {
        if pcm.is_empty() {
            return Ok(Transcript::default());
        }
        let result = self
            .inner
            // The API takes ownership; the pipeline reuses its buffer, so copy.
            .transcribe_samples(pcm.to_vec(), TARGET_SAMPLE_RATE, 1, None)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("transcribing audio")?;

        Ok(Transcript {
            text: result.text.trim().to_string(),
            language: None,
        })
    }

    fn max_segment_secs(&self) -> f32 {
        MAX_SEGMENT_SECS
    }

    fn name(&self) -> &str {
        &self.label
    }
}
