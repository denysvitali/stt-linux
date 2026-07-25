//! Parsing paste chords like `ctrl+shift+v` into wtype arguments.

use anyhow::{Result, bail};

/// Modifier names wtype accepts.
const MODIFIERS: &[(&str, &str)] = &[
    ("ctrl", "ctrl"),
    ("control", "ctrl"),
    ("shift", "shift"),
    ("alt", "alt"),
    ("altgr", "altgr"),
    ("super", "logo"),
    ("logo", "logo"),
    ("win", "logo"),
    ("meta", "logo"),
];

/// A parsed key combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// wtype modifier names, in the order given.
    pub modifiers: Vec<String>,
    /// The non-modifier key, as libxkbcommon names it.
    pub key: String,
}

impl Chord {
    /// Parse `ctrl+v`, `ctrl+shift+v`, `super+alt+Return`, …
    pub fn parse(spec: &str) -> Result<Self> {
        let mut modifiers = Vec::new();
        let mut key = None;

        for part in spec.split('+') {
            let part = part.trim();
            if part.is_empty() {
                bail!("empty component in key chord `{spec}`");
            }
            let lower = part.to_lowercase();
            match MODIFIERS.iter().find(|(name, _)| *name == lower) {
                Some((_, wtype_name)) => modifiers.push((*wtype_name).to_string()),
                None => {
                    if key.is_some() {
                        bail!("key chord `{spec}` names more than one non-modifier key");
                    }
                    // Single letters are lowercased; named keys such as
                    // `Return` or `Home` keep their case for libxkbcommon.
                    key = Some(if part.chars().count() == 1 {
                        lower
                    } else {
                        part.to_string()
                    });
                }
            }
        }

        let Some(key) = key else {
            bail!("key chord `{spec}` has no non-modifier key");
        };
        Ok(Self { modifiers, key })
    }

    /// Build the wtype argument vector that presses and releases this chord.
    ///
    /// Modifiers are released in reverse order, which is what a real keyboard
    /// does and what applications watching for key-up expect.
    pub fn to_wtype_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(self.modifiers.len() * 4 + 2);
        for m in &self.modifiers {
            args.push("-M".into());
            args.push(m.clone());
        }
        args.push("-k".into());
        args.push(self.key.clone());
        for m in self.modifiers.iter().rev() {
            args.push("-m".into());
            args.push(m.clone());
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ctrl_v() {
        let c = Chord::parse("ctrl+v").unwrap();
        assert_eq!(c.modifiers, vec!["ctrl"]);
        assert_eq!(c.key, "v");
    }

    #[test]
    fn parses_terminal_paste_chord() {
        let c = Chord::parse("ctrl+shift+v").unwrap();
        assert_eq!(c.modifiers, vec!["ctrl", "shift"]);
        assert_eq!(c.key, "v");
    }

    #[test]
    fn maps_super_aliases_to_logo() {
        for spec in ["super+v", "win+v", "meta+v", "logo+v"] {
            assert_eq!(Chord::parse(spec).unwrap().modifiers, vec!["logo"]);
        }
    }

    #[test]
    fn preserves_case_of_named_keys() {
        // libxkbcommon is case-sensitive: `Return` resolves, `return` does not.
        assert_eq!(Chord::parse("ctrl+Return").unwrap().key, "Return");
        assert_eq!(Chord::parse("V").unwrap().key, "v");
    }

    #[test]
    fn builds_balanced_press_and_release_args() {
        let args = Chord::parse("ctrl+shift+v").unwrap().to_wtype_args();
        assert_eq!(
            args,
            vec!["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"]
        );
    }

    #[test]
    fn every_pressed_modifier_is_released() {
        let args = Chord::parse("ctrl+alt+shift+x").unwrap().to_wtype_args();
        let pressed = args.iter().filter(|a| *a == "-M").count();
        let released = args.iter().filter(|a| *a == "-m").count();
        assert_eq!(pressed, released, "a stuck modifier would break the session");
        assert_eq!(pressed, 3);
    }

    #[test]
    fn rejects_malformed_chords() {
        assert!(Chord::parse("ctrl+").is_err());
        assert!(Chord::parse("ctrl").is_err(), "modifiers alone are not a chord");
        assert!(Chord::parse("ctrl+a+b").is_err(), "two real keys");
        assert!(Chord::parse("").is_err());
    }
}
