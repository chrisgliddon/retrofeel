//! Shared types for retrofeel: InputFrame IR, session manifests, config schema.
//!
//! These are plain serde structs so the FFI host crate, the Bevy app, and the
//! exporter never depend on each other — only on this crate.

pub mod config;
pub mod input;
pub mod manifest;
pub mod system;
pub mod transcription;
pub mod worker;

pub use config::{
    AppearanceConfig, AudioConfig, ConfigError, InputBindingSet, LibraryConfig, MetadataConfig,
    PathsConfig, RecordingConfig, RetroFeelConfig, RewindConfig, RomImportMode, ShaderConfig,
    SteamConfig, SteamGameEntry, SteamGameSource, SteamLaunchMode, ThemePreference, VideoConfig,
    WindowBehaviorConfig,
};
pub use input::{
    input_transitions_from_frames, AnalogStick, InputFrame, InputState, InputTransition,
    KeyboardState, MouseState, RawGamepadInput, RawHostInput, RetroPadButtons, Triggers,
};
pub use manifest::{
    CaptureProvenance, CaptureSourceKind, CoreInfo, ExternalAudioSource, ExternalCaptureInfo,
    ExternalCaptureKind, ExternalCaptureStatus, ExternalVideoClock, FrameMapEntry, FrameMapInfo,
    ManifestFormat, PauseSegment, RomInfo, SessionManifest, TimingInfo, TrackAlignment,
    TrackAlignmentMetadata, TrackAlignmentStatus, TrackPresence, VideoTiming, VideoTimingKind,
};
pub use transcription::{
    TranscriptDocument, TranscriptSegment, TranscriptionConfig, TranscriptionJobState,
    TranscriptionModelDescriptor, TranscriptionProvider, TranscriptionWorkerEvent,
    TranscriptionWorkerRequest, MAX_TRANSCRIPTION_MESSAGE_BYTES, TRANSCRIPTION_SCHEMA_VERSION,
};

/// A decoded video frame in RGBA8 (row-major, top-to-bottom).
///
/// This is the source-agnostic video frame type: a libretro core's decoded
/// pixel buffer and a ScreenCaptureKit capture of a Wine process both produce
/// the same `VideoFrame`. `libretro_host` re-exports this as `Frame` so
/// existing code is unaffected; non-libretro capture paths (Steam) depend only
/// on `retrofeel-types` and avoid coupling to `libretro-host`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA8 bytes, `width * height * 4` long.
    pub rgba: Vec<u8>,
}
pub use system::{
    preferred_core_slug, system_by_id, system_for_core_name, system_for_extension, system_for_rom,
    System, SYSTEMS,
};
pub use worker::{
    WorkerCommand, WorkerErrorKind, WorkerEvent, WorkerInput, WorkerLaunch, WorkerReady,
    WorkerRequest, MAX_WORKER_MESSAGE_BYTES, WORKER_PROTOCOL_VERSION,
};

/// RetroPad digital button bitmask. Matches libretro's `RETRO_DEVICE_ID_JOYPAD_*` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RetroPadButtonBits(pub u16);

impl RetroPadButtonBits {
    pub const EMPTY: Self = Self(0);
    pub fn set(&mut self, bit: u16) {
        self.0 |= 1 << bit;
    }
    pub fn clear(&mut self, bit: u16) {
        self.0 &= !(1 << bit);
    }
    pub fn has(&self, bit: u16) -> bool {
        (self.0 & (1 << bit)) != 0
    }
}

/// libretro device ids used by retrofeel.
pub mod device_ids {
    pub const DEVICE_NONE: u32 = 0;
    pub const DEVICE_JOYPAD: u32 = 1;
    pub const DEVICE_MOUSE: u32 = 2;
    pub const DEVICE_KEYBOARD: u32 = 3;
    pub const DEVICE_ANALOG: u32 = 5;

    pub mod joypad {
        pub const B: u16 = 0;
        pub const Y: u16 = 1;
        pub const SELECT: u16 = 2;
        pub const START: u16 = 3;
        pub const UP: u16 = 4;
        pub const DOWN: u16 = 5;
        pub const LEFT: u16 = 6;
        pub const RIGHT: u16 = 7;
        pub const A: u16 = 8;
        pub const X: u16 = 9;
        pub const L: u16 = 10;
        pub const R: u16 = 11;
        pub const L2: u16 = 12;
        pub const R2: u16 = 13;
        pub const L3: u16 = 14;
        pub const R3: u16 = 15;
    }
}
