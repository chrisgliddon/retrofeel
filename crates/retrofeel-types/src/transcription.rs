//! Versioned transcription configuration, manifests, and worker protocol.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const TRANSCRIPTION_SCHEMA_VERSION: u32 = 1;
pub const MAX_TRANSCRIPTION_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionProvider {
    /// The isolated RetroFeel worker backed by sherpa-onnx models.
    #[default]
    SherpaOnnx,
    /// A user-selected or discovered whisper.cpp executable and model.
    WhisperCpp,
    /// The Python openai-whisper command-line tool.
    OpenAiWhisper,
    /// A timestamped transcript imported from another device or application.
    /// Alignment provenance lives beside the transcript in the `.feel`
    /// package rather than being misrepresented as local speech recognition.
    ExternalTranscript,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionConfig {
    #[serde(default)]
    pub provider: TranscriptionProvider,
    #[serde(default)]
    pub selected_model_id: Option<String>,
    #[serde(default)]
    pub external_executable: Option<PathBuf>,
    #[serde(default)]
    pub external_model: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub automatic: bool,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default = "default_true")]
    pub backfill_missing: bool,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            provider: TranscriptionProvider::SherpaOnnx,
            selected_model_id: None,
            external_executable: None,
            external_model: None,
            automatic: true,
            language: None,
            backfill_missing: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionModelDescriptor {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub display_name: String,
    pub provider: TranscriptionProvider,
    pub languages: Vec<String>,
    pub download_bytes: u64,
    pub installed_bytes: u64,
    pub memory_mib: u32,
    pub source_url: String,
    pub version: String,
    pub license: String,
    pub sha256: String,
    #[serde(default)]
    pub recommended: bool,
}

const fn schema_version() -> u32 {
    TRANSCRIPTION_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum TranscriptionJobState {
    #[default]
    NotRequested,
    Queued,
    Running {
        progress_percent: u8,
    },
    Complete,
    Cancelled,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptDocument {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub language: Option<String>,
    pub provider: TranscriptionProvider,
    #[serde(default)]
    pub model_id: Option<String>,
    pub segments: Vec<TranscriptSegment>,
}

impl TranscriptDocument {
    pub fn plain_text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptionWorkerRequest {
    Transcribe {
        job_id: String,
        mic_wav: PathBuf,
        output_dir: PathBuf,
        config: TranscriptionConfig,
    },
    Cancel {
        job_id: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptionWorkerEvent {
    Ready {
        protocol_version: u32,
    },
    Progress {
        job_id: String,
        percent: u8,
    },
    Segment {
        job_id: String,
        segment: TranscriptSegment,
    },
    Complete {
        job_id: String,
        transcript_json: PathBuf,
        transcript_srt: PathBuf,
    },
    Cancelled {
        job_id: String,
    },
    Failed {
        job_id: String,
        message: String,
    },
}
