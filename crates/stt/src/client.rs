//! Blocking client for the daemon's control socket.
//!
//! This runs on every hotkey press, so it stays deliberately thin: connect,
//! write one line, read one line, exit. No async runtime, no retries.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use stt_ipc::{Request, Response, StatusReport, encode_line};

/// Generous enough to cover a busy daemon, short enough that a wedged daemon
/// does not hang a keybind forever.
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn send(request: &Request) -> Result<Response> {
    let path = stt_core::paths::socket_path()?;
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "cannot reach the daemon at {} (is `sttd` running?)",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;

    let mut writer = stream.try_clone()?;
    writer
        .write_all(encode_line(request)?.as_bytes())
        .context("writing request")?;
    writer.flush()?;

    let mut line = String::new();
    let n = BufReader::new(stream)
        .read_line(&mut line)
        .context("reading response")?;
    if n == 0 {
        bail!("daemon closed the connection without replying");
    }
    serde_json::from_str(line.trim())
        .with_context(|| format!("malformed response: {}", line.trim()))
}

/// Send a request, turning a protocol-level `Error` reply into an `Err`.
pub fn send_ok(request: &Request) -> Result<Response> {
    match send(request)? {
        Response::Error { message } => bail!("{message}"),
        other => Ok(other),
    }
}

pub fn status() -> Result<StatusReport> {
    match send_ok(&Request::Status)? {
        Response::Status(s) => Ok(s),
        other => bail!("expected a status reply, got {other:?}"),
    }
}
