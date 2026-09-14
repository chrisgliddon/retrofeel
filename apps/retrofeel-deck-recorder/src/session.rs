#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use retrofeel_types::{
    input_transitions_from_frames, CaptureProvenance, CaptureSourceKind, CoreInfo,
    ExternalCaptureInfo, ExternalCaptureKind, ExternalCaptureStatus, ExternalVideoClock,
    ManifestFormat, SessionManifest, TimingInfo, TrackAlignment, TrackAlignmentMetadata,
    TrackAlignmentStatus, TrackPresence, TranscriptionJobState, VideoTiming, VideoTimingKind,
};
use serde::{Deserialize, Serialize};

use crate::config::RecorderConfig;
use crate::model::{
    ControllerMap, InputDeviceInfo, InputDeviceLifecycle, InputDeviceSource, RawInputEvent,
};
use crate::steam_log::{RecordingMode, SteamLogEvent};
use crate::timeline::sample_input_frames;
use crate::vdf;

#[cfg(target_os = "linux")]
use crate::steam_log;

const INPUT_JSON: &str = "input.json";
const INPUT_RON: &str = "input.ron";
const INPUT_TRANSITIONS_JSONL: &str = "input-transitions.jsonl";
const RAW_EVENTS_JSONL: &str = "input-events.jsonl";
const DEVICES_JSON: &str = "input-devices.json";
const CONTROLLER_MAP_JSON: &str = "controller-map.json";
const MANIFEST_JSON: &str = "manifest.json";
const MANIFEST_RON: &str = "manifest.ron";

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub poll_interval: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(20),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub game_id: String,
    /// Wall-clock recording start, Unix epoch seconds from the manifest.
    pub start_timestamp: f64,
    pub frame_count: u64,
    pub status: ExternalCaptureStatus,
    pub directory: PathBuf,
    pub video_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub fn run_doctor(config: &RecorderConfig) -> Result<Vec<DoctorCheck>> {
    let streaming_log = steam_streaming_log(config);
    let controller_log = steam_controller_log(config);
    let mut checks = vec![
        path_check("Steam root", &config.steam_root, true),
        path_check("Steam Game Recording log", &streaming_log, true),
        path_check("Steam controller log", &controller_log, false),
        path_check("ffprobe", &config.ffprobe, true),
        path_check("ffmpeg", &config.ffmpeg, true),
    ];

    if config.transcription.automatic || config.transcription.model.is_some() {
        checks.push(path_check(
            "whisper.cpp executable",
            &config.transcription.executable,
            true,
        ));
        checks.push(match config.transcription.model.as_deref() {
            Some(model) => path_check("Whisper model", model, true),
            None => DoctorCheck {
                name: "Whisper model".into(),
                ok: false,
                detail: "automatic transcription is enabled but no model is configured".into(),
            },
        });
    }

    let output_parent = config
        .recordings_dir
        .parent()
        .unwrap_or(&config.recordings_dir);
    checks.push(DoctorCheck {
        name: "Recording destination".into(),
        ok: config.recordings_dir.is_dir() || output_parent.is_dir(),
        detail: config.recordings_dir.display().to_string(),
    });

    #[cfg(target_os = "linux")]
    {
        match crate::linux::discover_devices(config) {
            Ok(devices) if devices.is_empty() => checks.push(DoctorCheck {
                name: "Input capture device".into(),
                ok: false,
                detail: "no admitted Steam virtual, physical HID, or exact allowlisted evdev input devices are currently visible"
                    .into(),
            }),
            Ok(devices) => checks.push(DoctorCheck {
                name: "Input capture device".into(),
                ok: true,
                detail: devices
                    .iter()
                    .map(|device| {
                        format!(
                            "{} [{:?}] ({})",
                            device.name,
                            device.source,
                            device.event_path.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
            Err(error) => checks.push(DoctorCheck {
                name: "Input capture device".into(),
                ok: false,
                detail: error.to_string(),
            }),
        }

        match crate::linux::discover_evdev_candidates() {
            Ok(devices) => {
                for device in devices
                    .iter()
                    .filter(|device| device.is_keyboard || device.is_mouse)
                {
                    let admission = config.device_admission(device);
                    let matching_rules = config
                        .input_device_allowlist
                        .iter()
                        .enumerate()
                        .filter(|(_, rule)| rule.matches_identity(device))
                        .collect::<Vec<_>>();
                    let rule_issues = matching_rules
                        .iter()
                        .flat_map(|(index, rule)| {
                            rule.match_issues(device)
                                .into_iter()
                                .map(move |issue| format!("allowlist entry {}: {issue}", index + 1))
                        })
                        .collect::<Vec<_>>();
                    // Opening is appropriate only after admission. Rejected
                    // keyboard/mouse nodes remain sysfs-only even in doctor.
                    let readability = admission.is_admitted().then(|| {
                        File::open(&device.event_path)
                            .map(|_| "evdev open succeeded".to_string())
                            .unwrap_or_else(|error| format!("evdev open failed: {error}"))
                    });
                    let readable = readability
                        .as_deref()
                        .is_none_or(|result| result == "evdev open succeeded");
                    let rule_issue_detail = if rule_issues.is_empty() {
                        String::new()
                    } else {
                        format!("; configuration error: {}", rule_issues.join("; "))
                    };
                    checks.push(DoctorCheck {
                        name: "Keyboard/mouse policy".into(),
                        ok: (matching_rules.is_empty() || admission.is_admitted())
                            && rule_issues.is_empty()
                            && readable,
                        detail: format!(
                            "{} ({}) id={} vendor={:04x} product={:04x} unique={} physical={} virtual={} available={}: {}{}{}",
                            device.name,
                            device.event_path.display(),
                            device.device_id,
                            device.vendor,
                            device.product,
                            device.unique.as_deref().unwrap_or("<none>"),
                            device.physical_path.as_deref().unwrap_or("<none>"),
                            device.is_virtual,
                            device.capabilities().labels().join("+"),
                            admission.reason,
                            readability
                                .as_deref()
                                .map(|result| format!("; {result}"))
                                .unwrap_or_default(),
                            rule_issue_detail,
                        ),
                    });
                }
                for (index, rule) in config.input_device_allowlist.iter().enumerate() {
                    let matching_devices = devices
                        .iter()
                        .filter(|device| rule.matches_identity(device))
                        .collect::<Vec<_>>();
                    if matching_devices.is_empty() {
                        checks.push(DoctorCheck {
                            name: format!("Input allowlist #{}", index + 1),
                            ok: false,
                            detail: format!(
                                "no exact visible match for {} vendor={:04x} product={:04x} unique={} physical={}",
                                rule.name,
                                rule.vendor,
                                rule.product,
                                rule.unique_id.as_deref().unwrap_or("<any>"),
                                rule.physical_path.as_deref().unwrap_or("<any>"),
                            ),
                        });
                        continue;
                    }
                    let issues = matching_devices
                        .iter()
                        .flat_map(|device| {
                            rule.match_issues(device)
                                .into_iter()
                                .map(move |issue| format!("{}: {issue}", device.name))
                        })
                        .collect::<Vec<_>>();
                    if !issues.is_empty() {
                        checks.push(DoctorCheck {
                            name: format!("Input allowlist #{}", index + 1),
                            ok: false,
                            detail: issues.join("; "),
                        });
                    }
                }
            }
            Err(error) => checks.push(DoctorCheck {
                name: "Keyboard/mouse policy".into(),
                ok: false,
                detail: error.to_string(),
            }),
        }
    }
    #[cfg(not(target_os = "linux"))]
    checks.push(DoctorCheck {
        name: "Steam virtual input".into(),
        ok: false,
        detail: "evdev capture is available only on Linux".into(),
    });

    Ok(checks)
}

fn path_check(name: &str, path: &Path, required: bool) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        ok: path.exists() || !required,
        detail: if path.exists() {
            path.display().to_string()
        } else if required {
            format!("missing: {}", path.display())
        } else {
            format!("optional and not found: {}", path.display())
        },
    }
}

pub fn list_sessions(config: &RecorderConfig) -> Result<Vec<SessionSummary>> {
    if !config.recordings_dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = fs::read_dir(&config.recordings_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with(".partial-"))
        .filter_map(|entry| session_summary(&entry.path()).transpose())
        .collect::<Result<Vec<_>>>()?;
    sessions.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(sessions)
}

fn session_summary(directory: &Path) -> Result<Option<SessionSummary>> {
    let manifest_path = directory.join(MANIFEST_JSON);
    if !manifest_path.exists() {
        return Ok(None);
    }
    let manifest: SessionManifest = serde_json::from_reader(BufReader::new(
        File::open(&manifest_path)
            .with_context(|| format!("failed to open {}", manifest_path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let Some(external) = manifest.external_capture else {
        return Ok(None);
    };
    Ok(Some(SessionSummary {
        id: external.recording_id,
        game_id: external.game_id,
        start_timestamp: manifest.timing.start_timestamp,
        frame_count: manifest.frame_count,
        status: external.status,
        directory: directory.to_path_buf(),
        video_source: external.source_video.map(PathBuf::from),
    }))
}

/// Repair only into a fresh derivative root, retaining hashes of the original.
pub fn derive_session(config: &RecorderConfig, id: &str, out: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    if safe_id(id) != id || !id.starts_with("fg_") {
        bail!("expected an exact on-demand recording ID");
    }
    let completed = config.recordings_dir.join(id);
    // Failed finalization leaves the raw recording under its .partial- name.
    // Recover that exact stopped session into a derivative without reconciling
    // unrelated recordings or modifying the retained original.
    let source = if completed.try_exists()? {
        completed
    } else {
        config.recordings_dir.join(format!(".partial-{id}"))
    }
    .canonicalize()?;
    let original: OwnedCaptureMetadata =
        serde_json::from_reader(File::open(source.join("capture.json"))?)?;
    for name in [DEVICES_JSON, RAW_EVENTS_JSONL] {
        if !source.join(name).is_file() {
            bail!("required original sidecar is missing: {name}");
        }
    }
    let output_parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    if output_parent.starts_with(&source) {
        bail!("derivative root must be outside the original session");
    }
    if original.recording_id != id || !original.stopped {
        bail!("source recording identity mismatch or recording is not stopped");
    }
    // create_dir (not create_dir_all) makes an existing destination an error.
    fs::create_dir(out).context("derivative root must be new and its parent must exist")?;
    let destination = out.join(format!(".partial-{id}"));
    fs::create_dir(&destination)?;
    let mut source_hashes = BTreeMap::new();
    for name in [
        "capture.json",
        DEVICES_JSON,
        RAW_EVENTS_JSONL,
        CONTROLLER_MAP_JSON,
        MANIFEST_JSON,
        INPUT_JSON,
    ] {
        let path = source.join(name);
        if !path.exists() {
            continue;
        }
        if !fs::symlink_metadata(&path)?.is_file() {
            bail!("source sidecar is not a regular file: {name}");
        }
        let bytes = fs::read(&path)?;
        source_hashes.insert(name, format!("{:x}", Sha256::digest(&bytes)));
        if name != MANIFEST_JSON && name != INPUT_JSON {
            fs::write(destination.join(name), &bytes)?;
        }
    }
    let layouts = source.join("controller-layouts");
    if layouts.is_dir() {
        fs::create_dir(destination.join("controller-layouts"))?;
        for entry in fs::read_dir(layouts)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                bail!("controller layout is not a regular file");
            }
            fs::copy(
                entry.path(),
                destination
                    .join("controller-layouts")
                    .join(entry.file_name()),
            )?;
        }
    }
    write_json_atomic(
        &destination.join("repair-provenance.json"),
        &serde_json::json!({
            "schema_version": 1, "source_directory": source, "source_hashes": source_hashes,
            "created_unix_seconds": unix_now_seconds(), "method": "exact_segment_decoded_frames_and_clip_clock",
            "recorder_version": env!("CARGO_PKG_VERSION"),
        }),
    )?;
    let mut derived_config = config.clone();
    derived_config.recordings_dir = out.to_path_buf();
    let mut derived = ActiveSession::resume(&destination)?;
    // Re-read retained source bytes before writing canonical derivatives.
    for (name, expected) in source_hashes {
        if format!("{:x}", Sha256::digest(fs::read(source.join(name))?)) != expected {
            bail!("original sidecar changed during derivation: {name}");
        }
    }
    match finalize_session(&derived_config, &mut derived) {
        Ok(directory) => Ok(directory),
        Err(error) => {
            write_json_atomic(
                &destination.join("finalization-health.json"),
                &serde_json::json!({
                    "schema_version": 1, "status": "partial", "reason": format!("{error:#}"),
                }),
            )?;
            Err(error)
        }
    }
}

pub fn reconcile_sessions(config: &RecorderConfig) -> Result<Vec<String>> {
    if !config.recordings_dir.exists() {
        return Ok(Vec::new());
    }
    let mut findings = Vec::new();
    for entry in fs::read_dir(&config.recordings_dir)?.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(".partial-") || !entry.path().is_dir() {
            continue;
        }
        let metadata_path = entry.path().join("capture.json");
        if !metadata_path.exists() {
            findings.push(format!(
                "{name}: incomplete session preserved for manual inspection"
            ));
            continue;
        }
        let mut session = match ActiveSession::resume(&entry.path()) {
            Ok(session) => session,
            Err(error) => {
                findings.push(format!("{name}: could not read recovery data: {error:#}"));
                continue;
            }
        };
        if session.clip_id.is_none() && !matches!(session.mode, RecordingMode::DesktopSession) {
            findings.push(format!(
                "{name}: input is preserved, but Steam had not reported a matching clip before shutdown"
            ));
            continue;
        }
        match finalize_session(config, &mut session) {
            Ok(directory) => {
                crate::transcription::schedule_steam_audio_transcription(config, &directory);
                findings.push(format!("{name}: finalized at {}", directory.display()));
            }
            Err(error) => findings.push(format!(
                "{name}: still partial; finalization failed: {error:#}"
            )),
        }
    }
    Ok(findings)
}

pub fn export_video(
    config: &RecorderConfig,
    session_id: &str,
    output: Option<&Path>,
    stdout: bool,
) -> Result<()> {
    if stdout && output.is_some() {
        bail!("choose either --stdout or --output");
    }
    let session = find_session(config, session_id)?;
    let source = session
        .video_source
        .ok_or_else(|| anyhow!("session {session_id} has no Steam video source"))?;
    if !source.exists() {
        bail!(
            "Steam video source is no longer available: {}",
            source.display()
        );
    }

    let mut command = Command::new(&config.ffmpeg);
    command
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(&source)
        // Steam's DASH demuxer can stop the video adaptation set at the audio
        // boundary when both are mapped from one input, silently dropping one
        // or two trailing video frames while ffmpeg still exits successfully.
        // Opening the same local playlist independently keeps the complete
        // video clock while retaining the optional mixed-audio track.
        .arg("-i")
        .arg(&source)
        .args(["-map", "0:v:0", "-map", "1:a:0?"])
        .args(["-c", "copy"]);
    if stdout {
        command
            .args(["-f", "matroska", "pipe:1"])
            .stdout(Stdio::inherit());
    } else {
        let output = output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| session.directory.join("video.mkv"));
        command.arg("-y").arg(&output);
    }
    command.stderr(Stdio::inherit());
    let status = command.status().context("failed to start ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg failed with {status}");
    }
    Ok(())
}

fn find_session(config: &RecorderConfig, id: &str) -> Result<SessionSummary> {
    list_sessions(config)?
        .into_iter()
        .find(|session| session.id == id)
        .ok_or_else(|| anyhow!("recording session not found: {id}"))
}

#[cfg(target_os = "linux")]
pub fn watch(config: RecorderConfig, options: WatchOptions) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::linux::{InputMessage, InputMonitor};

    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn stop_handler(_: libc::c_int) {
        STOP.store(true, Ordering::Release);
    }
    // SAFETY: the handler only performs an atomic store, which is signal-safe.
    unsafe {
        let handler = stop_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    fs::create_dir_all(&config.recordings_dir).with_context(|| {
        format!(
            "failed to create recording directory {}",
            config.recordings_dir.display()
        )
    })?;
    crate::archive::spawn_archive_worker(config.clone());
    crate::youtube::spawn_youtube_worker(config.clone());
    let mut tail = LogTail::open(&steam_streaming_log(&config))?;
    let input = InputMonitor::start(&config);
    let mut devices = BTreeMap::<String, InputDeviceInfo>::new();
    let mut ring = VecDeque::<RawInputEvent>::new();
    let mut held_state = BTreeMap::<(String, u16, u16), RawInputEvent>::new();
    let ring_duration_us = config.input_ring_seconds.saturating_mul(1_000_000);
    let mut active: Option<ActiveSession> = None;
    let finalizer = spawn_finalizer(config.clone())?;

    log::info!("watching Steam Game Recording at {}", tail.path.display());
    while !STOP.load(Ordering::Acquire) {
        for message in input.receiver.try_iter() {
            match message {
                InputMessage::DeviceAdded {
                    device,
                    boottime_us,
                } => {
                    let was_known = devices.contains_key(&device.device_id);
                    log::info!(
                        "capturing {} on {}{}",
                        device.name,
                        device.event_path.display(),
                        if was_known { " (re-added)" } else { "" }
                    );
                    if let Some(active) = active.as_mut().filter(|session| !session.stopped) {
                        if active.consider_device(&config, &device)? {
                            log::info!(
                                "{} added eligible {:?} input track {}",
                                active.recording_id,
                                device.source,
                                device.device_id
                            );
                        } else {
                            log::debug!(
                                "{} ignored ineligible {:?} input {}",
                                active.recording_id,
                                device.source,
                                device.device_id
                            );
                        }
                    }
                    let lifecycle = RawInputEvent::lifecycle(
                        boottime_us,
                        device.device_id.clone(),
                        InputDeviceLifecycle::Added,
                    );
                    if let Some(active) = active.as_mut().filter(|session| !session.stopped) {
                        active.push_event(&lifecycle)?;
                    }
                    held_state.retain(|(id, _, _), _| id != &device.device_id);
                    push_ring_event(&mut ring, lifecycle, ring_duration_us);
                    devices.insert(device.device_id.clone(), device);
                }
                InputMessage::Event(event) => {
                    if is_gamepad_held_event(&event, &devices) {
                        held_state.insert(
                            (event.device_id.clone(), event.event_type, event.code),
                            event.clone(),
                        );
                    }
                    if let Some(active) = active.as_mut().filter(|session| !session.stopped) {
                        active.push_event(&event)?;
                    }
                    push_ring_event(&mut ring, event, ring_duration_us);
                }
                InputMessage::DeviceRemoved {
                    device_id,
                    event_path,
                    boottime_us,
                } => {
                    if devices
                        .get(&device_id)
                        .is_some_and(|device| device.event_path != event_path)
                    {
                        log::debug!(
                            "ignoring stale removal for {device_id} on {}",
                            event_path.display()
                        );
                        continue;
                    }
                    log::info!("input device removed: {device_id}");
                    let lifecycle = RawInputEvent::lifecycle(
                        boottime_us,
                        device_id.clone(),
                        InputDeviceLifecycle::Removed,
                    );
                    if let Some(active) = active.as_mut().filter(|session| !session.stopped) {
                        active.push_event(&lifecycle)?;
                    }
                    push_ring_event(&mut ring, lifecycle, ring_duration_us);
                    devices.remove(&device_id);
                    held_state.retain(|(id, _, _), _| id != &device_id);
                }
            }
        }

        // Prune against the current clock even when no input arrives. This
        // keeps keyboard/mouse pre-roll inside the configured privacy bound.
        prune_ring_events(&mut ring, crate::linux::boottime_us(), ring_duration_us);

        for line in tail.poll_lines()? {
            let Some(event) = steam_log::parse_line(&line) else {
                continue;
            };
            handle_steam_event(event, &config, &devices, &ring, &held_state, &mut active)?;
            if active.as_ref().is_some_and(|session| {
                session.stopped
                    && (session.clip_id.is_some()
                        || matches!(session.mode, RecordingMode::DesktopSession))
            }) {
                let session = active.take().expect("ready session exists");
                if let Err(error) = finalizer.try_send(session) {
                    let session = match error {
                        std::sync::mpsc::TrySendError::Full(session)
                        | std::sync::mpsc::TrySendError::Disconnected(session) => session,
                    };
                    write_json_atomic(
                        &session.directory.join("finalization-health.json"),
                        &serde_json::json!({
                            "schema_version": 1, "status": "partial", "reason": "finalization queue unavailable; raw capture retained for reconcile",
                        }),
                    )?;
                    log::warn!(
                        "finalization queue unavailable for {}; partial capture retained",
                        session.recording_id
                    );
                }
            }
        }
        thread::sleep(options.poll_interval);
    }

    if let Some(mut active) = active {
        active.flush()?;
        log::warn!(
            "shutdown left {} as a recoverable partial session",
            active.recording_id
        );
    }
    Ok(())
}

fn push_ring_event(
    ring: &mut VecDeque<RawInputEvent>,
    event: RawInputEvent,
    ring_duration_us: u64,
) {
    let event_time = event.boottime_us;
    ring.push_back(event);
    prune_ring_events(ring, event_time, ring_duration_us);
}

fn prune_ring_events(
    ring: &mut VecDeque<RawInputEvent>,
    now_boottime_us: u64,
    ring_duration_us: u64,
) {
    // Reader threads can deliver slightly out of timestamp order, so enforce
    // the bound for every event rather than assuming the front is oldest.
    ring.retain(|event| now_boottime_us.saturating_sub(event.boottime_us) <= ring_duration_us);
}

fn is_gamepad_held_event(
    event: &RawInputEvent,
    devices: &BTreeMap<String, InputDeviceInfo>,
) -> bool {
    let Some(device) = devices.get(&event.device_id) else {
        return false;
    };
    if !device.captured_capabilities().gamepad || event.lifecycle.is_some() {
        return false;
    }
    event.event_type == 3
        || (event.event_type == 1
            && ((0x120..=0x15f).contains(&event.code)
                || (0x220..=0x223).contains(&event.code)
                || (0x2c0..=0x2ff).contains(&event.code)))
}

fn handle_steam_event(
    event: SteamLogEvent,
    config: &RecorderConfig,
    devices: &BTreeMap<String, InputDeviceInfo>,
    ring: &VecDeque<RawInputEvent>,
    held_state: &BTreeMap<(String, u16, u16), RawInputEvent>,
    active: &mut Option<ActiveSession>,
) -> Result<()> {
    match event {
        SteamLogEvent::RecordingStarted {
            recording_id,
            game_id,
            mode: RecordingMode::Background,
        } => {
            log::debug!("ignoring Steam background recording {recording_id} for {game_id}");
        }
        SteamLogEvent::RecordingStarted {
            recording_id,
            game_id,
            mode: mode @ (RecordingMode::OnDemand | RecordingMode::DesktopSession),
        } => {
            if let Some(mut previous) = active.take() {
                previous.flush()?;
                log::warn!(
                    "new recording started before {} completed; preserving its partial session",
                    previous.recording_id
                );
            }
            let recording_id = if recording_id.is_empty() {
                desktop_recording_id(&game_id)
            } else {
                recording_id
            };
            let session = ActiveSession::start(
                config,
                recording_id,
                game_id,
                mode,
                devices,
                ring,
                held_state,
            )?;
            log::info!(
                "capturing input for {} ({:?})",
                session.recording_id,
                session.mode
            );
            *active = Some(session);
        }
        SteamLogEvent::VideoAnchor {
            source_pts_us,
            normalized_pts_us,
        } => {
            if let Some(active) = active.as_mut() {
                let anchor = (source_pts_us, normalized_pts_us);
                if active
                    .video_anchor
                    .is_some_and(|previous| previous != anchor)
                {
                    active.video_anchor_conflict = true;
                } else {
                    active.video_anchor = Some(anchor);
                }
                active.write_metadata()?;
            }
        }
        SteamLogEvent::RecordingStopped { recording_id } => {
            let should_take = active.as_ref().is_some_and(|session| {
                recording_id.is_empty() || session.recording_id == recording_id
            });
            if !should_take {
                return Ok(());
            }
            if let Some(mut session) = active.take() {
                session.stopped = true;
                session.flush()?;
                session.write_metadata()?;
                // The watcher hands ready stopped sessions to its bounded decoder
                // worker; input ingress must never wait for media decoding.
                *active = Some(session);
            }
        }
        SteamLogEvent::ClipSaved {
            clip_id,
            timeline_id,
        } => {
            if active.as_ref().is_some_and(|session| {
                !session.stopped || !clip_id.starts_with(&format!("clip_{}_", session.game_id))
            }) {
                log::debug!("ignoring unrelated Steam clip {clip_id}");
                return Ok(());
            }
            if let Some(mut session) = active.take() {
                session.clip_id = Some(clip_id);
                session.timeline_id = timeline_id;
                session.write_metadata()?;
                *active = Some(session);
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn spawn_finalizer(config: RecorderConfig) -> Result<std::sync::mpsc::SyncSender<ActiveSession>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<ActiveSession>(4);
    thread::Builder::new()
        .name("deck-finalizer".into())
        .spawn(move || {
            for session in receiver {
                let mut pending = Some(session);
                while let Some(session) = pending.take() {
                    if let Err(error) = try_finalize(&config, session, &mut pending) {
                        log::error!("finalization failed; raw session retained: {error:#}");
                        break;
                    }
                    if pending
                        .as_ref()
                        .is_some_and(|session| session.finalize_retry.exhausted(&config))
                    {
                        break;
                    }
                    if let Some(session) = &pending {
                        while !session.finalize_retry.is_due(Instant::now()) {
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
        })?;
    Ok(sender)
}

fn try_finalize(
    config: &RecorderConfig,
    mut session: ActiveSession,
    active: &mut Option<ActiveSession>,
) -> Result<()> {
    match finalize_session(config, &mut session) {
        Ok(directory) => {
            log::info!(
                "finalized {} at {}",
                session.recording_id,
                directory.display()
            );
            crate::transcription::schedule_steam_audio_transcription(config, &directory);
            Ok(())
        }
        Err(error) => {
            write_json_atomic(
                &session.directory.join("finalization-health.json"),
                &serde_json::json!({
                    "schema_version": 1, "status": "partial", "recording_id": session.recording_id,
                    "clip_id": session.clip_id, "reason": format!("{error:#}"),
                }),
            )?;
            session
                .finalize_retry
                .mark_attempt_failed(config, Instant::now());
            session.flush()?;
            session.write_metadata()?;
            if session.finalize_retry.exhausted(config) {
                log::error!(
                    "could not finalize {} after {} attempts; partial session retained: {error:#}",
                    session.recording_id,
                    session.finalize_retry.attempts
                );
            } else {
                log::warn!(
                    "could not finalize {} (attempt {}); retry scheduled: {error:#}",
                    session.recording_id,
                    session.finalize_retry.attempts
                );
            }
            *active = Some(session);
            Ok(())
        }
    }
}

fn desktop_recording_id(game_id: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("desktop_{game_id}_{stamp}")
}

struct ActiveSession {
    recording_id: String,
    game_id: String,
    mode: RecordingMode,
    directory: PathBuf,
    observed_boottime_us: u64,
    start_timestamp: f64,
    video_anchor: Option<(u64, u64)>,
    video_anchor_conflict: bool,
    clip_id: Option<String>,
    timeline_id: Option<String>,
    stopped: bool,
    input_source: Option<InputDeviceSource>,
    finalize_retry: FinalizeRetry,
    devices: BTreeMap<String, InputDeviceInfo>,
    raw_writer: BufWriter<File>,
}

#[derive(Serialize)]
struct CaptureMetadata<'a> {
    recording_id: &'a str,
    game_id: &'a str,
    mode: &'a str,
    observed_boottime_us: u64,
    start_timestamp: f64,
    video_anchor: Option<(u64, u64)>,
    video_anchor_conflict: bool,
    clip_id: &'a Option<String>,
    timeline_id: &'a Option<String>,
    stopped: bool,
    /// Preferred source for the legacy merged/mapped input projection.
    input_source: Option<InputDeviceSource>,
    /// Every source whose eligible devices are retained in this capture.
    input_sources: Vec<InputDeviceSource>,
}

#[derive(Deserialize)]
struct OwnedCaptureMetadata {
    recording_id: String,
    game_id: String,
    #[serde(default)]
    mode: Option<String>,
    observed_boottime_us: u64,
    start_timestamp: f64,
    video_anchor: Option<(u64, u64)>,
    #[serde(default)]
    video_anchor_conflict: bool,
    clip_id: Option<String>,
    timeline_id: Option<String>,
    stopped: bool,
    #[serde(default)]
    input_source: Option<InputDeviceSource>,
}

#[derive(Debug, Default)]
struct FinalizeRetry {
    attempts: u32,
    retry_after: Option<Instant>,
}

impl FinalizeRetry {
    fn mark_attempt_failed(&mut self, config: &RecorderConfig, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.retry_after = (self.attempts < config.finalize_retry_attempts)
            .then(|| now + Duration::from_secs(config.finalize_retry_interval_seconds.max(1)));
    }

    fn is_due(&self, now: Instant) -> bool {
        self.retry_after
            .is_some_and(|retry_after| now >= retry_after)
    }

    fn exhausted(&self, config: &RecorderConfig) -> bool {
        self.attempts >= config.finalize_retry_attempts
    }
}

fn preferred_input_source(
    config: &RecorderConfig,
    devices: &BTreeMap<String, InputDeviceInfo>,
) -> Option<InputDeviceSource> {
    if devices
        .values()
        .any(|device| device.source == InputDeviceSource::SteamVirtual)
    {
        Some(InputDeviceSource::SteamVirtual)
    } else if devices.values().any(|device| {
        device.source == InputDeviceSource::PhysicalFallback
            && config
                .physical_gamepad_priority(
                    device.vendor,
                    device.product,
                    &device.name,
                    device.unique.as_deref(),
                )
                .is_some()
    }) {
        Some(InputDeviceSource::PhysicalFallback)
    } else if devices.values().any(|device| {
        device.source == InputDeviceSource::AllowlistedEvdev
            && config.device_admission(device).is_admitted()
    }) {
        Some(InputDeviceSource::AllowlistedEvdev)
    } else {
        None
    }
}

fn captured_input_sources(devices: &BTreeMap<String, InputDeviceInfo>) -> Vec<InputDeviceSource> {
    [
        InputDeviceSource::SteamVirtual,
        InputDeviceSource::PhysicalFallback,
        InputDeviceSource::AllowlistedEvdev,
    ]
    .into_iter()
    .filter(|source| devices.values().any(|device| device.source == *source))
    .collect()
}

fn eligible_input_devices(
    config: &RecorderConfig,
    devices: &BTreeMap<String, InputDeviceInfo>,
) -> BTreeMap<String, InputDeviceInfo> {
    devices
        .iter()
        .filter(|(_, device)| {
            device.source == InputDeviceSource::SteamVirtual
                || (device.source == InputDeviceSource::PhysicalFallback
                    && config
                        .physical_gamepad_priority(
                            device.vendor,
                            device.product,
                            &device.name,
                            device.unique.as_deref(),
                        )
                        .is_some())
                || (device.source == InputDeviceSource::AllowlistedEvdev
                    && config.device_admission(device).is_admitted())
        })
        .map(|(id, device)| (id.clone(), device.clone()))
        .collect()
}

fn recording_mode_label(mode: RecordingMode) -> &'static str {
    match mode {
        RecordingMode::OnDemand => "on_demand",
        RecordingMode::Background => "background",
        RecordingMode::DesktopSession => "desktop_session",
    }
}

fn parse_recording_mode(value: Option<&str>, recording_id: &str) -> RecordingMode {
    match value {
        Some("desktop_session") => RecordingMode::DesktopSession,
        Some("background") => RecordingMode::Background,
        Some("on_demand") => RecordingMode::OnDemand,
        _ if recording_id.starts_with("desktop_") => RecordingMode::DesktopSession,
        _ if recording_id.starts_with("bg_") => RecordingMode::Background,
        _ => RecordingMode::OnDemand,
    }
}

impl ActiveSession {
    fn start(
        config: &RecorderConfig,
        recording_id: String,
        game_id: String,
        mode: RecordingMode,
        devices: &BTreeMap<String, InputDeviceInfo>,
        ring: &VecDeque<RawInputEvent>,
        held_state: &BTreeMap<(String, u16, u16), RawInputEvent>,
    ) -> Result<Self> {
        let input_source = preferred_input_source(config, devices);
        let devices = eligible_input_devices(config, devices);
        let directory = config
            .recordings_dir
            .join(format!(".partial-{}", safe_id(&recording_id)));
        fs::create_dir_all(&directory)?;
        let raw_path = directory.join(RAW_EVENTS_JSONL);
        let mut raw_writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&raw_path)?,
        );
        // Keyboard/mouse pre-roll is privacy-bounded by `ring`. Only gamepad
        // held state may be prefixed from before that window so a long-held
        // stick/button remains correct at video frame zero.
        for event in held_state
            .values()
            .filter(|event| is_gamepad_held_event(event, &devices))
            .chain(ring)
        {
            if devices.contains_key(&event.device_id) {
                write_json_line(&mut raw_writer, event)?;
            }
        }
        raw_writer.flush()?;

        if input_source.is_none() {
            log::warn!(
                "recording {recording_id} started with no admitted Steam virtual, physical HID, or evdev input; \
                 input will be blank unless an eligible device appears mid-session"
            );
        } else if input_source == Some(InputDeviceSource::PhysicalFallback) {
            log::warn!(
                "recording {recording_id} has only exact allowlisted physical input tracks at startup"
            );
        }

        let mut session = Self {
            recording_id,
            game_id,
            mode,
            directory,
            observed_boottime_us: current_boottime_us(),
            start_timestamp: unix_now_seconds(),
            video_anchor: None,
            video_anchor_conflict: false,
            clip_id: None,
            timeline_id: None,
            stopped: false,
            input_source,
            finalize_retry: FinalizeRetry::default(),
            devices,
            raw_writer,
        };
        snapshot_controller_layouts(config, &session.game_id, &session.directory)?;
        session.write_metadata()?;
        Ok(session)
    }

    fn resume(directory: &Path) -> Result<Self> {
        let metadata: OwnedCaptureMetadata =
            serde_json::from_reader(BufReader::new(File::open(directory.join("capture.json"))?))?;
        let devices = if directory.join(DEVICES_JSON).exists() {
            let devices: Vec<InputDeviceInfo> =
                serde_json::from_reader(BufReader::new(File::open(directory.join(DEVICES_JSON))?))?;
            devices
                .into_iter()
                .map(|device| (device.device_id.clone(), device))
                .collect()
        } else {
            BTreeMap::new()
        };
        let raw_writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join(RAW_EVENTS_JSONL))?,
        );
        let mode = parse_recording_mode(metadata.mode.as_deref(), &metadata.recording_id);
        let input_source = metadata
            .input_source
            .or_else(|| devices.values().next().map(|device| device.source));
        Ok(Self {
            recording_id: metadata.recording_id,
            game_id: metadata.game_id,
            mode,
            directory: directory.to_path_buf(),
            observed_boottime_us: metadata.observed_boottime_us,
            start_timestamp: metadata.start_timestamp,
            video_anchor: metadata.video_anchor,
            video_anchor_conflict: metadata.video_anchor_conflict,
            clip_id: metadata.clip_id,
            timeline_id: metadata.timeline_id,
            stopped: metadata.stopped,
            input_source,
            finalize_retry: FinalizeRetry::default(),
            devices,
            raw_writer,
        })
    }

    fn push_event(&mut self, event: &RawInputEvent) -> Result<()> {
        if !self.stopped && self.devices.contains_key(&event.device_id) {
            write_json_line(&mut self.raw_writer, event)?;
        }
        Ok(())
    }

    fn consider_device(
        &mut self,
        config: &RecorderConfig,
        device: &InputDeviceInfo,
    ) -> Result<bool> {
        if self.stopped {
            return Ok(false);
        }
        let eligible = match device.source {
            InputDeviceSource::SteamVirtual => true,
            InputDeviceSource::PhysicalFallback => config
                .physical_gamepad_priority(
                    device.vendor,
                    device.product,
                    &device.name,
                    device.unique.as_deref(),
                )
                .is_some(),
            InputDeviceSource::AllowlistedEvdev => config.device_admission(device).is_admitted(),
        };
        if !eligible {
            return Ok(false);
        }
        self.devices
            .insert(device.device_id.clone(), device.clone());
        self.input_source = preferred_input_source(config, &self.devices);
        self.write_metadata()?;
        Ok(true)
    }

    fn flush(&mut self) -> Result<()> {
        self.raw_writer
            .flush()
            .context("failed to flush input event log")
    }

    fn write_metadata(&mut self) -> Result<()> {
        self.flush()?;
        let metadata = CaptureMetadata {
            recording_id: &self.recording_id,
            game_id: &self.game_id,
            mode: recording_mode_label(self.mode),
            observed_boottime_us: self.observed_boottime_us,
            start_timestamp: self.start_timestamp,
            video_anchor: self.video_anchor,
            video_anchor_conflict: self.video_anchor_conflict,
            clip_id: &self.clip_id,
            timeline_id: &self.timeline_id,
            stopped: self.stopped,
            input_source: self.input_source,
            input_sources: captured_input_sources(&self.devices),
        };
        write_json_atomic(&self.directory.join("capture.json"), &metadata)?;
        write_json_atomic(
            &self.directory.join(DEVICES_JSON),
            &self.devices.values().collect::<Vec<_>>(),
        )
    }
}

fn finalize_session(config: &RecorderConfig, session: &mut ActiveSession) -> Result<PathBuf> {
    session.flush()?;
    session.write_metadata()?;
    if session.video_anchor_conflict {
        bail!("conflicting live video anchors; per-segment clock mapping is ambiguous");
    }
    let events = read_json_lines::<RawInputEvent>(&session.directory.join(RAW_EVENTS_JSONL))?;
    let devices = session.devices.values().cloned().collect::<Vec<_>>();

    let source_video = resolve_source_video(config, session)?;
    let (frame_pts_us, first_frame_boottime_us, status, clock, video_path) = if let Some(
        source_video,
    ) =
        source_video.as_ref()
    {
        let media = crate::media::validate(config, source_video)?;
        write_json_atomic(&session.directory.join("media-validation.json"), &media)?;
        let source_frame_pts_us = media.frame_pts_us;
        if source_frame_pts_us.is_empty() {
            bail!(
                "ffprobe returned no video frames for {}",
                source_video.display()
            );
        }
        let first_frame_pts_us = source_frame_pts_us[0];
        let frame_pts_us = source_frame_pts_us
            .iter()
            .map(|pts| pts.saturating_sub(first_frame_pts_us))
            .collect::<Vec<_>>();
        let (first_frame_boottime_us, status, clock) = if let Some((
            source_pts_us,
            normalized_pts_us,
        )) = session.video_anchor
        {
            let clip_path = source_video
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .context("missing clip root")?
                .join("clip.pb");
            let bytes =
                fs::read(&clip_path).context("clip metadata required to validate archive clock")?;
            let clip = crate::steam_clip::parse(
                &bytes,
                &session.game_id,
                session
                    .timeline_id
                    .as_deref()
                    .context("missing timeline identity")?,
            )?;
            let segment = clip
                .segments
                .iter()
                .find(|segment| segment.recording_id == session.recording_id)
                .context("exact segment missing from clip metadata")?;
            if media.declared_duration_us.is_none_or(|duration| {
                duration.abs_diff(segment.duration_ms.saturating_mul(1000)) > 1000
            }) {
                bail!("clip and DASH declared durations disagree");
            }
            let first = clip.first_frame_boottime_us(
                &session.recording_id,
                source_pts_us,
                first_frame_pts_us,
            )?;
            use sha2::{Digest, Sha256};
            write_json_atomic(
                &session.directory.join("archive-clock.json"),
                &serde_json::json!({
                    "schema_version": 1, "method": "whole_segment_zero_origin",
                    "recording_id": session.recording_id, "clip": clip,
                    "clip_sha256": format!("{:x}", Sha256::digest(&bytes)),
                    "live_source_pts_us": source_pts_us, "live_normalized_pts_us": normalized_pts_us,
                    "archive_first_pts_us": first_frame_pts_us, "first_frame_boottime_us": first,
                    "uncertainty_us": 1000,
                }),
            )?;
            let video_pts_zero_boottime_us = first;
            (
                first,
                ExternalCaptureStatus::Complete,
                Some(ExternalVideoClock {
                    video_pts_zero_boottime_us,
                    source_pts_us,
                    // This clock maps the archived source. Preserve the live
                    // normalized value separately in archive-clock.json.
                    normalized_pts_us: first_frame_pts_us,
                }),
            )
        } else {
            bail!("missing live source anchor; raw input retained without fabricating canonical alignment")
        };
        (
            frame_pts_us,
            first_frame_boottime_us,
            status,
            clock,
            Some(source_video.clone()),
        )
    } else if matches!(session.mode, RecordingMode::DesktopSession) {
        let end_boottime_us = events
            .iter()
            .map(|event| event.boottime_us)
            .max()
            .unwrap_or(session.observed_boottime_us)
            .max(current_boottime_us());
        let frame_pts_us =
            synthetic_frame_pts_us(session.observed_boottime_us, end_boottime_us, 60.0);
        (
            frame_pts_us,
            session.observed_boottime_us,
            ExternalCaptureStatus::MissingVideo,
            None,
            None,
        )
    } else {
        bail!("Steam did not report a clip ID or readable video source");
    };

    let frames = sample_input_frames(&frame_pts_us, first_frame_boottime_us, &events, &devices);
    write_json_atomic(&session.directory.join(INPUT_JSON), &frames)?;
    write_ron_atomic(&session.directory.join(INPUT_RON), &frames)?;
    // `input.json` is authoritative. The transition stream is a compact,
    // deterministic convenience index derived only after that log is final.
    write_json_lines_atomic(
        &session.directory.join(INPUT_TRANSITIONS_JSONL),
        &input_transitions_from_frames(&frames),
    )?;

    let fps = average_fps(&frame_pts_us);
    let game_name = steam_game_name(config, &session.game_id)
        .unwrap_or_else(|| format!("Steam game {}", session.game_id));
    let external = ExternalCaptureInfo {
        kind: ExternalCaptureKind::SteamGameRecording,
        game_id: session.game_id.clone(),
        recording_id: session.recording_id.clone(),
        clip_id: session.clip_id.clone(),
        timeline_id: session.timeline_id.clone(),
        source_video: video_path.as_ref().map(|path| path.display().to_string()),
        video_clock: clock,
        controller_map: session
            .directory
            .join(CONTROLLER_MAP_JSON)
            .exists()
            .then(|| CONTROLLER_MAP_JSON.to_string()),
        raw_event_log: Some(RAW_EVENTS_JSONL.into()),
        audio_transcript: None,
        audio_transcript_json: None,
        audio_transcript_source: None,
        audio_transcription_status: TranscriptionJobState::NotRequested,
        audio_transcription_provider: None,
        audio_transcription_model: None,
        status,
    };
    let manifest = SessionManifest {
        core: CoreInfo {
            name: game_name,
            version: "Steam Game Recording".into(),
            library_path: String::new(),
        },
        rom: None,
        timing: TimingInfo {
            fps,
            sample_rate: 48_000.0,
            start_timestamp: session.start_timestamp,
        },
        initial_state: None,
        frame_count: frames.len() as u64,
        pause_segments: Vec::new(),
        input_log: INPUT_JSON.into(),
        // The portable artifact is materialized by `export-video`/the pull
        // script. Steam's live DASH source remains in `external_capture`.
        video: video_path.as_ref().map(|_| "video.mkv".into()),
        mic_audio: None,
        transcript: None,
        transcript_json: None,
        transcription_status: TranscriptionJobState::NotRequested,
        transcription_provider: None,
        transcription_model: None,
        binding_map: None,
        capture_provenance: Some(CaptureProvenance {
            kind: CaptureSourceKind::SteamGameRecording,
            game_audio_policy: None,
            source_description: Some("Steam Game Recording DASH remux".into()),
        }),
        video_timing: Some(VideoTiming {
            kind: VideoTimingKind::ExternalVariableFrameRate,
            output_fps: None,
            source_clock: Some("CLOCK_BOOTTIME".into()),
        }),
        frame_map: None,
        input_transitions: Some(INPUT_TRANSITIONS_JSONL.into()),
        track_alignment: Some(TrackAlignmentMetadata {
            video: TrackAlignment {
                presence: if video_path.is_some() {
                    TrackPresence::Present
                } else {
                    TrackPresence::Unavailable
                },
                status: match status {
                    ExternalCaptureStatus::Complete => TrackAlignmentStatus::Complete,
                    ExternalCaptureStatus::DegradedAlignment => TrackAlignmentStatus::Degraded,
                    ExternalCaptureStatus::Partial | ExternalCaptureStatus::MissingVideo => {
                        TrackAlignmentStatus::Unavailable
                    }
                },
                offset_us: Some(0),
                uncertainty_us: clock.map(|_| 1000),
                clock_source: Some("CLOCK_BOOTTIME / Steam Game Recording PTS".into()),
            },
            narration: TrackAlignment {
                presence: TrackPresence::NotCaptured,
                status: TrackAlignmentStatus::NotCaptured,
                offset_us: None,
                uncertainty_us: None,
                clock_source: None,
            },
            game_audio: TrackAlignment {
                // Steam exposes mixed audio, not a separately measured game stem.
                // Presence is measured; its alignment remains unverified.
                presence: if session.directory.join("media-validation.json").is_file()
                    && read_media_audio_presence(&session.directory)?
                {
                    TrackPresence::Present
                } else {
                    TrackPresence::Unavailable
                },
                status: TrackAlignmentStatus::Unavailable,
                offset_us: None,
                uncertainty_us: None,
                clock_source: None,
            },
        }),
        external_capture: Some(external),
        dropped_frames: 0,
        format: ManifestFormat::Json,
    };
    write_json_atomic(&session.directory.join(MANIFEST_JSON), &manifest)?;
    let mut ron_manifest = manifest;
    ron_manifest.format = ManifestFormat::Ron;
    write_ron_atomic(&session.directory.join(MANIFEST_RON), &ron_manifest)?;

    let final_directory = unique_session_directory(config, &session.recording_id);
    fs::rename(&session.directory, &final_directory).with_context(|| {
        format!(
            "failed to move {} to {}",
            session.directory.display(),
            final_directory.display()
        )
    })?;
    Ok(final_directory)
}

fn resolve_source_video(
    config: &RecorderConfig,
    session: &ActiveSession,
) -> Result<Option<PathBuf>> {
    if let Some(clip_id) = session.clip_id.as_deref() {
        match wait_for_clip_video(config, clip_id, &session.recording_id) {
            Ok(path) => return Ok(Some(path)),
            Err(error) if matches!(session.mode, RecordingMode::DesktopSession) => {
                log::warn!(
                    "clip video unavailable for desktop session {}: {error:#}",
                    session.recording_id
                );
            }
            Err(error) => return Err(error),
        }
    }
    if matches!(session.mode, RecordingMode::DesktopSession) {
        return wait_for_desktop_video(config, &session.game_id, &session.recording_id);
    }
    bail!("Steam did not report a clip ID");
}

fn wait_for_desktop_video(
    config: &RecorderConfig,
    game_id: &str,
    recording_id: &str,
) -> Result<Option<PathBuf>> {
    let deadline = SystemTime::now() + Duration::from_secs(config.clip_timeout_seconds.min(15));
    loop {
        if let Some(path) = find_desktop_video(config, game_id, recording_id)? {
            return Ok(Some(path));
        }
        if SystemTime::now() >= deadline {
            log::warn!(
                "no Steam video found for desktop session {recording_id}; finalizing input-only"
            );
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn find_desktop_video(
    config: &RecorderConfig,
    game_id: &str,
    recording_id: &str,
) -> Result<Option<PathBuf>> {
    let userdata = config.steam_root.join("userdata");
    if !userdata.exists() {
        return Ok(None);
    }
    let _ = game_id;
    let mut matches = Vec::new();
    for user in fs::read_dir(userdata)?.filter_map(Result::ok) {
        collect_exact_media(
            &user.path().join("gamerecordings"),
            recording_id,
            7,
            &mut matches,
        )?;
    }
    matches.sort();
    matches.dedup();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => bail!("multiple video sources match recording {recording_id}; refusing to guess"),
    }
}

fn collect_exact_media(
    root: &Path,
    recording_id: &str,
    depth: usize,
    matches: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth == 0 || !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == recording_id && path.join("session.mpd").is_file() {
            matches.push(path.join("session.mpd"));
        } else {
            collect_exact_media(&path, recording_id, depth - 1, matches)?;
        }
    }
    Ok(())
}

fn synthetic_frame_pts_us(start_boottime_us: u64, end_boottime_us: u64, fps: f64) -> Vec<u64> {
    let fps = if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        60.0
    };
    let step = (1_000_000.0 / fps).round().max(1.0) as u64;
    let duration = end_boottime_us.saturating_sub(start_boottime_us).max(step);
    let frames = (duration / step).saturating_add(1).max(1);
    (0..frames)
        .map(|index| index.saturating_mul(step))
        .collect()
}

fn snapshot_controller_layouts(
    config: &RecorderConfig,
    game_id: &str,
    session_dir: &Path,
) -> Result<()> {
    let log_path = steam_controller_log(config);
    if !log_path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&log_path)?;
    let layout_paths = controller_layout_paths(config, game_id, &text);
    if layout_paths.is_empty() {
        return Ok(());
    }

    let output_dir = session_dir.join("controller-layouts");
    fs::create_dir_all(&output_dir)?;
    let mut controller_map = ControllerMap {
        game_id: game_id.into(),
        layouts: Vec::new(),
    };
    for (controller, source) in layout_paths {
        if !source.exists() {
            log::warn!("Steam controller layout is missing: {}", source.display());
            continue;
        }
        let file_name = format!(
            "controller-{controller}-{}",
            source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("layout.vdf")
        );
        let destination = output_dir.join(&file_name);
        fs::copy(&source, &destination)?;
        let bytes = fs::read(&source)?;
        match vdf::controller_layout_map(
            &String::from_utf8_lossy(&bytes),
            controller,
            format!("controller-layouts/{file_name}"),
        ) {
            Ok(layout) => controller_map.layouts.push(layout),
            Err(error) => log::warn!(
                "could not normalize controller layout {}: {error}",
                source.display()
            ),
        }
    }
    if !controller_map.layouts.is_empty() {
        write_json_atomic(&session_dir.join(CONTROLLER_MAP_JSON), &controller_map)?;
    }
    Ok(())
}

fn controller_layout_paths(
    config: &RecorderConfig,
    game_id: &str,
    controller_log: &str,
) -> BTreeMap<u32, PathBuf> {
    let mut layout_paths = BTreeMap::<u32, PathBuf>::new();
    for app_id in steam_input_app_ids(game_id) {
        let app_marker = format!("App ID {app_id}, Controller ");
        for line in controller_log.lines() {
            let Some(rest) = line.split(&app_marker).nth(1) else {
                continue;
            };
            let Some((controller, path)) = rest.split_once(':') else {
                continue;
            };
            let Ok(controller) = controller.trim().parse() else {
                continue;
            };
            let path = PathBuf::from(path.trim());
            let path = if path.is_absolute() {
                path
            } else {
                config.steam_root.join(path)
            };
            layout_paths.insert(controller, path);
        }
    }
    layout_paths
}

fn steam_input_app_ids(game_id: &str) -> Vec<String> {
    let mut app_ids = vec![game_id.to_string()];
    let Ok(game_id) = game_id.parse::<u64>() else {
        return app_ids;
    };
    // Steam Game Recording identifies shortcuts with a 64-bit game ID:
    // `(shortcut_app_id << 32) | 0x02000000`. Steam Input's controller logs
    // use the original unsigned 32-bit shortcut app ID instead.
    if game_id as u32 == 0x0200_0000 {
        let shortcut_app_id = (game_id >> 32) as u32;
        if shortcut_app_id & 0x8000_0000 != 0 {
            app_ids.push(shortcut_app_id.to_string());
        }
    }
    app_ids
}

fn wait_for_clip_video(
    config: &RecorderConfig,
    clip_id: &str,
    recording_id: &str,
) -> Result<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(config.clip_timeout_seconds);
    loop {
        if let Some(path) = find_clip_video(config, clip_id, recording_id)? {
            return Ok(path);
        }
        if Instant::now() >= deadline {
            bail!(
                "Steam clip {clip_id} did not become readable within {} seconds",
                config.clip_timeout_seconds
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn find_clip_video(
    config: &RecorderConfig,
    clip_id: &str,
    recording_id: &str,
) -> Result<Option<PathBuf>> {
    let userdata = config.steam_root.join("userdata");
    if !userdata.exists() {
        return Ok(None);
    }
    let mut found = None;
    for user in fs::read_dir(userdata)?.filter_map(Result::ok) {
        let clip = user
            .path()
            .join("gamerecordings/clips")
            .join(clip_id)
            .join("video");
        let exact = clip.join(recording_id).join("session.mpd");
        if exact.is_file() {
            if found.is_some() {
                bail!("multiple Steam users contain this exact clip/recording; refusing to guess");
            }
            found = Some(exact);
        }
    }
    Ok(found)
}

fn read_media_audio_presence(directory: &Path) -> Result<bool> {
    let value: serde_json::Value =
        serde_json::from_reader(File::open(directory.join("media-validation.json"))?)?;
    Ok(value["has_audio"].as_bool().unwrap_or(false))
}

#[cfg(test)]
fn parse_seconds_us(value: &str) -> Option<u64> {
    let seconds = value.trim().parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1_000_000.0).round() as u64)
}

fn average_fps(frame_pts_us: &[u64]) -> f64 {
    let Some(duration) = frame_pts_us
        .last()
        .copied()
        .filter(|duration| *duration > 0)
    else {
        return 0.0;
    };
    (frame_pts_us.len().saturating_sub(1) as f64) * 1_000_000.0 / duration as f64
}

fn steam_game_name(config: &RecorderConfig, game_id: &str) -> Option<String> {
    let path = config
        .steam_root
        .join("steamapps")
        .join(format!("appmanifest_{game_id}.acf"));
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let fields = line
            .split('"')
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<_>>();
        if fields.first().is_some_and(|key| key.trim() == "name") {
            return fields.get(1).map(|value| value.trim().to_string());
        }
    }
    None
}

fn unique_session_directory(config: &RecorderConfig, id: &str) -> PathBuf {
    let base = config.recordings_dir.join(safe_id(id));
    if !base.exists() {
        return base;
    }
    for suffix in 2.. {
        let candidate = config
            .recordings_dir
            .join(format!("{}-{suffix}", safe_id(id)));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn steam_streaming_log(config: &RecorderConfig) -> PathBuf {
    [
        "streaming_log.txt",
        "gamerecording_log.txt",
        "game_recording_log.txt",
    ]
    .into_iter()
    .map(|name| config.steam_root.join("logs").join(name))
    .find(|path| path.exists())
    .unwrap_or_else(|| config.steam_root.join("logs/gamerecording_log.txt"))
}

fn steam_controller_log(config: &RecorderConfig) -> PathBuf {
    config.steam_root.join("logs/controller_ui.txt")
}

fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(target_os = "linux")]
fn current_boottime_us() -> u64 {
    crate::linux::boottime_us()
}

#[cfg(not(target_os = "linux"))]
fn current_boottime_us() -> u64 {
    0
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("file")
    ));
    {
        let mut writer = BufWriter::new(File::create(&temporary)?);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

fn write_json_lines_atomic(path: &Path, values: &[impl Serialize]) -> Result<()> {
    let temporary = path.with_extension("jsonl.tmp");
    {
        let mut writer = BufWriter::new(File::create(&temporary)?);
        for value in values {
            write_json_line(&mut writer, value)?;
        }
        writer.flush()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

fn write_ron_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::new())?;
    let temporary = path.with_extension("ron.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn read_json_lines<T>(path: &Path) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let reader = BufReader::new(File::open(path)?);
    reader
        .lines()
        .filter(|line| line.as_ref().is_ok_and(|line| !line.trim().is_empty()))
        .map(|line| {
            let line = line?;
            serde_json::from_str(&line).map_err(anyhow::Error::from)
        })
        .collect()
}

struct LogTail {
    path: PathBuf,
    file: File,
    offset: u64,
    pending: String,
    identity: Option<(u64, u64)>,
}

impl LogTail {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open Steam log {}", path.display()))?;
        let metadata = file.metadata()?;
        let offset = file.seek(SeekFrom::End(0))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            offset,
            pending: String::new(),
            identity: file_identity(&metadata),
        })
    }

    fn poll_lines(&mut self) -> Result<Vec<String>> {
        let metadata = fs::metadata(&self.path)?;
        let identity = file_identity(&metadata);
        if metadata.len() < self.offset || identity != self.identity {
            self.file = File::open(&self.path)?;
            self.offset = 0;
            self.pending.clear();
            self.identity = file_identity(&self.file.metadata()?);
        }
        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.pending.push_str(&String::from_utf8_lossy(&bytes));

        let mut lines = Vec::new();
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending[..newline].trim_end_matches('\r').to_string();
            self.pending.drain(..=newline);
            lines.push(line);
        }
        Ok(lines)
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_device(id: &str, source: crate::model::InputDeviceSource) -> InputDeviceInfo {
        let is_steam_virtual = source == crate::model::InputDeviceSource::SteamVirtual;
        InputDeviceInfo {
            device_id: id.into(),
            event_path: format!("/dev/input/{id}").into(),
            name: id.into(),
            bus_type: 3,
            vendor: 0,
            product: 0,
            version: 1,
            unique: None,
            physical_path: Some(format!("test/{id}")),
            is_virtual: is_steam_virtual,
            is_steam_virtual,
            port: Some(0),
            source,
            is_gamepad: true,
            is_keyboard: false,
            is_mouse: false,
            admitted_capabilities: Some(crate::model::InputCapabilities {
                gamepad: true,
                keyboard: false,
                mouse: false,
            }),
            admission_reason: Some("test".into()),
            abs_ranges: BTreeMap::new(),
        }
    }

    fn physical_match(name: &str) -> crate::config::PhysicalGamepadMatch {
        crate::config::PhysicalGamepadMatch {
            vendor: 0,
            product: 0,
            name_contains: Some(name.into()),
            unique_contains: None,
            allow_virtual: false,
        }
    }

    #[test]
    fn steam_recording_lifecycle_is_the_only_session_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().join("steam"),
            recordings_dir: temporary.path().join("recordings"),
            ..RecorderConfig::default()
        };
        let device = source_device(
            "steam-28de-11ff-pad-0",
            crate::model::InputDeviceSource::SteamVirtual,
        );
        let devices = [(device.device_id.clone(), device)].into_iter().collect();
        let ring = VecDeque::new();
        let held = BTreeMap::new();
        let mut active = None;

        for event in [
            SteamLogEvent::VideoAnchor {
                source_pts_us: 10,
                normalized_pts_us: 0,
            },
            SteamLogEvent::RecordingStopped {
                recording_id: String::new(),
            },
            SteamLogEvent::ClipSaved {
                clip_id: "clip-before-opt-in".into(),
                timeline_id: None,
            },
            SteamLogEvent::RecordingStarted {
                recording_id: "bg_646570_test".into(),
                game_id: "646570".into(),
                mode: RecordingMode::Background,
            },
        ] {
            handle_steam_event(event, &config, &devices, &ring, &held, &mut active).unwrap();
            assert!(active.is_none());
        }

        let recording_id = "fg_646570_test";
        handle_steam_event(
            SteamLogEvent::RecordingStarted {
                recording_id: recording_id.into(),
                game_id: "646570".into(),
                mode: RecordingMode::OnDemand,
            },
            &config,
            &devices,
            &ring,
            &held,
            &mut active,
        )
        .unwrap();
        assert!(active.as_ref().is_some_and(|session| !session.stopped));

        let event = RawInputEvent {
            boottime_us: 1,
            device_id: "steam-28de-11ff-pad-0".into(),
            event_type: 1,
            code: 304,
            value: 1,
            name: Some("BTN_SOUTH".into()),
            lifecycle: None,
        };
        active.as_mut().unwrap().push_event(&event).unwrap();
        handle_steam_event(
            SteamLogEvent::RecordingStopped {
                recording_id: recording_id.into(),
            },
            &config,
            &devices,
            &ring,
            &held,
            &mut active,
        )
        .unwrap();
        let session = active.as_mut().unwrap();
        assert!(session.stopped);
        session
            .push_event(&RawInputEvent { value: 0, ..event })
            .unwrap();
        session.flush().unwrap();
        let events =
            read_json_lines::<RawInputEvent>(&session.directory.join(RAW_EVENTS_JSONL)).unwrap();
        assert_eq!(events.len(), 1, "post-stop input must not be appended");
    }

    #[test]
    fn session_records_virtual_and_physical_inputs_together() {
        use crate::model::InputDeviceSource::{PhysicalFallback, SteamVirtual};

        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().join("steam"),
            recordings_dir: temporary.path().join("recordings"),
            physical_gamepad_allowlist: vec![physical_match("physical")],
            ..RecorderConfig::default()
        };
        let devices = [
            (
                "physical".into(),
                source_device("physical", PhysicalFallback),
            ),
            ("virtual".into(), source_device("virtual", SteamVirtual)),
        ]
        .into_iter()
        .collect();

        let session = ActiveSession::start(
            &config,
            "fg_646570_test".into(),
            "646570".into(),
            RecordingMode::OnDemand,
            &devices,
            &VecDeque::new(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(session.input_source, Some(SteamVirtual));
        assert_eq!(
            session
                .devices
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["physical", "virtual"]
        );
        let metadata: serde_json::Value =
            serde_json::from_reader(File::open(session.directory.join("capture.json")).unwrap())
                .unwrap();
        assert_eq!(
            metadata["input_sources"],
            serde_json::json!(["steam_virtual", "physical_fallback"])
        );
    }

    #[test]
    fn allowlisted_physical_is_compatibility_primary_when_it_is_the_only_source() {
        use crate::model::InputDeviceSource::PhysicalFallback;

        let config = RecorderConfig {
            physical_gamepad_allowlist: vec![physical_match("physical")],
            ..RecorderConfig::default()
        };
        let devices = [(
            "physical".into(),
            source_device("physical", PhysicalFallback),
        )]
        .into_iter()
        .collect();

        assert_eq!(
            preferred_input_source(&config, &devices),
            Some(PhysicalFallback)
        );
    }

    #[test]
    fn empty_session_adopts_allowlisted_physical_input_for_any_game() {
        use crate::model::InputDeviceSource::PhysicalFallback;

        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().join("steam"),
            recordings_dir: temporary.path().join("recordings"),
            physical_gamepad_allowlist: vec![physical_match("physical")],
            ..RecorderConfig::default()
        };
        let mut session = ActiveSession::start(
            &config,
            "fg_646570_test".into(),
            "646570".into(),
            RecordingMode::OnDemand,
            &BTreeMap::new(),
            &VecDeque::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let physical = source_device("physical", PhysicalFallback);

        assert!(session.consider_device(&config, &physical).unwrap());
        assert_eq!(session.input_source, Some(PhysicalFallback));
        assert!(session.devices.contains_key("physical"));
    }

    #[test]
    fn session_adds_late_eligible_devices_without_dropping_existing_tracks() {
        use crate::model::InputDeviceSource::{PhysicalFallback, SteamVirtual};

        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().join("steam"),
            recordings_dir: temporary.path().join("recordings"),
            physical_gamepad_allowlist: vec![physical_match("physical")],
            ..RecorderConfig::default()
        };
        let mut session = ActiveSession::start(
            &config,
            "fg_646570_test".into(),
            "646570".into(),
            RecordingMode::OnDemand,
            &BTreeMap::new(),
            &VecDeque::new(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(session
            .consider_device(&config, &source_device("physical", PhysicalFallback))
            .unwrap());
        assert!(session
            .consider_device(&config, &source_device("virtual", SteamVirtual))
            .unwrap());
        assert_eq!(session.input_source, Some(SteamVirtual));
        assert_eq!(
            session
                .devices
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["physical", "virtual"]
        );
    }

    #[test]
    fn session_records_all_exact_allowlisted_physical_controllers() {
        use crate::config::PhysicalGamepadMatch;
        use crate::model::InputDeviceSource::PhysicalFallback;

        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().join("steam"),
            recordings_dir: temporary.path().join("recordings"),
            physical_gamepad_allowlist: vec![
                PhysicalGamepadMatch {
                    vendor: 0x28de,
                    product: 0x1205,
                    name_contains: Some("Steam Deck Controller".into()),
                    unique_contains: None,
                    allow_virtual: false,
                },
                PhysicalGamepadMatch {
                    vendor: 0x054c,
                    product: 0x0ce6,
                    name_contains: Some("DualSense".into()),
                    unique_contains: None,
                    allow_virtual: false,
                },
            ],
            ..RecorderConfig::default()
        };
        let mut deck = source_device("z-deck", PhysicalFallback);
        deck.vendor = 0x28de;
        deck.product = 0x1205;
        deck.name = "Valve Software Steam Deck Controller".into();
        let mut dualsense = source_device("a-dualsense", PhysicalFallback);
        dualsense.vendor = 0x054c;
        dualsense.product = 0x0ce6;
        dualsense.name = "Sony DualSense Wireless Controller".into();
        let devices = [
            (dualsense.device_id.clone(), dualsense),
            (deck.device_id.clone(), deck),
        ]
        .into_iter()
        .collect();

        let session = ActiveSession::start(
            &config,
            "fg_646570_test".into(),
            "646570".into(),
            RecordingMode::OnDemand,
            &devices,
            &VecDeque::new(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(session.input_source, Some(PhysicalFallback));
        assert_eq!(
            session
                .devices
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a-dualsense", "z-deck"]
        );
    }

    #[test]
    fn non_allowlisted_physical_input_is_ineligible() {
        use crate::model::InputDeviceSource::PhysicalFallback;

        let config = RecorderConfig::default();
        let devices = [(
            "physical".into(),
            source_device("physical", PhysicalFallback),
        )]
        .into_iter()
        .collect();

        assert_eq!(preferred_input_source(&config, &devices), None);
    }

    #[test]
    fn finalize_retry_is_delayed_and_bounded() {
        let config = RecorderConfig {
            finalize_retry_interval_seconds: 2,
            finalize_retry_attempts: 2,
            ..RecorderConfig::default()
        };
        let start = std::time::Instant::now();
        let mut retry = FinalizeRetry::default();

        retry.mark_attempt_failed(&config, start);
        assert!(!retry.is_due(start));
        assert!(retry.is_due(start + Duration::from_secs(2)));

        retry.mark_attempt_failed(&config, start + Duration::from_secs(2));
        assert!(!retry.is_due(start + Duration::from_secs(30)));
        assert!(retry.exhausted(&config));
    }

    #[cfg(unix)]
    fn restart_finalization_fixture() -> (tempfile::TempDir, RecorderConfig, ActiveSession) {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().join("steam"),
            recordings_dir: temporary.path().join("recordings"),
            ffprobe: temporary.path().join("ffprobe"),
            clip_timeout_seconds: 1,
            ..RecorderConfig::default()
        };
        fs::write(&config.ffprobe, "#!/bin/sh\nif [ -f \"$0.warning\" ]; then cat \"$0.warning\" >&2; fi\ncat \"$0.json\"\n").unwrap();
        fs::set_permissions(&config.ffprobe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(config.ffprobe.with_extension("json"), r#"{"frames":[{"pts_time":"0.0","nb_samples":1024},{"pts_time":"0.016667","nb_samples":1024},{"pts_time":"0.033333","duration_time":"0.016667","nb_samples":1024}],"packets":[{"pts_time":"0.0"},{"pts_time":"0.016667"},{"pts_time":"0.033333"}],"streams":[{"codec_type":"video"},{"codec_type":"audio"}]}"#).unwrap();
        let recording_id = "fg_9223372058363166720_20260912_141509";
        let clip_id = "clip_9223372058363166720_20260912_141532";
        let clip = config
            .steam_root
            .join("userdata/1/gamerecordings/clips")
            .join(clip_id);
        let video = clip.join("video").join(recording_id);
        fs::create_dir_all(&video).unwrap();
        fs::write(video.join("session.mpd"), r#"<MPD type="static" mediaPresentationDuration="PT0.05S"><Period><AdaptationSet contentType="video"><Representation id="0"><SegmentTemplate timescale="1000" duration="3000" startNumber="1" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s"/></Representation></AdaptationSet><AdaptationSet contentType="audio"><Representation id="1"><SegmentTemplate timescale="1000" duration="3000" startNumber="1" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s"/></Representation></AdaptationSet></Period></MPD>"#).unwrap();
        for name in [
            "init-stream0.m4s",
            "init-stream1.m4s",
            "chunk-stream0-00001.m4s",
            "chunk-stream1-00001.m4s",
        ] {
            fs::write(video.join(name), "fixture").unwrap();
        }
        fn var(mut value: u64) -> Vec<u8> {
            let mut bytes = Vec::new();
            while value >= 128 {
                bytes.push((value as u8 & 127) | 128);
                value >>= 7;
            }
            bytes.push(value as u8);
            bytes
        }
        fn num(key: u64, value: u64) -> Vec<u8> {
            [var(key << 3), var(value)].concat()
        }
        fn bytes(key: u64, value: &[u8]) -> Vec<u8> {
            [var(key << 3 | 2), var(value.len() as u64), value.to_vec()].concat()
        }
        let record = [
            bytes(1, recording_id.as_bytes()),
            num(2, 2499),
            num(3, 50),
            num(10, 6190),
        ]
        .concat();
        let timeline = [
            bytes(1, b"timeline_922337205836316672020260912_141503"),
            num(2, 9223372058363166720),
            bytes(5, &record),
        ]
        .concat();
        fs::write(
            clip.join("clip.pb"),
            [
                bytes(1, &timeline),
                num(2, 3691),
                num(4, 9223372058363166720),
            ]
            .concat(),
        )
        .unwrap();
        let devices = [(
            "virtual".into(),
            source_device("virtual", InputDeviceSource::SteamVirtual),
        )]
        .into_iter()
        .collect();
        let mut session = ActiveSession::start(
            &config,
            recording_id.into(),
            "9223372058363166720".into(),
            RecordingMode::OnDemand,
            &devices,
            &VecDeque::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        session.video_anchor = Some((54_962_746_483, 2_311_681));
        session.clip_id = Some(clip_id.into());
        session.timeline_id = Some("timeline_922337205836316672020260912_141503".into());
        session
            .push_event(&RawInputEvent {
                boottime_us: 54_962_762_483,
                device_id: "virtual".into(),
                event_type: 1,
                code: 304,
                value: 1,
                name: Some("BTN_SOUTH".into()),
                lifecycle: None,
            })
            .unwrap();
        session.stopped = true;
        (temporary, config, session)
    }

    #[cfg(unix)]
    #[test]
    fn finalization_reconstructs_button_edge_after_validated_archive_rebase() {
        let (_temporary, config, mut session) = restart_finalization_fixture();
        let directory = finalize_session(&config, &mut session).unwrap();
        let frames: Vec<retrofeel_types::InputFrame> =
            serde_json::from_reader(File::open(directory.join(INPUT_JSON)).unwrap()).unwrap();
        assert_eq!(frames.len(), 3);
        assert!(frames[0]
            .raw_host
            .as_ref()
            .unwrap()
            .gamepad_buttons
            .is_empty());
        assert_eq!(
            frames[1].raw_host.as_ref().unwrap().gamepad_buttons,
            vec!["South"]
        );
        let manifest: SessionManifest =
            serde_json::from_reader(File::open(directory.join(MANIFEST_JSON)).unwrap()).unwrap();
        assert_eq!(manifest.frame_count, 3);
        assert_eq!(
            manifest
                .external_capture
                .unwrap()
                .video_clock
                .unwrap()
                .video_pts_zero_boottime_us,
            54_962_746_483
        );
        assert_eq!(
            manifest.track_alignment.unwrap().game_audio.presence,
            TrackPresence::Present
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_probe_exit_with_fragment_warning_retains_partial_raw_session() {
        let (_temporary, config, mut session) = restart_finalization_fixture();
        fs::write(
            config.ffprobe.with_extension("warning"),
            "Failed to open fragment of playlist\n",
        )
        .unwrap();
        let directory = session.directory.clone();
        let mut active = None;
        session.flush().unwrap();
        let original = fs::read(directory.join(RAW_EVENTS_JSONL)).unwrap();
        try_finalize(&config, session, &mut active).unwrap();
        assert!(active.is_some());
        assert_eq!(
            fs::read(directory.join(RAW_EVENTS_JSONL)).unwrap(),
            original
        );
        assert!(!directory.join(INPUT_JSON).exists());
        assert!(!directory.join(MANIFEST_JSON).exists());
        assert!(
            fs::read_to_string(directory.join("finalization-health.json"))
                .unwrap()
                .contains("fragment")
        );
    }

    #[cfg(unix)]
    #[test]
    fn conflicting_live_anchor_cannot_rewrite_the_archive_clock() {
        let (_temporary, config, session) = restart_finalization_fixture();
        let original_anchor = session.video_anchor;
        let mut active = Some(session);
        handle_steam_event(
            SteamLogEvent::VideoAnchor {
                source_pts_us: 123,
                normalized_pts_us: 0,
            },
            &config,
            &BTreeMap::new(),
            &VecDeque::new(),
            &BTreeMap::new(),
            &mut active,
        )
        .unwrap();
        let mut session = active.unwrap();
        assert!(session.video_anchor_conflict);
        assert_eq!(session.video_anchor, original_anchor);
        assert!(finalize_session(&config, &mut session)
            .unwrap_err()
            .to_string()
            .contains("conflicting"));
        assert!(!session.directory.join(INPUT_JSON).exists());
    }

    #[cfg(unix)]
    #[test]
    fn clip_marker_hands_off_stopped_session_without_decoding_on_ingress() {
        let (_temporary, config, session) = restart_finalization_fixture();
        let directory = session.directory.clone();
        let mut active = Some(session);
        handle_steam_event(
            SteamLogEvent::ClipSaved {
                clip_id: "clip_9223372058363166720_20260912_141532".into(),
                timeline_id: Some("timeline_922337205836316672020260912_141503".into()),
            },
            &config,
            &BTreeMap::new(),
            &VecDeque::new(),
            &BTreeMap::new(),
            &mut active,
        )
        .unwrap();
        let pending = active.unwrap();
        assert!(pending.stopped);
        assert_eq!(pending.finalize_retry.attempts, 0);
        assert!(!directory.join(INPUT_JSON).exists());
    }

    #[cfg(unix)]
    #[test]
    fn repair_rebuilds_into_new_root_and_preserves_original_manifest() {
        let (temporary, config, mut session) = restart_finalization_fixture();
        let original = finalize_session(&config, &mut session).unwrap();
        let original_manifest = original.join(MANIFEST_JSON);
        let mut manifest: serde_json::Value =
            serde_json::from_reader(File::open(&original_manifest).unwrap()).unwrap();
        manifest["frame_count"] = 999.into();
        write_json_atomic(&original_manifest, &manifest).unwrap();
        let bytes = fs::read(&original_manifest).unwrap();
        let out = temporary.path().join("repair-v1");
        let repaired = derive_session(&config, &session.recording_id, &out).unwrap();
        let repaired_manifest: SessionManifest =
            serde_json::from_reader(File::open(repaired.join(MANIFEST_JSON)).unwrap()).unwrap();
        assert_eq!(repaired_manifest.frame_count, 3);
        assert_eq!(fs::read(&original_manifest).unwrap(), bytes);
        assert!(repaired.join("repair-provenance.json").is_file());
        assert!(derive_session(&config, &session.recording_id, &out).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn repair_recovers_stopped_partial_without_modifying_original() {
        let (temporary, config, mut session) = restart_finalization_fixture();
        session.flush().unwrap();
        session.write_metadata().unwrap();
        let original = session.directory.clone();
        assert!(original
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(".partial-"));
        let snapshot = fs::read_dir(&original)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (path.clone(), fs::read(path).unwrap())
            })
            .collect::<Vec<_>>();
        let out = temporary.path().join("partial-repair-v1");
        let repaired = derive_session(&config, &session.recording_id, &out).unwrap();
        let manifest: SessionManifest =
            serde_json::from_reader(File::open(repaired.join(MANIFEST_JSON)).unwrap()).unwrap();
        assert_eq!(manifest.frame_count, 3);
        assert!(!original.join(MANIFEST_JSON).exists());
        for (path, bytes) in snapshot {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        let provenance: serde_json::Value =
            serde_json::from_reader(File::open(repaired.join("repair-provenance.json")).unwrap())
                .unwrap();
        assert_eq!(provenance["source_directory"], original.to_str().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn missing_clip_clock_metadata_never_emits_canonical_input() {
        let (_temporary, config, mut session) = restart_finalization_fixture();
        let source = resolve_source_video(&config, &session).unwrap().unwrap();
        fs::remove_file(
            source
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("clip.pb"),
        )
        .unwrap();
        assert!(finalize_session(&config, &mut session).is_err());
        assert!(!session.directory.join(INPUT_JSON).exists());
    }

    #[test]
    fn exact_restart_segment_must_arrive_before_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let config = RecorderConfig {
            steam_root: temporary.path().into(),
            clip_timeout_seconds: 0,
            ..RecorderConfig::default()
        };
        let video = temporary
            .path()
            .join("userdata/1/gamerecordings/clips/clip_42_now/video");
        let preceding = video.join("fg_42_now/session.mpd");
        fs::create_dir_all(preceding.parent().unwrap()).unwrap();
        fs::write(&preceding, "earlier segment").unwrap();
        assert!(find_clip_video(&config, "clip_42_now", "fg_42_now_0")
            .unwrap()
            .is_none());
        assert!(wait_for_clip_video(&config, "clip_42_now", "fg_42_now_0").is_err());
        let intended = video.join("fg_42_now_0/session.mpd");
        fs::create_dir_all(intended.parent().unwrap()).unwrap();
        fs::write(&intended, "intended segment").unwrap();
        assert_eq!(
            find_clip_video(&config, "clip_42_now", "fg_42_now_0").unwrap(),
            Some(intended)
        );
    }

    #[test]
    fn ffprobe_parser_handles_csv_suffix_and_normalizes_origin() {
        let values = ["12.500000,", "12.516683,", "12.550000,"]
            .into_iter()
            .filter_map(|line| {
                line.split(',')
                    .find(|value| !value.trim().is_empty())
                    .and_then(parse_seconds_us)
            })
            .collect::<Vec<_>>();
        let first = values[0];
        let normalized = values.iter().map(|value| value - first).collect::<Vec<_>>();
        assert_eq!(normalized, vec![0, 16_683, 50_000]);
    }

    #[test]
    fn log_tail_starts_at_end_and_reads_appended_complete_lines() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("steam.log");
        fs::write(&path, "old line\n").unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        fs::write(&path, "old line\nnew line\npartial").unwrap();
        assert_eq!(tail.poll_lines().unwrap(), vec!["new line"]);
        fs::write(&path, "old line\nnew line\npartial line\n").unwrap();
        assert_eq!(tail.poll_lines().unwrap(), vec!["partial line"]);
    }

    #[cfg(unix)]
    #[test]
    fn log_tail_reopens_a_rotated_file() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("steam.log");
        fs::write(&path, "old line\n").unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        fs::rename(&path, temporary.path().join("steam.log.old")).unwrap();
        fs::write(&path, "new file\n").unwrap();
        assert_eq!(tail.poll_lines().unwrap(), vec!["new file"]);
    }

    #[test]
    fn session_ids_are_safe_as_directory_names() {
        assert_eq!(safe_id("fg_42/now"), "fg_42_now");
    }

    #[test]
    fn synthetic_frame_clock_covers_session_span() {
        let pts = synthetic_frame_pts_us(1_000_000, 1_050_000, 60.0);
        assert!(pts.len() >= 3);
        assert_eq!(pts[0], 0);
        assert_eq!(pts[1], 16_667);
    }

    #[test]
    fn recording_mode_round_trips_from_metadata_labels() {
        assert_eq!(
            parse_recording_mode(Some("desktop_session"), "x"),
            RecordingMode::DesktopSession
        );
        assert_eq!(
            parse_recording_mode(None, "desktop_42_1"),
            RecordingMode::DesktopSession
        );
        assert_eq!(
            parse_recording_mode(None, "fg_1_2"),
            RecordingMode::OnDemand
        );
    }

    #[test]
    fn non_steam_game_id_resolves_shortcut_controller_log_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let steam_root = temporary.path().join("steam");
        let templates = steam_root.join("controller_base/templates");
        let session = temporary.path().join("session");
        fs::create_dir_all(steam_root.join("logs")).unwrap();
        fs::create_dir_all(&templates).unwrap();
        fs::create_dir_all(&session).unwrap();
        let config = RecorderConfig {
            steam_root: steam_root.clone(),
            ..RecorderConfig::default()
        };
        let game_id = "9223372041183297536";
        assert_eq!(
            steam_input_app_ids(game_id),
            vec![game_id.to_string(), "2147483649".to_string()]
        );

        let layout = |controller_type: &str| {
            format!(
                r#""controller_mappings"
{{
    "title" "Gamepad"
    "controller_type" "{controller_type}"
    "group" {{ "id" "0" "mode" "four_buttons" "inputs" {{ "button_a" {{ "binding" "xinput_button A" }} }} }}
    "preset" {{ "group_source_bindings" {{ "0" "button_diamond active" }} }}
}}"#
            )
        };
        fs::write(
            templates.join("controller_neptune_gamepad_fps.vdf"),
            layout("controller_neptune"),
        )
        .unwrap();
        fs::write(
            templates.join("controller_ps5_gamepad_joystick.vdf"),
            layout("controller_ps5"),
        )
        .unwrap();
        fs::write(
            steam_root.join("logs/controller_ui.txt"),
            "[time] Loaded Config for Last Resort Path for App ID 2147483649, Controller 15: controller_base/templates/controller_neptune_gamepad_fps.vdf\n\
             [time] Loaded Config for Last Resort Path for App ID 2147483649, Controller 0: controller_base/templates/controller_ps5_gamepad_joystick.vdf\n",
        )
        .unwrap();

        snapshot_controller_layouts(&config, game_id, &session).unwrap();

        let map: ControllerMap =
            serde_json::from_reader(File::open(session.join(CONTROLLER_MAP_JSON)).unwrap())
                .unwrap();
        assert_eq!(map.game_id, game_id);
        assert_eq!(map.layouts.len(), 2);
        assert!(map.layouts.iter().all(|layout| layout.bindings.len() == 1));
        assert!(session
            .join("controller-layouts/controller-15-controller_neptune_gamepad_fps.vdf")
            .is_file());
        assert!(session
            .join("controller-layouts/controller-0-controller_ps5_gamepad_joystick.vdf")
            .is_file());
    }

    #[test]
    fn native_steam_game_id_is_unchanged_for_controller_logs() {
        assert_eq!(steam_input_app_ids("2436570"), vec!["2436570"]);
    }

    /// Reproduces the ClockworkValley reconfiguration path. Removal drops the indefinite
    /// held cache while the bounded ring retains an explicit lifecycle marker;
    /// `ActiveSession::start` filters the ring by the devices present then.
    #[test]
    fn device_removal_clears_held_state_and_marks_the_bounded_ring() {
        use crate::model::{AbsRange, InputDeviceInfo, RawInputEvent};

        let device_id = "steam-28de-11ff-pad-0";
        let make_event = |boottime: u64, code: u16, value: i32| RawInputEvent {
            boottime_us: boottime,
            device_id: device_id.to_string(),
            event_type: 3,
            code,
            value,
            name: None,
            lifecycle: None,
        };

        let mut devices = BTreeMap::<String, InputDeviceInfo>::new();
        let mut ring = VecDeque::<RawInputEvent>::new();
        let mut held_state = BTreeMap::<(String, u16, u16), RawInputEvent>::new();

        let device = InputDeviceInfo {
            event_path: "/dev/input/event18".into(),
            vendor: 0x28de,
            product: 0x11ff,
            abs_ranges: [(
                0u16,
                AbsRange {
                    minimum: -32768,
                    maximum: 32767,
                    flat: 128,
                },
            )]
            .into_iter()
            .collect(),
            ..source_device(device_id, crate::model::InputDeviceSource::SteamVirtual)
        };
        devices.insert(device_id.to_string(), device.clone());

        let axis_sync = make_event(1_000_000, 0, 0);
        held_state.insert((device_id.to_string(), 3, 0), axis_sync.clone());
        ring.push_back(axis_sync);

        assert_eq!(ring.len(), 1);
        assert_eq!(held_state.len(), 1);

        devices.remove(device_id);
        held_state.retain(|(id, _, _), _| id != device_id);
        push_ring_event(
            &mut ring,
            RawInputEvent::lifecycle(1_000_001, device_id.into(), InputDeviceLifecycle::Removed),
            5_000_000,
        );

        assert!(devices.is_empty());
        assert!(held_state.is_empty());
        assert_eq!(ring.len(), 2);
        assert_eq!(
            ring.back().unwrap().lifecycle,
            Some(InputDeviceLifecycle::Removed)
        );
    }

    #[test]
    fn keyboard_and_mouse_held_state_never_escape_the_pre_roll_bound() {
        use crate::model::{InputCapabilities, InputDeviceSource};

        let mut device = source_device("evdev-keyboard-mouse", InputDeviceSource::AllowlistedEvdev);
        device.name = "Allowed keyboard and mouse".into();
        device.port = None;
        device.is_gamepad = false;
        device.is_keyboard = true;
        device.is_mouse = true;
        device.admitted_capabilities = Some(InputCapabilities {
            gamepad: false,
            keyboard: true,
            mouse: true,
        });
        let mut devices = BTreeMap::new();
        devices.insert(device.device_id.clone(), device);
        let keyboard = RawInputEvent {
            boottime_us: 1,
            device_id: "evdev-keyboard-mouse".into(),
            event_type: 1,
            code: 30,
            value: 1,
            name: Some("KEY_A".into()),
            lifecycle: None,
        };
        let mouse_button = RawInputEvent {
            code: 272,
            name: Some("BTN_LEFT".into()),
            ..keyboard.clone()
        };
        assert!(!is_gamepad_held_event(&keyboard, &devices));
        assert!(!is_gamepad_held_event(&mouse_button, &devices));
    }

    #[test]
    fn pre_roll_expires_against_boottime_without_new_input() {
        let mut ring = VecDeque::from([
            RawInputEvent {
                boottime_us: 1_000_000,
                device_id: "keyboard".into(),
                event_type: 1,
                code: 30,
                value: 1,
                name: Some("KEY_A".into()),
                lifecycle: None,
            },
            RawInputEvent {
                boottime_us: 6_500_000,
                device_id: "mouse".into(),
                event_type: 2,
                code: 0,
                value: 4,
                name: Some("REL_X".into()),
                lifecycle: None,
            },
        ]);

        prune_ring_events(&mut ring, 7_000_000, 1_000_000);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring[0].device_id, "mouse");
    }

    /// A re-added device (same device_id, new event path) must be accepted into
    /// the active session's device map so mid-session hotplug is captured.
    #[test]
    fn re_added_device_enters_active_session_map() {
        use crate::model::{AbsRange, InputDeviceInfo};

        let device_id = "steam-28de-11ff-pad-0";
        let mut active_devices: BTreeMap<String, InputDeviceInfo> = BTreeMap::new();

        let recreated = InputDeviceInfo {
            event_path: "/dev/input/event21".into(),
            vendor: 0x28de,
            product: 0x11ff,
            abs_ranges: [(
                0u16,
                AbsRange {
                    minimum: -32768,
                    maximum: 32767,
                    flat: 128,
                },
            )]
            .into_iter()
            .collect(),
            ..source_device(device_id, crate::model::InputDeviceSource::SteamVirtual)
        };

        active_devices.insert(recreated.device_id.clone(), recreated.clone());
        assert!(active_devices.contains_key(device_id));
        assert_eq!(
            active_devices[device_id].event_path,
            std::path::Path::new("/dev/input/event21")
        );
    }
}
