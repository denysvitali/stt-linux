//! Runtime availability checks for each injection backend.
//!
//! Probing is separated from injecting so that `stt doctor` can report on the
//! whole chain without side effects, and so the daemon can pick a backend once
//! at startup instead of discovering failures mid-dictation.

use std::path::{Path, PathBuf};
use stt_core::Availability;
use stt_core::config::InjectBackend;
use stt_core::wayland::{self, Globals};

/// Outcome of probing one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub backend: InjectBackend,
    pub availability: Availability,
    /// Human-readable reason, shown by `stt doctor` whether or not the backend
    /// is available — a working backend still says *why* it works.
    pub detail: String,
}

impl Probe {
    fn new(backend: InjectBackend, availability: Availability, detail: impl Into<String>) -> Self {
        Self {
            backend,
            availability,
            detail: detail.into(),
        }
    }

    fn ok(backend: InjectBackend, detail: impl Into<String>) -> Self {
        Self::new(backend, Availability::Available, detail)
    }

    fn degraded(backend: InjectBackend, detail: impl Into<String>) -> Self {
        Self::new(backend, Availability::Degraded, detail)
    }

    fn no(backend: InjectBackend, detail: impl Into<String>) -> Self {
        Self::new(backend, Availability::Unavailable, detail)
    }

    pub fn is_usable(&self) -> bool {
        self.availability.is_usable()
    }

    /// Lowercase backend name, for status output and logs.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            InjectBackend::Wtype => "wtype",
            InjectBackend::Clipboard => "clipboard",
            InjectBackend::Uinput => "uinput",
            InjectBackend::InputMethod => "input-method",
            InjectBackend::CopyOnly => "copy-only",
        }
    }
}

/// Look up an executable on `$PATH`.
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn is_writable(path: &str) -> bool {
    // `access(2)` semantics: we want to know whether *this* process could open
    // the node for writing, which permission bits alone do not tell us
    // (supplementary groups, ACLs).
    std::fs::OpenOptions::new().write(true).open(path).is_ok()
}

/// Probe every backend against the current session.
///
/// `globals` is `None` when there is no Wayland connection, in which case the
/// compositor-dependent backends report as unavailable rather than guessing.
pub fn probe_all(globals: Option<&Globals>) -> Vec<Probe> {
    vec![
        probe_wtype(globals),
        probe_clipboard(globals),
        probe_uinput(),
        probe_input_method(globals),
        probe_copy_only(globals),
    ]
}

/// Copy-only is the last-resort backend: it keeps a transcript recoverable on
/// any Wayland session. It still needs Wayland, because `wl-copy` talks to the
/// compositor's clipboard.
fn probe_copy_only(globals: Option<&Globals>) -> Probe {
    let b = InjectBackend::CopyOnly;
    if which("wl-copy").is_none() {
        return Probe::no(b, "`wl-copy` not found on $PATH (install wl-clipboard)");
    }
    match globals {
        None => Probe::no(b, "wl-copy found, but no Wayland session"),
        // Degraded rather than available: it never actually types, so it is
        // only ever the fallback, never the thing you would pick first.
        Some(_) => Probe::degraded(b, "copies to the clipboard; never types"),
    }
}

fn probe_wtype(globals: Option<&Globals>) -> Probe {
    let b = InjectBackend::Wtype;
    let Some(bin) = which("wtype") else {
        return Probe::no(b, "`wtype` not found on $PATH");
    };
    match globals {
        None => Probe::no(b, format!("{} found, but no Wayland session", bin.display())),
        Some(g) if !g.has(wayland::VIRTUAL_KEYBOARD) => Probe::no(
            b,
            format!(
                "{} found, but the compositor does not advertise {}",
                bin.display(),
                wayland::VIRTUAL_KEYBOARD
            ),
        ),
        Some(_) => Probe::ok(b, format!("{} + virtual-keyboard protocol", bin.display())),
    }
}

fn probe_clipboard(globals: Option<&Globals>) -> Probe {
    let b = InjectBackend::Clipboard;
    let copy = which("wl-copy");
    let paste = which("wl-paste");
    match (copy, paste) {
        (None, _) => Probe::no(b, "`wl-copy` not found on $PATH (install wl-clipboard)"),
        (_, None) => Probe::no(b, "`wl-paste` not found on $PATH (install wl-clipboard)"),
        (Some(_), Some(_)) => {
            if globals.is_none() {
                return Probe::no(b, "wl-clipboard found, but no Wayland session");
            }
            // The clipboard backend also needs *some* way to send the paste
            // chord. Without one it can still place text on the clipboard, but
            // the user has to press the keys themselves.
            let can_send_keys = which("wtype").is_some() || is_writable("/dev/uinput");
            if can_send_keys {
                Probe::ok(b, "wl-clipboard + a key-synthesis path")
            } else {
                // Still worth something: the transcript lands on the clipboard
                // and the user presses paste themselves.
                Probe::degraded(
                    b,
                    "wl-clipboard found, but nothing can synthesize the paste chord \
                     (need wtype or /dev/uinput) — you would have to paste manually",
                )
            }
        }
    }
}

fn probe_uinput() -> Probe {
    let b = InjectBackend::Uinput;
    const NODE: &str = "/dev/uinput";
    if !Path::new(NODE).exists() {
        return Probe::no(b, "/dev/uinput does not exist (load the `uinput` module)");
    }
    if is_writable(NODE) {
        Probe::ok(b, "/dev/uinput is writable")
    } else {
        Probe::no(
            b,
            "/dev/uinput exists but is not writable (add yourself to the `input` \
             group or install a udev rule)",
        )
    }
}

fn probe_input_method(globals: Option<&Globals>) -> Probe {
    let b = InjectBackend::InputMethod;
    match globals {
        None => Probe::no(b, "no Wayland session"),
        Some(g) if !g.has(wayland::INPUT_METHOD) => Probe::no(
            b,
            format!("compositor does not advertise {}", wayland::INPUT_METHOD),
        ),
        // Architecturally the right protocol, but it only reaches clients that
        // implement text-input-v3 — which excludes most Electron apps,
        // XWayland apps and terminals. Never a clean tick.
        Some(_) => Probe::degraded(
            b,
            "input-method-v2 present, but it only reaches text-input-v3 clients \
             (not Electron, XWayland or most terminals)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stt_core::wayland::Global;

    fn globals_with(interfaces: &[&str]) -> Globals {
        Globals {
            globals: interfaces
                .iter()
                .map(|i| Global {
                    interface: (*i).into(),
                    version: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn which_finds_a_known_binary() {
        // `sh` is guaranteed by POSIX to exist on any system running these tests.
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyzzy").is_none());
    }

    #[test]
    fn wtype_unavailable_without_the_protocol() {
        let g = globals_with(&[wayland::LAYER_SHELL]);
        let p = probe_wtype(Some(&g));
        assert!(!p.is_usable());
        assert!(p.detail.contains(wayland::VIRTUAL_KEYBOARD));
    }

    #[test]
    fn input_method_requires_its_global() {
        assert!(!probe_input_method(None).is_usable());
        assert!(!probe_input_method(Some(&globals_with(&[]))).is_usable());
        // Present, but never a clean tick: it cannot reach every client.
        let p = probe_input_method(Some(&globals_with(&[wayland::INPUT_METHOD])));
        assert_eq!(p.availability, Availability::Degraded);
    }

    #[test]
    fn probe_all_covers_every_backend() {
        let probes = probe_all(None);
        assert_eq!(probes.len(), 5, "every InjectBackend variant must be probed");
    }

    #[test]
    fn nothing_claims_usability_without_wayland() {
        // Every backend here ultimately talks to the compositor. Only uinput
        // bypasses it, going straight to the kernel.
        for p in probe_all(None) {
            if p.backend != InjectBackend::Uinput {
                assert!(
                    !p.is_usable(),
                    "{:?} claimed usability off-Wayland: {}",
                    p.backend,
                    p.detail
                );
            }
        }
    }

    #[test]
    fn copy_only_is_available_whenever_wl_copy_is() {
        // This is the guarantee that a transcript is never lost: on any
        // Wayland session with wl-clipboard, there is always some backend.
        let globals = globals_with(&[]);
        let p = probe_copy_only(Some(&globals));
        if which("wl-copy").is_some() {
            assert!(p.is_usable(), "{}", p.detail);
        }
    }
}
