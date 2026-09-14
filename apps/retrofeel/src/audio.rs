//! Low-latency audio output for libretro PCM.
//!
//! A launch-scoped audio thread creates, plays, and drops the CPAL stream on
//! the same thread. The realtime callback owns both the ring consumer and the
//! resampler, so it performs no allocation and takes no locks.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use ringbuf::traits::*;
use ringbuf::{HeapCons, HeapProd, HeapRb};

const MIN_RING_FRAMES: usize = 1_024;
const FADE_FRAMES: usize = 48;
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Owns one launch's audio thread. Dropping it stops playback, drops the CPAL
/// stream on its creator thread, and joins the thread before returning.
#[derive(bevy::prelude::Resource)]
pub struct AudioOutput {
    shutdown: mpsc::SyncSender<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Default)]
struct AudioStats {
    fill_frames: AtomicUsize,
    underruns: AtomicU64,
    overflow_samples: AtomicU64,
}

/// Producer half used by the core/worker-event thread.
pub struct AudioProd {
    inner: Option<HeapProd<i16>>,
    stats: Arc<AudioStats>,
    fast_forward: Arc<AtomicBool>,
}

impl AudioProd {
    /// Push interleaved stereo i16 PCM. During fast-forward audio is muted and
    /// discarded so normal playback never drains a stale high-speed backlog.
    pub fn push(&mut self, samples: &[i16]) {
        if self.fast_forward.load(Ordering::Relaxed) {
            return;
        }
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let pushed = inner.push_slice(samples);
        if pushed < samples.len() {
            self.stats
                .overflow_samples
                .fetch_add((samples.len() - pushed) as u64, Ordering::Relaxed);
        }
    }
}

/// Build a bounded ring and an owned CPAL stream.
pub fn build(
    core_sample_rate: f64,
    volume: Arc<std::sync::atomic::AtomicU32>,
    latency_ms: u32,
    fast_forward: Arc<AtomicBool>,
) -> Result<(AudioProd, AudioOutput), AudioError> {
    if !core_sample_rate.is_finite() || core_sample_rate <= 0.0 {
        return Err(AudioError::InvalidCoreRate(core_sample_rate));
    }

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioError::NoOutputDevice)?;
    let supported = device.default_output_config()?;
    let device_sample_rate = f64::from(supported.sample_rate().0);
    let device_channels = usize::from(supported.channels());
    let sample_format = supported.sample_format();
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let core_latency_frames =
        (core_sample_rate * f64::from(latency_ms.max(1)) / 1_000.0).ceil() as usize;
    let ring_frames = core_latency_frames.max(MIN_RING_FRAMES);
    let prebuffer_frames = (core_latency_frames / 2).clamp(2, ring_frames / 2);
    let ring = HeapRb::<i16>::new(ring_frames * 2);
    let (producer, consumer) = ring.split();
    let stats = Arc::new(AudioStats::default());

    let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let thread_stats = Arc::clone(&stats);
    let thread_fast_forward = Arc::clone(&fast_forward);
    let join = thread::Builder::new()
        .name("retrofeel-audio".into())
        .spawn(move || {
            let result = run_audio_thread(
                device,
                config,
                sample_format,
                device_channels,
                core_sample_rate,
                device_sample_rate,
                prebuffer_frames,
                consumer,
                volume,
                thread_fast_forward,
                Arc::clone(&thread_stats),
                shutdown_rx,
                ready_tx,
            );
            if let Err(error) = result {
                log::error!("audio thread failed: {error}");
            }
        })
        .map_err(AudioError::SpawnThread)?;

    match ready_rx.recv() {
        Ok(Ok(())) => {
            log::info!(
                "audio started: core_rate={core_sample_rate:.2} device_rate={device_sample_rate:.2} channels={device_channels} ring_frames={ring_frames} prebuffer_frames={prebuffer_frames}"
            );
            Ok((
                AudioProd {
                    inner: Some(producer),
                    stats,
                    fast_forward,
                },
                AudioOutput {
                    shutdown: shutdown_tx,
                    join: Some(join),
                },
            ))
        }
        Ok(Err(error)) => {
            let _ = join.join();
            Err(AudioError::Initialize(error))
        }
        Err(_) => {
            let _ = join.join();
            Err(AudioError::ThreadExited)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_audio_thread(
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
    device_channels: usize,
    core_rate: f64,
    device_rate: f64,
    prebuffer_frames: usize,
    consumer: HeapCons<i16>,
    volume: Arc<std::sync::atomic::AtomicU32>,
    fast_forward: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
    shutdown: mpsc::Receiver<()>,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), AudioError> {
    let mut renderer = Some(Renderer::new(
        consumer,
        core_rate / device_rate,
        prebuffer_frames,
        fast_forward,
        Arc::clone(&stats),
    ));
    let stream = match sample_format {
        SampleFormat::I16 => {
            let mut renderer = renderer.take().expect("audio renderer missing");
            let volume = Arc::clone(&volume);
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    render(
                        &mut renderer,
                        data,
                        device_channels,
                        load_volume(&volume),
                        |v| v,
                        0,
                    );
                },
                stream_error,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut renderer = renderer.take().expect("audio renderer missing");
            let volume = Arc::clone(&volume);
            device.build_output_stream(
                &config,
                move |data: &mut [u16], _| {
                    render(
                        &mut renderer,
                        data,
                        device_channels,
                        load_volume(&volume),
                        i16_to_u16,
                        32_768,
                    );
                },
                stream_error,
                None,
            )
        }
        SampleFormat::F32 => {
            let mut renderer = renderer.take().expect("audio renderer missing");
            let volume = Arc::clone(&volume);
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    render(
                        &mut renderer,
                        data,
                        device_channels,
                        load_volume(&volume),
                        i16_to_f32,
                        0.0,
                    );
                },
                stream_error,
                None,
            )
        }
        other => {
            let error = AudioError::UnsupportedFormat(other);
            let _ = ready.send(Err(error.to_string()));
            return Err(error);
        }
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            let error = AudioError::BuildStream(error);
            let _ = ready.send(Err(error.to_string()));
            return Err(error);
        }
    };
    if let Err(error) = stream.play() {
        let error = AudioError::Play(error);
        let _ = ready.send(Err(error.to_string()));
        return Err(error);
    }
    let _ = ready.send(Ok(()));

    let mut previous_underruns = 0;
    let mut previous_overflows = 0;
    loop {
        match shutdown.recv_timeout(STATS_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let underruns = stats.underruns.load(Ordering::Relaxed);
                let overflows = stats.overflow_samples.load(Ordering::Relaxed);
                if underruns != previous_underruns || overflows != previous_overflows {
                    log::warn!(
                        "audio health: core_rate={core_rate:.2} device_rate={device_rate:.2} fill_frames={} underruns={} overflow_samples={}",
                        stats.fill_frames.load(Ordering::Relaxed),
                        underruns,
                        overflows,
                    );
                    previous_underruns = underruns;
                    previous_overflows = overflows;
                }
            }
        }
    }
    drop(stream);
    log::debug!("audio stream stopped");
    Ok(())
}

fn stream_error(error: cpal::StreamError) {
    log::error!("cpal audio callback error: {error}");
}

struct Renderer {
    consumer: HeapCons<i16>,
    /// Input frames advanced for each output frame (`core_rate/device_rate`).
    step: f64,
    phase: f64,
    current: (i16, i16),
    next: (i16, i16),
    playing: bool,
    prebuffer_frames: usize,
    fade_remaining: usize,
    last_output: (i16, i16),
    fast_forward: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
}

impl Renderer {
    fn new(
        consumer: HeapCons<i16>,
        step: f64,
        prebuffer_frames: usize,
        fast_forward: Arc<AtomicBool>,
        stats: Arc<AudioStats>,
    ) -> Self {
        Self {
            consumer,
            step,
            phase: 0.0,
            current: (0, 0),
            next: (0, 0),
            playing: false,
            prebuffer_frames,
            fade_remaining: 0,
            last_output: (0, 0),
            fast_forward,
            stats,
        }
    }

    fn available_frames(&self) -> usize {
        self.consumer.occupied_len() / 2
    }

    fn pop_frame(&mut self) -> Option<(i16, i16)> {
        // The producer publishes interleaved samples. Avoid consuming the
        // left channel while a concurrent batch push has not published the
        // matching right channel yet.
        if self.consumer.occupied_len() < 2 {
            return None;
        }
        let left = self.consumer.try_pop()?;
        let right = self.consumer.try_pop()?;
        Some((left, right))
    }

    fn clear_and_rebuffer(&mut self) {
        while self.consumer.try_pop().is_some() {}
        self.playing = false;
        self.phase = 0.0;
        self.fade_remaining = 0;
        self.last_output = (0, 0);
        self.stats.fill_frames.store(0, Ordering::Relaxed);
    }

    fn start_if_ready(&mut self) {
        if self.playing || self.available_frames() < self.prebuffer_frames.max(2) {
            return;
        }
        let Some(current) = self.pop_frame() else {
            return;
        };
        let Some(next) = self.pop_frame() else {
            return;
        };
        self.current = current;
        self.next = next;
        self.phase = 0.0;
        self.playing = true;
        self.fade_remaining = 0;
    }

    fn next_frame(&mut self) -> (i16, i16) {
        if self.fast_forward.load(Ordering::Relaxed) {
            self.clear_and_rebuffer();
            return (0, 0);
        }
        self.start_if_ready();
        if !self.playing {
            self.stats
                .fill_frames
                .store(self.available_frames(), Ordering::Relaxed);
            return self.fade_frame();
        }

        let output = (
            lerp(self.current.0, self.next.0, self.phase),
            lerp(self.current.1, self.next.1, self.phase),
        );
        self.last_output = output;
        self.phase += self.step;
        while self.phase >= 1.0 {
            self.phase -= 1.0;
            self.current = self.next;
            let Some(next) = self.pop_frame() else {
                self.playing = false;
                self.fade_remaining = FADE_FRAMES;
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
                break;
            };
            self.next = next;
        }
        self.stats
            .fill_frames
            .store(self.available_frames(), Ordering::Relaxed);
        output
    }

    fn fade_frame(&mut self) -> (i16, i16) {
        if self.fade_remaining == 0 {
            self.last_output = (0, 0);
            return (0, 0);
        }
        let gain = self.fade_remaining as f32 / FADE_FRAMES as f32;
        self.fade_remaining -= 1;
        let output = (
            (self.last_output.0 as f32 * gain) as i16,
            (self.last_output.1 as f32 * gain) as i16,
        );
        if self.fade_remaining == 0 {
            self.last_output = (0, 0);
        }
        output
    }
}

fn render<T: Copy>(
    renderer: &mut Renderer,
    out: &mut [T],
    channels: usize,
    volume: f32,
    convert: fn(i16) -> T,
    silence: T,
) {
    let channels = channels.max(1);
    for frame in out.chunks_exact_mut(channels) {
        let (left, right) = renderer.next_frame();
        let left = apply_volume(left, volume);
        let right = apply_volume(right, volume);
        if channels == 1 {
            frame[0] = convert(((i32::from(left) + i32::from(right)) / 2) as i16);
        } else {
            frame[0] = convert(left);
            frame[1] = convert(right);
            frame[2..].fill(silence);
        }
    }
}

fn load_volume(volume: &std::sync::atomic::AtomicU32) -> f32 {
    f32::from_bits(volume.load(Ordering::Relaxed))
}

/// No-op producer used when output is disabled or unavailable.
pub fn dummy_prod(fast_forward: Arc<AtomicBool>) -> AudioProd {
    AudioProd {
        inner: None,
        stats: Arc::new(AudioStats::default()),
        fast_forward,
    }
}

fn lerp(a: i16, b: i16, phase: f64) -> i16 {
    (f64::from(a) + (f64::from(b) - f64::from(a)) * phase) as i16
}

fn apply_volume(sample: i16, volume: f32) -> i16 {
    if (volume - 1.0).abs() < 1e-3 {
        return sample;
    }
    (sample as f32 * volume).clamp(-32_768.0, 32_767.0) as i16
}

fn i16_to_u16(sample: i16) -> u16 {
    (i32::from(sample) + 32_768) as u16
}

fn i16_to_f32(sample: i16) -> f32 {
    f32::from(sample) / 32_768.0
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio output device available")]
    NoOutputDevice,
    #[error("unsupported sample format: {0:?}")]
    UnsupportedFormat(SampleFormat),
    #[error("invalid core sample rate: {0}")]
    InvalidCoreRate(f64),
    #[error("failed to get default output config: {0}")]
    DefaultConfig(#[from] cpal::DefaultStreamConfigError),
    #[error("failed to build output stream: {0}")]
    BuildStream(#[from] cpal::BuildStreamError),
    #[error("failed to play stream: {0}")]
    Play(#[from] cpal::PlayStreamError),
    #[error("failed to spawn audio thread: {0}")]
    SpawnThread(std::io::Error),
    #[error("audio thread exited during initialization")]
    ThreadExited,
    #[error("audio initialization failed: {0}")]
    Initialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(
        step: f64,
        prebuffer: usize,
        capacity_frames: usize,
    ) -> (HeapProd<i16>, Renderer, Arc<AtomicBool>, Arc<AudioStats>) {
        let ring = HeapRb::new(capacity_frames * 2);
        let (producer, consumer) = ring.split();
        let fast_forward = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AudioStats::default());
        let renderer = Renderer::new(
            consumer,
            step,
            prebuffer,
            Arc::clone(&fast_forward),
            Arc::clone(&stats),
        );
        (producer, renderer, fast_forward, stats)
    }

    fn ramp(frames: usize) -> Vec<i16> {
        (0..frames)
            .flat_map(|frame| {
                let sample = frame.min(i16::MAX as usize) as i16;
                [sample, -sample]
            })
            .collect()
    }

    #[test]
    fn advances_by_core_rate_over_device_rate_for_snes_audio() {
        let (mut producer, mut renderer, _, _) = harness(32_040.0 / 48_000.0, 2, 40_000);
        producer.push_slice(&ramp(32_050));
        let mut last = (0, 0);
        for _ in 0..48_000 {
            last = renderer.next_frame();
        }
        assert!((i32::from(last.0) - 32_039).abs() <= 1, "{last:?}");
        assert!((i32::from(last.1) + 32_039).abs() <= 1, "{last:?}");
    }

    #[test]
    fn equal_rate_preserves_samples_and_callback_boundaries() {
        let source = ramp(32);
        let (mut producer_a, mut renderer_a, _, _) = harness(1.0, 2, 64);
        producer_a.push_slice(&source);
        let mut whole = vec![0i16; 24];
        render(&mut renderer_a, &mut whole, 2, 1.0, |v| v, 0);

        let (mut producer_b, mut renderer_b, _, _) = harness(1.0, 2, 64);
        producer_b.push_slice(&source);
        let mut split = vec![0i16; 24];
        render(&mut renderer_b, &mut split[..10], 2, 1.0, |v| v, 0);
        render(&mut renderer_b, &mut split[10..], 2, 1.0, |v| v, 0);
        assert_eq!(whole, split);
        assert_eq!(&whole[..20], &source[..20]);
    }

    #[test]
    fn resamples_44100_in_both_directions_without_pitch_drift() {
        for (input_rate, output_rate) in [(44_100.0, 48_000.0), (48_000.0, 44_100.0)] {
            let input_frames = input_rate as usize + 4;
            let output_frames = output_rate as usize;
            let (mut producer, mut renderer, _, stats) =
                harness(input_rate / output_rate, 2, input_frames + 8);
            producer.push_slice(&ramp(input_frames));
            let mut last = (0, 0);
            for _ in 0..output_frames {
                last = renderer.next_frame();
            }
            assert!(
                renderer.available_frames() <= 6,
                "{input_rate}->{output_rate}: {} input frames remained",
                renderer.available_frames()
            );
            assert_ne!(last, (0, 0));
            assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn prebuffers_then_fades_and_rebuffers_after_underrun() {
        let (mut producer, mut renderer, _, stats) = harness(1.0, 4, 32);
        producer.push_slice(&[10, -10, 20, -20, 30, -30]);
        assert_eq!(renderer.next_frame(), (0, 0));
        producer.push_slice(&[40, -40, 50, -50, 60, -60, 70, -70, 80, -80]);
        assert_ne!(renderer.next_frame(), (0, 0));
        for _ in 0..16 {
            let _ = renderer.next_frame();
        }
        assert!(stats.underruns.load(Ordering::Relaxed) > 0);
        for _ in 0..FADE_FRAMES {
            let _ = renderer.next_frame();
        }
        assert_eq!(renderer.next_frame(), (0, 0));
        producer.push_slice(&[100, -100, 200, -200, 300, -300, 400, -400]);
        assert_eq!(renderer.next_frame(), (100, -100));
    }

    #[test]
    fn overflow_is_counted_and_fast_forward_flushes_backlog() {
        let ring = HeapRb::new(8);
        let (producer, consumer) = ring.split();
        let stats = Arc::new(AudioStats::default());
        let fast_forward = Arc::new(AtomicBool::new(false));
        let mut audio_prod = AudioProd {
            inner: Some(producer),
            stats: Arc::clone(&stats),
            fast_forward: Arc::clone(&fast_forward),
        };
        audio_prod.push(&[1; 20]);
        assert_eq!(stats.overflow_samples.load(Ordering::Relaxed), 12);

        let mut renderer = Renderer::new(
            consumer,
            1.0,
            2,
            Arc::clone(&fast_forward),
            Arc::clone(&stats),
        );
        fast_forward.store(true, Ordering::Relaxed);
        assert_eq!(renderer.next_frame(), (0, 0));
        assert_eq!(renderer.available_frames(), 0);
        audio_prod.push(&[9; 8]);
        assert_eq!(renderer.available_frames(), 0);
    }

    #[test]
    fn dropping_audio_output_stops_and_joins_owner_thread() {
        let (shutdown, receiver) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let join = thread::spawn(move || {
            receiver.recv().unwrap();
            thread_stopped.store(true, Ordering::Release);
        });
        drop(AudioOutput {
            shutdown,
            join: Some(join),
        });
        assert!(stopped.load(Ordering::Acquire));
    }
}
