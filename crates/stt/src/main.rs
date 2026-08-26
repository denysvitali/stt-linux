//! `stt` — the command-line face of stt-linux.
//!
//! Most subcommands are one round-trip to the daemon. This binary is what a
//! compositor keybind actually executes, so startup cost matters: no async
//! runtime, no model loading, no config parsing on the hot path.

mod client;
mod doctor;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::time::Duration;
use stt_ipc::{Request, Response};

#[derive(Parser)]
#[command(
    name = "stt",
    version,
    about = "Local speech-to-text dictation for Wayland",
    long_about = "Controls the stt-linux dictation daemon.\n\n\
                  Bind `stt toggle` (or `stt start` / `stt stop` for \
                  push-to-talk) to a key in your compositor."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Begin recording.
    Start,
    /// Stop recording, transcribe and inject.
    Stop,
    /// Start if idle, stop if recording.
    Toggle,
    /// Abandon the current recording without injecting.
    Cancel,
    /// Show what the daemon is doing.
    Status {
        /// Emit JSON instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },
    /// Re-read the config file without restarting the daemon.
    Reload,
    /// Ask the daemon to exit.
    Shutdown,
    /// Diagnose this session: compositor, protocols, audio, model, backends.
    Doctor,
    /// Record from the microphone straight to a 16 kHz mono WAV.
    ///
    /// Bypasses the daemon entirely, so it verifies the capture path on its
    /// own — if this produces good audio, a bad transcript is the model's
    /// fault, not the microphone's.
    Record {
        /// Output WAV path.
        #[arg(long, short)]
        out: std::path::PathBuf,
        /// Stop after this many seconds instead of waiting for Enter.
        #[arg(long, short)]
        seconds: Option<f32>,
        /// Input device: `default`, a device id, or a device name.
        #[arg(long, short)]
        device: Option<String>,
    },
    /// Transcribe a WAV file. Loads the model each time, so it is slower than
    /// dictating through the daemon — this is for verification, not daily use.
    Transcribe {
        /// WAV file to transcribe. Any rate/channel count; converted as needed.
        file: std::path::PathBuf,
        /// Also print timing and realtime factor.
        #[arg(long)]
        bench: bool,
    },
    /// Download or inspect local ASR models.
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Inspect or create the config file.
    Config {
        /// Write a fully-populated default config, if none exists.
        #[arg(long)]
        init: bool,
        /// Print the config file path and exit.
        #[arg(long)]
        path: bool,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// List known models and whether they are present locally.
    List,
    /// Fetch a model from Hugging Face.
    Download {
        /// Model directory name. Defaults to the one named in the config.
        name: Option<String>,
    },
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("stt: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<std::process::ExitCode> {
    let cli = Cli::parse();
    let success = std::process::ExitCode::SUCCESS;

    // Quiet by default — this runs on every keypress. `RUST_LOG=info` opts in.
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    }

    match cli.command {
        Command::Start => simple(Request::Start)?,
        Command::Stop => simple(Request::Stop)?,
        Command::Toggle => simple(Request::Toggle)?,
        Command::Cancel => simple(Request::Cancel)?,
        Command::Reload => simple(Request::Reload)?,
        Command::Shutdown => simple(Request::Shutdown)?,

        Command::Status { json } => {
            let status = client::status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print!("{} · {}", status.state, status.engine);
                if let Some(ms) = status.recording_ms {
                    print!(" · {:.1}s", ms as f64 / 1000.0);
                }
                if let Some(inj) = &status.injector {
                    print!(" · via {inj}");
                }
                println!();
            }
        }

        Command::Doctor => {
            let (text, usable) = doctor::report()?;
            print!("{text}");
            if !usable {
                eprintln!("\nSome checks failed; dictation may not work yet.");
                return Ok(std::process::ExitCode::FAILURE);
            }
        }

        Command::Record {
            out,
            seconds,
            device,
        } => {
            let config = stt_core::Config::load()?;
            let selector = device.as_deref().unwrap_or(&config.audio.device);
            record(&out, seconds, selector)?;
        }

        Command::Transcribe { file, bench } => {
            transcribe(&file, bench)?;
        }

        Command::Model { action } => match action {
            ModelAction::List => model_list()?,
            ModelAction::Download { name } => model_download(name.as_deref())?,
        },

        Command::Config { init, path } => {
            let file = stt_core::paths::config_file()?;
            if path {
                println!("{}", file.display());
                return Ok(success);
            }
            if init {
                if file.exists() {
                    anyhow::bail!("{} already exists; not overwriting", file.display());
                }
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&file, stt_core::Config::default().to_toml()?)?;
                println!("wrote {}", file.display());
                return Ok(success);
            }
            // Default: show the effective config, defaults included.
            print!("{}", stt_core::Config::load_from(&file)?.to_toml()?);
        }
    }

    Ok(success)
}

/// Peak amplitude below which a recording is treated as silence. Room tone on
/// a working microphone sits well above this; a muted source sits below it.
const SILENCE_PEAK: f32 = 0.005;

/// Record to a WAV file, showing a live level meter.
fn record(out: &std::path::Path, seconds: Option<f32>, selector: &str) -> Result<()> {
    use std::io::Write as _;

    let recording = stt_core::capture::Recording::start(selector)?;
    eprintln!(
        "Recording from `{selector}` at {} Hz, {} ch → 16 kHz mono",
        recording.input_rate(),
        recording.channels()
    );
    match seconds {
        Some(s) => eprintln!("Stopping after {s:.1}s."),
        None => eprintln!("Press Enter to stop."),
    }

    // Poll the level rather than block, so the meter animates either way.
    let deadline = seconds.map(|s| std::time::Instant::now() + Duration::from_secs_f32(s));
    let stdin_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if seconds.is_none() {
        let flag = std::sync::Arc::clone(&stdin_done);
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            flag.store(true, std::sync::atomic::Ordering::Release);
        });
    }

    loop {
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            break;
        }
        if stdin_done.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let level = recording.level();
        let bars = (level * 60.0).min(30.0) as usize;
        eprint!(
            "\r  {:5.1}s [{:<30}] {:.3}",
            recording.elapsed().as_secs_f32(),
            "#".repeat(bars),
            level
        );
        let _ = std::io::stderr().flush();
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!();

    let dropped = recording.dropped_frames();
    let samples = recording.stop()?;
    if dropped > 0 {
        eprintln!("warning: dropped {dropped} frames — capture could not keep up");
    }
    if samples.is_empty() {
        anyhow::bail!("captured no audio at all");
    }

    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    stt_core::wav::write_mono_16k(out, &samples)?;
    println!(
        "wrote {} ({:.2}s, {} samples at 16 kHz, peak {:.3})",
        out.display(),
        stt_core::wav::duration_secs(&samples),
        samples.len(),
        peak
    );

    // Silence is the failure that masquerades as a bad model, so name it
    // explicitly and say what to check.
    if peak < SILENCE_PEAK {
        eprintln!(
            "\nwarning: that is effectively silence (peak {peak:.4}).\n\
             Check that the source is unmuted and turned up:\n  \
             pactl get-source-mute @DEFAULT_SOURCE@\n  \
             pactl set-source-volume @DEFAULT_SOURCE@ 100%\n\
             Or pick another device with --device (see `stt doctor`)."
        );
    }
    Ok(())
}

fn transcribe(file: &std::path::Path, bench: bool) -> Result<()> {
    let config = stt_core::Config::load()?;

    let read_start = std::time::Instant::now();
    let pcm = stt_core::wav::read_as_mono_16k(file)?;
    let audio_secs = stt_core::wav::duration_secs(&pcm);
    anyhow::ensure!(!pcm.is_empty(), "{} contains no audio", file.display());

    let load_start = std::time::Instant::now();
    let mut engine = stt_core::engine::load(&config.engine)?;
    let load_secs = load_start.elapsed().as_secs_f32();

    let infer_start = std::time::Instant::now();
    let transcript = engine.transcribe(&pcm)?;
    let infer_secs = infer_start.elapsed().as_secs_f32();

    println!("{}", transcript.text);

    if bench {
        eprintln!(
            "\n  audio      {audio_secs:.2}s\n  \
             read       {:.2}s\n  \
             model load {load_secs:.2}s\n  \
             inference  {infer_secs:.2}s\n  \
             realtime   {:.1}x  (inference only)",
            read_start.elapsed().as_secs_f32() - load_secs - infer_secs,
            audio_secs / infer_secs.max(1e-6),
        );
    }
    Ok(())
}

fn model_list() -> Result<()> {
    let root = stt_core::paths::models_dir()?;
    println!("models directory: {}\n", root.display());
    for spec in stt_core::model::ALL_MODELS {
        let dir = root.join(spec.dir_name);
        let missing = spec.missing_files(&dir);
        let state = if missing.is_empty() {
            "installed".to_string()
        } else if missing.len() == spec.files.len() {
            "not downloaded".to_string()
        } else {
            format!(
                "incomplete ({} of {} files)",
                missing.len(),
                spec.files.len()
            )
        };
        println!(
            "  {:<28} {:>7}  {state}\n      {}",
            spec.dir_name,
            human_bytes(spec.total_bytes()),
            spec.description
        );
    }
    Ok(())
}

fn model_download(name: Option<&str>) -> Result<()> {
    let name = match name {
        Some(n) => n.to_string(),
        None => stt_core::Config::load()?.engine.model_dir,
    };
    let spec = stt_core::model::spec_by_name(&name).with_context(|| {
        format!(
            "unknown model `{name}`; known models: {}",
            stt_core::model::ALL_MODELS
                .iter()
                .map(|m| m.dir_name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let dir = stt_core::paths::models_dir()?.join(spec.dir_name);
    if spec.is_complete(&dir) {
        println!(
            "{} is already installed at {}",
            spec.dir_name,
            dir.display()
        );
        return Ok(());
    }

    println!(
        "Downloading {} (~{}) into {}",
        spec.description,
        human_bytes(spec.total_bytes()),
        dir.display()
    );

    let bars = indicatif::MultiProgress::new();
    let style = indicatif::ProgressStyle::with_template(
        "  {msg:32} [{bar:28}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec}",
    )
    .unwrap()
    .progress_chars("=> ");

    let mut current: Option<(String, indicatif::ProgressBar)> = None;
    let mut progress = |file: &str, done: u64, total: Option<u64>| {
        // One bar per file, created lazily once the real size is known from
        // the response headers rather than our estimate.
        if current.as_ref().is_none_or(|(n, _)| n != file) {
            if let Some((_, bar)) = current.take() {
                bar.finish();
            }
            let bar = bars.add(indicatif::ProgressBar::new(total.unwrap_or(0)));
            bar.set_style(style.clone());
            bar.set_message(file.to_string());
            current = Some((file.to_string(), bar));
        }
        if let Some((_, bar)) = &current {
            if let Some(t) = total {
                bar.set_length(t);
            }
            bar.set_position(done);
        }
    };

    let dir = stt_core::model::download(spec, &mut progress)?;
    if let Some((_, bar)) = current.take() {
        bar.finish();
    }
    println!("\ninstalled to {}", dir.display());
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.0} {}", UNITS[u])
}

/// Send a fire-and-forget command and report anything the daemon says back.
fn simple(request: Request) -> Result<()> {
    match client::send_ok(&request)? {
        Response::Ok => Ok(()),
        Response::Status(s) => {
            println!("{}", s.state);
            Ok(())
        }
        Response::Error { message } => anyhow::bail!("{message}"),
    }
}
