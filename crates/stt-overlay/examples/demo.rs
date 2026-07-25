//! Standalone overlay demo: sweeps the level meter, then feeds a growing
//! transcript so the elision and glass text rendering can be inspected.
use std::time::Duration;
use stt_overlay::{Overlay, OverlayAnchor};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let overlay = Overlay::spawn(OverlayAnchor::Bottom)?;
    overlay.show();

    // Meter only, no transcript yet.
    for i in 0..45 {
        let t = i as f32 / 30.0;
        overlay.level(0.09 * (1.0 + (t * 3.0).sin()) * 0.5 + 0.01);
        std::thread::sleep(Duration::from_millis(33));
    }

    // A transcript that grows, exactly as the live preview delivers it.
    let full = "speech recognition on Linux should work offline and respect your privacy";
    let words: Vec<&str> = full.split(' ').collect();
    for n in 1..=words.len() {
        overlay.text(words[..n].join(" "));
        for i in 0..24 {
            let t = (n * 24 + i) as f32 / 30.0;
            overlay.level(0.09 * (1.0 + (t * 3.0).sin()) * 0.5 + 0.01);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    overlay.transcribing();
    std::thread::sleep(Duration::from_millis(1200));
    overlay.hide();
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
