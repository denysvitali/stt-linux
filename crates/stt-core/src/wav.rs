//! WAV read/write for the 16 kHz mono f32 buffers the pipeline passes around.
//!
//! Used by `stt record` and `stt transcribe`, and by the golden-audio tests.

use anyhow::{Context, Result};
use std::path::Path;

use crate::audio::TARGET_SAMPLE_RATE;

/// Write 16 kHz mono samples as 16-bit PCM.
///
/// 16-bit rather than float: every tool can play it, and the ASR models
/// quantize to this precision anyway.
pub fn write_mono_16k(path: &Path, samples: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for &s in samples {
        // Clamp before scaling: a sample above 1.0 would otherwise wrap to
        // full-scale negative and sound like a click.
        let clamped = s.clamp(-1.0, 1.0);
        writer.write_sample((clamped * i16::MAX as f32) as i16)?;
    }
    writer.finalize().context("finalizing WAV")?;
    Ok(())
}

/// Read any WAV into mono f32, resampling to 16 kHz if needed.
pub fn read_as_mono_16k(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .context("reading float samples")?,
        hound::SampleFormat::Int => {
            // Normalize by the format's full scale so 24-bit input is not 256x
            // too quiet.
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()
                .context("reading integer samples")?
        }
    };

    let mut mono = Vec::with_capacity(interleaved.len() / spec.channels.max(1) as usize);
    crate::resample::downmix_to_mono(&interleaved, spec.channels as usize, &mut mono);

    if spec.sample_rate == TARGET_SAMPLE_RATE {
        return Ok(mono);
    }
    let mut resampler = crate::resample::Resample::new(spec.sample_rate)?;
    let mut out = Vec::new();
    resampler.push(&mono, &mut out)?;
    resampler.finish(&mut out)?;
    Ok(out)
}

/// Duration of a 16 kHz buffer.
pub fn duration_secs(samples: &[f32]) -> f32 {
    samples.len() as f32 / TARGET_SAMPLE_RATE as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let samples = sine(440.0, TARGET_SAMPLE_RATE, 0.25);

        write_mono_16k(&path, &samples).unwrap();
        let read = read_as_mono_16k(&path).unwrap();

        assert_eq!(read.len(), samples.len());
        for (a, b) in samples.iter().zip(&read) {
            // 16-bit quantization is the only permitted loss.
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn written_file_has_the_expected_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        write_mono_16k(&path, &sine(440.0, TARGET_SAMPLE_RATE, 0.1)).unwrap();

        let spec = hound::WavReader::open(&path).unwrap().spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, 16);
    }

    #[test]
    fn out_of_range_samples_clamp_instead_of_wrapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.wav");
        write_mono_16k(&path, &[2.0, -2.0, 0.0]).unwrap();
        let read = read_as_mono_16k(&path).unwrap();
        assert!(read[0] > 0.99, "positive overflow wrapped: {}", read[0]);
        assert!(read[1] < -0.99, "negative overflow wrapped: {}", read[1]);
    }

    #[test]
    fn resamples_a_48k_file_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("48k.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for s in sine(440.0, 48_000, 1.0) {
            w.write_sample((s * i16::MAX as f32) as i16).unwrap();
        }
        w.finalize().unwrap();

        let read = read_as_mono_16k(&path).unwrap();
        let expected = TARGET_SAMPLE_RATE as usize;
        assert!(
            read.len().abs_diff(expected) <= expected / 50,
            "expected ~{expected} samples at 16k, got {}",
            read.len()
        );
    }

    #[test]
    fn downmixes_a_stereo_file_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        // Left at full scale, right silent -> mono should be half scale.
        for _ in 0..1000 {
            w.write_sample(i16::MAX / 2).unwrap();
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();

        let read = read_as_mono_16k(&path).unwrap();
        assert_eq!(read.len(), 1000);
        assert!((read[10] - 0.25).abs() < 1e-2, "got {}", read[10]);
    }

    #[test]
    fn duration_is_computed_at_16k() {
        assert!((duration_secs(&vec![0.0; 16_000]) - 1.0).abs() < 1e-6);
        assert_eq!(duration_secs(&[]), 0.0);
    }
}
