//! XDG base-directory resolution for config, models and the IPC socket.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Directory name used under every XDG base directory.
pub const APP_DIR: &str = "stt-linux";

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", APP_DIR)
        .context("could not determine XDG base directories")
}

/// `~/.config/stt-linux/`
pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// `~/.config/stt-linux/config.toml`
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// `~/.local/share/stt-linux/`
pub fn data_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

/// `~/.local/share/stt-linux/models/`
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("models"))
}

/// The daemon's control socket.
///
/// Lives in `$XDG_RUNTIME_DIR` so the kernel cleans it up on logout and so it
/// inherits that directory's 0700 permissions — the socket accepts commands
/// that inject keystrokes, so it must never be world-writable.
pub fn socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is unset; cannot place the control socket safely")?;
    Ok(runtime.join(format!("{APP_DIR}.sock")))
}
