//! Direct typing via `wtype` and the virtual-keyboard protocol.

use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::Injector;

pub struct WtypeInjector {
    binary: std::path::PathBuf,
}

impl WtypeInjector {
    pub fn new(binary: std::path::PathBuf) -> Self {
        Self { binary }
    }

    /// Arguments for typing `text` literally.
    ///
    /// The `--` matters: a transcript beginning with a hyphen would otherwise
    /// be parsed as options. wtype documents
    /// `wtype [OPTION_OR_TEXT]... -- [TEXT]...` for exactly this.
    pub fn type_args(text: &str) -> Vec<String> {
        vec!["--".to_string(), text.to_string()]
    }
}

impl Injector for WtypeInjector {
    fn name(&self) -> &'static str {
        "wtype"
    }

    fn inject(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        run(&self.binary, &WtypeInjector::type_args(text))
    }
}

/// Run wtype and turn a non-zero exit into a useful error.
pub(crate) fn run(binary: &std::path::Path, args: &[String]) -> Result<()> {
    let output = Command::new(binary)
        .args(args)
        .output()
        .with_context(|| format!("spawning {}", binary.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} failed ({}): {}",
            binary.display(),
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_separated_from_options() {
        assert_eq!(
            WtypeInjector::type_args("hello"),
            vec!["--".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn leading_hyphen_is_not_treated_as_a_flag() {
        // "-M ctrl" as dictated text must be typed, not executed as options.
        let args = WtypeInjector::type_args("-M ctrl");
        assert_eq!(args[0], "--");
        assert_eq!(args[1], "-M ctrl");
    }

    #[test]
    fn text_is_passed_as_one_argument() {
        // Passing via argv (not a shell string) means no quoting or escaping
        // can go wrong, whatever the transcript contains.
        let nasty = "a\"b'c$d`e;f\ng\\h";
        let args = WtypeInjector::type_args(nasty);
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], nasty);
    }

    #[test]
    fn unicode_survives_argument_construction() {
        let text = "café → naïve 日本語 🎤";
        assert_eq!(WtypeInjector::type_args(text)[1], text);
    }
}
