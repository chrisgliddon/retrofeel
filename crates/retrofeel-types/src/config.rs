//! Configuration schema and RON persistence helpers.
//!
//! `RetroFeelConfig` is the disk contract shared by the app, backend, and
//! exporters. Runtime code may layer richer behavior on top, but the persisted
//! shape stays serde-friendly and explicit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::TranscriptionConfig;

const CONFIG_FILE_NAME: &str = "retrofeel.ron";
const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "retrofeel";
const APPLICATION: &str = "retrofeel";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not resolve a platform config directory")]
    NoProjectDirs,
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse RON config {path}: {source}")]
    ParseRon {
        path: PathBuf,
        #[source]
        source: ron::error::SpannedError,
    },
    #[error("failed to serialize config as RON: {0}")]
    SerializeRon(#[from] ron::Error),
    #[error("failed to write config {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RetroFeelConfig {
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub library: LibraryConfig,
    #[serde(default)]
    pub metadata: MetadataConfig,
    #[serde(default)]
    pub rewind: RewindConfig,
    #[serde(default)]
    pub window_behavior: WindowBehaviorConfig,
    #[serde(default)]
    pub shaders: ShaderConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    /// Per-core option overrides keyed by libretro library name.
    #[serde(default)]
    pub core_options: BTreeMap<String, BTreeMap<String, String>>,
    /// ROM extension -> core path override. Extensions are stored lowercase and
    /// without a leading dot (`sfc`, not `.sfc`).
    #[serde(default)]
    pub core_overrides_by_extension: BTreeMap<String, PathBuf>,
    /// Global input binding defaults used when a core has no specific override.
    #[serde(default)]
    pub global_input_bindings: InputBindingSet,
    /// Per-core input bindings keyed by libretro library name.
    #[serde(default, alias = "input_bindings")]
    pub per_core_input_bindings: BTreeMap<String, InputBindingSet>,
    /// Most recently launched ROMs, newest first.
    #[serde(default)]
    pub recent_roms: Vec<PathBuf>,
    /// Steam game library + capture configuration.
    #[serde(default)]
    pub steam: SteamConfig,
}

/// Persisted choices that affect the application chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AppearanceConfig {
    #[serde(default)]
    pub theme: ThemePreference,
}

/// How RetroFeel resolves its light/dark application palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemePreference {
    /// Follow the primary window's native operating-system theme.
    #[default]
    System,
    Light,
    Dark,
}

/// Whether imported content remains at its original path or is copied into a
/// user-selected managed library. Referencing in place is intentionally the
/// default so an import never mutates or duplicates a collection implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RomImportMode {
    #[default]
    Reference,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryConfig {
    #[serde(default)]
    pub import_mode: RomImportMode,
    #[serde(default)]
    pub managed_library_dir: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub show_canonical_titles: bool,
    #[serde(default = "default_grid_size")]
    pub grid_size: u16,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            import_mode: RomImportMode::Reference,
            managed_library_dir: None,
            show_canonical_titles: true,
            grid_size: default_grid_size(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataConfig {
    #[serde(default = "default_true")]
    pub online_enabled: bool,
    #[serde(default)]
    pub openvgdb_path: Option<PathBuf>,
    #[serde(default)]
    pub libretro_database_dir: Option<PathBuf>,
    /// Changes when provider data changes, invalidating negative artwork
    /// results without deleting successful cached covers.
    #[serde(default)]
    pub provider_revision: u64,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            online_enabled: true,
            openvgdb_path: None,
            libretro_database_dir: None,
            provider_revision: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rewind_seconds")]
    pub seconds: u16,
    #[serde(default = "default_rewind_memory_mib")]
    pub memory_limit_mib: u16,
}

impl Default for RewindConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            seconds: default_rewind_seconds(),
            memory_limit_mib: default_rewind_memory_mib(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBehaviorConfig {
    #[serde(default)]
    pub popout_gameplay: bool,
    #[serde(default)]
    pub always_on_top: bool,
    #[serde(default = "default_true")]
    pub pause_on_focus_loss: bool,
    #[serde(default)]
    pub background_controller_input: bool,
}

impl Default for WindowBehaviorConfig {
    fn default() -> Self {
        Self {
            popout_gameplay: false,
            always_on_top: false,
            pause_on_focus_loss: true,
            background_controller_input: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub global_preset: Option<PathBuf>,
    #[serde(default)]
    pub per_system_presets: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub per_game_presets: BTreeMap<String, PathBuf>,
    #[serde(default = "default_shader_max_passes")]
    pub max_passes: u8,
    #[serde(default = "default_shader_max_texture_size")]
    pub max_texture_size: u16,
}

impl Default for ShaderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            global_preset: None,
            per_system_presets: BTreeMap::new(),
            per_game_presets: BTreeMap::new(),
            max_passes: default_shader_max_passes(),
            max_texture_size: default_shader_max_texture_size(),
        }
    }
}

impl RetroFeelConfig {
    pub fn project_dirs() -> Result<ProjectDirs, ConfigError> {
        ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).ok_or(ConfigError::NoProjectDirs)
    }

    pub fn platform_config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::project_dirs()?.config_dir().join(CONFIG_FILE_NAME))
    }

    /// Build a config whose content paths live under `data_base`.
    ///
    /// Tests and debug tooling can use this to avoid touching platform dirs.
    pub fn with_data_base(data_base: impl AsRef<Path>) -> Self {
        let base = data_base.as_ref();
        Self {
            paths: PathsConfig {
                cores: base.join("cores"),
                system: base.join("system"),
                roms: vec![base.join("roms")],
                saves: base.join("saves"),
                states: base.join("states"),
                recordings: base.join("recordings"),
            },
            ..Default::default()
        }
    }

    pub fn platform_default() -> Result<Self, ConfigError> {
        let dirs = Self::project_dirs()?;
        Ok(Self::with_data_base(dirs.data_dir()))
    }

    pub fn load_or_create_platform() -> Result<Self, ConfigError> {
        let path = Self::platform_config_path()?;
        Self::load_or_create(path)
    }

    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        if path.exists() {
            return Self::load_from_path(path);
        }
        let config = Self::platform_default().unwrap_or_default();
        config.save_to_path(path)?;
        Ok(config)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Self = ron::from_str(&text).map_err(|source| ConfigError::ParseRon {
            path: path.to_path_buf(),
            source,
        })?;
        config.normalize_legacy();
        Ok(config)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            create_dir(parent)?;
        }
        let pretty = ron::ser::PrettyConfig::default();
        let text = ron::ser::to_string_pretty(self, pretty)?;
        std::fs::write(path, text).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn ensure_dirs(&self) -> Result<(), ConfigError> {
        create_dir(&self.paths.cores)?;
        create_dir(&self.paths.system)?;
        create_dir(&self.paths.saves)?;
        create_dir(&self.paths.states)?;
        create_dir(&self.paths.recordings)?;
        for rom_dir in &self.paths.roms {
            create_dir(rom_dir)?;
        }
        Ok(())
    }

    /// Migrate additive legacy fields after deserialization without changing
    /// the accepted RON/JSON shape.
    pub fn normalize_legacy(&mut self) {
        self.recording.normalize_legacy();
    }

    pub fn core_options_for(&self, core_key: &str) -> BTreeMap<String, String> {
        self.core_options.get(core_key).cloned().unwrap_or_default()
    }

    pub fn input_bindings_for(&self, core_key: &str) -> InputBindingSet {
        self.per_core_input_bindings
            .get(core_key)
            .cloned()
            .unwrap_or_else(|| self.global_input_bindings.clone())
    }

    pub fn core_override_for_rom(&self, rom_path: impl AsRef<Path>) -> Option<&PathBuf> {
        let ext = normalize_extension(rom_path.as_ref().extension()?.to_string_lossy());
        self.core_overrides_by_extension.get(&ext)
    }

    pub fn set_core_override_for_extension(
        &mut self,
        extension: impl AsRef<str>,
        core_path: impl Into<PathBuf>,
    ) {
        self.core_overrides_by_extension
            .insert(normalize_extension(extension.as_ref()), core_path.into());
    }

    pub fn push_recent_rom(&mut self, rom_path: impl Into<PathBuf>) {
        let rom_path = rom_path.into();
        self.recent_roms.retain(|existing| existing != &rom_path);
        self.recent_roms.insert(0, rom_path);
        self.recent_roms.truncate(10);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathsConfig {
    pub cores: PathBuf,
    pub system: PathBuf,
    pub roms: Vec<PathBuf>,
    pub saves: PathBuf,
    pub states: PathBuf,
    pub recordings: PathBuf,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            cores: PathBuf::from("cores"),
            system: PathBuf::from("system"),
            roms: vec![PathBuf::from("roms")],
            saves: PathBuf::from("saves"),
            states: PathBuf::from("states"),
            recordings: PathBuf::from("recordings"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoConfig {
    #[serde(default = "default_true")]
    pub integer_scaling: bool,
    #[serde(default = "default_true")]
    pub aspect_correction: bool,
    #[serde(default)]
    pub fullscreen: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            integer_scaling: true,
            aspect_correction: true,
            fullscreen: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default = "default_audio_latency_ms")]
    pub latency_ms: u32,
    /// Stable cpal device name. `None` follows the operating-system default.
    #[serde(default)]
    pub output_device: Option<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 1.0,
            latency_ms: 80,
            output_device: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Capture microphone audio to `mic.wav` alongside recording sessions.
    /// Best-effort: a missing/denied input device logs a warning and the
    /// recording proceeds without a mic track.
    #[serde(default = "default_true")]
    pub mic_enabled: bool,
    /// Path to a whisper.cpp GGML/GGUF model. When set and `whisper-cli` is on
    /// PATH, mic audio is transcribed to `transcript.srt` after recording
    /// stops. When unset, the openai-whisper CLI (`whisper`) is tried instead.
    #[serde(default)]
    pub whisper_model: Option<PathBuf>,
    /// Local, transcription-first recording behavior. The legacy
    /// `whisper_model` value is copied into `external_model` on load.
    #[serde(default)]
    pub transcription: TranscriptionConfig,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            mic_enabled: true,
            whisper_model: None,
            transcription: TranscriptionConfig::default(),
        }
    }
}

impl RecordingConfig {
    pub fn normalize_legacy(&mut self) {
        if self.transcription.external_model.is_none() {
            if let Some(model) = self.whisper_model.clone() {
                self.transcription.external_model = Some(model);
                self.transcription.provider = crate::TranscriptionProvider::WhisperCpp;
            }
        }
    }
}

/// A Steam game entry in the user's library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamGameEntry {
    pub app_id: u32,
    pub name: String,
    /// Install directory (e.g. ~/GameHub/steamapps/common/Meadow of Lanterns).
    pub install_dir: PathBuf,
    /// Wine prefix directory (e.g. ~/FoM-wine).
    pub wine_prefix: PathBuf,
    /// The .exe filename inside install_dir to launch.
    pub exe_name: String,
    /// Where this installation was discovered. Existing configs deserialize
    /// as `Configured` so their explicit GameHub/Wine launch metadata wins.
    #[serde(default)]
    pub source: SteamGameSource,
}

/// Per-install launch semantics for a mixed Steam library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SteamGameSource {
    /// Explicit legacy/configured entry; follows [`SteamConfig::launch_mode`].
    #[default]
    Configured,
    /// Discovered in GameHub's Steam library; preserves attach-first behavior.
    GameHub,
    /// Discovered from the platform Steam client's installed manifests.
    NativeSteam,
}

/// How to launch a Steam game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SteamLaunchMode {
    /// Launch wine directly with WINEPREFIX.
    #[default]
    WineDirect,
    /// Launch through GameHub (discover the spawned Wine PID).
    GameHub,
}

/// Steam game library + capture configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SteamConfig {
    /// Path to the wine binary (default /opt/homebrew/bin/wine).
    #[serde(default = "default_wine_bin")]
    pub wine_bin: PathBuf,
    /// Default Wine prefix if a game entry doesn't specify one.
    #[serde(default)]
    pub default_wine_prefix: Option<PathBuf>,
    /// How to launch games.
    #[serde(default)]
    pub launch_mode: SteamLaunchMode,
    /// Discovered/added Steam games.
    #[serde(default)]
    pub games: Vec<SteamGameEntry>,
    /// Max capture width (downscaled if larger).
    #[serde(default = "default_capture_max_width")]
    pub capture_max_width: u32,
    /// Max capture height.
    #[serde(default = "default_capture_max_height")]
    pub capture_max_height: u32,
    /// Target capture frame rate.
    ///
    /// For ScreenCaptureKit this is the encoded CFR grid, not a promise about
    /// source callback cadence.
    #[serde(default = "default_capture_fps")]
    pub capture_fps: u32,
    /// Whether the non-activating local recording-status overlay is shown for
    /// Steam/GameHub/native-Steam sessions.
    #[serde(default = "default_true")]
    pub overlay_visible: bool,
    /// How long RetroFeel waits for an exact native Steam game window after
    /// opening `steam://run/<appid>`. Steam itself remains entirely owned by
    /// Steam; this only bounds RetroFeel's attach attempt.
    #[serde(default = "default_native_attach_timeout_seconds")]
    pub native_attach_timeout_seconds: u32,
    /// GameHub steamapps directory (for auto-discovery).
    #[serde(default = "default_gamehub_steamapps")]
    pub gamehub_steamapps: PathBuf,
}

impl Default for SteamConfig {
    fn default() -> Self {
        Self {
            wine_bin: PathBuf::from("/opt/homebrew/bin/wine"),
            default_wine_prefix: None,
            launch_mode: SteamLaunchMode::default(),
            games: Vec::new(),
            capture_max_width: 1920,
            capture_max_height: 1080,
            capture_fps: 60,
            overlay_visible: true,
            native_attach_timeout_seconds: 30,
            gamehub_steamapps: dirs_helper::home_dir()
                .unwrap_or_default()
                .join("GameHub/steamapps"),
        }
    }
}

fn default_wine_bin() -> PathBuf {
    PathBuf::from("/opt/homebrew/bin/wine")
}

fn default_capture_max_width() -> u32 {
    1920
}

fn default_capture_max_height() -> u32 {
    1080
}

fn default_capture_fps() -> u32 {
    60
}

fn default_native_attach_timeout_seconds() -> u32 {
    30
}

fn default_gamehub_steamapps() -> PathBuf {
    dirs_helper::home_dir()
        .unwrap_or_default()
        .join("GameHub/steamapps")
}

/// Minimal home-dir helper to avoid pulling in `dirs` just for the default.
mod dirs_helper {
    use std::path::PathBuf;
    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn default_true() -> bool {
    true
}

fn default_volume() -> f32 {
    1.0
}

fn default_audio_latency_ms() -> u32 {
    80
}

fn default_grid_size() -> u16 {
    180
}

fn default_rewind_seconds() -> u16 {
    10
}

fn default_rewind_memory_mib() -> u16 {
    128
}

fn default_shader_max_passes() -> u8 {
    16
}

fn default_shader_max_texture_size() -> u16 {
    4096
}

/// Persisted input binding maps. Phase 4 will provide editing UI; Phase 3 only
/// needs a durable schema for global and per-core overrides.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputBindingSet {
    /// RetroPad control name -> host keyboard code.
    #[serde(default)]
    pub keyboard: BTreeMap<String, String>,
    /// RetroPad control name -> host gamepad button/axis.
    #[serde(default)]
    pub gamepad: BTreeMap<String, String>,
    /// RetroPad control name -> host mouse input.
    #[serde(default)]
    pub mouse: BTreeMap<String, String>,
}

fn create_dir(path: &Path) -> Result<(), ConfigError> {
    std::fs::create_dir_all(path).map_err(|source| ConfigError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}

fn normalize_extension(ext: impl AsRef<str>) -> String {
    ext.as_ref().trim_start_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_as_ron() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("retrofeel.ron");

        let mut config = RetroFeelConfig::with_data_base(temp.path().join("data"));
        config.appearance.theme = ThemePreference::Light;
        config
            .core_options
            .entry("mock-core".into())
            .or_default()
            .insert("mock_palette".into(), "green".into());
        config.set_core_override_for_extension(".ROM", temp.path().join("cores/mock.so"));
        config
            .global_input_bindings
            .keyboard
            .insert("Start".into(), "Enter".into());

        config.save_to_path(&path).unwrap();
        let loaded = RetroFeelConfig::load_from_path(&path).unwrap();

        assert_eq!(loaded, config);
        assert_eq!(
            loaded.core_override_for_rom("game.rom").unwrap(),
            &temp.path().join("cores/mock.so")
        );
        assert_eq!(
            loaded.core_options_for("mock-core").get("mock_palette"),
            Some(&"green".to_string())
        );
    }

    #[test]
    fn missing_appearance_defaults_to_system() {
        let config: RetroFeelConfig = ron::from_str("(paths: (cores: \"cores\", system: \"system\", roms: [\"roms\"], saves: \"saves\", states: \"states\", recordings: \"recordings\"))").unwrap();

        assert_eq!(config.appearance.theme, ThemePreference::System);
    }

    #[test]
    fn legacy_whisper_model_migrates_into_transcription_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retrofeel.ron");
        std::fs::write(
            &path,
            "(recording: (mic_enabled: true, whisper_model: Some(\"models/ggml.bin\")))",
        )
        .unwrap();
        let config = RetroFeelConfig::load_from_path(path).unwrap();
        assert_eq!(
            config.recording.transcription.external_model,
            Some(PathBuf::from("models/ggml.bin"))
        );
        assert_eq!(
            config.recording.transcription.provider,
            crate::TranscriptionProvider::WhisperCpp
        );
        assert!(config.recording.transcription.automatic);
    }

    #[test]
    fn legacy_steam_game_defaults_to_configured_source() {
        let game: SteamGameEntry = ron::from_str(
            r#"(
                app_id: 42,
                name: "Legacy game",
                install_dir: "/games/legacy",
                wine_prefix: "/prefix",
                exe_name: "legacy.exe",
            )"#,
        )
        .unwrap();

        assert_eq!(game.source, SteamGameSource::Configured);
    }

    #[test]
    fn every_theme_preference_round_trips_as_ron() {
        for preference in [
            ThemePreference::System,
            ThemePreference::Light,
            ThemePreference::Dark,
        ] {
            let mut config = RetroFeelConfig::default();
            config.appearance.theme = preference;
            let serialized = ron::to_string(&config).unwrap();
            let loaded: RetroFeelConfig = ron::from_str(&serialized).unwrap();
            assert_eq!(loaded.appearance.theme, preference);
        }
    }

    #[test]
    fn portable_feature_defaults_are_safe_and_reference_first() {
        let config = RetroFeelConfig::default();
        assert_eq!(config.library.import_mode, RomImportMode::Reference);
        assert!(config.metadata.online_enabled);
        assert!(config.window_behavior.pause_on_focus_loss);
        assert!(!config.rewind.enabled);
        assert_eq!(config.rewind.memory_limit_mib, 128);
        assert_eq!(config.shaders.max_passes, 16);
        assert_eq!(config.audio.output_device, None);
    }

    #[test]
    fn ensure_dirs_creates_all_configured_paths() {
        let temp = tempfile::tempdir().unwrap();
        let config = RetroFeelConfig::with_data_base(temp.path().join("data"));

        config.ensure_dirs().unwrap();

        assert!(config.paths.cores.is_dir());
        assert!(config.paths.system.is_dir());
        assert!(config.paths.roms[0].is_dir());
        assert!(config.paths.saves.is_dir());
        assert!(config.paths.states.is_dir());
        assert!(config.paths.recordings.is_dir());
    }

    #[test]
    fn recent_roms_are_newest_first_and_unique() {
        let mut config = RetroFeelConfig::default();

        config.push_recent_rom("roms/first.sfc");
        config.push_recent_rom("roms/second.sfc");
        config.push_recent_rom("roms/first.sfc");

        assert_eq!(
            config.recent_roms,
            vec![
                PathBuf::from("roms/first.sfc"),
                PathBuf::from("roms/second.sfc")
            ]
        );
    }
}
