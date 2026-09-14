//! Microphone capture for recording sessions.
//!
//! cpal 0.15 input `Stream`s are `!Send + !Sync` (same bound as the output
//! path in `audio.rs`), so the stream is built and owned by a dedicated
//! thread that lives for the duration of one recording. The cpal callback
//! only converts samples to interleaved i16 and forwards them over a channel;
//! the owning thread does the WAV writing so file IO never runs inside the
//! realtime callback.
//!
//! Capture is best-effort by design: a missing input device, a denied OS
//! microphone permission, or an unsupported sample format logs a warning and
//! the recording proceeds without a mic track — it must never fail a session.
//!
//! While the emulation is paused the callback discards samples, so the mic
//! timeline stays aligned with the frame-indexed video timeline (both exclude
//! pause segments).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};

pub struct MicRecorder {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    clock_discontinuity: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<bool>>,
    wav_path: PathBuf,
    clock: Arc<Mutex<MicClockObservation>>,
}

/// The first CoreAudio capture timestamp represented on the same mach host
/// clock as ScreenCaptureKit PTS on macOS. Later samples are positioned by
/// count/rate; callback residuals are retained as an uncertainty bound.
#[derive(Debug, Clone, Copy, Default)]
pub struct MicClockObservation {
    pub first_capture_mach_us: Option<u64>,
    pub uncertainty_us: u64,
    pub sample_rate: u32,
    pub frames_written: u64,
    // Deliberate recording pauses remove both video and mic wall time. Keep
    // residual measurements within a continuous capture segment so a user
    // pause is not misreported as clock jitter.
    segment_first_capture_mach_us: Option<u64>,
    segment_frames_written: u64,
}

#[derive(Debug, Clone)]
pub struct MicCaptureOutcome {
    pub path: PathBuf,
    pub clock: MicClockObservation,
}

impl MicRecorder {
    /// Spawn the capture thread writing to `wav_path`. Never fails: device or
    /// permission problems are logged on the capture thread and `stop` then
    /// returns `None`.
    pub fn start(wav_path: PathBuf, level: Option<Arc<AtomicU32>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let clock_discontinuity = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_paused = paused.clone();
        let thread_discontinuity = clock_discontinuity.clone();
        let thread_path = wav_path.clone();
        let clock = Arc::new(Mutex::new(MicClockObservation::default()));
        let thread_clock = clock.clone();
        let thread_level = level.clone();
        let join = thread::Builder::new()
            .name("retrofeel-mic".into())
            .spawn(move || {
                capture_thread(
                    thread_path,
                    thread_stop,
                    thread_paused,
                    thread_discontinuity,
                    thread_clock,
                    thread_level,
                )
            })
            .map_err(|error| log::warn!("mic capture: failed to spawn thread: {error}"))
            .ok();
        Self {
            stop,
            paused,
            clock_discontinuity,
            join,
            wav_path,
            clock,
        }
    }

    /// Discard incoming samples while paused so the mic track skips pause
    /// segments, matching the video's frame-indexed timeline.
    pub fn set_paused(&self, paused: bool) {
        let was_paused = self.paused.swap(paused, Ordering::SeqCst);
        if was_paused && !paused {
            self.clock_discontinuity.store(true, Ordering::SeqCst);
        }
    }

    /// Stop capture, finalize the WAV, and return its path if samples were
    /// written successfully.
    pub fn stop(mut self) -> Option<MicCaptureOutcome> {
        if self.shutdown() {
            Some(MicCaptureOutcome {
                path: self.wav_path.clone(),
                clock: self.clock.lock().map(|clock| *clock).unwrap_or_default(),
            })
        } else {
            None
        }
    }

    fn shutdown(&mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        self.join
            .take()
            .map(|join| join.join().unwrap_or(false))
            .unwrap_or(false)
    }
}

impl Drop for MicRecorder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn capture_thread(
    wav_path: PathBuf,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    clock_discontinuity: Arc<AtomicBool>,
    clock: Arc<Mutex<MicClockObservation>>,
    level: Option<Arc<AtomicU32>>,
) -> bool {
    let ok = run_capture(
        &wav_path,
        &stop,
        &paused,
        &clock_discontinuity,
        &clock,
        level,
    );
    if !ok {
        // Don't leave a partial/empty WAV behind — the writer thread treats
        // `mic.wav` existence as "a mic track was captured".
        let _ = std::fs::remove_file(&wav_path);
    }
    ok
}

fn run_capture(
    wav_path: &PathBuf,
    stop: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    clock_discontinuity: &Arc<AtomicBool>,
    clock: &Arc<Mutex<MicClockObservation>>,
    level: Option<Arc<AtomicU32>>,
) -> bool {
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        log::warn!("mic capture: no default input device; recording without a mic track");
        return false;
    };
    let default_config = match device.default_input_config() {
        Ok(config) => config,
        Err(error) => {
            log::warn!("mic capture: no default input config: {error}");
            return false;
        }
    };
    let sample_format = default_config.sample_format();
    let config: cpal::StreamConfig = default_config.into();

    let mut writer = match hound::WavWriter::create(
        wav_path,
        hound::WavSpec {
            channels: config.channels,
            sample_rate: config.sample_rate.0,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    ) {
        Ok(writer) => writer,
        Err(error) => {
            log::warn!(
                "mic capture: failed to create {}: {error}",
                wav_path.display()
            );
            return false;
        }
    };

    let (sender, receiver) = unbounded::<CapturedSamples>();
    let stream = match build_stream(
        &device,
        &config,
        sample_format,
        sender,
        paused.clone(),
        level,
    ) {
        Some(stream) => stream,
        None => return false,
    };
    if let Err(error) = stream.play() {
        log::warn!("mic capture: failed to start input stream: {error}");
        return false;
    }
    log::info!(
        "mic capture started: {} ch @ {} Hz -> {}",
        config.channels,
        config.sample_rate.0,
        wav_path.display()
    );

    let mut wrote_any = false;
    while !stop.load(Ordering::SeqCst) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(samples) => {
                if !write_samples(&mut writer, &samples.samples) {
                    return false;
                }
                observe_capture_clock(
                    clock,
                    clock_discontinuity,
                    samples.capture_mach_us,
                    samples.samples.len(),
                    &config,
                );
                wrote_any = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Dropping the stream drops the callback's sender; drain what's left.
    drop(stream);
    while let Ok(samples) = receiver.try_recv() {
        if !write_samples(&mut writer, &samples.samples) {
            return false;
        }
        observe_capture_clock(
            clock,
            clock_discontinuity,
            samples.capture_mach_us,
            samples.samples.len(),
            &config,
        );
        wrote_any = true;
    }

    match writer.finalize() {
        Ok(()) => wrote_any,
        Err(error) => {
            log::warn!("mic capture: failed to finalize WAV: {error}");
            false
        }
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    sender: Sender<CapturedSamples>,
    paused: Arc<AtomicBool>,
    level: Option<Arc<AtomicU32>>,
) -> Option<cpal::Stream> {
    let i16_level = level.clone();
    let u16_level = level.clone();
    let f32_level = level;
    let result = match sample_format {
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], info: &cpal::InputCallbackInfo| {
                if !paused.load(Ordering::Relaxed) {
                    publish_peak(
                        &i16_level,
                        data.iter()
                            .map(|sample| f32::from(sample.unsigned_abs()) / i16::MAX as f32)
                            .fold(0.0_f32, f32::max),
                    );
                    let _ = sender.send(CapturedSamples {
                        samples: data.to_vec(),
                        capture_mach_us: capture_mach_us(info),
                    });
                }
            },
            stream_error,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], info: &cpal::InputCallbackInfo| {
                if !paused.load(Ordering::Relaxed) {
                    publish_peak(
                        &u16_level,
                        data.iter()
                            .map(|sample| {
                                ((i32::from(*sample) - 0x8000).unsigned_abs() as f32)
                                    / i16::MAX as f32
                            })
                            .fold(0.0_f32, f32::max),
                    );
                    let _ = sender.send(CapturedSamples {
                        samples: data
                            .iter()
                            .map(|&sample| (sample as i32 - 0x8000) as i16)
                            .collect(),
                        capture_mach_us: capture_mach_us(info),
                    });
                }
            },
            stream_error,
            None,
        ),
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                if !paused.load(Ordering::Relaxed) {
                    publish_peak(
                        &f32_level,
                        data.iter()
                            .map(|sample| sample.abs().min(1.0))
                            .fold(0.0_f32, f32::max),
                    );
                    let _ = sender.send(CapturedSamples {
                        samples: data
                            .iter()
                            .map(|&sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                            .collect(),
                        capture_mach_us: capture_mach_us(info),
                    });
                }
            },
            stream_error,
            None,
        ),
        other => {
            log::warn!("mic capture: unsupported input sample format {other:?}");
            return None;
        }
    };
    match result {
        Ok(stream) => Some(stream),
        Err(error) => {
            log::warn!("mic capture: failed to build input stream: {error}");
            None
        }
    }
}

fn publish_peak(level: &Option<Arc<AtomicU32>>, peak: f32) {
    if let Some(level) = level {
        level.store(peak.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct CapturedSamples {
    samples: Vec<i16>,
    capture_mach_us: Option<u64>,
}

fn observe_capture_clock(
    clock: &Arc<Mutex<MicClockObservation>>,
    clock_discontinuity: &Arc<AtomicBool>,
    capture_mach_us: Option<u64>,
    sample_count: usize,
    config: &cpal::StreamConfig,
) {
    let Ok(mut clock) = clock.lock() else {
        return;
    };
    if clock_discontinuity.swap(false, Ordering::SeqCst) {
        clock.segment_first_capture_mach_us = None;
        clock.segment_frames_written = 0;
    }
    let frames = sample_count as u64 / config.channels.max(1) as u64;
    if clock.sample_rate == 0 {
        clock.sample_rate = config.sample_rate.0;
    }
    if let Some(capture_mach_us) = capture_mach_us {
        if let Some(first) = clock.segment_first_capture_mach_us {
            let expected = first.saturating_add(
                clock.segment_frames_written.saturating_mul(1_000_000)
                    / clock.sample_rate.max(1) as u64,
            );
            let residual = capture_mach_us.abs_diff(expected);
            // Include a small measurement envelope for the interval between
            // CoreAudio's callback timestamp and this Rust callback.
            clock.uncertainty_us = clock.uncertainty_us.max(residual.saturating_add(2_000));
        } else {
            clock.first_capture_mach_us.get_or_insert(capture_mach_us);
            clock.segment_first_capture_mach_us = Some(capture_mach_us);
            clock.uncertainty_us = 2_000;
        }
    }
    clock.frames_written = clock.frames_written.saturating_add(frames);
    clock.segment_frames_written = clock.segment_frames_written.saturating_add(frames);
}

/// CPAL's CoreAudio backend represents `capture` with mach absolute time but
/// keeps the raw value private. Capture a nearby mach reading and subtract the
/// precisely reported callback-to-capture interval; the residual is retained
/// in `MicClockObservation` instead of being hidden.
#[cfg(target_os = "macos")]
fn capture_mach_us(info: &cpal::InputCallbackInfo) -> Option<u64> {
    let callback_after_capture = info
        .timestamp()
        .callback
        .duration_since(&info.timestamp().capture)?;
    mach_host_time_us().map(|now| now.saturating_sub(callback_after_capture.as_micros() as u64))
}

#[cfg(not(target_os = "macos"))]
fn capture_mach_us(_: &cpal::InputCallbackInfo) -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // CPAL/CoreAudio uses the same mach host-time basis.
fn mach_host_time_us() -> Option<u64> {
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    if unsafe { libc::mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
        return None;
    }
    let ticks = unsafe { libc::mach_absolute_time() } as u128;
    u64::try_from(ticks * timebase.numer as u128 / timebase.denom as u128 / 1_000).ok()
}

fn write_samples(
    writer: &mut hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    samples: &[i16],
) -> bool {
    for &sample in samples {
        if let Err(error) = writer.write_sample(sample) {
            log::warn!("mic capture: WAV write failed: {error}");
            return false;
        }
    }
    true
}

fn stream_error(error: cpal::StreamError) {
    log::warn!("mic capture stream error: {error}");
}
