use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Receiver, Sender};
use retrofeel_types::VideoFrame as Frame;

use crate::mic::{MicClockObservation, MicRecorder};
use retrofeel_types::{
    input_transitions_from_frames, CaptureProvenance, CoreInfo, FrameMapEntry, FrameMapInfo,
    InputBindingSet, InputFrame, InputState, ManifestFormat, PauseSegment, RawHostInput, RomInfo,
    SessionManifest, TimingInfo, TrackAlignment, TrackAlignmentMetadata, TrackAlignmentStatus,
    TrackPresence, TranscriptDocument, TranscriptSegment, TranscriptionConfig,
    TranscriptionJobState, VideoTiming,
};
use sha1::{Digest, Sha1};

const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
use thiserror::Error;

/// Limit queued 1080p RGBA payloads to roughly 64 MiB. Writer messages remain
/// ordered on an unbounded channel, but once this budget is exhausted they
/// carry compact duplicate markers instead of multi-megabyte pixel buffers.
/// This prevents encoder lag from turning into several GiB of resident memory
/// and, critically, never blocks the ScreenCaptureKit consumer.
const RECORDING_FRAME_PAYLOAD_CAPACITY: usize = 8;
const FFMPEG_STDERR_CAPTURE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("ffmpeg was not found; install it with MacPorts or Homebrew before recording")]
    FfmpegMissing,
    #[error("failed to create recording directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to spawn ffmpeg: {0}")]
    SpawnFfmpeg(std::io::Error),
}

#[derive(Debug, Clone)]
pub struct RecordingStart {
    pub recordings_dir: PathBuf,
    pub core_name: String,
    pub core_version: String,
    pub core_path: PathBuf,
    pub rom_path: Option<PathBuf>,
    pub rom_sha1: Option<String>,
    pub rom_size: Option<u64>,
    pub fps: f64,
    pub sample_rate: f64,
    pub width: u32,
    pub height: u32,
    pub binding_map: Option<InputBindingSet>,
    /// Optional path to a save-state file captured at launch. If present, the
    /// writer copies it into the session dir and records its path in the
    /// manifest's `initial_state` so stretch-phase S2 replay has a known
    /// starting point. None = no initial state recorded.
    pub initial_state_path: Option<PathBuf>,
    /// Capture microphone audio to `mic.wav` in the session dir. Best-effort:
    /// device/permission failures log a warning and the session records
    /// without a mic track.
    pub capture_mic: bool,
    /// Optional real-time mic peak shared with the local status overlay. This
    /// does not alter the persisted WAV; it visualizes the same samples that
    /// are written to it.
    pub live_mic_peak: Option<Arc<AtomicU32>>,
    pub transcription: TranscriptionConfig,
    /// Optional source-aware metadata. Core recordings leave these as `None`;
    /// the Steam ScreenCaptureKit path supplies its CFR contract.
    pub capture_provenance: Option<CaptureProvenance>,
    pub video_timing: Option<VideoTiming>,
    pub track_alignment: Option<TrackAlignmentMetadata>,
}

pub struct RecordingHandle {
    sender: Sender<WriterMessage>,
    queued_frame_payloads: Arc<AtomicUsize>,
    payload_overflows: AtomicU64,
    join: Option<thread::JoinHandle<()>>,
    /// Mic capture runs on its own thread (cpal input streams are `!Send`).
    /// Stopped and finalized *before* the writer gets `Stop`, so the writer
    /// sees a complete `mic.wav` when it builds the manifest and starts
    /// background transcription.
    mic: Option<MicRecorder>,
    /// First SCK presentation PTS rebased to mach host time by the CFR
    /// scheduler. It is retained until mic finalization so narration
    /// alignment is based on actual capture clocks rather than wall time.
    video_clock_anchor_us: Mutex<Option<u64>>,
    pub session_dir: PathBuf,
}

impl RecordingHandle {
    pub fn start(config: RecordingStart) -> Result<Self, RecordingError> {
        if !ffmpeg_available() {
            return Err(RecordingError::FfmpegMissing);
        }

        let session_dir = next_session_dir(&config.recordings_dir);
        std::fs::create_dir_all(&session_dir).map_err(|source| RecordingError::CreateDir {
            path: session_dir.clone(),
            source,
        })?;
        let mic = config
            .capture_mic
            .then(|| MicRecorder::start(session_dir.join("mic.wav"), config.live_mic_peak.clone()));

        let (sender, receiver) = unbounded();
        let queued_frame_payloads = Arc::new(AtomicUsize::new(0));
        let writer_queued_frame_payloads = queued_frame_payloads.clone();
        let thread_dir = session_dir.clone();
        let join = thread::Builder::new()
            .name("retrofeel-recorder".into())
            .spawn(move || {
                if let Err(error) =
                    writer_thread(config, thread_dir, receiver, writer_queued_frame_payloads)
                {
                    log::error!("recording writer failed: {error}");
                }
            })
            .map_err(RecordingError::SpawnFfmpeg)?;

        Ok(Self {
            sender,
            queued_frame_payloads,
            payload_overflows: AtomicU64::new(0),
            join: Some(join),
            mic,
            video_clock_anchor_us: Mutex::new(None),
            session_dir,
        })
    }

    pub fn frame(
        &self,
        frame_index: u64,
        input: InputState,
        raw_host: Option<RawHostInput>,
        frame: Option<Frame>,
        audio: Vec<i16>,
    ) {
        self.frame_timed(
            frame_index,
            None,
            None,
            input,
            raw_host,
            frame.map(Arc::new),
            audio,
        );
    }

    /// Queue one already-scheduled encoded video tick. External captures use
    /// this rather than treating a variable-rate source callback as a video
    /// frame: the elapsed timestamp and source map stay with the tick even
    /// when the writer must duplicate its previous RGBA buffer.
    #[allow(clippy::too_many_arguments)]
    pub fn frame_timed(
        &self,
        frame_index: u64,
        elapsed_us: Option<u64>,
        frame_map: Option<FrameMapEntry>,
        input: InputState,
        raw_host: Option<RawHostInput>,
        frame: Option<Arc<Frame>>,
        audio: Vec<i16>,
    ) {
        // The 1:1 input↔video invariant is the product's core guarantee. Keep
        // every scheduled tick and input sample in FIFO order, but bound the
        // expensive RGBA payloads independently. Encoder lag therefore
        // degrades video to explicit dupes without ever stalling capture.
        let reserved_payload = frame.is_some()
            && try_reserve_frame_payload(
                &self.queued_frame_payloads,
                RECORDING_FRAME_PAYLOAD_CAPACITY,
            );
        if frame.is_some() && !reserved_payload {
            let overflows = self.payload_overflows.fetch_add(1, Ordering::Relaxed) + 1;
            if overflows == 1 || overflows.is_multiple_of(60) {
                log::warn!(
                    "recording encoder behind; duping frame {frame_index} ({} payload overflows)",
                    overflows
                );
            }
            let _ = self.sender.send(WriterMessage::FrameDupe {
                frame_index,
                elapsed_us,
                frame_map,
                input,
                raw_host: raw_host.map(Box::new),
            });
            return;
        }

        let result = self
            .sender
            .send(WriterMessage::Frame(Box::new(RecordingFrame {
                frame_index,
                elapsed_us,
                frame_map,
                input,
                raw_host,
                frame,
                audio,
                reserved_payload,
            })));
        if result.is_err() && reserved_payload {
            self.queued_frame_payloads.fetch_sub(1, Ordering::Release);
        }
    }

    fn set_narration_alignment(&self, alignment: TrackAlignment) {
        let _ = self
            .sender
            .send(WriterMessage::NarrationAlignment(alignment));
    }

    pub fn set_video_clock_anchor(&self, source_mach_us: u64) {
        if let Ok(mut anchor) = self.video_clock_anchor_us.lock() {
            anchor.get_or_insert(source_mach_us);
        }
    }

    /// Supply capture-side quality counters after a source-aware scheduler has
    /// stopped. The writer still owns the path and map entries, while the
    /// scheduler owns knowledge of source frames that never reached a tick.
    pub fn set_frame_map_telemetry(
        &self,
        source_frames_received: u64,
        source_frames_discarded: u64,
        grid_duplicates: u64,
    ) {
        let _ = self.sender.send(WriterMessage::FrameMapTelemetry {
            source_frames_received,
            source_frames_discarded,
            grid_duplicates,
        });
    }

    pub fn pause(&self, frame_index: u64) {
        let _ = self.sender.try_send(WriterMessage::Pause { frame_index });
        if let Some(mic) = self.mic.as_ref() {
            mic.set_paused(true);
        }
    }

    pub fn resume(&self, frame_index: u64) {
        let _ = self.sender.try_send(WriterMessage::Resume { frame_index });
        if let Some(mic) = self.mic.as_ref() {
            mic.set_paused(false);
        }
    }

    pub fn stop(mut self) {
        self.finish();
    }

    /// Idempotent shutdown shared by `stop` and `Drop`: finalize the mic WAV
    /// first so the writer sees a complete `mic.wav` when it handles `Stop`.
    fn finish(&mut self) {
        if let Some(mic) = self.mic.take() {
            match mic.stop() {
                Some(outcome) => {
                    log::info!("mic capture finished: {}", outcome.path.display());
                    let source_pts_us = self
                        .video_clock_anchor_us
                        .lock()
                        .ok()
                        .and_then(|anchor| *anchor);
                    self.set_narration_alignment(narration_alignment(source_pts_us, outcome.clock));
                }
                None => {
                    log::info!("recording has no mic track");
                    self.set_narration_alignment(TrackAlignment {
                        presence: TrackPresence::Unavailable,
                        status: TrackAlignmentStatus::Unavailable,
                        offset_us: None,
                        uncertainty_us: None,
                        clock_source: Some("CoreAudio capture timestamp / mach host time".into()),
                    });
                }
            }
        }
        let _ = self.sender.send(WriterMessage::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub(crate) fn find_media_tool(name: &str) -> Option<PathBuf> {
    let fallback_dirs = [
        Path::new("/opt/local/bin"),
        Path::new("/opt/homebrew/bin"),
        Path::new("/usr/local/bin"),
        Path::new("/usr/bin"),
    ];
    find_media_tool_with_path(name, std::env::var_os("PATH").as_deref(), &fallback_dirs)
}

fn find_media_tool_with_path(
    name: &str,
    path: Option<&std::ffi::OsStr>,
    fallback_dirs: &[&Path],
) -> Option<PathBuf> {
    path.into_iter()
        .flat_map(|paths| std::env::split_paths(paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .chain(fallback_dirs.iter().map(|dir| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

pub fn ffmpeg_available() -> bool {
    find_media_tool("ffmpeg")
        .and_then(|ffmpeg| Command::new(ffmpeg).arg("-version").output().ok())
        .is_some_and(|output| output.status.success())
}

impl Drop for RecordingHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
pub fn rom_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

/// Hash the source content independently of the buffer handed to libretro.
/// Full-path cores intentionally receive an empty data buffer, but recording
/// identity must still reflect the real file.
pub fn rom_identity(path: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut digest = Sha1::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok((format!("{:x}", digest.finalize()), size))
}

enum WriterMessage {
    Frame(Box<RecordingFrame>),
    /// Sent when the queue was full for a frame; carries the input so the
    /// 1:1 invariant holds, and the writer writes the previous frame as a dupe.
    FrameDupe {
        frame_index: u64,
        elapsed_us: Option<u64>,
        frame_map: Option<FrameMapEntry>,
        input: InputState,
        raw_host: Option<Box<RawHostInput>>,
    },
    Pause {
        frame_index: u64,
    },
    Resume {
        frame_index: u64,
    },
    NarrationAlignment(TrackAlignment),
    FrameMapTelemetry {
        source_frames_received: u64,
        source_frames_discarded: u64,
        grid_duplicates: u64,
    },
    Stop,
}

struct RecordingFrame {
    frame_index: u64,
    elapsed_us: Option<u64>,
    frame_map: Option<FrameMapEntry>,
    input: InputState,
    raw_host: Option<RawHostInput>,
    frame: Option<Arc<Frame>>,
    audio: Vec<i16>,
    reserved_payload: bool,
}

fn try_reserve_frame_payload(queued: &AtomicUsize, capacity: usize) -> bool {
    let mut current = queued.load(Ordering::Acquire);
    loop {
        if current >= capacity {
            return false;
        }
        match queued.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn writer_thread(
    config: RecordingStart,
    session_dir: PathBuf,
    receiver: Receiver<WriterMessage>,
    queued_frame_payloads: Arc<AtomicUsize>,
) -> anyhow::Result<()> {
    let raw_video_path = session_dir.join("video-no-audio.mkv");
    let audio_path = session_dir.join("audio.wav");
    let video_path = session_dir.join("video.mkv");
    let input_json_path = session_dir.join("input.json");
    let input_ron_path = session_dir.join("input.ron");
    let input_transitions_path = session_dir.join("input-transitions.jsonl");
    let frame_map_path = session_dir.join("frame-map.json");
    let initial_state_session_path = session_dir.join("initial-state.bin");

    let mut ffmpeg = spawn_video_ffmpeg(&raw_video_path, &config)?;
    let stderr_drain = ffmpeg.stderr.take().map(spawn_ffmpeg_stderr_drain);
    let mut stdin = ffmpeg
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin was not piped"))?;

    // Stream the WAV incrementally (item 12) instead of buffering the whole
    // session in memory. ~21 MB/min at 48 kHz stereo was the prior worst case.
    let mut wav_writer = hound::WavWriter::create(
        &audio_path,
        hound::WavSpec {
            channels: 2,
            sample_rate: config.sample_rate as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;

    let mut input_frames = Vec::new();
    let mut frame_map_entries = Vec::new();
    let mut last_rgba = vec![0u8; config.width as usize * config.height as usize * 4];
    let mut encoded_frames = 0u64;
    let mut dropped_frames = 0u64;
    let mut pause_segments = Vec::new();
    let mut open_pause: Option<u64> = None;
    let mut track_alignment = config.track_alignment.clone();
    let mut frame_map_telemetry: Option<(u64, u64, u64)> = None;

    // Copy the initial save state into the session dir if one was provided
    // (item 16) so stretch-phase S2 replay has a known starting point.
    let initial_state = match &config.initial_state_path {
        Some(src) if src.exists() => match std::fs::copy(src, &initial_state_session_path) {
            Ok(_) => Some(initial_state_session_path.display().to_string()),
            Err(e) => {
                log::warn!("failed to copy initial state into session: {e}");
                None
            }
        },
        _ => None,
    };

    let write_result = (|| -> anyhow::Result<()> {
        while let Ok(message) = receiver.recv() {
            match message {
                WriterMessage::Frame(recorded) => {
                    if recorded.reserved_payload {
                        queued_frame_payloads.fetch_sub(1, Ordering::Release);
                    }
                    write_frame(
                        &mut stdin,
                        &mut last_rgba,
                        recorded.frame.as_deref(),
                        &config,
                    )?;
                    input_frames.push(InputFrame {
                        frame: recorded.frame_index,
                        elapsed_us: recorded.elapsed_us,
                        port: 0,
                        state: recorded.input,
                        raw_host: recorded.raw_host,
                    });
                    if let Some(frame_map) = recorded.frame_map {
                        frame_map_entries.push(frame_map);
                    }
                    // Stream audio straight into the WAV writer.
                    for chunk in recorded.audio.chunks_exact(2) {
                        wav_writer.write_sample(chunk[0])?;
                        wav_writer.write_sample(chunk[1])?;
                    }
                    if recorded.audio.len() % 2 == 1 {
                        // Odd tail: write a single sample (mono fallback).
                        wav_writer.write_sample(*recorded.audio.last().unwrap())?;
                    }
                    encoded_frames += 1;
                }
                WriterMessage::FrameDupe {
                    frame_index,
                    elapsed_us,
                    frame_map,
                    input,
                    raw_host,
                } => {
                    // Queue was full on the producer side: write the previous
                    // frame as a dupe so video frame count stays 1:1 with the input
                    // log, and still record the input (the whole point of the
                    // product). No audio for duped frames.
                    stdin.write_all(&last_rgba)?;
                    input_frames.push(InputFrame {
                        frame: frame_index,
                        elapsed_us,
                        port: 0,
                        state: input,
                        raw_host: raw_host.map(|raw| *raw),
                    });
                    if let Some(mut frame_map) = frame_map {
                        frame_map.writer_dupe = true;
                        frame_map_entries.push(frame_map);
                    }
                    encoded_frames += 1;
                    dropped_frames += 1;
                }
                WriterMessage::Pause { frame_index } => {
                    if open_pause.is_none() {
                        open_pause = Some(frame_index);
                    }
                }
                WriterMessage::Resume { frame_index } => {
                    if let Some(start_frame) = open_pause.take() {
                        pause_segments.push(PauseSegment {
                            start_frame,
                            end_frame: frame_index,
                        });
                    }
                }
                WriterMessage::NarrationAlignment(narration) => {
                    if let Some(alignment) = track_alignment.as_mut() {
                        alignment.narration = narration;
                    }
                }
                WriterMessage::FrameMapTelemetry {
                    source_frames_received,
                    source_frames_discarded,
                    grid_duplicates,
                } => {
                    frame_map_telemetry = Some((
                        source_frames_received,
                        source_frames_discarded,
                        grid_duplicates,
                    ));
                }
                WriterMessage::Stop => break,
            }
        }
        Ok(())
    })();

    if let Some(start_frame) = open_pause.take() {
        pause_segments.push(PauseSegment {
            start_frame,
            end_frame: encoded_frames,
        });
    }

    drop(stdin);
    let ffmpeg_result = wait_for_ffmpeg(&mut ffmpeg);
    log_ffmpeg_stderr(stderr_drain);
    let write_error = write_result.err();
    let wav_result = wav_writer.finalize();

    // Persist authoritative input and source timing even if the encoder was
    // interrupted. The previous all-or-nothing ordering discarded every
    // input sample when ffmpeg returned a non-zero status during shutdown.
    serde_json::to_writer_pretty(std::fs::File::create(&input_json_path)?, &input_frames)?;
    std::fs::write(
        &input_ron_path,
        ron::ser::to_string_pretty(&input_frames, ron::ser::PrettyConfig::default())?,
    )?;
    // This convenience log is generated from the finalized input JSON rather
    // than the in-memory producer state. That makes it reproducible and keeps
    // `input.json` as the only authority for analysis.
    write_input_transitions_from_log(&input_json_path, &input_transitions_path)?;

    let frame_map = if frame_map_entries.is_empty() {
        None
    } else {
        serde_json::to_writer_pretty(std::fs::File::create(&frame_map_path)?, &frame_map_entries)?;
        let source_frames_received = frame_map_entries
            .iter()
            .map(|entry| entry.source_frame.saturating_add(1))
            .max()
            .unwrap_or(0);
        let (source_frames_received, source_frames_discarded, grid_duplicates) =
            frame_map_telemetry.unwrap_or((
                source_frames_received,
                frame_map_entries
                    .iter()
                    .map(|entry| entry.discarded_source_frames_before)
                    .sum(),
                frame_map_entries
                    .iter()
                    .filter(|entry| entry.grid_duplicate)
                    .count() as u64,
            ));
        Some(FrameMapInfo {
            path: frame_map_path.display().to_string(),
            source_frames_received,
            source_frames_discarded,
            grid_duplicates,
        })
    };

    if let Some(error) = write_error {
        return Err(error.context(format!(
            "video writer stopped after preserving {encoded_frames} input frames"
        )));
    }
    wav_result?;

    let encoder_degraded = validate_encoder_result(ffmpeg_result, &raw_video_path, encoded_frames)?;
    if encoder_degraded || dropped_frames > 0 {
        if let Some(alignment) = track_alignment.as_mut() {
            alignment.video.status = TrackAlignmentStatus::Degraded;
        }
    }

    let manifest_video = if raw_video_path.exists() && audio_path.exists() {
        // Skip muxing if the audio WAV is essentially empty (no game audio in
        // Steam sessions, or a silent core session). A 44-byte WAV has just
        // the header and no samples — muxing it with ffmpeg -shortest produces
        // a broken file.
        let audio_size = std::fs::metadata(&audio_path).map(|m| m.len()).unwrap_or(0);
        if audio_size > 44 && mux_audio(&raw_video_path, &audio_path, &video_path) {
            Some(video_path.display().to_string())
        } else {
            promote_unmuxed_master(&raw_video_path, &video_path)
        }
    } else if raw_video_path.exists() {
        promote_unmuxed_master(&raw_video_path, &video_path)
    } else {
        None
    };

    // The mic WAV was finalized before `Stop` reached this thread (see
    // `RecordingHandle::finish`), so existence means a complete track.
    let mic_wav_path = session_dir.join("mic.wav");
    let mic_audio =
        (config.capture_mic && mic_wav_path.exists()).then(|| mic_wav_path.display().to_string());
    let mic_for_transcription = mic_audio.as_ref().map(|_| mic_wav_path);
    let transcription = config.transcription.clone();
    let should_transcribe = mic_audio.is_some() && transcription.automatic;

    let manifest = SessionManifest {
        core: CoreInfo {
            name: config.core_name,
            version: config.core_version,
            library_path: config.core_path.display().to_string(),
        },
        rom: match (config.rom_path, config.rom_sha1, config.rom_size) {
            (Some(path), Some(sha1), Some(size)) => Some(RomInfo {
                path: path.display().to_string(),
                sha1,
                size,
            }),
            _ => None,
        },
        timing: TimingInfo {
            fps: config.fps,
            sample_rate: config.sample_rate,
            start_timestamp: now_epoch_secs(),
        },
        initial_state,
        frame_count: encoded_frames,
        pause_segments,
        input_log: input_json_path.display().to_string(),
        video: manifest_video,
        mic_audio,
        transcript: None,
        transcript_json: None,
        transcription_status: if should_transcribe {
            TranscriptionJobState::Queued
        } else {
            TranscriptionJobState::NotRequested
        },
        transcription_provider: should_transcribe.then_some(transcription.provider),
        transcription_model: transcription.selected_model_id.clone(),
        binding_map: config.binding_map,
        capture_provenance: config.capture_provenance,
        video_timing: config.video_timing,
        frame_map,
        input_transitions: Some(input_transitions_path.display().to_string()),
        track_alignment,
        external_capture: None,
        dropped_frames,
        format: ManifestFormat::Json,
    };

    let _postprocess = finalize_manifest_and_start_transcription(
        &session_dir,
        manifest,
        mic_for_transcription,
        transcription,
        transcribe_mic,
    )?;

    log::info!(
        "recording complete: {} frames ({} dropped) in {}",
        encoded_frames,
        dropped_frames,
        session_dir.display()
    );
    Ok(())
}

fn narration_alignment(video_mach_us: Option<u64>, mic: MicClockObservation) -> TrackAlignment {
    let Some(video_mach_us) = video_mach_us else {
        return TrackAlignment {
            presence: TrackPresence::Unavailable,
            status: TrackAlignmentStatus::Unavailable,
            offset_us: None,
            uncertainty_us: None,
            clock_source: Some("CoreAudio capture timestamp / mach host time".into()),
        };
    };
    let Some(first_capture_mach_us) = mic.first_capture_mach_us else {
        return TrackAlignment {
            presence: TrackPresence::Unavailable,
            status: TrackAlignmentStatus::Unavailable,
            offset_us: None,
            uncertainty_us: None,
            clock_source: Some("CoreAudio capture timestamp / mach host time".into()),
        };
    };
    let offset = i128::from(first_capture_mach_us) - i128::from(video_mach_us);
    let offset_us = offset.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    TrackAlignment {
        presence: TrackPresence::Present,
        status: if mic.uncertainty_us <= 100_000 {
            TrackAlignmentStatus::Complete
        } else {
            TrackAlignmentStatus::Degraded
        },
        offset_us: Some(offset_us),
        uncertainty_us: Some(mic.uncertainty_us),
        clock_source: Some("CoreAudio capture timestamp / mach host time".into()),
    }
}

/// Persist both manifest formats before starting best-effort transcription.
/// The returned join handle is deliberately not joined by production callers:
/// recording shutdown only waits for media and manifest finalization, while
/// transcription updates the manifests later from its own thread.
fn finalize_manifest_and_start_transcription<F>(
    session_dir: &Path,
    manifest: SessionManifest,
    mic_wav_path: Option<PathBuf>,
    transcription: TranscriptionConfig,
    transcribe: F,
) -> anyhow::Result<Option<thread::JoinHandle<()>>>
where
    F: FnOnce(&Path, &Path, &TranscriptionConfig) -> Result<TranscriptArtifacts, String>
        + Send
        + 'static,
{
    write_session_manifests(session_dir, &manifest)?;
    if session_dir
        .extension()
        .is_some_and(|extension| extension == "feel")
    {
        if let Err(error) = retrofeel_feel::initialize_native_package(session_dir, None) {
            log::warn!("failed to initialize native .feel package: {error}");
        }
    }

    let Some(mic_wav_path) = mic_wav_path else {
        return Ok(None);
    };
    if !transcription.automatic {
        return Ok(None);
    }
    let thread_dir = session_dir.to_path_buf();
    let handle = match thread::Builder::new()
        .name("retrofeel-transcriber".into())
        .spawn(move || {
            let mut updated = manifest;
            updated.transcription_status = TranscriptionJobState::Running {
                progress_percent: 1,
            };
            if let Err(error) = write_session_manifests(&thread_dir, &updated) {
                log::warn!("failed to mark transcription running: {error}");
            }
            match transcribe(&thread_dir, &mic_wav_path, &transcription) {
                Ok(artifacts) => {
                    updated.transcript = Some(artifacts.srt.display().to_string());
                    updated.transcript_json = Some(artifacts.json.display().to_string());
                    updated.transcription_status = TranscriptionJobState::Complete;
                }
                Err(message) => {
                    log::warn!("recording transcription failed: {message}");
                    updated.transcription_status = TranscriptionJobState::Failed { message };
                }
            }
            if let Err(error) = write_session_manifests(&thread_dir, &updated) {
                log::warn!("failed to update recording manifests with transcript: {error}");
            }
            if thread_dir
                .extension()
                .is_some_and(|extension| extension == "feel")
            {
                if let Err(error) = retrofeel_feel::initialize_native_package(&thread_dir, None) {
                    log::warn!("failed to refresh native .feel transcript inventory: {error}");
                }
            }
        }) {
        Ok(handle) => handle,
        Err(error) => {
            log::warn!("failed to start recording transcription thread: {error}");
            return Ok(None);
        }
    };
    Ok(Some(handle))
}

/// Atomically replace each manifest so readers never observe a partially
/// serialized file while background transcription adds its artifact path.
fn write_session_manifests(session_dir: &Path, manifest: &SessionManifest) -> anyhow::Result<()> {
    let mut json_manifest = manifest.clone();
    json_manifest.format = ManifestFormat::Json;
    atomic_write(&session_dir.join("manifest.json"), |file| {
        serde_json::to_writer_pretty(file, &json_manifest)?;
        Ok(())
    })?;

    let mut ron_manifest = manifest.clone();
    ron_manifest.format = ManifestFormat::Ron;
    let ron = ron::ser::to_string_pretty(&ron_manifest, ron::ser::PrettyConfig::default())?;
    atomic_write(&session_dir.join("manifest.ron"), |file| {
        file.write_all(ron.as_bytes())?;
        Ok(())
    })
}

fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("manifest path has no parent: {}", path.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    write(temp.as_file_mut())?;
    temp.as_file_mut().flush()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn spawn_video_ffmpeg(path: &PathBuf, config: &RecordingStart) -> anyhow::Result<Child> {
    let ffmpeg = find_media_tool("ffmpeg")
        .ok_or_else(|| anyhow::anyhow!("ffmpeg executable was not found"))?;
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .args(["-hide_banner", "-loglevel", "warning", "-nostats"])
        .args(["-f", "rawvideo"])
        .args(["-pix_fmt", "rgba"])
        .arg("-s")
        .arg(format!("{}x{}", config.width, config.height))
        .arg("-r")
        .arg(format!("{}", config.fps))
        .args(["-i", "pipe:0"])
        .arg("-an");
    if cfg!(target_os = "macos") && ffmpeg_has_encoder("h264_videotoolbox") {
        command
            .args(["-c:v", "h264_videotoolbox"])
            .args(["-realtime", "1"])
            .args(["-prio_speed", "1"])
            .args(["-allow_sw", "1"])
            .args(["-b:v", "16M"])
            .args(["-pix_fmt", "yuv420p"]);
    } else {
        command
            .args(["-c:v", "libx264"])
            .args(["-preset", "veryfast"])
            .args(["-crf", "18"])
            .args(["-pix_fmt", "yuv420p"]);
    }
    command
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    isolate_terminal_signals(&mut command);
    command.spawn().map_err(Into::into)
}

fn ffmpeg_has_encoder(name: &str) -> bool {
    let Some(ffmpeg) = find_media_tool("ffmpeg") else {
        return false;
    };
    Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-h"])
        .arg(format!("encoder={name}"))
        .output()
        .is_ok_and(|output| output.status.success())
}

fn spawn_ffmpeg_stderr_drain(
    mut stderr: impl Read + Send + 'static,
) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stderr.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            let remaining = FFMPEG_STDERR_CAPTURE_LIMIT.saturating_sub(captured.len());
            captured.extend_from_slice(&chunk[..read.min(remaining)]);
        }
        Ok(captured)
    })
}

fn log_ffmpeg_stderr(drain: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>) {
    let Some(drain) = drain else {
        return;
    };
    match drain.join() {
        Ok(Ok(output)) if !output.is_empty() => {
            log::warn!("ffmpeg: {}", String::from_utf8_lossy(&output).trim());
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::warn!("failed reading ffmpeg diagnostics: {error}"),
        Err(_) => log::warn!("ffmpeg diagnostics thread panicked"),
    }
}

fn write_frame(
    stdin: &mut ChildStdin,
    last_rgba: &mut [u8],
    frame: Option<&Frame>,
    config: &RecordingStart,
) -> anyhow::Result<()> {
    if let Some(frame) = frame {
        if frame.width == config.width
            && frame.height == config.height
            && frame.rgba.len() == last_rgba.len()
        {
            last_rgba.copy_from_slice(&frame.rgba);
        } else {
            log::warn!(
                "recording frame dimensions changed ({}x{}); repeating previous frame",
                frame.width,
                frame.height
            );
        }
    }
    stdin.write_all(last_rgba)?;
    Ok(())
}

fn wait_for_ffmpeg(child: &mut Child) -> anyhow::Result<()> {
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("ffmpeg exited with status {status}"),
        Err(error) => Err(anyhow::anyhow!("failed waiting for ffmpeg: {error}")),
    }
}

fn decoded_video_frame_count(path: &Path) -> anyhow::Result<u64> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames"])
        .args(["-show_entries", "stream=nb_read_frames"])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "ffprobe exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)?
        .trim()
        .parse::<u64>()
        .map_err(Into::into)
}

fn validate_encoder_result(
    encoder_result: anyhow::Result<()>,
    video_path: &Path,
    expected_frames: u64,
) -> anyhow::Result<bool> {
    let Err(error) = encoder_result else {
        return Ok(false);
    };
    match decoded_video_frame_count(video_path) {
        Ok(decoded_frames) if decoded_frames == expected_frames => {
            log::warn!(
                "ffmpeg exited non-zero after writing a complete decodable stream; preserving degraded session: {error}"
            );
            Ok(true)
        }
        Ok(decoded_frames) => anyhow::bail!(
            "{error}; encoded video contains {decoded_frames} frames but input contains {expected_frames}"
        ),
        Err(probe_error) => Err(error.context(format!(
            "encoded video could not be validated after ffmpeg failure: {probe_error}"
        ))),
    }
}

#[cfg(unix)]
fn isolate_terminal_signals(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // A terminal interrupt targets RetroFeel's foreground process group. Give
    // ffmpeg its own group so graceful app shutdown can close stdin first and
    // let Matroska finalization finish normally.
    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_terminal_signals(_command: &mut Command) {}

/// Rebuild the convenience transition log from a finalized `input.json`.
/// Keeping this independent from the recording producer is what makes a
/// repair/regeneration byte-identical and prevents it from becoming a second
/// source of truth.
pub(crate) fn write_input_transitions_from_log(
    input_json_path: &Path,
    transitions_path: &Path,
) -> anyhow::Result<()> {
    let frames: Vec<InputFrame> = serde_json::from_reader(std::fs::File::open(input_json_path)?)?;
    let transitions = input_transitions_from_frames(&frames);
    atomic_write(transitions_path, |file| {
        for transition in transitions {
            serde_json::to_writer(&mut *file, &transition)?;
            file.write_all(b"\n")?;
        }
        Ok(())
    })
}

fn mux_audio(raw_video_path: &PathBuf, audio_path: &PathBuf, video_path: &PathBuf) -> bool {
    let Some(ffmpeg) = find_media_tool("ffmpeg") else {
        log::warn!("cannot mux recording audio because ffmpeg was not found");
        return false;
    };
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .args(["-hide_banner", "-loglevel", "warning", "-nostats"])
        .arg("-i")
        .arg(raw_video_path)
        .arg("-i")
        .arg(audio_path)
        .args(["-c:v", "copy"])
        .args(["-c:a", "aac"])
        .arg("-shortest")
        .arg(video_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    isolate_terminal_signals(&mut command);
    let child = command.spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            log::warn!("failed to start ffmpeg audio mux: {error}");
            return false;
        }
    };
    let stderr_drain = child.stderr.take().map(spawn_ffmpeg_stderr_drain);
    let status = child.wait();
    log_ffmpeg_stderr(stderr_drain);
    match status {
        Ok(status) if status.success() => true,
        Ok(status) => {
            log::warn!("ffmpeg mux exited with status {status}");
            false
        }
        Err(error) => {
            log::warn!("failed to mux recording audio: {error}");
            false
        }
    }
}

fn promote_unmuxed_master(raw_video_path: &Path, video_path: &Path) -> Option<String> {
    if video_path.exists() {
        if let Err(error) = std::fs::remove_file(video_path) {
            log::warn!(
                "failed removing incomplete video master {}: {error}",
                video_path.display()
            );
            return Some(raw_video_path.display().to_string());
        }
    }
    match std::fs::rename(raw_video_path, video_path) {
        Ok(()) => Some(video_path.display().to_string()),
        Err(error) => {
            log::warn!(
                "failed promoting clean video master {}: {error}",
                video_path.display()
            );
            Some(raw_video_path.display().to_string())
        }
    }
}

/// Transcribe the mic track to `transcript.srt`. Timestamps are relative to
/// recording start, matching the video timeline (the mic thread skips pause
/// segments the same way the video writer does).
///
/// Configured local providers are best-effort: the isolated Sherpa worker,
/// whisper.cpp's `whisper-cli` with an explicit model, or an explicitly
/// selected OpenAI Whisper CLI. No private model cache is searched.
#[derive(Debug, Clone)]
struct TranscriptArtifacts {
    srt: PathBuf,
    json: PathBuf,
}

fn transcribe_mic(
    session_dir: &Path,
    mic_wav: &Path,
    config: &TranscriptionConfig,
) -> Result<TranscriptArtifacts, String> {
    if config.provider == retrofeel_types::TranscriptionProvider::SherpaOnnx {
        return transcribe_with_sherpa_worker(session_dir, mic_wav, config);
    }
    let srt_path = session_dir.join("transcript.srt");
    let executable = discover_whisper_executable(config);
    let model = config.external_model.as_deref();

    let is_python_whisper = |path: &Path| {
        path.file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("whisper"))
    };
    let whisper_cpp = model
        .zip(executable.as_deref())
        .filter(|(_, executable)| !is_python_whisper(executable));
    if let Some((model, executable)) = whisper_cpp {
        if !model.exists() {
            return Err(format!("Whisper model does not exist: {}", model.display()));
        } else {
            let wav16_path = session_dir.join("mic-16k.wav");
            let converted = find_media_tool("ffmpeg")
                .and_then(|ffmpeg| {
                    Command::new(ffmpeg)
                        .arg("-y")
                        .arg("-i")
                        .arg(mic_wav)
                        .args(["-ar", "16000", "-ac", "1"])
                        .arg(&wav16_path)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .ok()
                })
                .is_some_and(|status| status.success());
            if converted {
                let output_base = session_dir.join("transcript");
                let status = Command::new(executable)
                    .arg("-m")
                    .arg(model)
                    .arg("-f")
                    .arg(&wav16_path)
                    .arg("-osrt")
                    .arg("-of")
                    .arg(&output_base)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = std::fs::remove_file(&wav16_path);
                match status {
                    Ok(status) if status.success() && srt_path.exists() => {
                        log::info!("mic transcript written: {}", srt_path.display());
                        return canonicalize_transcript(&srt_path, config);
                    }
                    Ok(status) => log::warn!("whisper-cli exited with status {status}"),
                    Err(error) => log::warn!("failed to run whisper-cli: {error}"),
                }
            } else {
                return Err("Failed to convert mic audio to 16 kHz mono".into());
            }
        }
    }

    if let Some(executable) = executable.filter(|path| is_python_whisper(path)) {
        let status = Command::new(executable)
            .arg(mic_wav)
            .args(["--output_format", "srt"])
            .arg("--output_dir")
            .arg(session_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => {
                // openai-whisper names the output after the input stem.
                let produced = session_dir.join("mic.srt");
                if produced.exists() && std::fs::rename(&produced, &srt_path).is_ok() {
                    log::info!("mic transcript written: {}", srt_path.display());
                    return canonicalize_transcript(&srt_path, config);
                }
                return Err("Whisper succeeded but produced no SRT output".into());
            }
            Ok(status) => return Err(format!("Whisper exited with status {status}")),
            Err(error) => return Err(format!("Failed to run Whisper: {error}")),
        }
    }
    if model.is_none() {
        Err("Microphone audio is ready, but no transcription model is selected".into())
    } else {
        Err("No valid Whisper executable was found; choose one in Settings → Transcription".into())
    }
}

fn transcribe_with_sherpa_worker(
    session_dir: &Path,
    mic_wav: &Path,
    config: &TranscriptionConfig,
) -> Result<TranscriptArtifacts, String> {
    let worker = transcriber_worker_path().ok_or_else(|| {
        "The isolated RetroFeel transcriber worker is not installed next to the application"
            .to_string()
    })?;
    let mut child = Command::new(&worker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Failed to start transcriber worker: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Transcriber worker stdin was unavailable".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Transcriber worker stdout was unavailable".to_string())?;
    let job_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recording")
        .to_string();
    let request = retrofeel_types::TranscriptionWorkerRequest::Transcribe {
        job_id: job_id.clone(),
        mic_wav: mic_wav.to_path_buf(),
        output_dir: session_dir.to_path_buf(),
        config: config.clone(),
    };
    serde_json::to_writer(&mut stdin, &request)
        .map_err(|error| format!("Failed to submit transcription: {error}"))?;
    stdin
        .write_all(b"\n")
        .map_err(|error| format!("Failed to submit transcription: {error}"))?;
    stdin
        .flush()
        .map_err(|error| format!("Failed to submit transcription: {error}"))?;

    let mut reader = BufReader::new(stdout);
    let result = loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("Transcriber worker transport failed: {error}"))?;
        if bytes == 0 {
            break Err("Transcriber worker exited before completing the job".into());
        }
        if bytes > retrofeel_types::MAX_TRANSCRIPTION_MESSAGE_BYTES {
            break Err("Transcriber worker returned an oversized message".into());
        }
        let event: retrofeel_types::TranscriptionWorkerEvent = serde_json::from_str(&line)
            .map_err(|error| format!("Invalid transcriber worker response: {error}"))?;
        match event {
            retrofeel_types::TranscriptionWorkerEvent::Complete {
                job_id: completed,
                transcript_json,
                transcript_srt,
            } if completed == job_id => {
                break Ok(TranscriptArtifacts {
                    srt: transcript_srt,
                    json: transcript_json,
                });
            }
            retrofeel_types::TranscriptionWorkerEvent::Failed {
                job_id: failed,
                message,
            } if failed == job_id => break Err(message),
            retrofeel_types::TranscriptionWorkerEvent::Cancelled { job_id: cancelled }
                if cancelled == job_id =>
            {
                break Err("Transcription was cancelled".into());
            }
            _ => {}
        }
    };
    let _ = serde_json::to_writer(
        &mut stdin,
        &retrofeel_types::TranscriptionWorkerRequest::Shutdown,
    );
    let _ = stdin.write_all(b"\n");
    drop(stdin);
    let _ = child.wait();
    result
}

fn transcriber_worker_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RETROFEEL_TRANSCRIBER_WORKER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let name = executable_name("retrofeel-transcriber-worker");
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join(&name);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(&name))
            .find(|path| path.is_file())
    })
}

pub(crate) fn retry_transcription(
    session_dir: &Path,
    config: TranscriptionConfig,
) -> anyhow::Result<()> {
    let manifest_path = session_dir.join("manifest.json");
    let mut manifest: SessionManifest =
        serde_json::from_reader(std::fs::File::open(&manifest_path)?)?;
    if let Some(rom) = manifest
        .rom
        .as_mut()
        .filter(|rom| rom.size == 0 || rom.sha1 == EMPTY_SHA1)
    {
        let source = Path::new(&rom.path);
        if source.is_file() {
            let (sha1, size) = rom_identity(source)?;
            rom.sha1 = sha1;
            rom.size = size;
        }
    }
    let mic_wav = manifest
        .mic_audio
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| session_dir.join("mic.wav"));
    if !mic_wav.is_file() {
        anyhow::bail!("This session has no microphone track");
    }
    manifest.transcription_status = TranscriptionJobState::Queued;
    manifest.transcription_provider = Some(config.provider);
    manifest.transcription_model = config.selected_model_id.clone();
    manifest.transcript = None;
    manifest.transcript_json = None;
    let _ = finalize_manifest_and_start_transcription(
        session_dir,
        manifest,
        Some(mic_wav),
        config,
        transcribe_mic,
    )?;
    Ok(())
}

pub(crate) fn discover_whisper_executable(config: &TranscriptionConfig) -> Option<PathBuf> {
    if let Some(path) = config.external_executable.as_ref() {
        if validate_whisper_executable(path) {
            return Some(path.clone());
        }
    }
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join(executable_name("whisper-cli")));
            candidates.push(dir.join(executable_name("whisper")));
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        candidates.push(PathBuf::from(dir).join(executable_name("whisper-cli")));
        candidates.push(PathBuf::from(dir).join(executable_name("whisper")));
    }
    if let Some(home) = home_dir() {
        for dir in [home.join(".local/bin"), home.join("bin")] {
            candidates.push(dir.join(executable_name("whisper-cli")));
            candidates.push(dir.join(executable_name("whisper")));
        }
    }
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        candidates.push(local.join("whisper.cpp/whisper-cli.exe"));
        candidates.push(local.join("Programs/whisper/whisper.exe"));
    }
    candidates
        .into_iter()
        .find(|candidate| validate_whisper_executable(candidate))
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn validate_whisper_executable(path: &Path) -> bool {
    path.is_file()
        && Command::new(path)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

fn canonicalize_transcript(
    srt_path: &Path,
    config: &TranscriptionConfig,
) -> Result<TranscriptArtifacts, String> {
    let srt = std::fs::read_to_string(srt_path)
        .map_err(|error| format!("Failed to read transcript SRT: {error}"))?;
    let segments = parse_srt(&srt)?;
    let document = TranscriptDocument {
        schema_version: retrofeel_types::TRANSCRIPTION_SCHEMA_VERSION,
        language: config.language.clone(),
        provider: config.provider,
        model_id: config.selected_model_id.clone(),
        segments,
    };
    let json_path = srt_path.with_extension("json");
    atomic_write(&json_path, |file| {
        serde_json::to_writer_pretty(file, &document)?;
        Ok(())
    })
    .map_err(|error| format!("Failed to write canonical transcript: {error}"))?;
    Ok(TranscriptArtifacts {
        srt: srt_path.to_path_buf(),
        json: json_path,
    })
}

pub(crate) fn parse_srt(text: &str) -> Result<Vec<TranscriptSegment>, String> {
    let normalized = text.replace("\r\n", "\n");
    let mut segments = Vec::new();
    for block in normalized.split("\n\n") {
        let mut lines = block.lines().filter(|line| !line.trim().is_empty());
        let Some(first) = lines.next() else { continue };
        let timing = if first.contains("-->") {
            first
        } else {
            lines
                .next()
                .ok_or_else(|| format!("SRT block has no timestamp: {block}"))?
        };
        let (start, end) = timing
            .split_once("-->")
            .ok_or_else(|| format!("Invalid SRT timestamp: {timing}"))?;
        let text = lines.collect::<Vec<_>>().join(" ").trim().to_string();
        if text.is_empty() {
            continue;
        }
        segments.push(TranscriptSegment {
            start_seconds: parse_srt_timestamp(start.trim())?,
            end_seconds: parse_srt_timestamp(end.trim())?,
            text,
        });
    }
    Ok(segments)
}

fn parse_srt_timestamp(value: &str) -> Result<f64, String> {
    let value = value.replace(',', ".");
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(format!("Invalid SRT timestamp: {value}"));
    }
    let hours = parts[0]
        .parse::<u64>()
        .map_err(|_| format!("Invalid SRT hours: {value}"))?;
    let minutes = parts[1]
        .parse::<u64>()
        .map_err(|_| format!("Invalid SRT minutes: {value}"))?;
    let seconds = parts[2]
        .parse::<f64>()
        .map_err(|_| format!("Invalid SRT seconds: {value}"))?;
    Ok(hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds)
}

fn next_session_dir(recordings_dir: &Path) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    recordings_dir.join(format!("session-{millis}.feel"))
}

fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_tool_discovery_uses_macos_package_manager_locations() {
        let fallback = tempfile::tempdir().unwrap();
        let ffmpeg = fallback.path().join("ffmpeg");
        std::fs::write(&ffmpeg, b"test executable").unwrap();

        let found = find_media_tool_with_path(
            "ffmpeg",
            Some(std::ffi::OsStr::new("/definitely/not/a/tool/directory")),
            &[fallback.path()],
        );

        assert_eq!(found.as_deref(), Some(ffmpeg.as_path()));
    }
    use retrofeel_types::TranscriptionProvider;
    use std::time::Duration;

    fn manifest(session_dir: &Path) -> SessionManifest {
        SessionManifest {
            core: CoreInfo {
                name: "Mock Core".to_string(),
                version: "1.0".to_string(),
                library_path: "mock_core.dylib".to_string(),
            },
            rom: None,
            timing: TimingInfo {
                fps: 60.0,
                sample_rate: 48_000.0,
                start_timestamp: 0.0,
            },
            initial_state: None,
            frame_count: 1,
            pause_segments: Vec::new(),
            input_log: session_dir.join("input.json").display().to_string(),
            video: Some(session_dir.join("video.mkv").display().to_string()),
            mic_audio: Some(session_dir.join("mic.wav").display().to_string()),
            transcript: None,
            transcript_json: None,
            transcription_status: TranscriptionJobState::Queued,
            transcription_provider: Some(TranscriptionProvider::WhisperCpp),
            transcription_model: Some("test".into()),
            binding_map: None,
            capture_provenance: None,
            video_timing: None,
            frame_map: None,
            input_transitions: None,
            track_alignment: None,
            external_capture: None,
            dropped_frames: 0,
            format: ManifestFormat::Json,
        }
    }

    #[test]
    fn manifests_are_finalized_before_transcription_completes() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().to_path_buf();
        let mic_path = session_dir.join("mic.wav");
        std::fs::write(&mic_path, b"wav").unwrap();
        let manifest = manifest(&session_dir);
        let transcript_path = session_dir.join("transcript.srt");
        let transcript_for_job = transcript_path.clone();
        let transcript_json = session_dir.join("transcript.json");
        let json_for_job = transcript_json.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let caller = std::thread::spawn(move || {
            let result = finalize_manifest_and_start_transcription(
                &session_dir,
                manifest,
                Some(mic_path),
                TranscriptionConfig::default(),
                move |_, _, _| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(TranscriptArtifacts {
                        srt: transcript_for_job,
                        json: json_for_job,
                    })
                },
            );
            returned_tx.send(result).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let postprocess = returned_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("manifest finalization must not wait for transcription")
            .unwrap()
            .expect("mic recording should start post-processing");

        let initial: SessionManifest =
            serde_json::from_reader(std::fs::File::open(dir.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(initial.transcript, None);

        release_tx.send(()).unwrap();
        postprocess.join().unwrap();
        caller.join().unwrap();

        let json: SessionManifest =
            serde_json::from_reader(std::fs::File::open(dir.path().join("manifest.json")).unwrap())
                .unwrap();
        let ron: SessionManifest =
            ron::de::from_reader(std::fs::File::open(dir.path().join("manifest.ron")).unwrap())
                .unwrap();
        assert_eq!(
            json.transcript.as_deref(),
            Some(transcript_path.to_str().unwrap())
        );
        assert_eq!(ron.transcript, json.transcript);
        assert_eq!(json.transcript_json.as_deref(), transcript_json.to_str());
        assert_eq!(json.transcription_status, TranscriptionJobState::Complete);
        assert_eq!(json.format, ManifestFormat::Json);
        assert_eq!(ron.format, ManifestFormat::Ron);
    }

    #[test]
    fn failed_transcription_is_actionable_in_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let mic_path = dir.path().join("mic.wav");
        std::fs::write(&mic_path, b"wav").unwrap();

        let postprocess = finalize_manifest_and_start_transcription(
            dir.path(),
            manifest(dir.path()),
            Some(mic_path),
            TranscriptionConfig::default(),
            |_, _, _| Err("model checksum failed".into()),
        )
        .unwrap()
        .unwrap();
        postprocess.join().unwrap();

        let json: SessionManifest =
            serde_json::from_reader(std::fs::File::open(dir.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(json.transcript, None);
        assert_eq!(
            json.transcription_status,
            TranscriptionJobState::Failed {
                message: "model checksum failed".into()
            }
        );
    }

    #[test]
    fn parses_multiline_srt_into_canonical_segments() {
        let segments = parse_srt(
            "1\n00:00:01,250 --> 00:00:03,500\nHello\nworld\n\n2\n00:01:00.000 --> 00:01:02.250\nAgain\n",
        )
        .unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start_seconds, 1.25);
        assert_eq!(segments[0].text, "Hello world");
        assert_eq!(segments[1].end_seconds, 62.25);
    }

    #[test]
    fn rom_identity_hashes_the_source_file_for_full_path_cores() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("game.chd");
        std::fs::write(&rom, b"full-path-content").unwrap();
        let (sha1, size) = rom_identity(&rom).unwrap();
        assert_eq!(sha1, rom_hash(b"full-path-content"));
        assert_eq!(size, 17);
        assert_ne!(sha1, rom_hash(&[]));
    }

    #[test]
    fn transition_log_is_byte_identical_when_regenerated_from_input_json() {
        let dir = tempfile::tempdir().unwrap();
        let input_path = dir.path().join("input.json");
        let first = InputFrame {
            frame: 0,
            elapsed_us: Some(0),
            port: 0,
            state: InputState::default(),
            raw_host: Some(RawHostInput::default()),
        };
        let mut pressed = first.clone();
        pressed.frame = 1;
        pressed.elapsed_us = Some(16_666);
        pressed.raw_host.as_mut().unwrap().keyboard_keys = vec!["A".into()];
        serde_json::to_writer_pretty(
            std::fs::File::create(&input_path).unwrap(),
            &[first, pressed],
        )
        .unwrap();

        let once = dir.path().join("once.jsonl");
        let twice = dir.path().join("twice.jsonl");
        write_input_transitions_from_log(&input_path, &once).unwrap();
        write_input_transitions_from_log(&input_path, &twice).unwrap();
        assert_eq!(std::fs::read(once).unwrap(), std::fs::read(twice).unwrap());
    }

    #[test]
    fn silent_recording_promotes_clean_master_to_video_mkv() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("video-no-audio.mkv");
        let master = dir.path().join("video.mkv");
        std::fs::write(&raw, b"clean cfr video").unwrap();

        let selected = promote_unmuxed_master(&raw, &master).unwrap();

        assert_eq!(selected, master.display().to_string());
        assert!(!raw.exists());
        assert_eq!(std::fs::read(master).unwrap(), b"clean cfr video");
    }

    #[test]
    fn exhausted_payload_budget_preserves_tick_as_writer_dupe_without_blocking() {
        let (sender, receiver) = unbounded();
        sender
            .send(WriterMessage::Pause { frame_index: 9 })
            .unwrap();
        let queued_frame_payloads = Arc::new(AtomicUsize::new(RECORDING_FRAME_PAYLOAD_CAPACITY));
        let handle = RecordingHandle {
            sender,
            queued_frame_payloads,
            payload_overflows: AtomicU64::new(0),
            join: None,
            mic: None,
            video_clock_anchor_us: Mutex::new(None),
            session_dir: PathBuf::new(),
        };

        let mut raw_host = RawHostInput::default();
        raw_host.keyboard_keys.push("A".into());
        let started = std::time::Instant::now();
        handle.frame_timed(
            10,
            Some(166_667),
            None,
            InputState::default(),
            Some(raw_host),
            Some(Arc::new(Frame {
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            })),
            Vec::new(),
        );
        assert!(started.elapsed() < Duration::from_millis(25));

        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            WriterMessage::Pause { frame_index: 9 }
        ));
        let message = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the payload-budget fallback must preserve the CFR tick");
        match message {
            WriterMessage::FrameDupe {
                frame_index,
                elapsed_us,
                raw_host,
                ..
            } => {
                assert_eq!(frame_index, 10);
                assert_eq!(elapsed_us, Some(166_667));
                assert_eq!(raw_host.unwrap().keyboard_keys, ["A"]);
            }
            _ => panic!("expected a compact writer dupe"),
        }
    }

    #[test]
    fn ffmpeg_stderr_drain_is_bounded_while_consuming_all_output() {
        let output = vec![b'x'; FFMPEG_STDERR_CAPTURE_LIMIT * 4];
        let drain = spawn_ffmpeg_stderr_drain(std::io::Cursor::new(output));
        let captured = drain.join().unwrap().unwrap();

        assert_eq!(captured.len(), FFMPEG_STDERR_CAPTURE_LIMIT);
        assert!(captured.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn nonzero_encoder_exit_keeps_a_complete_decodable_stream() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("interrupted.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("color=size=16x16:rate=4:duration=1")
            .args(["-c:v", "ffv1"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());

        let degraded = validate_encoder_result(
            Err(anyhow::anyhow!("synthetic interrupted encoder")),
            &video,
            4,
        )
        .unwrap();

        assert!(degraded);
    }

    #[test]
    fn nonzero_encoder_exit_rejects_a_frame_count_mismatch() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("interrupted.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("color=size=16x16:rate=4:duration=1")
            .args(["-c:v", "ffv1"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());

        let error = validate_encoder_result(
            Err(anyhow::anyhow!("synthetic interrupted encoder")),
            &video,
            5,
        )
        .unwrap_err();

        assert!(error.to_string().contains("contains 4 frames"));
        assert!(error.to_string().contains("input contains 5"));
    }

    #[test]
    fn video_encoder_writes_a_decodable_cfr_master() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let recording = RecordingHandle::start(RecordingStart {
            recordings_dir: dir.path().to_path_buf(),
            core_name: "encoder-smoke".into(),
            core_version: "test".into(),
            core_path: PathBuf::new(),
            rom_path: None,
            rom_sha1: None,
            rom_size: None,
            fps: 60.0,
            sample_rate: 48_000.0,
            width: 64,
            height: 64,
            binding_map: None,
            initial_state_path: None,
            capture_mic: false,
            live_mic_peak: None,
            transcription: TranscriptionConfig::default(),
            capture_provenance: None,
            video_timing: None,
            track_alignment: None,
        })
        .unwrap();
        let session_dir = recording.session_dir.clone();
        let frame = Frame {
            width: 64,
            height: 64,
            rgba: vec![0x7f; 64 * 64 * 4],
        };
        recording.frame(0, InputState::default(), None, Some(frame), Vec::new());
        for index in 1..4 {
            recording.frame(index, InputState::default(), None, None, Vec::new());
        }
        recording.stop();

        let video = session_dir.join("video.mkv");
        assert_eq!(decoded_video_frame_count(&video).unwrap(), 4);
        let manifest: SessionManifest = serde_json::from_reader(
            std::fs::File::open(session_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.frame_count, 4);
        assert_eq!(manifest.dropped_frames, 0);
    }

    #[test]
    fn discovers_homebrew_whisper_cli_when_installed() {
        let installed = Path::new("/opt/homebrew/bin/whisper-cli");
        if installed.is_file() {
            let discovered = discover_whisper_executable(&TranscriptionConfig::default());
            assert_eq!(discovered.as_deref(), Some(installed));
        }
    }

    /// Opt-in real inference smoke. Release/manual verification supplies a
    /// session with mic.wav, an installed model, and the sibling worker path.
    #[test]
    #[ignore = "requires a real model and RETROFEEL_SMOKE_SESSION/MODEL environment"]
    fn real_local_transcription_backfill() {
        let session = PathBuf::from(
            std::env::var_os("RETROFEEL_SMOKE_SESSION").expect("RETROFEEL_SMOKE_SESSION"),
        );
        let model = PathBuf::from(
            std::env::var_os("RETROFEEL_SMOKE_MODEL").expect("RETROFEEL_SMOKE_MODEL"),
        );
        let model_id = std::env::var("RETROFEEL_SMOKE_MODEL_ID")
            .unwrap_or_else(|_| "parakeet-tdt-0.6b-v3-int8".into());
        retry_transcription(
            &session,
            TranscriptionConfig {
                provider: TranscriptionProvider::SherpaOnnx,
                selected_model_id: Some(model_id),
                external_model: Some(model),
                ..Default::default()
            },
        )
        .expect("queue transcription");
        for _ in 0..600 {
            std::thread::sleep(Duration::from_secs(1));
            let manifest: SessionManifest = serde_json::from_reader(
                std::fs::File::open(session.join("manifest.json")).expect("manifest"),
            )
            .expect("valid manifest");
            match manifest.transcription_status {
                TranscriptionJobState::Complete => {
                    assert!(session.join("transcript.json").is_file());
                    assert!(session.join("transcript.srt").is_file());
                    return;
                }
                TranscriptionJobState::Failed { message } => {
                    panic!("real transcription failed: {message}")
                }
                _ => {}
            }
        }
        panic!("real transcription timed out");
    }
}
