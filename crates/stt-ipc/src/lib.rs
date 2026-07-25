//! Wire protocol for the daemon's control socket.
//!
//! Newline-delimited JSON in both directions: one [`Request`] per line, one
//! [`Response`] per line. Line-delimited JSON keeps the CLI trivial (write a
//! line, read a line) and stays debuggable with `socat`.

use serde::{Deserialize, Serialize};

/// Commands the CLI and activation backends send to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Begin recording. A no-op if already recording, so a repeated key-down
    /// from a stuck compositor bind cannot open a second stream.
    Start,
    /// Stop recording and transcribe.
    Stop,
    /// Start if idle, stop if recording.
    Toggle,
    /// Abandon the current recording or transcription; inject nothing.
    Cancel,
    /// Query daemon state.
    Status,
    /// Re-read the config file without restarting. Engine changes still need a
    /// restart; this is reported back in the response.
    Reload,
    /// Ask the daemon to exit cleanly.
    Shutdown,
}

/// What the daemon is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    /// Model is still loading; commands are accepted but recording will wait.
    Loading,
    Ready,
    Recording,
    Transcribing,
    Injecting,
}

impl DaemonState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Recording => "recording",
            Self::Transcribing => "transcribing",
            Self::Injecting => "injecting",
        }
    }
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(StatusReport),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub state: DaemonState,
    pub engine: String,
    /// Selected injector, or `None` while still probing.
    pub injector: Option<String>,
    pub version: String,
    /// Length of the in-progress recording, if any.
    pub recording_ms: Option<u64>,
}

/// Messages the daemon pushes to the overlay process over its stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayMessage {
    /// Current input amplitude in `[0.0, 1.0]`, sent at ~30 Hz.
    Level { value: f32 },
    /// Recording ended; the overlay switches to a "transcribing" indicator.
    Transcribing,
}

/// Encode one protocol message as a single newline-terminated line.
pub fn encode_line<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for req in [
            Request::Start,
            Request::Stop,
            Request::Toggle,
            Request::Cancel,
            Request::Status,
            Request::Reload,
            Request::Shutdown,
        ] {
            let line = encode_line(&req).unwrap();
            assert!(line.ends_with('\n'));
            assert_eq!(serde_json::from_str::<Request>(line.trim()).unwrap(), req);
        }
    }

    #[test]
    fn request_wire_format_is_stable() {
        // The CLI is a separate binary that may be upgraded independently of
        // the daemon, so this shape is a compatibility surface.
        assert_eq!(
            serde_json::to_string(&Request::Toggle).unwrap(),
            r#"{"cmd":"toggle"}"#
        );
    }

    #[test]
    fn responses_round_trip() {
        let report = Response::Status(StatusReport {
            state: DaemonState::Recording,
            engine: "parakeet-tdt-0.6b-v3".into(),
            injector: Some("wtype".into()),
            version: "0.1.0".into(),
            recording_ms: Some(2400),
        });
        let line = encode_line(&report).unwrap();
        assert_eq!(serde_json::from_str::<Response>(line.trim()).unwrap(), report);

        let err = Response::Error {
            message: "no speech detected".into(),
        };
        let line = encode_line(&err).unwrap();
        assert_eq!(serde_json::from_str::<Response>(line.trim()).unwrap(), err);
    }

    #[test]
    fn overlay_messages_round_trip() {
        let msg = OverlayMessage::Level { value: 0.42 };
        let line = encode_line(&msg).unwrap();
        assert_eq!(
            serde_json::from_str::<OverlayMessage>(line.trim()).unwrap(),
            msg
        );
    }

    #[test]
    fn encoded_lines_contain_no_interior_newlines() {
        // Framing depends on this: a newline inside a message would desync
        // the reader.
        let line = encode_line(&Response::Error {
            message: "multi\nline\nerror".into(),
        })
        .unwrap();
        assert_eq!(line.matches('\n').count(), 1);
    }
}
