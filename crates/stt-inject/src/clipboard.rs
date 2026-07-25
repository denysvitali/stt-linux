//! Clipboard-based injection: copy the text, synthesize a paste chord, then
//! put the user's clipboard back.
//!
//! Less elegant than typing, but it is the path that works in the most places
//! — and it is the only one that handles a long transcript instantly rather
//! than key by key.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::Injector;
use crate::keys::Chord;

pub struct ClipboardInjector {
    wl_copy: std::path::PathBuf,
    wl_paste: std::path::PathBuf,
    /// How to press paste. `None` means "copy only" — the transcript reaches
    /// the clipboard and the user pastes it themselves.
    paste: Option<PasteMethod>,
    restore: bool,
    restore_delay: Duration,
}

pub struct PasteMethod {
    pub wtype: std::path::PathBuf,
    pub chord: Chord,
}

pub struct ClipboardOptions {
    pub paste_keys: String,
    pub restore: bool,
    pub restore_delay_ms: u32,
}

impl ClipboardInjector {
    pub fn new(
        wl_copy: std::path::PathBuf,
        wl_paste: std::path::PathBuf,
        wtype: Option<std::path::PathBuf>,
        options: &ClipboardOptions,
    ) -> Result<Self> {
        let paste = match wtype {
            Some(wtype) => Some(PasteMethod {
                wtype,
                chord: Chord::parse(&options.paste_keys)?,
            }),
            None => None,
        };
        Ok(Self {
            wl_copy,
            wl_paste,
            paste,
            restore: options.restore,
            restore_delay: Duration::from_millis(options.restore_delay_ms as u64),
        })
    }

    /// Read the current clipboard so it can be put back afterwards.
    ///
    /// An empty or unset clipboard makes `wl-paste` exit non-zero; that is a
    /// normal state, not an error, so it maps to `None`.
    fn read_clipboard(&self) -> Option<Vec<u8>> {
        let output = Command::new(&self.wl_paste)
            .arg("--no-newline")
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }

    fn write_clipboard(&self, bytes: &[u8]) -> Result<()> {
        // No `--foreground`: the Wayland clipboard needs a live client to own
        // the selection, so wl-copy forks a small server by design and exits
        // once another client takes ownership. Running it in the foreground
        // would make `wait()` below block until that happens — which is
        // forever, if nothing else ever copies.
        let mut child = Command::new(&self.wl_copy)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {}", self.wl_copy.display()))?;

        child
            .stdin
            .take()
            .context("wl-copy stdin was not piped")?
            .write_all(bytes)
            .context("writing to wl-copy")?;

        let status = child.wait().context("waiting for wl-copy")?;
        if !status.success() {
            bail!("wl-copy exited with {status}");
        }
        Ok(())
    }
}

impl Injector for ClipboardInjector {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn inject(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let saved = if self.restore {
            self.read_clipboard()
        } else {
            None
        };

        self.write_clipboard(text.as_bytes())?;

        let Some(paste) = &self.paste else {
            // Copy-only mode: leave the text on the clipboard. Restoring here
            // would throw away the only copy the user has.
            tracing::info!("transcript copied to the clipboard; press paste to insert it");
            return Ok(());
        };

        crate::wtype::run(&paste.wtype, &paste.chord.to_wtype_args())
            .context("sending the paste chord")?;

        if let Some(previous) = saved {
            // The target application reads the selection asynchronously, so
            // restoring immediately can hand it the *old* contents. This delay
            // is the pragmatic fix; there is no completion signal to wait on.
            std::thread::sleep(self.restore_delay);
            if let Err(e) = self.write_clipboard(&previous) {
                tracing::warn!(error = %e, "could not restore the previous clipboard");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(keys: &str) -> ClipboardOptions {
        ClipboardOptions {
            paste_keys: keys.into(),
            restore: true,
            restore_delay_ms: 300,
        }
    }

    #[test]
    fn builds_with_a_valid_chord() {
        let injector = ClipboardInjector::new(
            "/usr/bin/wl-copy".into(),
            "/usr/bin/wl-paste".into(),
            Some("/usr/bin/wtype".into()),
            &options("ctrl+shift+v"),
        )
        .unwrap();
        let chord = &injector.paste.as_ref().unwrap().chord;
        assert_eq!(chord.modifiers, vec!["ctrl", "shift"]);
    }

    #[test]
    fn rejects_a_bad_chord_at_construction() {
        // Better to fail at daemon startup than on the first dictation.
        let result = ClipboardInjector::new(
            "/usr/bin/wl-copy".into(),
            "/usr/bin/wl-paste".into(),
            Some("/usr/bin/wtype".into()),
            &options("ctrl+"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn works_without_a_key_synthesizer() {
        // Degraded but useful: text still reaches the clipboard.
        let injector = ClipboardInjector::new(
            "/usr/bin/wl-copy".into(),
            "/usr/bin/wl-paste".into(),
            None,
            &options("ctrl+v"),
        )
        .unwrap();
        assert!(injector.paste.is_none());
    }
}
