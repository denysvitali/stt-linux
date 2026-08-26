//! The dictation state machine.
//!
//! ```text
//!   Idle ──start──► Recording ──stop──► Transcribing ──► Injecting ──► Idle
//!     ▲                 │                    │                          │
//!     └────── cancel ───┴────────────────────┘                          │
//!     └──────────────────────────────────────────────────────────────────
//! ```
//!
//! All transitions go through one mutex-guarded struct. The invariant that
//! matters: a second `start` while already recording is a no-op, not a second
//! capture stream — a compositor `bindr` that misfires would otherwise open
//! two microphones and lose the first recording.

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use stt_core::capture::Recording;
use stt_core::{Config, Engine};
use stt_inject::{Focus, Injector};
use stt_ipc::DaemonState;

/// What a completed dictation did, for logging and notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Injected {
        text: String,
        via: String,
    },
    /// Focus moved while we were transcribing, so the text went to the
    /// clipboard instead of into the wrong window.
    ClipboardOnly {
        text: String,
        reason: String,
    },
    /// The recording contained no speech.
    NoSpeech,
    Cancelled,
}

/// Live recording state, held only while recording.
struct Active {
    recording: Recording,
    /// Focus at the moment recording began — the window the user meant.
    focus: Focus,
    started: Instant,
}

pub struct Session {
    state: Mutex<Inner>,
    engine: Mutex<Box<dyn Engine>>,
    injector: Mutex<Box<dyn Injector>>,
    injector_name: String,
    config: Mutex<Config>,
    /// Absent when the compositor has no layer-shell, or the user turned it
    /// off. Dictation works either way.
    overlay: Option<stt_overlay::Overlay>,
}

struct Inner {
    state: DaemonState,
    active: Option<Active>,
}

impl Session {
    pub fn new(
        config: Config,
        engine: Box<dyn Engine>,
        injector: Box<dyn Injector>,
        injector_name: String,
        overlay: Option<stt_overlay::Overlay>,
    ) -> Self {
        Self {
            state: Mutex::new(Inner {
                state: DaemonState::Ready,
                active: None,
            }),
            engine: Mutex::new(engine),
            injector: Mutex::new(injector),
            injector_name,
            config: Mutex::new(config),
            overlay,
        }
    }

    /// Current input amplitude, for the overlay meter. Zero when not recording.
    pub fn level(&self) -> f32 {
        let inner = self.state.lock().unwrap();
        inner.active.as_ref().map_or(0.0, |a| a.recording.level())
    }

    /// Push the current level to the overlay. Called from the daemon's pump.
    pub fn pump_overlay(&self) {
        if let Some(overlay) = &self.overlay
            && self.is_recording()
        {
            overlay.level(self.level());
        }
    }

    /// Transcribe the audio captured so far and show it in the overlay.
    ///
    /// This is the live-preview pass. It re-runs the *same* engine over the
    /// growing buffer rather than using a separate streaming model, so the
    /// words shown while speaking can never disagree with the text finally
    /// injected — and it costs no extra download or resident memory. The price
    /// is that each pass re-transcribes from the beginning, so cost grows with
    /// the length of the recording; [`PREVIEW_MAX_SECS`] bounds it.
    ///
    /// Returns `false` when the pass was skipped.
    pub fn preview(&self) -> bool {
        let Some(overlay) = &self.overlay else {
            return false;
        };
        if !self.is_recording() {
            return false;
        }

        let pcm = {
            let inner = self.state.lock().unwrap();
            let Some(active) = inner.active.as_ref() else {
                return false;
            };
            let secs = active.recording.captured_secs();
            if !(PREVIEW_MIN_SECS..=PREVIEW_MAX_SECS).contains(&secs) {
                return false;
            }
            active.recording.snapshot()
        };

        if pcm.iter().fold(0.0f32, |m, s| m.max(s.abs())) < SILENCE_PEAK {
            return false;
        }

        // `try_lock`, not `lock`: if the engine is busy finishing a real
        // dictation, the preview is worthless and must not queue up behind it.
        let Ok(mut engine) = self.engine.try_lock() else {
            return false;
        };
        // Re-check under the lock — `stop` may have landed while we waited.
        if !self.is_recording() {
            return false;
        }

        match engine.transcribe(&pcm) {
            Ok(t) => {
                let text = t.text.trim().to_string();
                if !text.is_empty() {
                    overlay.text(text);
                }
                true
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "preview transcription failed");
                false
            }
        }
    }

    pub fn state(&self) -> DaemonState {
        self.state.lock().unwrap().state
    }

    pub fn injector_name(&self) -> &str {
        &self.injector_name
    }

    pub fn engine_name(&self) -> String {
        self.engine.lock().unwrap().name().to_string()
    }

    /// Milliseconds recorded so far, if recording.
    pub fn recording_ms(&self) -> Option<u64> {
        let inner = self.state.lock().unwrap();
        inner
            .active
            .as_ref()
            .map(|a| a.started.elapsed().as_millis() as u64)
    }

    pub fn is_recording(&self) -> bool {
        self.state() == DaemonState::Recording
    }

    /// Begin recording. A no-op if already recording.
    pub fn start(&self) -> Result<()> {
        let mut inner = self.state.lock().unwrap();
        match inner.state {
            DaemonState::Recording => {
                tracing::debug!("start ignored: already recording");
                return Ok(());
            }
            DaemonState::Transcribing | DaemonState::Injecting => {
                anyhow::bail!("busy: still finishing the previous dictation");
            }
            DaemonState::Loading => anyhow::bail!("the model is still loading"),
            DaemonState::Ready => {}
        }

        let device = self.config.lock().unwrap().audio.device.clone();
        let recording = Recording::start(&device).context("starting the microphone")?;

        // Captured *now*, before the user can move focus away.
        let focus = stt_inject::focus::current();
        tracing::info!(app = ?focus.app(), "recording started");

        inner.active = Some(Active {
            recording,
            focus,
            started: Instant::now(),
        });
        inner.state = DaemonState::Recording;

        if let Some(overlay) = &self.overlay {
            overlay.show();
        }
        Ok(())
    }

    /// Stop recording and transcribe on a background thread.
    ///
    /// Returns as soon as the microphone is released, *not* when the text has
    /// been injected. This is deliberate: `stt stop` is what a compositor
    /// keybind runs, and blocking it for the length of a transcription hangs
    /// the keypress — measured at over five seconds on the first inference,
    /// while ONNX Runtime grows its arena. The outcome is delivered through
    /// `on_outcome` instead.
    pub fn stop_async(
        self: &Arc<Self>,
        on_outcome: impl FnOnce(Result<Outcome>) + Send + 'static,
    ) -> Result<()> {
        let active = {
            let mut inner = self.state.lock().unwrap();
            if inner.state != DaemonState::Recording {
                anyhow::bail!("not recording");
            }
            inner.state = DaemonState::Transcribing;
            inner
                .active
                .take()
                .context("recording state went missing")?
        };
        if let Some(overlay) = &self.overlay {
            overlay.transcribing();
        }

        let session = Arc::clone(self);
        std::thread::Builder::new()
            .name("stt-transcribe".into())
            .spawn(move || {
                let result = session.finish(active);
                session.state.lock().unwrap().state = DaemonState::Ready;
                // Dismiss only once the text has actually landed, so the
                // indicator covers the whole operation rather than vanishing
                // while the user is still waiting.
                if let Some(overlay) = &session.overlay {
                    overlay.hide();
                }
                on_outcome(result);
            })
            .context("spawning the transcription thread")?;
        Ok(())
    }

    /// Synchronous stop, for tests and for callers that want the outcome.
    #[cfg(test)]
    fn stop_blocking(&self) -> Result<Outcome> {
        let active = {
            let mut inner = self.state.lock().unwrap();
            if inner.state != DaemonState::Recording {
                anyhow::bail!("not recording");
            }
            inner.state = DaemonState::Transcribing;
            inner
                .active
                .take()
                .context("recording state went missing")?
        };
        let result = self.finish(active);
        self.state.lock().unwrap().state = DaemonState::Ready;
        result
    }

    fn finish(&self, active: Active) -> Result<Outcome> {
        let Active {
            recording, focus, ..
        } = active;

        let dropped = recording.dropped_frames();
        let pcm = recording.stop().context("finishing the recording")?;
        if dropped > 0 {
            tracing::warn!(frames = dropped, "audio was dropped during capture");
        }

        let secs = stt_core::wav::duration_secs(&pcm);
        let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        tracing::info!(secs, peak, "recording finished");

        if pcm.is_empty() || peak < SILENCE_PEAK {
            tracing::warn!(peak, "recording was silent; not transcribing");
            return Ok(Outcome::NoSpeech);
        }

        let started = Instant::now();
        let transcript = self
            .engine
            .lock()
            .unwrap()
            .transcribe(&pcm)
            .context("transcribing")?;
        tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            realtime = secs / started.elapsed().as_secs_f32().max(1e-6),
            "transcribed"
        );

        let text = transcript.text.trim().to_string();
        if text.is_empty() {
            return Ok(Outcome::NoSpeech);
        }

        self.deliver(&text, &focus)
    }

    /// Put `text` where the user wanted it, or somewhere safe if we cannot.
    fn deliver(&self, text: &str, recorded_focus: &Focus) -> Result<Outcome> {
        {
            let mut inner = self.state.lock().unwrap();
            inner.state = DaemonState::Injecting;
        }

        let always_copy = self.config.lock().unwrap().inject.always_copy;
        // Skip the safety copy when the backend is going to write the
        // clipboard anyway; copying twice is pure waste.
        let redundant = self.injector.lock().unwrap().leaves_text_on_clipboard();

        // The safety net: whatever happens next, the text is recoverable.
        if always_copy
            && !redundant
            && let Err(e) = stt_inject::copy_to_clipboard(text)
        {
            tracing::warn!(error = %format!("{e:#}"), "could not copy to the clipboard");
        }

        // The focus guard. Injection types into whatever is focused *now*, so
        // if that is no longer the window the user dictated into, typing would
        // put their words somewhere they did not intend.
        let now = stt_inject::focus::current();
        if !recorded_focus.is_same_target(&now) {
            let reason = format!(
                "focus moved from {} to {} during transcription",
                recorded_focus.app().unwrap_or("?"),
                now.app().unwrap_or("nothing")
            );
            tracing::warn!(%reason, "not injecting; text is on the clipboard");
            return Ok(Outcome::ClipboardOnly {
                text: text.to_string(),
                reason,
            });
        }

        self.injector
            .lock()
            .unwrap()
            .inject(text)
            .context("injecting text")?;

        Ok(Outcome::Injected {
            text: text.to_string(),
            via: self.injector_name.clone(),
        })
    }

    /// Start if idle, stop if recording.
    pub fn toggle(
        self: &Arc<Self>,
        on_outcome: impl FnOnce(Result<Outcome>) + Send + 'static,
    ) -> Result<()> {
        if self.is_recording() {
            self.stop_async(on_outcome)
        } else {
            self.start()
        }
    }

    /// Throw away the current recording without transcribing it.
    pub fn cancel(&self) -> Result<Outcome> {
        let mut inner = self.state.lock().unwrap();
        match inner.state {
            DaemonState::Recording => {
                // Dropping `Active` stops the stream and discards the audio.
                inner.active = None;
                inner.state = DaemonState::Ready;
                if let Some(overlay) = &self.overlay {
                    overlay.hide();
                }
                tracing::info!("recording cancelled");
                Ok(Outcome::Cancelled)
            }
            _ => anyhow::bail!("nothing to cancel"),
        }
    }

    /// Re-read the config. Engine and injector changes need a restart, which
    /// this reports rather than silently ignoring.
    pub fn reload(&self, path: &std::path::Path) -> Result<Vec<String>> {
        let fresh = Config::load_from(path)?;
        let mut current = self.config.lock().unwrap();

        let mut needs_restart = Vec::new();
        if fresh.engine != current.engine {
            needs_restart.push("engine (restart sttd to apply)".to_string());
        }
        if fresh.inject.backends != current.inject.backends {
            needs_restart.push("inject.backends (restart sttd to apply)".to_string());
        }
        *current = fresh;
        Ok(needs_restart)
    }

    /// Longest permitted recording, used by the runaway guard.
    pub fn max_duration_secs(&self) -> u32 {
        self.config.lock().unwrap().audio.max_duration_secs
    }
}

/// Below this peak a recording is silence, not speech.
const SILENCE_PEAK: f32 = 0.005;

/// Do not preview until there is enough audio to say anything useful.
const PREVIEW_MIN_SECS: f32 = 0.8;

/// Stop previewing past this length.
///
/// Each pass re-transcribes the whole buffer, so cost grows linearly with the
/// recording while the benefit does not — past this point the text is scrolled
/// well off the overlay anyway, and the CPU is better spent elsewhere.
const PREVIEW_MAX_SECS: f32 = 45.0;

#[cfg(test)]
mod tests {
    use super::*;
    use stt_core::Transcript;

    struct FakeEngine {
        text: String,
    }
    impl Engine for FakeEngine {
        fn transcribe(&mut self, _pcm: &[f32]) -> Result<Transcript> {
            Ok(Transcript {
                text: self.text.clone(),
                language: None,
            })
        }
        fn max_segment_secs(&self) -> f32 {
            240.0
        }
        fn name(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct FakeInjector {
        injected: Arc<Mutex<Vec<String>>>,
    }
    impl Injector for FakeInjector {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn inject(&mut self, text: &str) -> Result<()> {
            self.injected.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn session() -> Session {
        Session::new(
            Config::default(),
            Box::new(FakeEngine {
                text: "hello world".into(),
            }),
            Box::new(FakeInjector::default()),
            "fake".into(),
            None,
        )
    }

    #[test]
    fn starts_in_ready() {
        let s = session();
        assert_eq!(s.state(), DaemonState::Ready);
        assert!(!s.is_recording());
        assert_eq!(s.recording_ms(), None);
    }

    #[test]
    fn stop_without_start_is_an_error() {
        let s = session();
        assert!(s.stop_blocking().is_err());
    }

    #[test]
    fn cancel_without_recording_is_an_error() {
        let s = session();
        assert!(s.cancel().is_err());
    }

    #[test]
    fn reload_flags_engine_changes_as_needing_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[engine]\nmodel_dir = \"something-else\"\n").unwrap();

        let s = session();
        let restart = s.reload(&path).unwrap();
        assert!(
            restart.iter().any(|r| r.contains("engine")),
            "changing the model must not silently do nothing: {restart:?}"
        );
    }

    #[test]
    fn reload_of_an_unchanged_config_needs_no_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, Config::default().to_toml().unwrap()).unwrap();

        let s = session();
        assert!(s.reload(&path).unwrap().is_empty());
    }

    #[test]
    fn reload_rejects_a_broken_config_and_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[audio]\nsilence_timeout_ms = \"not a number\"\n").unwrap();

        let s = session();
        assert!(s.reload(&path).is_err());
        // The live config is untouched.
        assert_eq!(
            s.max_duration_secs(),
            Config::default().audio.max_duration_secs
        );
    }
}
