//! Software rendering for the overlay.
//!
//! Deliberately font-free and GPU-free. The indicator is a pill, a dot and a
//! bar meter — shapes, not text — which keeps the dependency tree small and,
//! more importantly, means the surface can be up in milliseconds. An overlay
//! that appears half a second after you start speaking is worse than none,
//! because you have already started talking by then.

/// Straight (non-premultiplied) colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Same colour at a different opacity, as a fraction of its own alpha.
    pub fn fade(self, factor: f32) -> Self {
        Self {
            a: (self.a as f32 * factor.clamp(0.0, 1.0)) as u8,
            ..self
        }
    }
}

/// An ARGB8888 canvas.
///
/// Wayland's `Argb8888` is **premultiplied**, so alpha is folded into the
/// colour channels on write. Skipping that makes translucent pixels glow.
pub struct Canvas<'a> {
    pub buf: &'a mut [u8],
    pub width: i32,
    pub height: i32,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u8], width: i32, height: i32) -> Self {
        Self { buf, width, height }
    }

    pub fn clear(&mut self) {
        self.buf.fill(0);
    }

    /// Alpha-blend one pixel. `coverage` in `[0,1]` provides antialiasing.
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba, coverage: f32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let a = (c.a as f32 / 255.0) * coverage.clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let idx = ((y * self.width + x) * 4) as usize;
        let dst = &mut self.buf[idx..idx + 4];

        // Stored little-endian as B, G, R, A.
        let (db, dg, dr, da) = (dst[0] as f32, dst[1] as f32, dst[2] as f32, dst[3] as f32);
        // Source is premultiplied on the way in.
        let (sr, sg, sb) = (c.r as f32 * a, c.g as f32 * a, c.b as f32 * a);
        let inv = 1.0 - a;

        dst[0] = (sb + db * inv).min(255.0) as u8;
        dst[1] = (sg + dg * inv).min(255.0) as u8;
        dst[2] = (sr + dr * inv).min(255.0) as u8;
        dst[3] = (a * 255.0 + da * inv).min(255.0) as u8;
    }

    /// Rounded rectangle, antialiased on the edges.
    ///
    /// Uses the standard rounded-box signed distance function, evaluated from
    /// the centre. Two earlier attempts at a shortcut both failed: clamping a
    /// point into the "inner rect" and measuring from there gives distance 0
    /// for *every* interior pixel when the radius is zero (so a plain square
    /// renders at uniform 50% alpha), and it computes `x + w - r` against
    /// `x + r`, which in f32 can invert when `w == 2r` and panic inside
    /// `clamp`. The SDF below has neither problem: it is negative inside,
    /// positive outside, and never compares two float expressions that ought
    /// to be equal.
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, c: Rgba) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let half_w = w * 0.5;
        let half_h = h * 0.5;
        let r = radius.clamp(0.0, half_w.min(half_h));
        let (centre_x, centre_y) = (x + half_w, y + half_h);

        // One pixel of slack so the antialiased edge is not clipped.
        let x0 = (x - 1.0).floor() as i32;
        let y0 = (y - 1.0).floor() as i32;
        let x1 = (x + w + 1.0).ceil() as i32;
        let y1 = (y + h + 1.0).ceil() as i32;

        for py in y0..y1 {
            for px in x0..x1 {
                let dx = (px as f32 + 0.5 - centre_x).abs() - (half_w - r);
                let dy = (py as f32 + 0.5 - centre_y).abs() - (half_h - r);
                // Distance outside the corner-inset box, plus the (negative)
                // depth when fully inside it, minus the corner radius.
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
                let inside = dx.max(dy).min(0.0);
                let dist = outside + inside - r;
                // One-pixel smoothstep across the boundary.
                let coverage = (0.5 - dist).clamp(0.0, 1.0);
                self.blend(px, py, c, coverage);
            }
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba) {
        self.rounded_rect(cx - radius, cy - radius, radius * 2.0, radius * 2.0, radius, c);
    }

    /// A pane of glass: soft drop shadow, translucent body, and a specular rim
    /// that catches the light from one direction.
    ///
    /// The *blur* behind this is not ours to draw. A Wayland client cannot read
    /// the pixels underneath its own surface — there is no protocol for it, by
    /// design — so the backdrop blur has to come from the compositor
    /// (`hl.layer_rule({ blur = true })` on Hyprland). What is drawn here is
    /// everything that sits on top of that blur and makes it read as a
    /// physical object rather than a translucent rectangle: the lensed edge,
    /// the depth, and the shadow that lifts it off the desktop.
    ///
    /// `light` is the direction light arrives from, in radians, measured from
    /// the positive x axis with y pointing down.
    pub fn glass_pill(&mut self, geom: PillGeometry, style: GlassStyle, light: f32) {
        let PillGeometry {
            x,
            y,
            w,
            h,
            radius,
        } = geom;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let half_w = w * 0.5;
        let half_h = h * 0.5;
        let r = radius.clamp(0.0, half_w.min(half_h));
        let (cx, cy) = (x + half_w, y + half_h);

        let sdf = |px: f32, py: f32| -> f32 {
            let dx = (px - cx).abs() - (half_w - r);
            let dy = (py - cy).abs() - (half_h - r);
            let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
            outside + dx.max(dy).min(0.0) - r
        };

        let (lx, ly) = (light.cos(), light.sin());
        let margin = style.shadow_radius.max(style.rim_width) + 2.0;
        let x0 = (x - margin).floor() as i32;
        let y0 = (y - margin).floor() as i32;
        let x1 = (x + w + margin).ceil() as i32;
        let y1 = (y + h + margin).ceil() as i32;

        for py in y0..y1 {
            for px in x0..x1 {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let d = sdf(fx, fy);

                // Drop shadow, outside the body only.
                if d > 0.0 && style.shadow_radius > 0.0 {
                    let t = (1.0 - d / style.shadow_radius).clamp(0.0, 1.0);
                    // Squared falloff reads softer than linear.
                    self.blend(px, py, style.shadow, t * t * style.shadow_strength);
                }

                // Translucent body.
                if d < 0.5 {
                    let coverage = (0.5 - d).clamp(0.0, 1.0);
                    // A vertical gradient: glass is brighter where it faces
                    // the light, which keeps a large flat pane from looking
                    // like a sticker.
                    let grade = 1.0 - ((fy - y) / h).clamp(0.0, 1.0) * style.body_falloff;
                    self.blend(px, py, style.body.fade(grade), coverage);
                }

                // Specular rim. Only computed near the boundary, where the
                // surface normal is meaningful and the cost is small.
                if style.rim_width > 0.0 && d.abs() < style.rim_width {
                    // Numerical gradient of the SDF gives the outward normal.
                    let nx = sdf(fx + 1.0, fy) - sdf(fx - 1.0, fy);
                    let ny = sdf(fx, fy + 1.0) - sdf(fx, fy - 1.0);
                    let len = (nx * nx + ny * ny).sqrt().max(1e-5);
                    let facing = ((nx / len) * lx + (ny / len) * ly).clamp(-1.0, 1.0);

                    // Brightest where the edge faces the light, never fully
                    // dark elsewhere — real glass catches a little everywhere.
                    let lambert = style.rim_ambient
                        + (1.0 - style.rim_ambient) * (0.5 + 0.5 * facing).powf(style.rim_focus);
                    // Triangular band centred on the boundary.
                    let band = 1.0 - (d.abs() / style.rim_width).clamp(0.0, 1.0);
                    self.blend(px, py, style.rim, band * band * lambert);
                }
            }
        }
    }
}

impl Canvas<'_> {
    /// A symmetric waveform ribbon: amplitude over time, mirrored about a
    /// centre line, with the samples interpolated so it reads as one
    /// continuous shape.
    ///
    /// Discrete bars were the obvious choice and the wrong one — at
    /// conversational volume they collapse into a row of evenly spaced dots,
    /// which is both ugly and easily mistaken for an ellipsis. A ribbon
    /// degrades to a clean hairline at silence instead.
    pub fn waveform(
        &mut self,
        x: f32,
        cy: f32,
        w: f32,
        max_amp: f32,
        samples: &[f32],
        c: Rgba,
    ) {
        if samples.len() < 2 || w <= 0.0 {
            return;
        }
        let min_amp = 1.0;
        let x0 = x.floor() as i32;
        let x1 = (x + w).ceil() as i32;

        for px in x0..x1 {
            // Position along the ribbon, 0..1.
            let t = ((px as f32 + 0.5 - x) / w).clamp(0.0, 1.0);
            // Linear interpolation between neighbouring samples.
            let pos = t * (samples.len() - 1) as f32;
            let i = pos.floor() as usize;
            let frac = pos - i as f32;
            let a = samples[i.min(samples.len() - 1)];
            let b = samples[(i + 1).min(samples.len() - 1)];
            let level = a + (b - a) * frac;

            let amp = (max_amp * level).max(min_amp);
            let top = cy - amp;
            let bottom = cy + amp;

            // Antialias the vertical extent by coverage per pixel row.
            let py0 = (top - 1.0).floor() as i32;
            let py1 = (bottom + 1.0).ceil() as i32;
            for py in py0..py1 {
                let pixel_top = py as f32;
                let pixel_bottom = py as f32 + 1.0;
                let covered = pixel_bottom.min(bottom) - pixel_top.max(top);
                if covered > 0.0 {
                    self.blend(px, py, c, covered.min(1.0));
                }
            }
        }
    }
}

/// Where a pill sits and how round it is.
#[derive(Debug, Clone, Copy)]
pub struct PillGeometry {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
}

/// The optical properties of the glass.
#[derive(Debug, Clone, Copy)]
pub struct GlassStyle {
    /// Tint of the pane itself. Low alpha lets the compositor's blur through.
    pub body: Rgba,
    /// How much darker the bottom of the pane is than the top, `0.0..1.0`.
    pub body_falloff: f32,
    pub rim: Rgba,
    pub rim_width: f32,
    /// Rim brightness on the unlit side, `0.0..1.0`.
    pub rim_ambient: f32,
    /// Higher values tighten the specular highlight.
    pub rim_focus: f32,
    pub shadow: Rgba,
    pub shadow_radius: f32,
    pub shadow_strength: f32,
}

/// Palette.
///
/// Taken from the user's own Hyprland theme rather than invented, so the
/// overlay reads as part of their desktop instead of a visitor on it. The
/// alphas assume the compositor is blurring the backdrop; without that the
/// pane still works, just as plain smoked glass.
pub mod theme {
    use super::{GlassStyle, Rgba};

    /// Smoked body. Dark enough that white text stays legible over a bright
    /// backdrop, sheer enough that the blur behind it is clearly visible.
    pub const GLASS_BODY: Rgba = Rgba::new(0x0b, 0x10, 0x18, 0x9c);
    /// The lensed edge. White rather than tinted: a coloured rim reads as a
    /// glow, and glass catches the light uncoloured.
    pub const GLASS_RIM: Rgba = Rgba::new(0xff, 0xff, 0xff, 0xd8);
    pub const GLASS_SHADOW: Rgba = Rgba::new(0x00, 0x00, 0x00, 0xb4);

    /// Recording — their theme's warning pink, which reads as "live".
    pub const REC: Rgba = Rgba::new(0xf3, 0x8b, 0xaa, 0xff);
    /// Transcribing — their accent lavender.
    pub const BUSY: Rgba = Rgba::new(0xca, 0xbf, 0xfd, 0xff);
    /// Level bars at rest.
    pub const BAR_IDLE: Rgba = Rgba::new(0x8b, 0xd5, 0xfa, 0x3d);
    /// Level bars carrying signal — their accent cyan.
    pub const BAR_HOT: Rgba = Rgba::new(0x8b, 0xd5, 0xfa, 0xff);
    /// Live transcript.
    pub const TEXT: Rgba = Rgba::new(0xf8, 0xfa, 0xff, 0xff);
    /// Drop shadow behind the text, for legibility over a bright backdrop.
    pub const TEXT_SHADOW: Rgba = Rgba::new(0x00, 0x00, 0x00, 0x9c);

    pub const GLASS: GlassStyle = GlassStyle {
        body: GLASS_BODY,
        body_falloff: 0.35,
        rim: GLASS_RIM,
        rim_width: 1.6,
        rim_ambient: 0.16,
        rim_focus: 2.6,
        shadow: GLASS_SHADOW,
        shadow_radius: 14.0,
        shadow_strength: 0.5,
    };

    /// Light arriving from above and slightly left, matching the convention
    /// every other element on a desktop already assumes.
    pub const LIGHT_ANGLE: f32 = -std::f32::consts::FRAC_PI_2 - 0.35;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(w: i32, h: i32) -> Vec<u8> {
        vec![0u8; (w * h * 4) as usize]
    }

    fn pixel(buf: &[u8], w: i32, x: i32, y: i32) -> (u8, u8, u8, u8) {
        let i = ((y * w + x) * 4) as usize;
        (buf[i + 2], buf[i + 1], buf[i], buf[i + 3]) // r, g, b, a
    }

    #[test]
    fn opaque_fill_writes_exact_colour() {
        let mut buf = canvas(4, 4);
        let mut c = Canvas::new(&mut buf, 4, 4);
        c.blend(1, 1, Rgba::new(200, 100, 50, 255), 1.0);
        assert_eq!(pixel(&buf, 4, 1, 1), (200, 100, 50, 255));
    }

    #[test]
    fn alpha_is_premultiplied() {
        // Wayland Argb8888 expects premultiplied colour; at 50% alpha a
        // full-red pixel must store ~127 in the red channel, not 255.
        let mut buf = canvas(2, 2);
        let mut c = Canvas::new(&mut buf, 2, 2);
        c.blend(0, 0, Rgba::new(255, 0, 0, 128), 1.0);
        let (r, _, _, a) = pixel(&buf, 2, 0, 0);
        assert!((120..=135).contains(&r), "red was {r}, expected ~127");
        assert!((120..=135).contains(&a), "alpha was {a}, expected ~128");
    }

    #[test]
    fn writes_outside_the_canvas_are_dropped() {
        let mut buf = canvas(2, 2);
        let mut c = Canvas::new(&mut buf, 2, 2);
        // Must not panic or corrupt neighbouring memory.
        c.blend(-1, 0, Rgba::new(255, 255, 255, 255), 1.0);
        c.blend(0, -1, Rgba::new(255, 255, 255, 255), 1.0);
        c.blend(2, 0, Rgba::new(255, 255, 255, 255), 1.0);
        c.blend(0, 2, Rgba::new(255, 255, 255, 255), 1.0);
        assert!(buf.iter().all(|&b| b == 0), "an out-of-bounds write landed");
    }

    #[test]
    fn zero_coverage_leaves_the_pixel_alone() {
        let mut buf = canvas(2, 2);
        let mut c = Canvas::new(&mut buf, 2, 2);
        c.blend(0, 0, Rgba::new(255, 255, 255, 255), 0.0);
        assert_eq!(pixel(&buf, 2, 0, 0), (0, 0, 0, 0));
    }

    #[test]
    fn rounded_rect_fills_its_centre_and_clears_its_corners() {
        let (w, h) = (20, 20);
        let mut buf = canvas(w, h);
        {
            let mut c = Canvas::new(&mut buf, w, h);
            c.rounded_rect(0.0, 0.0, 20.0, 20.0, 8.0, Rgba::new(255, 255, 255, 255));
        }
        let (_, _, _, centre_a) = pixel(&buf, w, 10, 10);
        assert_eq!(centre_a, 255, "centre should be solid");
        let (_, _, _, corner_a) = pixel(&buf, w, 0, 0);
        assert_eq!(corner_a, 0, "corner should be rounded away");
    }

    #[test]
    fn a_zero_radius_rect_keeps_its_corners() {
        let (w, h) = (10, 10);
        let mut buf = canvas(w, h);
        {
            let mut c = Canvas::new(&mut buf, w, h);
            c.rounded_rect(0.0, 0.0, 10.0, 10.0, 0.0, Rgba::new(255, 255, 255, 255));
        }
        assert_eq!(pixel(&buf, w, 0, 0).3, 255);
    }

    #[test]
    fn a_square_is_uniformly_opaque_not_half_transparent() {
        // Regression: the first distance formula returned 0 for every interior
        // pixel at radius 0, so a solid square rendered at 50% alpha
        // everywhere. Sample the interior rather than just one corner.
        let (w, h) = (12, 12);
        let mut buf = canvas(w, h);
        {
            let mut c = Canvas::new(&mut buf, w, h);
            c.rounded_rect(0.0, 0.0, 12.0, 12.0, 0.0, Rgba::new(255, 255, 255, 255));
        }
        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    pixel(&buf, w, x, y).3,
                    255,
                    "pixel ({x},{y}) should be fully opaque"
                );
            }
        }
    }

    #[test]
    fn circular_geometry_does_not_panic_on_float_rounding() {
        // Regression: the level meter's bars are drawn with radius == w/2, and
        // `x + w - r` rounds one ulp below `x + r` in f32. `clamp` panics on
        // min > max, which killed the whole overlay thread mid-render.
        // These are the real numbers from the 18-bar meter.
        let bar_w = (166.0f32 - 3.0 * 17.0) / 18.0; // 6.3888893
        let mut buf = canvas(240, 60);
        let mut c = Canvas::new(&mut buf, 240, 60);
        for i in 0..18 {
            let x = 50.0 + i as f32 * (bar_w + 3.0);
            let bar_h = bar_w; // the exact w == h == 2r case
            let y = 26.0 - bar_h / 2.0;
            c.rounded_rect(x, y, bar_w, bar_h, bar_w / 2.0, Rgba::new(255, 0, 0, 255));
        }
    }

    #[test]
    fn awkward_float_dimensions_never_panic() {
        // Sweep sizes where w/h and the radius are mutually irrational-ish,
        // since any of them could reproduce the same clamp inversion.
        let mut buf = canvas(64, 64);
        let mut c = Canvas::new(&mut buf, 64, 64);
        for i in 1..60 {
            let w = i as f32 / 3.0;
            let h = i as f32 / 7.0;
            c.rounded_rect(1.5, 2.25, w, h, w / 2.0, Rgba::new(1, 2, 3, 128));
            c.rounded_rect(0.1, 0.2, w, h, h / 2.0, Rgba::new(1, 2, 3, 128));
            c.circle(i as f32 / 5.0, i as f32 / 11.0, w / 2.0, Rgba::new(4, 5, 6, 90));
        }
    }

    #[test]
    fn a_perfect_circle_still_fills_its_centre() {
        // Guard against "fixed the panic by drawing nothing".
        let mut buf = canvas(16, 16);
        {
            let mut c = Canvas::new(&mut buf, 16, 16);
            c.circle(8.0, 8.0, 6.0, Rgba::new(255, 255, 255, 255));
        }
        assert_eq!(pixel(&buf, 16, 8, 8).3, 255, "circle centre must be solid");
        assert_eq!(pixel(&buf, 16, 0, 0).3, 0, "circle must not fill the corner");
    }

    #[test]
    fn zero_and_negative_sizes_are_harmless() {
        let mut buf = canvas(8, 8);
        let mut c = Canvas::new(&mut buf, 8, 8);
        c.rounded_rect(2.0, 2.0, 0.0, 0.0, 0.0, Rgba::new(255, 0, 0, 255));
        c.rounded_rect(2.0, 2.0, -5.0, -5.0, 3.0, Rgba::new(255, 0, 0, 255));
        c.circle(4.0, 4.0, 0.0, Rgba::new(255, 0, 0, 255));
    }

    #[test]
    fn clear_resets_to_transparent() {
        let (w, h) = (4, 4);
        let mut buf = canvas(w, h);
        let mut c = Canvas::new(&mut buf, w, h);
        c.rounded_rect(0.0, 0.0, 4.0, 4.0, 0.0, Rgba::new(1, 2, 3, 255));
        c.clear();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn fade_scales_alpha_only() {
        let c = Rgba::new(10, 20, 30, 200);
        let f = c.fade(0.5);
        assert_eq!((f.r, f.g, f.b), (10, 20, 30));
        assert_eq!(f.a, 100);
        assert_eq!(c.fade(0.0).a, 0);
        assert_eq!(c.fade(5.0).a, 200, "factor should clamp at 1.0");
    }
}
