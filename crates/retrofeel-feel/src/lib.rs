//! Portable `.feel` directory packages.
//!
//! A package keeps the original capture at its root for compatibility with
//! existing RetroFeel tools. `feel.json` inventories those immutable source
//! files while transcripts and analyses live in versioned subdirectories.

mod agent;
mod alignment;
mod model;
mod package;
mod srt;

pub use agent::{
    analyze_package, discover_agents, AgentAdapterKind, AgentDescriptor, AnalyzeOptions,
};
pub use alignment::{
    align_srt, anchors_from_whisper_json, AlignmentOptions, AlignmentOutcome, TimedAnchor,
};
pub use model::{
    ActionItem, AlignmentMethod, AlignmentQuality, AlignmentReport, AnalysisEvidence,
    AnalysisInsight, AnalysisObservation, AnalysisResultV2, AnalysisRun, FeelManifestV1,
    FeelResource, FeelResourceRole, PackageValidation, TranscriptAsset, TranscriptKind,
    FEEL_FORMAT_VERSION, FEEL_KIND,
};
pub use package::{
    create_package, import_aligned_transcript, import_package_copy, initialize_native_package,
    package_title, FeelError, FeelPackage, PackageOptions,
};
pub use srt::{parse_srt, render_srt};
