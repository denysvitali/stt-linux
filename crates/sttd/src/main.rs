//! `sttd` — the dictation daemon.
//!
//! The daemon exists for one reason: the ASR model takes seconds to load, so it
//! is loaded once and kept resident. Everything else here is in service of
//! that — the control socket, the state machine, the audio thread.

mod session;
mod socket;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use stt_ipc::{DaemonState, Request, Response, StatusReport, encode_line};

use session::{Outcome, Session};

#[derive(Parser)]
#[command(name = "sttd", version, about = "The stt-linux dictation daemon")]
struct Cli {
    /// Path to the config file. Defaults to the XDG location.
    #[arg(long)]
    config: Option<PathBuf>,
}

struct Daemon {
    /// `Arc` because `stop_async` hands the session to a worker thread.
    session: Arc<Session>,
    config_path: PathBuf,
    shutdown: AtomicBool,
    socket_path: PathBuf,
}

impl Daemon {
    /// Ask the accept loop to stop.
    ///
    /// Setting the flag is not enough: the main thread is parked inside
    /// `accept()` and will not look at it until *some* connection arrives. A
    /// throwaway self-connect is what actually wakes it.
    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
    }

    fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn status(&self) -> StatusReport {
        StatusReport {
            state: self.session.state(),
            engine: self.session.engine_name(),
            injector: Some(self.session.injector_name().to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            recording_ms: self.session.recording_ms(),
        }
    }

    fn handle(&self, request: Request) -> Response {
        match request {
            Request::Status => Response::Status(self.status()),

            Request::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                Response::Ok
            }

            Request::Start => match self.session.start() {
                Ok(()) => Response::Ok,
                Err(e) => err(e),
            },

            // Both reply as soon as the microphone is released. Transcription
            // continues on a worker thread, because a compositor keybind must
            // not block for the length of an inference.
            Request::Stop => match self.session.stop_async(report_result) {
                Ok(()) => Response::Ok,
                Err(e) => err(e),
            },

            Request::Toggle => match self.session.toggle(report_result) {
                Ok(()) => Response::Ok,
                Err(e) => err(e),
            },

            Request::Cancel => match self.session.cancel() {
                Ok(_) => Response::Ok,
                Err(e) => err(e),
            },

            Request::Reload => match self.session.reload(&self.config_path) {
                Ok(needs_restart) if needs_restart.is_empty() => Response::Ok,
                Ok(needs_restart) => Response::Error {
                    message: format!(
                        "reloaded, but these need a restart: {}",
                        needs_restart.join(", ")
                    ),
                },
                Err(e) => err(e),
            },
        }
    }
}

fn err(e: anyhow::Error) -> Response {
    Response::Error {
        message: format!("{e:#}"),
    }
}

fn report(outcome: &Outcome) {
    match outcome {
        Outcome::Injected { text, via } => {
            tracing::info!(via, chars = text.len(), "injected");
        }
        Outcome::ClipboardOnly { text, reason } => {
            tracing::warn!(chars = text.len(), reason, "clipboard only");
        }
        Outcome::NoSpeech => tracing::info!("no speech detected"),
        Outcome::Cancelled => tracing::info!("cancelled"),
    }
}

/// Callback for asynchronous transcription. The client has already been
/// answered by this point, so failures can only be logged.
fn report_result(result: Result<Outcome>) {
    match result {
        Ok(outcome) => report(&outcome),
        Err(e) => tracing::error!(error = %format!("{e:#}"), "dictation failed"),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // ONNX Runtime logs every arena allocation at INFO — hundreds
                // of lines per inference, which buries our own output.
                .unwrap_or_else(|_| "info,ort=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let config_path = match cli.config {
        Some(p) => p,
        None => stt_core::paths::config_file()?,
    };
    let config = stt_core::Config::load_from(&config_path)?;
    tracing::info!(config = %config_path.display(), "loaded configuration");

    // Probe the session once at startup so failures surface here rather than
    // mid-dictation.
    let globals = stt_core::wayland::Globals::probe().ok();
    let (injector, probe) = stt_inject::build(&config.inject, globals.as_ref())?;
    let injector_name = probe.backend_name().to_string();
    tracing::info!(backend = %injector_name, detail = %probe.detail, "selected injector");

    // The whole point of the daemon: pay the model load cost once.
    let engine = stt_core::engine::load(&config.engine)?;

    let overlay = build_overlay(&config);

    let socket_path = stt_core::paths::socket_path()?;
    let daemon = Arc::new(Daemon {
        session: Arc::new(Session::new(
            config,
            engine,
            injector,
            injector_name,
            overlay,
        )),
        config_path,
        shutdown: AtomicBool::new(false),
        socket_path: socket_path.clone(),
    });

    let socket = socket::ControlSocket::bind(&socket_path)?;
    tracing::info!(path = %socket.path().display(), state = %DaemonState::Ready, "listening");

    // Without this, SIGTERM (which is how systemd stops us) kills the process
    // outright and `ControlSocket`'s destructor never runs, leaving a stale
    // socket behind.
    install_signal_handlers(Arc::clone(&daemon))?;
    spawn_duration_guard(Arc::clone(&daemon));
    spawn_overlay_pump(Arc::clone(&daemon));
    spawn_preview(Arc::clone(&daemon));

    for stream in socket.listener.incoming() {
        // Checked before dispatching, so the self-connect that woke us is not
        // served as a real request.
        if daemon.is_shutting_down() {
            break;
        }
        match stream {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                // One thread per connection: commands are rare, and a
                // long-running `stop` must not block `stt status`.
                std::thread::spawn(move || {
                    if let Err(e) = serve(&daemon, stream) {
                        tracing::warn!(error = %format!("{e:#}"), "connection failed");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }

    tracing::info!("shutting down");
    // `socket` drops here, removing the socket file.
    Ok(())
}

/// Start the recording overlay, or explain why there isn't one.
///
/// Never fatal: a missing layer-shell or a headless session means no visual
/// feedback, not a broken dictation daemon.
fn build_overlay(config: &stt_core::Config) -> Option<stt_overlay::Overlay> {
    if !config.overlay.enabled {
        tracing::info!("overlay disabled in config");
        return None;
    }
    let anchor = match config.overlay.anchor {
        stt_core::config::OverlayAnchor::Top => stt_overlay::OverlayAnchor::Top,
        stt_core::config::OverlayAnchor::Bottom => stt_overlay::OverlayAnchor::Bottom,
        stt_core::config::OverlayAnchor::Center => stt_overlay::OverlayAnchor::Center,
    };
    match stt_overlay::Overlay::spawn(anchor) {
        Ok(o) => {
            tracing::info!(?anchor, "overlay ready");
            Some(o)
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "no overlay; dictation will work without visual feedback"
            );
            None
        }
    }
}

/// Feed the input level to the overlay while recording.
///
/// 30 Hz: fast enough that the meter tracks speech, slow enough to be free.
fn spawn_overlay_pump(daemon: Arc<Daemon>) {
    std::thread::Builder::new()
        .name("stt-overlay-pump".into())
        .spawn(move || {
            while !daemon.is_shutting_down() {
                daemon.session.pump_overlay();
                std::thread::sleep(std::time::Duration::from_millis(33));
            }
        })
        .expect("spawning the overlay pump");
}

/// Show a live transcript in the overlay while the user speaks.
///
/// Runs on its own thread because a preview pass is a full inference — at the
/// measured ~27x realtime a 10-second buffer costs about 370 ms, which must
/// not be anywhere near the socket handler or the audio path.
///
/// The interval is measured *between* passes rather than as a fixed rate, so
/// as the buffer grows and each pass takes longer the previews naturally space
/// themselves out instead of piling up.
fn spawn_preview(daemon: Arc<Daemon>) {
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(1200);

    std::thread::Builder::new()
        .name("stt-preview".into())
        .spawn(move || {
            while !daemon.is_shutting_down() {
                if daemon.session.is_recording() {
                    let started = std::time::Instant::now();
                    if daemon.session.preview() {
                        tracing::debug!(ms = started.elapsed().as_millis() as u64, "preview");
                    }
                    std::thread::sleep(INTERVAL);
                } else {
                    // Idle: cheap poll waiting for a recording to start.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        })
        .expect("spawning the preview thread");
}

/// Stop a recording that has run past `audio.max_duration_secs`.
///
/// A push-to-talk bind whose key-release never fires — a dropped `bindr`, a
/// crashed compositor reload — would otherwise record until memory runs out.
/// Stopping transcribes what was captured rather than discarding it, since the
/// user did say something.
fn spawn_duration_guard(daemon: Arc<Daemon>) {
    std::thread::Builder::new()
        .name("stt-duration-guard".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if daemon.is_shutting_down() {
                    return;
                }
                let max_ms = daemon.session.max_duration_secs() as u64 * 1000;
                if max_ms == 0 {
                    continue;
                }
                let Some(elapsed) = daemon.session.recording_ms() else {
                    continue;
                };
                if elapsed >= max_ms {
                    tracing::warn!(
                        elapsed_ms = elapsed,
                        max_ms,
                        "recording exceeded audio.max_duration_secs; stopping it"
                    );
                    if let Err(e) = daemon.session.stop_async(report_result) {
                        tracing::warn!(error = %format!("{e:#}"), "guard stop failed");
                    }
                }
            }
        })
        .expect("spawning the duration guard");
}

fn install_signal_handlers(daemon: Arc<Daemon>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    std::thread::spawn(move || {
        // The first signal is enough — after it we are already tearing down,
        // and a second one should hit the default handler and kill us outright
        // rather than be swallowed here.
        if let Some(signal) = signals.forever().next() {
            tracing::info!(signal, "received signal");
            daemon.request_shutdown();
        }
    });
    Ok(())
}

/// Serve newline-delimited requests until the peer hangs up.
fn serve(daemon: &Daemon, stream: UnixStream) -> Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                tracing::debug!(?req, "request");
                daemon.handle(req)
            }
            Err(e) => Response::Error {
                message: format!("malformed request: {e}"),
            },
        };
        writer.write_all(encode_line(&response)?.as_bytes())?;
        writer.flush()?;

        // Only now that the reply is on the wire is it safe to unblock the
        // accept loop and let the process exit.
        if daemon.is_shutting_down() {
            daemon.request_shutdown();
            break;
        }
    }
    Ok(())
}
