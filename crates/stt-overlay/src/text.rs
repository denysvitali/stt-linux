//! Minimal single-line text rendering.
//!
//! Enough to show a live transcript inside the overlay: one line, one font, no
//! shaping. That is a real limitation — it lays glyphs out left to right by
//! advance width, so scripts needing shaping or bidi (Arabic, Hebrew, Devanagari)
//! will render incorrectly. Latin, Cyrillic and Greek — which covers Parakeet
//! TDT's 25 languages — are fine.

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use std::collections::HashMap;

use crate::paint::{Canvas, Rgba};

/// Whether the character immediately before byte offset `i` is whitespace.
fn prev_is_space(s: &str, i: usize) -> bool {
    s[..i].chars().next_back().is_some_and(char::is_whitespace)
}

/// A rasterized glyph, cached because the same characters recur constantly
/// as a transcript grows.
struct Glyph {
    bitmap: Vec<u8>,
    width: usize,
    height: usize,
    /// Offset from the pen position to the top-left of the bitmap.
    xmin: i32,
    ymin: i32,
    advance: f32,
}

pub struct TextRenderer {
    font: Font,
    size: f32,
    cache: HashMap<char, Glyph>,
    /// Distance from the baseline to the top of the tallest glyph.
    ascent: f32,
    /// Height of a capital letter, measured from the font rather than
    /// estimated. Used for optical centring — see [`Self::draw_centered`].
    cap_height: f32,
}

impl TextRenderer {
    /// Load a UI font from the system.
    ///
    /// No font is embedded: shipping one means picking a licence and adding a
    /// megabyte to the binary, and every desktop already has a sans-serif.
    /// If none is found the caller falls back to a text-free overlay.
    pub fn from_system(size: f32) -> Result<Self> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();

        // Prefer a UI sans in the same spirit as the rest of the indicator.
        let query = fontdb::Query {
            families: &[
                fontdb::Family::Name("Inter"),
                fontdb::Family::Name("Noto Sans"),
                fontdb::Family::Name("DejaVu Sans"),
                fontdb::Family::Name("Liberation Sans"),
                fontdb::Family::SansSerif,
            ],
            weight: fontdb::Weight::MEDIUM,
            ..Default::default()
        };
        let id = db.query(&query).context("no sans-serif font found")?;

        let data = db
            .with_face_data(id, |data, index| {
                Font::from_bytes(data, FontSettings {
                    collection_index: index,
                    scale: size,
                    ..FontSettings::default()
                })
            })
            .context("reading font data")?
            .map_err(|e| anyhow::anyhow!("parsing font: {e}"))?;

        let metrics = data
            .horizontal_line_metrics(size)
            .context("font has no horizontal metrics")?;

        // Measure the cap height rather than assuming a ratio: 'H' sits on the
        // baseline, so its rasterized height is exactly the cap height for
        // whatever font the system gave us.
        let cap_height = {
            let (m, _) = data.rasterize('H', size);
            if m.height > 0 {
                m.height as f32
            } else {
                // Some fonts have no 'H' (icon fonts); fall back to a typical
                // 0.7 em rather than collapsing to zero.
                size * 0.70
            }
        };

        Ok(Self {
            font: data,
            size,
            cache: HashMap::new(),
            ascent: metrics.ascent,
            cap_height,
        })
    }

    pub fn cap_height(&self) -> f32 {
        self.cap_height
    }

    fn glyph(&mut self, c: char) -> &Glyph {
        self.cache.entry(c).or_insert_with(|| {
            let (metrics, bitmap) = self.font.rasterize(c, self.size);
            Glyph {
                bitmap,
                width: metrics.width,
                height: metrics.height,
                xmin: metrics.xmin,
                ymin: metrics.ymin,
                advance: metrics.advance_width,
            }
        })
    }

    /// Total advance width of `s` at the current size.
    pub fn measure(&mut self, s: &str) -> f32 {
        s.chars().map(|c| self.glyph(c).advance).sum()
    }

    /// Trim `s` from the *left* until it fits `max_width`, prefixing an
    /// ellipsis and cutting at a word boundary.
    ///
    /// The tail is what matters in a live transcript: the newest words are the
    /// ones the user is checking, and the beginning has already scrolled out
    /// of interest.
    ///
    /// Cutting mid-word produces genuinely confusing output — eliding
    /// "speech recognition" to "h recognition" reads as a different word
    /// rather than a truncation — so whole words are dropped instead. A single
    /// word too long for the budget still falls back to a character cut,
    /// because dropping it entirely would show nothing at all.
    pub fn fit_tail(&mut self, s: &str, max_width: f32) -> String {
        if self.measure(s) <= max_width {
            return s.to_string();
        }
        let ellipsis = "… ";
        let budget = (max_width - self.measure(ellipsis)).max(0.0);

        // Byte offsets at which a word starts, newest last.
        let starts: Vec<usize> = s
            .char_indices()
            .filter(|(i, c)| !c.is_whitespace() && (*i == 0 || prev_is_space(s, *i)))
            .map(|(i, _)| i)
            .collect();

        // The latest word boundary whose tail still fits.
        let mut best: Option<usize> = None;
        for &start in starts.iter().rev() {
            if self.measure(&s[start..]) <= budget {
                best = Some(start);
            } else {
                break;
            }
        }

        if let Some(start) = best {
            return format!("{ellipsis}{}", &s[start..]);
        }

        // Not even the final word fits; fall back to a character cut so the
        // user still sees the most recent characters.
        let chars: Vec<char> = s.chars().collect();
        let mut width = 0.0;
        let mut take = chars.len();
        for (i, &c) in chars.iter().enumerate().rev() {
            let w = self.glyph(c).advance;
            if width + w > budget {
                take = i + 1;
                break;
            }
            width += w;
            take = i;
        }
        let tail: String = chars[take..].iter().collect();
        format!("{ellipsis}{tail}")
    }

    /// Draw `s` with its baseline positioned so the line box starts at `y`.
    /// Returns the advance width actually drawn.
    ///
    /// Prefer [`draw_centered`](Self::draw_centered) for anything being
    /// vertically centred in a container.
    pub fn draw(&mut self, canvas: &mut Canvas<'_>, x: f32, y: f32, s: &str, colour: Rgba) -> f32 {
        self.draw_at_baseline(canvas, x, y + self.ascent, s, colour)
    }

    fn draw_at_baseline(
        &mut self,
        canvas: &mut Canvas<'_>,
        x: f32,
        baseline: f32,
        s: &str,
        colour: Rgba,
    ) -> f32 {
        let mut pen = x;
        for c in s.chars() {
            // Copy the fields needed for blitting so the immutable borrow of
            // the cache ends before `canvas` is touched.
            let (bitmap, gw, gh, xmin, ymin, advance) = {
                let g = self.glyph(c);
                (
                    g.bitmap.clone(),
                    g.width,
                    g.height,
                    g.xmin,
                    g.ymin,
                    g.advance,
                )
            };

            if gw > 0 && gh > 0 {
                let gx = (pen + xmin as f32).round() as i32;
                // fontdue's ymin is the offset of the bitmap bottom below the
                // baseline, so the top edge sits ascent-height above it.
                let gy = (baseline - (gh as f32 + ymin as f32)).round() as i32;
                for row in 0..gh {
                    for col in 0..gw {
                        let coverage = bitmap[row * gw + col] as f32 / 255.0;
                        if coverage > 0.0 {
                            canvas.blend(gx + col as i32, gy + row as i32, colour, coverage);
                        }
                    }
                }
            }
            pen += advance;
        }
        pen - x
    }

    /// Draw `s` optically centred on the horizontal line `cy`.
    ///
    /// Centring on the cap-height box, not the line box and not the ink box.
    ///
    /// The line box is wrong because it includes the font's internal leading,
    /// which is asymmetric — positioning by it put the text about 3px low in a
    /// 52px pill, which is visible. The ink box is wrong because it depends on
    /// whether the current words happen to contain a descender, so the text
    /// would bob up and down as a live transcript grows. The cap box is stable
    /// for a given font and is what the eye reads as the centre of a line.
    pub fn draw_centered(
        &mut self,
        canvas: &mut Canvas<'_>,
        x: f32,
        cy: f32,
        s: &str,
        colour: Rgba,
    ) -> f32 {
        let baseline = cy + self.cap_height * 0.5;
        self.draw_at_baseline(canvas, x, baseline, s, colour)
    }

    pub fn line_height(&self) -> f32 {
        self.size * 1.2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skipped on machines with no fonts installed (some CI images).
    fn renderer() -> Option<TextRenderer> {
        TextRenderer::from_system(16.0).ok()
    }

    #[test]
    fn measures_wider_for_longer_text() {
        let Some(mut r) = renderer() else { return };
        let short = r.measure("hi");
        let long = r.measure("hello there");
        assert!(long > short, "{long} should exceed {short}");
        assert_eq!(r.measure(""), 0.0);
    }

    #[test]
    fn short_text_is_returned_unchanged() {
        let Some(mut r) = renderer() else { return };
        assert_eq!(r.fit_tail("hello", 10_000.0), "hello");
    }

    #[test]
    fn long_text_is_trimmed_from_the_left() {
        let Some(mut r) = renderer() else { return };
        let s = "the quick brown fox jumps over the lazy dog";
        let fitted = r.fit_tail(s, 120.0);
        assert!(fitted.starts_with('…'), "should mark the elision: {fitted:?}");
        // The tail is what survives, because that is the newest speech.
        assert!(s.ends_with(fitted.trim_start_matches(['…', ' '])), "{fitted:?}");
        assert!(r.measure(&fitted) <= 121.0, "still too wide: {fitted:?}");
    }

    #[test]
    fn elision_cuts_at_a_word_boundary() {
        // Regression: eliding "speech recognition on Linux" produced
        // "… h recognition on Linux", which reads as a different word rather
        // than a truncation.
        let Some(mut r) = renderer() else { return };
        let s = "speech recognition on Linux should work offline";
        let ellipsis_w = r.measure("… ");
        let last_word_w = r.measure(s.rsplit(' ').next().unwrap());

        for budget in [90.0, 140.0, 200.0, 260.0, 320.0] {
            // The word-boundary guarantee only holds when at least the final
            // word fits; below that a character cut is the correct fallback,
            // covered by `a_single_oversized_word_still_shows_something`.
            assert!(
                budget - ellipsis_w >= last_word_w,
                "test budget {budget} is too small to exercise word cutting"
            );

            let fitted = r.fit_tail(s, budget);
            let body = fitted.trim_start_matches(['…', ' ']);
            let offset = s.find(body).expect("the tail must come from the source");
            assert!(
                offset == 0 || s[..offset].ends_with(char::is_whitespace),
                "budget {budget}: {fitted:?} starts mid-word"
            );
            assert!(r.measure(&fitted) <= budget + 1.0, "{fitted:?} overflows");
        }
    }

    #[test]
    fn a_single_oversized_word_still_shows_something() {
        let Some(mut r) = renderer() else { return };
        // No word boundary can help here; a character cut is correct.
        let fitted = r.fit_tail("Donaudampfschifffahrtsgesellschaft", 60.0);
        assert!(fitted.starts_with('…'));
        assert!(fitted.chars().count() > 2, "showed nothing at all: {fitted:?}");
    }

    #[test]
    fn fits_within_an_absurdly_small_budget_without_panicking() {
        let Some(mut r) = renderer() else { return };
        let out = r.fit_tail("some words here", 1.0);
        assert!(out.starts_with('…'));
    }

    #[test]
    fn drawing_marks_pixels_and_advances() {
        let Some(mut r) = renderer() else { return };
        let (w, h) = (200, 40);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let advance = {
            let mut c = Canvas::new(&mut buf, w, h);
            r.draw(&mut c, 4.0, 8.0, "Hello", Rgba::new(255, 255, 255, 255))
        };
        assert!(advance > 0.0);
        assert!(
            buf.iter().any(|&b| b != 0),
            "drawing text produced a blank canvas"
        );
    }

    #[test]
    fn drawing_off_canvas_is_harmless() {
        let Some(mut r) = renderer() else { return };
        let (w, h) = (16, 16);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut c = Canvas::new(&mut buf, w, h);
        // Far outside in both directions; must clip, not panic.
        r.draw(&mut c, -500.0, -500.0, "clipped", Rgba::new(255, 0, 0, 255));
        r.draw(&mut c, 900.0, 900.0, "clipped", Rgba::new(255, 0, 0, 255));
    }

    #[test]
    fn unicode_outside_ascii_does_not_panic() {
        let Some(mut r) = renderer() else { return };
        let (w, h) = (300, 40);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut c = Canvas::new(&mut buf, w, h);
        r.draw(&mut c, 2.0, 2.0, "café — naïve — Привет", Rgba::new(255, 255, 255, 255));
    }
}
