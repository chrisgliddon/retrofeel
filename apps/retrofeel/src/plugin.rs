//! `RetrofeelPlugin`: Phase 4 app state, library/settings UI, and in-game core
//! orchestration.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bevy::ecs::system::SystemParam;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::input_focus::{
    tab_navigation::TabNavigationPlugin, InputDispatchPlugin, InputFocus, InputFocusVisible,
};
use bevy::picking::hover::HoverMap;
use bevy::prelude::*;
use bevy::window::{FileDragAndDrop, MonitorSelection, WindowMode};
use crossbeam_channel::{bounded, unbounded, Receiver};
use retrofeel_backend::{
    install_core, refresh_core_catalog, scan_system_dir, BiosReport, CoreCatalogEntry,
    CoreInstallResult, CoreRegistry, CoreScanReport,
};
use retrofeel_types::{
    InputBindingSet, RetroFeelConfig, SteamGameEntry, SteamGameSource, SteamLaunchMode,
};

use crate::core_thread::{self, latest_frame, send_input, InputSnapshot};
use crate::diagnostics::{self, Diagnostics, UiActionIds};
use crate::frame_schedule::{FramePhase, FrameSchedulePlugin};
use crate::icons::{Icon, IconAssets};
use crate::input::{
    candidate_gamepad_buttons, capture_raw_host_input, gamepad_button_to_binding,
    keycode_to_binding, map_input,
};
use crate::screen::{self, CoreScreen, PendingFrame, ScreenSettings};
use crate::ui::{
    self, active_bindings, BindingCapture, FrontendModel, PauseButtonLabel, PreferencesSection,
    RecIndicator, RecordButtonLabel, Scrollable, UiAction, UiActionRequest, UiButton, UiRoot,
};

type UiButtonPresses<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static Interaction, &'static UiButton),
    (Changed<Interaction>, With<Button>),
>;
type RecIndicatorIcon<'w, 's> = Query<
    'w,
    's,
    &'static mut ImageNode,
    (
        With<RecIndicator>,
        Without<RecordButtonLabel>,
        Without<PauseButtonLabel>,
    ),
>;
type RecordToggleIcon<'w, 's> = Query<
    'w,
    's,
    &'static mut ImageNode,
    (
        With<RecordButtonLabel>,
        Without<RecIndicator>,
        Without<PauseButtonLabel>,
    ),
>;
type PauseToggleIcon<'w, 's> = Query<
    'w,
    's,
    &'static mut ImageNode,
    (
        With<PauseButtonLabel>,
        Without<RecIndicator>,
        Without<RecordButtonLabel>,
    ),
>;

#[derive(States, Clone, Eq, PartialEq, Debug, Hash, Default)]
pub enum AppState {
    #[default]
    Boot,
    FirstRun,
    Library,
    Settings,
    InGame,
    InGameOverlay,
}

#[derive(Resource, Default)]
pub struct FastForward(pub bool);

#[derive(Resource)]
pub struct CoreResource {
    pub handle: Box<dyn core_thread::GameSource>,
}

/// Remove the active game source at a deferred-command seam so every caller
/// uses the same teardown path.
fn detach_game_source(world: &mut World) {
    let audio = world.remove_resource::<crate::audio::AudioOutput>();
    let source = world.remove_resource::<CoreResource>();
    if audio.is_none() && source.is_none() {
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("retrofeel-game-source-teardown".into())
        .spawn(move || {
            drop(audio);
            drop(source);
        })
    {
        log::error!("could not start game-source teardown thread: {error}");
    }
}

fn queue_game_source_teardown(commands: &mut Commands) {
    commands.queue(detach_game_source);
}

#[derive(Resource, Clone)]
pub struct ActiveBindings(pub InputBindingSet);

#[derive(Resource, Default)]
pub struct RecordingUiState {
    pub active: bool,
    pub started_at: Option<Instant>,
}

#[derive(Resource, Default)]
struct TranscriptionBackfill {
    active: Option<PathBuf>,
}

#[derive(Resource, Default)]
struct ModelInstallJobs {
    active_id: Option<String>,
    receiver: Option<Receiver<crate::transcription::ModelInstallEvent>>,
    cancelled: Option<Arc<AtomicBool>>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[derive(SystemParam)]
struct InstallJobs<'w> {
    cores: ResMut<'w, CoreCatalogJobs>,
    models: ResMut<'w, ModelInstallJobs>,
}

#[derive(SystemParam)]
struct PendingUiJobs<'w> {
    pick: ResMut<'w, PendingPick>,
    steam: ResMut<'w, PendingSteamLaunch>,
}

/// Whether the running core is currently paused. Single source of truth shared
/// by the ESC overlay (which pauses while open) and the in-game control bar's
/// Pause/Resume toggle, so the two never disagree.
#[derive(Resource, Default)]
pub struct Paused(pub bool);

#[derive(Resource, Default)]
pub struct CoreCatalogJobs {
    receiver: Option<Receiver<CoreCatalogJobResult>>,
    /// Join handle of the worker thread, so [`drain_core_catalog_jobs`] can
    /// detect a thread that died without sending a result (a non-fatal Rust
    /// panic in the download/extract path). A C-level `exit()` from a core
    /// still takes the whole process down — that's handled by the
    /// `Core::probe`/`Core::load` split, not here — but a Rust panic would
    /// otherwise leak the `busy` flag forever.
    join: Option<std::thread::JoinHandle<()>>,
    busy: bool,
    current_install: Option<CoreInstallRequest>,
    pending_installs: VecDeque<CoreInstallRequest>,
}

#[derive(Clone)]
struct CoreInstallRequest {
    entry: CoreCatalogEntry,
    auto_launch_rom: Option<PathBuf>,
}

enum CoreCatalogJobResult {
    Catalog(Result<Vec<CoreCatalogEntry>, String>),
    Install(Result<CoreInstallResult, String>),
}

/// A request to start a background refresh. Sent by `handle_ui_actions` and
/// `poll_pending_pick` when the model's heavy fields need recomputing. The
/// `start_refresh_from_event` system drains these and spawns the worker.
#[derive(Clone, Debug)]
pub struct RefreshRequest {
    pub next_state: Option<AppState>,
}

impl bevy::ecs::message::Message for RefreshRequest {}

/// A background refresh job. When active, a worker thread computes the
/// model's heavy fields (registry, bios, roms, recordings) off the main
/// thread. [`drain_refresh_job`] applies the result each frame. This prevents
/// the app from freezing when the user picks a large ROM folder or clicks
/// Refresh — the old `model.refresh()` ran all four scans synchronously on the
/// main thread, blocking Bevy's render loop.
#[derive(Resource, Default)]
pub struct RefreshJob {
    receiver: Option<crossbeam_channel::Receiver<RefreshResult>>,
    join: Option<std::thread::JoinHandle<()>>,
    /// What state to return to after the refresh completes (e.g. Library after
    /// quit-to-library, Settings after a path change).
    pub next_state: Option<AppState>,
    busy: bool,
}

/// The computed results of a background refresh.
struct RefreshResult {
    registry: CoreScanReport,
    bios: Option<BiosReport>,
    roms: Vec<ui::RomEntry>,
    recordings: Vec<ui::RecordingEntry>,
    steam_games: Vec<SteamGameEntry>,
    status: String,
}

impl RefreshJob {
    #[allow(dead_code)]
    fn is_busy(&self) -> bool {
        self.busy
    }
}

/// A pending rfd file/folder picker. Only one picker is allowed at a time.
///
/// On macOS, rfd's `AsyncFileDialog` falls back to a **synchronous** modal
/// when `NSApplication::isRunning()` returns false — which is always the case
/// with Bevy 0.18's `pump_events` event loop (winit starts and immediately
/// stops `NSApplication`, then uses `CFRunLoopRunInMode`). The sync modal
/// blocks the main thread, freezing the app.
///
/// Fix: run the **synchronous** `rfd::FileDialog` on a dedicated worker
/// thread. The thread blocks while the user browses, but the main thread keeps
/// rendering Bevy frames. The result is delivered via a channel that
/// `poll_pending_pick` checks each frame with `try_recv`.
#[derive(Resource, Default)]
pub struct PendingPick {
    /// Channel receiver for the picker result (None when no picker is active).
    receiver: Option<crossbeam_channel::Receiver<Option<PathBuf>>>,
    kind: Option<PickKind>,
    /// Timing span opened by `start_*` and finished when the result arrives,
    /// so the `timing.end` log records the full picker wait (including the
    /// user's time in the native dialog).
    timing: Option<diagnostics::Timing>,
}

/// A ScreenCaptureKit attach being resolved away from Bevy's main thread.
///
/// Window discovery can legitimately take the configured attach timeout. It
/// must not run inside `Startup` or an input-handling system: blocking winit's
/// macOS event loop there leaves whichever app is frontmost unable to make a
/// clean focus handoff while RetroFeel waits.
#[derive(Resource, Default)]
pub struct PendingSteamLaunch {
    receiver: Option<
        Receiver<Result<crate::steam_thread::SteamHandle, crate::steam_thread::SteamThreadError>>,
    >,
    join: Option<std::thread::JoinHandle<()>>,
    auto_record: bool,
    auto_record_frames: Option<u64>,
    activate_pid: Option<u32>,
}

impl PendingSteamLaunch {
    fn start(
        &mut self,
        auto_record: bool,
        auto_record_frames: Option<u64>,
        activate_pid: Option<u32>,
        work: impl FnOnce() -> Result<
                crate::steam_thread::SteamHandle,
                crate::steam_thread::SteamThreadError,
            > + Send
            + 'static,
    ) -> Result<(), crate::steam_thread::SteamThreadError> {
        if self.receiver.is_some() {
            return Err(crate::steam_thread::SteamThreadError::AlreadyLaunching);
        }
        let (sender, receiver) = bounded(1);
        let join = std::thread::Builder::new()
            .name("retrofeel-steam-attach".into())
            .spawn(move || {
                let _ = sender.send(work());
            })
            .map_err(crate::steam_thread::SteamThreadError::LaunchWorker)?;
        self.receiver = Some(receiver);
        self.join = Some(join);
        self.auto_record = auto_record;
        self.auto_record_frames = auto_record_frames;
        self.activate_pid = activate_pid.filter(|pid| *pid != 0);
        Ok(())
    }

    #[cfg(test)]
    fn is_busy(&self) -> bool {
        self.receiver.is_some()
    }
}

#[derive(Clone)]
enum PickKind {
    /// Folder picker for a config path (`PickPath`).
    Dir(ui::PathTarget),
    /// File picker for a BIOS import (`ImportBios`). `filename` is the
    /// expected name to copy the picked file under in the System folder.
    BiosFile { filename: String },
    /// File picker for a whisper.cpp model persisted in recording settings.
    WhisperModel,
    /// File picker for an existing whisper.cpp/OpenAI Whisper executable.
    WhisperExecutable,
}

#[derive(Resource)]
pub struct RetrofeelArgs {
    pub core: Option<PathBuf>,
    pub rom: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    /// `true` only for an explicit `--config FILE` launch. Default GUI and
    /// CLI launches load/save SQLite; this mode deliberately keeps the RON
    /// file as a one-off override.
    pub config_file_override: bool,
    pub config: RetroFeelConfig,
    /// Start a recording immediately and continue until the user stops it.
    pub auto_record: bool,
    pub auto_record_frames: Option<u64>,
    pub auto_record_pause_at: Option<u64>,
    pub auto_record_pause_ms: u64,
    pub log_path: PathBuf,
    /// Steam launch target. When set, the startup system launches a Steam
    /// capture session instead of a libretro core.
    pub steam_target: Option<crate::steam_thread::SteamTarget>,
    /// External `.feel` package selected through Finder or `retrofeel open`.
    pub initial_recording: Option<PathBuf>,
}

pub struct RetrofeelPlugin;

impl Plugin for RetrofeelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            FrameSchedulePlugin,
            InputDispatchPlugin,
            TabNavigationPlugin,
        ))
        .init_state::<AppState>()
        .init_resource::<FastForward>()
        .init_resource::<RecordingUiState>()
        .init_resource::<crate::overlay::OverlayRequested>()
        .init_resource::<crate::overlay::OverlayState>()
        .init_resource::<TranscriptionBackfill>()
        .init_resource::<ModelInstallJobs>()
        .init_resource::<Paused>()
        .init_resource::<CoreCatalogJobs>()
        .init_resource::<RefreshJob>()
        .init_resource::<crate::feel_viewer::PendingFeelAnalysis>()
        .add_message::<RefreshRequest>()
        .add_message::<UiActionRequest>()
        .init_resource::<PendingFrame>()
        .init_resource::<PendingPick>()
        .init_resource::<PendingSteamLaunch>()
        .init_resource::<LastFullscreen>()
        .init_resource::<InputFocus>()
        .init_resource::<crate::theme::ThemeRuntime>()
        .init_resource::<Diagnostics>()
        .init_resource::<UiActionIds>()
        .add_systems(
            Startup,
            (
                diagnostics::start_watchdog_system,
                startup,
                crate::feel_viewer::install_viewer,
                crate::theme::install_embedded_fonts,
                crate::theme::initialize_theme,
            )
                .chain(),
        )
        .add_systems(OnEnter(AppState::FirstRun), enter_first_run)
        .add_systems(OnExit(AppState::FirstRun), despawn_ui)
        .add_systems(OnEnter(AppState::Library), enter_library)
        .add_systems(OnExit(AppState::Library), despawn_ui)
        .add_systems(OnEnter(AppState::Settings), enter_settings)
        .add_systems(OnExit(AppState::Settings), despawn_ui)
        .add_systems(OnEnter(AppState::InGame), enter_ingame)
        .add_systems(OnExit(AppState::InGame), despawn_ui)
        .add_systems(OnEnter(AppState::InGameOverlay), enter_overlay)
        .add_systems(OnExit(AppState::InGameOverlay), exit_overlay)
        .add_systems(
            Update,
            (
                diagnostics::heartbeat_system,
                poll_pending_steam_launch,
                poll_pending_pick,
                open_recording_documents,
                poll_input.run_if(in_state(AppState::InGame)),
            )
                .in_set(FramePhase::Ingress),
        )
        .add_systems(
            Update,
            (drain_frames, drain_core_status)
                .chain()
                .in_set(FramePhase::RunningSource),
        )
        .add_systems(
            Update,
            (
                (
                    drain_core_catalog_jobs,
                    start_refresh_from_event,
                    drain_refresh_job,
                )
                    .chain(),
                (poll_recording_changes, schedule_transcription_backfill)
                    .chain()
                    .run_if(in_state(AppState::Library)),
                drain_model_install,
                crate::feel_viewer::poll_analysis.run_if(in_state(AppState::Library)),
            )
                .in_set(FramePhase::JobCompletion),
        )
        .add_systems(
            Update,
            (
                toggle_overlay,
                toggle_fast_forward,
                recording_hotkey,
                screenshot_hotkey.run_if(in_state(AppState::InGame)),
                handle_library_search_input.run_if(in_state(AppState::Library)),
                scroll_lists,
                capture_binding.run_if(in_state(AppState::Settings)),
                (
                    dispatch_ui_actions,
                    handle_ui_actions,
                    crate::feel_viewer::sync_selected_recording.run_if(in_state(AppState::Library)),
                    crate::feel_viewer::handle_viewer_actions.run_if(in_state(AppState::Library)),
                )
                    .chain(),
            )
                .in_set(FramePhase::UiIntent),
        )
        .add_systems(
            Update,
            (
                crate::feel_viewer::update_viewer.run_if(in_state(AppState::Library)),
                crate::theme::sync_theme,
                apply_window_mode_on_change,
                crate::boxart::update_box_art,
                update_hud.run_if(in_state(AppState::InGame)),
            )
                .in_set(FramePhase::ViewMaintenance),
        )
        .add_systems(
            Update,
            (
                screen::upload_frame,
                (
                    crate::overlay::sync_status_window,
                    crate::overlay::update_status,
                )
                    .chain(),
                (ui::sync_theme_segments, ui::button_interactions).chain(),
                ui::focus_outlines,
                ui::hide_decorative_accessibility,
                ui::sync_model_status,
                crate::theme::apply_typography,
                crate::theme::apply_shell_theme_colors,
            )
                .in_set(FramePhase::Presentation),
        );
    }
}

/// Cached fullscreen flag for [`apply_window_mode_on_change`].
#[derive(Resource, Default)]
struct LastFullscreen {
    fullscreen: bool,
}

fn apply_window_mode_on_change(
    model: Res<FrontendModel>,
    mut last: ResMut<LastFullscreen>,
    mut windows: Query<&mut Window>,
) {
    let fullscreen = model.config.video.fullscreen;
    if fullscreen == last.fullscreen {
        return;
    }
    last.fullscreen = fullscreen;
    apply_window_mode(&model.config, &mut windows);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn startup(
    mut commands: Commands,
    args: Res<RetrofeelArgs>,
    mut next: ResMut<NextState<AppState>>,
    mut pending: ResMut<PendingFrame>,
    mut images: ResMut<Assets<Image>>,
    mut windows: Query<&mut Window>,
    screen_entities: Query<Entity, With<CoreScreen>>,
    db_res: Option<Res<crate::db::DbResource>>,
    mut core_jobs: ResMut<CoreCatalogJobs>,
    mut recording: ResMut<RecordingUiState>,
    mut pending_steam: ResMut<PendingSteamLaunch>,
) {
    crate::macos_documents::install();
    log::debug!("diagnostics.log_path path=\"{}\"", args.log_path.display());
    screen::setup_camera(&mut commands);
    commands.insert_resource(crate::icons::build_icons(&mut images));
    let boxart_cache = retrofeel_types::RetroFeelConfig::project_dirs()
        .map(|dirs| dirs.data_dir().join("boxart"))
        .unwrap_or_else(|_| PathBuf::from("boxart"));
    commands.insert_resource(crate::boxart::BoxArt::new(boxart_cache));

    let mut model = diagnostics::time_block("frontend_model.new", || {
        match db_res.as_deref().map(|r| &r.db) {
            Some(db) => {
                if args.config_file_override {
                    FrontendModel::new_with_db_file_override(
                        args.config_path
                            .clone()
                            .expect("explicit config override has a path"),
                        args.config.clone(),
                        db,
                    )
                } else {
                    FrontendModel::new_with_db(None, args.config.clone(), db)
                }
            }
            None => FrontendModel::new(args.config_path.clone(), args.config.clone()),
        }
    });
    if let Some(path) = args.initial_recording.as_ref() {
        let selected = choose_external_recording(path, &model.config.paths.recordings)
            .unwrap_or_else(|error| {
                model.status = format!("Could not open {}: {error}", path.display());
                None
            });
        match selected.and_then(ui::load_recording_entry) {
            Some(recording) => {
                let selected_path = recording.path.clone();
                model.recordings.retain(|entry| entry.path != selected_path);
                model.recordings.insert(0, recording);
                model.selected_recording = Some(selected_path.clone());
                model.library_top_view = ui::LibraryTopView::Recordings;
                model.status = format!("Opened {}", selected_path.display());
            }
            None if model.status.is_empty() => {
                model.status = format!("Could not read recording package {}", path.display());
            }
            None => {}
        }
    }
    if let Some(db) = db_res.as_deref() {
        sync_steam_games_to_db(&db.db, &model.config.steam.games);
    }
    if model.core_catalog.is_empty() && !model.roms.is_empty() {
        start_core_catalog_refresh(&mut model, &mut core_jobs);
    } else {
        queue_missing_library_cores(&mut model, &mut core_jobs);
    }
    let bindings = active_bindings(&model.config);
    commands.insert_resource(ActiveBindings(bindings));
    commands.insert_resource(ScreenSettings::from(&model.config.video));
    apply_window_mode(&model.config, &mut windows);
    // Seed the cached fullscreen flag so `apply_window_mode_on_change` does
    // not re-apply the window mode on the first frame after startup.
    commands.insert_resource(LastFullscreen {
        fullscreen: model.config.video.fullscreen,
    });

    let initial_core = args.core.clone();
    let initial_rom = args.rom.clone();
    let steam_target = args.steam_target.clone();
    if let Some(target) = steam_target {
        match launch_steam(
            &mut commands,
            &screen_entities,
            &mut pending_steam,
            &model.config,
            target,
            args.auto_record,
            args.auto_record_frames,
        ) {
            Ok(()) => {
                recording.active = false;
                recording.started_at = None;
                model.status = "Attaching to the running game…".into();
            }
            Err(error) => {
                log::error!("steam launch failed: {error}");
                model.status = format!("Steam launch failed: {error}");
                next.set(AppState::Library);
            }
        }
    } else if let Some(core_path) = initial_core {
        match launch_core(
            &mut commands,
            &mut pending,
            &mut images,
            &screen_entities,
            &model.config,
            core_path,
            initial_rom,
            args.auto_record_frames,
            args.auto_record_pause_at,
            args.auto_record_pause_ms,
        ) {
            Ok(()) => {
                if args.auto_record_frames.is_some() {
                    recording.active = true;
                    recording.started_at = Some(Instant::now());
                }
                next.set(AppState::InGame);
            }
            Err(error) => {
                log::error!("initial launch failed: {error}");
                model.status = format!("Launch failed: {error}");
                next.set(AppState::Library);
            }
        }
    } else {
        next.set(AppState::Library);
    }

    commands.insert_resource(model);
}

fn open_recording_documents(
    mut dropped: MessageReader<FileDragAndDrop>,
    mut model: ResMut<FrontendModel>,
    mut next: ResMut<NextState<AppState>>,
) {
    let mut paths = crate::macos_documents::drain();
    paths.extend(dropped.read().filter_map(|event| match event {
        FileDragAndDrop::DroppedFile { path_buf, .. } => Some(path_buf.clone()),
        _ => None,
    }));
    for path in paths {
        if path.extension().is_none_or(|extension| extension != "feel") {
            continue;
        }
        let selected = match choose_external_recording(&path, &model.config.paths.recordings) {
            Ok(Some(path)) => path,
            Ok(None) => continue,
            Err(error) => {
                model.status = format!("Could not open {}: {error}", path.display());
                continue;
            }
        };
        let Some(recording) = ui::load_recording_entry(selected.clone()) else {
            model.status = format!("Could not read recording package {}", selected.display());
            continue;
        };
        model.recordings.retain(|entry| entry.path != selected);
        model.recordings.insert(0, recording);
        model.selected_recording = Some(selected.clone());
        model.library_top_view = ui::LibraryTopView::Recordings;
        model.status = format!("Opened {}", selected.display());
        next.set(AppState::Library);
    }
}

fn choose_external_recording(path: &Path, library: &Path) -> anyhow::Result<Option<PathBuf>> {
    retrofeel_feel::FeelPackage::open(path)?;
    if path.starts_with(library) {
        return Ok(Some(path.to_path_buf()));
    }
    let result = rfd::MessageDialog::new()
        .set_title("Open RetroFeel recording")
        .set_description(
            "Open this .feel package where it is, or import a verified copy into RetroFeel's recording library?",
        )
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
            "Open In Place".into(),
            "Import Copy & Open".into(),
            "Cancel".into(),
        ))
        .show();
    match result {
        rfd::MessageDialogResult::Custom(value) if value == "Open In Place" => {
            Ok(Some(path.to_path_buf()))
        }
        rfd::MessageDialogResult::Custom(value) if value == "Import Copy & Open" => {
            let package = retrofeel_feel::import_package_copy(path, library)?;
            Ok(Some(package.root().to_path_buf()))
        }
        _ => Ok(None),
    }
}

fn sync_steam_games_to_db(db: &retrofeel_db::Db, games: &[SteamGameEntry]) {
    let repo = retrofeel_db::SteamRepo::new(db);
    for game in games {
        let existing = repo.get(game.app_id).ok().flatten();
        let row = retrofeel_db::SteamGameRow {
            app_id: game.app_id,
            name: game.name.clone(),
            install_dir: game.install_dir.clone(),
            wine_prefix: Some(game.wine_prefix.clone()),
            exe_name: game.exe_name.clone(),
            banner_path: existing.as_ref().and_then(|row| row.banner_path.clone()),
            last_played: existing.as_ref().and_then(|row| row.last_played),
            added_at: existing
                .as_ref()
                .map(|row| row.added_at)
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0)
                }),
        };
        if let Err(error) = repo.upsert(&row) {
            log::warn!(
                "steam library cache: could not upsert {}: {error}",
                game.app_id
            );
        }
    }
}

fn native_steam_launch_url(app_id: u32) -> String {
    format!("steam://run/{app_id}")
}

fn launch_native_steam_game(app_id: u32) -> std::io::Result<()> {
    let url = native_steam_launch_url(app_id);
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "opening Steam links is unsupported on this platform",
        ))
    }
}

fn enter_first_run(mut commands: Commands, model: Res<FrontendModel>, icons: Res<IconAssets>) {
    diagnostics::time_block("state.enter.first_run", || {
        ui::spawn_first_run(&mut commands, &model, &icons);
    });
}

fn enter_library(
    mut commands: Commands,
    model: Res<FrontendModel>,
    icons: Res<IconAssets>,
    viewer: Res<crate::feel_viewer::RecordingViewer>,
) {
    diagnostics::time_block("state.enter.library", || {
        ui::spawn_library(&mut commands, &model, &icons, &viewer);
    });
}

fn enter_settings(
    mut commands: Commands,
    model: Res<FrontendModel>,
    icons: Res<IconAssets>,
    jobs: Res<ModelInstallJobs>,
) {
    diagnostics::time_block("state.enter.settings", || {
        ui::spawn_settings(&mut commands, &model, &icons, jobs.active_id.as_deref());
    });
}

fn enter_ingame(
    mut commands: Commands,
    recording: Res<RecordingUiState>,
    paused: Res<Paused>,
    icons: Res<IconAssets>,
) {
    diagnostics::time_block("state.enter.ingame", || {
        ui::spawn_hud_bar(&mut commands, &icons, recording.active, paused.0);
    });
}

fn enter_overlay(
    mut commands: Commands,
    core: Option<Res<CoreResource>>,
    mut paused: ResMut<Paused>,
    mut focus: ResMut<InputFocus>,
) {
    if let Some(core) = core {
        core.handle.set_paused(true);
    }
    paused.0 = true;
    diagnostics::time_block("state.enter.overlay", || {
        focus.set(ui::spawn_overlay(&mut commands));
    });
}

fn exit_overlay(
    mut commands: Commands,
    roots: Query<Entity, With<UiRoot>>,
    core: Option<Res<CoreResource>>,
    mut paused: ResMut<Paused>,
) {
    despawn_ui_roots(&mut commands, &roots);
    if let Some(core) = core {
        core.handle.set_paused(false);
    }
    paused.0 = false;
}

/// Update the in-game HUD's live text (REC indicator + Record/Pause labels)
/// when the recording or pause state changes. The bar is spawned with correct
/// initial labels, so we only refresh on change.
fn update_hud(
    recording: Res<RecordingUiState>,
    paused: Res<Paused>,
    icons: Res<IconAssets>,
    mut indicators: RecIndicatorIcon,
    mut record_toggles: RecordToggleIcon,
    mut pause_toggles: PauseToggleIcon,
) {
    if !recording.is_changed() && !paused.is_changed() {
        return;
    }
    for mut image in &mut indicators {
        image.color = ui::rec_indicator_tint(recording.active);
    }
    for mut image in &mut record_toggles {
        image.image = icons.get(if recording.active {
            Icon::Stop
        } else {
            Icon::Record
        });
    }
    for mut image in &mut pause_toggles {
        image.image = icons.get(if paused.0 { Icon::Play } else { Icon::Pause });
    }
}

fn despawn_ui(mut commands: Commands, roots: Query<Entity, With<UiRoot>>) {
    despawn_ui_roots(&mut commands, &roots);
}

fn despawn_ui_roots(commands: &mut Commands, roots: &Query<Entity, With<UiRoot>>) {
    for entity in roots {
        commands.entity(entity).despawn();
    }
}

fn poll_input(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mouse_buttons: Res<ButtonInput<bevy::input::mouse::MouseButton>>,
    mouse_motion: Res<bevy::input::mouse::AccumulatedMouseMotion>,
    core: Res<CoreResource>,
    bindings: Res<ActiveBindings>,
) {
    // Steam sessions obtain input from the event tap/gamepad state at the SCK
    // sampling boundary. Do not overwrite that evidence from RetroFeel's own
    // Bevy window event queue; the overlay reads the same source snapshot
    // later persisted in `input.json`.
    if core.handle.kind() == core_thread::SourceKind::Steam {
        return;
    }
    let mapped = map_input(
        &keys,
        &gamepads,
        Some(&bindings.0),
        &mouse_buttons,
        &mouse_motion,
    );
    let raw_host = capture_raw_host_input(&keys, &gamepads, &mouse_buttons, &mouse_motion);
    send_input(core.handle.input_slot(), InputSnapshot { mapped, raw_host });
}

/// Apply a completed Steam/GameHub attach on Bevy's main thread without ever
/// blocking that thread while ScreenCaptureKit searches for the game window.
#[allow(clippy::too_many_arguments)]
fn poll_pending_steam_launch(
    mut commands: Commands,
    mut launch: ResMut<PendingSteamLaunch>,
    mut pending: ResMut<PendingFrame>,
    mut images: ResMut<Assets<Image>>,
    screen_entities: Query<Entity, With<CoreScreen>>,
    mut model: ResMut<FrontendModel>,
    mut recording: ResMut<RecordingUiState>,
    mut paused: ResMut<Paused>,
    mut next: ResMut<NextState<AppState>>,
) {
    let Some(receiver) = launch.receiver.as_ref() else {
        return;
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(crossbeam_channel::TryRecvError::Empty) => return,
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            Err(crate::steam_thread::SteamThreadError::Capture(
                "attach worker exited without returning a result".into(),
            ))
        }
    };

    launch.receiver = None;
    if let Some(join) = launch.join.take() {
        let _ = join.join();
    }
    let auto_record = launch.auto_record;
    let auto_record_frames = launch.auto_record_frames.take();
    let activate_pid = launch.activate_pid.take();

    match result {
        Ok(handle) => {
            pending.width = handle.base_width;
            pending.height = handle.base_height;
            pending.frame = None;
            commands.insert_resource(ScreenSettings::from(&model.config.video));
            screen::setup_screen(&mut commands, &mut images);
            if let Some(frames) = auto_record_frames {
                handle.start_recording_for(model.config.paths.recordings.clone(), frames);
            } else if auto_record {
                handle.start_recording(model.config.paths.recordings.clone());
            }
            commands.insert_resource(CoreResource {
                handle: Box::new(handle),
            });
            recording.active = auto_record || auto_record_frames.is_some();
            recording.started_at = recording.active.then(Instant::now);
            paused.0 = false;
            crate::overlay::request_visible(&mut commands, model.config.steam.overlay_visible);

            if let Some(pid) = activate_pid {
                let spawned = std::thread::Builder::new()
                    .name("retrofeel-game-focus".into())
                    .spawn(
                        move || match retrofeel_steamcapture::activate_process(pid) {
                            Ok(()) => log::info!("steam: requested focus for game PID={pid}"),
                            Err(error) => {
                                log::warn!(
                                    "steam: could not request focus for game PID={pid}: {error}"
                                )
                            }
                        },
                    );
                if let Err(error) = spawned {
                    log::warn!("steam: could not start game-focus request: {error}");
                }
            }
            model.status = "Attached to running game".into();
            next.set(AppState::InGame);
        }
        Err(error) => {
            log::error!("steam launch failed: {error}");
            model.status = format!("Steam launch failed: {error}");
            recording.active = false;
            recording.started_at = None;
            crate::overlay::request_visible(&mut commands, false);
            for entity in &screen_entities {
                commands.entity(entity).despawn();
            }
            next.set(AppState::Library);
        }
    }
}

fn drain_frames(core: Option<Res<CoreResource>>, mut pending: ResMut<PendingFrame>) {
    let Some(core) = core else {
        return;
    };
    if let Some(cf) = latest_frame(core.handle.latest_frame_slot()) {
        pending.frame = Some(cf);
    }
}

/// Surface worker status (load/run failures) into the frontend model so a core
/// crash is visible in-app instead of only in the logs (item 15 of the post-v1
/// review). The worker writes a human-readable message into a shared slot; we
/// copy it into `FrontendModel.status` which the UI renders as a banner.
fn drain_core_status(
    mut commands: Commands,
    core: Option<Res<CoreResource>>,
    mut model: ResMut<FrontendModel>,
    mut next: ResMut<NextState<AppState>>,
) {
    let Some(core) = core else {
        return;
    };
    let msg = {
        let Ok(mut slot) = core.handle.status().lock() else {
            return;
        };
        slot.take()
    };
    if let Some(msg) = msg {
        let fatal = msg.starts_with("Core worker crashed")
            || msg.starts_with("Core worker Core")
            || msg.starts_with("Core worker ResourceLimit")
            || msg.starts_with("Core worker Protocol")
            || msg.starts_with("Core worker Authentication")
            || msg.starts_with("Core requested a graceful shutdown");
        model.status = msg;
        if fatal {
            // Dropping the handle reaps (or force-kills) the worker. The
            // Library remains alive in the GUI process and shows the message.
            queue_game_source_teardown(&mut commands);
            next.set(AppState::Library);
        }
    }
}

fn drain_core_catalog_jobs(
    mut jobs: ResMut<CoreCatalogJobs>,
    mut model: ResMut<FrontendModel>,
    mut refresh_tx: bevy::ecs::message::MessageWriter<RefreshRequest>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    db_res: Option<Res<crate::db::DbResource>>,
    mut actions: MessageWriter<UiActionRequest>,
) {
    let Some(message) = jobs
        .receiver
        .as_ref()
        .and_then(|receiver| receiver.try_recv().ok())
    else {
        // No result yet. If the worker thread has died without sending a
        // result (a non-fatal Rust panic in the download/extract path — a
        // C-level `exit()` from a core still kills the process and can't be
        // caught here), recover the job slot so the UI isn't stuck "busy"
        // forever. `is_finished()` is true once the thread has exited.
        if jobs.busy
            && jobs
                .join
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
        {
            jobs.receiver = None;
            jobs.join = None;
            jobs.busy = false;
            jobs.current_install = None;
            model.status = "Core catalog job failed unexpectedly (worker thread died)".to_string();
            log::error!("core_catalog.job.worker_died");
            start_next_core_install(&mut model, &mut jobs);
        }
        return;
    };
    jobs.receiver = None;
    jobs.join = None;
    jobs.busy = false;
    let completed_install = jobs.current_install.take();

    match message {
        CoreCatalogJobResult::Catalog(Ok(entries)) => {
            let count = entries.len();
            // Write through to the DB so the next launch reads from cache.
            if let Some(db_res) = db_res.as_deref() {
                let repo = retrofeel_db::CatalogRepo::new(&db_res.db);
                let now = retrofeel_db::cores::now_secs();
                if let Err(error) = repo.replace_all(&entries, now) {
                    log::warn!("db.catalog_write_failed error=\"{error}\"");
                }
            }
            model.core_catalog = entries;
            model.status = format!("Core catalog loaded: {count} downloadable core(s)");
            queue_missing_library_cores(&mut model, &mut jobs);
            log::debug!("core_catalog.refresh.finished count={count}");
            next.set(state.get().clone());
        }
        CoreCatalogJobResult::Catalog(Err(error)) => {
            model.status = format!("Core catalog refresh failed: {error}");
            log::warn!("core_catalog.refresh.failed error=\"{error}\"");
            next.set(state.get().clone());
        }
        CoreCatalogJobResult::Install(Ok(result)) => {
            let installed_path = result.installed_path.clone();
            model.registry = result.scan_report;
            if let Some(db_res) = db_res.as_deref() {
                persist_core_registry(&db_res.db, &model.registry);
            }
            refresh_tx.write(RefreshRequest {
                next_state: Some(state.get().clone()),
            });
            model.mark_catalog_installed(&installed_path);
            model.status = format!("Installed core: {}", installed_path.display());
            log::debug!(
                "core_catalog.install.finished path=\"{}\"",
                installed_path.display()
            );
            if let Some(rom) = completed_install.and_then(|request| request.auto_launch_rom) {
                actions.write(UiActionRequest {
                    action: UiAction::LaunchRom(rom),
                });
            } else {
                next.set(state_after_setup_change(state.get(), &model));
            }
        }
        CoreCatalogJobResult::Install(Err(error)) => {
            model.status = format!("Core install failed: {error}");
            log::warn!("core_catalog.install.failed error=\"{error}\"");
            next.set(state.get().clone());
        }
    }
    start_next_core_install(&mut model, &mut jobs);
}

fn toggle_overlay(
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    match state.get() {
        AppState::InGame => next.set(AppState::InGameOverlay),
        AppState::InGameOverlay => next.set(AppState::InGame),
        _ => {}
    }
}

fn toggle_fast_forward(
    keys: Res<ButtonInput<KeyCode>>,
    mut ff: ResMut<FastForward>,
    core: Option<Res<CoreResource>>,
) {
    if !keys.just_pressed(KeyCode::KeyF) {
        return;
    }
    ff.0 = !ff.0;
    if let Some(core) = core.as_ref() {
        core.handle.set_fast_forward(ff.0);
    }
    log::info!("fast-forward: {}", ff.0);
}

fn handle_library_search_input(
    keys: Res<ButtonInput<KeyCode>>,
    focus_visible: Res<InputFocusVisible>,
    mut model: ResMut<FrontendModel>,
    mut next: ResMut<NextState<AppState>>,
) {
    if focus_visible.0 {
        return;
    }
    let mut changed = false;
    if keys.just_pressed(KeyCode::Escape) && !model.library_query.is_empty() {
        model.library_query.clear();
        changed = true;
    }
    if keys.just_pressed(KeyCode::Backspace) {
        changed |= model.library_query.pop().is_some();
    }
    for key in keys.get_just_pressed().copied() {
        if let Some(ch) = library_search_char(key) {
            model.library_query.push(ch);
            changed = true;
        }
    }
    if changed {
        next.set(AppState::Library);
    }
}

#[derive(Default)]
struct RecordingPollState {
    next_check: Option<Instant>,
    fingerprint: Option<u64>,
}

fn poll_recording_changes(
    mut state: Local<RecordingPollState>,
    mut model: ResMut<FrontendModel>,
    mut next: ResMut<NextState<AppState>>,
) {
    let now = Instant::now();
    if state.next_check.is_some_and(|deadline| now < deadline) {
        return;
    }
    state.next_check = Some(now + std::time::Duration::from_secs(1));
    let fingerprint = recording_manifest_fingerprint(&model.config.paths.recordings);
    match state.fingerprint.replace(fingerprint) {
        None => {}
        Some(previous) if previous != fingerprint => {
            if let Some(db) = model.db.clone() {
                // Session directories remain authoritative; the DB index is
                // refreshed in the same frame as the UI after worker output.
                model.refresh_with_db(&db);
            } else {
                model.recordings = ui::scan_recordings_dir(&model.config.paths.recordings);
            }
            next.set(AppState::Library);
        }
        Some(_) => {}
    }
}

fn schedule_transcription_backfill(
    mut backfill: ResMut<TranscriptionBackfill>,
    mut model: ResMut<FrontendModel>,
) {
    let config = &model.config.recording.transcription;
    if !config.automatic || !config.backfill_missing || config.external_model.is_none() {
        return;
    }
    if let Some(active) = backfill.active.as_ref() {
        let still_running = model.recordings.iter().any(|recording| {
            &recording.path == active
                && matches!(
                    recording.transcription_status,
                    retrofeel_types::TranscriptionJobState::Queued
                        | retrofeel_types::TranscriptionJobState::Running { .. }
                )
        });
        if still_running {
            return;
        }
        backfill.active = None;
    }
    // Queued/running state from a prior process is deliberately recoverable:
    // on the first pass there is no active in-memory job, so the newest stale
    // session is submitted again.
    let candidate = model.recordings.iter().find(|recording| {
        recording.has_mic
            && recording.transcript_text.is_none()
            && !matches!(
                recording.transcription_status,
                retrofeel_types::TranscriptionJobState::Failed { .. }
                    | retrofeel_types::TranscriptionJobState::Cancelled
            )
    });
    let Some(path) = candidate.map(|recording| recording.path.clone()) else {
        return;
    };
    match crate::recording::retry_transcription(&path, config.clone()) {
        Ok(()) => {
            if let Some(recording) = model
                .recordings
                .iter_mut()
                .find(|recording| recording.path == path)
            {
                recording.transcription_status = retrofeel_types::TranscriptionJobState::Queued;
            }
            backfill.active = Some(path);
            model.status = "Backfilling recording transcripts newest first".into();
        }
        Err(error) => {
            model.status = format!("Transcript backfill could not start: {error}");
        }
    }
}

fn recording_manifest_fingerprint(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| entry.path().join("manifest.json"))
        .filter_map(|path| std::fs::metadata(path).ok())
        .filter_map(|metadata| metadata.modified().ok())
        .filter_map(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .fold(0_u64, |hash, duration| {
            hash.rotate_left(7) ^ duration.as_nanos() as u64
        })
}

fn copy_recording_transcript(session: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let document: retrofeel_types::TranscriptDocument =
        serde_json::from_reader(std::fs::File::open(session.join("transcript.json"))?)?;
    let text = document.plain_text();
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if cfg!(windows) {
        ("cmd", &["/C", "clip"])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("clipboard stdin was not available"))?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("clipboard command exited with {status}");
    }
    Ok(())
}

fn reveal_path(path: &Path) -> anyhow::Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status()?
    } else if cfg!(windows) {
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .status()?
    } else {
        std::process::Command::new("xdg-open").arg(path).status()?
    };
    if !status.success() {
        anyhow::bail!("file manager exited with {status}");
    }
    Ok(())
}

fn dispatch_ui_actions(
    keys: Res<ButtonInput<KeyCode>>,
    focus: Res<InputFocus>,
    focus_visible: Res<InputFocusVisible>,
    pressed: UiButtonPresses,
    buttons: Query<&UiButton>,
    mut requests: MessageWriter<UiActionRequest>,
) {
    let mut dispatched = std::collections::HashSet::new();
    for (entity, interaction, button) in &pressed {
        if *interaction == Interaction::Pressed && dispatched.insert(entity) {
            requests.write(UiActionRequest {
                action: button.action.clone(),
            });
        }
    }

    let keyboard_activation = keys.just_pressed(KeyCode::Enter)
        || keys.just_pressed(KeyCode::NumpadEnter)
        || keys.just_pressed(KeyCode::Space);
    if !keyboard_activation || !focus_visible.0 {
        return;
    }
    let Some(entity) = focus.get() else {
        return;
    };
    let Ok(button) = buttons.get(entity) else {
        return;
    };
    if dispatched.insert(entity) {
        requests.write(UiActionRequest {
            action: button.action.clone(),
        });
    }
}

fn library_search_char(key: KeyCode) -> Option<char> {
    use KeyCode as K;
    match key {
        K::KeyA => Some('a'),
        K::KeyB => Some('b'),
        K::KeyC => Some('c'),
        K::KeyD => Some('d'),
        K::KeyE => Some('e'),
        K::KeyF => Some('f'),
        K::KeyG => Some('g'),
        K::KeyH => Some('h'),
        K::KeyI => Some('i'),
        K::KeyJ => Some('j'),
        K::KeyK => Some('k'),
        K::KeyL => Some('l'),
        K::KeyM => Some('m'),
        K::KeyN => Some('n'),
        K::KeyO => Some('o'),
        K::KeyP => Some('p'),
        K::KeyQ => Some('q'),
        K::KeyR => Some('r'),
        K::KeyS => Some('s'),
        K::KeyT => Some('t'),
        K::KeyU => Some('u'),
        K::KeyV => Some('v'),
        K::KeyW => Some('w'),
        K::KeyX => Some('x'),
        K::KeyY => Some('y'),
        K::KeyZ => Some('z'),
        K::Digit0 => Some('0'),
        K::Digit1 => Some('1'),
        K::Digit2 => Some('2'),
        K::Digit3 => Some('3'),
        K::Digit4 => Some('4'),
        K::Digit5 => Some('5'),
        K::Digit6 => Some('6'),
        K::Digit7 => Some('7'),
        K::Digit8 => Some('8'),
        K::Digit9 => Some('9'),
        K::Space => Some(' '),
        K::Minus => Some('-'),
        K::Period => Some('.'),
        K::Slash => Some('/'),
        _ => None,
    }
}

/// Scroll `Scrollable` content panels with the mouse wheel. `ScrollPosition` is
/// a required component of every `Node`. We target the scrollable panel under
/// the cursor by walking up from the hovered entity via `HoverMap`; if nothing
/// scrollable is under the pointer (or picking is unavailable), we fall back to
/// scrolling every `Scrollable` (screens have a single main panel, so the
/// fallback behaves correctly).
fn scroll_lists(
    mut wheel: MessageReader<MouseWheel>,
    hover_map: Option<Res<HoverMap>>,
    parents: Query<&ChildOf>,
    mut scrollables: Query<(&mut ScrollPosition, &ComputedNode), With<Scrollable>>,
) {
    const LINE_HEIGHT: f32 = 28.0;
    let mut delta = 0.0f32;
    for event in wheel.read() {
        delta += match event.unit {
            MouseScrollUnit::Line => event.y * LINE_HEIGHT,
            MouseScrollUnit::Pixel => event.y,
        };
    }
    if delta == 0.0 {
        return;
    }

    let mut targeted = false;
    if let Some(hover_map) = hover_map.as_ref() {
        'hovered: for hits in hover_map.values() {
            for &hovered in hits.keys() {
                let mut entity = hovered;
                loop {
                    if let Ok((mut pos, computed)) = scrollables.get_mut(entity) {
                        if apply_vertical_scroll(&mut pos, computed, delta) {
                            targeted = true;
                            break 'hovered;
                        }
                    }
                    match parents.get(entity) {
                        Ok(child_of) => entity = child_of.parent(),
                        Err(_) => break,
                    }
                }
            }
        }
    }

    if !targeted {
        for (mut pos, computed) in &mut scrollables {
            apply_vertical_scroll(&mut pos, computed, delta);
        }
    }
}

fn apply_vertical_scroll(pos: &mut ScrollPosition, computed: &ComputedNode, delta: f32) -> bool {
    let max_offset = ((computed.content_size().y - computed.size().y)
        * computed.inverse_scale_factor())
    .max(0.0);
    let (next, consumed) = bounded_scroll_offset(pos.0.y, delta, max_offset);
    pos.0.y = next;
    consumed
}

fn bounded_scroll_offset(current: f32, delta: f32, max_offset: f32) -> (f32, bool) {
    let next = (current - delta).clamp(0.0, max_offset);
    (next, next != current)
}

#[allow(clippy::too_many_arguments)]
fn handle_ui_actions(
    mut commands: Commands,
    mut requests: MessageReader<UiActionRequest>,
    mut action_id_counter: Local<u64>,
    diagnostics: Res<Diagnostics>,
    mut model: ResMut<FrontendModel>,
    mut recording: ResMut<RecordingUiState>,
    mut paused: ResMut<Paused>,
    mut next: ResMut<NextState<AppState>>,
    state: Res<State<AppState>>,
    mut pending: ResMut<PendingFrame>,
    mut pending_jobs: PendingUiJobs,
    mut images: ResMut<Assets<Image>>,
    mut install_jobs: InstallJobs,
    mut refresh_tx: bevy::ecs::message::MessageWriter<RefreshRequest>,
    core: Option<Res<CoreResource>>,
    screen_entities: Query<Entity, With<CoreScreen>>,
) {
    for request in requests.read() {
        *action_id_counter += 1;
        let action_id = *action_id_counter;
        let current_state = state.get().clone();
        let current_state_label = format!("{current_state:?}");
        let mut action_next_state = current_state.clone();
        let action_label = request.action.log_label();
        diagnostics.begin_action(action_id, action_label.clone(), current_state_label.clone());
        log::debug!(
            "ui.action.begin action_id={action_id} state=\"{current_state_label}\" action=\"{action_label}\""
        );
        let action_start = Instant::now();

        match &request.action {
            UiAction::GoLibrary => {
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::GoSettings => {
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::SetThemePreference(preference) => {
                if model.config.appearance.theme != *preference {
                    model.config.appearance.theme = *preference;
                    model.save();
                }
            }
            UiAction::SelectPreferencesSection(section) => {
                model.preferences_section = *section;
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::RefreshCoreCatalog => {
                start_core_catalog_refresh(&mut model, &mut install_jobs.cores);
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::InstallCore(entry) => {
                start_core_install(&mut model, &mut install_jobs.cores, entry.clone());
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::SelectLibraryView(view) => {
                model.library_view = view.clone();
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::SetLibraryDisplayMode(mode) => {
                model.library_display_mode = *mode;
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::SetLibraryTopView(view) => {
                model.library_top_view = *view;
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::ClearLibrarySearch => {
                model.library_query.clear();
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::SelectRecording(path) => {
                model.selected_recording = Some(path.clone());
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::CloseRecording => {
                model.selected_recording = None;
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::RetryTranscription(path) => {
                match crate::recording::retry_transcription(
                    path,
                    model.config.recording.transcription.clone(),
                ) {
                    Ok(()) => model.status = "Transcription queued".into(),
                    Err(error) => model.status = format!("Could not retry transcription: {error}"),
                }
                model.recordings = ui::scan_recordings_dir(&model.config.paths.recordings);
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::CopyTranscript(path) => match copy_recording_transcript(path) {
                Ok(()) => model.status = "Transcript copied".into(),
                Err(error) => model.status = format!("Could not copy transcript: {error}"),
            },
            UiAction::RevealSession(path) => {
                if let Err(error) = reveal_path(path) {
                    model.status = format!("Could not reveal session: {error}");
                }
            }
            UiAction::ToggleRecordingPlayback
            | UiAction::SeekRecordingRelative(_)
            | UiAction::SeekRecordingTo(_)
            | UiAction::AnalyzeRecording { .. } => {
                // Handled by the dedicated `.feel` viewer system so the
                // already-full general UI action system stays responsive.
            }
            UiAction::ImportBios { filename } => {
                start_bios_pick(&mut pending_jobs.pick, &diagnostics, filename.clone());
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::Refresh => {
                refresh_tx.write(RefreshRequest {
                    next_state: Some(current_state.clone()),
                });
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::PickPath(target) => {
                start_dir_pick(&mut pending_jobs.pick, &diagnostics, *target);
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::PickWhisperModel => {
                start_whisper_model_pick(&mut pending_jobs.pick, &diagnostics);
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::PickWhisperExecutable => {
                start_whisper_executable_pick(&mut pending_jobs.pick, &diagnostics);
                set_next_state(&mut next, &mut action_next_state, current_state.clone());
            }
            UiAction::InstallTranscriptionModel(id) => {
                start_model_install(&mut model, &mut install_jobs.models, id);
                model.preferences_section = PreferencesSection::Transcription;
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::CancelTranscriptionModelDownload => {
                if let Some(cancelled) = &install_jobs.models.cancelled {
                    cancelled.store(true, Ordering::Release);
                    model.status = "Cancelling model download…".into();
                }
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ToggleAutomaticTranscription => {
                model.toggle_automatic_transcription();
                model.preferences_section = PreferencesSection::Transcription;
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ClearWhisperModel => {
                model.set_whisper_model(None);
                model.preferences_section = PreferencesSection::Transcription;
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::LaunchRom(rom) => {
                if let Some(core_path) = model.resolved_core_for_rom(rom) {
                    // Warn (but do not block) if the ROM's system is missing a
                    // required BIOS — the file check is best-effort.
                    let bios_warning = missing_bios_warning(&model, rom, &core_path);
                    match launch_core(
                        &mut commands,
                        &mut pending,
                        &mut images,
                        &screen_entities,
                        &model.config,
                        core_path,
                        Some(rom.clone()),
                        None,
                        None,
                        0,
                    ) {
                        Ok(()) => {
                            model.note_recent_rom(rom);
                            paused.0 = false;
                            if let Some(warning) = bios_warning {
                                log::warn!("{warning}");
                                model.status = warning;
                            }
                            set_next_state(&mut next, &mut action_next_state, AppState::InGame);
                        }
                        Err(error) => {
                            model.status = format!("Launch failed: {error}");
                        }
                    }
                } else if let Some(entry) = preferred_core_entry_for_rom(&model, rom) {
                    queue_core_install(
                        &mut model,
                        &mut install_jobs.cores,
                        entry,
                        Some(rom.clone()),
                    );
                } else {
                    model.status = format!(
                        "No compatible core is installed or available for {}",
                        rom.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("this game")
                    );
                }
            }
            UiAction::AssignCore {
                extension,
                core_path,
            } => {
                model
                    .config
                    .set_core_override_for_extension(extension, core_path);
                model.save();
                refresh_tx.write(RefreshRequest {
                    next_state: Some(AppState::Settings),
                });
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::BindKeyboard(control) => {
                model.binding_capture = Some(BindingCapture::Keyboard {
                    control: control.clone(),
                });
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::BindGamepad(control) => {
                model.binding_capture = Some(BindingCapture::Gamepad {
                    control: control.clone(),
                });
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ResetKeyboardBindings => {
                model.config.global_input_bindings.keyboard = ui::keyboard_defaults().keyboard;
                commands.insert_resource(ActiveBindings(active_bindings(&model.config)));
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ResetGamepadBindings => {
                model.config.global_input_bindings.gamepad = ui::gamepad_defaults().gamepad;
                commands.insert_resource(ActiveBindings(active_bindings(&model.config)));
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::SetCoreOption {
                core_key,
                option_key,
                value,
            } => {
                model
                    .config
                    .core_options
                    .entry(core_key.clone())
                    .or_default()
                    .insert(option_key.clone(), value.clone());
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ClearCoreOption {
                core_key,
                option_key,
            } => {
                if let Some(options) = model.config.core_options.get_mut(core_key) {
                    options.remove(option_key);
                    if options.is_empty() {
                        model.config.core_options.remove(core_key);
                    }
                }
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ToggleIntegerScaling => {
                model.config.video.integer_scaling = !model.config.video.integer_scaling;
                commands.insert_resource(ScreenSettings::from(&model.config.video));
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ToggleAspectCorrection => {
                model.config.video.aspect_correction = !model.config.video.aspect_correction;
                commands.insert_resource(ScreenSettings::from(&model.config.video));
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ToggleFullscreen => {
                model.config.video.fullscreen = !model.config.video.fullscreen;
                model.save();
                // Rebuild the Settings toggle label; from the in-game HUD, stay.
                if current_state == AppState::Settings {
                    set_next_state(&mut next, &mut action_next_state, AppState::Settings);
                }
            }
            UiAction::ToggleAudio => {
                model.config.audio.enabled = !model.config.audio.enabled;
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::ToggleMicCapture => {
                model.config.recording.mic_enabled = !model.config.recording.mic_enabled;
                model.save();
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::StartRecording => {
                if !crate::recording::ffmpeg_available() {
                    model.status =
                        "Recording requires ffmpeg on PATH. Install ffmpeg and try again."
                            .to_string();
                } else if let Some(core) = core.as_ref() {
                    core.handle
                        .start_recording(model.config.paths.recordings.clone());
                    recording.active = true;
                    recording.started_at = Some(Instant::now());
                }
            }
            UiAction::StopRecording => {
                if let Some(core) = core.as_ref() {
                    core.handle.stop_recording();
                    recording.active = false;
                    recording.started_at = None;
                }
            }
            UiAction::ToggleRecording => {
                if let Some(core) = core.as_ref() {
                    if recording.active {
                        core.handle.stop_recording();
                        recording.active = false;
                        recording.started_at = None;
                    } else if !crate::recording::ffmpeg_available() {
                        model.status =
                            "Recording requires ffmpeg on PATH. Install ffmpeg and try again."
                                .to_string();
                    } else {
                        core.handle
                            .start_recording(model.config.paths.recordings.clone());
                        recording.active = true;
                        recording.started_at = Some(Instant::now());
                    }
                }
            }
            UiAction::TogglePause => {
                if let Some(core) = core.as_ref() {
                    paused.0 = !paused.0;
                    core.handle.set_paused(paused.0);
                }
            }
            UiAction::ResetCore => {
                if let Some(core) = core.as_ref() {
                    core.handle.reset();
                }
            }
            UiAction::VolumeUp => adjust_volume(&mut model, core.as_deref(), 0.1),
            UiAction::VolumeDown => adjust_volume(&mut model, core.as_deref(), -0.1),
            UiAction::OpenOverlay => {
                set_next_state(&mut next, &mut action_next_state, AppState::InGameOverlay);
            }
            UiAction::Screenshot => {
                match save_screenshot(&model.config, &pending) {
                    Ok(path) => model.status = format!("Screenshot saved: {}", path.display()),
                    Err(error) => model.status = format!("Screenshot failed: {error}"),
                }
                // Rebuild the overlay if that's where the button was invoked.
                if current_state == AppState::InGameOverlay {
                    set_next_state(&mut next, &mut action_next_state, AppState::InGameOverlay);
                }
            }
            UiAction::ExportRecording { target, session } => {
                let out = session.join("exports");
                match diagnostics::time_block("recording.export.ui", || {
                    retrofeel_export::export_session(target.engine(), session, &out)
                }) {
                    Ok(exported) => {
                        model.status =
                            format!("Exported {}: {}", target.label(), exported.path.display());
                    }
                    Err(error) => {
                        model.status = format!("Export failed for {}: {error}", target.label());
                    }
                }
                refresh_tx.write(RefreshRequest {
                    next_state: Some(AppState::Settings),
                });
                set_next_state(&mut next, &mut action_next_state, AppState::Settings);
            }
            UiAction::Resume => {
                set_next_state(&mut next, &mut action_next_state, AppState::InGame);
            }
            UiAction::QuitToLibrary => {
                if let Some(core) = core.as_ref() {
                    core.handle.stop_recording();
                }
                recording.active = false;
                recording.started_at = None;
                crate::overlay::request_visible(&mut commands, false);
                paused.0 = false;
                queue_game_source_teardown(&mut commands);
                pending.frame = None;
                for entity in &screen_entities {
                    commands.entity(entity).despawn();
                }
                refresh_tx.write(RefreshRequest {
                    next_state: Some(AppState::Library),
                });
                set_next_state(&mut next, &mut action_next_state, AppState::Library);
            }
            UiAction::SaveState(slot) => {
                if let Some(core) = core.as_ref() {
                    core.handle.save_state(*slot);
                }
            }
            UiAction::LoadState(slot) => {
                if let Some(core) = core.as_ref() {
                    core.handle.load_state(*slot);
                }
            }
            UiAction::LaunchSteamGame(app_id) => {
                let Some(game) = model
                    .config
                    .steam
                    .games
                    .iter()
                    .find(|game| game.app_id == *app_id)
                    .cloned()
                else {
                    model.status = format!("Steam app {app_id} is no longer configured");
                    continue;
                };
                if game.source == SteamGameSource::NativeSteam {
                    match launch_native_steam_game(game.app_id) {
                        Ok(()) => {
                            model.status = format!(
                                "Launching {} through Steam and waiting for its game window",
                                game.name
                            );
                            let target = crate::steam_thread::SteamTarget {
                                app_id: game.app_id,
                                name: game.name.clone(),
                                install_dir: game.install_dir,
                                wine_prefix: PathBuf::new(),
                                wine_bin: model.config.steam.wine_bin.clone(),
                                exe_name: String::new(),
                                capture_max_width: model.config.steam.capture_max_width,
                                capture_max_height: model.config.steam.capture_max_height,
                                capture_fps: model.config.steam.capture_fps,
                                capture_mic: model.config.recording.mic_enabled,
                                transcription: model.config.recording.transcription.clone(),
                                attach_pid: None,
                                native_steam_launch: true,
                                native_attach_timeout_seconds: model
                                    .config
                                    .steam
                                    .native_attach_timeout_seconds,
                            };
                            match launch_steam(
                                &mut commands,
                                &screen_entities,
                                &mut pending_jobs.steam,
                                &model.config,
                                target,
                                false,
                                None,
                            ) {
                                Ok(()) => {
                                    paused.0 = false;
                                }
                                Err(error) => {
                                    model.status = format!(
                                        "Steam launched {}, but RetroFeel could not attach: {error}",
                                        game.name
                                    );
                                    set_next_state(
                                        &mut next,
                                        &mut action_next_state,
                                        AppState::Library,
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            model.status = format!("Could not launch {}: {error}", game.name);
                            set_next_state(&mut next, &mut action_next_state, AppState::Library);
                        }
                    }
                    continue;
                }
                // In GameHub mode the game is launched by GameHub with its
                // own tuned Wine; we only attach to the running process.
                // Launching a second Wine (WineDirect) alongside GameHub's is
                // what caused input conflicts between the two environments.
                let attach_first = game.source == SteamGameSource::GameHub
                    || (game.source == SteamGameSource::Configured
                        && model.config.steam.launch_mode == SteamLaunchMode::GameHub);
                let attach_pid = if attach_first {
                    match retrofeel_steamcapture::find_wine_pid(&game.exe_name) {
                        Some(pid) => Some(pid),
                        None => {
                            model.status =
                                format!("Launch {} from GameHub first, then try again", game.name);
                            set_next_state(&mut next, &mut action_next_state, AppState::Library);
                            continue;
                        }
                    }
                } else {
                    None
                };
                let wine_prefix = if game.wine_prefix.as_os_str().is_empty() {
                    model
                        .config
                        .steam
                        .default_wine_prefix
                        .clone()
                        .unwrap_or_default()
                } else {
                    game.wine_prefix.clone()
                };
                if attach_pid.is_none() && wine_prefix.as_os_str().is_empty() {
                    model.status =
                        format!("Set a Wine prefix for {} before launching it", game.name);
                    set_next_state(&mut next, &mut action_next_state, AppState::Library);
                    continue;
                }
                let target = crate::steam_thread::SteamTarget {
                    app_id: game.app_id,
                    name: game.name.clone(),
                    install_dir: game.install_dir,
                    wine_prefix,
                    wine_bin: model.config.steam.wine_bin.clone(),
                    exe_name: game.exe_name,
                    capture_max_width: model.config.steam.capture_max_width,
                    capture_max_height: model.config.steam.capture_max_height,
                    capture_fps: model.config.steam.capture_fps,
                    capture_mic: model.config.recording.mic_enabled,
                    transcription: model.config.recording.transcription.clone(),
                    attach_pid,
                    native_steam_launch: false,
                    native_attach_timeout_seconds: model.config.steam.native_attach_timeout_seconds,
                };
                match launch_steam(
                    &mut commands,
                    &screen_entities,
                    &mut pending_jobs.steam,
                    &model.config,
                    target,
                    false,
                    None,
                ) {
                    Ok(()) => {
                        paused.0 = false;
                        model.status = format!("Attaching to {}…", game.name);
                    }
                    Err(error) => {
                        model.status = format!("Steam launch failed: {error}");
                        set_next_state(&mut next, &mut action_next_state, AppState::Library);
                    }
                }
            }
        }

        let next_state_label = format!("{action_next_state:?}");
        diagnostics::log_action_end(
            action_id,
            &current_state_label,
            &action_label,
            &next_state_label,
            action_start.elapsed(),
        );
        diagnostics.end_action(action_id);
    }
}

fn set_next_state(
    next: &mut NextState<AppState>,
    action_next_state: &mut AppState,
    state: AppState,
) {
    *action_next_state = state.clone();
    next.set(state);
}

fn start_core_catalog_refresh(model: &mut FrontendModel, jobs: &mut CoreCatalogJobs) {
    if jobs.busy {
        model.status = "Core catalog job already running".to_string();
        return;
    }
    let cores_dir = model.config.paths.cores.clone();
    let system_dir = model.config.paths.system.clone();
    let (sender, receiver) = unbounded();
    jobs.receiver = Some(receiver);
    jobs.busy = true;
    model.status = "Refreshing core catalog...".to_string();
    log::debug!(
        "core_catalog.refresh.start cores_dir=\"{}\" system_dir=\"{}\"",
        cores_dir.display(),
        system_dir.display()
    );

    match std::thread::Builder::new()
        .name("retrofeel-core-catalog".into())
        .spawn(move || {
            let result = diagnostics::time_block("core_catalog.refresh.job", || {
                refresh_core_catalog(cores_dir, system_dir).map_err(|error| error.to_string())
            });
            let _ = sender.send(CoreCatalogJobResult::Catalog(result));
        }) {
        Ok(handle) => jobs.join = Some(handle),
        Err(error) => {
            jobs.receiver = None;
            jobs.busy = false;
            model.status = format!("Could not start catalog refresh: {error}");
        }
    }
}

fn start_core_install(
    model: &mut FrontendModel,
    jobs: &mut CoreCatalogJobs,
    entry: CoreCatalogEntry,
) {
    queue_core_install(model, jobs, entry, None);
}

fn queue_core_install(
    model: &mut FrontendModel,
    jobs: &mut CoreCatalogJobs,
    entry: CoreCatalogEntry,
    auto_launch_rom: Option<PathBuf>,
) {
    if let Some(current) = jobs
        .current_install
        .as_mut()
        .filter(|current| current.entry.slug == entry.slug)
    {
        if auto_launch_rom.is_some() {
            current.auto_launch_rom = auto_launch_rom;
        }
        return;
    }
    if let Some(pending) = jobs
        .pending_installs
        .iter_mut()
        .find(|pending| pending.entry.slug == entry.slug)
    {
        if auto_launch_rom.is_some() {
            pending.auto_launch_rom = auto_launch_rom;
        }
        return;
    }
    jobs.pending_installs.push_back(CoreInstallRequest {
        entry,
        auto_launch_rom,
    });
    start_next_core_install(model, jobs);
}

fn start_next_core_install(model: &mut FrontendModel, jobs: &mut CoreCatalogJobs) {
    if jobs.busy {
        return;
    }
    let Some(request) = jobs.pending_installs.pop_front() else {
        return;
    };
    let entry = request.entry.clone();
    let label = entry.display_name.clone();
    let cores_dir = model.config.paths.cores.clone();
    let system_dir = model.config.paths.system.clone();
    let (sender, receiver) = unbounded();
    jobs.receiver = Some(receiver);
    jobs.busy = true;
    jobs.current_install = Some(request);
    model.status = format!("Installing core: {label}");
    log::debug!(
        "core_catalog.install.start label=\"{label}\" cores_dir=\"{}\" system_dir=\"{}\"",
        cores_dir.display(),
        system_dir.display()
    );

    match std::thread::Builder::new()
        .name("retrofeel-core-install".into())
        .spawn(move || {
            let result = diagnostics::time_block("core_catalog.install.job", || {
                install_core(&entry, cores_dir, system_dir).map_err(|error| error.to_string())
            });
            let _ = sender.send(CoreCatalogJobResult::Install(result));
        }) {
        Ok(handle) => jobs.join = Some(handle),
        Err(error) => {
            jobs.receiver = None;
            jobs.busy = false;
            jobs.current_install = None;
            model.status = format!("Could not start core install: {error}");
        }
    }
}

fn preferred_core_entry_for_rom(model: &FrontendModel, rom: &Path) -> Option<CoreCatalogEntry> {
    let system = retrofeel_types::system_for_rom(rom, None)?;
    let slug = retrofeel_types::preferred_core_slug(system.id)?;
    model
        .core_catalog
        .iter()
        .find(|entry| entry.slug == slug)
        .cloned()
}

fn queue_missing_library_cores(model: &mut FrontendModel, jobs: &mut CoreCatalogJobs) {
    let mut queued_slugs = std::collections::HashSet::new();
    let entries: Vec<CoreCatalogEntry> = model
        .roms
        .iter()
        .filter(|rom| model.resolved_core_for_rom(&rom.path).is_none())
        .filter_map(|rom| preferred_core_entry_for_rom(model, &rom.path))
        .filter(|entry| queued_slugs.insert(entry.slug.clone()))
        .collect();
    for entry in entries {
        queue_core_install(model, jobs, entry, None);
    }
}

fn persist_core_registry(db: &retrofeel_db::Db, report: &CoreScanReport) {
    let now = retrofeel_db::cores::now_secs();
    let rows: Vec<retrofeel_db::cores::CoreRow> = report
        .registry
        .cores
        .iter()
        .map(|core| {
            let (file_size, file_mtime) =
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
                file_size,
                file_mtime,
                last_seen: now,
            }
        })
        .collect();
    if let Err(error) = retrofeel_db::CoresRepo::new(db).replace_all(&rows) {
        log::warn!("db.cores_write_failed error=\"{error}\"");
    }
}

/// Start a folder picker for a config path (`UiAction::PickPath`).
///
/// Runs the **synchronous** `rfd::FileDialog::pick_folder` on a dedicated
/// worker thread. rfd's async API falls back to a sync modal on macOS when
/// `NSApplication::isRunning()` is false (Bevy's `pump_events` mode), which
/// blocks the main thread. By using the sync API on a worker thread, the
/// main thread keeps rendering while the user browses. The result is
/// delivered via a channel that `poll_pending_pick` checks each frame.
fn start_dir_pick(pending: &mut PendingPick, diag: &Diagnostics, target: ui::PathTarget) {
    if pending.receiver.is_some() {
        log::warn!(
            "file_picker.busy kind=directory target=\"{}\"",
            target.log_label()
        );
        return;
    }
    let target_label = target.log_label();
    log::debug!("file_picker.begin kind=directory target=\"{target_label}\"");
    let timing =
        diagnostics::Timing::start(format!("file_picker.pick_folder target={target_label}"));
    diag.set_picker_open(true);

    let (sender, receiver) = crossbeam_channel::bounded(1);
    pending.receiver = Some(receiver);
    pending.kind = Some(PickKind::Dir(target));
    pending.timing = Some(timing);

    let _ = std::thread::Builder::new()
        .name("retrofeel-file-picker".into())
        .spawn(move || {
            let result = rfd::FileDialog::new().pick_folder();
            let _ = sender.send(result);
        });
}

/// Drain `RefreshRequest` events and spawn background refresh workers. Runs
/// before `drain_refresh_job` so a refresh can complete and be applied in the
/// same frame when it's fast enough.
fn start_refresh_from_event(
    mut events: bevy::ecs::message::MessageReader<RefreshRequest>,
    mut job: ResMut<RefreshJob>,
    model: Res<FrontendModel>,
) {
    for event in events.read() {
        start_refresh(&mut job, &model.config, event.next_state.clone());
    }
}

/// Start a background refresh of the model's heavy fields (cores, BIOS, ROMs, ROMs,
/// recordings). The scans run on a worker thread so Bevy's render loop keeps
/// running. [`drain_refresh_job`] applies the results when done.
///
/// If a refresh is already in flight, this is a no-op (the pending refresh
/// will pick up the current config state when it completes).
fn start_refresh(job: &mut RefreshJob, config: &RetroFeelConfig, next_state: Option<AppState>) {
    if job.busy {
        log::debug!("refresh.already_busy — skipping");
        return;
    }
    let (sender, receiver) = crossbeam_channel::bounded(1);
    job.receiver = Some(receiver);
    job.next_state = next_state;
    job.busy = true;

    let config = config.clone();
    job.join = std::thread::Builder::new()
        .name("retrofeel-refresh".into())
        .spawn(move || {
            let result = do_refresh(&config);
            let _ = sender.send(result);
        })
        .ok();
}

/// Compute the model's heavy fields on a worker thread. This is the same logic
/// as `FrontendModel::refresh` but without holding the model (which lives on
/// the main thread).
fn do_refresh(config: &RetroFeelConfig) -> RefreshResult {
    let registry = diagnostics::time_block("refresh.scan_cores", || {
        match CoreRegistry::scan_dir(&config.paths.cores, &config.paths.system) {
            Ok(report) => report,
            Err(error) => {
                log::warn!("refresh.scan_cores_failed error=\"{error}\"");
                CoreScanReport::default()
            }
        }
    });

    let bios = diagnostics::time_block("refresh.scan_bios", || {
        match scan_system_dir(&config.paths.system) {
            Ok(report) => Some(report),
            Err(error) => {
                log::warn!("refresh.scan_bios_failed error=\"{error}\"");
                None
            }
        }
    });

    let roms =
        diagnostics::time_block("refresh.scan_roms", || ui::scan_rom_dirs(config, &registry));

    let recordings = diagnostics::time_block("refresh.scan_recordings", || {
        ui::scan_recordings_dir(&config.paths.recordings)
    });
    let steam_games = diagnostics::time_block("refresh.scan_steam", || {
        retrofeel_backend::discover_installed_steam_games(&config.steam)
    });

    let status = format!(
        "{} ROMs, {} cores, {} BIOS entries checked, {} recordings, {} Steam games",
        roms.len(),
        registry.registry.cores.len(),
        bios.as_ref().map(|b| b.checks.len()).unwrap_or(0),
        recordings.len(),
        steam_games.len(),
    );

    RefreshResult {
        registry,
        bios,
        roms,
        recordings,
        steam_games,
        status,
    }
}

/// Drain the background refresh job and apply results to the model.
fn drain_refresh_job(
    mut job: ResMut<RefreshJob>,
    mut model: ResMut<FrontendModel>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
    db_res: Option<Res<crate::db::DbResource>>,
) {
    let Some(receiver) = &job.receiver else {
        return;
    };
    let Some(result) = receiver.try_recv().ok() else {
        // Worker still running. If the thread died without sending, recover.
        if job.busy && job.join.as_ref().is_some_and(|h| h.is_finished()) {
            log::warn!("refresh.worker_died_without_result");
            job.busy = false;
            job.receiver = None;
            job.join = None;
        }
        return;
    };

    model.registry = result.registry;
    model.bios = result.bios;
    model.roms = result.roms;
    model.recordings = result.recordings;
    model.config.steam.games = result.steam_games;
    model.status = result.status;
    if let Some(db) = db_res.as_deref() {
        sync_steam_games_to_db(&db.db, &model.config.steam.games);
    }

    job.busy = false;
    job.receiver = None;
    job.join = None;
    let target_state = job.next_state.take();
    if let Some(target) = target_state {
        if state.get() != &target {
            next.set(target);
        }
    }
}
/// Mirrors [`start_dir_pick`]: sync picker on a worker thread.
fn start_bios_pick(pending: &mut PendingPick, diag: &Diagnostics, filename: String) {
    if pending.receiver.is_some() {
        log::warn!("file_picker.busy kind=bios_file filename=\"{filename}\"");
        return;
    }
    log::debug!("file_picker.begin kind=bios_file filename=\"{filename}\"");
    let timing = diagnostics::Timing::start("file_picker.pick_bios_file");
    diag.set_picker_open(true);

    let (sender, receiver) = crossbeam_channel::bounded(1);
    pending.receiver = Some(receiver);
    pending.kind = Some(PickKind::BiosFile { filename });
    pending.timing = Some(timing);

    let _ = std::thread::Builder::new()
        .name("retrofeel-file-picker".into())
        .spawn(move || {
            let result = rfd::FileDialog::new().pick_file();
            let _ = sender.send(result);
        });
}

/// Start a file picker for a whisper.cpp GGML/GGUF model. Sync picker on a
/// worker thread — same pattern as `start_dir_pick`.
fn start_whisper_model_pick(pending: &mut PendingPick, diag: &Diagnostics) {
    if pending.receiver.is_some() {
        log::warn!("file_picker.busy kind=whisper_model");
        return;
    }
    log::debug!("file_picker.begin kind=whisper_model");
    let timing = diagnostics::Timing::start("file_picker.pick_whisper_model");
    diag.set_picker_open(true);

    let (sender, receiver) = crossbeam_channel::bounded(1);
    pending.receiver = Some(receiver);
    pending.kind = Some(PickKind::WhisperModel);
    pending.timing = Some(timing);

    let _ = std::thread::Builder::new()
        .name("retrofeel-file-picker".into())
        .spawn(move || {
            let result = rfd::FileDialog::new()
                .add_filter("Whisper model", &["bin", "gguf"])
                .pick_file();
            let _ = sender.send(result);
        });
}

fn start_whisper_executable_pick(pending: &mut PendingPick, diag: &Diagnostics) {
    if pending.receiver.is_some() {
        log::warn!("file_picker.busy kind=whisper_executable");
        return;
    }
    let timing = diagnostics::Timing::start("file_picker.pick_whisper_executable");
    diag.set_picker_open(true);
    let (sender, receiver) = crossbeam_channel::bounded(1);
    pending.receiver = Some(receiver);
    pending.kind = Some(PickKind::WhisperExecutable);
    pending.timing = Some(timing);
    let _ = std::thread::Builder::new()
        .name("retrofeel-file-picker".into())
        .spawn(move || {
            let result = rfd::FileDialog::new().pick_file();
            let _ = sender.send(result);
        });
}

/// Check the in-flight file picker for a result (non-blocking `try_recv`).
/// Runs in `Update` before [`handle_ui_actions`] so a picker that resolves
/// this frame is applied before any new button press is handled.
///
/// The picker runs on a dedicated worker thread using the synchronous
/// `rfd::FileDialog` API (the async API falls back to a sync modal on macOS
/// with Bevy's `pump_events` event loop, which would block the main thread).
/// The result arrives via a channel; we `try_recv` each frame.
#[allow(clippy::too_many_arguments)]
fn poll_pending_pick(
    mut pending: ResMut<PendingPick>,
    diag: Res<Diagnostics>,
    mut model: ResMut<FrontendModel>,
    mut bindings: ResMut<ActiveBindings>,
    mut refresh_tx: bevy::ecs::message::MessageWriter<RefreshRequest>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
) {
    let Some(receiver) = &pending.receiver else {
        return;
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(_) => return, // Still waiting for the user to pick.
    };
    // The picker resolved — take the kind + timing + receiver out of the resource.
    let kind = pending.kind.take().unwrap();
    let timing = pending.timing.take();
    pending.receiver = None;
    diag.set_picker_open(false);
    if let Some(timing) = timing {
        timing.finish();
    }

    match kind {
        PickKind::Dir(target) => {
            let target_label = target.log_label();
            let Some(path) = result else {
                log::debug!("file_picker.cancelled kind=directory target=\"{target_label}\"");
                return;
            };
            model.apply_dir_choice(target, path);
            bindings.0 = active_bindings(&model.config);
            model.preferences_section = preferences_section_for_path_target(target);
            let next_app_state = state_after_setup_change(state.get(), &model);
            refresh_tx.write(RefreshRequest {
                next_state: Some(next_app_state.clone()),
            });
            next.set(next_app_state);
        }
        PickKind::BiosFile { filename } => {
            let Some(source) = result else {
                log::debug!("file_picker.cancelled kind=bios_file filename=\"{filename}\"");
                return;
            };
            match model.apply_bios_import(&filename, &source) {
                Ok(dest) => {
                    log::debug!(
                        "file_picker.imported kind=bios_file filename=\"{filename}\" dest=\"{}\"",
                        dest.display()
                    );
                }
                Err(error) => {
                    model.status = format!("BIOS import failed: {error}");
                    log::warn!(
                        "file_picker.import_failed kind=bios_file filename=\"{filename}\" error=\"{error}\""
                    );
                }
            }
            next.set(AppState::Settings);
        }
        PickKind::WhisperModel => {
            let Some(path) = result else {
                log::debug!("file_picker.cancelled kind=whisper_model");
                return;
            };
            log::debug!(
                "file_picker.selected kind=whisper_model path=\"{}\"",
                path.display()
            );
            model.set_whisper_model(Some(path));
            model.preferences_section = PreferencesSection::Transcription;
            next.set(AppState::Settings);
        }
        PickKind::WhisperExecutable => {
            let Some(path) = result else {
                log::debug!("file_picker.cancelled kind=whisper_executable");
                return;
            };
            let mut validation = model.config.recording.transcription.clone();
            validation.external_executable = Some(path.clone());
            if crate::recording::discover_whisper_executable(&validation).as_deref()
                != Some(path.as_path())
            {
                model.status = format!(
                    "The selected file is not a working Whisper executable: {}",
                    path.display()
                );
            } else {
                model.set_whisper_executable(Some(path));
            }
            model.preferences_section = PreferencesSection::Transcription;
            next.set(AppState::Settings);
        }
    }
}

fn state_after_setup_change(current: &AppState, _model: &FrontendModel) -> AppState {
    match current {
        AppState::FirstRun => AppState::Library,
        _ => current.clone(),
    }
}

fn preferences_section_for_path_target(target: ui::PathTarget) -> PreferencesSection {
    match target {
        ui::PathTarget::Cores => PreferencesSection::Cores,
        ui::PathTarget::System => PreferencesSection::SystemFiles,
        ui::PathTarget::Roms
        | ui::PathTarget::Saves
        | ui::PathTarget::States
        | ui::PathTarget::Recordings => PreferencesSection::Library,
    }
}

fn transcription_models_dir(config: &RetroFeelConfig) -> PathBuf {
    config
        .paths
        .recordings
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("transcription-models")
}

fn start_model_install(model: &mut FrontendModel, jobs: &mut ModelInstallJobs, id: &str) {
    if jobs.active_id.is_some() {
        model.status = "Another transcription model download is already running".into();
        return;
    }
    let Some(descriptor) = crate::transcription::model_catalog()
        .into_iter()
        .find(|descriptor| descriptor.id == id)
    else {
        model.status = format!("Unknown transcription model: {id}");
        return;
    };
    let models_dir = transcription_models_dir(&model.config);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_worker = cancelled.clone();
    let (sender, receiver) = bounded(32);
    let label = descriptor.display_name.clone();
    jobs.active_id = Some(descriptor.id.clone());
    jobs.receiver = Some(receiver);
    jobs.cancelled = Some(cancelled);
    model.status = format!("Downloading {label}…");
    match std::thread::Builder::new()
        .name("retrofeel-model-install".into())
        .spawn(move || {
            let result = crate::transcription::install_model(
                &descriptor,
                &models_dir,
                cancelled_worker.clone(),
                |event| {
                    let _ = sender.send(event);
                },
            );
            if let Err(error) = result {
                if !cancelled_worker.load(Ordering::Acquire) {
                    let _ = sender.send(crate::transcription::ModelInstallEvent::Failed(error));
                }
            }
        }) {
        Ok(join) => jobs.join = Some(join),
        Err(error) => {
            jobs.active_id = None;
            jobs.receiver = None;
            jobs.cancelled = None;
            model.status = format!("Could not start model download: {error}");
        }
    }
}

fn drain_model_install(
    mut jobs: ResMut<ModelInstallJobs>,
    mut model: ResMut<FrontendModel>,
    state: Res<State<AppState>>,
    mut next: ResMut<NextState<AppState>>,
) {
    let Some(receiver) = jobs.receiver.as_ref() else {
        return;
    };
    let events: Vec<_> = receiver.try_iter().collect();
    if events.is_empty() {
        return;
    }
    let mut terminal = false;
    for event in events {
        match event {
            crate::transcription::ModelInstallEvent::Progress { downloaded, total } => {
                let percent = downloaded.saturating_mul(100) / total.max(1);
                model.status = format!("Downloading transcription model… {percent}%");
            }
            crate::transcription::ModelInstallEvent::Complete { model_path } => {
                let id = jobs.active_id.clone();
                if let Some(descriptor) = crate::transcription::model_catalog()
                    .into_iter()
                    .find(|descriptor| Some(&descriptor.id) == id.as_ref())
                {
                    model.config.recording.transcription.provider = descriptor.provider;
                    model.config.recording.transcription.selected_model_id = Some(descriptor.id);
                    model.config.recording.transcription.external_model = Some(model_path.clone());
                    if descriptor.provider == retrofeel_types::TranscriptionProvider::WhisperCpp {
                        model.config.recording.whisper_model = Some(model_path);
                    } else {
                        model.config.recording.whisper_model = None;
                    }
                    model.save();
                }
                model.status = "Transcription model installed and selected".into();
                terminal = true;
            }
            crate::transcription::ModelInstallEvent::Cancelled => {
                model.status = "Model download cancelled; partial data kept for resume".into();
                terminal = true;
            }
            crate::transcription::ModelInstallEvent::Failed(error) => {
                model.status = format!("Model download needs attention: {error}");
                terminal = true;
            }
        }
    }
    if terminal {
        jobs.active_id = None;
        jobs.receiver = None;
        jobs.cancelled = None;
        if let Some(join) = jobs.join.take() {
            let _ = join.join();
        }
        if *state.get() == AppState::Settings {
            next.set(AppState::Settings);
        }
    }
}

fn apply_window_mode(config: &RetroFeelConfig, windows: &mut Query<&mut Window>) {
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    window.mode = if config.video.fullscreen {
        WindowMode::BorderlessFullscreen(MonitorSelection::Primary)
    } else {
        WindowMode::Windowed
    };
}

fn recording_hotkey(
    keys: Res<ButtonInput<KeyCode>>,
    core: Option<Res<CoreResource>>,
    mut model: ResMut<FrontendModel>,
    mut recording: ResMut<RecordingUiState>,
) {
    if !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    let Some(core) = core else {
        return;
    };
    if recording.active {
        core.handle.stop_recording();
        recording.active = false;
        recording.started_at = None;
    } else {
        if !crate::recording::ffmpeg_available() {
            model.status =
                "Recording requires ffmpeg on PATH. Install ffmpeg and try again.".to_string();
            return;
        }
        core.handle
            .start_recording(model.config.paths.recordings.clone());
        recording.active = true;
        recording.started_at = Some(Instant::now());
    }
}

fn screenshot_hotkey(
    keys: Res<ButtonInput<KeyCode>>,
    model: Res<FrontendModel>,
    pending: Res<PendingFrame>,
) {
    if !keys.just_pressed(KeyCode::KeyP) {
        return;
    }
    match save_screenshot(&model.config, &pending) {
        Ok(path) => log::info!("screenshot saved: {}", path.display()),
        Err(error) => log::warn!("screenshot failed: {error}"),
    }
}

fn save_screenshot(config: &RetroFeelConfig, pending: &PendingFrame) -> anyhow::Result<PathBuf> {
    diagnostics::time_block("screenshot.save", || {
        let frame = pending
            .frame
            .as_ref()
            .and_then(|core_frame| core_frame.frame.as_ref())
            .ok_or_else(|| anyhow::anyhow!("no core frame is available"))?;
        let dir = config.paths.recordings.join("screenshots");
        std::fs::create_dir_all(&dir)?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("screenshot-{millis}.png"));
        let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
            .ok_or_else(|| anyhow::anyhow!("frame buffer size did not match dimensions"))?;
        image.save(&path)?;
        Ok(path)
    })
}

/// Build a warning if the ROM's system has a required BIOS that is missing or
/// has a bad hash. Returns `None` when nothing is required or everything checks
/// out. Uses the resolved core's name to disambiguate disc-based systems.
fn missing_bios_warning(
    model: &FrontendModel,
    rom: &std::path::Path,
    core_path: &std::path::Path,
) -> Option<String> {
    let core_name = model
        .registry
        .registry
        .find_by_path(core_path)
        .map(|core| core.library_name.clone());
    let system = retrofeel_types::system_for_rom(rom, core_name.as_deref())?;
    let bios = model.bios.as_ref()?;
    let missing: Vec<String> = bios
        .missing_required_for_system(system.id)
        .map(|check| check.entry.filename.clone())
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "Warning: {} may need BIOS not found in the System folder: {}",
            system.name,
            missing.join(", ")
        ))
    }
}

/// Nudge the audio volume by `delta`, apply it to the live cpal stream, and
/// persist it to config. Clamped to `[0.0, 2.0]` (0-200%).
fn adjust_volume(model: &mut FrontendModel, core: Option<&CoreResource>, delta: f32) {
    let volume = (model.config.audio.volume + delta).clamp(0.0, 2.0);
    model.config.audio.volume = volume;
    if let Some(core) = core {
        core.handle.set_volume(volume);
    }
    model.save();
    model.status = format!("Volume: {:.0}%", volume * 100.0);
}

fn capture_binding(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    mut model: ResMut<FrontendModel>,
    mut bindings: ResMut<ActiveBindings>,
    mut next: ResMut<NextState<AppState>>,
) {
    let Some(capture) = model.binding_capture.clone() else {
        return;
    };
    if keys.just_pressed(KeyCode::Escape) {
        model.binding_capture = None;
        next.set(AppState::Settings);
        return;
    }
    match capture {
        BindingCapture::Keyboard { control } => {
            let Some(key) = keys.get_just_pressed().next().copied() else {
                return;
            };
            model
                .config
                .global_input_bindings
                .keyboard
                .insert(control, keycode_to_binding(key));
            model.binding_capture = None;
            bindings.0 = active_bindings(&model.config);
            model.save();
            next.set(AppState::Settings);
        }
        BindingCapture::Gamepad { control } => {
            let Some(button) = first_just_pressed_gamepad_button(&gamepads) else {
                return;
            };
            model
                .config
                .global_input_bindings
                .gamepad
                .insert(control, gamepad_button_to_binding(button));
            model.binding_capture = None;
            bindings.0 = active_bindings(&model.config);
            model.save();
            next.set(AppState::Settings);
        }
    }
}

fn first_just_pressed_gamepad_button(gamepads: &Query<&Gamepad>) -> Option<GamepadButton> {
    for pad in gamepads {
        for button in candidate_gamepad_buttons() {
            if pad.just_pressed(*button) {
                return Some(*button);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn launch_core(
    commands: &mut Commands,
    pending: &mut PendingFrame,
    images: &mut Assets<Image>,
    screen_entities: &Query<Entity, With<CoreScreen>>,
    config: &RetroFeelConfig,
    core_path: PathBuf,
    rom: Option<PathBuf>,
    auto_record_frames: Option<u64>,
    auto_record_pause_at: Option<u64>,
    auto_record_pause_ms: u64,
) -> Result<(), core_thread::CoreThreadError> {
    diagnostics::time_block("rom_launch.setup", || {
        queue_game_source_teardown(commands);
        for entity in screen_entities {
            commands.entity(entity).despawn();
        }

        let (handle, audio_out) = core_thread::spawn(
            core_path,
            rom,
            config.paths.system.to_string_lossy().to_string(),
            config.audio.clone(),
            Some(config.clone()),
        )?;
        pending.width = handle.base_width;
        pending.height = handle.base_height;
        pending.frame = None;

        commands.insert_resource(ScreenSettings::from(&config.video));
        screen::setup_screen(commands, images);
        if let Some(frames) = auto_record_frames {
            if auto_record_pause_at.is_some() {
                handle.start_recording_debug(
                    config.paths.recordings.clone(),
                    frames,
                    auto_record_pause_at,
                    auto_record_pause_ms,
                );
            } else {
                handle.start_recording_for(config.paths.recordings.clone(), frames);
            }
        }
        commands.insert_resource(CoreResource {
            handle: Box::new(handle),
        });
        if let Some(audio_out) = audio_out {
            commands.insert_resource(audio_out);
        }
        Ok(())
    })
}

/// Launch a Steam game under Wine with screen + input capture.
/// Mirrors `launch_core` but calls `steam_thread::spawn` instead of
/// `core_thread::spawn`.
#[allow(clippy::too_many_arguments)]
fn launch_steam(
    commands: &mut Commands,
    screen_entities: &Query<Entity, With<CoreScreen>>,
    pending_launch: &mut PendingSteamLaunch,
    config: &RetroFeelConfig,
    target: crate::steam_thread::SteamTarget,
    auto_record: bool,
    auto_record_frames: Option<u64>,
) -> Result<(), crate::steam_thread::SteamThreadError> {
    crate::overlay::request_visible(commands, false);
    queue_game_source_teardown(commands);
    for entity in screen_entities {
        commands.entity(entity).despawn();
    }

    let activate_pid = target.attach_pid;
    let audio = config.audio.clone();
    let recordings = config.paths.recordings.clone();
    pending_launch.start(auto_record, auto_record_frames, activate_pid, move || {
        crate::steam_thread::spawn(target, audio, recordings)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ButtonVariant;
    use retrofeel_backend::CoreInstallStatus;

    #[test]
    fn native_steam_launch_url_uses_app_id() {
        assert_eq!(native_steam_launch_url(2379780), "steam://run/2379780");
    }

    #[test]
    fn steam_window_discovery_stays_off_the_bevy_thread() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let worker_gate = gate.clone();
        let mut pending = PendingSteamLaunch::default();

        pending
            .start(false, Some(60), Some(42), move || {
                worker_gate.wait();
                Err(crate::steam_thread::SteamThreadError::Capture(
                    "expected test failure".into(),
                ))
            })
            .unwrap();

        assert!(pending.is_busy());
        assert!(matches!(
            pending.receiver.as_ref().unwrap().try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
        assert!(matches!(
            pending.start(false, None, None, || unreachable!()),
            Err(crate::steam_thread::SteamThreadError::AlreadyLaunching)
        ));

        gate.wait();
        let result = pending
            .receiver
            .as_ref()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            result,
            Err(crate::steam_thread::SteamThreadError::Capture(_))
        ));
        pending.join.take().unwrap().join().unwrap();
    }

    struct SlowDropSource {
        input: core_thread::LatestInput,
        latest: core_thread::LatestFrame,
        status: core_thread::CoreStatus,
        dropped: std::sync::mpsc::Sender<()>,
    }

    impl SlowDropSource {
        fn new(dropped: std::sync::mpsc::Sender<()>) -> Self {
            Self {
                input: Arc::new(std::sync::Mutex::new(InputSnapshot::default())),
                latest: Arc::new(std::sync::Mutex::new(None)),
                status: Arc::new(std::sync::Mutex::new(None)),
                dropped,
            }
        }
    }

    impl Drop for SlowDropSource {
        fn drop(&mut self) {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = self.dropped.send(());
        }
    }

    impl core_thread::GameSource for SlowDropSource {
        fn kind(&self) -> core_thread::SourceKind {
            core_thread::SourceKind::Libretro
        }

        fn input_slot(&self) -> &core_thread::LatestInput {
            &self.input
        }

        fn latest_frame_slot(&self) -> &core_thread::LatestFrame {
            &self.latest
        }

        fn status(&self) -> &core_thread::CoreStatus {
            &self.status
        }

        fn base_width(&self) -> u32 {
            320
        }

        fn base_height(&self) -> u32 {
            240
        }

        fn fps(&self) -> f64 {
            60.0
        }

        fn sample_rate(&self) -> f64 {
            48_000.0
        }

        fn set_paused(&self, _paused: bool) {}
        fn set_volume(&self, _volume: f32) {}
        fn set_fast_forward(&self, _on: bool) {}
        fn start_recording(&self, _recordings_dir: PathBuf) {}
        fn start_recording_for(&self, _recordings_dir: PathBuf, _frames: u64) {}

        fn start_recording_debug(
            &self,
            _recordings_dir: PathBuf,
            _frames: u64,
            _pause_at_frame: Option<u64>,
            _pause_duration_ms: u64,
        ) {
        }

        fn stop_recording(&self) {}
        fn save_state(&self, _slot: u8) {}
        fn load_state(&self, _slot: u8) {}
        fn reset(&self) {}
    }

    #[test]
    fn game_source_teardown_does_not_block_the_bevy_world() {
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let mut world = World::new();
        world.insert_resource(CoreResource {
            handle: Box::new(SlowDropSource::new(dropped_tx)),
        });

        let started = Instant::now();
        detach_game_source(&mut world);
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "game-source removal blocked the Bevy world for {elapsed:?}"
        );
        assert!(!world.contains_resource::<CoreResource>());
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("game source should finish teardown off the Bevy thread");
    }

    #[derive(Resource, Default)]
    struct ReceivedActions(Vec<UiAction>);

    fn collect_actions(
        mut requests: MessageReader<UiActionRequest>,
        mut received: ResMut<ReceivedActions>,
    ) {
        received
            .0
            .extend(requests.read().map(|request| request.action.clone()));
    }

    fn dispatch_count(pointer: bool, keyboard_key: Option<KeyCode>) -> usize {
        let mut app = App::new();
        app.add_message::<UiActionRequest>()
            .insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(InputFocus::default())
            .insert_resource(InputFocusVisible(keyboard_key.is_some()))
            .init_resource::<ReceivedActions>()
            .add_systems(Update, (dispatch_ui_actions, collect_actions).chain());

        let entity = app
            .world_mut()
            .spawn((
                Button,
                if pointer {
                    Interaction::Pressed
                } else {
                    Interaction::None
                },
                UiButton {
                    action: UiAction::Refresh,
                    variant: ButtonVariant::Secondary,
                },
            ))
            .id();
        if let Some(key) = keyboard_key {
            app.world_mut().resource_mut::<InputFocus>().set(entity);
            app.world_mut()
                .resource_mut::<ButtonInput<KeyCode>>()
                .press(key);
        }

        app.update();
        app.world().resource::<ReceivedActions>().0.len()
    }

    #[test]
    fn pointer_and_keyboard_activation_dispatch_once() {
        assert_eq!(dispatch_count(true, None), 1);
        assert_eq!(dispatch_count(false, Some(KeyCode::Enter)), 1);
        assert_eq!(dispatch_count(false, Some(KeyCode::Space)), 1);
        assert_eq!(dispatch_count(true, Some(KeyCode::Enter)), 1);
        assert_eq!(dispatch_count(true, Some(KeyCode::Space)), 1);
    }

    #[test]
    fn wheel_offsets_stay_inside_the_scrollable_range() {
        assert_eq!(bounded_scroll_offset(0.0, -28.0, 200.0), (28.0, true));
        assert_eq!(bounded_scroll_offset(28.0, 28.0, 200.0), (0.0, true));
        assert_eq!(bounded_scroll_offset(0.0, -500.0, 200.0), (200.0, true));
        assert_eq!(bounded_scroll_offset(200.0, -28.0, 200.0), (200.0, false));
        assert_eq!(bounded_scroll_offset(0.0, -28.0, 0.0), (0.0, false));
    }

    fn catalog_entry(slug: &str) -> CoreCatalogEntry {
        CoreCatalogEntry {
            slug: slug.to_string(),
            archive_name: format!("{slug}_libretro.dylib.zip"),
            display_name: slug.to_string(),
            inferred_system: None,
            installed_path: None,
            status: CoreInstallStatus::Available,
            download_url: format!("https://example.test/{slug}.zip"),
        }
    }

    fn empty_model() -> FrontendModel {
        let data = tempfile::tempdir().unwrap();
        FrontendModel::new(None, RetroFeelConfig::with_data_base(data.path()))
    }

    #[test]
    fn preferred_core_is_selected_for_a_library_rom() {
        let mut model = empty_model();
        model.core_catalog = vec![catalog_entry("snes9x"), catalog_entry("bsnes")];

        let selected = preferred_core_entry_for_rom(&model, Path::new("game.sfc")).unwrap();

        assert_eq!(selected.slug, "bsnes");
    }

    #[test]
    fn repeated_core_requests_are_deduplicated_and_keep_auto_launch() {
        let mut model = empty_model();
        let mut jobs = CoreCatalogJobs {
            busy: true,
            ..default()
        };
        let entry = catalog_entry("fceumm");
        queue_core_install(&mut model, &mut jobs, entry.clone(), None);
        queue_core_install(
            &mut model,
            &mut jobs,
            entry,
            Some(PathBuf::from("game.nes")),
        );

        assert_eq!(jobs.pending_installs.len(), 1);
        assert_eq!(
            jobs.pending_installs[0].auto_launch_rom.as_deref(),
            Some(Path::new("game.nes"))
        );
    }
}
