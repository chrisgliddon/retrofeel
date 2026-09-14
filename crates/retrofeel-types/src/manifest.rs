//! Session manifest — emitted alongside a recording.

use serde::{Deserialize, Serialize};

use crate::{InputBindingSet, TranscriptionJobState, TranscriptionProvider};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreInfo {
    pub name: String,
    pub version: String,
    pub library_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RomInfo {
    pub path: String,
    /// SHA-1 of the ROM bytes, lowercased hex.
    pub sha1: String,
    /// Size in bytes.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TimingInfo {
    pub fps: f64,
    pub sample_rate: f64,
    /// Wall-clock start, Unix epoch seconds.
    pub start_timestamp: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PauseSegment {
    /// Frame index at which capture was paused.
    pub start_frame: u64,
    /// Frame index at which capture resumed (exclusive of pause gap).
    pub end_frame: u64,
}

/// Where the recording's primary video came from. This is deliberately
/// separate from the legacy `external_capture` field: a local ScreenCaptureKit
/// recording is owned by RetroFeel, but it still needs to disclose that its
/// source clock was ScreenCaptureKit rather than a libretro frame clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSourceKind {
    Libretro,
    MacosScreenCaptureKit,
    SteamGameRecording,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureProvenance {
    pub kind: CaptureSourceKind,
    /// Human-readable policy rather than a capability guess. For example,
    /// macOS local Steam capture intentionally records no game/system audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_audio_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoTimingKind {
    /// The producer's frame number is the video clock (libretro).
    FrameIndexed,
    /// A variable-rate ScreenCaptureKit source was explicitly resampled onto
    /// a constant encoded-video grid.
    CfrResampledSck,
    /// Video is owned by an external variable-frame-rate recorder.
    ExternalVariableFrameRate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTiming {
    pub kind: VideoTimingKind,
    /// Output/encoded video rate when the recording has a CFR grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_fps: Option<f64>,
    /// The source timestamp domain retained by source-aware captures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_clock: Option<String>,
}

/// One explicit, lossy association from an encoded CFR frame to the SCK frame
/// that supplied its pixels and input state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMapEntry {
    pub encoded_frame: u64,
    pub encoded_pts_us: u64,
    pub source_frame: u64,
    pub source_pts_us: u64,
    /// True when this encoded grid tick repeats the prior selected source
    /// frame because no newer source frame existed at the tick.
    #[serde(default)]
    pub grid_duplicate: bool,
    /// Source frames superseded before this selected source frame reached an
    /// encoded grid tick. These are intentionally lossy and never hidden.
    #[serde(default)]
    pub discarded_source_frames_before: u64,
    /// True only when the recording writer queue was full and it encoded the
    /// previous RGBA buffer for this grid tick.
    #[serde(default)]
    pub writer_dupe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMapInfo {
    pub path: String,
    #[serde(default)]
    pub source_frames_received: u64,
    #[serde(default)]
    pub source_frames_discarded: u64,
    #[serde(default)]
    pub grid_duplicates: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPresence {
    Present,
    /// Explicitly excluded by recording policy, rather than absent because a
    /// source could not provide it.
    NotCaptured,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackAlignmentStatus {
    Complete,
    Degraded,
    NotCaptured,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackAlignment {
    pub presence: TrackPresence,
    pub status: TrackAlignmentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_us: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackAlignmentMetadata {
    pub video: TrackAlignment,
    pub narration: TrackAlignment,
    pub game_audio: TrackAlignment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCaptureKind {
    SteamGameRecording,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCaptureStatus {
    Complete,
    #[default]
    Partial,
    DegradedAlignment,
    MissingVideo,
}

/// Audio source owned by an external recorder. Steam currently exposes one
/// mixed game/system/microphone track rather than an isolated microphone stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAudioSource {
    SteamMixedAudio,
}

/// Mapping between an external recorder's source clock and video PTS zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalVideoClock {
    /// CLOCK_BOOTTIME timestamp corresponding to encoded video PTS zero.
    pub video_pts_zero_boottime_us: u64,
    /// Source timestamp reported by the external recorder.
    pub source_pts_us: u64,
    /// Recorder-normalized PTS reported for that same source frame.
    pub normalized_pts_us: u64,
}

/// Additional provenance for recordings whose video is owned by another app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCaptureInfo {
    pub kind: ExternalCaptureKind,
    /// String-valued because Steam uses both normal App IDs and 64-bit IDs for
    /// non-Steam shortcuts.
    pub game_id: String,
    pub recording_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_video: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_clock: Option<ExternalVideoClock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_map: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_event_log: Option<String>,
    /// SRT derived from the external recorder's audio track. This is separate
    /// from `SessionManifest::transcript`, which is reserved for a dedicated
    /// microphone/narration asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_transcript_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_transcript_source: Option<ExternalAudioSource>,
    #[serde(default)]
    pub audio_transcription_status: TranscriptionJobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_transcription_provider: Option<TranscriptionProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_transcription_model: Option<String>,
    #[serde(default)]
    pub status: ExternalCaptureStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub core: CoreInfo,
    pub rom: Option<RomInfo>,
    pub timing: TimingInfo,
    /// Optional path to an initial save state used for deterministic replay.
    pub initial_state: Option<String>,
    /// Total number of frames captured.
    pub frame_count: u64,
    pub pause_segments: Vec<PauseSegment>,
    /// Path to the input log (JSON or RON).
    pub input_log: String,
    /// Path to the encoded video.
    pub video: Option<String>,
    /// Path to the microphone audio captured alongside the session (`mic.wav`).
    /// The mic track starts at recording start and skips pause segments, so
    /// its timeline matches the frame-indexed video timeline.
    #[serde(default)]
    pub mic_audio: Option<String>,
    /// Path to the timestamped SRT transcript of the mic track
    /// (`transcript.srt`). None when no transcriber was available or the mic
    /// was not captured.
    #[serde(default)]
    pub transcript: Option<String>,
    /// Canonical timestamped transcript document (`transcript.json`).
    #[serde(default)]
    pub transcript_json: Option<String>,
    #[serde(default)]
    pub transcription_status: TranscriptionJobState,
    #[serde(default)]
    pub transcription_provider: Option<TranscriptionProvider>,
    #[serde(default)]
    pub transcription_model: Option<String>,
    /// Input bindings active for this recording.
    #[serde(default)]
    pub binding_map: Option<InputBindingSet>,
    /// Capture source and explicit audio policy. New readers should prefer
    /// this over inferring provenance from `core.name` or file names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_provenance: Option<CaptureProvenance>,
    /// Contract for the encoded video timeline. `cfr_resampled_sck` means
    /// `InputFrame.elapsed_us` is the same CFR clock as decoded video PTS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_timing: Option<VideoTiming>,
    /// Relative path and quality counters for the source-to-CFR mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_map: Option<FrameMapInfo>,
    /// Deterministically derived from the authoritative `input.json` after
    /// finalization. It is a convenience index, never a competing source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_transitions: Option<String>,
    /// Alignment and availability metadata for each audio/video track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_alignment: Option<TrackAlignmentMetadata>,
    /// Provenance and clock information for video owned by an external
    /// recorder such as Steam Game Recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_capture: Option<ExternalCaptureInfo>,
    /// Number of recording frames that were dropped because the writer queue
    /// was full. The writer writes the previous frame as a dupe for each
    /// dropped video frame so `frame_count` stays 1:1 with the input log, but
    /// a non-zero value indicates the host couldn't keep up. Surfaced so
    /// exporters/replayers can flag a session as potentially lossy.
    #[serde(default)]
    pub dropped_frames: u64,
    /// Format the manifest was serialized as.
    pub format: ManifestFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestFormat {
    Json,
    Ron,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_manifests_default_new_transcription_fields() {
        let json = r#"{
          "core":{"name":"core","version":"1","library_path":"core.so"},
          "rom":null,
          "timing":{"fps":60.0,"sample_rate":48000.0,"start_timestamp":0.0},
          "initial_state":null,
          "frame_count":1,
          "pause_segments":[],
          "input_log":"input.json",
          "video":null,
          "mic_audio":"mic.wav",
          "transcript":null,
          "binding_map":null,
          "dropped_frames":0,
          "format":"Json"
        }"#;
        let manifest: SessionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.transcript_json, None);
        assert_eq!(
            manifest.transcription_status,
            TranscriptionJobState::NotRequested
        );
        assert_eq!(manifest.external_capture, None);
        assert_eq!(manifest.capture_provenance, None);
        assert_eq!(manifest.video_timing, None);
        assert_eq!(manifest.frame_map, None);
        assert_eq!(manifest.input_transitions, None);
        assert_eq!(manifest.track_alignment, None);
    }

    #[test]
    fn older_external_captures_default_mixed_audio_transcription_fields() {
        let json = r#"{
          "kind":"steam_game_recording",
          "game_id":"43",
          "recording_id":"fg_43_test",
          "source_video":"/tmp/session.mpd",
          "status":"complete"
        }"#;
        let external: ExternalCaptureInfo = serde_json::from_str(json).unwrap();
        assert_eq!(external.audio_transcript, None);
        assert_eq!(external.audio_transcript_json, None);
        assert_eq!(external.audio_transcript_source, None);
        assert_eq!(
            external.audio_transcription_status,
            TranscriptionJobState::NotRequested
        );
        assert_eq!(external.audio_transcription_provider, None);
        assert_eq!(external.audio_transcription_model, None);
    }
}
