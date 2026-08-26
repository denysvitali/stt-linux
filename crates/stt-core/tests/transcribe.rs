//! Golden-audio regression tests.
//!
//! Requires the model to be downloaded, so it is gated behind the
//! `model-tests` feature and skipped by default:
//!
//! ```text
//! stt model download
//! cargo test -p stt-core --features model-tests --test transcribe
//! ```
//!
//! Assertions are on word error rate, not exact strings: ASR output is not
//! bit-stable across `ort` and model revisions, so an exact match would be a
//! test that fails for the wrong reasons.

#![cfg(feature = "model-tests")]

use stt_core::config::EngineConfig;

/// Normalize for comparison: lowercase, strip punctuation, collapse spaces.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Levenshtein distance over words, divided by reference length.
fn word_error_rate(reference: &str, hypothesis: &str) -> f32 {
    let r = words(reference);
    let h = words(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }

    let mut prev: Vec<usize> = (0..=h.len()).collect();
    let mut curr = vec![0usize; h.len() + 1];
    for (i, rw) in r.iter().enumerate() {
        curr[0] = i + 1;
        for (j, hw) in h.iter().enumerate() {
            let cost = usize::from(rw != hw);
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[h.len()] as f32 / r.len() as f32
}

fn fixture(name: &str) -> (std::path::PathBuf, String) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("assets/fixtures");
    let wav = root.join(format!("{name}.wav"));
    let text = std::fs::read_to_string(root.join(format!("{name}.txt")))
        .unwrap_or_else(|e| panic!("reading {name}.txt: {e}"));
    (wav, text.trim().to_owned())
}

fn transcribe_fixture(name: &str) -> (String, String) {
    let (wav, truth) = fixture(name);
    let pcm = stt_core::wav::read_as_mono_16k(&wav).expect("reading fixture");
    let mut engine = stt_core::engine::load(&EngineConfig::default())
        .expect("loading the engine — run `stt model download` first");
    let got = engine.transcribe(&pcm).expect("transcribing").text;
    (truth, got)
}

/// Threshold is deliberately loose: the fixtures are synthetic `flite` speech,
/// which is harder for ASR than real voices, and the point is to catch gross
/// breakage (wrong sample rate, silent audio, wrong weights), not to track
/// small accuracy drift.
const MAX_WER: f32 = 0.15;

#[test]
fn transcribes_the_quick_brown_fox() {
    let (truth, got) = transcribe_fixture("quick-brown-fox");
    let wer = word_error_rate(&truth, &got);
    println!("truth: {truth}\ngot:   {got}\nWER:   {wer:.3}");
    assert!(wer <= MAX_WER, "WER {wer:.3} exceeds {MAX_WER}\ngot: {got}");
}

#[test]
fn transcribes_the_invoice_sentence() {
    let (truth, got) = transcribe_fixture("invoice");
    let wer = word_error_rate(&truth, &got);
    println!("truth: {truth}\ngot:   {got}\nWER:   {wer:.3}");
    assert!(wer <= MAX_WER, "WER {wer:.3} exceeds {MAX_WER}\ngot: {got}");
}

/// A transducer must emit nothing for silence. This is the property that
/// motivated choosing Parakeet over Whisper, so it is worth asserting.
#[test]
fn silence_does_not_hallucinate_text() {
    let silence = vec![0.0f32; 16_000 * 3];
    let mut engine = stt_core::engine::load(&EngineConfig::default())
        .expect("loading the engine — run `stt model download` first");
    let got = engine.transcribe(&silence).expect("transcribing").text;
    assert!(
        got.trim().is_empty(),
        "three seconds of silence produced text: {got:?}"
    );
}

#[test]
fn empty_input_is_handled_without_calling_the_model() {
    let mut engine = stt_core::engine::load(&EngineConfig::default())
        .expect("loading the engine — run `stt model download` first");
    assert_eq!(engine.transcribe(&[]).unwrap().text, "");
}

#[cfg(test)]
mod wer_self_tests {
    use super::word_error_rate;

    #[test]
    fn identical_strings_score_zero() {
        assert_eq!(word_error_rate("hello world", "hello world"), 0.0);
    }

    #[test]
    fn punctuation_and_case_are_ignored() {
        assert_eq!(word_error_rate("Hello, World!", "hello world"), 0.0);
    }

    #[test]
    fn one_wrong_word_in_four() {
        assert!((word_error_rate("a b c d", "a b x d") - 0.25).abs() < 1e-6);
    }

    #[test]
    fn empty_hypothesis_scores_one() {
        assert_eq!(word_error_rate("a b c", ""), 1.0);
    }
}
