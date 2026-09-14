use serde::{Deserialize, Serialize};

pub const FEEL_FORMAT_VERSION: u32 = 1;
pub const FEEL_KIND: &str = "com.retrofeel.feel";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeelResourceRole {
    SessionManifest,
    Video,
    AuthoritativeInput,
    RawInputEvents,
    InputTransitions,
    ControllerMap,
    ControllerLayout,
    FrameMap,
    CaptureMetadata,
    InitialState,
    OtherCapture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeelResource {
    pub role: FeelResourceRole,
    pub path: String,
    pub media_type: String,
    pub byte_len: u64,
    pub sha256: String,
    pub required: bool,
    /// Immutable resources are clean-master capture evidence. RetroFeel never
    /// rewrites them after a package is finalized.
    pub immutable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    Source,
    Aligned,
    Generated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptAsset {
    pub id: String,
    pub kind: TranscriptKind,
    pub srt_path: String,
    pub srt_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment_sha256: Option<String>,
    #[serde(default)]
    pub primary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlignmentMethod {
    ManualAffine,
    AutomaticWordAnchors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlignmentQuality {
    Complete,
    Estimated,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignmentReport {
    pub schema_version: u32,
    pub method: AlignmentMethod,
    pub quality: AlignmentQuality,
    /// `video_seconds = scale * source_seconds + offset_seconds`.
    pub scale: f64,
    pub offset_seconds: f64,
    pub anchor_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_start_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_end_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_absolute_residual_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_absolute_residual_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_absolute_residual_seconds: Option<f64>,
    #[serde(default)]
    pub extrapolated_ranges_seconds: Vec<[f64; 2]>,
    pub source_audio: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisRun {
    pub id: String,
    pub created_at_epoch_seconds: f64,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub result_path: String,
    pub result_sha256: String,
    pub report_path: String,
    pub report_sha256: String,
    pub action_plan_path: String,
    pub action_plan_sha256: String,
    pub agent_path: String,
    pub agent_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeelManifestV1 {
    pub kind: String,
    pub format_version: u32,
    pub package_id: String,
    pub title: String,
    pub created_at_epoch_seconds: f64,
    pub session_manifest: String,
    pub context_brief: String,
    pub context_brief_sha256: String,
    pub resources: Vec<FeelResource>,
    #[serde(default)]
    pub transcripts: Vec<TranscriptAsset>,
    #[serde(default)]
    pub analyses: Vec<AnalysisRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageValidation {
    pub package: String,
    pub valid: bool,
    pub checked_resources: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisEvidence {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_excerpt: Option<String>,
    #[serde(default)]
    pub controls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisObservation {
    pub category: String,
    pub polarity: String,
    pub statement: String,
    pub confidence: f64,
    #[serde(default)]
    pub evidence: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisInsight {
    pub title: String,
    pub implication: String,
    pub project_relevance: String,
    #[serde(default)]
    pub evidence: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionItem {
    pub title: String,
    pub rationale: String,
    pub priority: String,
    pub effort: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisResultV2 {
    pub schema_version: u32,
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<AnalysisEvidence>,
    #[serde(default)]
    pub observations: Vec<AnalysisObservation>,
    #[serde(default)]
    pub insights: Vec<AnalysisInsight>,
    #[serde(default)]
    pub actions: Vec<ActionItem>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}
