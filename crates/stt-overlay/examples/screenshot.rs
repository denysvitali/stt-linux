//! Headless screenshot of the overlay, no Wayland required.
//!
//! Renders two frames straight into an `Argb8888` buffer using the same
//! paint and text paths the live surface uses, then writes them as PNGs.
//! Used by the screenshots CI job: the runner has no compositor, no GPU
//! and no seat, so hosting a layer-shell surface is not an option there.
//!
//! The backdrop blur that a real compositor would supply is faked by
//! compositing each frame over a flat panel colour. The pictures stay
//! legible (the pane is dark, the backdrop is medium-dark) without
//! pretending to be a true screenshot.
use stt_overlay::paint::{Canvas, PillGeometry, theme};
use stt_overlay::text::TextRenderer;

// Mirrors the constants in lib.rs.
const WIDTH: i32 = 780;
const HEIGHT: i32 = 96;
const BARS: usize = 18;
const FONT_SIZE: f32 = 17.0;
const PILL_H: f32 = 52.0;
const MIN_PILL_W: f32 = 168.0;
const MAX_PILL_W: f32 = WIDTH as f32 - 40.0;
const PAD_X: f32 = 20.0;
const DOT_R: f32 = 6.0;
const DOT_SLOT: f32 = DOT_R * 2.0 + 18.0;
const METER_W: f32 = 148.0;

// One-shot mock levels, picked to read clearly at static sizes.
fn meter_levels(seed: f32) -> [f32; BARS] {
    let mut v = [0.0f32; BARS];
    for (i, slot) in v.iter_mut().enumerate() {
        let t = (i as f32 + seed) * 0.55;
        *slot = 0.55 + 0.45 * t.sin();
    }
    v
}

fn write_png(path: &str, buf: &[u8]) -> anyhow::Result<()> {
    let f = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(f);
    let mut enc = png::Encoder::new(&mut w, WIDTH as u32, HEIGHT as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(buf)?;
    Ok(())
}

// Composites the ARGB frame over a flat backdrop, producing a final
// premultiplied RGBA buffer ready for PNG (where alpha 255 = solid).
//
//   bg = the colour you would see behind the pane in a real session
//   pane alpha is already in the source's 4th channel
//
// We assume the backdrop is a solid colour rather than a blurred wallpaper
// because CI cannot generate a real one; the README copy is explicit that
// the demo backdrop is a stand-in.
fn composite_over_backdrop(panel: &[u8], bg: (u8, u8, u8)) -> Vec<u8> {
    let mut out = vec![0u8; panel.len()];
    // Source pixels are stored as B, G, R, A (canvas convention). The output
    // is straight RGBA for the encoder.
    for (px, slot) in panel.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        let af = a as f32 / 255.0;
        let ib = 1.0 - af;
        slot[0] = ((r as f32 * af) + (bg.0 as f32 * ib)) as u8;
        slot[1] = ((g as f32 * af) + (bg.1 as f32 * ib)) as u8;
        slot[2] = ((b as f32 * af) + (bg.2 as f32 * ib)) as u8;
        slot[3] = 255;
    }
    out
}

fn draw_frame(text: Option<&str>, levels: [f32; BARS], phase_t: f32) -> Vec<u8> {
    let mut buf = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    let w = WIDTH;
    let h = HEIGHT;
    let mut canvas = Canvas::new(&mut buf, w, h);
    canvas.clear();

    let mut text_renderer = TextRenderer::from_system(FONT_SIZE).ok();

    let show_text = text.is_some() && text_renderer.is_some();
    let line = if show_text {
        text.and_then(|t| {
            text_renderer
                .as_mut()
                .map(|r| r.fit_tail(t, MAX_PILL_W - PAD_X * 2.0 - DOT_SLOT))
        })
    } else {
        None
    };
    let text_w = match (&line, text_renderer.as_mut()) {
        (Some(l), Some(r)) => r.measure(l),
        _ => 0.0,
    };
    let content_w = if show_text { text_w } else { METER_W };
    let pill_w = (PAD_X * 2.0 + DOT_SLOT + content_w).clamp(MIN_PILL_W, MAX_PILL_W);
    let pill_x = (w as f32 - pill_w) * 0.5;
    let pill_y = (h as f32 - PILL_H) * 0.5;
    let cy = pill_y + PILL_H * 0.5;

    canvas.glass_pill(
        PillGeometry {
            x: pill_x,
            y: pill_y,
            w: pill_w,
            h: PILL_H,
            radius: PILL_H * 0.5,
        },
        theme::GLASS,
        theme::LIGHT_ANGLE,
    );

    let breathe = 0.72 + 0.28 * (phase_t * std::f32::consts::TAU / 2.2).sin();
    let dot_x = pill_x + PAD_X + DOT_R;
    let halo_r = DOT_R + 4.0 + levels[BARS - 1] * 12.0;
    let halo_a = 0.10 + levels[BARS - 1] * 0.45;
    canvas.circle(dot_x, cy, halo_r, theme::REC.fade(halo_a));
    canvas.circle(dot_x, cy, DOT_R, theme::REC.fade(breathe));

    let content_x = pill_x + PAD_X + DOT_SLOT;
    if let (Some(line), Some(mut renderer)) = (line, text_renderer.take()) {
        renderer.draw_centered(
            &mut canvas,
            content_x + 1.0,
            cy + 1.0,
            &line,
            theme::TEXT_SHADOW,
        );
        renderer.draw_centered(&mut canvas, content_x, cy, &line, theme::TEXT);
    } else {
        let colour = theme::BAR_HOT.fade(0.55 + 0.45 * levels[BARS - 1]);
        canvas.waveform(
            content_x,
            cy,
            METER_W,
            (PILL_H - 26.0) * 0.5,
            &levels,
            colour,
        );
    }
    buf
}

fn main() -> anyhow::Result<()> {
    // Sway's solid_color backdrop in the live capture script was #2e3440,
    // so the PNG composites are pixel-equivalent to what grim would see.
    let backdrop = (0x2e, 0x34, 0x40);

    // --- "meter" frame: listening, no words yet, levels animating.
    let levels = meter_levels(0.0);
    let frame = draw_frame(None, levels, 0.35);
    let png = composite_over_backdrop(&frame, backdrop);
    write_png("assets/overlay-meter.png", &png)?;
    println!("wrote assets/overlay-meter.png ({} bytes)", png.len());

    // --- "live" frame: a transcript of 10 words, fully visible (no elision).
    let transcript = "speech recognition on Linux should work offline";
    let levels = meter_levels(2.5);
    let frame = draw_frame(Some(transcript), levels, 0.6);
    let png = composite_over_backdrop(&frame, backdrop);
    write_png("assets/overlay-live.png", &png)?;
    println!("wrote assets/overlay-live.png ({} bytes)", png.len());

    Ok(())
}
