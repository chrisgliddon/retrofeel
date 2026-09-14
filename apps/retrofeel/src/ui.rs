use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use accesskit::{Node as AccessNode, Role};
use bevy::a11y::AccessibilityNode;
use bevy::input_focus::{
    tab_navigation::{TabGroup, TabIndex},
    InputFocus, InputFocusVisible,
};
use bevy::prelude::*;
use retrofeel_backend::{
    parse_core_variable, scan_system_dir, BiosReport, BiosStatus, CoreCatalogEntry,
    CoreInstallStatus, CoreRegistry, CoreScanReport,
};
use retrofeel_feel::{
    AgentAdapterKind, AlignmentReport, AnalysisResultV2, FeelManifestV1, FeelPackage,
};
use retrofeel_types::{
    system_by_id, system_for_rom, CaptureSourceKind, InputBindingSet, InputFrame, InputTransition,
    RetroFeelConfig, SessionManifest, SteamGameSource, System, ThemePreference,
    TrackAlignmentStatus, TrackPresence, TranscriptDocument, TranscriptionJobState, SYSTEMS,
};

use crate::diagnostics;
use crate::icons::{Icon, IconAssets, ICON_PX};
use crate::input::{default_gamepad_bindings, default_input_bindings, keyboard_controls};
use crate::theme::{self, ButtonVariant};
use crate::theme::{
    PALETTE_ACCENT, PALETTE_BAD, PALETTE_BG, PALETTE_BUTTON_ACTIVE, PALETTE_EMPTY, PALETTE_GOOD,
    PALETTE_INFO, PALETTE_LINE, PALETTE_LINE_DARK, PALETTE_MUTED, PALETTE_MUTED_DARK,
    PALETTE_PANEL, PALETTE_POSTER_BORDER, PALETTE_PREF_CARD, PALETTE_ROW, PALETTE_SEARCH,
    PALETTE_SEGMENT_BG, PALETTE_STAR, PALETTE_TEXT, PALETTE_TILE, PALETTE_TOOLBAR, PALETTE_WARN,
};

type UiButtonVisuals<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Ref<'static, Interaction>,
        &'static mut BackgroundColor,
        &'static mut BorderColor,
        &'static mut Button,
        &'static UiButton,
        Option<&'static Selected>,
        Option<&'static Children>,
    ),
    With<UiButton>,
>;

#[derive(Resource)]
pub struct FrontendModel {
    pub config_path: Option<PathBuf>,
    pub config: RetroFeelConfig,
    pub registry: CoreScanReport,
    pub bios: Option<BiosReport>,
    pub roms: Vec<RomEntry>,
    pub recordings: Vec<RecordingEntry>,
    pub selected_recording: Option<PathBuf>,
    pub status: String,
    pub binding_capture: Option<BindingCapture>,
    /// Which library collection/console is currently selected in the sidebar.
    pub library_view: LibraryView,
    /// Text typed into the library search field.
    pub library_query: String,
    /// Box-art grid or dense table display for game collections.
    pub library_display_mode: LibraryDisplayMode,
    /// Top-level library content switcher.
    pub library_top_view: LibraryTopView,
    /// Which top tab is active in the Preferences window.
    pub preferences_section: PreferencesSection,
    /// Downloadable cores discovered from the libretro buildbot catalog.
    pub core_catalog: Vec<CoreCatalogEntry>,
    /// The embedded SQLite database, if open. Normal launches persist config
    /// here; `config_path` is populated only for an explicit CLI-file
    /// override. Cached cores/BIOS/recordings also use this DB.
    pub db: Option<retrofeel_db::Db>,
}

/// The current library filter, driven by the console/collection sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LibraryView {
    /// Every resolvable ROM across all systems.
    #[default]
    AllGames,
    /// The `recent_roms` list, most-recent first.
    RecentlyAdded,
    /// A specific system, keyed by its canonical [`System::id`].
    System(String),
    /// ROMs whose system could not be identified.
    Other,
    /// Steam games discovered from configured, GameHub, and native libraries.
    Steam,
}

impl LibraryView {
    fn log_label(&self) -> String {
        match self {
            Self::AllGames => "all_games".to_string(),
            Self::RecentlyAdded => "recently_added".to_string(),
            Self::System(system_id) => format!("system:{system_id}"),
            Self::Other => "other".to_string(),
            Self::Steam => "steam".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryDisplayMode {
    #[default]
    Grid,
    List,
}

impl LibraryDisplayMode {
    fn icon(self) -> Icon {
        match self {
            Self::Grid => Icon::Grid,
            Self::List => Icon::List,
        }
    }

    fn log_label(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::List => "list",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryTopView {
    #[default]
    Games,
    Recordings,
    Screenshots,
}

impl LibraryTopView {
    const ALL: [Self; 3] = [Self::Games, Self::Recordings, Self::Screenshots];

    fn label(self) -> &'static str {
        match self {
            Self::Games => "Games",
            Self::Recordings => "Recordings",
            Self::Screenshots => "Screenshots",
        }
    }

    fn log_label(self) -> &'static str {
        match self {
            Self::Games => "games",
            Self::Recordings => "recordings",
            Self::Screenshots => "screenshots",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreferencesSection {
    #[default]
    Library,
    Gameplay,
    Transcription,
    Controls,
    Cores,
    SystemFiles,
}

impl PreferencesSection {
    pub const ALL: [Self; 6] = [
        Self::Library,
        Self::Gameplay,
        Self::Transcription,
        Self::Controls,
        Self::Cores,
        Self::SystemFiles,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Gameplay => "Gameplay",
            Self::Transcription => "Transcription",
            Self::Controls => "Controls",
            Self::Cores => "Cores",
            Self::SystemFiles => "System files",
        }
    }

    fn log_label(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Gameplay => "gameplay",
            Self::Transcription => "transcription",
            Self::Controls => "controls",
            Self::Cores => "cores",
            Self::SystemFiles => "system_files",
        }
    }
}

impl FrontendModel {
    pub fn new(config_path: Option<PathBuf>, config: RetroFeelConfig) -> Self {
        Self::new_inner(config_path, config, None)
    }

    /// Construct with a DB handle for the fast launch path. Reads cached
    /// cores/BIOS/ROMs/recordings/catalog from SQLite instead of scanning.
    pub fn new_with_db(
        config_path: Option<PathBuf>,
        fallback_config: RetroFeelConfig,
        db: &retrofeel_db::Db,
    ) -> Self {
        let config = retrofeel_db::ConfigRepo::new(db)
            .load_config()
            .unwrap_or(fallback_config);
        Self::new_inner(config_path, config, Some(db))
    }

    /// Preserve an explicit `--config FILE` override while still using the DB
    /// for cache/index facilities. File overrides never overwrite default DB
    /// configuration on save.
    pub fn new_with_db_file_override(
        config_path: PathBuf,
        config: RetroFeelConfig,
        db: &retrofeel_db::Db,
    ) -> Self {
        Self::new_inner(Some(config_path), config, Some(db))
    }

    fn new_inner(
        config_path: Option<PathBuf>,
        mut config: RetroFeelConfig,
        db: Option<&retrofeel_db::Db>,
    ) -> Self {
        if config.global_input_bindings.keyboard.is_empty()
            && config.global_input_bindings.gamepad.is_empty()
        {
            config.global_input_bindings = default_input_bindings();
        }
        let mut model = Self {
            config_path,
            config,
            registry: CoreScanReport::default(),
            bios: None,
            roms: Vec::new(),
            recordings: Vec::new(),
            selected_recording: None,
            status: String::new(),
            binding_capture: None,
            library_view: LibraryView::default(),
            library_query: String::new(),
            library_display_mode: LibraryDisplayMode::default(),
            library_top_view: LibraryTopView::default(),
            preferences_section: PreferencesSection::default(),
            core_catalog: Vec::new(),
            db: db.cloned(),
        };
        match db {
            Some(db) => model.refresh_with_db(db),
            None => model.refresh(),
        }
        model
    }

    pub fn refresh(&mut self) {
        let refresh_timing = diagnostics::Timing::start("frontend_model.refresh");
        diagnostics::time_block("frontend_model.refresh.ensure_dirs", || {
            if let Err(error) = self.config.ensure_dirs() {
                self.status = format!("Could not create configured directories: {error}");
            }
        });

        let registry = diagnostics::time_block("frontend_model.refresh.scan_cores", || {
            match CoreRegistry::scan_dir(&self.config.paths.cores, &self.config.paths.system) {
                Ok(report) => report,
                Err(error) => {
                    self.status = format!("Could not scan cores: {error}");
                    CoreScanReport::default()
                }
            }
        });
        self.registry = registry;

        let bios =
            diagnostics::time_block(
                "frontend_model.refresh.scan_bios",
                || match scan_system_dir(&self.config.paths.system) {
                    Ok(report) => Some(report),
                    Err(error) => {
                        self.status = format!("Could not scan BIOS files: {error}");
                        None
                    }
                },
            );
        self.bios = bios;

        self.roms = diagnostics::time_block("frontend_model.refresh.scan_roms", || {
            scan_rom_dirs(&self.config, &self.registry)
        });
        self.recordings = diagnostics::time_block("frontend_model.refresh.scan_recordings", || {
            scan_recordings_dir(&self.config.paths.recordings)
        });
        self.config.steam.games =
            diagnostics::time_block("frontend_model.refresh.scan_steam", || {
                retrofeel_backend::discover_installed_steam_games(&self.config.steam)
            });
        refresh_timing.finish();
        if self.status.is_empty() {
            self.status = format!(
                "{} ROMs, {} cores, {} BIOS entries checked, {} recordings, {} Steam games",
                self.roms.len(),
                self.registry.registry.cores.len(),
                self.bios
                    .as_ref()
                    .map(|bios| bios.checks.len())
                    .unwrap_or(0),
                self.recordings.len(),
                self.config.steam.games.len()
            );
        }
    }

    /// Refresh the model from the SQLite cache (fast path — no filesystem
    /// scans, no `Core::probe` calls). Falls back to the filesystem scan
    /// [`refresh`](Self::refresh) if the DB is unavailable or empty. Also
    /// writes the filesystem-scan results back to the DB so the next launch
    /// is fast.
    pub fn refresh_with_db(&mut self, db: &retrofeel_db::Db) {
        let refresh_timing = diagnostics::Timing::start("frontend_model.refresh_from_db");
        diagnostics::time_block("frontend_model.refresh.ensure_dirs", || {
            if let Err(error) = self.config.ensure_dirs() {
                self.status = format!("Could not create configured directories: {error}");
            }
        });

        // Core registry: read from DB. If empty, do a filesystem scan and
        // write the results back to the DB.
        let registry = {
            let repo = retrofeel_db::CoresRepo::new(db);
            match repo.registry() {
                Ok(report) if !report.registry.cores.is_empty() => report,
                _ => {
                    log::debug!("db.cores_cache_miss — falling back to filesystem scan");
                    let report =
                        CoreRegistry::scan_dir(&self.config.paths.cores, &self.config.paths.system)
                            .unwrap_or_default();
                    // Write the scan results back to the DB for next time.
                    let now = retrofeel_db::cores::now_secs();
                    let rows: Vec<retrofeel_db::cores::CoreRow> = report
                        .registry
                        .cores
                        .iter()
                        .map(|core| {
                            let (size, mtime) =
                                retrofeel_db::cores::file_metadata(&core.path).unwrap_or((0, now));
                            retrofeel_db::cores::CoreRow {
                                path: core.path.clone(),
                                library_name: core.library_name.clone(),
                                library_version: core.library_version.clone(),
                                valid_extensions: core.valid_extensions.clone(),
                                need_fullpath: core.need_fullpath,
                                block_extract: core.block_extract,
                                source: "filesystem".to_string(),
                                bundled: false,
                                file_size: size,
                                file_mtime: mtime,
                                last_seen: now,
                            }
                        })
                        .collect();
                    if let Err(error) = repo.replace_all(&rows) {
                        log::warn!("db.cores_cache_write_failed error=\"{error}\"");
                    }
                    report
                }
            }
        };
        self.registry = registry;

        // BIOS report: read from DB. If empty, scan and write back.
        let bios = {
            let repo = retrofeel_db::BiosRepo::new(db);
            match repo.report() {
                Ok(report) if !report.checks.is_empty() => Some(report),
                _ => {
                    let report = retrofeel_backend::scan_system_dir(&self.config.paths.system).ok();
                    if let Some(ref report) = report {
                        let now = retrofeel_db::cores::now_secs();
                        if let Err(error) = repo.replace_all(report, now) {
                            log::warn!("db.bios_cache_write_failed error=\"{error}\"");
                        }
                    }
                    report
                }
            }
        };
        self.bios = bios;

        // ROM list: scan the filesystem (this is fast — just `read_dir` +
        // extension matching, no dlopen). Write through to the DB.
        self.roms = diagnostics::time_block("frontend_model.refresh.scan_roms", || {
            scan_rom_dirs(&self.config, &self.registry)
        });

        // Session directories are authoritative. Re-index additive searchable
        // metadata so existing recordings gain transcript/input fields after
        // migration without requiring a one-off destructive conversion.
        let recordings = scan_recordings_dir(&self.config.paths.recordings);
        let repo = retrofeel_db::RecordingsRepo::new(db);
        for recording in &recordings {
            if let Some(row) = recording_db_row(recording) {
                if let Err(error) = repo.insert(&row) {
                    log::warn!("db.recording_reindex_failed error=\"{error}\"");
                }
            }
        }
        self.recordings = recordings;
        self.config.steam.games =
            diagnostics::time_block("frontend_model.refresh.scan_steam", || {
                retrofeel_backend::discover_installed_steam_games(&self.config.steam)
            });

        refresh_timing.finish();
        if self.status.is_empty() {
            self.status = format!(
                "{} ROMs, {} cores, {} BIOS entries checked, {} recordings, {} Steam games",
                self.roms.len(),
                self.registry.registry.cores.len(),
                self.bios
                    .as_ref()
                    .map(|bios| bios.checks.len())
                    .unwrap_or(0),
                self.recordings.len(),
                self.config.steam.games.len()
            );
        }

        // Load the cached core catalog from the DB (if present).
        let catalog_repo = retrofeel_db::CatalogRepo::new(db);
        if let Ok(catalog) = catalog_repo.all() {
            if !catalog.is_empty() {
                self.core_catalog = catalog;
            }
        }
    }

    pub fn save(&mut self) {
        if self.config_path.is_some() {
            diagnostics::time_block("frontend_model.save_file_override", || self.save_inner());
        } else if let Some(db) = &self.db {
            let repo = retrofeel_db::ConfigRepo::new(db);
            if let Err(error) = repo.save_config(&self.config) {
                log::warn!("db.config_save_failed error=\"{error}\"");
            }
        } else {
            self.status = "No configuration store is available".to_string();
        }
    }

    fn save_inner(&mut self) {
        let Some(path) = &self.config_path else {
            self.status = "Config has no writable path".to_string();
            return;
        };
        match self.config.save_to_path(path) {
            Ok(()) => self.status = format!("Saved config: {}", path.display()),
            Err(error) => self.status = format!("Could not save config: {error}"),
        }
    }

    /// Apply a folder chosen by the user (via the async picker) to the model.
    /// This is the post-pick half of the directory picker flow: assign the
    /// path, persist config, and refresh derived state. Split out so the
    /// async picker path can call it from `poll_pending_pick` once the rfd
    /// future resolves. The picker itself (rfd async dialog + timing span +
    /// `file_picker.begin`/`selected`/`cancelled` logs) lives in
    /// `plugin::start_dir_pick`. Does NOT call `refresh()` — the caller is
    /// responsible for triggering a background refresh via `start_refresh`
    /// so the main thread doesn't block on filesystem scans.
    pub fn apply_dir_choice(&mut self, target: PathTarget, path: PathBuf) {
        let target_label = target.log_label();
        log::debug!(
            "file_picker.selected kind=directory target=\"{target_label}\" path=\"{}\"",
            path.display()
        );
        match target {
            PathTarget::Cores => self.config.paths.cores = path,
            PathTarget::System => self.config.paths.system = path,
            PathTarget::Roms => self.config.paths.roms = vec![path],
            PathTarget::Saves => self.config.paths.saves = path,
            PathTarget::States => self.config.paths.states = path,
            PathTarget::Recordings => self.config.paths.recordings = path,
        }
        self.save();
    }

    /// Persist an explicitly selected whisper.cpp model. `None` clears the
    /// external model without probing another application's model cache.
    pub fn set_whisper_model(&mut self, path: Option<PathBuf>) {
        self.config.recording.whisper_model = path.clone();
        self.config.recording.transcription.external_model = path;
        self.config.recording.transcription.provider =
            retrofeel_types::TranscriptionProvider::WhisperCpp;
        self.save();
    }

    pub fn set_whisper_executable(&mut self, path: Option<PathBuf>) {
        self.config.recording.transcription.external_executable = path;
        self.config.recording.transcription.provider =
            retrofeel_types::TranscriptionProvider::WhisperCpp;
        self.save();
    }

    pub fn toggle_automatic_transcription(&mut self) {
        let transcription = &mut self.config.recording.transcription;
        transcription.automatic = !transcription.automatic;
        self.save();
    }

    /// Apply a BIOS file chosen by the user (via the async picker): copy it
    /// into the System folder under the expected filename, set status, and
    /// refresh. Split out so the async picker path can call it from
    /// `poll_pending_pick` once the rfd future resolves. Returns the
    /// destination path on success. The picker itself (rfd async dialog +
    /// timing span + `file_picker.begin`/`selected`/`cancelled` logs) lives
    /// in `plugin::start_bios_pick`.
    pub fn apply_bios_import(&mut self, filename: &str, source: &Path) -> std::io::Result<PathBuf> {
        log::debug!(
            "file_picker.selected kind=bios_file filename=\"{filename}\" source=\"{}\"",
            source.display()
        );
        let dest_dir = self.config.paths.system.clone();
        std::fs::create_dir_all(&dest_dir)?;
        let dest = dest_dir.join(filename);
        std::fs::copy(source, &dest)?;
        self.status = format!("Imported BIOS: {}", dest.display());
        self.refresh();
        self.preferences_section = PreferencesSection::SystemFiles;
        Ok(dest)
    }

    pub fn resolved_core_for_rom(&self, rom: &Path) -> Option<PathBuf> {
        self.registry
            .registry
            .resolve_rom(rom, Some(&self.config), None)
            .map(|core| core.path.clone())
            .or_else(|| self.config.core_override_for_rom(rom).cloned())
    }

    pub fn note_recent_rom(&mut self, rom: &Path) {
        self.config.push_recent_rom(rom.to_path_buf());
        self.save();
    }

    #[allow(dead_code)]
    pub fn needs_first_run_setup(&self) -> bool {
        self.registry.registry.cores.is_empty() || self.roms.is_empty()
    }

    pub fn mark_catalog_installed(&mut self, installed_path: &Path) {
        let Some(installed_stem) = installed_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.to_ascii_lowercase())
        else {
            return;
        };
        for entry in &mut self.core_catalog {
            let archive_stem = entry
                .archive_name
                .trim_end_matches(".zip")
                .trim_end_matches(".so")
                .trim_end_matches(".dll")
                .trim_end_matches(".dylib")
                .to_ascii_lowercase();
            if archive_stem == installed_stem
                || archive_stem.trim_end_matches("_libretro") == installed_stem
                || entry.slug.eq_ignore_ascii_case(&installed_stem)
            {
                entry.installed_path = Some(installed_path.to_path_buf());
                entry.status = CoreInstallStatus::Installed;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RomEntry {
    pub path: PathBuf,
    pub name: String,
    /// The core that will run this ROM, if one is discovered/assigned. `None`
    /// means the ROM matches a known system but has no core to launch it.
    pub core_name: Option<String>,
    /// The console this ROM belongs to, resolved by extension then core name.
    pub system: Option<&'static System>,
}

#[derive(Debug, Clone)]
pub struct RecordingEntry {
    pub path: PathBuf,
    pub name: String,
    pub frame_count: Option<u64>,
    pub title: String,
    pub core_name: String,
    pub start_timestamp: f64,
    pub duration_seconds: f64,
    pub dropped_frames: u64,
    pub has_video: bool,
    pub has_game_audio: bool,
    pub has_mic: bool,
    pub transcription_status: TranscriptionJobState,
    pub transcript_text: Option<String>,
    pub transcript: Option<TranscriptDocument>,
    pub input_summary: InputActivitySummary,
    pub input_changes: Vec<InputChange>,
    pub manifest: Option<SessionManifest>,
    pub feel_manifest: Option<FeelManifestV1>,
    pub alignment: Option<AlignmentReport>,
    pub latest_analysis: Option<AnalysisResultV2>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InputActivitySummary {
    pub keyboard_frames: u64,
    pub gamepad_frames: u64,
    pub mouse_frames: u64,
    pub change_count: u64,
    pub control_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InputChange {
    pub frame: u64,
    pub elapsed_seconds: f64,
    pub mapped: String,
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosterColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl PosterColor {
    fn color(self) -> Color {
        Color::srgb_u8(self.red, self.green, self.blue)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PosterArtwork {
    pub primary: PosterColor,
    pub secondary: PosterColor,
    pub accent: PosterColor,
    pub initials: String,
    pub stripe_seed: u8,
    pub rating: u8,
}

pub fn filtered_game_entries(model: &FrontendModel) -> Vec<&RomEntry> {
    filtered_library_entries(&model.roms, &model.library_view, &model.library_query)
}

pub fn filtered_library_entries<'a>(
    roms: &'a [RomEntry],
    view: &LibraryView,
    query: &str,
) -> Vec<&'a RomEntry> {
    let query = query.trim().to_ascii_lowercase();
    roms.iter()
        .filter(|rom| library_view_matches(rom, view))
        .filter(|rom| query.is_empty() || searchable_rom_text(rom).contains(&query))
        .collect()
}

pub fn deterministic_placeholder_artwork(rom: &RomEntry) -> PosterArtwork {
    let system_id = rom.system.map(|system| system.id).unwrap_or("unknown");
    let mut hash = stable_hash(system_id.as_bytes());
    hash = stable_hash_with(hash, rom.name.as_bytes());
    hash = stable_hash_with(hash, rom.path.to_string_lossy().as_bytes());

    const COLORS: [PosterColor; 10] = [
        PosterColor {
            red: 79,
            green: 138,
            blue: 190,
        },
        PosterColor {
            red: 194,
            green: 96,
            blue: 90,
        },
        PosterColor {
            red: 103,
            green: 153,
            blue: 104,
        },
        PosterColor {
            red: 202,
            green: 154,
            blue: 73,
        },
        PosterColor {
            red: 129,
            green: 111,
            blue: 179,
        },
        PosterColor {
            red: 69,
            green: 151,
            blue: 152,
        },
        PosterColor {
            red: 178,
            green: 108,
            blue: 145,
        },
        PosterColor {
            red: 151,
            green: 133,
            blue: 86,
        },
        PosterColor {
            red: 90,
            green: 121,
            blue: 169,
        },
        PosterColor {
            red: 169,
            green: 117,
            blue: 77,
        },
    ];
    let primary = COLORS[(hash as usize) % COLORS.len()];
    let secondary = COLORS[((hash >> 8) as usize + system_id.len()) % COLORS.len()];
    let accent = COLORS[((hash >> 16) as usize + rom.name.len()) % COLORS.len()];

    PosterArtwork {
        primary,
        secondary,
        accent,
        initials: poster_initials(&rom.name),
        stripe_seed: ((hash >> 24) & 0x7) as u8,
        rating: ((hash >> 32) % 5 + 1) as u8,
    }
}

fn library_view_matches(rom: &RomEntry, view: &LibraryView) -> bool {
    match view {
        LibraryView::AllGames => true,
        LibraryView::Other => rom.system.is_none(),
        LibraryView::System(id) => rom.system.map(|system| system.id) == Some(id.as_str()),
        LibraryView::Steam => false,
        LibraryView::RecentlyAdded => false,
    }
}

fn searchable_rom_text(rom: &RomEntry) -> String {
    format!(
        "{} {} {} {} {}",
        rom.name,
        rom.system.map(|system| system.name).unwrap_or("Unknown"),
        rom.system.map(|system| system.id).unwrap_or("unknown"),
        rom.core_name.as_deref().unwrap_or("missing core"),
        rom.path.display(),
    )
    .to_ascii_lowercase()
}

fn poster_initials(name: &str) -> String {
    let mut initials = String::new();
    for word in name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        if let Some(ch) = word.chars().next() {
            initials.push(ch.to_ascii_uppercase());
        }
        if initials.len() == 2 {
            break;
        }
    }
    if initials.is_empty() {
        "RF".to_string()
    } else {
        initials
    }
}

fn clipped_label(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut clipped: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn stable_hash(bytes: &[u8]) -> u64 {
    stable_hash_with(0xcbf2_9ce4_8422_2325, bytes)
}

fn stable_hash_with(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingCapture {
    Keyboard { control: String },
    Gamepad { control: String },
}

impl BindingCapture {
    pub fn label(&self) -> String {
        match self {
            BindingCapture::Keyboard { control } => format!("keyboard key for {control}"),
            BindingCapture::Gamepad { control } => format!("gamepad button for {control}"),
        }
    }
}

#[derive(Component)]
pub struct UiRoot;

/// Marker for a content panel that should respond to the mouse wheel. The
/// `scroll_lists` system (in `plugin.rs`) adjusts the entity's `ScrollPosition`;
/// the panel must also set `overflow: Overflow::scroll_y()` for the offset to
/// have any effect. `ScrollPosition` is a required component of `Node`, so it is
/// already present — we only add this marker + the scroll overflow.
#[derive(Component)]
pub struct Scrollable;

/// Marker for a button that represents the currently-selected item (e.g. the
/// active console in the library sidebar). `button_interactions` keeps such a
/// button tinted with the active color even when not hovered.
#[derive(Component)]
pub struct Selected;

/// Root marker for the in-game control bar (spawned on entering `InGame`).
#[derive(Component)]
pub struct HudBar;

/// Live-updated text on the HUD: the recording status indicator.
#[derive(Component)]
pub struct RecIndicator;

/// Live-updated text on the HUD: the Record/Stop button label.
#[derive(Component)]
pub struct RecordButtonLabel;

/// Live-updated text on the HUD: the Pause/Resume button label.
#[derive(Component)]
pub struct PauseButtonLabel;

/// Live-updated synchronized viewer timecode.
#[derive(Component)]
pub struct RecordingTimeLabel;

/// Live-updated narration and controller-input cue.
#[derive(Component)]
pub struct RecordingTimelineLabel;

/// Live-updated Play/Pause label inside the recording viewer.
#[derive(Component)]
pub struct RecordingPlaybackLabel;

#[derive(Component, Clone)]
#[require(TabIndex)]
pub struct UiButton {
    pub action: UiAction,
    pub variant: ButtonVariant,
}

#[derive(Message, Debug, Clone)]
pub struct UiActionRequest {
    pub action: UiAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    GoLibrary,
    GoSettings,
    SetThemePreference(ThemePreference),
    SelectPreferencesSection(PreferencesSection),
    RefreshCoreCatalog,
    InstallCore(CoreCatalogEntry),
    ImportBios {
        filename: String,
    },
    SelectLibraryView(LibraryView),
    SetLibraryDisplayMode(LibraryDisplayMode),
    SetLibraryTopView(LibraryTopView),
    ClearLibrarySearch,
    SelectRecording(PathBuf),
    CloseRecording,
    RetryTranscription(PathBuf),
    CopyTranscript(PathBuf),
    RevealSession(PathBuf),
    ToggleRecordingPlayback,
    SeekRecordingRelative(i64),
    SeekRecordingTo(u64),
    AnalyzeRecording {
        package: PathBuf,
        adapter: AgentAdapterKind,
    },
    Refresh,
    PickPath(PathTarget),
    LaunchRom(PathBuf),
    AssignCore {
        extension: String,
        core_path: PathBuf,
    },
    BindKeyboard(String),
    BindGamepad(String),
    ResetKeyboardBindings,
    ResetGamepadBindings,
    SetCoreOption {
        core_key: String,
        option_key: String,
        value: String,
    },
    ClearCoreOption {
        core_key: String,
        option_key: String,
    },
    ToggleIntegerScaling,
    ToggleAspectCorrection,
    ToggleFullscreen,
    ToggleAudio,
    ToggleMicCapture,
    ToggleAutomaticTranscription,
    InstallTranscriptionModel(String),
    CancelTranscriptionModelDownload,
    PickWhisperExecutable,
    PickWhisperModel,
    ClearWhisperModel,
    StartRecording,
    StopRecording,
    ToggleRecording,
    TogglePause,
    ResetCore,
    VolumeUp,
    VolumeDown,
    OpenOverlay,
    Screenshot,
    ExportRecording {
        target: ExportTarget,
        session: PathBuf,
    },
    Resume,
    QuitToLibrary,
    SaveState(u8),
    LoadState(u8),
    LaunchSteamGame(u32),
}

impl UiAction {
    pub fn log_label(&self) -> String {
        match self {
            Self::GoLibrary => "go_library".to_string(),
            Self::GoSettings => "go_settings".to_string(),
            Self::SetThemePreference(preference) => {
                format!("set_theme_preference:{preference:?}").to_ascii_lowercase()
            }
            Self::SelectPreferencesSection(section) => {
                format!("select_preferences_section:{}", section.log_label())
            }
            Self::RefreshCoreCatalog => "refresh_core_catalog".to_string(),
            Self::InstallCore(entry) => format!("install_core:{}", entry.slug),
            Self::ImportBios { filename } => format!("import_bios:{filename}"),
            Self::SelectLibraryView(view) => format!("select_library_view:{}", view.log_label()),
            Self::SetLibraryDisplayMode(mode) => {
                format!("set_library_display_mode:{}", mode.log_label())
            }
            Self::SetLibraryTopView(view) => format!("set_library_top_view:{}", view.log_label()),
            Self::ClearLibrarySearch => "clear_library_search".to_string(),
            Self::SelectRecording(path) => format!("select_recording:{}", path.display()),
            Self::CloseRecording => "close_recording".to_string(),
            Self::RetryTranscription(path) => format!("retry_transcription:{}", path.display()),
            Self::CopyTranscript(path) => format!("copy_transcript:{}", path.display()),
            Self::RevealSession(path) => format!("reveal_session:{}", path.display()),
            Self::ToggleRecordingPlayback => "toggle_recording_playback".to_string(),
            Self::SeekRecordingRelative(milliseconds) => {
                format!("seek_recording_relative:{milliseconds}")
            }
            Self::SeekRecordingTo(milliseconds) => format!("seek_recording_to:{milliseconds}"),
            Self::AnalyzeRecording { package, adapter } => {
                format!("analyze_recording:{}:{}", adapter.id(), package.display())
            }
            Self::Refresh => "refresh".to_string(),
            Self::PickPath(target) => format!("pick_path:{}", target.log_label()),
            Self::LaunchRom(path) => format!("launch_rom:{}", path.display()),
            Self::AssignCore {
                extension,
                core_path,
            } => format!("assign_core:.{extension}->{}", core_path.display()),
            Self::BindKeyboard(control) => format!("bind_keyboard:{control}"),
            Self::BindGamepad(control) => format!("bind_gamepad:{control}"),
            Self::ResetKeyboardBindings => "reset_keyboard_bindings".to_string(),
            Self::ResetGamepadBindings => "reset_gamepad_bindings".to_string(),
            Self::SetCoreOption {
                core_key,
                option_key,
                value,
            } => format!("set_core_option:{core_key}/{option_key}={value}"),
            Self::ClearCoreOption {
                core_key,
                option_key,
            } => format!("clear_core_option:{core_key}/{option_key}"),
            Self::ToggleIntegerScaling => "toggle_integer_scaling".to_string(),
            Self::ToggleAspectCorrection => "toggle_aspect_correction".to_string(),
            Self::ToggleFullscreen => "toggle_fullscreen".to_string(),
            Self::ToggleAudio => "toggle_audio".to_string(),
            Self::ToggleMicCapture => "toggle_mic_capture".to_string(),
            Self::ToggleAutomaticTranscription => "toggle_automatic_transcription".to_string(),
            Self::InstallTranscriptionModel(id) => {
                format!("install_transcription_model:{id}")
            }
            Self::CancelTranscriptionModelDownload => {
                "cancel_transcription_model_download".to_string()
            }
            Self::PickWhisperExecutable => "pick_whisper_executable".to_string(),
            Self::PickWhisperModel => "pick_whisper_model".to_string(),
            Self::ClearWhisperModel => "clear_whisper_model".to_string(),
            Self::StartRecording => "start_recording".to_string(),
            Self::StopRecording => "stop_recording".to_string(),
            Self::ToggleRecording => "toggle_recording".to_string(),
            Self::TogglePause => "toggle_pause".to_string(),
            Self::ResetCore => "reset_core".to_string(),
            Self::VolumeUp => "volume_up".to_string(),
            Self::VolumeDown => "volume_down".to_string(),
            Self::OpenOverlay => "open_overlay".to_string(),
            Self::Screenshot => "screenshot".to_string(),
            Self::ExportRecording { target, session } => {
                format!(
                    "export_recording:{}:{}",
                    target.log_label(),
                    session.display()
                )
            }
            Self::Resume => "resume".to_string(),
            Self::QuitToLibrary => "quit_to_library".to_string(),
            Self::SaveState(slot) => format!("save_state:{slot}"),
            Self::LoadState(slot) => format!("load_state:{slot}"),
            Self::LaunchSteamGame(app_id) => format!("launch_steam:{app_id}"),
        }
    }

    fn accessible_label(&self) -> String {
        match self {
            Self::SetLibraryDisplayMode(LibraryDisplayMode::Grid) => "Grid view".to_string(),
            Self::SetLibraryDisplayMode(LibraryDisplayMode::List) => "List view".to_string(),
            Self::ClearLibrarySearch => "Clear search".to_string(),
            Self::ToggleRecording => "Toggle recording".to_string(),
            Self::TogglePause => "Pause or resume".to_string(),
            Self::ResetCore => "Reset game".to_string(),
            Self::VolumeUp => "Volume up".to_string(),
            Self::VolumeDown => "Volume down".to_string(),
            Self::ToggleFullscreen => "Toggle fullscreen".to_string(),
            Self::OpenOverlay => "Open game menu".to_string(),
            Self::QuitToLibrary => "Quit to library".to_string(),
            Self::SaveState(slot) => format!("Save state slot {slot}"),
            Self::LoadState(slot) => format!("Load state slot {slot}"),
            _ => self.log_label().replace(['_', ':'], " "),
        }
    }
}

#[derive(Component)]
pub struct Decorative;

/// Preserves the baked system badge instead of tinting it like a generic
/// monochrome button icon.
#[derive(Component)]
pub struct SystemBadge;

/// Marks full-color media that must not inherit a button's foreground tint.
#[derive(Component)]
pub struct PreserveImageColor;

/// Live status text shared by the library banner and the frontend model.
#[derive(Component)]
pub struct ModelStatusText;

pub fn hide_decorative_accessibility(
    mut commands: Commands,
    decorative: Query<Entity, Added<Decorative>>,
) {
    for entity in &decorative {
        let mut node = AccessNode::new(Role::Image);
        node.set_hidden();
        commands
            .entity(entity)
            .insert(AccessibilityNode::from(node));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportTarget {
    Bevy,
    Unity,
    Godot,
    Unreal,
}

impl ExportTarget {
    pub fn label(self) -> &'static str {
        match self {
            ExportTarget::Bevy => "Bevy",
            ExportTarget::Unity => "Unity",
            ExportTarget::Godot => "Godot",
            ExportTarget::Unreal => "Unreal",
        }
    }

    pub fn engine(self) -> retrofeel_export::Engine {
        match self {
            ExportTarget::Bevy => retrofeel_export::Engine::Bevy,
            ExportTarget::Unity => retrofeel_export::Engine::Unity,
            ExportTarget::Godot => retrofeel_export::Engine::Godot,
            ExportTarget::Unreal => retrofeel_export::Engine::Unreal,
        }
    }

    fn log_label(self) -> &'static str {
        match self {
            ExportTarget::Bevy => "bevy",
            ExportTarget::Unity => "unity",
            ExportTarget::Godot => "godot",
            ExportTarget::Unreal => "unreal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTarget {
    Cores,
    System,
    Roms,
    Saves,
    States,
    Recordings,
}

impl PathTarget {
    pub fn log_label(self) -> &'static str {
        match self {
            Self::Cores => "cores",
            Self::System => "system",
            Self::Roms => "roms",
            Self::Saves => "saves",
            Self::States => "states",
            Self::Recordings => "recordings",
        }
    }
}

pub fn spawn_library(
    commands: &mut Commands,
    model: &FrontendModel,
    icons: &IconAssets,
    viewer: &crate::feel_viewer::RecordingViewer,
) {
    diagnostics::time_block("ui.spawn.library", || {
        commands
            .spawn((
                library_root_node(),
                BackgroundColor(PALETTE_BG),
                UiRoot,
                TabGroup::default(),
            ))
            .with_children(|root| {
                spawn_shell_header(root, model, icons, "Library");
                spawn_library_body(root, model, NavScreen::Library, |content| {
                    spawn_status_banner(content, &model.status);
                    spawn_library_main(content, model, icons, viewer);
                });
                spawn_bottom_toolbar(root, model, icons, NavScreen::Library);
            });
    });
}

fn spawn_status_banner(parent: &mut ChildSpawnerCommands, status: &str) {
    parent
        .spawn((
            Node {
                width: percent(100),
                min_height: px(40.0),
                padding: UiRect::axes(px(12.0), px(8.0)),
                align_items: AlignItems::Center,
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
                ..default()
            },
            BackgroundColor(PALETTE_PREF_CARD),
            BorderColor::all(PALETTE_LINE),
        ))
        .with_children(|banner| {
            banner.spawn((
                Text::new(status.to_string()),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(PALETTE_MUTED),
                ModelStatusText,
            ));
        });
}

pub fn sync_model_status(
    model: Res<FrontendModel>,
    mut labels: Query<&mut Text, With<ModelStatusText>>,
) {
    if !model.is_changed() {
        return;
    }
    for mut label in &mut labels {
        if label.as_str() != model.status {
            **label = model.status.clone();
        }
    }
}

fn library_root_node() -> Node {
    Node {
        width: percent(100),
        height: percent(100),
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Stretch,
        ..default()
    }
}

fn library_body_node() -> Node {
    Node {
        width: percent(100),
        flex_grow: 1.0,
        flex_basis: px(0.0),
        min_height: px(0.0),
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Stretch,
        overflow: Overflow::clip(),
        ..default()
    }
}

fn library_content_node() -> Node {
    Node {
        flex_grow: 1.0,
        flex_basis: px(0.0),
        min_width: px(0.0),
        min_height: px(0.0),
        padding: UiRect::all(px(theme::LAYOUT.space_8)),
        flex_direction: FlexDirection::Column,
        row_gap: px(UI.gap_md),
        overflow: Overflow::scroll_y(),
        ..default()
    }
}

fn spawn_library_body(
    root: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    current: NavScreen,
    spawn_main: impl FnOnce(&mut ChildSpawnerCommands),
) {
    root.spawn(library_body_node()).with_children(|body| {
        spawn_context_navigation(body, model, current);
        body.spawn((
            library_content_node(),
            BackgroundColor(PALETTE_PANEL),
            Scrollable,
        ))
        .with_children(spawn_main);
    });
}

fn spawn_shell_header(
    parent: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    icons: &IconAssets,
    title: &str,
) {
    parent
        .spawn((
            Node {
                width: percent(100),
                height: px(UI.toolbar_height),
                padding: UiRect::axes(px(14.0), px(8.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: px(12.0),
                border: UiRect::bottom(px(1.0)),
                ..default()
            },
            BackgroundColor(PALETTE_TOOLBAR),
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(|bar| {
            bar.spawn((
                Node {
                    width: px(34.0),
                    height: px(34.0),
                    ..default()
                },
                ImageNode::new(icons.get(Icon::Brand)),
                Decorative,
            ));
            bar.spawn((
                Text::new("RetroFeel"),
                TextFont {
                    font_size: 24.0,
                    ..default()
                },
                TextColor(PALETTE_TEXT),
            ));
            if title != "Library" {
                spawn_text(bar, "/", 18.0, PALETTE_MUTED);
                spawn_text(bar, title, 18.0, PALETTE_MUTED);
            }
            bar.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if title == "Library" {
                spawn_toolbar_icon_button(
                    bar,
                    icons,
                    LibraryDisplayMode::Grid.icon(),
                    UiAction::SetLibraryDisplayMode(LibraryDisplayMode::Grid),
                    model.library_display_mode == LibraryDisplayMode::Grid,
                );
                spawn_toolbar_icon_button(
                    bar,
                    icons,
                    LibraryDisplayMode::List.icon(),
                    UiAction::SetLibraryDisplayMode(LibraryDisplayMode::List),
                    model.library_display_mode == LibraryDisplayMode::List,
                );
                spawn_library_top_segments(bar, model);
                spawn_library_search_field(bar, model, icons);
            }
        });
}

fn spawn_library_top_segments(parent: &mut ChildSpawnerCommands, model: &FrontendModel) {
    parent
        .spawn((
            Node {
                min_height: px(30.0),
                padding: UiRect::all(px(2.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
                ..default()
            },
            BackgroundColor(PALETTE_SEGMENT_BG),
            BorderColor::all(PALETTE_LINE),
        ))
        .with_children(|segments| {
            for view in LibraryTopView::ALL {
                spawn_segment_button(
                    segments,
                    view.label(),
                    UiAction::SetLibraryTopView(view),
                    model.library_top_view == view,
                    13.0,
                );
            }
        });
}

fn spawn_library_search_field(
    parent: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    icons: &IconAssets,
) {
    parent
        .spawn((
            Node {
                width: px(210.0),
                height: px(30.0),
                padding: UiRect::axes(px(9.0), px(5.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: px(7.0),
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
                ..default()
            },
            BackgroundColor(PALETTE_SEARCH),
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(|field| {
            spawn_icon(field, icons.get(Icon::Search), PALETTE_MUTED);
            field.spawn((
                Text::new(if model.library_query.is_empty() {
                    "Search".to_string()
                } else {
                    clipped_label(&model.library_query, 22)
                }),
                TextFont {
                    font_size: 13.0,
                    ..default()
                },
                TextColor(if model.library_query.is_empty() {
                    PALETTE_MUTED
                } else {
                    PALETTE_TEXT
                }),
            ));
            if !model.library_query.is_empty() {
                field.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                spawn_toolbar_icon_button(
                    field,
                    icons,
                    Icon::Close,
                    UiAction::ClearLibrarySearch,
                    false,
                );
            }
        });
}

fn spawn_library_main(
    content: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    icons: &IconAssets,
    viewer: &crate::feel_viewer::RecordingViewer,
) {
    match model.library_top_view {
        LibraryTopView::Games => spawn_games_view(content, model, icons),
        LibraryTopView::Recordings => spawn_recordings_view(content, model, viewer),
        LibraryTopView::Screenshots => spawn_screenshots_view(content, model),
    }
}

fn spawn_games_view(content: &mut ChildSpawnerCommands, model: &FrontendModel, icons: &IconAssets) {
    if model.library_view == LibraryView::RecentlyAdded {
        spawn_recent_games_view(content, model);
        return;
    }

    if model.library_view == LibraryView::Steam {
        spawn_steam_games_view(content, model, icons);
        return;
    }

    let filtered = filtered_game_entries(model);
    spawn_collection_header(
        content,
        &library_collection_title(&model.library_view),
        &format!(
            "{} game(s){}",
            filtered.len(),
            if model.library_query.is_empty() {
                String::new()
            } else {
                format!(" matching \"{}\"", model.library_query)
            }
        ),
    );

    if filtered.is_empty() {
        let hint = match &model.library_view {
            LibraryView::System(id) => {
                if let Some(system) = system_by_id(id) {
                    let exts = if system.extensions.is_empty() {
                        "disc images".to_string()
                    } else {
                        system.extensions.join(", .")
                    };
                    format!(
                        "No {} games found.\nSet your ROMs folder in Settings and ensure it contains .{} files.",
                        system.name, exts
                    )
                } else {
                    "No games in this collection.".to_string()
                }
            }
            _ => "No games in this collection.".to_string(),
        };
        spawn_empty_state(content, &hint);
        return;
    }

    match model.library_display_mode {
        LibraryDisplayMode::Grid => spawn_game_grid(content, icons, &filtered),
        LibraryDisplayMode::List => spawn_game_list(content, &filtered),
    }
}

fn spawn_steam_games_view(
    content: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    _icons: &IconAssets,
) {
    let games = &model.config.steam.games;
    spawn_collection_header(
        content,
        "Steam games",
        &format!("{} game(s) discovered", games.len()),
    );
    if games.is_empty() {
        spawn_empty_state(
            content,
            "No installed Steam games discovered.\nInstall a game through Steam or GameHub, then refresh the library.",
        );
        return;
    }
    content
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            column_gap: px(18.0),
            row_gap: px(18.0),
            ..default()
        })
        .with_children(|grid| {
            for game in games {
                grid.spawn((
                    Button,
                    Node {
                        width: px(300.0),
                        padding: UiRect::all(px(8.0)),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8.0),
                        border: UiRect::all(px(1.0)),
                        border_radius: BorderRadius::all(px(theme::LAYOUT.card_radius)),
                        ..default()
                    },
                    BackgroundColor(PALETTE_TILE),
                    BorderColor::all(PALETTE_LINE_DARK),
                    theme::quiet_card_shadow(),
                    UiButton {
                        action: UiAction::LaunchSteamGame(game.app_id),
                        variant: ButtonVariant::Secondary,
                    },
                ))
                .with_children(|card| {
                    card.spawn((
                        Node {
                            width: percent(100),
                            height: px(140.0),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            border_radius: BorderRadius::all(px(5.0)),
                            overflow: Overflow::clip(),
                            ..default()
                        },
                        BackgroundColor(PALETTE_EMPTY),
                        crate::boxart::SteamArtSlot {
                            app_id: game.app_id,
                        },
                        PreserveImageColor,
                        children![(
                            Text::new("STEAM"),
                            TextFont {
                                font_size: 26.0,
                                ..default()
                            },
                            TextColor(PALETTE_MUTED_DARK),
                        )],
                    ));
                    spawn_text(card, &game.name, 16.0, PALETTE_TEXT);
                    spawn_text(card, &format!("App {}", game.app_id), 12.0, PALETTE_MUTED);
                    let source = match game.source {
                        SteamGameSource::Configured => "Configured Steam game",
                        SteamGameSource::GameHub => "Installed through GameHub",
                        SteamGameSource::NativeSteam => "Installed through Steam",
                    };
                    spawn_text(card, source, 12.0, PALETTE_MUTED);
                });
            }
        });
}

fn spawn_recent_games_view(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    spawn_collection_header(content, "Recently added", "Most recent launches");
    if model.config.recent_roms.is_empty() {
        spawn_empty_state(content, "No games launched yet.");
        return;
    }
    for path in model.config.recent_roms.iter().take(50) {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("ROM");
        spawn_button(content, name, UiAction::LaunchRom(path.clone()));
    }
}

fn spawn_recordings_view(
    content: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    viewer: &crate::feel_viewer::RecordingViewer,
) {
    if let Some(selected) = model.selected_recording.as_ref() {
        if let Some(recording) = model
            .recordings
            .iter()
            .find(|entry| &entry.path == selected)
        {
            spawn_recording_detail(content, recording, viewer);
            return;
        }
    }
    let query = model.library_query.trim().to_ascii_lowercase();
    let recordings = model
        .recordings
        .iter()
        .filter(|recording| query.is_empty() || recording_search_text(recording).contains(&query))
        .collect::<Vec<_>>();
    spawn_collection_header(
        content,
        "Recordings",
        &format!("{} session(s)", recordings.len()),
    );
    if recordings.is_empty() {
        spawn_empty_state(content, "No completed recordings found.");
        return;
    }
    for recording in recordings.into_iter().take(80) {
        let status = transcription_status_label(&recording.transcription_status);
        let excerpt = recording
            .transcript_text
            .as_deref()
            .map(|text| clipped_label(text, 180))
            .unwrap_or_else(|| "No transcript yet".into());
        content
            .spawn((
                Button,
                Node {
                    width: percent(100),
                    padding: UiRect::all(px(14.0)),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(7.0),
                    border: UiRect::all(px(1.0)),
                    border_radius: BorderRadius::all(px(theme::LAYOUT.card_radius)),
                    ..default()
                },
                BackgroundColor(PALETTE_ROW),
                BorderColor::all(PALETTE_LINE_DARK),
                theme::quiet_card_shadow(),
                UiButton {
                    action: UiAction::SelectRecording(recording.path.clone()),
                    variant: ButtonVariant::Secondary,
                },
            ))
            .with_children(|card| {
                spawn_text(card, &recording.title, 18.0, PALETTE_TEXT);
                spawn_text(
                    card,
                    &format!(
                        "{:.1}s · {} frames · {} · {} · started {:.0}",
                        recording.duration_seconds,
                        recording.frame_count.unwrap_or_default(),
                        recording.core_name,
                        status,
                        recording.start_timestamp,
                    ),
                    13.0,
                    PALETTE_MUTED,
                );
                spawn_text(card, &excerpt, 14.0, PALETTE_TEXT);
                spawn_text(
                    card,
                    &format!(
                        "{}{}{} · {} input changes · {} dropped",
                        if recording.input_summary.keyboard_frames > 0 {
                            "Keyboard "
                        } else {
                            ""
                        },
                        if recording.input_summary.gamepad_frames > 0 {
                            "Gamepad "
                        } else {
                            ""
                        },
                        if recording.input_summary.mouse_frames > 0 {
                            "Mouse "
                        } else {
                            ""
                        },
                        recording.input_summary.change_count,
                        recording.dropped_frames,
                    ),
                    12.0,
                    if recording.dropped_frames == 0 {
                        PALETTE_GOOD
                    } else {
                        PALETTE_WARN
                    },
                );
            });
    }
}

fn spawn_recording_detail(
    content: &mut ChildSpawnerCommands,
    recording: &RecordingEntry,
    viewer: &crate::feel_viewer::RecordingViewer,
) {
    spawn_button(content, "← All recordings", UiAction::CloseRecording);
    spawn_collection_header(
        content,
        &recording.title,
        &format!(
            "{:.2}s · {} · {} dropped frame(s)",
            recording.duration_seconds, recording.core_name, recording.dropped_frames
        ),
    );
    spawn_text(
        content,
        &format!(
            "Video: {} · game audio: {} · microphone: {} · transcript: {}",
            yes_no(recording.has_video),
            yes_no(recording.has_game_audio),
            yes_no(recording.has_mic),
            transcription_status_label(&recording.transcription_status),
        ),
        14.0,
        PALETTE_MUTED,
    );

    if recording.has_video {
        content
            .spawn(Node {
                width: percent(100),
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(8.0),
                row_gap: px(8.0),
                ..default()
            })
            .with_children(|controls| {
                spawn_playback_button(controls, viewer.is_playing());
                spawn_button(
                    controls,
                    "−10 seconds",
                    UiAction::SeekRecordingRelative(-10_000),
                );
                spawn_button(
                    controls,
                    "+10 seconds",
                    UiAction::SeekRecordingRelative(10_000),
                );
            });
        content.spawn((
            Text::new(format!(
                "{} / {}",
                short_timestamp(viewer.position_seconds()),
                short_timestamp(viewer.duration_seconds())
            )),
            TextFont {
                font_size: 14.0,
                ..default()
            },
            TextColor(PALETTE_MUTED),
            RecordingTimeLabel,
        ));
        content.spawn((
            Text::new("Narration and controller cues follow playback."),
            TextFont {
                font_size: 14.0,
                ..default()
            },
            TextColor(PALETTE_TEXT),
            RecordingTimelineLabel,
        ));
        content.spawn((
            recording_preview_node(),
            BackgroundColor(PALETTE_EMPTY),
            BorderColor::all(PALETTE_LINE),
            ImageNode::new(viewer.preview.clone()),
            PreserveImageColor,
        ));
    }

    if let Some(manifest) = recording.feel_manifest.as_ref() {
        spawn_section(content, ".feel package");
        spawn_text(
            content,
            &format!(
                "Format v{} · package {} · {} transcript(s) · {} analysis run(s)",
                manifest.format_version,
                manifest.package_id,
                manifest.transcripts.len(),
                manifest.analyses.len()
            ),
            13.0,
            PALETTE_MUTED,
        );
        if let Some(alignment) = recording.alignment.as_ref() {
            spawn_text(
                content,
                &format!(
                    "Transcript alignment: {:?} · {:.3}s offset · {:.6}× scale · {} anchors{}",
                    alignment.quality,
                    alignment.offset_seconds,
                    alignment.scale,
                    alignment.anchor_count,
                    alignment
                        .median_absolute_residual_seconds
                        .map(|value| format!(" · {:.3}s median residual", value))
                        .unwrap_or_default()
                ),
                13.0,
                match alignment.quality {
                    retrofeel_feel::AlignmentQuality::Complete => PALETTE_GOOD,
                    retrofeel_feel::AlignmentQuality::Estimated => PALETTE_INFO,
                    retrofeel_feel::AlignmentQuality::Degraded => PALETTE_WARN,
                },
            );
        }
        content
            .spawn(Node {
                width: percent(100),
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(8.0),
                row_gap: px(8.0),
                ..default()
            })
            .with_children(|agents| {
                for adapter in [
                    AgentAdapterKind::Codex,
                    AgentAdapterKind::Claude,
                    AgentAdapterKind::OpenCode,
                    AgentAdapterKind::Kimi,
                ] {
                    spawn_button(
                        agents,
                        &format!("Analyze with {adapter}"),
                        UiAction::AnalyzeRecording {
                            package: recording.path.clone(),
                            adapter,
                        },
                    );
                }
            });
    }
    content
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            column_gap: px(8.0),
            row_gap: px(8.0),
            ..default()
        })
        .with_children(|actions| {
            spawn_button(
                actions,
                "Retry transcription",
                UiAction::RetryTranscription(recording.path.clone()),
            );
            spawn_button(
                actions,
                "Copy transcript",
                UiAction::CopyTranscript(recording.path.clone()),
            );
            spawn_button(
                actions,
                "Reveal session",
                UiAction::RevealSession(recording.path.clone()),
            );
            for target in [
                ExportTarget::Bevy,
                ExportTarget::Unity,
                ExportTarget::Godot,
                ExportTarget::Unreal,
            ] {
                spawn_button(
                    actions,
                    &format!("Export {}", target.label()),
                    UiAction::ExportRecording {
                        target,
                        session: recording.path.clone(),
                    },
                );
            }
        });

    spawn_section(content, "Timestamped transcript");
    match recording.transcript.as_ref() {
        Some(document) if !document.segments.is_empty() => {
            for segment in document.segments.iter().take(400) {
                spawn_button(
                    content,
                    &format!(
                        "[{}–{}] {}",
                        short_timestamp(segment.start_seconds),
                        short_timestamp(segment.end_seconds),
                        segment.text
                    ),
                    UiAction::SeekRecordingTo(
                        (segment.start_seconds.max(0.0) * 1_000.0).round() as u64
                    ),
                );
            }
        }
        _ => spawn_text(
            content,
            "No timestamped transcript is available.",
            14.0,
            PALETTE_MUTED,
        ),
    }

    spawn_section(content, "Collapsed input-change timeline");
    if recording.input_changes.is_empty() {
        spawn_text(content, "No input changes captured.", 14.0, PALETTE_MUTED);
    } else {
        for change in recording.input_changes.iter().take(300) {
            spawn_button(
                content,
                &format!(
                    "[{}] Frame {} · mapped: {} · raw host: {}",
                    short_timestamp(change.elapsed_seconds),
                    change.frame,
                    change.mapped,
                    change.raw
                ),
                UiAction::SeekRecordingTo((change.elapsed_seconds * 1_000.0).round() as u64),
            );
        }
    }

    if let Some(analysis) = recording.latest_analysis.as_ref() {
        spawn_section(content, "Latest analysis");
        spawn_text(content, &analysis.summary, 15.0, PALETTE_TEXT);
        for (index, evidence) in analysis.evidence.iter().enumerate() {
            spawn_button(
                content,
                &format!(
                    "Evidence {} [{}–{}] {}",
                    index + 1,
                    short_timestamp(evidence.start_seconds),
                    short_timestamp(evidence.end_seconds),
                    evidence.description
                ),
                UiAction::SeekRecordingTo(
                    (evidence.start_seconds.max(0.0) * 1_000.0).round() as u64
                ),
            );
        }
        for insight in &analysis.insights {
            spawn_text(
                content,
                &format!("Insight — {}: {}", insight.title, insight.implication),
                14.0,
                PALETTE_INFO,
            );
        }
        for action in &analysis.actions {
            spawn_text(
                content,
                &format!(
                    "{} priority / {} effort — {}: {}",
                    action.priority, action.effort, action.title, action.rationale
                ),
                14.0,
                PALETTE_GOOD,
            );
        }
    }
}

fn recording_preview_node() -> Node {
    Node {
        width: percent(100),
        height: auto(),
        flex_shrink: 0.0,
        border: UiRect::all(px(1.0)),
        border_radius: BorderRadius::all(px(theme::LAYOUT.card_radius)),
        overflow: Overflow::clip(),
        ..default()
    }
}

fn transcription_status_label(status: &TranscriptionJobState) -> String {
    match status {
        TranscriptionJobState::NotRequested => "not requested".into(),
        TranscriptionJobState::Queued => "queued".into(),
        TranscriptionJobState::Running { progress_percent } => {
            format!("transcribing {progress_percent}%")
        }
        TranscriptionJobState::Complete => "transcript ready".into(),
        TranscriptionJobState::Cancelled => "cancelled".into(),
        TranscriptionJobState::Failed { message } => format!("needs attention: {message}"),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn short_timestamp(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn spawn_screenshots_view(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    let dir = model.config.paths.recordings.join("screenshots");
    spawn_collection_header(content, "Screenshots", &dir.display().to_string());
    spawn_empty_state(
        content,
        "Screenshots are saved here after using the in-game screenshot action.",
    );
}

fn spawn_game_grid(content: &mut ChildSpawnerCommands, icons: &IconAssets, roms: &[&RomEntry]) {
    content
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::FlexStart,
            align_content: AlignContent::FlexStart,
            column_gap: px(18.0),
            row_gap: px(22.0),
            ..default()
        })
        .with_children(|grid| {
            for rom in roms.iter().take(180) {
                spawn_game_tile(grid, icons, rom);
            }
        });
}

fn spawn_game_tile(parent: &mut ChildSpawnerCommands, icons: &IconAssets, rom: &RomEntry) {
    let artwork = deterministic_placeholder_artwork(rom);
    parent
        .spawn((
            Button,
            Node {
                width: px(UI.cover_tile_width),
                height: px(UI.cover_tile_height),
                padding: UiRect::all(px(8.0)),
                flex_direction: FlexDirection::Column,
                row_gap: px(6.0),
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(theme::LAYOUT.card_radius)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(PALETTE_TILE),
            BorderColor::all(PALETTE_LINE_DARK),
            theme::quiet_card_shadow(),
            UiButton {
                action: UiAction::LaunchRom(rom.path.clone()),
                variant: ButtonVariant::Secondary,
            },
        ))
        .with_children(|tile| {
            spawn_poster_art(tile, &artwork, rom);
            tile.spawn((
                Text::new(rom.name.clone()),
                TextFont {
                    font_size: 13.0,
                    ..default()
                },
                TextColor(PALETTE_TEXT),
            ));
            tile.spawn((
                Text::new(
                    rom.system
                        .map(|system| system.name)
                        .unwrap_or("Unknown")
                        .to_string(),
                ),
                TextFont {
                    font_size: 11.0,
                    ..default()
                },
                TextColor(PALETTE_MUTED),
            ));
            spawn_star_rating(tile, icons, artwork.rating);
            let (status, color) = match &rom.core_name {
                Some(core) => (core.as_str(), PALETTE_GOOD),
                None => ("Missing core", PALETTE_WARN),
            };
            tile.spawn((
                Text::new(status.to_string()),
                TextFont {
                    font_size: 11.0,
                    ..default()
                },
                TextColor(color),
            ));
        });
}

fn spawn_poster_art(parent: &mut ChildSpawnerCommands, artwork: &PosterArtwork, rom: &RomEntry) {
    let mut poster = parent.spawn((
        Node {
            width: percent(100),
            height: px(UI.cover_art_height),
            flex_direction: FlexDirection::Column,
            justify_content: JustifyContent::SpaceBetween,
            border: UiRect::all(px(1.0)),
            border_radius: BorderRadius::all(px(5.0)),
            overflow: Overflow::clip(),
            ..default()
        },
        BackgroundColor(artwork.primary.color()),
        BorderColor::all(PALETTE_POSTER_BORDER),
        Decorative,
        PreserveImageColor,
    ));
    // Tag posters of known systems so `update_box_art` can swap in real
    // cover art from the libretro-thumbnails cache when it arrives.
    if let Some(system) = rom.system {
        poster.insert(crate::boxart::BoxArtSlot {
            system_dir: system.thumbnail_dir_for_rom(&rom.path),
            game_name: rom.name.clone(),
        });
    }
    poster.with_children(|poster| {
        for index in 0..3 {
            let offset = (artwork.stripe_seed as usize + index) % 5;
            poster.spawn((
                Node {
                    width: percent(100),
                    height: px(14.0 + offset as f32 * 3.0),
                    ..default()
                },
                BackgroundColor(if index % 2 == 0 {
                    artwork.secondary.color()
                } else {
                    artwork.accent.color()
                }),
            ));
        }
        poster.spawn((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            children![(
                Text::new(artwork.initials.clone()),
                TextFont {
                    font_size: 38.0,
                    ..default()
                },
                TextColor(Color::srgba(1.0, 1.0, 1.0, 0.92)),
            )],
        ));
        poster.spawn((
            Node {
                width: percent(100),
                height: px(8.0),
                ..default()
            },
            BackgroundColor(PALETTE_POSTER_BORDER),
        ));
    });
}

fn spawn_star_rating(parent: &mut ChildSpawnerCommands, icons: &IconAssets, rating: u8) {
    parent
        .spawn(Node {
            width: percent(100),
            height: px(14.0),
            flex_direction: FlexDirection::Row,
            column_gap: px(2.0),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|stars| {
            for index in 0..5 {
                spawn_mini_icon(
                    stars,
                    icons.get(Icon::Star),
                    if index < rating {
                        PALETTE_STAR
                    } else {
                        PALETTE_MUTED_DARK
                    },
                    11.0,
                );
            }
        });
}

fn spawn_game_list(content: &mut ChildSpawnerCommands, roms: &[&RomEntry]) {
    spawn_table_header(content, &["Title", "System", "Core", "Path"]);
    for rom in roms.iter().take(300) {
        spawn_table_row(content, |row| {
            spawn_table_cell(row, &rom.name, 0.30, PALETTE_TEXT);
            spawn_table_cell(
                row,
                rom.system.map(|system| system.name).unwrap_or("Unknown"),
                0.22,
                PALETTE_MUTED,
            );
            spawn_table_cell(
                row,
                rom.core_name.as_deref().unwrap_or("Missing core"),
                0.18,
                if rom.core_name.is_some() {
                    PALETTE_GOOD
                } else {
                    PALETTE_WARN
                },
            );
            spawn_table_cell(row, &rom.path.display().to_string(), 0.30, PALETTE_MUTED);
        });
    }
}

fn spawn_collection_header(parent: &mut ChildSpawnerCommands, title: &str, subtitle: &str) {
    parent
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Column,
            row_gap: px(2.0),
            margin: UiRect::bottom(px(6.0)),
            ..default()
        })
        .with_children(|header| {
            header.spawn((
                Text::new(title.to_string()),
                TextFont {
                    font_size: 24.0,
                    ..default()
                },
                TextColor(PALETTE_TEXT),
            ));
            header.spawn((
                Text::new(subtitle.to_string()),
                TextFont {
                    font_size: 13.0,
                    ..default()
                },
                TextColor(PALETTE_MUTED),
            ));
        });
}

fn spawn_empty_state(parent: &mut ChildSpawnerCommands, label: &str) {
    parent
        .spawn((
            Node {
                width: percent(100),
                min_height: px(110.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(7.0)),
                ..default()
            },
            BackgroundColor(PALETTE_EMPTY),
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(|empty| {
            spawn_text(empty, label, 15.0, PALETTE_MUTED);
        });
}

fn library_collection_title(view: &LibraryView) -> String {
    match view {
        LibraryView::AllGames => "All games".to_string(),
        LibraryView::Other => "Other".to_string(),
        LibraryView::System(id) => system_by_id(id)
            .map(|system| system.name.to_string())
            .unwrap_or_else(|| id.clone()),
        LibraryView::RecentlyAdded => "Recently added".to_string(),
        LibraryView::Steam => "Steam games".to_string(),
    }
}

/// Build a map of system_id → ROM count for all systems that have at least
/// one discovered ROM. Used by the sidebar to show counts.
fn discovered_system_ids(roms: &[RomEntry]) -> std::collections::HashMap<&'static str, usize> {
    let mut counts = std::collections::HashMap::new();
    for rom in roms {
        if let Some(system) = rom.system {
            *counts.entry(system.id).or_insert(0) += 1;
        }
    }
    counts
}

pub fn spawn_first_run(commands: &mut Commands, model: &FrontendModel, icons: &IconAssets) {
    diagnostics::time_block("ui.spawn.first_run", || {
        commands
            .spawn((
                library_root_node(),
                BackgroundColor(PALETTE_BG),
                UiRoot,
                TabGroup::default(),
            ))
            .with_children(|root| {
                spawn_shell_header(root, model, icons, "Setup");
                spawn_library_body(root, model, NavScreen::Setup, |content| {
                    spawn_collection_header(content, "Setup", &model.status);
                    spawn_section(content, "Required");
                    if model.registry.registry.cores.is_empty() {
                        spawn_text(content, "No libretro cores installed.", 16.0, PALETTE_WARN);
                    } else {
                        spawn_text(
                            content,
                            &format!("{} core(s) installed.", model.registry.registry.cores.len()),
                            16.0,
                            PALETTE_GOOD,
                        );
                    }
                    if model.roms.is_empty() {
                        spawn_text(content, "No launchable ROMs found.", 16.0, PALETTE_WARN);
                    } else {
                        spawn_text(
                            content,
                            &format!("{} ROM(s) found.", model.roms.len()),
                            16.0,
                            PALETTE_GOOD,
                        );
                    }
                    spawn_button_variant(
                        content,
                        "Install cores",
                        UiAction::SelectPreferencesSection(PreferencesSection::Cores),
                        ButtonVariant::Primary,
                    );
                    spawn_button(
                        content,
                        "Choose ROMs folder",
                        UiAction::PickPath(PathTarget::Roms),
                    );
                    spawn_button(
                        content,
                        "Choose system files folder",
                        UiAction::PickPath(PathTarget::System),
                    );
                    spawn_button(content, "Refresh scans", UiAction::Refresh);
                });
                spawn_bottom_toolbar(root, model, icons, NavScreen::Setup);
            });
    });
}

pub fn spawn_settings(
    commands: &mut Commands,
    model: &FrontendModel,
    icons: &IconAssets,
    active_model_download: Option<&str>,
) {
    diagnostics::time_block("ui.spawn.settings", || {
        commands
            .spawn((
                library_root_node(),
                BackgroundColor(PALETTE_BG),
                UiRoot,
                TabGroup::default(),
            ))
            .with_children(|root| {
                spawn_shell_header(root, model, icons, "Settings");
                spawn_library_body(root, model, NavScreen::Settings, |content| {
                    spawn_text(content, &model.status, 14.0, PALETTE_MUTED);
                    if let Some(capture) = &model.binding_capture {
                        spawn_text(
                            content,
                            &format!("Press a {}", capture.label()),
                            19.0,
                            PALETTE_ACCENT,
                        );
                    }
                    match model.preferences_section {
                        PreferencesSection::Library => spawn_preferences_library(content, model),
                        PreferencesSection::Gameplay => spawn_preferences_gameplay(content, model),
                        PreferencesSection::Transcription => {
                            spawn_preferences_transcription(content, model, active_model_download)
                        }
                        PreferencesSection::Controls => {
                            spawn_preferences_controls(content, model, icons)
                        }
                        PreferencesSection::Cores => spawn_preferences_cores(content, model),
                        PreferencesSection::SystemFiles => {
                            spawn_preferences_system_files(content, model)
                        }
                    }
                });
                spawn_bottom_toolbar(root, model, icons, NavScreen::Settings);
            });
    });
}

fn spawn_theme_control(parent: &mut ChildSpawnerCommands, selected: ThemePreference) {
    parent
        .spawn((
            Node {
                min_height: px(36.0),
                padding: UiRect::all(px(2.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
                ..default()
            },
            BackgroundColor(PALETTE_SEGMENT_BG),
            BorderColor::all(PALETTE_LINE),
        ))
        .with_children(|segments| {
            for (preference, label) in [
                (ThemePreference::System, "System"),
                (ThemePreference::Light, "Light"),
                (ThemePreference::Dark, "Dark"),
            ] {
                spawn_segment_button(
                    segments,
                    label,
                    UiAction::SetThemePreference(preference),
                    selected == preference,
                    14.0,
                );
            }
        });
}

fn spawn_preferences_library(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    spawn_collection_header(content, "Library", "Folders, organization, and scan status");
    let rom_dir = model
        .config
        .paths
        .roms
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("roms"));
    spawn_path_button(content, "ROMs", &rom_dir, PathTarget::Roms);
    spawn_path_button(
        content,
        "Recordings",
        &model.config.paths.recordings,
        PathTarget::Recordings,
    );
    spawn_path_button(
        content,
        "Saves",
        &model.config.paths.saves,
        PathTarget::Saves,
    );
    spawn_path_button(
        content,
        "States",
        &model.config.paths.states,
        PathTarget::States,
    );

    spawn_section(content, "Organization");
    spawn_text(
        content,
        &format!(
            "{} ROMs across {} collection(s)",
            model.roms.len(),
            model
                .roms
                .iter()
                .filter_map(|rom| rom.system)
                .map(|system| system.id)
                .collect::<BTreeSet<_>>()
                .len()
        ),
        15.0,
        PALETTE_TEXT,
    );
    spawn_preference_summary(content, "Group by console", "On");
    spawn_preference_summary(content, "Generated cover placeholders", "On");
    spawn_preference_summary(content, "Missing-core games", "Visible");
    spawn_button(content, "Refresh library", UiAction::Refresh);
}

fn spawn_preferences_gameplay(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    spawn_collection_header(content, "Gameplay", "Video, audio, and export defaults");
    spawn_section(content, "Video");
    spawn_button(
        content,
        &format!(
            "Integer scaling: {}",
            on_off(model.config.video.integer_scaling)
        ),
        UiAction::ToggleIntegerScaling,
    );
    spawn_button(
        content,
        &format!(
            "Aspect correction: {}",
            on_off(model.config.video.aspect_correction)
        ),
        UiAction::ToggleAspectCorrection,
    );
    spawn_button(
        content,
        &format!("Fullscreen: {}", on_off(model.config.video.fullscreen)),
        UiAction::ToggleFullscreen,
    );

    spawn_section(content, "Audio");
    spawn_button(
        content,
        &format!("Audio: {}", on_off(model.config.audio.enabled)),
        UiAction::ToggleAudio,
    );
    spawn_preference_summary(
        content,
        "Volume",
        &format!("{:.0}%", model.config.audio.volume * 100.0),
    );
    spawn_preference_summary(
        content,
        "Latency",
        &format!("{} ms", model.config.audio.latency_ms),
    );

    spawn_section(content, "Recording");
    spawn_button(
        content,
        &format!(
            "Record microphone: {}",
            on_off(model.config.recording.mic_enabled)
        ),
        UiAction::ToggleMicCapture,
    );
    spawn_section(content, "Recordings");
    if model.recordings.is_empty() {
        spawn_text(
            content,
            "No completed recordings found.",
            16.0,
            PALETTE_TEXT,
        );
    }
    for recording in model.recordings.iter().take(12) {
        let frames = recording
            .frame_count
            .map(|count| format!("{count} frames"))
            .unwrap_or_else(|| "unknown frame count".to_string());
        spawn_text(
            content,
            &format!("{}  {}", recording.name, frames),
            15.0,
            PALETTE_TEXT,
        );
        for target in [
            ExportTarget::Bevy,
            ExportTarget::Unity,
            ExportTarget::Godot,
            ExportTarget::Unreal,
        ] {
            spawn_button(
                content,
                &format!("Export {} to {}", recording.name, target.label()),
                UiAction::ExportRecording {
                    target,
                    session: recording.path.clone(),
                },
            );
        }
    }
}

fn spawn_preferences_transcription(
    content: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    active_model_download: Option<&str>,
) {
    let config = &model.config.recording.transcription;
    spawn_collection_header(
        content,
        "Transcription",
        "Private, local speech recognition of the microphone track",
    );
    spawn_button(
        content,
        &format!("Automatic after recording: {}", on_off(config.automatic)),
        UiAction::ToggleAutomaticTranscription,
    );
    spawn_preference_summary(
        content,
        "Audio source",
        "Microphone only · game audio is never sent to speech recognition",
    );
    let discovered = crate::recording::discover_whisper_executable(config);
    spawn_preference_summary(
        content,
        "Whisper executable",
        &discovered
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not found".into()),
    );
    spawn_button(
        content,
        "Choose existing Whisper executable…",
        UiAction::PickWhisperExecutable,
    );
    spawn_preference_summary(
        content,
        "External Whisper model",
        &config
            .external_model
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not selected".into()),
    );
    spawn_button(
        content,
        "Choose existing Whisper model…",
        UiAction::PickWhisperModel,
    );
    if config.external_model.is_some() {
        spawn_button_variant(
            content,
            "Clear external model",
            UiAction::ClearWhisperModel,
            ButtonVariant::Danger,
        );
    }

    spawn_section(content, "Local models");
    let recommended = crate::transcription::recommended_model_id(config.language.as_deref());
    for descriptor in crate::transcription::model_catalog() {
        let selected = config.selected_model_id.as_deref() == Some(descriptor.id.as_str());
        let recommendation = if descriptor.id == recommended {
            " · Recommended"
        } else {
            ""
        };
        let size_mib = descriptor.download_bytes / (1024 * 1024);
        content
            .spawn((
                Node {
                    width: percent(100),
                    padding: UiRect::all(px(14.0)),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(7.0),
                    border: UiRect::all(px(1.0)),
                    border_radius: BorderRadius::all(px(theme::LAYOUT.card_radius)),
                    ..default()
                },
                BackgroundColor(PALETTE_PREF_CARD),
                BorderColor::all(if selected {
                    PALETTE_ACCENT
                } else {
                    PALETTE_LINE
                }),
                theme::quiet_card_shadow(),
            ))
            .with_children(|card| {
                spawn_text(
                    card,
                    &format!("{}{}", descriptor.display_name, recommendation),
                    18.0,
                    PALETTE_TEXT,
                );
                spawn_text(card, &descriptor.languages.join(", "), 14.0, PALETTE_MUTED);
                spawn_text(
                    card,
                    &format!(
                        "Download {size_mib} MiB · memory {} MiB · {} · {}",
                        descriptor.memory_mib, descriptor.version, descriptor.license
                    ),
                    12.0,
                    PALETTE_MUTED,
                );
                spawn_text(
                    card,
                    &format!("Source: {}", descriptor.source_url),
                    11.0,
                    PALETTE_MUTED_DARK,
                );
                if active_model_download == Some(descriptor.id.as_str()) {
                    spawn_button_variant(
                        card,
                        "Cancel download",
                        UiAction::CancelTranscriptionModelDownload,
                        ButtonVariant::Danger,
                    );
                } else {
                    spawn_button_variant(
                        card,
                        if selected {
                            "Reinstall model"
                        } else {
                            "Download and use"
                        },
                        UiAction::InstallTranscriptionModel(descriptor.id.clone()),
                        if selected {
                            ButtonVariant::Secondary
                        } else {
                            ButtonVariant::Primary
                        },
                    );
                }
            });
    }

    let missing = model
        .recordings
        .iter()
        .filter(|recording| recording.has_mic && recording.transcript_text.is_none())
        .count();
    spawn_section(content, "Backfill");
    spawn_preference_summary(
        content,
        "Sessions waiting for a transcript",
        &format!("{missing} · newest first after setup"),
    );
}

fn spawn_preferences_controls(
    content: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    icons: &IconAssets,
) {
    spawn_collection_header(content, "Controls", "Player 1 defaults and host bindings");
    content
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::FlexStart,
            column_gap: px(22.0),
            ..default()
        })
        .with_children(|columns| {
            columns
                .spawn((
                    Node {
                        width: percent(38),
                        min_height: px(360.0),
                        padding: UiRect::all(px(18.0)),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(14.0),
                        border: UiRect::all(px(1.0)),
                        border_radius: BorderRadius::all(px(theme::LAYOUT.card_radius)),
                        ..default()
                    },
                    BackgroundColor(PALETTE_PREF_CARD),
                    BorderColor::all(PALETTE_LINE_DARK),
                ))
                .with_children(|left| {
                    spawn_controller_illustration(left, icons);
                    spawn_preference_summary(left, "System", "Global");
                    spawn_preference_summary(left, "Player", "Player 1");
                    spawn_preference_summary(left, "Input device", "Keyboard + gamepad");
                    spawn_button(
                        left,
                        "Reset keyboard defaults",
                        UiAction::ResetKeyboardBindings,
                    );
                    spawn_button(
                        left,
                        "Reset gamepad defaults",
                        UiAction::ResetGamepadBindings,
                    );
                });
            columns
                .spawn(Node {
                    width: percent(62),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(8.0),
                    ..default()
                })
                .with_children(|right| {
                    spawn_section(right, "Player 1 keyboard");
                    for control in keyboard_controls() {
                        let binding = model
                            .config
                            .global_input_bindings
                            .keyboard
                            .get(control.name)
                            .cloned()
                            .unwrap_or_else(|| format!("{:?}", control.default_key));
                        spawn_binding_row(
                            right,
                            control.name,
                            &binding,
                            UiAction::BindKeyboard(control.name.to_string()),
                        );
                    }

                    spawn_section(right, "Player 1 keyboard analog");
                    for control in crate::input::keyboard_analog_controls() {
                        let binding = model
                            .config
                            .global_input_bindings
                            .keyboard
                            .get(control.name)
                            .cloned()
                            .unwrap_or_else(|| format!("{:?}", control.default_key));
                        spawn_binding_row(
                            right,
                            control.name,
                            &binding,
                            UiAction::BindKeyboard(control.name.to_string()),
                        );
                    }

                    spawn_section(right, "Player 1 gamepad");
                    for control in crate::input::gamepad_controls() {
                        let binding = model
                            .config
                            .global_input_bindings
                            .gamepad
                            .get(control.name)
                            .cloned()
                            .unwrap_or_else(|| format!("{:?}", control.default_button));
                        spawn_binding_row(
                            right,
                            control.name,
                            &binding,
                            UiAction::BindGamepad(control.name.to_string()),
                        );
                    }
                });
        });
}

fn spawn_preferences_cores(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    spawn_collection_header(content, "Cores", "Installed cores, downloads, and options");
    spawn_section(content, "Installed");
    spawn_path_button(
        content,
        "Cores folder",
        &model.config.paths.cores,
        PathTarget::Cores,
    );
    if model.registry.registry.cores.is_empty() {
        spawn_text(content, "No cores found.", 16.0, PALETTE_TEXT);
    }
    spawn_table_header(content, &["Core", "Version", "Extensions", "Path"]);
    for core in &model.registry.registry.cores {
        spawn_table_row(content, |row| {
            spawn_table_cell(row, &core.library_name, 0.22, PALETTE_TEXT);
            spawn_table_cell(row, &core.library_version, 0.12, PALETTE_MUTED);
            spawn_table_cell(row, &core.valid_extensions.join("|"), 0.26, PALETTE_MUTED);
            spawn_table_cell(row, &core.path.display().to_string(), 0.40, PALETTE_MUTED);
        });
        for ext in &core.valid_extensions {
            spawn_button(
                content,
                &format!("Use {} for .{}", core.library_name, ext),
                UiAction::AssignCore {
                    extension: ext.clone(),
                    core_path: core.path.clone(),
                },
            );
        }
    }

    spawn_section(content, "Downloadable");
    spawn_button(
        content,
        "Refresh core catalog",
        UiAction::RefreshCoreCatalog,
    );
    if model.core_catalog.is_empty() {
        spawn_text(
            content,
            "No downloadable cores loaded.",
            16.0,
            PALETTE_MUTED,
        );
    }
    if !model.core_catalog.is_empty() {
        spawn_table_header(content, &["Core", "System", "Status"]);
    }
    for entry in model.core_catalog.iter().take(160) {
        let system = entry.inferred_system.as_deref().unwrap_or("Unknown system");
        match entry.status {
            CoreInstallStatus::Installed => {
                let path = entry
                    .installed_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "installed".to_string());
                spawn_text(
                    content,
                    &format!("{}  ·  {}  ·  {}", entry.display_name, system, path),
                    14.0,
                    PALETTE_GOOD,
                );
            }
            CoreInstallStatus::Available => {
                spawn_button(
                    content,
                    &format!("Install {}  ·  {}", entry.display_name, system),
                    UiAction::InstallCore(entry.clone()),
                );
            }
        }
    }

    spawn_core_options(content, model);
}

fn spawn_core_options(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    diagnostics::time_block("ui.spawn_core_options", || {
        spawn_section(content, "Core options");
        for core in model.registry.registry.cores.iter().take(8) {
            let load_label = format!(
                "ui.spawn_core_options.probe_variables core={} path={}",
                core.library_name,
                core.path.display()
            );
            // Use `probe_variables` (no `retro_init`) instead of `Core::load`.
            // This is the crash fix: cores like FB Alpha / MAME call C
            // `exit()` from `retro_init` when their BIOS is missing, and a
            // C `exit()` from the main thread kills the process.
            // `probe_variables` calls `retro_set_environment` only, which is
            // safe. Cores that declare variables during `retro_init` (rather
            // than `retro_set_environment`) will return an empty Vec — the UI
            // shows "no options declared" in that case.
            match diagnostics::time_block(load_label, || {
                libretro_host::Core::probe_variables(
                    &core.path,
                    &model.config.paths.system.to_string_lossy(),
                )
            }) {
                Ok((_info, vars)) => {
                    if vars.is_empty() {
                        spawn_text(
                            content,
                            &format!("{}: no options declared", core.library_name),
                            14.0,
                            PALETTE_MUTED,
                        );
                    }
                    for variable in vars.iter().take(12) {
                        let parsed = parse_core_variable(variable);
                        let current = model
                            .config
                            .core_options
                            .get(&core.library_name)
                            .and_then(|options| options.get(&parsed.key))
                            .cloned()
                            .or_else(|| parsed.default_value.clone())
                            .unwrap_or_default();
                        spawn_text(
                            content,
                            &format!(
                                "{} / {}: current {}",
                                core.library_name, parsed.key, current,
                            ),
                            14.0,
                            PALETTE_MUTED,
                        );
                        for value in &parsed.values {
                            spawn_button(
                                content,
                                &format!("Set {} / {} = {}", core.library_name, parsed.key, value),
                                UiAction::SetCoreOption {
                                    core_key: core.library_name.clone(),
                                    option_key: parsed.key.clone(),
                                    value: value.clone(),
                                },
                            );
                        }
                        spawn_button(
                            content,
                            &format!("Clear {} / {} override", core.library_name, parsed.key),
                            UiAction::ClearCoreOption {
                                core_key: core.library_name.clone(),
                                option_key: parsed.key.clone(),
                            },
                        );
                    }
                }
                Err(error) => spawn_text(
                    content,
                    &format!("{}: could not load options: {error}", core.library_name),
                    14.0,
                    PALETTE_BAD,
                ),
            }
        }
    });
}

fn spawn_preferences_system_files(content: &mut ChildSpawnerCommands, model: &FrontendModel) {
    spawn_collection_header(
        content,
        "System files",
        "BIOS and firmware files required by some cores",
    );
    spawn_section(content, "System folder");
    spawn_path_button(
        content,
        "System files",
        &model.config.paths.system,
        PathTarget::System,
    );

    match &model.bios {
        None => spawn_text(
            content,
            "Could not scan the System folder.",
            16.0,
            PALETTE_BAD,
        ),
        Some(bios) => {
            let present = bios
                .checks
                .iter()
                .filter(|check| {
                    matches!(
                        check.status,
                        BiosStatus::Present | BiosStatus::PresentUnverified
                    )
                })
                .count();
            let missing_required = bios.missing_required().count();
            let (summary, color) = if missing_required == 0 {
                (
                    format!("{present}/{} known files present", bios.checks.len()),
                    PALETTE_MUTED,
                )
            } else {
                (
                    format!(
                        "{present}/{} present · {missing_required} required file(s) missing",
                        bios.checks.len()
                    ),
                    PALETTE_WARN,
                )
            };
            spawn_text(content, &summary, 14.0, color);

            let mut last_system: Option<&str> = None;
            for check in &bios.checks {
                if last_system != Some(check.entry.system.as_str()) {
                    spawn_section(content, &check.entry.system);
                    last_system = Some(check.entry.system.as_str());
                }
                let (status_label, color) = bios_status_label_color(&check.status);
                let required = if check.entry.required {
                    "required"
                } else {
                    "optional"
                };
                spawn_system_file_row(
                    content,
                    &check.entry.name,
                    &check.entry.filename,
                    status_label,
                    required,
                    color,
                );
                if !matches!(check.status, BiosStatus::Present) {
                    spawn_button(
                        content,
                        &format!("Import {}", check.entry.filename),
                        UiAction::ImportBios {
                            filename: check.entry.filename.clone(),
                        },
                    );
                }
            }
        }
    }
}

fn bios_status_label_color(status: &BiosStatus) -> (&'static str, Color) {
    match status {
        BiosStatus::Present => ("present", PALETTE_GOOD),
        BiosStatus::PresentUnverified => ("present (unverified)", PALETTE_INFO),
        BiosStatus::Missing => ("missing", PALETTE_WARN),
        BiosStatus::BadHash { .. } => ("bad hash", PALETTE_BAD),
    }
}

pub fn spawn_overlay(commands: &mut Commands) -> Entity {
    diagnostics::time_block("ui.spawn.overlay", || {
        let mut first_focus = None;
        commands
            .spawn((
                Node {
                    width: percent(100),
                    height: percent(100),
                    position_type: PositionType::Absolute,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                BackgroundColor(theme::INGAME.backdrop),
                GlobalZIndex(100),
                UiRoot,
                TabGroup::modal(),
            ))
            .with_children(|parent| {
                parent
                    .spawn((
                        Node {
                            width: px(340.0),
                            padding: UiRect::all(px(18.0)),
                            flex_direction: FlexDirection::Column,
                            row_gap: px(10.0),
                            border: UiRect::all(px(1.0)),
                            border_radius: BorderRadius::all(px(theme::LAYOUT.modal_radius)),
                            ..default()
                        },
                        BorderColor::all(theme::INGAME.border),
                        BackgroundColor(theme::INGAME.panel),
                    ))
                    .with_children(|panel| {
                        spawn_text(panel, "Paused", 28.0, theme::INGAME.text);
                        first_focus = Some(spawn_button_variant(
                            panel,
                            "Resume",
                            UiAction::Resume,
                            ButtonVariant::InGame,
                        ));
                        spawn_button_variant(
                            panel,
                            "Start recording",
                            UiAction::StartRecording,
                            ButtonVariant::InGame,
                        );
                        spawn_button_variant(
                            panel,
                            "Stop recording",
                            UiAction::StopRecording,
                            ButtonVariant::InGameDanger,
                        );
                        spawn_button_variant(
                            panel,
                            "Screenshot",
                            UiAction::Screenshot,
                            ButtonVariant::InGame,
                        );
                        // Save/load state slots: the backend supports 0-255; the
                        // overlay exposes slots 1-4 so users have more than one
                        // save point (item 17 of the post-v1 review).
                        for slot in 1..=4u8 {
                            spawn_button_variant(
                                panel,
                                &format!("Save state slot {slot}"),
                                UiAction::SaveState(slot),
                                ButtonVariant::InGame,
                            );
                            spawn_button_variant(
                                panel,
                                &format!("Load state slot {slot}"),
                                UiAction::LoadState(slot),
                                ButtonVariant::InGame,
                            );
                        }
                        spawn_button_variant(
                            panel,
                            "Quit to library",
                            UiAction::QuitToLibrary,
                            ButtonVariant::InGameDanger,
                        );
                    });
            });
        first_focus.expect("the pause overlay always has a Resume button")
    })
}

/// The floating, non-modal control bar shown during gameplay. Unlike the ESC
/// overlay it does not pause the game or block input. Record/pause labels + the
/// REC indicator are updated live by `update_hud` (in `plugin.rs`).
pub fn spawn_hud_bar(
    commands: &mut Commands,
    icons: &IconAssets,
    recording_active: bool,
    paused: bool,
) {
    diagnostics::time_block("ui.spawn.hud_bar", || {
        commands
            .spawn((
                // Full-width transparent anchor that floats the pill at bottom-center.
                // It paints nothing; gameplay input is read from raw input resources,
                // not UI picking, so this strip never blocks the game.
                Node {
                    position_type: PositionType::Absolute,
                    bottom: px(14.0),
                    left: px(0.0),
                    width: percent(100),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                GlobalZIndex(50),
                UiRoot,
                HudBar,
            ))
            .with_children(|anchor| {
                anchor
                    .spawn((
                        Node {
                            padding: UiRect::axes(px(10.0), px(7.0)),
                            column_gap: px(6.0),
                            flex_direction: FlexDirection::Row,
                            align_items: AlignItems::Center,
                            border: UiRect::all(px(1.0)),
                            border_radius: BorderRadius::all(px(14.0)),
                            ..default()
                        },
                        BackgroundColor(theme::INGAME.panel),
                        BorderColor::all(theme::INGAME.border),
                    ))
                    .with_children(|bar| {
                        // Recording status lamp: red disc while recording, dim otherwise.
                        bar.spawn((
                            Node {
                                width: px(ICON_PX),
                                height: px(ICON_PX),
                                margin: UiRect::right(px(2.0)),
                                ..default()
                            },
                            ImageNode {
                                color: rec_indicator_tint(recording_active),
                                ..ImageNode::new(icons.get(Icon::Record))
                            },
                            RecIndicator,
                        ));
                        spawn_hud_icon_toggle(
                            bar,
                            icons.get(if recording_active {
                                Icon::Stop
                            } else {
                                Icon::Record
                            }),
                            UiAction::ToggleRecording,
                            HudLabel::Record,
                        );
                        spawn_hud_icon_toggle(
                            bar,
                            icons.get(if paused { Icon::Play } else { Icon::Pause }),
                            UiAction::TogglePause,
                            HudLabel::Pause,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::Reset,
                            UiAction::ResetCore,
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::Save,
                            UiAction::SaveState(1),
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::Load,
                            UiAction::LoadState(1),
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::VolumeDown,
                            UiAction::VolumeDown,
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::VolumeUp,
                            UiAction::VolumeUp,
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::Fullscreen,
                            UiAction::ToggleFullscreen,
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::Menu,
                            UiAction::OpenOverlay,
                            theme::INGAME.text,
                        );
                        spawn_hud_icon_button(
                            bar,
                            icons,
                            Icon::Quit,
                            UiAction::QuitToLibrary,
                            theme::INGAME.destructive,
                        );
                    });
            });
    });
}

/// Tint for the recording status lamp given the current record state.
pub fn rec_indicator_tint(active: bool) -> Color {
    if active {
        theme::INGAME.destructive
    } else {
        theme::INGAME.muted
    }
}

/// Which live-updated toggle a HUD icon carries (swapped by `update_hud`).
enum HudLabel {
    Record,
    Pause,
}

fn hud_button_node() -> Node {
    Node {
        min_height: px(34.0),
        min_width: px(34.0),
        padding: UiRect::all(px(6.0)),
        border: UiRect::all(px(1.0)),
        border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
        align_items: AlignItems::Center,
        justify_content: JustifyContent::Center,
        ..default()
    }
}

/// A small square icon widget (used inside buttons and as the REC lamp).
fn spawn_icon(parent: &mut ChildSpawnerCommands, image: Handle<Image>, tint: Color) {
    parent.spawn((
        Node {
            width: px(ICON_PX),
            height: px(ICON_PX),
            ..default()
        },
        ImageNode {
            color: tint,
            ..ImageNode::new(image)
        },
        Decorative,
    ));
}

/// An icon-only HUD button.
fn spawn_hud_icon_button(
    parent: &mut ChildSpawnerCommands,
    icons: &IconAssets,
    icon: Icon,
    action: UiAction,
    tint: Color,
) {
    let accessible_label = action.accessible_label();
    let variant = if matches!(&action, UiAction::QuitToLibrary) {
        ButtonVariant::InGameDanger
    } else {
        ButtonVariant::InGame
    };
    let visual = theme::current_button_visual(variant, Interaction::None, false);
    parent
        .spawn((
            Button,
            hud_button_node(),
            BorderColor::all(visual.border),
            BackgroundColor(visual.background),
            UiButton { action, variant },
        ))
        .with_children(|button| {
            spawn_icon(button, icons.get(icon), tint);
            spawn_hidden_accessible_label(button, &accessible_label);
        });
}

/// A HUD icon button whose icon is swapped live by `update_hud` (Record⇄Stop,
/// Pause⇄Play). The live-update marker lives on the inner `ImageNode`.
fn spawn_hud_icon_toggle(
    parent: &mut ChildSpawnerCommands,
    image: Handle<Image>,
    action: UiAction,
    which: HudLabel,
) {
    let accessible_label = action.accessible_label();
    let visual = theme::current_button_visual(ButtonVariant::InGame, Interaction::None, false);
    parent
        .spawn((
            Button,
            hud_button_node(),
            BorderColor::all(visual.border),
            BackgroundColor(visual.background),
            UiButton {
                action,
                variant: ButtonVariant::InGame,
            },
        ))
        .with_children(|button| {
            let icon = (
                Node {
                    width: px(ICON_PX),
                    height: px(ICON_PX),
                    ..default()
                },
                ImageNode {
                    color: visual.foreground,
                    ..ImageNode::new(image)
                },
                Decorative,
            );
            match which {
                HudLabel::Record => {
                    button.spawn((icon, RecordButtonLabel));
                }
                HudLabel::Pause => {
                    button.spawn((icon, PauseButtonLabel));
                }
            }
            spawn_hidden_accessible_label(button, &accessible_label);
        });
}

pub fn button_interactions(
    mut query: UiButtonVisuals,
    mut labels: Query<&mut TextColor>,
    mut icons: Query<&mut ImageNode, (Without<SystemBadge>, Without<PreserveImageColor>)>,
) {
    for (
        _entity,
        interaction,
        mut background,
        mut border,
        mut button,
        ui_button,
        selected,
        children,
    ) in &mut query
    {
        let visual =
            theme::current_button_visual(ui_button.variant, *interaction, selected.is_some());
        *background = BackgroundColor(visual.background);
        *border = BorderColor::all(visual.border);
        if let Some(children) = children {
            for child in children.iter() {
                if let Ok(mut label) = labels.get_mut(child) {
                    label.0 = visual.foreground;
                }
                if let Ok(mut icon) = icons.get_mut(child) {
                    icon.color = visual.foreground;
                }
            }
        }
        if interaction.is_changed()
            && matches!(*interaction, Interaction::Hovered | Interaction::Pressed)
        {
            button.set_changed();
        }
    }
}

pub fn focus_outlines(
    mut commands: Commands,
    focus: Res<InputFocus>,
    visible: Res<InputFocusVisible>,
    buttons: Query<Entity, With<UiButton>>,
) {
    if !focus.is_changed() && !visible.is_changed() {
        return;
    }
    let color = theme::palette(theme::current_theme()).focus_ring;
    for entity in &buttons {
        if visible.0 && focus.get() == Some(entity) {
            commands.entity(entity).insert(Outline {
                color,
                width: px(2.0),
                offset: px(2.0),
            });
        } else {
            commands.entity(entity).remove::<Outline>();
        }
    }
}

pub fn sync_theme_segments(
    mut commands: Commands,
    model: Res<FrontendModel>,
    segments: Query<(Entity, &UiButton, Option<&Selected>)>,
) {
    if !model.is_changed() {
        return;
    }
    for (entity, button, selected) in &segments {
        let UiAction::SetThemePreference(preference) = &button.action else {
            continue;
        };
        let should_be_selected = *preference == model.config.appearance.theme;
        match (selected.is_some(), should_be_selected) {
            (false, true) => {
                commands.entity(entity).insert(Selected);
            }
            (true, false) => {
                commands.entity(entity).remove::<Selected>();
            }
            _ => {}
        }
    }
}

pub fn scan_rom_dirs(config: &RetroFeelConfig, registry: &CoreScanReport) -> Vec<RomEntry> {
    let mut roms = Vec::new();
    for dir in &config.paths.roms {
        scan_rom_dir(dir, config, registry, &mut roms);
    }
    roms.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    roms
}

fn scan_rom_dir(
    dir: &Path,
    config: &RetroFeelConfig,
    registry: &CoreScanReport,
    roms: &mut Vec<RomEntry>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_rom_dir(&path, config, registry, roms);
            continue;
        }
        let core = registry.registry.resolve_rom(&path, Some(config), None);
        let core_name = core.map(|core| core.library_name.clone());
        let system = system_for_rom(&path, core_name.as_deref());
        if core_name.is_none() && config.core_override_for_rom(&path).is_none() && system.is_none()
        {
            continue;
        }
        let name = path
            .file_stem()
            .or_else(|| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("ROM")
            .to_string();
        roms.push(RomEntry {
            name,
            path,
            core_name,
            system,
        });
    }
}

pub fn scan_recordings_dir(dir: &Path) -> Vec<RecordingEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut recordings = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        if let Some(recording) = load_recording_entry(path) {
            recordings.push(recording);
        }
    }
    recordings.sort_by(|a, b| b.name.cmp(&a.name));
    recordings
}

pub(crate) fn load_recording_entry(path: PathBuf) -> Option<RecordingEntry> {
    let manifest: SessionManifest =
        serde_json::from_reader(std::fs::File::open(path.join("manifest.json")).ok()?).ok()?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recording")
        .to_string();
    let title = manifest
        .rom
        .as_ref()
        .and_then(|rom| Path::new(&rom.path).file_stem())
        .and_then(|stem| stem.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            manifest
                .core
                .name
                .strip_prefix("steam:")
                .map(|_| manifest.core.version.clone())
                .unwrap_or_else(|| manifest.core.name.clone())
        });
    let transcript_path = manifest
        .transcript_json
        .as_deref()
        .map(|value| resolve_recording_artifact(&path, value))
        .or_else(|| Some(path.join("transcript.json")).filter(|path| path.is_file()));
    let transcript = transcript_path
        .as_deref()
        .and_then(|path| std::fs::File::open(path).ok())
        .and_then(|file| serde_json::from_reader::<_, TranscriptDocument>(file).ok());
    let transcript_text = transcript.as_ref().map(TranscriptDocument::plain_text);
    let transitions_path = manifest
        .input_transitions
        .as_deref()
        .map(|value| resolve_recording_artifact(&path, value))
        .or_else(|| {
            Some(path.join("input-transitions.jsonl")).filter(|candidate| candidate.is_file())
        });
    let transition_summary = transitions_path
        .as_deref()
        .and_then(load_input_transitions)
        .map(|transitions| {
            summarize_transitions(&transitions, manifest.frame_count, manifest.timing.fps)
        });
    let (input_summary, input_changes) = transition_summary.unwrap_or_else(|| {
        let inputs: Vec<InputFrame> =
            std::fs::File::open(resolve_recording_artifact(&path, &manifest.input_log))
                .ok()
                .and_then(|file| serde_json::from_reader(file).ok())
                .unwrap_or_default();
        summarize_inputs(&inputs, manifest.timing.fps)
    });
    let feel_package = FeelPackage::open(&path).ok();
    let feel_manifest = feel_package
        .as_ref()
        .map(|package| package.manifest().clone());
    let alignment = feel_package.as_ref().and_then(|package| {
        let transcript = package.primary_transcript()?;
        let relative = transcript.alignment_path.as_deref()?;
        let path = package.resolve(relative).ok()?;
        serde_json::from_reader(std::fs::File::open(path).ok()?).ok()
    });
    let latest_analysis = feel_package.as_ref().and_then(|package| {
        let run = package.manifest().analyses.last()?;
        let path = package.resolve(&run.result_path).ok()?;
        serde_json::from_reader(std::fs::File::open(path).ok()?).ok()
    });
    let title = feel_manifest
        .as_ref()
        .map(|manifest| manifest.title.clone())
        .unwrap_or(title);
    Some(RecordingEntry {
        path: path.clone(),
        name,
        frame_count: Some(manifest.frame_count),
        title,
        core_name: manifest.core.name.clone(),
        start_timestamp: manifest.timing.start_timestamp,
        duration_seconds: if manifest.timing.fps > 0.0 {
            manifest.frame_count as f64 / manifest.timing.fps
        } else {
            0.0
        },
        dropped_frames: manifest.dropped_frames,
        has_video: manifest
            .video
            .as_deref()
            .is_some_and(|value| resolve_recording_artifact(&path, value).is_file()),
        has_game_audio: manifest.track_alignment.as_ref().is_some_and(|alignment| {
            alignment.game_audio.presence == retrofeel_types::TrackPresence::Present
        }) || path.join("audio.wav").is_file(),
        has_mic: manifest
            .mic_audio
            .as_deref()
            .is_some_and(|value| resolve_recording_artifact(&path, value).is_file()),
        transcription_status: manifest.transcription_status.clone(),
        transcript_text,
        transcript,
        input_summary,
        input_changes,
        manifest: Some(manifest),
        feel_manifest,
        alignment,
        latest_analysis,
    })
}

fn resolve_recording_artifact(session: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        if path.starts_with(session) {
            path.to_path_buf()
        } else {
            path.file_name()
                .map(|name| session.join(name))
                .unwrap_or_else(|| session.to_path_buf())
        }
    } else {
        session.join(path)
    }
}

fn recording_db_row(recording: &RecordingEntry) -> Option<retrofeel_db::recordings::RecordingRow> {
    let manifest = recording.manifest.as_ref()?;
    Some(retrofeel_db::recordings::RecordingRow {
        session_dir: recording.path.clone(),
        core_name: manifest.core.name.clone(),
        core_version: manifest.core.version.clone(),
        rom_path: manifest.rom.as_ref().map(|rom| PathBuf::from(&rom.path)),
        rom_sha1: manifest.rom.as_ref().map(|rom| rom.sha1.clone()),
        frame_count: manifest.frame_count,
        dropped_frames: manifest.dropped_frames,
        fps: manifest.timing.fps,
        sample_rate: manifest.timing.sample_rate,
        start_timestamp: manifest.timing.start_timestamp,
        video_path: manifest.video.as_deref().map(PathBuf::from),
        input_log_path: PathBuf::from(&manifest.input_log),
        manifest_path: recording.path.join("manifest.json"),
        transcript_text: recording.transcript_text.clone(),
        transcript_path: manifest.transcript_json.as_deref().map(PathBuf::from),
        transcript_status: transcription_status_key(&manifest.transcription_status).into(),
        transcription_model: manifest.transcription_model.clone(),
        input_summary_json: serde_json::to_string(&recording.input_summary).ok(),
        source_kind: capture_source_kind(manifest).into(),
        video_timing_mode: video_timing_mode(manifest).into(),
        frame_map_path: manifest
            .frame_map
            .as_ref()
            .map(|map| PathBuf::from(&map.path)),
        input_transitions_path: manifest.input_transitions.as_deref().map(PathBuf::from),
        narration_offset_us: manifest
            .track_alignment
            .as_ref()
            .and_then(|alignment| alignment.narration.offset_us),
        narration_uncertainty_us: manifest
            .track_alignment
            .as_ref()
            .and_then(|alignment| alignment.narration.uncertainty_us),
        narration_status: manifest
            .track_alignment
            .as_ref()
            .map(|alignment| track_status_key(alignment.narration.status))
            .unwrap_or("unavailable")
            .into(),
        narration_presence: manifest
            .track_alignment
            .as_ref()
            .map(|alignment| track_presence_key(alignment.narration.presence))
            .unwrap_or("unknown")
            .into(),
        game_audio_presence: manifest
            .track_alignment
            .as_ref()
            .map(|alignment| track_presence_key(alignment.game_audio.presence))
            .unwrap_or_else(|| {
                if recording.has_game_audio {
                    "present"
                } else {
                    "unknown"
                }
            })
            .into(),
        created_at: manifest.timing.start_timestamp.max(0.0) as u64,
    })
}

fn capture_source_kind(manifest: &SessionManifest) -> &'static str {
    match manifest
        .capture_provenance
        .as_ref()
        .map(|provenance| provenance.kind)
    {
        Some(CaptureSourceKind::Libretro) => "libretro",
        Some(CaptureSourceKind::MacosScreenCaptureKit) => "macos_screencapturekit",
        Some(CaptureSourceKind::SteamGameRecording) => "steam_game_recording",
        None => "legacy",
    }
}

fn video_timing_mode(manifest: &SessionManifest) -> &'static str {
    match manifest.video_timing.as_ref().map(|timing| timing.kind) {
        Some(retrofeel_types::VideoTimingKind::FrameIndexed) => "frame_indexed",
        Some(retrofeel_types::VideoTimingKind::CfrResampledSck) => "cfr_resampled_sck",
        Some(retrofeel_types::VideoTimingKind::ExternalVariableFrameRate) => {
            "external_variable_frame_rate"
        }
        None => "legacy_frame_indexed",
    }
}

fn track_status_key(status: TrackAlignmentStatus) -> &'static str {
    match status {
        TrackAlignmentStatus::Complete => "complete",
        TrackAlignmentStatus::Degraded => "degraded",
        TrackAlignmentStatus::NotCaptured => "not_captured",
        TrackAlignmentStatus::Unavailable => "unavailable",
    }
}

fn track_presence_key(presence: TrackPresence) -> &'static str {
    match presence {
        TrackPresence::Present => "present",
        TrackPresence::NotCaptured => "not_captured",
        TrackPresence::Unavailable => "unavailable",
    }
}

fn transcription_status_key(status: &TranscriptionJobState) -> &'static str {
    match status {
        TranscriptionJobState::NotRequested => "not_requested",
        TranscriptionJobState::Queued => "queued",
        TranscriptionJobState::Running { .. } => "running",
        TranscriptionJobState::Complete => "complete",
        TranscriptionJobState::Cancelled => "cancelled",
        TranscriptionJobState::Failed { .. } => "failed",
    }
}

fn summarize_inputs(frames: &[InputFrame], fps: f64) -> (InputActivitySummary, Vec<InputChange>) {
    let mut summary = InputActivitySummary::default();
    let mut controls = BTreeSet::new();
    let mut changes = Vec::new();
    let mut previous: Option<&InputFrame> = None;
    for frame in frames {
        let raw = frame.raw_host.as_ref();
        if raw.is_some_and(|raw| !raw.keyboard_keys.is_empty()) {
            summary.keyboard_frames += 1;
        }
        if raw.is_some_and(|raw| !raw.gamepad_buttons.is_empty() || !raw.gamepad_axes.is_empty()) {
            summary.gamepad_frames += 1;
        }
        if raw
            .and_then(|raw| raw.mouse)
            .is_some_and(|mouse| mouse.dx != 0 || mouse.dy != 0 || mouse.buttons != 0)
        {
            summary.mouse_frames += 1;
        }
        if let Some(raw) = raw {
            controls.extend(raw.keyboard_keys.iter().cloned());
            controls.extend(raw.gamepad_buttons.iter().cloned());
            controls.extend(raw.gamepad_axes.keys().cloned());
            if raw.mouse.is_some() {
                controls.insert("Mouse".to_string());
            }
        }
        for (name, bit) in mapped_button_names() {
            if frame.state.buttons.has(bit) {
                controls.insert(name.to_string());
            }
        }
        let changed = previous.is_none_or(|previous| {
            previous.state != frame.state || previous.raw_host != frame.raw_host
        });
        if changed {
            changes.push(InputChange {
                frame: frame.frame,
                elapsed_seconds: frame
                    .elapsed_us
                    .map(|value| value as f64 / 1_000_000.0)
                    .unwrap_or_else(|| {
                        if fps > 0.0 {
                            frame.frame as f64 / fps
                        } else {
                            0.0
                        }
                    }),
                mapped: describe_mapped_input(&frame.state),
                raw: describe_raw_input(frame.raw_host.as_ref()),
            });
        }
        previous = Some(frame);
    }
    summary.change_count = changes.len() as u64;
    summary.control_names = controls.into_iter().collect();
    (summary, changes)
}

fn load_input_transitions(path: &Path) -> Option<Vec<InputTransition>> {
    let reader = BufReader::new(std::fs::File::open(path).ok()?);
    reader
        .lines()
        .map(|line| serde_json::from_str(&line.ok()?).ok())
        .collect()
}

fn summarize_transitions(
    transitions: &[InputTransition],
    frame_count: u64,
    fps: f64,
) -> (InputActivitySummary, Vec<InputChange>) {
    let mut summary = InputActivitySummary::default();
    let mut controls = BTreeSet::new();
    let mut changes = Vec::with_capacity(transitions.len());
    for (index, transition) in transitions.iter().enumerate() {
        let next_frame = transitions
            .get(index + 1)
            .map(|next| next.frame)
            .unwrap_or(frame_count);
        let held_frames = next_frame.saturating_sub(transition.frame);
        let raw = transition.raw_host.as_ref();
        if raw.is_some_and(|raw| !raw.keyboard_keys.is_empty()) {
            summary.keyboard_frames = summary.keyboard_frames.saturating_add(held_frames);
        }
        if raw.is_some_and(|raw| !raw.gamepad_buttons.is_empty() || !raw.gamepad_axes.is_empty()) {
            summary.gamepad_frames = summary.gamepad_frames.saturating_add(held_frames);
        }
        if raw
            .and_then(|raw| raw.mouse)
            .is_some_and(|mouse| mouse.dx != 0 || mouse.dy != 0 || mouse.buttons != 0)
        {
            summary.mouse_frames = summary.mouse_frames.saturating_add(held_frames);
        }
        if let Some(raw) = raw {
            controls.extend(raw.keyboard_keys.iter().cloned());
            controls.extend(raw.gamepad_buttons.iter().cloned());
            controls.extend(raw.gamepad_axes.keys().cloned());
            if raw.mouse.is_some() {
                controls.insert("Mouse".to_string());
            }
        }
        for (name, bit) in mapped_button_names() {
            if transition.state.buttons.has(bit) {
                controls.insert(name.to_string());
            }
        }
        changes.push(InputChange {
            frame: transition.frame,
            elapsed_seconds: transition
                .elapsed_us
                .map(|value| value as f64 / 1_000_000.0)
                .unwrap_or_else(|| {
                    if fps > 0.0 {
                        transition.frame as f64 / fps
                    } else {
                        0.0
                    }
                }),
            mapped: describe_mapped_input(&transition.state),
            raw: describe_raw_input(raw),
        });
    }
    summary.change_count = changes.len() as u64;
    summary.control_names = controls.into_iter().collect();
    (summary, changes)
}

fn mapped_button_names() -> [(&'static str, u16); 16] {
    [
        ("B", 0),
        ("Y", 1),
        ("Select", 2),
        ("Start", 3),
        ("Up", 4),
        ("Down", 5),
        ("Left", 6),
        ("Right", 7),
        ("A", 8),
        ("X", 9),
        ("L", 10),
        ("R", 11),
        ("L2", 12),
        ("R2", 13),
        ("L3", 14),
        ("R3", 15),
    ]
}

fn describe_mapped_input(state: &retrofeel_types::InputState) -> String {
    let mut active = mapped_button_names()
        .into_iter()
        .filter_map(|(name, bit)| state.buttons.has(bit).then_some(name.to_string()))
        .collect::<Vec<_>>();
    if state.analog_l.x != 0 || state.analog_l.y != 0 {
        active.push(format!(
            "Left stick {},{}",
            state.analog_l.x, state.analog_l.y
        ));
    }
    if state.analog_r.x != 0 || state.analog_r.y != 0 {
        active.push(format!(
            "Right stick {},{}",
            state.analog_r.x, state.analog_r.y
        ));
    }
    if active.is_empty() {
        "Idle".into()
    } else {
        active.join(" + ")
    }
}

fn describe_raw_input(raw: Option<&retrofeel_types::RawHostInput>) -> String {
    let Some(raw) = raw else { return "None".into() };
    let mut active = raw.keyboard_keys.clone();
    active.extend(raw.gamepad_buttons.iter().cloned());
    active.extend(
        raw.gamepad_axes
            .iter()
            .filter(|(_, value)| value.abs() > 0.01)
            .map(|(name, value)| format!("{name}={value:.2}")),
    );
    if let Some(mouse) = raw.mouse {
        if mouse.dx != 0 || mouse.dy != 0 || mouse.buttons != 0 {
            active.push(format!(
                "Mouse {},{} buttons={}",
                mouse.dx, mouse.dy, mouse.buttons
            ));
        }
    }
    if active.is_empty() {
        "Idle".into()
    } else {
        active.join(" + ")
    }
}

fn recording_search_text(recording: &RecordingEntry) -> String {
    format!(
        "{} {} {} {} {}",
        recording.title,
        recording.name,
        recording.core_name,
        recording.transcript_text.as_deref().unwrap_or_default(),
        recording.input_summary.control_names.join(" ")
    )
    .to_ascii_lowercase()
}

/// Which top-level destination the persistent app shell is displaying.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NavScreen {
    Library,
    Settings,
    Setup,
}

/// Context-specific navigation sits above the content instead of competing
/// with primary destinations and utility actions in a left rail.
fn spawn_context_navigation(
    parent: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    current: NavScreen,
) {
    if current == NavScreen::Setup
        || (current == NavScreen::Library && model.library_top_view != LibraryTopView::Games)
    {
        return;
    }

    parent
        .spawn((
            Node {
                width: percent(100),
                min_height: px(48.0),
                padding: UiRect::axes(px(theme::LAYOUT.space_4), px(theme::LAYOUT.space_2)),
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                align_items: AlignItems::Center,
                row_gap: px(theme::LAYOUT.space_1),
                column_gap: px(theme::LAYOUT.space_1),
                border: UiRect::bottom(px(1.0)),
                ..default()
            },
            BackgroundColor(PALETTE_TOOLBAR),
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(|navigation| {
            if current == NavScreen::Library {
                spawn_context_button(
                    navigation,
                    "All games",
                    UiAction::SelectLibraryView(LibraryView::AllGames),
                    model.library_view == LibraryView::AllGames,
                );
                spawn_context_button(
                    navigation,
                    "Recently added",
                    UiAction::SelectLibraryView(LibraryView::RecentlyAdded),
                    model.library_view == LibraryView::RecentlyAdded,
                );

                let systems_with_roms = discovered_system_ids(&model.roms);
                let has_other = model.roms.iter().any(|rom| rom.system.is_none());

                for system in SYSTEMS {
                    let selected =
                        matches!(&model.library_view, LibraryView::System(id) if id == system.id);
                    let count = systems_with_roms.get(system.id).copied().unwrap_or(0);
                    if count == 0 {
                        continue;
                    }
                    let label = format!("{} ({})", system.name, count);
                    spawn_context_button(
                        navigation,
                        &label,
                        UiAction::SelectLibraryView(LibraryView::System(system.id.to_string())),
                        selected,
                    );
                }
                if has_other {
                    spawn_context_button(
                        navigation,
                        "Other",
                        UiAction::SelectLibraryView(LibraryView::Other),
                        model.library_view == LibraryView::Other,
                    );
                }

                let steam_count = model.config.steam.games.len();
                let steam_label = if steam_count > 0 {
                    format!("Steam ({steam_count})")
                } else {
                    "Steam".to_string()
                };
                spawn_context_button(
                    navigation,
                    &steam_label,
                    UiAction::SelectLibraryView(LibraryView::Steam),
                    model.library_view == LibraryView::Steam,
                );
            } else if current == NavScreen::Settings {
                for section in PreferencesSection::ALL {
                    spawn_context_button(
                        navigation,
                        section.label(),
                        UiAction::SelectPreferencesSection(section),
                        model.preferences_section == section,
                    );
                }
            }
        });
}

fn spawn_context_button(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    action: UiAction,
    selected: bool,
) {
    let visual = theme::current_button_visual(ButtonVariant::Nav, Interaction::None, selected);
    let mut entity = parent.spawn((
        Button,
        Node {
            min_height: px(32.0),
            padding: UiRect::axes(px(11.0), px(5.0)),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(px(1.0)),
            border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
            ..default()
        },
        BorderColor::all(visual.border),
        BackgroundColor(visual.background),
        UiButton {
            action,
            variant: ButtonVariant::Nav,
        },
        children![(
            Text::new(label.to_string()),
            TextFont {
                font_size: 13.0,
                ..default()
            },
            TextColor(visual.foreground),
        )],
    ));
    if selected {
        entity.insert(Selected);
    }
}

fn bottom_toolbar_node() -> Node {
    Node {
        width: percent(100),
        min_height: px(56.0),
        padding: UiRect::axes(px(theme::LAYOUT.space_4), px(theme::LAYOUT.space_2)),
        flex_direction: FlexDirection::Row,
        align_items: AlignItems::Center,
        column_gap: px(theme::LAYOUT.space_2),
        border: UiRect::top(px(1.0)),
        ..default()
    }
}

fn spawn_bottom_toolbar(
    parent: &mut ChildSpawnerCommands,
    model: &FrontendModel,
    icons: &IconAssets,
    current: NavScreen,
) {
    parent
        .spawn((
            bottom_toolbar_node(),
            BackgroundColor(PALETTE_TOOLBAR),
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(|toolbar| {
            spawn_utility_button(
                toolbar,
                icons,
                Icon::Library,
                "Library",
                UiAction::GoLibrary,
                current == NavScreen::Library,
            );
            spawn_utility_button(
                toolbar,
                icons,
                Icon::Settings,
                "Settings",
                UiAction::GoSettings,
                current == NavScreen::Settings,
            );
            toolbar.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            spawn_utility_button(
                toolbar,
                icons,
                Icon::Refresh,
                "Refresh",
                UiAction::Refresh,
                false,
            );
            spawn_theme_control(toolbar, model.config.appearance.theme);
        });
}

fn spawn_utility_button(
    parent: &mut ChildSpawnerCommands,
    icons: &IconAssets,
    icon: Icon,
    label: &str,
    action: UiAction,
    selected: bool,
) {
    let visual = theme::current_button_visual(ButtonVariant::Nav, Interaction::None, selected);
    let mut entity = parent.spawn((
        Button,
        Node {
            min_width: px(108.0),
            height: px(38.0),
            padding: UiRect::axes(px(12.0), px(7.0)),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            column_gap: px(8.0),
            border: UiRect::all(px(1.0)),
            border_radius: BorderRadius::all(px(theme::LAYOUT.control_radius)),
            ..default()
        },
        BorderColor::all(visual.border),
        BackgroundColor(visual.background),
        UiButton {
            action,
            variant: ButtonVariant::Nav,
        },
    ));
    entity.with_children(|button| {
        spawn_mini_icon(button, icons.get(icon), visual.foreground, 17.0);
        spawn_text(button, label, 14.0, visual.foreground);
    });
    if selected {
        entity.insert(Selected);
    }
}

/// The standard scrollable content panel to the right of the nav sidebar, used
/// by every top-level screen so their chrome matches.
fn spawn_spacer(parent: &mut ChildSpawnerCommands, height: f32) {
    parent.spawn(Node {
        height: px(height),
        ..default()
    });
}

/// A small, muted, uppercase section header — the shared divider style across
/// the sidebar and content panels.
fn spawn_section(parent: &mut ChildSpawnerCommands, label: &str) {
    spawn_spacer(parent, 6.0);
    spawn_text(parent, &label.to_uppercase(), 13.0, PALETTE_MUTED);
}

fn spawn_toolbar_icon_button(
    parent: &mut ChildSpawnerCommands,
    icons: &IconAssets,
    icon: Icon,
    action: UiAction,
    selected: bool,
) {
    let visual = theme::current_button_visual(ButtonVariant::Icon, Interaction::None, selected);
    let accessible_label = action.accessible_label();
    let mut entity = parent.spawn((
        Button,
        Node {
            width: px(30.0),
            height: px(30.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(px(1.0)),
            border_radius: BorderRadius::all(px(6.0)),
            ..default()
        },
        BorderColor::all(visual.border),
        BackgroundColor(visual.background),
        UiButton {
            action,
            variant: ButtonVariant::Icon,
        },
    ));
    entity.with_children(|button| {
        spawn_mini_icon(button, icons.get(icon), visual.foreground, 15.0);
        spawn_hidden_accessible_label(button, &accessible_label);
    });
    if selected {
        entity.insert(Selected);
    }
}

fn spawn_segment_button(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    action: UiAction,
    selected: bool,
    font_size: f32,
) {
    let visual = theme::current_button_visual(ButtonVariant::Segment, Interaction::None, selected);
    let mut entity = parent.spawn((
        Button,
        Node {
            min_width: px(82.0),
            height: px(24.0),
            padding: UiRect::axes(px(10.0), px(3.0)),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: BorderRadius::all(px(5.0)),
            ..default()
        },
        BackgroundColor(visual.background),
        BorderColor::all(visual.border),
        UiButton {
            action,
            variant: ButtonVariant::Segment,
        },
    ));
    entity.with_children(|button| {
        button.spawn((
            Text::new(label.to_string()),
            TextFont {
                font_size,
                ..default()
            },
            TextColor(visual.foreground),
        ));
    });
    if selected {
        entity.insert(Selected);
    }
}

fn spawn_mini_icon(
    parent: &mut ChildSpawnerCommands,
    image: Handle<Image>,
    tint: Color,
    size: f32,
) {
    parent.spawn((
        Node {
            width: px(size),
            height: px(size),
            ..default()
        },
        ImageNode {
            color: tint,
            ..ImageNode::new(image)
        },
        Decorative,
    ));
}

fn spawn_hidden_accessible_label(parent: &mut ChildSpawnerCommands, label: &str) {
    parent.spawn((
        Text::new(label.to_string()),
        TextFont {
            font_size: 1.0,
            ..default()
        },
        TextColor(Color::NONE),
        Node {
            display: Display::None,
            ..default()
        },
    ));
}

fn spawn_preference_summary(parent: &mut ChildSpawnerCommands, label: &str, value: &str) {
    spawn_table_row(parent, |row| {
        spawn_table_cell(row, label, 0.45, PALETTE_TEXT);
        spawn_table_cell(row, value, 0.55, PALETTE_MUTED);
    });
}

fn spawn_binding_row(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    binding: &str,
    action: UiAction,
) {
    parent
        .spawn((
            Button,
            Node {
                width: percent(100),
                min_height: px(34.0),
                padding: UiRect::axes(px(10.0), px(6.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                border: UiRect::all(px(1.0)),
                border_radius: BorderRadius::all(px(6.0)),
                column_gap: px(12.0),
                ..default()
            },
            BackgroundColor(PALETTE_ROW),
            BorderColor::all(PALETTE_LINE_DARK),
            UiButton {
                action,
                variant: ButtonVariant::Secondary,
            },
        ))
        .with_children(|row| {
            spawn_table_cell(row, label, 0.46, PALETTE_TEXT);
            spawn_table_cell(row, binding, 0.54, PALETTE_ACCENT);
        });
}

fn spawn_table_header(parent: &mut ChildSpawnerCommands, labels: &[&str]) {
    parent
        .spawn((
            Node {
                width: percent(100),
                min_height: px(28.0),
                padding: UiRect::axes(px(10.0), px(5.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                border: UiRect::bottom(px(1.0)),
                ..default()
            },
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(|row| {
            let width = if labels.is_empty() {
                100.0
            } else {
                100.0 / labels.len() as f32
            };
            for label in labels {
                row.spawn((
                    Node {
                        width: percent(width),
                        ..default()
                    },
                    children![(
                        Text::new(label.to_ascii_uppercase()),
                        TextFont {
                            font_size: 11.0,
                            ..default()
                        },
                        TextColor(PALETTE_MUTED),
                    )],
                ));
            }
        });
}

fn spawn_table_row(
    parent: &mut ChildSpawnerCommands,
    build: impl FnOnce(&mut ChildSpawnerCommands),
) {
    parent
        .spawn((
            Node {
                width: percent(100),
                min_height: px(34.0),
                padding: UiRect::axes(px(10.0), px(6.0)),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                border: UiRect::bottom(px(1.0)),
                column_gap: px(10.0),
                ..default()
            },
            BackgroundColor(PALETTE_ROW),
            BorderColor::all(PALETTE_LINE_DARK),
        ))
        .with_children(build);
}

fn spawn_table_cell(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    width_fraction: f32,
    color: Color,
) {
    parent.spawn((
        Node {
            width: percent((width_fraction * 100.0).clamp(1.0, 100.0)),
            ..default()
        },
        children![(
            Text::new(label.to_string()),
            TextFont {
                font_size: 13.0,
                ..default()
            },
            TextColor(color),
        )],
    ));
}

fn spawn_system_file_row(
    parent: &mut ChildSpawnerCommands,
    name: &str,
    filename: &str,
    status: &str,
    required: &str,
    status_color: Color,
) {
    spawn_table_row(parent, |row| {
        spawn_table_cell(row, name, 0.30, PALETTE_TEXT);
        spawn_table_cell(row, filename, 0.30, PALETTE_MUTED);
        spawn_table_cell(row, status, 0.22, status_color);
        spawn_table_cell(row, required, 0.18, PALETTE_MUTED);
    });
}

fn spawn_controller_illustration(parent: &mut ChildSpawnerCommands, icons: &IconAssets) {
    parent
        .spawn((
            Node {
                width: percent(100),
                height: px(190.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            BackgroundColor(Color::NONE),
        ))
        .with_children(|stage| {
            stage
                .spawn((
                    Node {
                        width: px(250.0),
                        height: px(135.0),
                        padding: UiRect::all(px(14.0)),
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::SpaceBetween,
                        border: UiRect::all(px(1.0)),
                        border_radius: BorderRadius::all(px(34.0)),
                        ..default()
                    },
                    BackgroundColor(Color::srgb(0.18, 0.19, 0.20)),
                    BorderColor::all(PALETTE_LINE),
                ))
                .with_children(|pad| {
                    pad.spawn((
                        Node {
                            width: px(68.0),
                            height: px(68.0),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            ..default()
                        },
                        children![
                            (
                                Node {
                                    width: px(52.0),
                                    height: px(18.0),
                                    position_type: PositionType::Absolute,
                                    border_radius: BorderRadius::all(px(4.0)),
                                    ..default()
                                },
                                BackgroundColor(PALETTE_BUTTON_ACTIVE),
                            ),
                            (
                                Node {
                                    width: px(18.0),
                                    height: px(52.0),
                                    position_type: PositionType::Absolute,
                                    border_radius: BorderRadius::all(px(4.0)),
                                    ..default()
                                },
                                BackgroundColor(PALETTE_BUTTON_ACTIVE),
                            )
                        ],
                    ));
                    pad.spawn((
                        Node {
                            width: px(58.0),
                            height: px(48.0),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            ..default()
                        },
                        ImageNode {
                            color: PALETTE_MUTED,
                            ..ImageNode::new(icons.get(Icon::Controller))
                        },
                    ));
                    pad.spawn((
                        Node {
                            width: px(78.0),
                            height: px(78.0),
                            flex_direction: FlexDirection::Row,
                            flex_wrap: FlexWrap::Wrap,
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            row_gap: px(7.0),
                            column_gap: px(7.0),
                            ..default()
                        },
                        BackgroundColor(Color::NONE),
                    ))
                    .with_children(|buttons| {
                        for color in [
                            Color::srgb(0.78, 0.20, 0.18),
                            Color::srgb(0.22, 0.55, 0.88),
                            Color::srgb(0.28, 0.68, 0.35),
                            Color::srgb(0.86, 0.67, 0.22),
                        ] {
                            buttons.spawn((
                                Node {
                                    width: px(27.0),
                                    height: px(27.0),
                                    border_radius: BorderRadius::all(px(16.0)),
                                    border: UiRect::all(px(1.0)),
                                    ..default()
                                },
                                BackgroundColor(color),
                                BorderColor::all(PALETTE_POSTER_BORDER),
                            ));
                        }
                    });
                });
        });
}

fn spawn_path_button(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    path: &Path,
    target: PathTarget,
) {
    spawn_button(
        parent,
        &format!("{label}: {}", path.display()),
        UiAction::PickPath(target),
    );
}

fn button_node() -> Node {
    Node {
        width: percent(100),
        min_height: px(38.0),
        padding: UiRect::axes(px(12.0), px(8.0)),
        border: UiRect::all(px(1.0)),
        border_radius: BorderRadius::all(px(6.0)),
        align_items: AlignItems::Center,
        justify_content: JustifyContent::FlexStart,
        ..default()
    }
}

fn spawn_button(parent: &mut ChildSpawnerCommands, label: &str, action: UiAction) {
    spawn_button_variant(parent, label, action, ButtonVariant::Secondary);
}

fn spawn_playback_button(parent: &mut ChildSpawnerCommands, playing: bool) {
    let visual = theme::current_button_visual(ButtonVariant::Secondary, Interaction::None, false);
    parent
        .spawn((
            Button,
            button_node(),
            BorderColor::all(visual.border),
            BackgroundColor(visual.background),
            UiButton {
                action: UiAction::ToggleRecordingPlayback,
                variant: ButtonVariant::Secondary,
            },
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(if playing { "Pause" } else { "Play" }),
                TextFont {
                    font_size: 15.0,
                    ..default()
                },
                TextColor(visual.foreground),
                RecordingPlaybackLabel,
            ));
        });
}

fn spawn_button_variant(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    action: UiAction,
    variant: ButtonVariant,
) -> Entity {
    let visual = theme::current_button_visual(variant, Interaction::None, false);
    parent
        .spawn((
            Button,
            button_node(),
            BorderColor::all(visual.border),
            BackgroundColor(visual.background),
            UiButton { action, variant },
            children![(
                Text::new(label.to_string()),
                TextFont {
                    font_size: 15.0,
                    ..default()
                },
                TextColor(visual.foreground),
            )],
        ))
        .id()
}

fn spawn_text(parent: &mut ChildSpawnerCommands, text: &str, size: f32, color: Color) {
    parent.spawn((
        Text::new(text.to_string()),
        TextFont {
            font_size: size,
            ..default()
        },
        TextColor(color),
    ));
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

struct UiTheme {
    toolbar_height: f32,
    cover_tile_width: f32,
    cover_tile_height: f32,
    cover_art_height: f32,
    gap_md: f32,
}

const UI: UiTheme = UiTheme {
    toolbar_height: 56.0,
    cover_tile_width: 148.0,
    cover_tile_height: 254.0,
    cover_art_height: 150.0,
    gap_md: 12.0,
};

pub fn active_bindings(config: &RetroFeelConfig) -> InputBindingSet {
    let mut bindings = default_input_bindings();
    for (control, key) in &config.global_input_bindings.keyboard {
        bindings.keyboard.insert(control.clone(), key.clone());
    }
    for (control, button) in &config.global_input_bindings.gamepad {
        bindings.gamepad.insert(control.clone(), button.clone());
    }
    bindings
}

pub fn keyboard_defaults() -> InputBindingSet {
    crate::input::default_keyboard_bindings()
}

pub fn gamepad_defaults() -> InputBindingSet {
    default_gamepad_bindings()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_preview_uses_its_source_aspect_ratio() {
        let node = recording_preview_node();

        assert_eq!(node.width, percent(100));
        assert_eq!(node.height, auto());
        assert_eq!(node.aspect_ratio, None);
    }

    #[test]
    fn button_interactions_do_not_tint_full_color_media() {
        let mut app = App::new();
        app.add_systems(Update, button_interactions);
        let button = app
            .world_mut()
            .spawn((
                Button,
                Interaction::None,
                BackgroundColor(Color::NONE),
                BorderColor::all(Color::NONE),
                UiButton {
                    action: UiAction::GoLibrary,
                    variant: ButtonVariant::Secondary,
                },
            ))
            .id();
        let original = Color::srgb(0.91, 0.42, 0.18);
        let artwork = app
            .world_mut()
            .spawn((
                ImageNode {
                    color: original,
                    ..default()
                },
                PreserveImageColor,
                ChildOf(button),
            ))
            .id();

        app.update();

        assert_eq!(
            app.world().get::<ImageNode>(artwork).unwrap().color,
            original
        );
    }

    fn rom(name: &str, path: &str, system_id: Option<&str>, core_name: Option<&str>) -> RomEntry {
        RomEntry {
            path: PathBuf::from(path),
            name: name.to_string(),
            core_name: core_name.map(str::to_string),
            system: system_id.and_then(system_by_id),
        }
    }

    fn model_with_roms(roms: Vec<RomEntry>) -> FrontendModel {
        FrontendModel {
            config_path: None,
            config: RetroFeelConfig::default(),
            registry: CoreScanReport::default(),
            bios: None,
            roms,
            recordings: Vec::new(),
            selected_recording: None,
            status: String::new(),
            binding_capture: None,
            library_view: LibraryView::AllGames,
            library_query: String::new(),
            library_display_mode: LibraryDisplayMode::Grid,
            library_top_view: LibraryTopView::Games,
            preferences_section: PreferencesSection::Library,
            core_catalog: Vec::new(),
            db: None,
        }
    }

    #[test]
    fn library_content_is_bounded_so_overflow_can_scroll() {
        let body = library_body_node();
        let content = library_content_node();

        assert_eq!(body.flex_direction, FlexDirection::Column);
        assert_eq!(body.min_height, px(0.0));
        assert_eq!(body.flex_basis, px(0.0));
        assert_eq!(content.min_width, px(0.0));
        assert_eq!(content.min_height, px(0.0));
        assert_eq!(content.flex_basis, px(0.0));
        assert_eq!(content.overflow, Overflow::scroll_y());
    }

    #[test]
    fn persistent_utility_bar_is_horizontal_and_bottom_bordered() {
        let toolbar = bottom_toolbar_node();

        assert_eq!(toolbar.width, percent(100));
        assert_eq!(toolbar.min_height, px(56.0));
        assert_eq!(toolbar.flex_direction, FlexDirection::Row);
        assert_eq!(toolbar.border.top, px(1.0));
        assert_eq!(toolbar.border.bottom, px(0.0));
    }

    #[test]
    fn ui_action_log_labels_cover_all_variants() {
        let catalog_entry = CoreCatalogEntry {
            slug: "snes9x".to_string(),
            archive_name: "snes9x_libretro.dylib.zip".to_string(),
            display_name: "Snes9x".to_string(),
            inferred_system: Some("SNES".to_string()),
            installed_path: None,
            status: CoreInstallStatus::Available,
            download_url: "https://example.test/snes9x.zip".to_string(),
        };
        let actions = vec![
            UiAction::GoLibrary,
            UiAction::GoSettings,
            UiAction::SetThemePreference(ThemePreference::System),
            UiAction::SelectPreferencesSection(PreferencesSection::Library),
            UiAction::RefreshCoreCatalog,
            UiAction::InstallCore(catalog_entry),
            UiAction::ImportBios {
                filename: "bios.bin".to_string(),
            },
            UiAction::SelectLibraryView(LibraryView::System("snes".to_string())),
            UiAction::SetLibraryDisplayMode(LibraryDisplayMode::List),
            UiAction::SetLibraryTopView(LibraryTopView::Screenshots),
            UiAction::ClearLibrarySearch,
            UiAction::Refresh,
            UiAction::PickPath(PathTarget::Roms),
            UiAction::LaunchRom(PathBuf::from("roms/game.sfc")),
            UiAction::AssignCore {
                extension: "sfc".to_string(),
                core_path: PathBuf::from("cores/snes9x.dylib"),
            },
            UiAction::BindKeyboard("A".to_string()),
            UiAction::BindGamepad("B".to_string()),
            UiAction::ResetKeyboardBindings,
            UiAction::ResetGamepadBindings,
            UiAction::SetCoreOption {
                core_key: "Snes9x".to_string(),
                option_key: "region".to_string(),
                value: "auto".to_string(),
            },
            UiAction::ClearCoreOption {
                core_key: "Snes9x".to_string(),
                option_key: "region".to_string(),
            },
            UiAction::ToggleIntegerScaling,
            UiAction::ToggleAspectCorrection,
            UiAction::ToggleFullscreen,
            UiAction::ToggleAudio,
            UiAction::ToggleMicCapture,
            UiAction::PickWhisperModel,
            UiAction::ClearWhisperModel,
            UiAction::StartRecording,
            UiAction::StopRecording,
            UiAction::ToggleRecording,
            UiAction::TogglePause,
            UiAction::ResetCore,
            UiAction::VolumeUp,
            UiAction::VolumeDown,
            UiAction::OpenOverlay,
            UiAction::Screenshot,
            UiAction::ExportRecording {
                target: ExportTarget::Bevy,
                session: PathBuf::from("recordings/session"),
            },
            UiAction::Resume,
            UiAction::QuitToLibrary,
            UiAction::SaveState(1),
            UiAction::LoadState(1),
        ];

        for action in actions {
            assert!(!action.log_label().is_empty(), "{action:?}");
        }
    }

    #[test]
    fn icon_button_actions_have_accessible_names() {
        for action in [
            UiAction::SetLibraryDisplayMode(LibraryDisplayMode::Grid),
            UiAction::SetLibraryDisplayMode(LibraryDisplayMode::List),
            UiAction::ClearLibrarySearch,
            UiAction::ToggleRecording,
            UiAction::TogglePause,
            UiAction::ResetCore,
            UiAction::VolumeDown,
            UiAction::VolumeUp,
            UiAction::ToggleFullscreen,
            UiAction::OpenOverlay,
            UiAction::QuitToLibrary,
        ] {
            assert!(!action.accessible_label().trim().is_empty());
        }
    }

    #[test]
    fn tab_order_and_modal_containment_are_deterministic() {
        use bevy::ecs::system::SystemState;
        use bevy::input_focus::tab_navigation::{NavAction, TabNavigation};

        let mut app = App::new();
        let world = app.world_mut();
        let normal = world.spawn(TabGroup::default()).id();
        let first = world.spawn((TabIndex(0), ChildOf(normal))).id();
        let second = world.spawn((TabIndex(1), ChildOf(normal))).id();
        let modal = world.spawn(TabGroup::modal()).id();
        let modal_first = world.spawn((TabIndex(0), ChildOf(modal))).id();
        let modal_second = world.spawn((TabIndex(1), ChildOf(modal))).id();

        let mut state: SystemState<TabNavigation> = SystemState::new(world);
        let navigation = state.get(world);
        assert_eq!(
            navigation.navigate(&InputFocus::from_entity(first), NavAction::Next),
            Ok(second)
        );
        assert_eq!(
            navigation.navigate(&InputFocus::from_entity(second), NavAction::Previous),
            Ok(first)
        );
        assert_eq!(
            navigation.navigate(&InputFocus::from_entity(modal_second), NavAction::Next),
            Ok(modal_first)
        );
    }

    #[test]
    fn pause_overlay_focus_starts_inside_its_modal_tab_group() {
        use bevy::ecs::system::SystemState;
        use bevy::input_focus::tab_navigation::{NavAction, TabNavigation};

        fn spawn(mut commands: Commands, mut focus: ResMut<InputFocus>) {
            focus.set(spawn_overlay(&mut commands));
        }

        let mut app = App::new();
        app.init_resource::<InputFocus>().add_systems(Update, spawn);
        app.update();

        let world = app.world_mut();
        let focused = world
            .resource::<InputFocus>()
            .get()
            .expect("overlay should focus Resume");
        let mut buttons = world.query::<(Entity, &UiButton)>();
        let focused_action = buttons
            .get(world, focused)
            .expect("focused entity should be a UI button")
            .1
            .action
            .clone();
        let quit = buttons
            .iter(world)
            .find_map(|(entity, button)| {
                matches!(button.action, UiAction::QuitToLibrary).then_some(entity)
            })
            .expect("overlay should contain Quit to library");
        assert!(matches!(focused_action, UiAction::Resume));

        let mut state: SystemState<TabNavigation> = SystemState::new(world);
        let navigation = state.get(world);
        assert_eq!(
            navigation.navigate(&InputFocus::from_entity(focused), NavAction::Previous),
            Ok(quit)
        );
    }

    #[test]
    fn library_filter_combines_context_tabs_and_search() {
        let roms = vec![
            rom(
                "Super Metroid",
                "roms/super-metroid.sfc",
                Some("snes"),
                Some("Snes9x"),
            ),
            rom("Metroid", "roms/metroid.nes", Some("nes"), Some("Mesen")),
            rom("Mario Paint", "roms/mario-paint.sfc", Some("snes"), None),
        ];

        let filtered =
            filtered_library_entries(&roms, &LibraryView::System("snes".into()), "metroid");

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Super Metroid");
    }

    #[test]
    fn missing_core_roms_remain_visible() {
        let roms = vec![rom(
            "Mario Paint",
            "roms/mario-paint.sfc",
            Some("snes"),
            None,
        )];

        let filtered = filtered_library_entries(&roms, &LibraryView::AllGames, "missing core");

        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].core_name.is_none());
    }

    #[test]
    fn top_library_views_do_not_change_game_filtering() {
        let mut model = model_with_roms(vec![
            rom(
                "Super Metroid",
                "roms/super-metroid.sfc",
                Some("snes"),
                Some("Snes9x"),
            ),
            rom("Metroid", "roms/metroid.nes", Some("nes"), Some("Mesen")),
        ]);
        model.library_view = LibraryView::AllGames;
        model.library_query = "metroid".to_string();
        model.library_top_view = LibraryTopView::Games;
        let games: Vec<String> = filtered_game_entries(&model)
            .iter()
            .map(|rom| rom.name.clone())
            .collect();

        model.library_top_view = LibraryTopView::Recordings;
        let recordings: Vec<String> = filtered_game_entries(&model)
            .iter()
            .map(|rom| rom.name.clone())
            .collect();
        model.library_top_view = LibraryTopView::Screenshots;
        let screenshots: Vec<String> = filtered_game_entries(&model)
            .iter()
            .map(|rom| rom.name.clone())
            .collect();

        assert_eq!(games, recordings);
        assert_eq!(games, screenshots);
    }

    #[test]
    fn placeholder_artwork_is_stable_for_same_rom() {
        let rom = rom(
            "Super Metroid",
            "roms/super-metroid.sfc",
            Some("snes"),
            Some("Snes9x"),
        );

        assert_eq!(
            deterministic_placeholder_artwork(&rom),
            deterministic_placeholder_artwork(&rom)
        );
    }

    #[test]
    fn placeholder_artwork_changes_between_systems() {
        let nes = rom("Tetris", "roms/tetris.nes", Some("nes"), Some("Mesen"));
        let gb = rom("Tetris", "roms/tetris.gb", Some("gb"), Some("Gambatte"));

        let nes_art = deterministic_placeholder_artwork(&nes);
        let gb_art = deterministic_placeholder_artwork(&gb);

        assert_ne!(
            (nes_art.primary, nes_art.secondary, nes_art.accent),
            (gb_art.primary, gb_art.secondary, gb_art.accent)
        );
    }

    #[test]
    fn whisper_model_choice_is_persisted_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("retrofeel.ron");
        let model_path = dir.path().join("ggml-base.bin");
        let mut model = model_with_roms(Vec::new());
        model.config_path = Some(config_path.clone());

        model.set_whisper_model(Some(model_path.clone()));
        let saved = RetroFeelConfig::load_from_path(&config_path).unwrap();
        assert_eq!(saved.recording.whisper_model, Some(model_path));

        model.set_whisper_model(None);
        let saved = RetroFeelConfig::load_from_path(config_path).unwrap();
        assert_eq!(saved.recording.whisper_model, None);
    }

    #[test]
    fn collapses_raw_and_mapped_input_changes() {
        let mut pressed = retrofeel_types::InputState::default();
        pressed.buttons.set(retrofeel_types::device_ids::joypad::A);
        let raw = retrofeel_types::RawHostInput {
            keyboard_keys: vec!["KeyZ".into()],
            ..Default::default()
        };
        let frames = vec![
            InputFrame {
                frame: 0,
                elapsed_us: None,
                port: 0,
                state: Default::default(),
                raw_host: Some(Default::default()),
            },
            InputFrame {
                frame: 1,
                elapsed_us: None,
                port: 0,
                state: pressed,
                raw_host: Some(raw.clone()),
            },
            InputFrame {
                frame: 2,
                elapsed_us: None,
                port: 0,
                state: pressed,
                raw_host: Some(raw),
            },
        ];
        let (summary, changes) = summarize_inputs(&frames, 60.0);
        let transitions = retrofeel_types::input_transitions_from_frames(&frames);
        let (indexed_summary, indexed_changes) = summarize_transitions(&transitions, 3, 60.0);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[1].mapped, "A");
        assert_eq!(changes[1].raw, "KeyZ");
        assert_eq!(summary.keyboard_frames, 2);
        assert!(summary.control_names.contains(&"KeyZ".to_string()));
        assert!(summary.control_names.contains(&"A".to_string()));
        assert_eq!(indexed_summary.keyboard_frames, summary.keyboard_frames);
        assert_eq!(indexed_summary.gamepad_frames, summary.gamepad_frames);
        assert_eq!(indexed_summary.mouse_frames, summary.mouse_frames);
        assert_eq!(indexed_summary.change_count, summary.change_count);
        assert_eq!(indexed_summary.control_names, summary.control_names);
        assert_eq!(indexed_changes.len(), changes.len());
        assert_eq!(indexed_changes[1].frame, changes[1].frame);
        assert_eq!(indexed_changes[1].mapped, changes[1].mapped);
        assert_eq!(indexed_changes[1].raw, changes[1].raw);
    }

    #[test]
    fn recording_search_includes_title_core_transcript_and_controls() {
        let recording = RecordingEntry {
            path: PathBuf::from("recordings/session"),
            name: "session".into(),
            frame_count: Some(60),
            title: "Chrono Trigger".into(),
            core_name: "bsnes".into(),
            start_timestamp: 0.0,
            duration_seconds: 1.0,
            dropped_frames: 0,
            has_video: true,
            has_game_audio: true,
            has_mic: true,
            transcription_status: TranscriptionJobState::Complete,
            transcript_text: Some("Time travel begins".into()),
            transcript: None,
            input_summary: InputActivitySummary {
                control_names: vec!["GamepadSouth".into()],
                ..Default::default()
            },
            input_changes: Vec::new(),
            manifest: None,
            feel_manifest: None,
            alignment: None,
            latest_analysis: None,
        };
        let searchable = recording_search_text(&recording);
        for needle in ["chrono", "bsnes", "time travel", "gamepadsouth"] {
            assert!(searchable.contains(needle));
        }
    }
}
