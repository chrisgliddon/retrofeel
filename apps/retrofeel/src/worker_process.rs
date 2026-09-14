//! Isolated libretro worker process and its bounded wire transport.
//!
//! The GUI starts a second copy of the current executable with the hidden
//! `__core-worker` command. Native core code is loaded only in that child. A
//! segfault, `abort`, or C `exit()` therefore closes the pipe instead of
//! terminating Bevy.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant};

use bincode::Options;
use libretro_host::Core;
use retrofeel_types::{
    WorkerCommand, WorkerErrorKind, WorkerEvent, WorkerInput, WorkerLaunch, WorkerReady,
    WorkerRequest, MAX_WORKER_MESSAGE_BYTES, WORKER_PROTOCOL_VERSION,
};

use crate::recording::{RecordingHandle, RecordingStart};

const MAX_FRAME_PIXELS: u64 = 4096 * 4096;
const FIRST_FRAME_ATTEMPTS: usize = 120;

pub fn write_message<T: serde::Serialize>(
    writer: &mut impl Write,
    message: &T,
) -> anyhow::Result<()> {
    let options = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_WORKER_MESSAGE_BYTES);
    let bytes = options.serialize(message)?;
    if bytes.len() as u64 > MAX_WORKER_MESSAGE_BYTES {
        anyhow::bail!("worker message exceeds {} bytes", MAX_WORKER_MESSAGE_BYTES);
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_message<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> anyhow::Result<T> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header) as u64;
    if length > MAX_WORKER_MESSAGE_BYTES {
        anyhow::bail!("worker message length {length} exceeds safety limit");
    }
    let mut bytes = vec![0_u8; length as usize];
    reader.read_exact(&mut bytes)?;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_WORKER_MESSAGE_BYTES)
        .deserialize(&bytes)
        .map_err(Into::into)
}

pub fn token_hex(token: &[u8; 32]) -> String {
    token.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn token_from_hex(value: &str) -> anyhow::Result<[u8; 32]> {
    if value.len() != 64 {
        anyhow::bail!("worker token has invalid length");
    }
    let mut token = [0_u8; 32];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(token)
}

/// Entry point for the hidden worker command. Nothing outside this function
/// loads a native core in the GUI launch path.
pub fn run_worker() -> anyhow::Result<()> {
    let expected_token = std::env::var("RETROFEEL_WORKER_TOKEN")
        .map_err(|_| anyhow::anyhow!("worker authentication token is missing"))
        .and_then(|value| token_from_hex(&value))?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = BufWriter::new(stdout.lock());

    let request: WorkerRequest = read_message(&mut reader)?;
    let WorkerRequest::Launch(launch) = request else {
        send_error(
            &mut writer,
            WorkerErrorKind::Protocol,
            "first worker message must be Launch",
        );
        return Ok(());
    };
    if launch.protocol_version != WORKER_PROTOCOL_VERSION {
        send_error(
            &mut writer,
            WorkerErrorKind::Protocol,
            &format!(
                "worker protocol mismatch: GUI {}, worker {}",
                launch.protocol_version, WORKER_PROTOCOL_VERSION
            ),
        );
        return Ok(());
    }
    if launch.authentication_token != expected_token {
        send_error(
            &mut writer,
            WorkerErrorKind::Authentication,
            "worker authentication failed",
        );
        return Ok(());
    }

    let (request_tx, request_rx) = std::sync::mpsc::sync_channel(64);
    std::thread::Builder::new()
        .name("retrofeel-worker-ipc".into())
        .spawn(move || read_requests(reader, request_tx))?;

    run_core(*launch, request_rx, &mut writer)
}

fn read_requests(mut reader: BufReader<std::io::Stdin>, tx: SyncSender<WorkerRequest>) {
    while let Ok(request) = read_message(&mut reader) {
        let shutdown = matches!(request, WorkerRequest::Shutdown);
        if tx.send(request).is_err() || shutdown {
            break;
        }
    }
}

fn run_core(
    launch: WorkerLaunch,
    requests: Receiver<WorkerRequest>,
    writer: &mut impl Write,
) -> anyhow::Result<()> {
    let mut core = match Core::load(&launch.core_path, &launch.system_dir.to_string_lossy()) {
        Ok(core) => core,
        Err(error) => {
            send_error(
                writer,
                WorkerErrorKind::Core,
                &format!("Core load failed: {error}"),
            );
            return Ok(());
        }
    };
    if let Some(config) = launch.config.as_ref() {
        log_option_report(&retrofeel_backend::apply_configured_core_options(
            &core, config,
        ));
    }

    let rom_bytes = match read_content(&core, launch.rom_path.as_deref()) {
        Ok(bytes) => bytes,
        Err(error) => {
            send_error(writer, WorkerErrorKind::Content, &error.to_string());
            return Ok(());
        }
    };
    if let Err(error) = core.load_game(
        &rom_bytes,
        launch.rom_path.as_ref().and_then(|path| path.to_str()),
    ) {
        send_error(
            writer,
            WorkerErrorKind::Content,
            &format!("Content load failed: {error}"),
        );
        return Ok(());
    }

    let core_key = core.system_info().library_name.clone();
    if let Some(config) = launch.config.as_ref() {
        if let Err(error) =
            retrofeel_backend::load_sram(&mut core, config, &core_key, launch.rom_path.as_deref())
        {
            let _ = write_message(
                writer,
                &WorkerEvent::Status(format!("Could not load save RAM: {error}")),
            );
        }
    }

    let mut first_audio = Vec::new();
    let mut first_frame = None;
    for _ in 0..FIRST_FRAME_ATTEMPTS {
        match core.run_frame(Default::default()) {
            Ok(output) => {
                first_audio.extend(output.audio);
                if output.frame.is_some() {
                    first_frame = output.frame;
                    break;
                }
            }
            Err(error) => {
                send_error(
                    writer,
                    WorkerErrorKind::Core,
                    &format!("Core failed before its first frame: {error}"),
                );
                return Ok(());
            }
        }
    }
    let Some(first_frame) = first_frame else {
        send_error(
            writer,
            WorkerErrorKind::Core,
            "Core produced no video frame during launch",
        );
        return Ok(());
    };
    if let Err(message) = validate_frame(&first_frame) {
        send_error(writer, WorkerErrorKind::ResourceLimit, &message);
        return Ok(());
    }

    let av = core.av_info();
    write_message(
        writer,
        &WorkerEvent::Ready(WorkerReady {
            core_name: core.system_info().library_name.clone(),
            core_version: core.system_info().library_version.clone(),
            fps: av.fps,
            sample_rate: av.sample_rate,
            base_width: av.base_width,
            base_height: av.base_height,
            first_frame,
            first_audio,
        }),
    )?;

    let fps = if av.fps.is_finite() && av.fps > 0.0 {
        av.fps
    } else {
        60.0
    };
    let frame_duration = Duration::from_secs_f64(1.0 / fps);
    let mut next_deadline = Instant::now() + frame_duration;
    let mut latest_input = WorkerInput {
        mapped: Default::default(),
        raw_host: Default::default(),
    };
    let mut paused = false;
    let mut fast_forward = false;
    let mut recording: Option<RecordingHandle> = None;
    let mut recording_frame = 0_u64;
    let mut recording_limit = None;
    let mut auto_pause: Option<AutoPause> = None;
    let mut frames_since_sram_flush = 0_u32;
    let mut shutdown = false;

    while !shutdown {
        for request in requests.try_iter() {
            match request {
                WorkerRequest::Input(input) => latest_input = input,
                WorkerRequest::Command(command) => handle_command(
                    command,
                    &mut core,
                    &launch,
                    &core_key,
                    &rom_bytes,
                    &mut recording,
                    &mut recording_frame,
                    &mut recording_limit,
                    &mut auto_pause,
                    &mut paused,
                    &mut fast_forward,
                    writer,
                ),
                WorkerRequest::Shutdown => shutdown = true,
                WorkerRequest::Launch(_) => {
                    let _ = write_message(
                        writer,
                        &WorkerEvent::Status("Ignoring duplicate launch request".into()),
                    );
                }
            }
        }
        if shutdown {
            break;
        }
        if let Some(pause) = auto_pause.as_ref() {
            if pause.resume_at.is_some_and(|at| Instant::now() >= at) {
                paused = false;
                if let Some(recording) = recording.as_ref() {
                    recording.resume(recording_frame);
                }
                auto_pause = None;
            }
        }
        if paused {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }

        let output = match core.run_frame(latest_input.mapped) {
            Ok(output) => output,
            Err(error) => {
                send_error(
                    writer,
                    WorkerErrorKind::Core,
                    &format!("Core frame failed: {error}"),
                );
                break;
            }
        };
        if core.take_shutdown_requested() {
            let _ = write_message(
                writer,
                &WorkerEvent::Status("Core requested a graceful shutdown".into()),
            );
            break;
        }
        if let Some(frame) = output.frame.as_ref() {
            if let Err(message) = validate_frame(frame) {
                send_error(writer, WorkerErrorKind::ResourceLimit, &message);
                break;
            }
        }
        frames_since_sram_flush = frames_since_sram_flush.saturating_add(1);

        if let Some(active) = recording.as_ref() {
            active.frame(
                recording_frame,
                latest_input.mapped,
                Some(latest_input.raw_host.clone()),
                output.frame.clone(),
                output.audio.clone(),
            );
            recording_frame = recording_frame.saturating_add(1);
            if recording_limit.is_some_and(|limit| recording_frame >= limit) {
                if let Some(recording) = recording.take() {
                    recording.stop();
                }
                recording_limit = None;
                auto_pause = None;
            } else if let Some(pause) = auto_pause.as_mut() {
                if !pause.triggered && recording_frame >= pause.at_frame {
                    pause.triggered = true;
                    pause.resume_at = Some(Instant::now() + pause.duration);
                    paused = true;
                    active.pause(recording_frame);
                }
            }
        }

        if let Err(error) = write_message(
            writer,
            &WorkerEvent::Frame {
                frame: output.frame,
                audio: output.audio,
            },
        ) {
            log::warn!("worker transport closed: {error}");
            break;
        }

        if frames_since_sram_flush >= 300 {
            flush_sram(&mut core, &launch, &core_key);
            frames_since_sram_flush = 0;
        }
        if !fast_forward {
            let now = Instant::now();
            if now < next_deadline {
                std::thread::sleep(next_deadline - now);
            }
        }
        next_deadline += frame_duration;
        if next_deadline < Instant::now() {
            next_deadline = Instant::now() + frame_duration;
        }
    }

    if let Some(recording) = recording.take() {
        recording.stop();
    }
    flush_sram(&mut core, &launch, &core_key);
    let _ = write_message(writer, &WorkerEvent::Stopped);
    Ok(())
}

fn read_content(core: &Core, path: Option<&Path>) -> anyhow::Result<Vec<u8>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if core.system_info().need_fullpath {
        if !path.is_file() {
            anyhow::bail!("Content file is missing: {}", path.display());
        }
        return Ok(Vec::new());
    }
    std::fs::read(path)
        .map_err(|error| anyhow::anyhow!("Could not read content {}: {error}", path.display()))
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    command: WorkerCommand,
    core: &mut Core,
    launch: &WorkerLaunch,
    core_key: &str,
    _rom_bytes: &[u8],
    recording: &mut Option<RecordingHandle>,
    recording_frame: &mut u64,
    recording_limit: &mut Option<u64>,
    auto_pause: &mut Option<AutoPause>,
    paused: &mut bool,
    fast_forward: &mut bool,
    writer: &mut impl Write,
) {
    match command {
        WorkerCommand::SaveState(slot) => {
            let result = launch.config.as_ref().map_or_else(
                || Err(anyhow::anyhow!("no configuration is active")),
                |config| -> anyhow::Result<()> {
                    retrofeel_backend::save_state_slot(
                        core,
                        config,
                        core_key,
                        launch.rom_path.as_deref(),
                        slot,
                    )
                    .map(|_| ())?;
                    Ok(())
                },
            );
            report_result(writer, result, &format!("Saved state slot {slot}"));
        }
        WorkerCommand::LoadState(slot) => {
            let result = launch.config.as_ref().map_or_else(
                || Err(anyhow::anyhow!("no configuration is active")),
                |config| -> anyhow::Result<()> {
                    let loaded = retrofeel_backend::load_state_slot(
                        core,
                        config,
                        core_key,
                        launch.rom_path.as_deref(),
                        slot,
                    )?;
                    loaded
                        .map(|_| ())
                        .ok_or_else(|| anyhow::anyhow!("state slot is empty"))
                },
            );
            report_result(writer, result, &format!("Loaded state slot {slot}"));
        }
        WorkerCommand::Reset => {
            report_result(writer, core.reset().map_err(Into::into), "Core reset");
        }
        WorkerCommand::StartRecording {
            recordings_dir,
            stop_after_frames,
            pause_at_frame,
            pause_duration_ms,
        } => {
            if recording.is_some() {
                report_result(
                    writer,
                    Err(anyhow::anyhow!("recording is already active")),
                    "Recording started",
                );
                return;
            }
            let core_name = core.system_info().library_name.clone();
            let core_version = core.system_info().library_version.clone();
            let av = core.av_info();
            let initial_state_path = core.serialize().ok().and_then(|bytes| {
                let path = std::env::temp_dir().join(format!(
                    "retrofeel-init-{}-{}.bin",
                    std::process::id(),
                    recording_frame
                ));
                std::fs::write(&path, bytes).ok().map(|_| path)
            });
            let rom_identity = launch
                .rom_path
                .as_deref()
                .and_then(|path| crate::recording::rom_identity(path).ok());
            let start = RecordingStart {
                recordings_dir,
                core_name,
                core_version,
                core_path: launch.core_path.clone(),
                rom_path: launch.rom_path.clone(),
                rom_sha1: rom_identity.as_ref().map(|(sha1, _)| sha1.clone()),
                rom_size: rom_identity.map(|(_, size)| size),
                fps: av.fps,
                sample_rate: av.sample_rate,
                width: av.base_width,
                height: av.base_height,
                binding_map: launch
                    .config
                    .as_ref()
                    .map(|config| config.global_input_bindings.clone()),
                initial_state_path,
                capture_mic: launch
                    .config
                    .as_ref()
                    .is_none_or(|config| config.recording.mic_enabled),
                live_mic_peak: None,
                transcription: launch
                    .config
                    .as_ref()
                    .map(|config| config.recording.transcription.clone())
                    .unwrap_or_default(),
                capture_provenance: None,
                video_timing: None,
                track_alignment: None,
            };
            match RecordingHandle::start(start) {
                Ok(handle) => {
                    *recording_frame = 0;
                    *recording_limit = stop_after_frames;
                    *auto_pause = pause_at_frame.map(|at_frame| AutoPause {
                        at_frame,
                        duration: Duration::from_millis(pause_duration_ms),
                        triggered: false,
                        resume_at: None,
                    });
                    *recording = Some(handle);
                    let _ = write_message(writer, &WorkerEvent::Status("Recording started".into()));
                }
                Err(error) => {
                    report_result(writer, Err(anyhow::Error::from(error)), "Recording started")
                }
            }
        }
        WorkerCommand::StopRecording => {
            if let Some(handle) = recording.take() {
                handle.stop();
            }
            *recording_limit = None;
            *auto_pause = None;
        }
        WorkerCommand::SetPaused(value) => {
            *paused = value;
            if let Some(recording) = recording.as_ref() {
                if value {
                    recording.pause(*recording_frame);
                } else {
                    recording.resume(*recording_frame);
                }
            }
        }
        WorkerCommand::SetFastForward(value) => *fast_forward = value,
    }
}

fn report_result(writer: &mut impl Write, result: anyhow::Result<()>, success_message: &str) {
    let message = match result {
        Ok(()) => success_message.to_string(),
        Err(error) => format!("{success_message} failed: {error}"),
    };
    let _ = write_message(writer, &WorkerEvent::Status(message));
}

fn validate_frame(frame: &retrofeel_types::VideoFrame) -> Result<(), String> {
    let pixels = u64::from(frame.width) * u64::from(frame.height);
    if pixels == 0 || pixels > MAX_FRAME_PIXELS {
        return Err(format!(
            "Core frame dimensions {}x{} exceed safety limits",
            frame.width, frame.height
        ));
    }
    let expected = pixels.saturating_mul(4);
    if frame.rgba.len() as u64 != expected {
        return Err(format!(
            "Core frame has {} bytes; expected {expected}",
            frame.rgba.len()
        ));
    }
    Ok(())
}

fn flush_sram(core: &mut Core, launch: &WorkerLaunch, core_key: &str) {
    let Some(config) = launch.config.as_ref() else {
        return;
    };
    if let Err(error) =
        retrofeel_backend::flush_sram(core, config, core_key, launch.rom_path.as_deref())
    {
        log::warn!("worker failed to flush save RAM: {error}");
    }
}

fn send_error(writer: &mut impl Write, kind: WorkerErrorKind, message: &str) {
    let _ = write_message(
        writer,
        &WorkerEvent::Error {
            kind,
            message: message.to_string(),
        },
    );
}

fn log_option_report(report: &retrofeel_backend::CoreOptionApplyReport) {
    for (key, value) in &report.applied {
        log::info!("worker applied core option {key}={value}");
    }
    for (key, value) in &report.unknown {
        log::warn!("worker ignored unknown core option {key}={value}");
    }
    for invalid in &report.invalid {
        log::warn!(
            "worker ignored invalid core option {}={}",
            invalid.key,
            invalid.value
        );
    }
}

struct AutoPause {
    at_frame: u64,
    duration: Duration,
    triggered: bool,
    resume_at: Option<Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_messages_round_trip() {
        let message = WorkerRequest::Shutdown;
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).unwrap();
        let decoded: WorkerRequest = read_message(&mut bytes.as_slice()).unwrap();
        assert!(matches!(decoded, WorkerRequest::Shutdown));
    }

    #[test]
    fn oversized_messages_are_rejected_before_allocation() {
        let bytes = ((MAX_WORKER_MESSAGE_BYTES as u32) + 1)
            .to_le_bytes()
            .to_vec();
        let result: anyhow::Result<WorkerRequest> = read_message(&mut bytes.as_slice());
        assert!(result.unwrap_err().to_string().contains("safety limit"));
    }

    #[test]
    fn frame_validation_rejects_bad_lengths_and_dimensions() {
        let bad_length = retrofeel_types::VideoFrame {
            width: 2,
            height: 2,
            rgba: vec![0; 15],
        };
        assert!(validate_frame(&bad_length).is_err());
        let too_large = retrofeel_types::VideoFrame {
            width: 4097,
            height: 4097,
            rgba: Vec::new(),
        };
        assert!(validate_frame(&too_large).is_err());
    }
}
