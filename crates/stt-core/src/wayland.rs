//! Wayland global (protocol) discovery.
//!
//! Which text-injection and overlay strategies are viable is decided entirely
//! by what the running compositor advertises on its registry, so we ask it once
//! at startup rather than guessing from `XDG_CURRENT_DESKTOP`.

use anyhow::{Context, Result};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

/// Interface name for the wlroots layer-shell, used by the recording overlay.
pub const LAYER_SHELL: &str = "zwlr_layer_shell_v1";
/// Interface backing `wtype`.
pub const VIRTUAL_KEYBOARD: &str = "zwp_virtual_keyboard_manager_v1";
/// Interface backing the input-method injector.
pub const INPUT_METHOD: &str = "zwp_input_method_manager_v2";
/// Interface backing `wl-copy` / `wl-paste`.
pub const DATA_CONTROL: &str = "zwlr_data_control_manager_v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    pub interface: String,
    pub version: u32,
}

/// Everything the compositor advertises, sorted by interface name.
#[derive(Debug, Clone, Default)]
pub struct Globals {
    pub globals: Vec<Global>,
}

impl Globals {
    /// Connect to the compositor and enumerate its registry.
    ///
    /// Returns `Err` when there is no Wayland session at all, which the caller
    /// should treat as "not on Wayland" rather than a hard failure.
    pub fn probe() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("no Wayland connection (is WAYLAND_DISPLAY set?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());

        let mut state = Globals::default();
        // A single roundtrip is enough: the compositor sends the whole initial
        // global list before the sync callback it triggers.
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip failed")?;
        state.globals.sort_by(|a, b| a.interface.cmp(&b.interface));
        Ok(state)
    }

    pub fn has(&self, interface: &str) -> bool {
        self.globals.iter().any(|g| g.interface == interface)
    }

    pub fn version_of(&self, interface: &str) -> Option<u32> {
        self.globals
            .iter()
            .find(|g| g.interface == interface)
            .map(|g| g.version)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            interface, version, ..
        } = event
        {
            state.globals.push(Global { interface, version });
        }
    }
}

/// Best-effort identification of the running compositor, for diagnostics only.
/// Never branch behaviour on this — branch on [`Globals`] instead.
pub fn compositor_name() -> String {
    for var in ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return v;
        }
    }
    "unknown".into()
}

/// Whether this process is talking to a Wayland compositor at all.
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_helpers() {
        let g = Globals {
            globals: vec![Global {
                interface: LAYER_SHELL.into(),
                version: 4,
            }],
        };
        assert!(g.has(LAYER_SHELL));
        assert!(!g.has(VIRTUAL_KEYBOARD));
        assert_eq!(g.version_of(LAYER_SHELL), Some(4));
        assert_eq!(g.version_of(INPUT_METHOD), None);
    }
}
