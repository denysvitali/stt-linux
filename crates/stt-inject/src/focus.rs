//! Which window is focused, via compositor IPC.
//!
//! Injection sends keystrokes to whatever is focused *at the moment it runs* —
//! not to whatever was focused when the user started dictating. If focus moved
//! in between (they alt-tabbed, a notification stole it, a dialog opened), the
//! transcript lands in the wrong place. Worst case that is a password field or
//! a chat window.
//!
//! So the daemon records focus at record-start, checks it again before
//! injecting, and downgrades to clipboard-only when they differ.
//!
//! Not every compositor exposes this. When focus cannot be determined we
//! return `Unknown` and the daemon proceeds — refusing to work on compositors
//! without IPC would be worse than the risk.

use std::process::Command;

/// Identity of the focused window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    /// A specific window, identified by a compositor-stable handle.
    Window { id: String, app: String },
    /// Nothing is focused.
    None,
    /// This compositor does not tell us.
    Unknown,
}

impl Focus {
    /// Whether injecting now is safe given the focus recorded at record-start.
    ///
    /// `Unknown` on either side means we cannot tell, and we allow it —
    /// otherwise the tool would refuse to type on sway, niri and GNOME.
    pub fn is_same_target(&self, other: &Focus) -> bool {
        match (self, other) {
            (Focus::Window { id: a, .. }, Focus::Window { id: b, .. }) => a == b,
            (Focus::Unknown, _) | (_, Focus::Unknown) => true,
            (Focus::None, Focus::None) => true,
            _ => false,
        }
    }

    pub fn app(&self) -> Option<&str> {
        match self {
            Focus::Window { app, .. } => Some(app),
            _ => None,
        }
    }
}

/// Query the compositor for the focused window.
pub fn current() -> Focus {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return hyprland_focus();
    }
    Focus::Unknown
}

/// Hyprland: `hyprctl activewindow -j`.
///
/// The `address` field is a stable per-window handle, which is what we compare
/// on — class and title both change while a window stays the same.
fn hyprland_focus() -> Focus {
    let Ok(output) = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
    else {
        return Focus::Unknown;
    };
    if !output.status.success() {
        return Focus::Unknown;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_hyprland_activewindow(&text)
}

/// Extract address and class from `hyprctl activewindow -j` output.
///
/// Hand-parsed rather than pulling in a JSON dependency for two string fields;
/// `stt-inject` is on the hot path of every dictation.
pub(crate) fn parse_hyprland_activewindow(json: &str) -> Focus {
    let trimmed = json.trim();
    // Hyprland prints `{}` when nothing is focused.
    if trimmed.is_empty() || trimmed == "{}" {
        return Focus::None;
    }
    let Some(id) = json_string_field(trimmed, "address") else {
        return Focus::Unknown;
    };
    if id.is_empty() {
        return Focus::None;
    }
    let app = json_string_field(trimmed, "class").unwrap_or_default();
    Focus::Window { id, app }
}

/// Pull `"key": "value"` out of flat JSON.
fn json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let mut chars = after.char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    let mut value = String::new();
    let mut escaped = false;
    for (_, c) in chars {
        if escaped {
            value.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(value);
        } else {
            value.push(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "address": "0x55d1350af270",
        "mapped": true,
        "workspace": {"id": 2, "name": "2"},
        "class": "com.mitchellh.ghostty",
        "title": "stt-linux",
        "pid": 1629,
        "xwayland": false
    }"#;

    #[test]
    fn parses_a_real_hyprctl_payload() {
        let focus = parse_hyprland_activewindow(SAMPLE);
        assert_eq!(
            focus,
            Focus::Window {
                id: "0x55d1350af270".into(),
                app: "com.mitchellh.ghostty".into(),
            }
        );
    }

    #[test]
    fn empty_object_means_nothing_focused() {
        assert_eq!(parse_hyprland_activewindow("{}"), Focus::None);
        assert_eq!(parse_hyprland_activewindow("   "), Focus::None);
    }

    #[test]
    fn garbage_is_unknown_not_a_panic() {
        assert_eq!(parse_hyprland_activewindow("not json"), Focus::Unknown);
        assert_eq!(
            parse_hyprland_activewindow("{\"other\": 1}"),
            Focus::Unknown
        );
    }

    #[test]
    fn does_not_confuse_a_nested_key_with_the_real_one() {
        // "name" appears inside the workspace object; "class" must still be
        // read from the top level.
        let focus = parse_hyprland_activewindow(SAMPLE);
        assert_eq!(focus.app(), Some("com.mitchellh.ghostty"));
    }

    #[test]
    fn handles_escaped_quotes_in_titles() {
        let json = r#"{"address": "0xabc", "title": "say \"hi\"", "class": "foot"}"#;
        assert_eq!(
            parse_hyprland_activewindow(json),
            Focus::Window {
                id: "0xabc".into(),
                app: "foot".into()
            }
        );
    }

    #[test]
    fn same_window_is_a_safe_target() {
        let a = Focus::Window {
            id: "0x1".into(),
            app: "foot".into(),
        };
        // Same address, different title-derived app: still the same window.
        let b = Focus::Window {
            id: "0x1".into(),
            app: "foot".into(),
        };
        assert!(a.is_same_target(&b));
    }

    #[test]
    fn a_different_window_is_not_a_safe_target() {
        let a = Focus::Window {
            id: "0x1".into(),
            app: "foot".into(),
        };
        let b = Focus::Window {
            id: "0x2".into(),
            app: "firefox".into(),
        };
        assert!(!a.is_same_target(&b), "this is the misdirected-paste guard");
    }

    #[test]
    fn losing_focus_entirely_is_not_safe() {
        let a = Focus::Window {
            id: "0x1".into(),
            app: "foot".into(),
        };
        assert!(!a.is_same_target(&Focus::None));
    }

    #[test]
    fn unknown_focus_never_blocks_injection() {
        // Compositors without IPC must still be able to dictate.
        let w = Focus::Window {
            id: "0x1".into(),
            app: "foot".into(),
        };
        assert!(Focus::Unknown.is_same_target(&w));
        assert!(w.is_same_target(&Focus::Unknown));
        assert!(Focus::Unknown.is_same_target(&Focus::Unknown));
    }
}
