//! Core thread manager.
//!
//! Runs a libretro [`Core`] on a dedicated OS thread, decoupled from Bevy's
//! render loop, per the plan's "core on a dedicated thread" decision. The
//! thread owns the `Core` (libretro cores are single-threaded and tied to the
//! thread that created them), paces itself to the core's reported fps, reads
//! input from a channel, and writes frames + audio into channels/ring buffer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audio::{self, AudioOutput, AudioProd};
use crate::recording::{RecordingHandle, RecordingStart};
use crate::worker_process;
use libretro_host::Core;
use retrofeel_types::config::AudioConfig;
use retrofeel_types::RetroFeelConfig;
use retrofeel_types::VideoFrame as Frame;
use retrofeel_types::{InputState, RawHostInput};

/// The source type currently running: a libretro core or a Steam/Wine game.
/// The HUD uses this to hide save-state/reset buttons for Steam sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SourceKind {
    Libretro,
    Steam,
}

/// Trait abstracting a running game source (libretro core or Steam capture).
/// Both `CoreHandle` and `SteamHandle` implement this so the Bevy systems
/// (`poll_input`, `drain_frames`, `drain_core_status`, HUD actions, recording
/// hotkeys) work unchanged regardless of source.
///
/// Some methods (e.g. `kind()`, `fps()`, `start_recording_debug()`) are not yet
/// called through `dyn GameSource` — they're used directly on the concrete
/// types during launch. They're part of the trait contract so the HUD (Phase 6)
/// can call them through the trait object to gate save-state/reset buttons for
/// Steam sessions.
#[allow(dead_code)]
pub trait GameSource: Send + Sync {
    fn kind(&self) -> SourceKind;
    fn input_slot(&self) -> &LatestInput;
    fn latest_frame_slot(&self) -> &LatestFrame;
    fn status(&self) -> &CoreStatus;
    fn base_width(&self) -> u32;
    fn base_height(&self) -> u32;
    fn fps(&self) -> f64;
    fn sample_rate(&self) -> f64;
    /// Latest microphone peak from the recording writer, if this source
    /// provides one. Used only by the non-interactive local status overlay.
    fn mic_peak(&self) -> f32 {
        0.0
    }

    fn set_paused(&self, paused: bool);
    fn set_volume(&self, volume: f32);
    fn set_fast_forward(&self, on: bool);

    fn start_recording(&self, recordings_dir: PathBuf);
    fn start_recording_for(&self, recordings_dir: PathBuf, frames: u64);
    fn start_recording_debug(
        &self,
        recordings_dir: PathBuf,
        frames: u64,
        pause_at_frame: Option<u64>,
        pause_duration_ms: u64,
    );
    fn stop_recording(&self);

    /// Save state to slot. No-op for Steam (non-controllable process).
    fn save_state(&self, slot: u8);
    /// Load state from slot. No-op for Steam.
    fn load_state(&self, slot: u8);
    /// Reset the core/game. No-op for Steam.
    fn reset(&self);
}

/// A produced frame. The audio is pushed into the ring buffer by the worker
/// before this struct is published; recording gets its own copy via
/// `RecordingHandle::frame`, so this struct carries only the video frame for
/// the render thread. (Earlier versions also carried `audio` here, which was
/// cloned per-frame and never read by the render thread — item 23.)
pub struct CoreFrame {
    pub frame: Option<Arc<Frame>>,
}

/// Shared slot holding the latest frame produced by the core thread.
pub type LatestFrame = Arc<Mutex<Option<Arc<CoreFrame>>>>;

/// Shared slot holding the latest input the render thread wants fed to the core.
pub type LatestInput = Arc<Mutex<InputSnapshot>>;

/// Shared slot holding the latest worker status message. The worker writes a
/// human-readable string here when it hits a load/load_game/run_frame failure
/// or exits; the UI reads it each frame and surfaces a toast/banner so a core
/// crash is no longer invisible (item 15 of the post-v1 review). `None` means
/// the worker is running cleanly.
pub type CoreStatus = Arc<Mutex<Option<String>>>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputSnapshot {
    pub mapped: InputState,
    pub raw_host: RawHostInput,
}

/// Stop flag shared between the handle and the worker.
type StopFlag = Arc<AtomicBool>;
type PauseFlag = Arc<AtomicBool>;
/// Fast-forward flag shared between the handle and the worker. When set, the
/// worker drops pacing so the core runs as fast as possible while keeping the
/// capture frame-indexed (the input log still records one entry per frame).
type FastForwardFlag = Arc<AtomicBool>;

#[derive(Debug, Clone)]
pub enum CoreCommand {
    SaveState(u8),
    LoadState(u8),
    Reset,
    StartRecording {
        recordings_dir: PathBuf,
        stop_after_frames: Option<u64>,
        pause_at_frame: Option<u64>,
        pause_duration_ms: u64,
    },
    StopRecording,
    SetPaused(bool),
}

/// Handle to a running core thread. Drop it to stop the thread.
pub struct CoreHandle {
    pub input: LatestInput,
    pub latest: LatestFrame,
    /// Worker status (None = healthy; Some(msg) = load/run failure or exit).
    /// The UI reads this to surface a toast so a core crash isn't invisible.
    pub status: CoreStatus,
    #[allow(dead_code)]
    pub fps: f64,
    #[allow(dead_code)]
    pub sample_rate: f64,
    pub base_width: u32,
    pub base_height: u32,
    stop: StopFlag,
    paused: PauseFlag,
    fast_forward: FastForwardFlag,
    /// Shared audio gain as `f32` bits; the cpal callback re-reads it each buffer
    /// so `set_volume` takes effect live.
    volume: Arc<AtomicU32>,
    commands: Sender<CoreCommand>,
    joins: Vec<thread::JoinHandle<()>>,
    child: Option<Arc<Mutex<std::process::Child>>>,
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.as_ref() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let finished = child
                    .lock()
                    .ok()
                    .and_then(|mut child| child.try_wait().ok().flatten())
                    .is_some();
                if finished || std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if let Ok(mut child) = child.lock() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}

impl CoreHandle {
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        let _ = self.commands.send(CoreCommand::SetPaused(paused));
    }

    pub fn save_state(&self, slot: u8) {
        let _ = self.commands.send(CoreCommand::SaveState(slot));
    }

    pub fn reset(&self) {
        let _ = self.commands.send(CoreCommand::Reset);
    }

    /// Set the audio gain (1.0 = unity). Takes effect on the next cpal buffer.
    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.to_bits(), Ordering::SeqCst);
    }

    pub fn load_state(&self, slot: u8) {
        let _ = self.commands.send(CoreCommand::LoadState(slot));
    }

    pub fn start_recording(&self, recordings_dir: PathBuf) {
        let _ = self.commands.send(CoreCommand::StartRecording {
            recordings_dir,
            stop_after_frames: None,
            pause_at_frame: None,
            pause_duration_ms: 0,
        });
    }

    pub fn start_recording_for(&self, recordings_dir: PathBuf, frames: u64) {
        let _ = self.commands.send(CoreCommand::StartRecording {
            recordings_dir,
            stop_after_frames: Some(frames),
            pause_at_frame: None,
            pause_duration_ms: 0,
        });
    }

    pub fn start_recording_debug(
        &self,
        recordings_dir: PathBuf,
        frames: u64,
        pause_at_frame: Option<u64>,
        pause_duration_ms: u64,
    ) {
        let _ = self.commands.send(CoreCommand::StartRecording {
            recordings_dir,
            stop_after_frames: Some(frames),
            pause_at_frame,
            pause_duration_ms,
        });
    }

    pub fn stop_recording(&self) {
        let _ = self.commands.send(CoreCommand::StopRecording);
    }

    /// Toggle fast-forward. When enabled, the core thread stops pacing to fps
    /// and runs frames back-to-back; capture stays frame-indexed (one input
    /// record per `retro_run`), so the input log remains valid for replay.
    pub fn set_fast_forward(&self, on: bool) {
        self.fast_forward.store(on, Ordering::SeqCst);
    }
}

impl GameSource for CoreHandle {
    fn kind(&self) -> SourceKind {
        SourceKind::Libretro
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
    fn set_paused(&self, paused: bool) {
        CoreHandle::set_paused(self, paused);
    }
    fn set_volume(&self, volume: f32) {
        CoreHandle::set_volume(self, volume);
    }
    fn set_fast_forward(&self, on: bool) {
        CoreHandle::set_fast_forward(self, on);
    }
    fn start_recording(&self, recordings_dir: PathBuf) {
        CoreHandle::start_recording(self, recordings_dir);
    }
    fn start_recording_for(&self, recordings_dir: PathBuf, frames: u64) {
        CoreHandle::start_recording_for(self, recordings_dir, frames);
    }
    fn start_recording_debug(
        &self,
        recordings_dir: PathBuf,
        frames: u64,
        pause_at_frame: Option<u64>,
        pause_duration_ms: u64,
    ) {
        CoreHandle::start_recording_debug(
            self,
            recordings_dir,
            frames,
            pause_at_frame,
            pause_duration_ms,
        );
    }
    fn stop_recording(&self) {
        CoreHandle::stop_recording(self);
    }
    fn save_state(&self, slot: u8) {
        CoreHandle::save_state(self, slot);
    }
    fn load_state(&self, slot: u8) {
        CoreHandle::load_state(self, slot);
    }
    fn reset(&self) {
        CoreHandle::reset(self);
    }
}

/// Errors from launching a core thread.
#[derive(Debug, thiserror::Error)]
pub enum CoreThreadError {
    #[error("failed to load core: {0}")]
    Load(#[from] libretro_host::CoreError),
    #[error("failed to read rom: {0}")]
    Rom(std::io::Error),
    #[error("failed to start isolated core worker: {0}")]
    SpawnWorker(std::io::Error),
    #[error("core worker transport failed: {0}")]
    WorkerTransport(String),
    #[error("core worker rejected launch: {0}")]
    WorkerRejected(String),
    #[error("core worker exited before launch completed")]
    WorkerExited,
    #[error("core worker did not produce a first frame within 20 seconds")]
    WorkerTimeout,
}

enum WorkerEnvelope {
    Event(retrofeel_types::WorkerEvent),
    Closed(String),
}

/// Spawn the crash-isolated core worker used by the GUI.
///
/// No native core function is called in this process. The function returns
/// only after the child has loaded content and produced its first decoded
/// frame, so callers can safely keep the Library visible on any launch error.
pub fn spawn(
    core_path: PathBuf,
    rom: Option<PathBuf>,
    system_dir: String,
    audio_config: AudioConfig,
    config: Option<RetroFeelConfig>,
) -> Result<(CoreHandle, Option<AudioOutput>), CoreThreadError> {
    use std::process::Stdio;

    let mut token = [0_u8; 32];
    getrandom::getrandom(&mut token)
        .map_err(|error| CoreThreadError::WorkerTransport(error.to_string()))?;
    let executable = std::env::current_exe().map_err(CoreThreadError::SpawnWorker)?;
    let mut child = std::process::Command::new(executable)
        .arg("__core-worker")
        .env("RETROFEEL_WORKER_TOKEN", worker_process::token_hex(&token))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(CoreThreadError::SpawnWorker)?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| CoreThreadError::WorkerTransport("worker stdin was not available".into()))?;
    let child_stdout = child.stdout.take().ok_or_else(|| {
        CoreThreadError::WorkerTransport("worker stdout was not available".into())
    })?;
    let child = Arc::new(Mutex::new(child));

    let launch = retrofeel_types::WorkerRequest::Launch(Box::new(retrofeel_types::WorkerLaunch {
        protocol_version: retrofeel_types::WORKER_PROTOCOL_VERSION,
        authentication_token: token,
        core_path,
        rom_path: rom,
        system_dir: PathBuf::from(system_dir),
        config,
    }));
    if let Err(error) = worker_process::write_message(&mut child_stdin, &launch) {
        terminate_child(&child);
        return Err(CoreThreadError::WorkerTransport(error.to_string()));
    }

    // A capacity of two bounds decoded frame memory while still allowing the
    // reader to overlap one pipe read with GUI-side frame publication.
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(2);
    let child_reader = child.clone();
    let reader_join = thread::Builder::new()
        .name("retrofeel-worker-reader".into())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(child_stdout);
            loop {
                match worker_process::read_message(&mut reader) {
                    Ok(event) => {
                        if event_tx.send(WorkerEnvelope::Event(event)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // Pipe EOF can become observable a few milliseconds
                        // before wait status on some platforms. Poll briefly
                        // so an actionable exit code/signal wins over a generic
                        // read error.
                        let mut exit = None;
                        for _ in 0..10 {
                            exit = child_reader
                                .lock()
                                .ok()
                                .and_then(|mut child| child.try_wait().ok().flatten());
                            if exit.is_some() {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        let exit = exit
                            .map(|status| format!("worker exited with {status}"))
                            .unwrap_or_else(|| error.to_string());
                        let _ = event_tx.send(WorkerEnvelope::Closed(exit));
                        break;
                    }
                }
            }
        })
        .map_err(CoreThreadError::SpawnWorker)?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let ready = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            terminate_child(&child);
            let _ = reader_join.join();
            return Err(CoreThreadError::WorkerTimeout);
        }
        match event_rx.recv_timeout(remaining) {
            Ok(WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Ready(ready))) => break ready,
            Ok(WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Error { kind, message })) => {
                terminate_child(&child);
                let _ = reader_join.join();
                return Err(CoreThreadError::WorkerRejected(format!(
                    "{kind:?}: {message}"
                )));
            }
            Ok(WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Status(message))) => {
                log::info!("core worker launch: {message}");
            }
            Ok(WorkerEnvelope::Closed(error)) => {
                log::warn!("core worker closed during launch: {error}");
                terminate_child(&child);
                let _ = reader_join.join();
                return Err(CoreThreadError::WorkerExited);
            }
            Ok(WorkerEnvelope::Event(_)) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                terminate_child(&child);
                let _ = reader_join.join();
                return Err(CoreThreadError::WorkerTimeout);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                terminate_child(&child);
                let _ = reader_join.join();
                return Err(CoreThreadError::WorkerExited);
            }
        }
    };

    let latest: LatestFrame = Arc::new(Mutex::new(Some(Arc::new(CoreFrame {
        frame: Some(Arc::new(ready.first_frame)),
    }))));
    let input: LatestInput = Arc::new(Mutex::new(InputSnapshot::default()));
    let status: CoreStatus = Arc::new(Mutex::new(None));
    let stop: StopFlag = Arc::new(AtomicBool::new(false));
    let paused: PauseFlag = Arc::new(AtomicBool::new(false));
    let fast_forward: FastForwardFlag = Arc::new(AtomicBool::new(false));
    let volume = Arc::new(AtomicU32::new(audio_config.volume.to_bits()));
    let (mut audio_prod, audio_out) = if audio_config.enabled {
        match audio::build(
            ready.sample_rate,
            volume.clone(),
            audio_config.latency_ms,
            fast_forward.clone(),
        ) {
            Ok((producer, output)) => (producer, Some(output)),
            Err(error) => {
                log::warn!("audio init failed ({error}); continuing without sound");
                (audio::dummy_prod(fast_forward.clone()), None)
            }
        }
    } else {
        (audio::dummy_prod(fast_forward.clone()), None)
    };
    push_audio(&mut audio_prod, &ready.first_audio);

    let latest_events = latest.clone();
    let status_events = status.clone();
    let stop_events = stop.clone();
    let processor_join = thread::Builder::new()
        .name("retrofeel-worker-events".into())
        .spawn(move || {
            while let Ok(envelope) = event_rx.recv() {
                match envelope {
                    WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Frame { frame, audio }) => {
                        push_audio(&mut audio_prod, &audio);
                        if let Ok(mut slot) = latest_events.lock() {
                            *slot = Some(Arc::new(CoreFrame {
                                frame: frame.map(Arc::new),
                            }));
                        }
                    }
                    WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Status(message)) => {
                        if let Ok(mut slot) = status_events.lock() {
                            *slot = Some(message);
                        }
                    }
                    WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Error {
                        kind,
                        message,
                    }) => {
                        if let Ok(mut slot) = status_events.lock() {
                            *slot = Some(format!("Core worker {kind:?}: {message}"));
                        }
                    }
                    WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Stopped) => break,
                    WorkerEnvelope::Closed(error) => {
                        if !stop_events.load(Ordering::SeqCst) {
                            if let Ok(mut slot) = status_events.lock() {
                                *slot = Some(format!(
                                    "Core worker crashed or exited unexpectedly ({error})"
                                ));
                            }
                        }
                        break;
                    }
                    WorkerEnvelope::Event(retrofeel_types::WorkerEvent::Ready(_)) => {}
                }
            }
        })
        .map_err(CoreThreadError::SpawnWorker)?;

    let (command_tx, command_rx) = mpsc::channel();
    let input_writer = input.clone();
    let stop_writer = stop.clone();
    let fast_forward_writer = fast_forward.clone();
    let writer_join = thread::Builder::new()
        .name("retrofeel-worker-writer".into())
        .spawn(move || {
            let mut previous_input = InputSnapshot::default();
            let mut previous_fast_forward = false;
            while !stop_writer.load(Ordering::SeqCst) {
                match command_rx.recv_timeout(std::time::Duration::from_millis(4)) {
                    Ok(command) => {
                        if worker_process::write_message(
                            &mut child_stdin,
                            &retrofeel_types::WorkerRequest::Command(map_worker_command(command)),
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                let current = input_writer
                    .lock()
                    .map(|input| input.clone())
                    .unwrap_or_default();
                if current != previous_input {
                    previous_input = current.clone();
                    let request =
                        retrofeel_types::WorkerRequest::Input(retrofeel_types::WorkerInput {
                            mapped: current.mapped,
                            raw_host: current.raw_host,
                        });
                    if worker_process::write_message(&mut child_stdin, &request).is_err() {
                        return;
                    }
                }
                let current_fast_forward = fast_forward_writer.load(Ordering::SeqCst);
                if current_fast_forward != previous_fast_forward {
                    previous_fast_forward = current_fast_forward;
                    let request = retrofeel_types::WorkerRequest::Command(
                        retrofeel_types::WorkerCommand::SetFastForward(current_fast_forward),
                    );
                    if worker_process::write_message(&mut child_stdin, &request).is_err() {
                        return;
                    }
                }
            }
            let _ = worker_process::write_message(
                &mut child_stdin,
                &retrofeel_types::WorkerRequest::Shutdown,
            );
        })
        .map_err(CoreThreadError::SpawnWorker)?;

    Ok((
        CoreHandle {
            input,
            latest,
            status,
            fps: ready.fps,
            sample_rate: ready.sample_rate,
            base_width: ready.base_width,
            base_height: ready.base_height,
            stop,
            paused,
            fast_forward,
            volume,
            commands: command_tx,
            joins: vec![writer_join, reader_join, processor_join],
            child: Some(child),
        },
        audio_out,
    ))
}

fn map_worker_command(command: CoreCommand) -> retrofeel_types::WorkerCommand {
    match command {
        CoreCommand::SaveState(slot) => retrofeel_types::WorkerCommand::SaveState(slot),
        CoreCommand::LoadState(slot) => retrofeel_types::WorkerCommand::LoadState(slot),
        CoreCommand::Reset => retrofeel_types::WorkerCommand::Reset,
        CoreCommand::StartRecording {
            recordings_dir,
            stop_after_frames,
            pause_at_frame,
            pause_duration_ms,
        } => retrofeel_types::WorkerCommand::StartRecording {
            recordings_dir,
            stop_after_frames,
            pause_at_frame,
            pause_duration_ms,
        },
        CoreCommand::StopRecording => retrofeel_types::WorkerCommand::StopRecording,
        CoreCommand::SetPaused(paused) => retrofeel_types::WorkerCommand::SetPaused(paused),
    }
}

fn terminate_child(child: &Arc<Mutex<std::process::Child>>) {
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Spawn a core thread. The core is loaded on the worker thread (libretro cores
/// are thread-affine). We pre-flight with a throwaway Core to read av_info, and
/// build the audio output using the core's reported sample rate + the user's
/// audio config (volume/latency). Returns the handle and the owned audio
/// stream so the caller can drop it on quit-to-library instead of leaking.
#[allow(dead_code)]
pub fn spawn_in_process(
    core_path: PathBuf,
    rom: Option<PathBuf>,
    system_dir: String,
    audio_config: AudioConfig,
    config: Option<RetroFeelConfig>,
) -> Result<(CoreHandle, Option<AudioOutput>), CoreThreadError> {
    use CoreThreadError as E;

    let latest: LatestFrame = Arc::new(Mutex::new(None));
    let input_slot: LatestInput = Arc::new(Mutex::new(InputSnapshot::default()));
    let status: CoreStatus = Arc::new(Mutex::new(None));
    let stop: StopFlag = Arc::new(AtomicBool::new(false));
    let paused: PauseFlag = Arc::new(AtomicBool::new(false));
    let fast_forward: FastForwardFlag = Arc::new(AtomicBool::new(false));
    let (command_tx, command_rx) = mpsc::channel();

    // Pre-flight: load a throwaway core to read av_info, then drop it.
    let mut pre = Core::load(&core_path, &system_dir)?;
    if let Some(config) = &config {
        log_option_report(&retrofeel_backend::apply_configured_core_options(
            &pre, config,
        ));
    }
    let rom_bytes = match &rom {
        Some(p) => std::fs::read(p).map_err(E::Rom)?,
        None => Vec::new(),
    };
    pre.load_game(&rom_bytes, rom.as_ref().and_then(|p| p.to_str()))?;
    let fps = pre.av_info().fps;
    let sample_rate = pre.av_info().sample_rate;
    let base_width = pre.av_info().base_width;
    let base_height = pre.av_info().base_height;
    drop(pre);

    // Build audio using the core's sample rate so the resampler maps to the
    // device rate correctly. If audio is disabled or init fails, hand the
    // worker a dummy producer so the run loop still works. The volume atomic is
    // shared with the cpal callback so the in-game control can change it live.
    let volume = Arc::new(AtomicU32::new(audio_config.volume.to_bits()));
    let (audio_prod, audio_out) = if audio_config.enabled {
        match audio::build(
            sample_rate,
            volume.clone(),
            audio_config.latency_ms,
            fast_forward.clone(),
        ) {
            Ok((prod, out)) => (prod, Some(out)),
            Err(error) => {
                log::warn!("audio init failed ({error}); continuing without sound");
                (audio::dummy_prod(fast_forward.clone()), None)
            }
        }
    } else {
        (audio::dummy_prod(fast_forward.clone()), None)
    };

    let latest_w = latest.clone();
    let input_w = input_slot.clone();
    let status_w = status.clone();
    let stop_w = stop.clone();
    let paused_w = paused.clone();
    let fast_forward_w = fast_forward.clone();
    let join = thread::Builder::new()
        .name("retrofeel-core".into())
        .spawn(move || {
            worker(
                core_path,
                rom_bytes,
                rom,
                system_dir,
                input_w,
                latest_w,
                status_w,
                stop_w,
                paused_w,
                fast_forward_w,
                command_rx,
                audio_prod,
                config,
            );
        })
        .expect("spawn core thread");

    Ok((
        CoreHandle {
            input: input_slot,
            latest,
            status,
            fps,
            sample_rate,
            base_width,
            base_height,
            stop,
            paused,
            fast_forward,
            volume,
            commands: command_tx,
            joins: vec![join],
            child: None,
        },
        audio_out,
    ))
}

#[allow(clippy::too_many_arguments)]
fn worker(
    core_path: PathBuf,
    rom_bytes: Vec<u8>,
    rom: Option<PathBuf>,
    system_dir: String,
    input: LatestInput,
    latest: LatestFrame,
    status: CoreStatus,
    stop: StopFlag,
    paused: PauseFlag,
    fast_forward: FastForwardFlag,
    commands: Receiver<CoreCommand>,
    mut audio_prod: AudioProd,
    config: Option<RetroFeelConfig>,
) {
    let report_status = |msg: String| {
        if let Ok(mut s) = status.lock() {
            *s = Some(msg);
        }
    };
    let mut core = match Core::load(&core_path, &system_dir) {
        Ok(c) => c,
        Err(e) => {
            log::error!("core thread: load failed: {e}");
            report_status(format!("Core load failed: {e}"));
            return;
        }
    };
    if let Some(config) = &config {
        log_option_report(&retrofeel_backend::apply_configured_core_options(
            &core, config,
        ));
    }
    if let Err(e) = core.load_game(&rom_bytes, rom.as_ref().and_then(|p| p.to_str())) {
        log::error!("core thread: load_game failed: {e}");
        report_status(format!("Failed to load ROM: {e}"));
        return;
    }
    let core_key = core.system_info().library_name.clone();
    if let Some(config) = &config {
        match retrofeel_backend::load_sram(&mut core, config, &core_key, rom.as_deref()) {
            Ok(Some(bytes)) => log::info!("loaded SRAM: {bytes} bytes"),
            Ok(None) => {}
            Err(e) => log::warn!("failed to load SRAM: {e}"),
        }
    }

    let fps = core.av_info().fps;
    let frame_dt = if fps > 0.0 {
        std::time::Duration::from_secs_f64(1.0 / fps)
    } else {
        std::time::Duration::from_secs_f64(1.0 / 60.0)
    };

    log::info!(
        "core thread running: {} v{} @ {:.3} fps, {}x{}",
        core.system_info().library_name,
        core.system_info().library_version,
        fps,
        core.av_info().base_width,
        core.av_info().base_height,
    );

    let mut next_deadline = std::time::Instant::now() + frame_dt;
    let mut frames_since_sram_flush = 0u32;
    let mut recording: Option<RecordingHandle> = None;
    let mut recording_frame = 0u64;
    let mut recording_limit: Option<u64> = None;
    let mut auto_pause: Option<AutoPause> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Some(pause) = &auto_pause {
            if pause
                .resume_at
                .map(|resume_at| std::time::Instant::now() >= resume_at)
                .unwrap_or(false)
            {
                paused.store(false, Ordering::SeqCst);
                if let Some(recording) = recording.as_ref() {
                    recording.resume(recording_frame);
                }
                auto_pause = None;
                next_deadline = std::time::Instant::now() + frame_dt;
            }
        }
        process_commands(
            &commands,
            &mut core,
            config.as_ref(),
            &core_key,
            rom.as_deref(),
            &core_path,
            &rom_bytes,
            &mut recording,
            &mut recording_frame,
            &mut recording_limit,
            &mut auto_pause,
        );
        if paused.load(Ordering::SeqCst) {
            thread::sleep(std::time::Duration::from_millis(8));
            continue;
        }
        // Read the latest input the render thread staged. Defaults to idle if
        // the slot hasn't been written yet.
        let input = input.lock().map(|s| s.clone()).unwrap_or_default();

        let out = match core.run_frame(input.mapped) {
            Ok(o) => o,
            Err(e) => {
                log::error!("core thread: run_frame failed: {e}");
                report_status(format!("Core crashed: {e}"));
                break;
            }
        };
        frames_since_sram_flush = frames_since_sram_flush.saturating_add(1);

        if let Some(active_recording) = &recording {
            active_recording.frame(
                recording_frame,
                input.mapped,
                Some(input.raw_host),
                out.frame.clone(),
                out.audio.clone(),
            );
            recording_frame = recording_frame.saturating_add(1);
            if recording_limit
                .map(|limit| recording_frame >= limit)
                .unwrap_or(false)
            {
                if let Some(recording) = recording.take() {
                    recording.stop();
                }
                recording_limit = None;
                auto_pause = None;
            } else if let Some(pause) = &mut auto_pause {
                if !pause.triggered && recording_frame >= pause.at_frame {
                    pause.triggered = true;
                    pause.resume_at = Some(std::time::Instant::now() + pause.duration);
                    paused.store(true, Ordering::SeqCst);
                    active_recording.pause(recording_frame);
                }
            }
        }

        // Push audio into the ring buffer (interleaved stereo i16 at the core's
        // sample rate; the cpal callback resamples to the device rate/format).
        // Drop samples if the buffer is full.
        if !out.audio.is_empty() {
            push_audio(&mut audio_prod, &out.audio);
        }

        let cf = Arc::new(CoreFrame {
            frame: out.frame.map(Arc::new),
        });
        if let Ok(mut slot) = latest.lock() {
            *slot = Some(cf);
        }

        if frames_since_sram_flush >= 300 {
            flush_sram_if_configured(&mut core, config.as_ref(), &core_key, rom.as_deref());
            frames_since_sram_flush = 0;
        }

        // Pace to the core's fps — unless fast-forward is requested, in which
        // case we drop pacing and run frames back-to-back. Capture stays
        // frame-indexed either way (one record per retro_run), so the input
        // log remains valid for deterministic replay.
        if !fast_forward.load(Ordering::SeqCst) {
            let now = std::time::Instant::now();
            if now < next_deadline {
                thread::sleep(next_deadline - now);
            }
        }
        next_deadline += frame_dt;
        let now = std::time::Instant::now();
        if next_deadline < now {
            next_deadline = now + frame_dt;
        }
    }

    if let Some(recording) = recording.take() {
        recording.stop();
    }
    flush_sram_if_configured(&mut core, config.as_ref(), &core_key, rom.as_deref());
    log::info!("core thread exiting");
}

#[allow(clippy::too_many_arguments)]
fn process_commands(
    commands: &Receiver<CoreCommand>,
    core: &mut Core,
    config: Option<&RetroFeelConfig>,
    core_key: &str,
    rom: Option<&std::path::Path>,
    core_path: &std::path::Path,
    _rom_bytes: &[u8],
    recording: &mut Option<RecordingHandle>,
    recording_frame: &mut u64,
    recording_limit: &mut Option<u64>,
    auto_pause: &mut Option<AutoPause>,
) {
    for command in commands.try_iter() {
        match command {
            CoreCommand::SaveState(slot) => {
                let Some(config) = config else {
                    log::warn!("cannot save state slot {slot}: no config is active");
                    continue;
                };
                match retrofeel_backend::save_state_slot(core, config, core_key, rom, slot) {
                    Ok(bytes) => log::info!("saved state slot {slot}: {bytes} bytes"),
                    Err(e) => log::warn!("failed to save state slot {slot}: {e}"),
                }
            }
            CoreCommand::LoadState(slot) => {
                let Some(config) = config else {
                    log::warn!("cannot load state slot {slot}: no config is active");
                    continue;
                };
                match retrofeel_backend::load_state_slot(core, config, core_key, rom, slot) {
                    Ok(Some(bytes)) => log::info!("loaded state slot {slot}: {bytes} bytes"),
                    Ok(None) => log::warn!("state slot {slot} is empty"),
                    Err(e) => log::warn!("failed to load state slot {slot}: {e}"),
                }
            }
            CoreCommand::Reset => match core.reset() {
                Ok(()) => log::info!("core reset"),
                Err(e) => log::warn!("core reset failed: {e}"),
            },
            CoreCommand::StartRecording {
                recordings_dir,
                stop_after_frames,
                pause_at_frame,
                pause_duration_ms,
            } => {
                if recording.is_some() {
                    log::warn!("recording already active");
                    continue;
                }
                let info = core.system_info();
                let av = core.av_info();
                let core_name = info.library_name.clone();
                let core_version = info.library_version.clone();
                // Capture the launch state (optional) so stretch-phase S2
                // deterministic replay has a known starting point. Best-effort:
                // a core that doesn't support save states returns an error and
                // we record without an initial state.
                let initial_state_path = match core.serialize() {
                    Ok(bytes) => {
                        let dir = std::env::temp_dir().join(format!(
                            "retrofeel-init-{}.bin",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0),
                        ));
                        match std::fs::write(&dir, &bytes) {
                            Ok(_) => Some(dir),
                            Err(e) => {
                                log::warn!("failed to write initial state: {e}");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        log::debug!("core does not support save states for initial_state: {e}");
                        None
                    }
                };
                let rom_identity = rom.and_then(|path| crate::recording::rom_identity(path).ok());
                let start = RecordingStart {
                    recordings_dir,
                    core_name,
                    core_version,
                    core_path: core_path.to_path_buf(),
                    rom_path: rom.map(ToOwned::to_owned),
                    rom_sha1: rom_identity.as_ref().map(|(sha1, _)| sha1.clone()),
                    rom_size: rom_identity.map(|(_, size)| size),
                    fps: av.fps,
                    sample_rate: av.sample_rate,
                    width: av.base_width,
                    height: av.base_height,
                    binding_map: config.map(|config| config.global_input_bindings.clone()),
                    initial_state_path,
                    capture_mic: config.map(|c| c.recording.mic_enabled).unwrap_or(true),
                    live_mic_peak: None,
                    transcription: config
                        .map(|c| c.recording.transcription.clone())
                        .unwrap_or_default(),
                    capture_provenance: None,
                    video_timing: None,
                    track_alignment: None,
                };
                match RecordingHandle::start(start) {
                    Ok(handle) => {
                        log::info!("recording started: {}", handle.session_dir.display());
                        *recording_frame = 0;
                        *recording_limit = stop_after_frames;
                        *auto_pause = pause_at_frame.map(|at_frame| AutoPause {
                            at_frame,
                            duration: std::time::Duration::from_millis(pause_duration_ms),
                            triggered: false,
                            resume_at: None,
                        });
                        *recording = Some(handle);
                    }
                    Err(error) => log::warn!("failed to start recording: {error}"),
                }
            }
            CoreCommand::StopRecording => {
                if let Some(recording) = recording.take() {
                    recording.stop();
                }
                *recording_limit = None;
                *auto_pause = None;
            }
            CoreCommand::SetPaused(paused) => {
                if let Some(recording) = recording.as_ref() {
                    if paused {
                        recording.pause(*recording_frame);
                    } else {
                        recording.resume(*recording_frame);
                    }
                }
            }
        }
    }
}

struct AutoPause {
    at_frame: u64,
    duration: std::time::Duration,
    triggered: bool,
    resume_at: Option<std::time::Instant>,
}

fn log_option_report(report: &retrofeel_backend::CoreOptionApplyReport) {
    for (key, value) in &report.applied {
        log::info!("core option applied for {}: {key}={value}", report.core_key);
    }
    for (key, value) in &report.unknown {
        log::warn!(
            "unknown core option for {} ignored: {key}={value}",
            report.core_key
        );
    }
    for invalid in &report.invalid {
        log::warn!(
            "invalid core option for {} ignored: {}={} (allowed: {})",
            report.core_key,
            invalid.key,
            invalid.value,
            invalid.allowed_values.join(", ")
        );
    }
}

fn flush_sram_if_configured(
    core: &mut Core,
    config: Option<&RetroFeelConfig>,
    core_key: &str,
    rom: Option<&std::path::Path>,
) {
    let Some(config) = config else {
        return;
    };
    match retrofeel_backend::flush_sram(core, config, core_key, rom) {
        Ok(Some(bytes)) => log::debug!("flushed SRAM: {bytes} bytes"),
        Ok(None) => {}
        Err(e) => log::warn!("failed to flush SRAM: {e}"),
    }
}

fn push_audio(prod: &mut AudioProd, samples: &[i16]) {
    prod.push(samples);
}

/// Read the latest frame from the shared slot (None if no new frame since last read).
pub fn latest_frame(latest: &LatestFrame) -> Option<Arc<CoreFrame>> {
    latest.lock().ok().and_then(|mut slot| slot.take())
}

/// Stage input for the core thread to pick up on its next frame.
pub fn send_input(slot: &LatestInput, input: InputSnapshot) {
    if let Ok(mut s) = slot.lock() {
        *s = input;
    }
}
