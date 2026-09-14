//! Engine emitters over the `retrofeel-types` recording IR.

use std::path::{Path, PathBuf};

use retrofeel_types::{InputBindingSet, InputFrame, MouseState, RawHostInput, SessionManifest};
use serde::Serialize;
use thiserror::Error;

pub mod bevy;
pub mod godot;
pub mod unity;
pub mod unreal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Bevy,
    Unity,
    Godot,
    Unreal,
}

impl Engine {
    pub fn extension(self) -> &'static str {
        match self {
            Engine::Bevy => "ron",
            Engine::Unity | Engine::Godot | Engine::Unreal => "json",
        }
    }

    pub fn file_stem(self) -> &'static str {
        match self {
            Engine::Bevy => "bevy",
            Engine::Unity => "unity",
            Engine::Godot => "godot",
            Engine::Unreal => "unreal",
        }
    }
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse manifest {path}: {source}")]
    ManifestJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to parse input log {path}: {source}")]
    InputJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize export: {0}")]
    Serialize(String),
}

#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub dir: PathBuf,
    pub manifest: SessionManifest,
    pub frames: Vec<InputFrame>,
}

#[derive(Debug, Clone)]
pub struct ExportedFile {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum YAxisConvention {
    PositiveDown,
    PositiveUp,
}

impl YAxisConvention {
    pub fn label(self) -> &'static str {
        match self {
            YAxisConvention::PositiveDown => "positive-down",
            YAxisConvention::PositiveUp => "positive-up",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportMetadata {
    pub core_name: String,
    pub core_version: String,
    pub fps: f64,
    pub frame_count: u64,
    pub source_y_axis: &'static str,
    pub export_y_axis: &'static str,
    pub binding_map: Option<InputBindingSet>,
}

pub fn load_session(session_dir: impl AsRef<Path>) -> Result<LoadedSession, ExportError> {
    let dir = session_dir.as_ref().to_path_buf();
    let manifest_path = dir.join("manifest.json");
    let manifest_text = read_to_string(&manifest_path)?;
    let manifest: SessionManifest =
        serde_json::from_str(&manifest_text).map_err(|source| ExportError::ManifestJson {
            path: manifest_path.clone(),
            source,
        })?;
    let input_path = resolve_session_path(&dir, &manifest.input_log);
    let input_text = read_to_string(&input_path)?;
    let frames: Vec<InputFrame> =
        serde_json::from_str(&input_text).map_err(|source| ExportError::InputJson {
            path: input_path,
            source,
        })?;
    Ok(LoadedSession {
        dir,
        manifest,
        frames,
    })
}

pub fn export_session(
    engine: Engine,
    session_dir: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
) -> Result<ExportedFile, ExportError> {
    let session = load_session(session_dir)?;
    std::fs::create_dir_all(out_dir.as_ref()).map_err(|source| ExportError::Io {
        path: out_dir.as_ref().to_path_buf(),
        source,
    })?;
    let output = match engine {
        Engine::Bevy => bevy::emit(&session)?,
        Engine::Unity => unity::emit(&session)?,
        Engine::Godot => godot::emit(&session)?,
        Engine::Unreal => unreal::emit(&session)?,
    };
    let path = out_dir
        .as_ref()
        .join(format!("{}.{}", engine.file_stem(), engine.extension()));
    std::fs::write(&path, output).map_err(|source| ExportError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(ExportedFile { path })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NormalizedFrame {
    pub frame: u64,
    pub seconds: f64,
    pub port: u8,
    pub buttons: Vec<&'static str>,
    pub left_stick: [f32; 2],
    pub right_stick: [f32; 2],
    pub triggers: [f32; 2],
    pub mouse: MouseState,
    pub keyboard: Vec<i32>,
    pub raw_host: Option<RawHostInput>,
}

pub fn export_metadata(session: &LoadedSession, y_axis: YAxisConvention) -> ExportMetadata {
    ExportMetadata {
        core_name: session.manifest.core.name.clone(),
        core_version: session.manifest.core.version.clone(),
        fps: session.manifest.timing.fps,
        frame_count: session.manifest.frame_count,
        source_y_axis: YAxisConvention::PositiveDown.label(),
        export_y_axis: y_axis.label(),
        binding_map: session.manifest.binding_map.clone(),
    }
}

pub fn normalized_frames(session: &LoadedSession) -> Vec<NormalizedFrame> {
    normalized_frames_for(session, YAxisConvention::PositiveDown)
}

pub fn normalized_frames_for(
    session: &LoadedSession,
    y_axis: YAxisConvention,
) -> Vec<NormalizedFrame> {
    let fps = session.manifest.timing.fps.max(1.0);
    session
        .frames
        .iter()
        .map(|frame| NormalizedFrame {
            frame: frame.frame,
            seconds: frame
                .elapsed_us
                .map(|elapsed| elapsed as f64 / 1_000_000.0)
                .unwrap_or_else(|| frame.frame as f64 / fps),
            port: frame.port,
            buttons: button_names(frame.state.buttons.0),
            left_stick: normalize_stick_for(frame.state.analog_l.x, frame.state.analog_l.y, y_axis),
            right_stick: normalize_stick_for(
                frame.state.analog_r.x,
                frame.state.analog_r.y,
                y_axis,
            ),
            triggers: [
                normalize_trigger(frame.state.triggers.l),
                normalize_trigger(frame.state.triggers.r),
            ],
            mouse: frame.state.mouse,
            keyboard: keyboard_codes(frame),
            raw_host: frame.raw_host.clone(),
        })
        .collect()
}

pub fn button_names(bits: u16) -> Vec<&'static str> {
    const NAMES: [&str; 16] = [
        "B", "Y", "Select", "Start", "Up", "Down", "Left", "Right", "A", "X", "L", "R", "L2", "R2",
        "L3", "R3",
    ];
    NAMES
        .iter()
        .enumerate()
        .filter_map(|(bit, name)| {
            if bits & (1 << bit) != 0 {
                Some(*name)
            } else {
                None
            }
        })
        .collect()
}

pub fn normalize_stick(x: i16, y: i16) -> [f32; 2] {
    [normalize_axis(x), normalize_axis(y)]
}

pub fn normalize_stick_for(x: i16, y: i16, y_axis: YAxisConvention) -> [f32; 2] {
    let y = normalize_axis(y);
    [
        normalize_axis(x),
        match y_axis {
            YAxisConvention::PositiveDown => y,
            YAxisConvention::PositiveUp => -y,
        },
    ]
}

pub fn normalize_axis(value: i16) -> f32 {
    (value as f32 / 0x7fff as f32).clamp(-1.0, 1.0)
}

pub fn normalize_trigger(value: i16) -> f32 {
    (value.max(0) as f32 / 0x7fff as f32).clamp(0.0, 1.0)
}

pub fn serialize_json<T: Serialize>(value: &T) -> Result<String, ExportError> {
    serde_json::to_string_pretty(value).map_err(|error| ExportError::Serialize(error.to_string()))
}

pub fn serialize_ron<T: Serialize>(value: &T) -> Result<String, ExportError> {
    ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::default())
        .map_err(|error| ExportError::Serialize(error.to_string()))
}

fn keyboard_codes(frame: &InputFrame) -> Vec<i32> {
    frame.state.keyboard.keys[..frame.state.keyboard.count as usize].to_vec()
}

fn read_to_string(path: &Path) -> Result<String, ExportError> {
    std::fs::read_to_string(path).map_err(|source| ExportError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_session_path(session_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() || path.exists() {
        return path;
    }
    if let Some(name) = path.file_name() {
        return session_dir.join(name);
    }
    session_dir.join(value)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use retrofeel_types::{
        input::Triggers, AnalogStick, InputFrame, InputState, RawHostInput, RetroPadButtonBits,
        TimingInfo,
    };
    use std::collections::BTreeMap;

    pub fn fixture_session() -> LoadedSession {
        let mut buttons = RetroPadButtonBits::EMPTY;
        buttons.set(retrofeel_types::device_ids::joypad::A);
        buttons.set(retrofeel_types::device_ids::joypad::RIGHT);
        let mut raw_axes = BTreeMap::new();
        raw_axes.insert("LeftStickX".into(), 0.25);
        raw_axes.insert("LeftStickY".into(), 0.5);
        LoadedSession {
            dir: PathBuf::from("fixture"),
            manifest: SessionManifest {
                core: retrofeel_types::CoreInfo {
                    name: "mock-core".into(),
                    version: "0.1.0".into(),
                    library_path: "mock".into(),
                },
                rom: None,
                timing: TimingInfo {
                    fps: 60.0,
                    sample_rate: 44100.0,
                    start_timestamp: 0.0,
                },
                initial_state: None,
                frame_count: 2,
                pause_segments: Vec::new(),
                input_log: "input.json".into(),
                video: None,
                mic_audio: None,
                transcript: None,
                transcript_json: None,
                transcription_status: Default::default(),
                transcription_provider: None,
                transcription_model: None,
                binding_map: None,
                capture_provenance: None,
                video_timing: None,
                frame_map: None,
                input_transitions: None,
                track_alignment: None,
                external_capture: None,
                dropped_frames: 0,
                format: retrofeel_types::ManifestFormat::Json,
            },
            frames: vec![
                InputFrame {
                    frame: 0,
                    elapsed_us: None,
                    port: 0,
                    state: InputState {
                        buttons,
                        analog_l: AnalogStick {
                            x: 0x4000,
                            y: -0x4000,
                        },
                        triggers: Triggers {
                            l: 0x2000,
                            r: 0x7fff,
                        },
                        ..Default::default()
                    },
                    raw_host: Some(RawHostInput {
                        keyboard_keys: vec!["KeyZ".into()],
                        keyboard_key_codes: Vec::new(),
                        gamepad_buttons: vec!["South".into()],
                        gamepad_axes: raw_axes,
                        gamepads: Vec::new(),
                        mouse: None,
                    }),
                },
                InputFrame {
                    frame: 1,
                    elapsed_us: None,
                    port: 0,
                    state: InputState::default(),
                    raw_host: None,
                },
            ],
        }
    }

    #[test]
    fn normalizes_frame_values() {
        let frames = normalized_frames(&fixture_session());
        assert_eq!(frames[0].buttons, vec!["Right", "A"]);
        assert!((frames[0].seconds - 0.0).abs() < f64::EPSILON);
        assert!(frames[0].left_stick[0] > 0.49);
        assert!(frames[0].left_stick[1] < -0.49);
        assert!(frames[0].triggers[0] > 0.24);
        assert_eq!(
            normalized_frames_for(&fixture_session(), YAxisConvention::PositiveUp)[0].left_stick[1],
            -frames[0].left_stick[1]
        );
        assert_eq!(
            frames[0].raw_host.as_ref().unwrap().keyboard_keys,
            vec!["KeyZ"]
        );
    }

    #[test]
    fn external_capture_uses_exact_elapsed_time_instead_of_average_fps() {
        let mut session = fixture_session();
        session.frames[1].elapsed_us = Some(19_750);
        let frames = normalized_frames(&session);
        assert!((frames[1].seconds - 0.019_75).abs() < f64::EPSILON);
    }
}
