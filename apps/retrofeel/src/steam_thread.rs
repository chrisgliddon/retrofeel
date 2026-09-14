//! Steam game capture thread manager.
//!
//! Mirrors `core_thread.rs` but replaces `retro_run()` with a
//! `retrofeel-steamcapture` session: ScreenCaptureKit video + CGEventTap input
//! of a Wine process. The capture frame callback IS the clock — one SCK frame
//! = one input sample = one `RecordingHandle::frame` call, preserving the
//! 1:1 video↔input-log invariant.
//!
//! No game audio is captured in v1 (the `audio.wav` / core-audio path is
//! skipped). Mic capture is handled by `RecordingHandle` → `MicRecorder`
//! exactly as in the libretro path.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use retrofeel_types::config::AudioConfig;
use retrofeel_types::{
    CaptureProvenance, CaptureSourceKind, InputState, TrackAlignment, TrackAlignmentMetadata,
    TrackAlignmentStatus, TrackPresence, VideoTiming, VideoTimingKind,
};

use crate::core_thread::{
    CoreFrame, CoreStatus, GameSource, InputSnapshot, LatestFrame, LatestInput, SourceKind,
};
use crate::recording::{RecordingHandle, RecordingStart};

/// Commands sent to the Steam worker thread.
#[derive(Debug, Clone)]
pub enum SteamCommand {
    StartRecording {
        recordings_dir: PathBuf,
        stop_after_frames: Option<u64>,
    },
    StopRecording,
    SetPaused(bool),
}

/// Handle to a running Steam capture session. Exposes the same surface as
/// `CoreHandle` via `GameSource`.
pub struct SteamHandle {
    pub input: LatestInput,
    pub latest: LatestFrame,
    pub status: CoreStatus,
    pub fps: f64,
    pub sample_rate: f64,
    pub base_width: u32,
    pub base_height: u32,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    mic_peak: Arc<AtomicU32>,
    commands: Sender<SteamCommand>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for SteamHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.commands.send(SteamCommand::SetPaused(false));
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl SteamHandle {
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        let _ = self.commands.send(SteamCommand::SetPaused(paused));
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.to_bits(), Ordering::SeqCst);
    }

    pub fn start_recording(&self, recordings_dir: PathBuf) {
        let _ = self.commands.send(SteamCommand::StartRecording {
            recordings_dir,
            stop_after_frames: None,
        });
    }

    pub fn start_recording_for(&self, recordings_dir: PathBuf, frames: u64) {
        let _ = self.commands.send(SteamCommand::StartRecording {
            recordings_dir,
            stop_after_frames: Some(frames),
        });
    }

    pub fn start_recording_debug(
        &self,
        recordings_dir: PathBuf,
        frames: u64,
        _pause_at_frame: Option<u64>,
        _pause_duration_ms: u64,
    ) {
        let _ = self.commands.send(SteamCommand::StartRecording {
            recordings_dir,
            stop_after_frames: Some(frames),
        });
    }

    pub fn stop_recording(&self) {
        let _ = self.commands.send(SteamCommand::StopRecording);
    }

    pub fn set_fast_forward(&self, _on: bool) {
        // No-op for Steam — pacing is driven by the SCK frame callback, not a
        // fixed fps deadline. Fast-forward doesn't apply to a live capture.
    }
}

impl GameSource for SteamHandle {
    fn kind(&self) -> SourceKind {
        SourceKind::Steam
    }
    fn input_slot(&self) -> &LatestInput {
        &self.input
    }
    fn latest_frame_slot(&self) -> &LatestFrame {
        &self.latest
    }
    fn status(&self) -> &CoreStatus {
        &self.status
    }
    fn base_width(&self) -> u32 {
        self.base_width
    }
    fn base_height(&self) -> u32 {
        self.base_height
    }
    fn fps(&self) -> f64 {
        self.fps
    }
    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
    fn mic_peak(&self) -> f32 {
        f32::from_bits(self.mic_peak.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }
    fn set_paused(&self, paused: bool) {
        SteamHandle::set_paused(self, paused);
    }
    fn set_volume(&self, volume: f32) {
        SteamHandle::set_volume(self, volume);
    }
    fn set_fast_forward(&self, on: bool) {
        SteamHandle::set_fast_forward(self, on);
    }
    fn start_recording(&self, recordings_dir: PathBuf) {
        SteamHandle::start_recording(self, recordings_dir);
    }
    fn start_recording_for(&self, recordings_dir: PathBuf, frames: u64) {
        SteamHandle::start_recording_for(self, recordings_dir, frames);
    }
    fn start_recording_debug(
        &self,
        recordings_dir: PathBuf,
        frames: u64,
        pause_at_frame: Option<u64>,
        pause_duration_ms: u64,
    ) {
        SteamHandle::start_recording_debug(
            self,
            recordings_dir,
            frames,
            pause_at_frame,
            pause_duration_ms,
        );
    }
    fn stop_recording(&self) {
        SteamHandle::stop_recording(self);
    }
    fn save_state(&self, _slot: u8) {
        log::warn!("steam: save_state is not supported for Steam game sessions");
    }
    fn load_state(&self, _slot: u8) {
        log::warn!("steam: load_state is not supported for Steam game sessions");
    }
    fn reset(&self) {
        log::warn!("steam: reset is not supported for Steam game sessions");
    }
}

/// Configuration for launching a Steam game under Wine.
#[derive(Debug, Clone)]
pub struct SteamTarget {
    pub app_id: u32,
    pub name: String,
    pub install_dir: PathBuf,
    /// The Wine prefix directory (e.g. ~/FoM-wine).
    pub wine_prefix: PathBuf,
    /// Path to the wine binary (e.g. /opt/homebrew/bin/wine).
    pub wine_bin: PathBuf,
    /// The .exe filename inside install_dir to launch.
    pub exe_name: String,
    /// Capture configuration.
    pub capture_max_width: u32,
    pub capture_max_height: u32,
    pub capture_fps: u32,
    /// Mic + local transcription config from RecordingConfig.
    pub capture_mic: bool,
    pub transcription: retrofeel_types::TranscriptionConfig,
    /// When set, attach to an already-running process by PID instead of
    /// launching Wine. The game was launched externally (e.g. by GameHub).
    pub attach_pid: Option<u32>,
    /// The platform Steam client launched the game with `steam://run`. Steam
    /// remains external: we wait only for its exact game window and never
    /// create, own, or kill a Steam child process.
    pub native_steam_launch: bool,
    /// Bounded native-Steam window wait, supplied from `SteamConfig`.
    pub native_attach_timeout_seconds: u32,
}

/// Errors from launching a Steam capture session.
#[derive(Debug, thiserror::Error)]
pub enum SteamThreadError {
    #[error("a Steam capture attach is already in progress")]
    AlreadyLaunching,
    #[error("failed to start Steam attach worker: {0}")]
    LaunchWorker(std::io::Error),
    #[error("failed to launch wine: {0}")]
    LaunchWine(std::io::Error),
    #[error("failed to start capture: {0}")]
    Capture(String),
    #[error("wine process exited immediately")]
    ProcessExited,
}

/// Spawn a Steam capture session. Launches Wine, waits for the process to
/// appear, starts SCK capture, and returns a `SteamHandle`.
pub fn spawn(
    target: SteamTarget,
    audio_config: AudioConfig,
    recordings_dir: PathBuf,
) -> Result<SteamHandle, SteamThreadError> {
    let latest: LatestFrame = Arc::new(Mutex::new(None));
    let input_slot: LatestInput = Arc::new(Mutex::new(InputSnapshot::default()));
    let status: CoreStatus = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let volume = Arc::new(AtomicU32::new(audio_config.volume.to_bits()));
    let mic_peak = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
    let (command_tx, command_rx) = mpsc::channel();

    // Determine the target PID: either attach to an existing process or
    // launch Wine ourselves.
    let (pid, mut child) = if let Some(attach_pid) = target.attach_pid {
        log::info!("steam: attaching to existing process PID={attach_pid}");
        (attach_pid, None)
    } else if target.native_steam_launch {
        // PID zero intentionally makes ScreenCaptureKit use the exact title
        // fallback. The platform Steam process and its game are never our
        // child, so failure/stop must not terminate either one.
        log::info!("steam: waiting to attach to native Steam game window");
        (0, None)
    } else {
        let exe_path = target.install_dir.join(&target.exe_name);
        log::info!(
            "steam: launching wine: WINEPREFIX={} wine {}",
            target.wine_prefix.display(),
            exe_path.display()
        );
        let child =
            retrofeel_steamcapture::launch_wine(&target.wine_bin, &target.wine_prefix, &exe_path)
                .map_err(SteamThreadError::LaunchWine)?;
        let pid = child.id();
        log::info!("steam: wine process started, PID={pid}");
        (pid, Some(child))
    };

    // Wait for the game window to appear (attach mode: should be immediate;
    // launch mode: may take 10+ seconds for MoltenVK/FMOD/Steam init).
    log::info!("steam: waiting for game window to appear...");
    let capture_config = retrofeel_steamcapture::CaptureConfig {
        target_pid: pid,
        exclude_pid: Some(std::process::id()),
        window_title_hint: Some(target.name.clone()),
        max_width: target.capture_max_width,
        max_height: target.capture_max_height,
        fps: target.capture_fps,
        // GameHub/Wine already owns the controller through Steam Input. A
        // second controller client can change that route and make one
        // physical action arrive more than once. Native Steam does not have
        // that Wine-owned route, so it retains direct gamepad observation.
        capture_gamepads: target.native_steam_launch,
    };

    // Retry window discovery for up to 30 seconds.
    let capture_handle = {
        let mut delay_ms = 500u64;
        let mut waited = 0u64;
        let mut last_error: Option<retrofeel_steamcapture::CaptureError> = None;
        let mut handle: Option<retrofeel_steamcapture::CaptureHandle> = None;
        let timeout_ms = u64::from(target.native_attach_timeout_seconds.max(1)) * 1_000;
        while waited < timeout_ms && handle.is_none() {
            thread::sleep(std::time::Duration::from_millis(delay_ms));
            waited += delay_ms;
            match retrofeel_steamcapture::start_capture(capture_config.clone()) {
                Ok(h) => {
                    log::info!("steam: game window found after {waited}ms, capture started");
                    handle = Some(h);
                }
                Err(e) => {
                    log::debug!("steam: window not found yet ({e}), retrying...");
                    last_error = Some(e);
                    delay_ms = (delay_ms + 1000).min(3000);
                }
            }
        }
        match handle {
            Some(h) => h,
            None => {
                if let Some(mut process) = child.take() {
                    let _ = process.kill();
                    let _ = process.wait();
                }
                return Err(SteamThreadError::Capture(
                    last_error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "timed out waiting for game window".to_string()),
                ));
            }
        }
    };

    let fps = target.capture_fps as f64;
    let sample_rate = 48000.0; // Mic sample rate; no game audio in v1.
    let base_width = target.capture_max_width;
    let base_height = target.capture_max_height;

    // Spawn the worker thread that consumes captured frames, publishes them
    // to `latest`, drives recording, and handles commands.
    let latest_w = latest.clone();
    let input_w = input_slot.clone();
    let status_w = status.clone();
    let stop_w = stop.clone();
    let paused_w = paused.clone();
    let volume_w = volume.clone();
    let mic_peak_w = mic_peak.clone();
    let recordings_dir_w = recordings_dir.clone();
    let capture_mic_w = target.capture_mic;
    let transcription_w = target.transcription.clone();
    let app_id_w = target.app_id;
    let name_w = target.name.clone();
    let install_dir_w = target.install_dir.clone();
    let fps_w = fps;

    let join = thread::Builder::new()
        .name("retrofeel-steam".into())
        .spawn(move || {
            worker(
                capture_handle,
                child,
                input_w,
                latest_w,
                status_w,
                stop_w,
                paused_w,
                volume_w,
                mic_peak_w,
                command_rx,
                recordings_dir_w,
                capture_mic_w,
                transcription_w,
                app_id_w,
                name_w,
                install_dir_w,
                fps_w,
            );
        })
        .expect("spawn steam thread");

    Ok(SteamHandle {
        input: input_slot,
        latest,
        status,
        fps,
        sample_rate,
        base_width,
        base_height,
        stop,
        paused,
        volume,
        mic_peak,
        commands: command_tx,
        join: Some(join),
    })
}

#[allow(clippy::too_many_arguments)]
fn worker(
    mut capture_handle: retrofeel_steamcapture::CaptureHandle,
    mut child: Option<Child>,
    input: LatestInput,
    latest: LatestFrame,
    status: CoreStatus,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    _volume: Arc<AtomicU32>,
    mic_peak: Arc<AtomicU32>,
    commands: Receiver<SteamCommand>,
    _recordings_dir: PathBuf,
    capture_mic: bool,
    transcription: retrofeel_types::TranscriptionConfig,
    app_id: u32,
    name: String,
    install_dir: PathBuf,
    fps: f64,
) {
    let report_status = |msg: String| {
        if let Ok(mut s) = status.lock() {
            *s = Some(msg);
        }
    };

    let receiver = capture_handle.receiver().clone();
    let mut recording: Option<RecordingHandle> = None;
    let mut cfr_resampler: Option<retrofeel_steamcapture::CfrResampler> = None;
    let mut recording_frame: u64 = 0;
    let mut recording_limit: Option<u64> = None;
    let mut recording_finalizers = Vec::new();

    // Loop: consume captured frames, handle commands, check process exit.
    while !stop.load(Ordering::SeqCst) {
        reap_recording_finalizers(&mut recording_finalizers);

        // Check if the child process has exited (only in launch mode).
        if let Some(child) = &mut child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    report_status("Game process exited".to_string());
                    break;
                }
                Ok(None) => {} // still running
                Err(e) => {
                    report_status(format!("Failed to check process: {e}"));
                    break;
                }
            }
        }

        // Handle commands (non-blocking).
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                SteamCommand::SetPaused(p) => {
                    paused.store(p, Ordering::SeqCst);
                    if let Some(r) = &recording {
                        if p {
                            r.pause(recording_frame);
                        } else {
                            r.resume(recording_frame);
                            if let Some(resampler) = cfr_resampler.as_mut() {
                                resampler.rebase_after_pause();
                            }
                        }
                    }
                }
                SteamCommand::StartRecording {
                    recordings_dir: rd,
                    stop_after_frames,
                } => {
                    if recording.is_none() {
                        match start_steam_recording(
                            &rd,
                            app_id,
                            &name,
                            &install_dir,
                            fps,
                            capture_mic,
                            &transcription,
                            base_resolution(&latest),
                            mic_peak.clone(),
                        ) {
                            Ok(r) => {
                                recording = Some(r);
                                cfr_resampler = Some(retrofeel_steamcapture::CfrResampler::new(
                                    fps.max(1.0) as u32,
                                ));
                                recording_frame = 0;
                                recording_limit = stop_after_frames;
                                log::info!("steam: recording started");
                            }
                            Err(e) => {
                                log::error!("steam: failed to start recording: {e}");
                            }
                        }
                    }
                }
                SteamCommand::StopRecording => {
                    if let Some(r) = recording.take() {
                        recording_finalizers
                            .push(finalize_recording_async(r, cfr_resampler.take()));
                    }
                }
            }
        }

        // Try to receive a captured frame (non-blocking).
        match receiver.try_recv() {
            Ok(captured) => {
                let is_paused = paused.load(Ordering::SeqCst);

                // Expose the precise held state sampled by ScreenCaptureKit
                // to the status overlay. This is the same `RawHostInput`
                // later persisted with each CFR tick.
                if let Ok(mut slot) = input.lock() {
                    *slot = InputSnapshot {
                        mapped: InputState::default(),
                        raw_host: captured.raw_host.clone(),
                    };
                }

                // Publish the same immutable pixel allocation used by CFR
                // recording. A 1080p frame is ~8 MiB, so deep-cloning it for
                // render plus every duplicate grid tick is prohibitively
                // expensive under game load.
                let cf = Arc::new(CoreFrame {
                    frame: captured.video.clone(),
                });
                if let Ok(mut slot) = latest.lock() {
                    *slot = Some(cf);
                }

                // Drive recording (skip during pause to keep timelines aligned).
                if !is_paused {
                    if let Some(r) = &recording {
                        let resampler = cfr_resampler.get_or_insert_with(|| {
                            retrofeel_steamcapture::CfrResampler::new(fps.max(1.0) as u32)
                        });
                        for tick in resampler.push(captured) {
                            r.set_video_clock_anchor(tick.source.source_mach_us);
                            r.frame_timed(
                                tick.encoded_frame,
                                Some(tick.elapsed_us),
                                Some(tick.frame_map),
                                InputState::default(),
                                Some(tick.source.raw_host),
                                tick.source.video,
                                Vec::new(),
                            );
                            recording_frame = tick.encoded_frame.saturating_add(1);
                        }

                        // Auto-stop if we hit the frame limit.
                        if let Some(limit) = recording_limit {
                            if recording_frame >= limit {
                                log::info!("steam: auto-stop at frame {recording_frame}");
                                if let Some(r) = recording.take() {
                                    recording_finalizers
                                        .push(finalize_recording_async(r, cfr_resampler.take()));
                                }
                                recording_limit = None;
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // No frame available; sleep briefly.
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    // Clean up: stop recording, stop capture, reap child if we own it.
    if let Some(r) = recording.take() {
        recording_finalizers.push(finalize_recording_async(r, cfr_resampler.take()));
    }
    capture_handle.stop();
    for finalizer in recording_finalizers {
        let _ = finalizer.join();
    }
    if let Some(mut child) = child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    log::info!("steam: worker thread exiting");
}

fn finalize_recording_async(
    recording: RecordingHandle,
    resampler: Option<retrofeel_steamcapture::CfrResampler>,
) -> thread::JoinHandle<()> {
    log::info!("steam: recording finalization started");
    thread::spawn(move || {
        let mut resampler = resampler;
        finish_cfr_resampler(&recording, &mut resampler);
        recording.stop();
        log::info!("steam: recording stopped");
    })
}

fn reap_recording_finalizers(finalizers: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < finalizers.len() {
        if finalizers[index].is_finished() {
            let finalizer = finalizers.swap_remove(index);
            let _ = finalizer.join();
        } else {
            index += 1;
        }
    }
}

fn finish_cfr_resampler(
    recording: &RecordingHandle,
    resampler: &mut Option<retrofeel_steamcapture::CfrResampler>,
) {
    if let Some(mut resampler) = resampler.take() {
        let stats = resampler.finish();
        recording.set_frame_map_telemetry(
            stats.source_frames_received,
            stats.source_frames_discarded,
            stats.grid_duplicates,
        );
    }
}

fn base_resolution(latest: &LatestFrame) -> (u32, u32) {
    if let Ok(slot) = latest.lock() {
        if let Some(cf) = slot.as_ref() {
            if let Some(frame) = cf.frame.as_ref() {
                return (frame.width, frame.height);
            }
        }
    }
    (1920, 1080)
}

#[allow(clippy::too_many_arguments)]
fn start_steam_recording(
    recordings_dir: &std::path::Path,
    app_id: u32,
    name: &str,
    install_dir: &std::path::Path,
    fps: f64,
    capture_mic: bool,
    transcription: &retrofeel_types::TranscriptionConfig,
    (width, height): (u32, u32),
    live_mic_peak: Arc<AtomicU32>,
) -> anyhow::Result<RecordingHandle> {
    let start = RecordingStart {
        recordings_dir: recordings_dir.to_path_buf(),
        core_name: format!("steam:{app_id}"),
        core_version: name.to_string(),
        core_path: install_dir.to_path_buf(),
        rom_path: Some(install_dir.to_path_buf()),
        rom_sha1: None,
        rom_size: None,
        fps,
        sample_rate: 48000.0,
        width,
        height,
        binding_map: None,
        initial_state_path: None,
        capture_mic,
        live_mic_peak: Some(live_mic_peak),
        transcription: transcription.clone(),
        capture_provenance: Some(CaptureProvenance {
            kind: CaptureSourceKind::MacosScreenCaptureKit,
            game_audio_policy: Some("not_captured".into()),
            source_description: Some("single-window ScreenCaptureKit capture".into()),
        }),
        video_timing: Some(VideoTiming {
            kind: VideoTimingKind::CfrResampledSck,
            output_fps: Some(fps),
            source_clock: Some("ScreenCaptureKit presentation PTS / mach host time".into()),
        }),
        track_alignment: Some(steam_track_alignment(capture_mic)),
    };
    RecordingHandle::start(start).map_err(anyhow::Error::from)
}

fn steam_track_alignment(capture_mic: bool) -> TrackAlignmentMetadata {
    TrackAlignmentMetadata {
        video: TrackAlignment {
            presence: TrackPresence::Present,
            status: TrackAlignmentStatus::Complete,
            offset_us: Some(0),
            uncertainty_us: Some(0),
            clock_source: Some("ScreenCaptureKit presentation PTS / mach host time".into()),
        },
        narration: TrackAlignment {
            presence: if capture_mic {
                TrackPresence::Present
            } else {
                TrackPresence::NotCaptured
            },
            status: if capture_mic {
                // The mic recorder replaces this best-effort placeholder with
                // its measured CoreAudio/SCK relation once it has samples.
                TrackAlignmentStatus::Degraded
            } else {
                TrackAlignmentStatus::NotCaptured
            },
            offset_us: None,
            uncertainty_us: None,
            clock_source: capture_mic
                .then_some("CoreAudio capture timestamp / mach host time".into()),
        },
        game_audio: TrackAlignment {
            presence: TrackPresence::NotCaptured,
            status: TrackAlignmentStatus::NotCaptured,
            offset_us: None,
            uncertainty_us: None,
            clock_source: Some("macOS local capture policy".into()),
        },
    }
}
