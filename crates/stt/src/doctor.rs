//! `stt doctor` — one command that explains what this session can and cannot do.
//!
//! Because the viable strategies differ per compositor, most bug reports for a
//! tool like this are environment problems. Doctor exists so that the answer is
//! one paste away, and so a broken audio path is never mistaken for a broken
//! model.

use anyhow::Result;
use std::fmt::Write as _;
use stt_core::{Config, audio, paths, wayland};

const OK: &str = "\x1b[32m✓\x1b[0m";
const BAD: &str = "\x1b[31m✗\x1b[0m";
const WARN: &str = "\x1b[33m!\x1b[0m";

fn mark(good: bool) -> &'static str {
    if good { OK } else { BAD }
}

/// Renders the full report. Returns the text plus whether the session looks
/// usable, so the caller can set a non-zero exit code for scripting.
pub fn report() -> Result<(String, bool)> {
    let mut s = String::new();
    let mut usable = true;

    writeln!(s, "stt-linux {}\n", env!("CARGO_PKG_VERSION"))?;

    // ---- session -------------------------------------------------------
    writeln!(s, "Session")?;
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into());
    let on_wayland = wayland::is_wayland();
    writeln!(s, "  {} type        {session_type}", mark(on_wayland))?;
    writeln!(s, "  {OK} compositor  {}", wayland::compositor_name())?;

    let globals = match wayland::Globals::probe() {
        Ok(g) => {
            writeln!(s, "  {OK} registry    {} globals advertised", g.globals.len())?;
            Some(g)
        }
        Err(e) => {
            usable = false;
            writeln!(s, "  {BAD} registry    {e}")?;
            None
        }
    };

    if let Some(g) = &globals {
        writeln!(s, "\nWayland protocols")?;
        for (iface, why) in [
            (wayland::VIRTUAL_KEYBOARD, "wtype text injection"),
            (wayland::LAYER_SHELL, "recording overlay"),
            (wayland::INPUT_METHOD, "input-method injection"),
            (wayland::DATA_CONTROL, "clipboard access"),
        ] {
            match g.version_of(iface) {
                Some(v) => writeln!(s, "  {OK} {iface} v{v} — {why}")?,
                None => writeln!(s, "  {WARN} {iface} absent — {why} unavailable")?,
            }
        }
    }

    // ---- config --------------------------------------------------------
    writeln!(s, "\nConfiguration")?;
    let config_path = paths::config_file()?;
    let config = match Config::load_from(&config_path) {
        Ok(c) => {
            let note = if config_path.exists() {
                config_path.display().to_string()
            } else {
                format!("{} (absent, using defaults)", config_path.display())
            };
            writeln!(s, "  {OK} config      {note}")?;
            c
        }
        Err(e) => {
            usable = false;
            writeln!(s, "  {BAD} config      {e:#}")?;
            // A broken config must not hide the rest of the report.
            Config::default()
        }
    };
    writeln!(s, "  {OK} models dir  {}", paths::models_dir()?.display())?;
    writeln!(s, "  {OK} socket      {}", paths::socket_path()?.display())?;

    // ---- model ---------------------------------------------------------
    writeln!(s, "\nModel")?;
    let model_dir = config.engine.resolved_model_dir()?;
    if model_dir.is_dir() {
        let files: Vec<_> = std::fs::read_dir(&model_dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        writeln!(s, "  {OK} {:?} at {}", config.engine.backend, model_dir.display())?;
        if files.is_empty() {
            usable = false;
            writeln!(s, "  {BAD} directory is empty — run `stt model download`")?;
        } else {
            let mut sorted = files;
            sorted.sort();
            writeln!(s, "  {OK} files       {}", sorted.join(", "))?;
        }
    } else {
        usable = false;
        writeln!(
            s,
            "  {BAD} not found at {} — run `stt model download`",
            model_dir.display()
        )?;
    }

    // ---- audio ---------------------------------------------------------
    writeln!(s, "\nAudio input")?;
    match audio::list_input_devices() {
        Ok(found) => {
            // What we would *actually* open, which is not necessarily the host
            // default — see `audio::default_input_device`.
            match audio::describe_default_input() {
                Some(d) => {
                    let cfg = d.default_config.as_deref().unwrap_or("unknown format");
                    writeln!(
                        s,
                        "  {OK} will record from `{}` — {cfg}",
                        d.id.as_deref().unwrap_or(&d.name)
                    )?;
                    if d.id.as_deref() == Some(audio::PIPEWIRE_PCM) {
                        writeln!(
                            s,
                            "      (preferring the PipeWire PCM; ALSA's `default` often \
                             captures silence here)"
                        )?;
                    }
                }
                None => {
                    usable = false;
                    writeln!(s, "  {BAD} no usable input device")?;
                }
            }
            if let Some(d) = &found.default {
                writeln!(
                    s,
                    "  {OK} host default {} ({})",
                    d.name,
                    d.id.as_deref().unwrap_or("no id")
                )?;
            }
            if found.devices.is_empty() {
                usable = false;
                writeln!(s, "  {BAD} no input devices enumerated")?;
            } else {
                writeln!(s, "  {OK} {} other devices selectable:", found.devices.len())?;
                for d in &found.devices {
                    let cfg = d.default_config.as_deref().unwrap_or("unknown format");
                    writeln!(
                        s,
                        "      {:<28} {cfg}",
                        d.id.as_deref().unwrap_or(d.name.as_str())
                    )?;
                }
            }
        }
        Err(e) => {
            usable = false;
            writeln!(s, "  {BAD} {e:#}")?;
        }
    }

    // ---- injection -----------------------------------------------------
    writeln!(s, "\nText injection (configured order: {:?})", config.inject.backends)?;
    let inject_probes = stt_inject::probe_all(globals.as_ref());
    let mut chosen = None;
    for backend in &config.inject.backends {
        if let Some(p) = inject_probes.iter().find(|p| p.backend == *backend)
            && p.is_usable()
        {
            chosen = Some(*backend);
            break;
        }
    }
    for p in &inject_probes {
        let selected = Some(p.backend) == chosen;
        let bullet = if selected { "→" } else { " " };
        writeln!(
            s,
            "  {bullet} {} {:?}: {}",
            p.availability.glyph(),
            p.backend,
            p.detail
        )?;
    }
    match chosen {
        Some(b) => writeln!(s, "  would use: {b:?}")?,
        None => {
            usable = false;
            writeln!(
                s,
                "  {BAD} no configured injector is available — transcripts could \
                 only reach the clipboard"
            )?;
        }
    }

    // ---- activation ----------------------------------------------------
    writeln!(s, "\nActivation")?;
    for p in stt_activate::probe_all() {
        writeln!(
            s,
            "  {} {:?}: {}",
            p.availability.glyph(),
            p.backend,
            p.detail
        )?;
    }
    if !config
        .activation
        .backends
        .contains(&stt_core::config::ActivationBackend::Socket)
    {
        writeln!(
            s,
            "  {WARN} the socket backend is disabled in config; it is the only \
             backend that works everywhere"
        )?;
    }
    writeln!(s, "\n  Bind these in your compositor:")?;
    writeln!(s, "    bind  = SUPER, D, exec, stt toggle      # toggle mode")?;
    writeln!(s, "    bind  = SUPER, D, exec, stt start       # push-to-talk")?;
    writeln!(s, "    bindr = SUPER, D, exec, stt stop")?;

    // ---- daemon --------------------------------------------------------
    writeln!(s, "\nDaemon")?;
    match crate::client::status() {
        Ok(st) => writeln!(
            s,
            "  {OK} running — state {}, engine {}",
            st.state, st.engine
        )?,
        Err(e) => writeln!(s, "  {WARN} not reachable: {e}")?,
    }

    Ok((s, usable))
}
