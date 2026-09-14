//! Versioned protocol shared by the GUI and the isolated libretro worker.
//!
//! The transport is deliberately independent of Bevy and `libretro-host` so a
//! worker can reject an incompatible GUI before loading native core code.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{InputState, RawHostInput, RetroFeelConfig, VideoFrame};

/// Increment whenever a wire-incompatible protocol change is made.
pub const WORKER_PROTOCOL_VERSION: u32 = 2;

/// Hard cap for one length-prefixed message. This bounds allocations when a
/// worker is corrupt, compromised, or simply speaking a different protocol.
pub const MAX_WORKER_MESSAGE_BYTES: u64 = 80 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLaunch {
    pub protocol_version: u32,
    pub authentication_token: [u8; 32],
    pub core_path: PathBuf,
    pub rom_path: Option<PathBuf>,
    pub system_dir: PathBuf,
    pub config: Option<RetroFeelConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerInput {
    pub mapped: InputState,
    pub raw_host: RawHostInput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerCommand {
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
    SetFastForward(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerRequest {
    Launch(Box<WorkerLaunch>),
    Input(WorkerInput),
    Command(WorkerCommand),
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReady {
    pub core_name: String,
    pub core_version: String,
    pub fps: f64,
    pub sample_rate: f64,
    pub base_width: u32,
    pub base_height: u32,
    /// The GUI does not leave the library until this is a real decoded frame.
    pub first_frame: VideoFrame,
    pub first_audio: Vec<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerErrorKind {
    Protocol,
    Authentication,
    Core,
    Bios,
    Content,
    Io,
    ResourceLimit,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerEvent {
    Ready(WorkerReady),
    Frame {
        frame: Option<VideoFrame>,
        audio: Vec<i16>,
    },
    Status(String),
    Error {
        kind: WorkerErrorKind,
        message: String,
    },
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_protocol_round_trips() {
        let request = WorkerRequest::Launch(Box::new(WorkerLaunch {
            protocol_version: WORKER_PROTOCOL_VERSION,
            authentication_token: [7; 32],
            core_path: "core.so".into(),
            rom_path: Some("game.rom".into()),
            system_dir: "system".into(),
            config: Some(RetroFeelConfig::default()),
        }));

        let bytes = bincode::serialize(&request).unwrap();
        let decoded: WorkerRequest = bincode::deserialize(&bytes).unwrap();
        let WorkerRequest::Launch(decoded) = decoded else {
            panic!("wrong request variant");
        };
        assert_eq!(decoded.protocol_version, WORKER_PROTOCOL_VERSION);
        assert_eq!(decoded.authentication_token, [7; 32]);
        assert_eq!(decoded.rom_path, Some(PathBuf::from("game.rom")));
    }
}
