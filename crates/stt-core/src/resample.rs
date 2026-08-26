//! Mono resampling to the 16 kHz the ASR engines require.
//!
//! Device rates are fixed for the lifetime of a stream, so this uses rubato's
//! *synchronous* FFT resampler rather than the asynchronous one: the ratio
//! never changes, and the FFT path is both faster and cleaner for a fixed
//! rational ratio like 48000:16000.

use anyhow::{Context, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

/// Frames of input consumed per `process` call. 1024 at 48 kHz is ~21 ms,
/// which keeps the level meter responsive without thrashing the FFT.
const CHUNK: usize = 1024;

/// Streaming mono resampler.
///
/// Feed it arbitrary-length slices with [`push`](Self::push); it buffers
/// whatever does not fill a chunk and emits output as chunks complete. Call
/// [`finish`](Self::finish) to flush the tail.
pub struct Resample {
    inner: Option<Fft<f32>>,
    /// Input frames not yet consumed by a full chunk.
    pending: Vec<f32>,
    input_rate: u32,
}

impl Resample {
    pub fn new(input_rate: u32) -> Result<Self> {
        anyhow::ensure!(input_rate > 0, "input sample rate must be non-zero");
        // Identity: skip the FFT entirely rather than paying for a 1:1 pass.
        let inner = if input_rate == crate::audio::TARGET_SAMPLE_RATE {
            None
        } else {
            Some(
                Fft::<f32>::new(
                    input_rate as usize,
                    crate::audio::TARGET_SAMPLE_RATE as usize,
                    CHUNK,
                    1,
                    FixedSync::Input,
                )
                .context("constructing the resampler")?,
            )
        };
        Ok(Self {
            inner,
            pending: Vec::with_capacity(CHUNK * 2),
            input_rate,
        })
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// Whether this resampler is a pass-through.
    pub fn is_identity(&self) -> bool {
        self.inner.is_none()
    }

    /// Consume `samples` (mono, at the input rate), appending 16 kHz output to
    /// `out`.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) -> Result<()> {
        let Some(resampler) = self.inner.as_mut() else {
            out.extend_from_slice(samples);
            return Ok(());
        };

        self.pending.extend_from_slice(samples);
        while self.pending.len() >= CHUNK {
            let input = InterleavedSlice::new(&self.pending[..CHUNK], 1, CHUNK)
                .map_err(|e| anyhow::anyhow!("wrapping resampler input: {e}"))?;
            let produced = resampler
                .process(&input, None)
                .context("resampling audio")?;
            out.extend_from_slice(&produced.take_data());
            self.pending.drain(..CHUNK);
        }
        Ok(())
    }

    /// Flush the final partial chunk.
    ///
    /// Without this the last <21 ms of a recording is dropped — inaudible in
    /// music, but in dictation it is the end of the final word.
    pub fn finish(mut self, out: &mut Vec<f32>) -> Result<()> {
        let Some(resampler) = self.inner.as_mut() else {
            out.append(&mut self.pending);
            return Ok(());
        };
        if self.pending.is_empty() {
            return Ok(());
        }

        // `partial_len` tells rubato how much of the chunk is real; the rest is
        // zero padding it should not treat as signal.
        let partial = self.pending.len();
        self.pending.resize(CHUNK, 0.0);
        let input = InterleavedSlice::new(&self.pending[..CHUNK], 1, CHUNK)
            .map_err(|e| anyhow::anyhow!("wrapping resampler input: {e}"))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(partial),
            active_channels_mask: None,
        };
        let produced = resampler
            .process(&input, Some(&indexing))
            .context("flushing the resampler")?;
        out.extend_from_slice(&produced.take_data());
        Ok(())
    }
}

/// Average interleaved frames down to one channel.
///
/// Called from the audio callback, so it must not allocate: the caller owns
/// `out` and reuses it.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    debug_assert!(channels > 0);
    if channels == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let scale = 1.0 / channels as f32;
    for frame in interleaved.chunks_exact(channels) {
        out.push(frame.iter().sum::<f32>() * scale);
    }
}

/// Root-mean-square amplitude, for the overlay's level meter.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::TARGET_SAMPLE_RATE;

    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Feed a signal through in irregular slices, as a real audio callback would.
    fn run(input: &[f32], rate: u32, slice: usize) -> Vec<f32> {
        let mut r = Resample::new(rate).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(slice) {
            r.push(chunk, &mut out).unwrap();
        }
        r.finish(&mut out).unwrap();
        out
    }

    #[test]
    fn identity_rate_is_pass_through() {
        let r = Resample::new(TARGET_SAMPLE_RATE).unwrap();
        assert!(r.is_identity());
        let input = sine(440.0, TARGET_SAMPLE_RATE, 0.1);
        let out = run(&input, TARGET_SAMPLE_RATE, 333);
        assert_eq!(out, input, "pass-through must not alter samples");
    }

    #[test]
    fn downsamples_48k_to_16k_with_the_right_length() {
        let secs = 1.0;
        let input = sine(440.0, 48_000, secs);
        let out = run(&input, 48_000, 1024);
        let expected = (TARGET_SAMPLE_RATE as f32 * secs) as usize;
        // FFT resampling has a little edge slop; a few ms either way is fine,
        // a 3x error would mean the ratio is inverted.
        let tolerance = expected / 50;
        assert!(
            out.len().abs_diff(expected) <= tolerance,
            "got {} samples, expected ~{expected}",
            out.len()
        );
    }

    #[test]
    fn handles_non_integer_ratio_44100_to_16000() {
        let input = sine(440.0, 44_100, 1.0);
        let out = run(&input, 44_100, 1024);
        let expected = TARGET_SAMPLE_RATE as usize;
        assert!(
            out.len().abs_diff(expected) <= expected / 50,
            "got {} samples, expected ~{expected}",
            out.len()
        );
    }

    #[test]
    fn output_is_independent_of_input_slicing() {
        // The audio callback hands us whatever buffer size it likes; that must
        // not change the result.
        let input = sine(440.0, 48_000, 0.5);
        let a = run(&input, 48_000, 128);
        let b = run(&input, 48_000, 1024);
        let c = run(&input, 48_000, 4099);
        assert_eq!(a.len(), b.len());
        assert_eq!(b.len(), c.len());
        for (i, ((x, y), z)) in a.iter().zip(&b).zip(&c).enumerate() {
            assert!(
                (x - y).abs() < 1e-5 && (y - z).abs() < 1e-5,
                "divergence at {i}: {x} {y} {z}"
            );
        }
    }

    #[test]
    fn preserves_signal_amplitude() {
        // A resampler that silently zeroed its output would still pass the
        // length checks above.
        let input = sine(440.0, 48_000, 0.5);
        let out = run(&input, 48_000, 1024);
        let in_rms = rms(&input);
        let out_rms = rms(&out);
        assert!(
            (in_rms - out_rms).abs() < 0.05,
            "amplitude changed: {in_rms} -> {out_rms}"
        );
    }

    #[test]
    fn tail_is_not_dropped() {
        // Length deliberately not a multiple of CHUNK.
        let input = sine(440.0, 48_000, 0.0)
            .into_iter()
            .chain(sine(440.0, 48_000, 0.037))
            .collect::<Vec<_>>();
        assert_ne!(input.len() % CHUNK, 0);
        let out = run(&input, 48_000, 1024);
        assert!(!out.is_empty(), "the whole tail was discarded");
    }

    #[test]
    fn downmix_averages_channels() {
        let mut out = Vec::new();
        // Two frames of stereo: (1.0, 0.0) and (0.5, 0.5).
        downmix_to_mono(&[1.0, 0.0, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn downmix_mono_is_a_copy() {
        let mut out = Vec::new();
        downmix_to_mono(&[0.1, -0.2, 0.3], 1, &mut out);
        assert_eq!(out, vec![0.1, -0.2, 0.3]);
    }

    #[test]
    fn downmix_ignores_a_trailing_partial_frame() {
        let mut out = Vec::new();
        // Five samples of stereo is two whole frames plus a stray.
        downmix_to_mono(&[1.0, 1.0, 0.0, 0.0, 1.0], 2, &mut out);
        assert_eq!(out, vec![1.0, 0.0]);
    }

    #[test]
    fn rms_of_silence_and_full_scale() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 64]), 0.0);
        assert!((rms(&[1.0, -1.0, 1.0, -1.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn zero_input_rate_is_rejected() {
        assert!(Resample::new(0).is_err());
    }
}
