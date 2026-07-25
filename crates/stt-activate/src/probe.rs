//! Runtime availability checks for each activation backend.

use anyhow::Result;
use std::path::Path;
use stt_core::Availability;
use stt_core::config::ActivationBackend;

const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const GLOBAL_SHORTCUTS_IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub backend: ActivationBackend,
    pub availability: Availability,
    pub detail: String,
}

impl Probe {
    fn new(
        backend: ActivationBackend,
        availability: Availability,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            availability,
            detail: detail.into(),
        }
    }

    fn ok(backend: ActivationBackend, detail: impl Into<String>) -> Self {
        Self::new(backend, Availability::Available, detail)
    }

    fn degraded(backend: ActivationBackend, detail: impl Into<String>) -> Self {
        Self::new(backend, Availability::Degraded, detail)
    }

    fn no(backend: ActivationBackend, detail: impl Into<String>) -> Self {
        Self::new(backend, Availability::Unavailable, detail)
    }

    pub fn is_usable(&self) -> bool {
        self.availability.is_usable()
    }
}

pub fn probe_all() -> Vec<Probe> {
    vec![probe_portal(), probe_evdev(), probe_socket()]
}

/// Query the session bus for a `GlobalShortcuts` portal implementation.
///
/// Presence of `org.freedesktop.portal.Desktop` is not enough: the wlroots
/// backend ships the bus name but no GlobalShortcuts interface, so we read the
/// interface's `version` property and treat a failure as "unimplemented".
fn probe_portal() -> Probe {
    let b = ActivationBackend::Portal;
    match global_shortcuts_version() {
        Ok(Some(v)) => Probe::ok(b, format!("{GLOBAL_SHORTCUTS_IFACE} version {v}")),
        Ok(None) => Probe::no(
            b,
            "portal is running but implements no GlobalShortcuts interface \
             (xdg-desktop-portal-wlr does not; use the socket backend)",
        ),
        Err(e) => Probe::no(b, format!("no session portal: {e}")),
    }
}

fn global_shortcuts_version() -> Result<Option<u32>> {
    let conn = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&conn, PORTAL_BUS, PORTAL_PATH, GLOBAL_SHORTCUTS_IFACE)?;
    // An unimplemented interface answers with an error, not a missing value.
    Ok(proxy.get_property::<u32>("version").ok())
}

/// evdev needs read access to the raw input devices. Rather than inspecting
/// group membership, try to open one and see.
fn probe_evdev() -> Probe {
    let b = ActivationBackend::Evdev;
    let dir = Path::new("/dev/input");
    if !dir.exists() {
        return Probe::no(b, "/dev/input does not exist");
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Probe::no(b, "/dev/input is not readable");
    };

    let mut total = 0usize;
    let mut readable = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"))
        {
            continue;
        }
        total += 1;
        if std::fs::File::open(&path).is_ok() {
            readable += 1;
        }
    }

    match (total, readable) {
        (0, _) => Probe::no(b, "no /dev/input/event* devices found"),
        (t, 0) => Probe::no(
            b,
            format!("{t} input devices found, none readable (add yourself to the `input` group)"),
        ),
        (t, r) if r == t => Probe::ok(b, format!("all {t} input devices readable")),
        // Partial access usually means the readable node is something like a
        // power button, not the keyboard we need. Do not present that as a
        // working backend; M6 replaces this count with a real capability check.
        (t, r) => Probe::degraded(
            b,
            format!(
                "only {r}/{t} input devices readable — the keyboard may not be \
                 among them (add yourself to the `input` group)"
            ),
        ),
    }
}

/// The control socket is part of the daemon itself, so it is unconditionally
/// available. This is the backstop that makes every compositor work.
fn probe_socket() -> Probe {
    match stt_core::paths::socket_path() {
        Ok(p) => Probe::ok(
            ActivationBackend::Socket,
            format!("always available at {}", p.display()),
        ),
        Err(e) => Probe::no(ActivationBackend::Socket, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_all_covers_every_backend() {
        let probes = probe_all();
        assert_eq!(probes.len(), 3);
        let kinds: Vec<_> = probes.iter().map(|p| p.backend).collect();
        assert!(kinds.contains(&ActivationBackend::Portal));
        assert!(kinds.contains(&ActivationBackend::Evdev));
        assert!(kinds.contains(&ActivationBackend::Socket));
    }

    #[test]
    fn probes_never_panic_and_always_explain_themselves() {
        for p in probe_all() {
            assert!(!p.detail.is_empty(), "{:?} gave no explanation", p.backend);
        }
    }

    #[test]
    fn socket_backend_is_always_usable() {
        // This is the guarantee the whole activation design rests on: whatever
        // the compositor does or does not implement, the socket works.
        let socket = probe_all()
            .into_iter()
            .find(|p| p.backend == ActivationBackend::Socket)
            .expect("socket backend must always be probed");
        assert!(socket.is_usable(), "{}", socket.detail);
    }
}
