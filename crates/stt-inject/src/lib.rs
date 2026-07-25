//! Text injection backends.
//!
//! Wayland deliberately offers no universal "type this text" API, so there is
//! no single correct backend — only a chain of increasingly compromised ones.
//! This crate probes what the session supports, builds the best available
//! injector, and guards against sending text to the wrong window.

pub mod clipboard;
pub mod focus;
pub mod keys;
pub mod probe;
pub mod wtype;

use anyhow::{Context, Result};
use stt_core::config::{InjectBackend, InjectConfig};
use stt_core::wayland::Globals;

pub use focus::Focus;
pub use probe::{Probe, probe_all, which};

/// Puts text into the focused application.
pub trait Injector: Send {
    fn name(&self) -> &'static str;
    fn inject(&mut self, text: &str) -> Result<()>;

    /// Whether this backend already leaves the text on the clipboard.
    ///
    /// Lets the caller skip its own `always_copy` safety copy, which would
    /// otherwise write the selection twice per dictation.
    fn leaves_text_on_clipboard(&self) -> bool {
        false
    }
}

/// Build the first injector from `config.backends` that this session supports.
///
/// Returns the injector and the probe that justified choosing it, so the
/// daemon can log and report what it settled on.
pub fn build(config: &InjectConfig, globals: Option<&Globals>) -> Result<(Box<dyn Injector>, Probe)> {
    let probes = probe_all(globals);

    for wanted in &config.backends {
        let Some(probe) = probes.iter().find(|p| p.backend == *wanted) else {
            continue;
        };
        if !probe.is_usable() {
            tracing::debug!(backend = ?wanted, reason = %probe.detail, "backend unavailable");
            continue;
        }
        match construct(*wanted, config) {
            Ok(injector) => return Ok((injector, probe.clone())),
            Err(e) => {
                // A backend that probed clean but will not build is worth
                // shouting about; fall through to the next one regardless.
                tracing::warn!(backend = ?wanted, error = %format!("{e:#}"), "could not build backend");
            }
        }
    }

    let tried: Vec<_> = config
        .backends
        .iter()
        .map(|b| format!("{b:?}"))
        .collect();
    anyhow::bail!(
        "no usable text injector (tried: {}); run `stt doctor` for details",
        tried.join(", ")
    )
}

fn construct(backend: InjectBackend, config: &InjectConfig) -> Result<Box<dyn Injector>> {
    match backend {
        InjectBackend::Wtype => {
            let bin = which("wtype").context("wtype vanished after probing")?;
            Ok(Box::new(wtype::WtypeInjector::new(bin)))
        }
        InjectBackend::Clipboard => {
            let copy = which("wl-copy").context("wl-copy vanished after probing")?;
            let paste = which("wl-paste").context("wl-paste vanished after probing")?;
            Ok(Box::new(clipboard::ClipboardInjector::new(
                copy,
                paste,
                which("wtype"),
                &clipboard::ClipboardOptions {
                    paste_keys: config.paste_keys.clone(),
                    restore: config.restore_clipboard,
                    restore_delay_ms: config.restore_delay_ms,
                },
            )?))
        }
        InjectBackend::CopyOnly => Ok(Box::new(CopyOnlyInjector)),
        InjectBackend::Uinput => {
            anyhow::bail!("the uinput backend is not implemented yet")
        }
        InjectBackend::InputMethod => {
            anyhow::bail!("the input-method backend is not implemented yet")
        }
    }
}

/// Places the transcript on the clipboard and does nothing else.
pub struct CopyOnlyInjector;

impl Injector for CopyOnlyInjector {
    fn name(&self) -> &'static str {
        "copy-only"
    }

    fn inject(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        copy_to_clipboard(text)?;
        tracing::info!(chars = text.len(), "copied to the clipboard (copy-only mode)");
        Ok(())
    }

    fn leaves_text_on_clipboard(&self) -> bool {
        true
    }
}

/// Copy text to the clipboard without pasting.
///
/// Used as the safety net: every transcript goes here regardless of which
/// injector runs, so a misdirected or failed injection never loses the text.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;

    // See `clipboard::write_clipboard`: wl-copy must be allowed to fork, or
    // `wait()` blocks until some other client takes the selection.
    let wl_copy = which("wl-copy").context("wl-copy not found on $PATH")?;
    let mut child = std::process::Command::new(&wl_copy)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", wl_copy.display()))?;
    child
        .stdin
        .take()
        .context("wl-copy stdin was not piped")?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    anyhow::ensure!(status.success(), "wl-copy exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_reports_everything_it_tried_when_nothing_works() {
        let config = InjectConfig {
            // Neither is implemented, so this always fails and we can assert
            // on the error text without depending on the environment.
            backends: vec![InjectBackend::Uinput, InjectBackend::InputMethod],
            ..Default::default()
        };
        // `unwrap_err` would need `dyn Injector: Debug`, which it is not.
        let Err(err) = build(&config, None) else {
            panic!("expected no usable injector");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("Uinput"), "{msg}");
        assert!(msg.contains("InputMethod"), "{msg}");
        assert!(msg.contains("stt doctor"), "should point at the diagnostic");
    }

    #[test]
    fn an_empty_backend_list_is_an_error_not_a_panic() {
        let config = InjectConfig {
            backends: vec![],
            ..Default::default()
        };
        assert!(build(&config, None).is_err());
    }
}
