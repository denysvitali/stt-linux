//! Control socket lifecycle.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// A listener that removes its socket file on drop.
pub struct ControlSocket {
    pub listener: UnixListener,
    path: PathBuf,
}

impl ControlSocket {
    /// Bind the control socket, clearing a stale file left by a crashed daemon.
    ///
    /// Unix sockets are not cleaned up by the kernel when a process dies, so a
    /// leftover file is the normal case after a crash — but we must
    /// distinguish that from a second daemon that is genuinely running, or
    /// we would silently steal its socket.
    pub fn bind(path: &Path) -> Result<Self> {
        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_) => bail!(
                    "another sttd is already listening on {}",
                    path.display()
                ),
                Err(_) => {
                    tracing::warn!(path = %path.display(), "removing stale socket");
                    std::fs::remove_file(path)
                        .with_context(|| format!("removing stale socket {}", path.display()))?;
                }
            }
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(path)
            .with_context(|| format!("binding control socket {}", path.display()))?;

        // The socket accepts commands that synthesize keystrokes; keep it
        // owner-only even if XDG_RUNTIME_DIR is unusually permissive.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", path.display()))?;

        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), error = %e, "could not remove socket");
            }
        }
    }
}
