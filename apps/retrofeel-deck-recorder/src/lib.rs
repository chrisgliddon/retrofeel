//! Steam Deck companion for frame-aligned Steam Game Recording input logs.

pub mod archive;
pub mod config;
mod dash;
pub mod hid;
pub mod media;
pub mod model;
#[cfg(any(target_os = "linux", test))]
mod reader_registry;
pub mod session;
pub mod steam_clip;
pub mod steam_log;
pub mod timeline;
pub mod transcription;
pub mod vdf;
pub mod youtube;

#[cfg(target_os = "linux")]
pub mod linux;

pub use archive::{
    migrate_recording_archives, repair_recording_archives, sync_recording_archives,
    ArchiveMigrationReport, ArchiveRepairReport, ArchiveSyncReport,
};
pub use config::{
    DeckArchiveFormat, DeckArchiveRule, DeckTranscriptionConfig, DeckYoutubeCategory,
    DeckYoutubeConfig, DeckYoutubePrivacy, DeviceAdmission, InputDeviceRule, PhysicalGamepadMatch,
    RecorderConfig,
};
pub use model::{
    AbsRange, ControllerBinding, ControllerLayoutMap, ControllerMap, InputCapabilities,
    InputCapability, InputDeviceInfo, InputDeviceLifecycle, InputDeviceSource, RawInputEvent,
};
pub use session::{
    export_video, list_sessions, reconcile_sessions, run_doctor, SessionSummary, WatchOptions,
};
pub use transcription::transcribe_steam_audio;
pub use youtube::{
    sync_youtube, sync_youtube_publishers, youtube_auth, youtube_auth_for, youtube_status,
    youtube_status_all, YoutubeStatusReport, YoutubeSyncReport,
};

#[cfg(target_os = "linux")]
pub use session::watch;
