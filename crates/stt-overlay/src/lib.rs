//! The recording overlay: a small always-on-top indicator that shows whether
//! dictation is listening, and how loud you are.
//!
//! # Why it cannot take focus
//!
//! This is the whole reason the overlay is built the way it is. Handy — the
//! closest existing tool — has a recording overlay that some compositors treat
//! as the active window, and because its text injection pastes into *the
//! focused window*, its own overlay breaks its own paste. Handy's answer was to
//! disable the overlay on Linux by default.
//!
//! The fix is `KeyboardInteractivity::None` on a `wlr-layer-shell` surface.
//! Such a surface is never focusable, so it cannot become the injection target
//! no matter what the compositor does with stacking.
//!
//! # Why it runs in-process
//!
//! An earlier design had this as a separate binary, because a GUI toolkit would
//! have demanded the main thread. Rendering by hand removes that constraint, so
//! it is a thread instead — which also removes process-spawn latency from the
//! path between pressing the key and seeing the indicator.

pub mod paint;
pub mod text;

use anyhow::{Context, Result};
use std::sync::mpsc;
use std::time::Instant;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

use paint::{Canvas, PillGeometry, theme};
use text::TextRenderer;

/// The surface is a fixed size and mostly transparent; the pane inside it is
/// sized to its contents. Keeping the surface constant avoids asking the
/// compositor to resize on every preview, which would flicker.
const WIDTH: u32 = 780;
const HEIGHT: u32 = 96;
/// Gap from the screen edge.
const MARGIN: i32 = 72;
/// Number of level bars in the meter shown before any text arrives.
const BARS: usize = 18;
const FONT_SIZE: f32 = 17.0;

/// Pane geometry.
const PILL_H: f32 = 52.0;
const MIN_PILL_W: f32 = 168.0;
/// Leaves a margin inside the surface for the drop shadow.
const MAX_PILL_W: f32 = WIDTH as f32 - 40.0;
const PAD_X: f32 = 20.0;
const DOT_R: f32 = 6.0;
/// Horizontal space reserved for the dot and its halo, from the pane's inner
/// left edge to where content starts.
const DOT_SLOT: f32 = DOT_R * 2.0 + 18.0;
const METER_W: f32 = 148.0;

/// Where to put the indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Top,
    Bottom,
    Center,
}

/// What the overlay is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Recording,
    Transcribing,
}

/// Messages from the daemon to the overlay thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    Show,
    Level(f32),
    /// Live transcript of what has been said so far.
    Text(String),
    Transcribing,
    Hide,
    Quit,
}

/// Controls the overlay thread. Dropping it shuts the thread down.
pub struct Overlay {
    tx: mpsc::Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Overlay {
    /// Start the overlay thread.
    ///
    /// Fails if there is no Wayland session or the compositor has no
    /// layer-shell. Callers should treat that as "run without an overlay",
    /// not as a fatal error — dictation works fine without one.
    pub fn spawn(anchor: OverlayAnchor) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        // Confirm the compositor can actually host the surface before
        // reporting success, so the daemon logs the real reason at startup
        // rather than failing silently on the first dictation.
        let (ready_tx, ready_rx) = mpsc::channel();

        let thread = std::thread::Builder::new()
            .name("stt-overlay".into())
            .spawn(move || {
                match run(anchor, rx, &ready_tx) {
                    Ok(()) => tracing::debug!("overlay thread exited"),
                    Err(e) => {
                        // Report the failure if startup had not yet succeeded.
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        tracing::warn!(error = %format!("{e:#}"), "overlay stopped");
                    }
                }
            })
            .context("spawning the overlay thread")?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Err(_) => anyhow::bail!("the overlay did not start within 3s"),
        }
    }

    fn send(&self, cmd: Cmd) {
        // A dead overlay thread must never break dictation.
        if let Err(e) = self.tx.send(cmd) {
            tracing::debug!(cmd = ?e.0, "overlay is gone; dropping command");
        }
    }

    pub fn show(&self) {
        self.send(Cmd::Show);
    }

    pub fn level(&self, value: f32) {
        self.send(Cmd::Level(value));
    }

    /// Update the live transcript shown in the overlay.
    pub fn text(&self, text: String) {
        self.send(Cmd::Text(text));
    }

    pub fn transcribing(&self) {
        self.send(Cmd::Transcribing);
    }

    pub fn hide(&self) {
        self.send(Cmd::Hide);
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    compositor: CompositorState,
    layer_shell: LayerShell,
    anchor: OverlayAnchor,

    /// Present only while visible; dropping it unmaps the surface.
    layer: Option<LayerSurface>,
    configured: bool,
    phase: Phase,
    /// Recent levels, oldest first, driving the bar meter.
    levels: [f32; BARS],
    /// Live transcript, empty until the first preview arrives.
    transcript: String,
    /// `None` when no system font could be loaded; the overlay then shows the
    /// meter only, rather than failing outright.
    text: Option<TextRenderer>,
    started: Instant,
    quit: bool,
}

fn run(
    anchor: OverlayAnchor,
    rx: mpsc::Receiver<Cmd>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("initializing the Wayland registry")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_compositor: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("zwlr_layer_shell_v1 unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm: {e}"))?;
    let pool = SlotPool::new((WIDTH * HEIGHT * 4) as usize, &shm).context("creating a shm pool")?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        anchor,
        layer: None,
        configured: false,
        phase: Phase::Recording,
        levels: [0.0; BARS],
        transcript: String::new(),
        text: match TextRenderer::from_system(FONT_SIZE) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "no font; overlay will show the meter only");
                None
            }
        },
        started: Instant::now(),
        quit: false,
    };

    let _ = ready.send(Ok(()));

    // Poll rather than block: the thread has two event sources (Wayland and
    // the command channel) and this is the smallest way to serve both. 8 ms
    // keeps the meter smooth without meaningful CPU cost, and only while the
    // overlay is actually visible.
    while !state.quit {
        loop {
            match rx.try_recv() {
                Ok(cmd) => state.apply(cmd, &qh),
                Err(mpsc::TryRecvError::Empty) => break,
                // The daemon dropped the handle without a Quit.
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.quit = true;
                    break;
                }
            }
        }

        event_queue
            .dispatch_pending(&mut state)
            .context("dispatching Wayland events")?;

        if state.layer.is_some() {
            state.draw();
        }

        conn.flush().context("flushing the Wayland connection")?;

        // Read anything the compositor sent while we were busy.
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
        }
        event_queue.dispatch_pending(&mut state)?;

        std::thread::sleep(std::time::Duration::from_millis(if state.layer.is_some() {
            8
        } else {
            // Idle: nothing on screen, so poll lazily.
            40
        }));
    }

    Ok(())
}

impl State {
    fn apply(&mut self, cmd: Cmd, qh: &QueueHandle<Self>) {
        match cmd {
            Cmd::Show => {
                if self.layer.is_none() {
                    self.create_surface(qh);
                }
                self.phase = Phase::Recording;
                self.levels = [0.0; BARS];
                self.transcript.clear();
                self.started = Instant::now();
            }
            Cmd::Level(v) => {
                self.levels.rotate_left(1);
                // Speech RMS sits low in the range; this scaling makes normal
                // talking fill roughly two-thirds of the meter.
                self.levels[BARS - 1] = (v * 6.0).clamp(0.0, 1.0);
            }
            Cmd::Text(t) => self.transcript = t,
            Cmd::Transcribing => self.phase = Phase::Transcribing,
            Cmd::Hide => self.destroy_surface(),
            Cmd::Quit => {
                self.destroy_surface();
                self.quit = true;
            }
        }
    }

    fn create_surface(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            // Overlay, not Top: it should sit above fullscreen windows, since
            // dictating into a fullscreen editor is a normal thing to do.
            Layer::Overlay,
            Some("stt-linux"),
            None,
        );

        layer.set_anchor(match self.anchor {
            OverlayAnchor::Top => Anchor::TOP,
            OverlayAnchor::Bottom => Anchor::BOTTOM,
            // No anchor at all centres the surface.
            OverlayAnchor::Center => Anchor::empty(),
        });
        layer.set_margin(MARGIN, 0, MARGIN, 0);
        layer.set_size(WIDTH, HEIGHT);
        // The point of the whole exercise: never focusable, so it can never
        // become the target of our own text injection.
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Do not reserve screen space; float over the existing layout.
        layer.set_exclusive_zone(-1);
        layer.commit();

        self.configured = false;
        self.layer = Some(layer);
    }

    fn destroy_surface(&mut self) {
        // Dropping the LayerSurface destroys it and unmaps the overlay.
        self.layer = None;
        self.configured = false;
    }

    fn draw(&mut self) {
        let Some(layer) = &self.layer else { return };
        if !self.configured {
            return;
        }

        let (w, h) = (WIDTH as i32, HEIGHT as i32);
        let Ok((buffer, canvas_buf)) =
            self.pool
                .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("could not allocate an overlay buffer");
            return;
        };

        let elapsed = self.started.elapsed().as_secs_f32();
        // Newest level, used to make the indicator physically react to the
        // voice rather than just animate on a timer.
        let level = self.levels[BARS - 1];

        let mut canvas = Canvas::new(canvas_buf, w, h);
        canvas.clear();

        let show_text = !self.transcript.is_empty() && self.text.is_some();

        // --- measure before drawing ------------------------------------
        // The pane is sized to its contents and centred in a fixed, mostly
        // transparent surface. Sizing the *surface* instead would mean asking
        // the compositor to resize on every preview, which reflows and
        // flickers; drawing a narrower pane inside a constant surface costs
        // nothing and stays perfectly still.
        let fitted = if show_text {
            let max_text = MAX_PILL_W - PAD_X * 2.0 - DOT_SLOT;
            self.text
                .as_mut()
                .map(|r| r.fit_tail(&self.transcript, max_text))
        } else {
            None
        };
        let text_w = match (&fitted, self.text.as_mut()) {
            (Some(line), Some(r)) => r.measure(line),
            _ => 0.0,
        };

        let content_w = if show_text { text_w } else { METER_W };
        let pill_w = (PAD_X * 2.0 + DOT_SLOT + content_w).clamp(MIN_PILL_W, MAX_PILL_W);
        let pill_h = PILL_H;
        let pill_x = (w as f32 - pill_w) * 0.5;
        let pill_y = (h as f32 - pill_h) * 0.5;
        let cy = pill_y + pill_h * 0.5;

        // --- the pane ---------------------------------------------------
        canvas.glass_pill(
            PillGeometry {
                x: pill_x,
                y: pill_y,
                w: pill_w,
                h: pill_h,
                radius: pill_h * 0.5,
            },
            theme::GLASS,
            theme::LIGHT_ANGLE,
        );

        // --- status dot -------------------------------------------------
        // The dot is the level meter. Its halo swells with the sound actually
        // reaching the microphone, so a glance answers both questions at once:
        // is it listening, and can it hear me. A separate bar meter alongside
        // the transcript competed with the words for attention and, at three
        // bars wide, was mistaken for an ellipsis.
        let (colour, period) = match self.phase {
            Phase::Recording => (theme::REC, 2.2),
            Phase::Transcribing => (theme::BUSY, 0.9),
        };
        let breathe = 0.72 + 0.28 * (elapsed * std::f32::consts::TAU / period).sin();
        let dot_x = pill_x + PAD_X + DOT_R;

        let (halo_r, halo_a) = match self.phase {
            // Voice drives the halo; the timed breath only keeps it alive
            // through a pause.
            Phase::Recording => (DOT_R + 4.0 + level * 12.0, 0.10 + level * 0.45),
            Phase::Transcribing => (DOT_R + 4.0 + breathe * 3.0, 0.16 * breathe),
        };
        canvas.circle(dot_x, cy, halo_r, colour.fade(halo_a));
        canvas.circle(dot_x, cy, DOT_R, colour.fade(breathe));

        // --- content ------------------------------------------------------
        let content_x = pill_x + PAD_X + DOT_SLOT;

        // `self.text` must only be taken when it is definitely going to be put
        // back. Evaluating `self.text.take()` as part of a tuple pattern takes
        // it even when the pattern fails, which silently destroyed the font
        // renderer on the first meter-only frame and meant no transcript ever
        // rendered afterwards.
        if let Some(line) = fitted
            && let Some(mut renderer) = self.text.take()
        {
            let colour = match self.phase {
                Phase::Recording => theme::TEXT,
                // Dim once the microphone is closed: this is the last preview,
                // not a live one.
                Phase::Transcribing => theme::TEXT.fade(0.72),
            };
            // A one-pixel shadow keeps the text readable when the blurred
            // backdrop happens to be pale.
            renderer.draw_centered(
                &mut canvas,
                content_x + 1.0,
                cy + 1.0,
                &line,
                theme::TEXT_SHADOW,
            );
            renderer.draw_centered(&mut canvas, content_x, cy, &line, colour);
            self.text = Some(renderer);
        } else {
            // Nothing transcribed yet. A waveform is the most honest thing to
            // show: it proves the microphone is live, and at silence it
            // settles into a hairline rather than a row of dots.
            let colour = if self.phase == Phase::Transcribing {
                theme::BAR_IDLE
            } else {
                theme::BAR_HOT.fade(0.55 + 0.45 * level)
            };
            canvas.waveform(
                content_x,
                cy,
                METER_W,
                (pill_h - 26.0) * 0.5,
                &self.levels,
                colour,
            );
        }

        let surface = layer.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_err() {
            tracing::warn!("could not attach the overlay buffer");
            return;
        }
        layer.commit();
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.destroy_surface();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        // Fixed size, so the configured dimensions are not interesting; what
        // matters is that the surface is now allowed to draw.
        self.configured = true;
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // Redraw is driven by the poll loop, not by frame callbacks.
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_history_scrolls_oldest_out() {
        let mut levels = [0.0f32; BARS];
        for i in 0..BARS {
            levels.rotate_left(1);
            levels[BARS - 1] = i as f32;
        }
        // After BARS pushes the newest value is last and the oldest is gone.
        assert_eq!(levels[BARS - 1], (BARS - 1) as f32);
        assert_eq!(levels[0], 0.0);
    }

    #[test]
    fn level_scaling_clamps_to_unit_range() {
        // A loud transient must not produce a bar taller than the widget.
        let scaled = |v: f32| (v * 6.0).clamp(0.0, 1.0);
        assert_eq!(scaled(0.0), 0.0);
        assert_eq!(scaled(1.0), 1.0);
        assert_eq!(scaled(-0.5), 0.0);
        assert!(scaled(0.1) > 0.5, "normal speech should register clearly");
    }
}
