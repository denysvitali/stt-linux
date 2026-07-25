//! User configuration, loaded from `~/.config/stt-linux/config.toml`.
//!
//! Every field has a default, so a missing or partial file is not an error —
//! a fresh install must work with no config at all.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub engine: EngineConfig,
    pub audio: AudioConfig,
    pub activation: ActivationConfig,
    pub inject: InjectConfig,
    pub overlay: OverlayConfig,
    /// Post-transcription text substitutions, applied in order.
    #[serde(default)]
    pub replacements: Vec<Replacement>,
    pub postproc: PostProcConfig,
}

// ---------------------------------------------------------------- engine

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineBackend {
    Parakeet,
    Whisper,
}

/// ONNX Runtime execution provider. `parakeet-rs` falls back to CPU when a
/// provider fails to initialize, so an unsupported choice degrades rather than
/// crashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProvider {
    Cpu,
    OpenVino,
    WebGpu,
    Cuda,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct EngineConfig {
    pub backend: EngineBackend,
    /// Directory name under the models dir, or an absolute path.
    pub model_dir: String,
    pub execution_provider: ExecutionProvider,
    /// BCP-47 code, or `"auto"` to let the model decide.
    pub language: String,
    /// Prefer int8 model weights when present. Roughly 4x smaller and faster
    /// on CPU; falls back to fp32 automatically when the files are absent.
    pub prefer_int8: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            backend: EngineBackend::Parakeet,
            model_dir: "parakeet-tdt-0.6b-v3".into(),
            execution_provider: ExecutionProvider::Cpu,
            language: "auto".into(),
            prefer_int8: true,
        }
    }
}

impl EngineConfig {
    /// Resolve [`Self::model_dir`] against the models directory unless it is
    /// already absolute.
    pub fn resolved_model_dir(&self) -> Result<PathBuf> {
        let raw = Path::new(&self.model_dir);
        if raw.is_absolute() {
            Ok(raw.to_path_buf())
        } else {
            Ok(crate::paths::models_dir()?.join(raw))
        }
    }
}

// ---------------------------------------------------------------- audio

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AudioConfig {
    /// Input device name, or `"default"`.
    pub device: String,
    /// Stop recording after this much trailing silence. `0` disables
    /// VAD auto-stop entirely.
    pub silence_timeout_ms: u32,
    /// Hard cap on a single recording, as a runaway guard.
    pub max_duration_secs: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: "default".into(),
            silence_timeout_ms: 1500,
            max_duration_secs: 300,
        }
    }
}

// ---------------------------------------------------------------- activation

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    Toggle,
    PushToTalk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationBackend {
    /// XDG desktop portal GlobalShortcuts.
    Portal,
    /// Raw `/dev/input/event*` reads. Needs `input` group membership.
    Evdev,
    /// The control socket. Always available; cannot be disabled.
    Socket,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ActivationConfig {
    pub mode: ActivationMode,
    pub backends: Vec<ActivationBackend>,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            mode: ActivationMode::Toggle,
            backends: vec![ActivationBackend::Portal, ActivationBackend::Socket],
        }
    }
}

// ---------------------------------------------------------------- inject

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectBackend {
    /// `wtype`, via the virtual-keyboard protocol.
    Wtype,
    /// Clipboard write plus a synthesized paste chord.
    Clipboard,
    /// `/dev/uinput` writes, compositor-independent.
    Uinput,
    /// `zwp_input_method_v2` commit_string. Only reaches text-input-v3 clients.
    InputMethod,
    /// Put the transcript on the clipboard and stop there — never synthesize
    /// keystrokes. Always available. Useful when you would rather paste
    /// deliberately than have text appear, and the safe choice for testing.
    CopyOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct InjectConfig {
    /// Tried in order; the first that probes successfully wins.
    pub backends: Vec<InjectBackend>,
    /// Paste chord for the clipboard backend. Terminals usually need
    /// `ctrl+shift+v`.
    pub paste_keys: String,
    /// Restore the previous clipboard contents after pasting.
    pub restore_clipboard: bool,
    /// How long to wait before restoring, so the target app has actually read
    /// the selection. Too short and the paste lands empty.
    pub restore_delay_ms: u32,
    /// Always copy the transcript to the clipboard, whichever backend is used,
    /// so a misdirected injection never loses text.
    pub always_copy: bool,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            backends: vec![InjectBackend::Wtype, InjectBackend::Clipboard],
            paste_keys: "ctrl+v".into(),
            restore_clipboard: true,
            restore_delay_ms: 300,
            always_copy: true,
        }
    }
}

// ---------------------------------------------------------------- overlay

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayAnchor {
    Top,
    Bottom,
    Center,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct OverlayConfig {
    pub enabled: bool,
    pub anchor: OverlayAnchor,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            anchor: OverlayAnchor::Bottom,
        }
    }
}

// ---------------------------------------------------------------- post-processing

/// A single text substitution.
///
/// Exactly one of `from` (literal) or `pattern` (regex) must be set; this is
/// validated in [`Config::validate`] rather than by the type system so that a
/// bad config produces a readable error instead of a serde message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PostProcConfig {
    /// Uppercase the first alphabetic character of the transcript.
    pub capitalize_first: bool,
    /// Append a space, so consecutive dictations do not run together.
    pub trailing_space: bool,
}

impl Default for PostProcConfig {
    fn default() -> Self {
        Self {
            capitalize_first: true,
            trailing_space: true,
        }
    }
}

// ---------------------------------------------------------------- loading

impl Config {
    /// Load from the default location, or return defaults if the file is absent.
    pub fn load() -> Result<Self> {
        let path = crate::paths::config_file()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::debug!(path = %path.display(), "no config file, using defaults");
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.inject.backends.is_empty() {
            anyhow::bail!("inject.backends must list at least one backend");
        }
        for (i, r) in self.replacements.iter().enumerate() {
            match (&r.from, &r.pattern) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("replacement #{i}: set either `from` or `pattern`, not both")
                }
                (None, None) => {
                    anyhow::bail!("replacement #{i}: needs either `from` or `pattern`")
                }
                (_, Some(p)) => {
                    regex::Regex::new(p)
                        .with_context(|| format!("replacement #{i}: invalid regex `{p}`"))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Serialize the full config with all defaults filled in, for
    /// `stt config --init`.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let toml_src = r#"
            [audio]
            silence_timeout_ms = 800
        "#;
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.audio.silence_timeout_ms, 800);
        // Untouched fields keep their defaults.
        assert_eq!(cfg.audio.device, "default");
        assert_eq!(cfg.engine.backend, EngineBackend::Parakeet);
        assert!(cfg.overlay.enabled);
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let cfg = Config::default();
        let text = cfg.to_toml().unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("[audio]\nwibble = 3\n").unwrap_err();
        assert!(err.to_string().contains("wibble"), "{err}");
    }

    #[test]
    fn replacement_needs_exactly_one_matcher() {
        let both = r#"[[replacements]]
from = "a"
pattern = "b"
to = "c"
"#;
        let cfg: Config = toml::from_str(both).unwrap();
        assert!(cfg.validate().is_err());

        let neither = "[[replacements]]\nto = \"c\"\n";
        let cfg: Config = toml::from_str(neither).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn bad_regex_is_caught_at_load() {
        let cfg: Config = toml::from_str("[[replacements]]\npattern = \"(\"\nto = \"x\"\n").unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(format!("{err:#}").contains("invalid regex"), "{err:#}");
    }

    #[test]
    fn empty_inject_backends_rejected() {
        let cfg: Config = toml::from_str("[inject]\nbackends = []\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn absolute_model_dir_is_not_rebased() {
        let cfg = EngineConfig {
            model_dir: "/opt/models/tdt".into(),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_model_dir().unwrap(), Path::new("/opt/models/tdt"));
    }
}
