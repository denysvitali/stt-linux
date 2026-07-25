//! Microphone capture.
//!
//! Two threads and a lock-free queue:
//!
//! ```text
//!   cpal callback  ──push──►  SPSC ring  ──pop──►  worker thread
//!   (real-time)                                    (downmix, resample)
//! ```
//!
//! The callback runs under real-time constraints: it converts to `f32`, mixes
//! to mono into a reused buffer, and pushes. It never allocates, never locks
//! and never touches the resampler. Everything expensive happens on the worker.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::TARGET_SAMPLE_RATE;
use crate::resample::{Resample, downmix_to_mono, rms};

/// Ring capacity in mono frames — about two seconds at 48 kHz. Large enough
/// that a briefly descheduled worker cannot drop audio, small enough to notice
/// if one wedges entirely.
const RING_FRAMES: usize = 96_000;

/// How much audio the worker folds into one level-meter update (~30 Hz).
const LEVEL_WINDOW: usize = 1600;

/// State shared between the callback, the worker and the controlling thread.
#[derive(Debug)]
struct Shared {
    /// Latest RMS level, as `f32::to_bits`. Atomic so the overlay feeder can
    /// read it without blocking the worker.
    level: AtomicU32,
    stop: AtomicBool,
    /// Frames the callback could not push because the ring was full. Non-zero
    /// means audio was genuinely lost, so it is surfaced rather than ignored.
    dropped: AtomicU64,
    /// Resampled 16 kHz mono audio accumulated so far.
    ///
    /// Shared rather than owned by the worker so that a live preview can
    /// transcribe the audio captured up to now without interrupting capture.
    samples: Mutex<Vec<f32>>,
}

/// A live recording. Dropping this stops capture and discards the audio; call
/// [`stop`](Self::stop) to get the samples.
pub struct Recording {
    stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
    started: Instant,
    input_rate: u32,
    channels: u16,
}

impl Recording {
    /// Open `selector` and start capturing.
    ///
    /// Uses the device's own default input configuration, which on a PipeWire
    /// system is the format the server is already running — asking for
    /// anything else invites a resample we did not choose.
    pub fn start(selector: &str) -> Result<Self> {
        let device = crate::audio::find_input_device(selector)?;
        let supported = device
            .default_input_config()
            .context("querying the default input configuration")?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let input_rate = config.sample_rate;
        let channels = config.channels;

        anyhow::ensure!(channels > 0, "device reports zero input channels");

        let shared = Arc::new(Shared {
            level: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            samples: Mutex::new(Vec::new()),
        });

        let (mut producer, mut consumer) =
            ringbuf::HeapRb::<f32>::new(RING_FRAMES).split();

        // --- the real-time callback ---------------------------------------
        let cb_shared = Arc::clone(&shared);
        // Reused across callbacks so the hot path performs no allocation.
        let mut mono_scratch: Vec<f32> = Vec::with_capacity(4096);
        let mut push_mono = move |interleaved: &[f32]| {
            mono_scratch.clear();
            downmix_to_mono(interleaved, channels as usize, &mut mono_scratch);
            let pushed = producer.push_slice(&mono_scratch);
            if pushed < mono_scratch.len() {
                cb_shared
                    .dropped
                    .fetch_add((mono_scratch.len() - pushed) as u64, Ordering::Relaxed);
            }
        };

        let on_error = |err| tracing::error!(error = %err, "audio input stream error");

        // cpal streams are typed by sample format, so each supported format
        // needs its own `build_input_stream` call. The closure body is shared.
        macro_rules! build {
            ($sample:ty) => {{
                let mut convert_scratch: Vec<f32> = Vec::with_capacity(4096);
                device.build_input_stream(
                    config,
                    move |data: &[$sample], _: &cpal::InputCallbackInfo| {
                        convert_scratch.clear();
                        convert_scratch.extend(
                            data.iter().map(|s| cpal::Sample::to_sample::<f32>(*s)),
                        );
                        push_mono(&convert_scratch);
                    },
                    on_error,
                    None,
                )
            }};
        }

        let stream = match sample_format {
            // f32 needs no conversion pass at all.
            cpal::SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| push_mono(data),
                on_error,
                None,
            ),
            cpal::SampleFormat::I8 => build!(i8),
            cpal::SampleFormat::I16 => build!(i16),
            cpal::SampleFormat::I24 => build!(cpal::I24),
            cpal::SampleFormat::I32 => build!(i32),
            cpal::SampleFormat::U8 => build!(u8),
            cpal::SampleFormat::U16 => build!(u16),
            cpal::SampleFormat::F64 => build!(f64),
            other => anyhow::bail!("unsupported sample format {other:?}"),
        }
        .context("building the input stream")?;

        stream.play().context("starting the input stream")?;

        // --- the worker ----------------------------------------------------
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("stt-capture".into())
            .spawn(move || -> Result<()> {
                let mut resampler = Resample::new(input_rate)?;
                let mut scratch = vec![0.0f32; 4096];
                let mut level_acc: Vec<f32> = Vec::with_capacity(LEVEL_WINDOW);
                // Resampler output for one iteration, appended to the shared
                // buffer under a brief lock rather than holding it throughout.
                let mut produced: Vec<f32> = Vec::with_capacity(4096);

                loop {
                    let n = consumer.pop_slice(&mut scratch);
                    if n == 0 {
                        if worker_shared.stop.load(Ordering::Acquire) {
                            break;
                        }
                        // Nothing buffered yet; yield rather than spin.
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    let block = &scratch[..n];

                    level_acc.extend_from_slice(block);
                    if level_acc.len() >= LEVEL_WINDOW {
                        worker_shared
                            .level
                            .store(rms(&level_acc).to_bits(), Ordering::Relaxed);
                        level_acc.clear();
                    }

                    produced.clear();
                    resampler.push(block, &mut produced)?;
                    if !produced.is_empty() {
                        worker_shared.samples.lock().unwrap().extend_from_slice(&produced);
                    }
                }

                produced.clear();
                resampler.finish(&mut produced)?;
                if !produced.is_empty() {
                    worker_shared.samples.lock().unwrap().extend_from_slice(&produced);
                }
                Ok(())
            })
            .context("spawning the capture worker")?;

        tracing::debug!(
            rate = input_rate,
            channels,
            ?sample_format,
            "capture started"
        );

        Ok(Self {
            stream: Some(stream),
            shared,
            worker: Some(worker),
            started: Instant::now(),
            input_rate,
            channels,
        })
    }

    /// Current input amplitude in `[0, 1]`, for the overlay meter.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The audio captured so far, without interrupting capture.
    ///
    /// Used by the live preview: the transcript shown while you speak comes
    /// from re-running the ordinary engine over this growing buffer, which is
    /// why the preview and the final text can never disagree about which model
    /// produced them.
    pub fn snapshot(&self) -> Vec<f32> {
        self.shared.samples.lock().unwrap().clone()
    }

    /// Duration of the audio captured so far.
    pub fn captured_secs(&self) -> f32 {
        self.shared.samples.lock().unwrap().len() as f32 / TARGET_SAMPLE_RATE as f32
    }

    /// Stop capturing and return the recorded audio as 16 kHz mono.
    pub fn stop(mut self) -> Result<Vec<f32>> {
        // Order matters: drop the stream first so the callback stops producing,
        // *then* tell the worker to drain and exit. Reversing this races the
        // worker against a still-live callback and truncates the tail.
        drop(self.stream.take());
        self.shared.stop.store(true, Ordering::Release);

        self.worker
            .take()
            .expect("worker is taken only here")
            .join()
            .map_err(|_| anyhow::anyhow!("the capture worker panicked"))??;

        let samples = std::mem::take(&mut *self.shared.samples.lock().unwrap());
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(
                frames = dropped,
                "audio dropped: the capture worker could not keep up"
            );
        }
        Ok(samples)
    }

    /// Frames lost to a full ring buffer. Should always be zero.
    pub fn dropped_frames(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Only runs when `stop` was not called; make sure the worker still exits.
        drop(self.stream.take());
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
