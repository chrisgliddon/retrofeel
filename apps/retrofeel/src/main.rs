//! `retrofeel` - Bevy shell and phase-3 debug CLI for libretro cores.
//!
//! Existing launch path:
//!   retrofeel --core <path> [--rom <path>] [--system <dir>]
//!
//! Config-backed launch:
//!   retrofeel --rom <path>
//!
//! Debug backend commands:
//!   retrofeel config init
//!   retrofeel config doctor
//!   retrofeel config resolve --rom <path>
//!   retrofeel config options --core <path>

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bevy::log::LogPlugin;
use bevy::prelude::*;
use clap::{Args, Parser, Subcommand, ValueEnum};
use libretro_host::Core;
use retrofeel_backend::{parse_core_variable, scan_system_dir, BiosStatus, CoreRegistry};
use retrofeel_types::{RetroFeelConfig, SteamGameEntry, SteamGameSource};

use plugin::{RetrofeelArgs, RetrofeelPlugin};

mod audio;
mod boxart;
mod bundled_cores;
mod core_thread;
mod db;
mod diagnostics;
mod feel_viewer;
mod frame_schedule;
mod icons;
mod input;
mod macos_documents;
mod mic;
mod overlay;
mod plugin;
mod recording;
mod screen;
mod steam_thread;
mod theme;
mod transcription;
mod ui;
mod worker_process;

#[derive(Parser, Debug)]
#[command(
    name = "retrofeel",
    version,
    about = "RetroFeel — play libretro cores in Bevy"
)]
struct Cli {
    /// Enable more verbose retrofeel diagnostics when RUST_LOG is not set.
    #[arg(long, global = true)]
    verbose: bool,

    /// Write diagnostics to this file instead of the platform cache location.
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    play: PlayArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch a ROM/core in the Bevy shell.
    Play(PlayArgs),
    /// Auto-detect a running Steam/Wine game and start recording.
    Steam(SteamArgs),
    /// Attach to the currently running GameHub game and start recording.
    Gamehub(GameHubArgs),
    /// Configuration backend debug commands.
    Config(ConfigArgs),
    /// Export a recorded session to an engine-specific input trace.
    Export(ExportArgs),
    /// Create a portable `.feel` package from an existing recording session.
    Package(FeelPackageArgs),
    /// Validate the paths, sizes, and hashes in a `.feel` package.
    Validate(FeelValidateArgs),
    /// Import and align an external SRT inside a `.feel` package.
    Align(FeelAlignArgs),
    /// Analyze a `.feel` package with an installed agent CLI.
    Analyze(FeelAnalyzeArgs),
    /// List installed analysis agent adapters.
    Agents,
    /// Open a `.feel` package in RetroFeel's recording evidence view.
    Open(FeelOpenArgs),
    /// Internal crash-isolated libretro worker. Not a user-facing command.
    #[command(name = "__core-worker", hide = true)]
    CoreWorker,
    /// Internal end-to-end worker launch probe used by smoke tests.
    #[command(name = "__worker-smoke", hide = true)]
    WorkerSmoke {
        #[arg(long)]
        core: PathBuf,
        #[arg(long)]
        rom: Option<PathBuf>,
        #[arg(long, default_value = "system")]
        system: PathBuf,
    },
}

#[derive(Args, Clone, Debug, Default)]
struct PlayArgs {
    /// Path to the libretro core (.so/.dll/.dylib).
    #[arg(long)]
    core: Option<PathBuf>,

    /// Path to a ROM to load (optional for no-content cores).
    #[arg(long)]
    rom: Option<PathBuf>,

    /// System directory (BIOS etc.). Defaults to config, then `system`.
    #[arg(long)]
    system: Option<PathBuf>,

    /// RON config path. Defaults to the platform config location.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Launch without loading or creating a config file.
    #[arg(long)]
    no_config: bool,

    /// Debug: start recording on launch and stop after this many core frames.
    #[arg(long)]
    record_frames: Option<u64>,

    /// Debug: pause recording/emulation when this captured frame index is reached.
    #[arg(long, requires = "record_frames")]
    record_pause_at: Option<u64>,

    /// Debug: duration of the scheduled recording pause in milliseconds.
    #[arg(long, default_value_t = 500, requires = "record_pause_at")]
    record_pause_ms: u64,
}

#[derive(Args, Debug)]
struct ConfigArgs {
    /// RON config path. Defaults to the platform config location.
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Args, Debug)]
struct ExportArgs {
    /// Engine export target.
    #[arg(long, value_enum)]
    engine: ExportEngine,

    /// Recording session directory containing manifest.json and input.json.
    #[arg(long)]
    session: PathBuf,

    /// Output directory. Defaults to <session>/exports.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct FeelPackageArgs {
    /// Existing RetroFeel recording directory.
    #[arg(long)]
    session: PathBuf,
    /// Destination directory ending in `.feel`.
    #[arg(long)]
    out: PathBuf,
    /// Optional original SRT to preserve and align into the package.
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Optional whisper.cpp JSON used to derive automatic word anchors.
    #[arg(long, requires = "transcript")]
    whisper_json: Option<PathBuf>,
    /// Manual source-to-video scale when no anchor JSON is supplied.
    #[arg(long, default_value_t = 1.0, requires = "transcript")]
    scale: f64,
    /// Manual source-to-video offset in seconds.
    #[arg(long, default_value_t = 0.0, requires = "transcript")]
    offset: f64,
    /// Portable Markdown analysis brief. A generic playtest brief is generated by default.
    #[arg(long)]
    context: Option<PathBuf>,
    /// Override the title inferred from the session manifest.
    #[arg(long)]
    title: Option<String>,
}

#[derive(Args, Debug)]
struct FeelValidateArgs {
    /// `.feel` package to validate.
    package: PathBuf,
}

#[derive(Args, Debug)]
struct FeelAlignArgs {
    /// Writable `.feel` package.
    package: PathBuf,
    /// Original SRT to preserve and align.
    #[arg(long)]
    transcript: PathBuf,
    /// Optional whisper.cpp JSON used to derive automatic word anchors.
    #[arg(long)]
    whisper_json: Option<PathBuf>,
    /// Manual source-to-video scale when no anchor JSON is supplied.
    #[arg(long, default_value_t = 1.0)]
    scale: f64,
    /// Manual source-to-video offset in seconds.
    #[arg(long, default_value_t = 0.0)]
    offset: f64,
    /// Description retained as transcript provenance.
    #[arg(long, default_value = "External SRT")]
    source: String,
}

#[derive(Args, Debug)]
struct FeelAnalyzeArgs {
    /// Writable `.feel` package.
    package: PathBuf,
    /// Installed agent CLI adapter.
    #[arg(long, value_enum, default_value_t = FeelAgent::Codex)]
    agent: FeelAgent,
    /// Optional model override passed to the selected CLI.
    #[arg(long)]
    model: Option<String>,
    /// Representative video frames supplied to the agent (maximum 12).
    #[arg(long, default_value_t = 12)]
    max_frames: usize,
}

#[derive(Args, Debug)]
struct FeelOpenArgs {
    /// `.feel` directory package to open.
    package: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum FeelAgent {
    #[default]
    Codex,
    Claude,
    OpenCode,
    Kimi,
}

impl From<FeelAgent> for retrofeel_feel::AgentAdapterKind {
    fn from(value: FeelAgent) -> Self {
        match value {
            FeelAgent::Codex => Self::Codex,
            FeelAgent::Claude => Self::Claude,
            FeelAgent::OpenCode => Self::OpenCode,
            FeelAgent::Kimi => Self::Kimi,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExportEngine {
    Bevy,
    Unity,
    Godot,
    Unreal,
}

impl From<ExportEngine> for retrofeel_export::Engine {
    fn from(value: ExportEngine) -> Self {
        match value {
            ExportEngine::Bevy => retrofeel_export::Engine::Bevy,
            ExportEngine::Unity => retrofeel_export::Engine::Unity,
            ExportEngine::Godot => retrofeel_export::Engine::Godot,
            ExportEngine::Unreal => retrofeel_export::Engine::Unreal,
        }
    }
}

#[derive(Args, Clone, Debug)]
struct SteamArgs {
    /// Steam app ID. Omit it to auto-detect the running Steam/Wine game.
    #[arg(long)]
    appid: Option<u32>,

    /// Display name for the game (used in manifest + SCK window title matching).
    #[arg(long)]
    name: Option<String>,

    /// Install directory containing the game .exe
    /// (e.g. ~/GameHub/steamapps/common/Meadow of Lanterns).
    #[arg(long)]
    install_dir: Option<PathBuf>,

    /// The .exe filename to launch (e.g. MeadowOfLanterns.exe).
    /// Required when not using --pid.
    #[arg(long)]
    exe: Option<String>,

    /// Wine prefix directory (e.g. ~/FoM-wine). Defaults to config.
    #[arg(long)]
    wine_prefix: Option<PathBuf>,

    /// Path to the wine binary. Defaults to /opt/homebrew/bin/wine.
    #[arg(long)]
    wine_bin: Option<PathBuf>,

    /// Attach to an already-running game process by PID instead of launching
    /// Wine. Use this when the game was launched via GameHub or Steam directly.
    /// When set, --install-dir, --exe, --wine-prefix, and --wine-bin are not
    /// needed for launch (but --install-dir is still used for the manifest).
    #[arg(long)]
    pid: Option<u32>,

    /// Advanced: attach by scanning for a Wine process whose command line
    /// contains this string (defaults to --exe, then --name).
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    attach: Option<String>,

    /// RON config path. Defaults to the platform SQLite-backed configuration.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Debug: start recording on launch and stop after this many captured frames.
    #[arg(long, conflicts_with = "record")]
    record_frames: Option<u64>,

    /// Start recording immediately and continue until stopped from RetroFeel.
    #[arg(long)]
    record: bool,
}

#[derive(Args, Clone, Debug, Default)]
struct GameHubArgs {
    /// Select one running GameHub game when more than one is active.
    #[arg(long)]
    appid: Option<u32>,

    /// RON config path. Defaults to the platform SQLite-backed configuration.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Attach and show the overlay without starting a recording.
    #[arg(long)]
    no_record: bool,

    /// Debug: record exactly this many CFR frames instead of recording until stopped.
    #[arg(long, conflicts_with = "no_record")]
    record_frames: Option<u64>,
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Create a default config file and configured directories.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Print configured paths, discovered cores, and BIOS status.
    Doctor,
    /// Resolve a ROM to a configured/discovered core.
    Resolve {
        /// ROM path to resolve by extension.
        #[arg(long)]
        rom: PathBuf,

        /// Explicit core override for comparison/debugging.
        #[arg(long)]
        core: Option<PathBuf>,
    },
    /// Enumerate a core's declared options and configured overrides.
    Options {
        /// Path to the libretro core.
        #[arg(long)]
        core: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let logging = match diagnostics::init_logging(diagnostics::LoggingOptions {
        verbose: cli.verbose,
        log_file: cli.log_file.clone(),
    }) {
        Ok(logging) => logging,
        Err(error) => {
            eprintln!("retrofeel: ERROR: failed to initialize logging: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = run_main(cli, logging.log_path) {
        eprintln!("retrofeel: ERROR: {error:#}");
        std::process::exit(1);
    }
}

fn run_main(cli: Cli, log_path: PathBuf) -> Result<()> {
    match cli.command {
        Some(Command::Config(args)) => run_config_command(args),
        Some(Command::Play(args)) => launch(args, log_path, None),
        Some(Command::Steam(args)) => launch_steam(args, log_path),
        Some(Command::Gamehub(args)) => launch_gamehub(args, log_path),
        Some(Command::Export(args)) => run_export(args),
        Some(Command::Package(args)) => run_feel_package(args),
        Some(Command::Validate(args)) => run_feel_validate(args),
        Some(Command::Align(args)) => run_feel_align(args),
        Some(Command::Analyze(args)) => run_feel_analyze(args),
        Some(Command::Agents) => run_feel_agents(),
        Some(Command::Open(args)) => {
            retrofeel_feel::FeelPackage::open(&args.package)
                .with_context(|| format!("opening {}", args.package.display()))?;
            launch(PlayArgs::default(), log_path, Some(args.package))
        }
        Some(Command::CoreWorker) => worker_process::run_worker(),
        Some(Command::WorkerSmoke { core, rom, system }) => {
            let audio = retrofeel_types::AudioConfig {
                enabled: false,
                ..Default::default()
            };
            let (handle, _) = core_thread::spawn(
                core,
                rom,
                system.to_string_lossy().into_owned(),
                audio,
                None,
            )?;
            println!(
                "worker ready: {}x{} @ {:.3} fps",
                handle.base_width, handle.base_height, handle.fps
            );
            drop(handle);
            Ok(())
        }
        None => launch(cli.play, log_path, None),
    }
}

fn launch(args: PlayArgs, log_path: PathBuf, initial_recording: Option<PathBuf>) -> Result<()> {
    preflight_bevy_runtime()?;
    let (mut config, config_path) = load_play_config(&args)?;
    if let Some(system_dir) = args.system.clone() {
        config.paths.system = system_dir;
    }
    let core = if args.core.is_some() {
        args.core.clone()
    } else if args.rom.is_some() {
        match resolve_core_path(&args, Some(&config), &config.paths.system) {
            Ok(core) => Some(core),
            Err(error) => {
                log::warn!("could not resolve initial ROM from config: {error:#}");
                None
            }
        }
    } else {
        None
    };

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "RetroFeel".into(),
                        resolution: (960u32, 720u32).into(),
                        ..default()
                    }),
                    ..default()
                })
                .disable::<LogPlugin>(),
        )
        .insert_resource(RetrofeelArgs {
            core,
            rom: args.rom,
            config_path,
            config_file_override: args.config.is_some(),
            config,
            auto_record: false,
            auto_record_frames: args.record_frames,
            auto_record_pause_at: args.record_pause_at,
            auto_record_pause_ms: args.record_pause_ms,
            log_path,
            steam_target: None,
            initial_recording,
        })
        .add_plugins(RetrofeelPlugin)
        .add_plugins(db::DbPlugin)
        .run();

    Ok(())
}

fn steam_capture_primary_window() -> Window {
    Window {
        title: "RetroFeel".into(),
        resolution: (960u32, 720u32).into(),
        visible: false,
        focused: false,
        ..default()
    }
}

fn launch_steam(args: SteamArgs, log_path: PathBuf) -> Result<()> {
    let (config, config_path) = load_play_config(&PlayArgs {
        config: args.config.clone(),
        ..Default::default()
    })?;

    let has_advanced_selector = args.name.is_some()
        || args.install_dir.is_some()
        || args.exe.is_some()
        || args.wine_prefix.is_some()
        || args.wine_bin.is_some()
        || args.pid.is_some()
        || args.attach.is_some();

    // The everyday path discovers a running attach-compatible Steam/Wine
    // game and records immediately. --appid only disambiguates; it never
    // makes the user supply the name, executable, or install directory too.
    if !has_advanced_selector {
        let games = retrofeel_backend::discover_installed_steam_games(&config.steam)
            .into_iter()
            .filter(|game| game.source != SteamGameSource::NativeSteam && !game.exe_name.is_empty())
            .collect();
        let (game, pid) = select_running_game(
            games,
            args.appid,
            retrofeel_steamcapture::find_wine_pid,
            "Steam/Wine",
        )?;
        let target = attached_steam_target(&config, game, pid);
        return run_steam_app(
            config,
            config_path,
            target,
            true,
            args.record_frames,
            log_path,
        );
    }

    let app_id = args.appid.context(
        "--appid is required with explicit Steam launch/attach options; omit all options to auto-detect",
    )?;

    let name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("Steam App {app_id}"));

    // Attach to an existing process (--pid), discover one (--attach), or
    // launch Wine ourselves.
    let attach_pid = match (args.pid, args.attach.as_deref()) {
        (Some(pid), _) => Some(pid),
        (None, Some(hint)) => {
            let needle = if hint.is_empty() {
                args.exe.clone().unwrap_or_else(|| name.clone())
            } else {
                hint.to_string()
            };
            let pid = retrofeel_steamcapture::find_wine_pid(&needle).with_context(|| {
                format!(
                    "--attach: no running process matches '{needle}' — launch the game \
                     (e.g. from GameHub) first, or pass --pid explicitly"
                )
            })?;
            log::info!("retrofeel steam: --attach found PID {pid} matching '{needle}'");
            Some(pid)
        }
        (None, None) => None,
    };

    let install_dir = args
        .install_dir
        .clone()
        .or_else(|| {
            // Try to infer from GameHub steamapps if not provided.
            let common = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join("GameHub/steamapps/common");
            // If a dir matching the name exists, use it.
            let candidate = common.join(&name);
            if candidate.is_dir() {
                Some(candidate)
            } else {
                None
            }
        })
        .unwrap_or_default();

    let target = if let Some(pid) = attach_pid {
        // Attach mode: the game is already running (e.g. launched by GameHub).
        log::info!(
            "retrofeel steam: attaching to running process PID={pid} for '{}' (appid={})",
            name,
            app_id
        );
        crate::steam_thread::SteamTarget {
            app_id,
            name: name.clone(),
            install_dir,
            wine_prefix: args.wine_prefix.clone().unwrap_or_default(),
            wine_bin: args
                .wine_bin
                .clone()
                .unwrap_or_else(|| config.steam.wine_bin.clone()),
            exe_name: args.exe.clone().unwrap_or_default(),
            capture_max_width: config.steam.capture_max_width,
            capture_max_height: config.steam.capture_max_height,
            capture_fps: config.steam.capture_fps,
            capture_mic: config.recording.mic_enabled,
            transcription: config.recording.transcription.clone(),
            attach_pid: Some(pid),
            native_steam_launch: false,
            native_attach_timeout_seconds: config.steam.native_attach_timeout_seconds,
        }
    } else {
        // Launch mode: start Wine ourselves.
        let wine_bin = args
            .wine_bin
            .clone()
            .unwrap_or_else(|| config.steam.wine_bin.clone());
        let wine_prefix = args
            .wine_prefix
            .clone()
            .or_else(|| config.steam.default_wine_prefix.clone())
            .context("no Wine prefix specified (use --wine-prefix or set steam.default_wine_prefix in config)")?;
        let exe = args
            .exe
            .clone()
            .context("--exe is required when not using --pid")?;

        log::info!(
            "retrofeel steam: launching '{}' (appid={}) via wine {}",
            name,
            app_id,
            wine_prefix.display()
        );
        crate::steam_thread::SteamTarget {
            app_id,
            name: name.clone(),
            install_dir,
            wine_prefix,
            wine_bin,
            exe_name: exe,
            capture_max_width: config.steam.capture_max_width,
            capture_max_height: config.steam.capture_max_height,
            capture_fps: config.steam.capture_fps,
            capture_mic: config.recording.mic_enabled,
            transcription: config.recording.transcription.clone(),
            attach_pid: None,
            native_steam_launch: false,
            native_attach_timeout_seconds: config.steam.native_attach_timeout_seconds,
        }
    };

    run_steam_app(
        config,
        config_path,
        target,
        args.record,
        args.record_frames,
        log_path,
    )
}

fn launch_gamehub(args: GameHubArgs, log_path: PathBuf) -> Result<()> {
    let (config, config_path) = load_play_config(&PlayArgs {
        config: args.config.clone(),
        ..Default::default()
    })?;
    let games = retrofeel_backend::scan_steamapps(&config.steam.gamehub_steamapps);
    let (game, pid) = select_running_game(
        games,
        args.appid,
        retrofeel_steamcapture::find_wine_pid,
        "GameHub",
    )?;
    let target = attached_steam_target(&config, game, pid);
    run_steam_app(
        config,
        config_path,
        target,
        !args.no_record,
        args.record_frames,
        log_path,
    )
}

fn select_running_game(
    games: Vec<SteamGameEntry>,
    requested_app_id: Option<u32>,
    mut find_pid: impl FnMut(&str) -> Option<u32>,
    source_label: &str,
) -> Result<(SteamGameEntry, u32)> {
    let installed = games
        .into_iter()
        .filter(|game| requested_app_id.is_none_or(|app_id| game.app_id == app_id))
        .collect::<Vec<_>>();
    if installed.is_empty() {
        if let Some(app_id) = requested_app_id {
            anyhow::bail!("Steam app {app_id} is not installed through {source_label}");
        }
        anyhow::bail!("no {source_label} Steam games were discovered");
    }

    let mut running = installed
        .iter()
        .filter_map(|game| find_pid(&game.exe_name).map(|pid| (game.clone(), pid)))
        .collect::<Vec<_>>();
    match running.len() {
        0 => {
            if installed.len() == 1 {
                anyhow::bail!(
                    "{} is installed but not running; launch it through {source_label}, then retry",
                    installed[0].name
                );
            }
            anyhow::bail!(
                "no running {source_label} game found; launch the game first, then retry"
            );
        }
        1 => Ok(running.remove(0)),
        _ => {
            let choices = running
                .iter()
                .map(|(game, _)| format!("{} ({})", game.name, game.app_id))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "multiple {source_label} games are running: {choices}; retry with --appid <ID>"
            );
        }
    }
}

fn attached_steam_target(
    config: &RetroFeelConfig,
    game: SteamGameEntry,
    pid: u32,
) -> crate::steam_thread::SteamTarget {
    log::info!(
        "retrofeel: auto-attaching to '{}' (appid={}, pid={pid})",
        game.name,
        game.app_id
    );
    crate::steam_thread::SteamTarget {
        app_id: game.app_id,
        name: game.name,
        install_dir: game.install_dir,
        wine_prefix: game.wine_prefix,
        wine_bin: config.steam.wine_bin.clone(),
        exe_name: game.exe_name,
        capture_max_width: config.steam.capture_max_width,
        capture_max_height: config.steam.capture_max_height,
        capture_fps: config.steam.capture_fps,
        capture_mic: config.recording.mic_enabled,
        transcription: config.recording.transcription.clone(),
        attach_pid: Some(pid),
        native_steam_launch: false,
        native_attach_timeout_seconds: config.steam.native_attach_timeout_seconds,
    }
}

fn run_steam_app(
    config: RetroFeelConfig,
    config_path: Option<PathBuf>,
    target: crate::steam_thread::SteamTarget,
    auto_record: bool,
    auto_record_frames: Option<u64>,
    log_path: PathBuf,
) -> Result<()> {
    let config_file_override = config_path.is_some();
    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(steam_capture_primary_window()),
                    ..default()
                })
                // Steam/GameHub capture samples Apple's shared GameController
                // state into input.json. Bevy's gilrs plugin would open a raw
                // IOHIDManager reader that has no consumer in capture mode and
                // can disrupt the controller already owned by GameHub/Wine.
                .disable::<bevy::gilrs::GilrsPlugin>()
                .disable::<LogPlugin>(),
        )
        .insert_resource(RetrofeelArgs {
            core: None,
            rom: None,
            config_path,
            config_file_override,
            config,
            auto_record,
            auto_record_frames,
            auto_record_pause_at: None,
            auto_record_pause_ms: 500,
            log_path,
            steam_target: Some(target),
            initial_recording: None,
        })
        .add_plugins(RetrofeelPlugin)
        .add_plugins(db::DbPlugin)
        .run();

    Ok(())
}

#[cfg(target_os = "linux")]
fn preflight_bevy_runtime() -> Result<()> {
    let has_x11_display = std::env::var_os("DISPLAY")
        .as_deref()
        .is_some_and(|display| !display.is_empty());
    if !has_x11_display {
        return Ok(());
    }

    unsafe { libloading::Library::new("libxkbcommon-x11.so.0") }.map_err(|error| {
        anyhow::anyhow!(
            "Linux/X11 runtime dependency missing: libxkbcommon-x11.so.0 could not be loaded ({error}). Install it with: sudo apt-get install libxkbcommon-x11-0"
        )
    })?;

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn preflight_bevy_runtime() -> Result<()> {
    Ok(())
}

fn load_play_config(args: &PlayArgs) -> Result<(RetroFeelConfig, Option<PathBuf>)> {
    if args.no_config {
        return Ok((RetroFeelConfig::default(), None));
    }

    if let Some(path) = &args.config {
        let config = RetroFeelConfig::load_or_create(path)
            .with_context(|| format!("loading config override {}", path.display()))?;
        if let Err(error) = config.ensure_dirs() {
            log::warn!("failed to create configured directories: {error}");
        }
        return Ok((config, Some(path.clone())));
    }

    // The normal app/CLI launch intentionally does not create or re-read a
    // RON file. Read the same SQLite configuration that DbPlugin will expose
    // to the GUI so command-line capture honors custom paths and settings.
    let mut config = RetroFeelConfig::platform_default()
        .context("building platform SQLite-backed default config")?;
    let db_path = db::db_path_for(&config);
    if db_path.is_file() {
        let database = retrofeel_db::Db::open(&db_path)
            .with_context(|| format!("opening configuration database {}", db_path.display()))?;
        let repo = retrofeel_db::ConfigRepo::new(&database);
        if repo.config_imported()? {
            config = repo
                .load_config()
                .context("loading the SQLite-backed RetroFeel configuration")?;
        }
    }
    if let Err(error) = config.ensure_dirs() {
        log::warn!("failed to create configured directories: {error}");
    }
    Ok((config, None))
}

fn resolve_core_path(
    args: &PlayArgs,
    config: Option<&RetroFeelConfig>,
    system_dir: &Path,
) -> Result<PathBuf> {
    if let Some(core) = &args.core {
        return Ok(core.clone());
    }

    let rom = args
        .rom
        .as_ref()
        .context("--core is required unless --rom can be resolved from config")?;
    let config = config.context("config is required to resolve --rom without --core")?;

    if let Some(core) = config.core_override_for_rom(rom) {
        return Ok(core.clone());
    }

    let report = CoreRegistry::scan_dir(&config.paths.cores, system_dir)
        .with_context(|| format!("scanning cores in {}", config.paths.cores.display()))?;
    if !report.failures.is_empty() {
        for failure in &report.failures {
            log::warn!(
                "core scan failed for {}: {}",
                failure.path.display(),
                failure.error
            );
        }
    }

    report
        .registry
        .resolve_rom(rom, Some(config), None)
        .map(|core| core.path.clone())
        .with_context(|| format!("no discovered core supports ROM {}", rom.display()))
}

fn run_config_command(args: ConfigArgs) -> Result<()> {
    let config_path = config_path(args.config)?;
    match args.action {
        ConfigAction::Init { force } => init_config(&config_path, force),
        ConfigAction::Doctor => doctor_config(&config_path),
        ConfigAction::Resolve { rom, core } => resolve_config(&config_path, &rom, core.as_deref()),
        ConfigAction::Options { core } => options_config(&config_path, &core),
    }
}

fn run_export(args: ExportArgs) -> Result<()> {
    let out = args.out.unwrap_or_else(|| args.session.join("exports"));
    let exported = diagnostics::time_block("recording.export.cli", || {
        retrofeel_export::export_session(args.engine.into(), &args.session, &out)
    })
    .with_context(|| format!("exporting session {}", args.session.display()))?;
    println!("exported: {}", exported.path.display());
    Ok(())
}

fn run_feel_package(args: FeelPackageArgs) -> Result<()> {
    let options = retrofeel_feel::PackageOptions {
        title: args.title,
        context_brief: args.context,
    };
    let package =
        retrofeel_feel::create_package(&args.session, &args.out, &options).with_context(|| {
            format!(
                "packaging recording {} as {}",
                args.session.display(),
                args.out.display()
            )
        })?;
    if let Some(transcript) = args.transcript {
        align_transcript_into_package(
            package.root(),
            &transcript,
            args.whisper_json.as_deref(),
            args.scale,
            args.offset,
            Some(format!(
                "External SRT: {}",
                transcript
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("transcript.srt")
            )),
        )?;
    }
    let package = retrofeel_feel::FeelPackage::open(&args.out)?;
    let report = package.validate();
    if !report.valid {
        anyhow::bail!(
            "new package failed validation: {}",
            report.errors.join("; ")
        );
    }
    println!("package: {}", package.root().display());
    println!("resources verified: {}", report.checked_resources);
    Ok(())
}

fn run_feel_validate(args: FeelValidateArgs) -> Result<()> {
    let package = retrofeel_feel::FeelPackage::open(&args.package)
        .with_context(|| format!("opening {}", args.package.display()))?;
    let report = package.validate();
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.valid {
        anyhow::bail!("package validation failed");
    }
    Ok(())
}

fn run_feel_align(args: FeelAlignArgs) -> Result<()> {
    let asset = align_transcript_into_package(
        &args.package,
        &args.transcript,
        args.whisper_json.as_deref(),
        args.scale,
        args.offset,
        Some(args.source),
    )?;
    println!("aligned transcript: {}", asset.srt_path);
    println!(
        "alignment metadata: {}",
        asset.alignment_path.as_deref().unwrap_or("unavailable")
    );
    Ok(())
}

fn align_transcript_into_package(
    package_path: &Path,
    transcript_path: &Path,
    whisper_json: Option<&Path>,
    scale: f64,
    offset: f64,
    source: Option<String>,
) -> Result<retrofeel_feel::TranscriptAsset> {
    let package = retrofeel_feel::FeelPackage::open(package_path)?;
    let source_text = std::fs::read_to_string(transcript_path)
        .with_context(|| format!("reading {}", transcript_path.display()))?;
    let segments = retrofeel_feel::parse_srt(&source_text).map_err(anyhow::Error::msg)?;
    let anchors = if let Some(path) = whisper_json {
        let value: serde_json::Value = serde_json::from_reader(
            std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        retrofeel_feel::anchors_from_whisper_json(&segments, &value).map_err(anyhow::Error::msg)?
    } else {
        Vec::new()
    };
    let session = package.session_manifest()?;
    let duration = if session.timing.fps > 0.0 {
        Some(session.frame_count as f64 / session.timing.fps)
    } else {
        None
    };
    let outcome = retrofeel_feel::align_srt(
        &segments,
        &anchors,
        &retrofeel_feel::AlignmentOptions {
            manual_scale: scale,
            manual_offset_seconds: offset,
            video_duration_seconds: duration,
            source_audio: "video.mkv:a:0".into(),
            tool: if whisper_json.is_some() {
                "RetroFeel monotonic whisper word alignment".into()
            } else {
                "RetroFeel manual affine alignment".into()
            },
            model: whisper_json.map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("whisper.json")
                    .to_string()
            }),
        },
    )
    .map_err(anyhow::Error::msg)?;
    println!(
        "alignment: scale={:.9}, offset={:+.3}s, anchors={}, quality={:?}, p95={}",
        outcome.report.scale,
        outcome.report.offset_seconds,
        outcome.report.anchor_count,
        outcome.report.quality,
        outcome
            .report
            .p95_absolute_residual_seconds
            .map(|value| format!("{value:.3}s"))
            .unwrap_or_else(|| "n/a".into())
    );
    retrofeel_feel::import_aligned_transcript(package_path, transcript_path, &outcome, source)
        .map_err(Into::into)
}

fn run_feel_analyze(args: FeelAnalyzeArgs) -> Result<()> {
    if !(1..=12).contains(&args.max_frames) {
        anyhow::bail!("--max-frames must be between 1 and 12");
    }
    let options = retrofeel_feel::AnalyzeOptions {
        adapter: args.agent.into(),
        model: args.model,
        max_frames: args.max_frames,
    };
    let result = retrofeel_feel::analyze_package(&args.package, &options)
        .with_context(|| format!("analyzing {}", args.package.display()))?;
    let package = retrofeel_feel::FeelPackage::open(&args.package)?;
    let run = package
        .manifest()
        .analyses
        .last()
        .context("analysis completed without a package run entry")?;
    println!("analysis summary: {}", result.summary);
    println!(
        "report: {}",
        package.root().join(&run.report_path).display()
    );
    println!(
        "action plan: {}",
        package.root().join(&run.action_plan_path).display()
    );
    Ok(())
}

fn run_feel_agents() -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&retrofeel_feel::discover_agents())?
    );
    Ok(())
}

fn config_path(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => RetroFeelConfig::platform_config_path().context("resolving platform config path"),
    }
}

fn init_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "config already exists at {} (use --force to overwrite)",
            path.display()
        );
    }

    let config = RetroFeelConfig::platform_default().context("building platform default config")?;
    config
        .save_to_path(path)
        .with_context(|| format!("writing config {}", path.display()))?;
    config
        .ensure_dirs()
        .context("creating configured directories")?;

    println!("config written: {}", path.display());
    println!("cores: {}", config.paths.cores.display());
    println!("system: {}", config.paths.system.display());
    println!("roms: {}", join_paths(&config.paths.roms));
    println!("saves: {}", config.paths.saves.display());
    println!("states: {}", config.paths.states.display());
    println!("recordings: {}", config.paths.recordings.display());
    Ok(())
}

fn doctor_config(path: &Path) -> Result<()> {
    let config = RetroFeelConfig::load_or_create(path)
        .with_context(|| format!("loading config {}", path.display()))?;
    config
        .ensure_dirs()
        .context("creating configured directories")?;

    println!("config: {}", path.display());
    println!("cores dir: {}", config.paths.cores.display());
    println!("system dir: {}", config.paths.system.display());
    println!("rom dirs: {}", join_paths(&config.paths.roms));

    let report = CoreRegistry::scan_dir(&config.paths.cores, &config.paths.system)
        .with_context(|| format!("scanning cores in {}", config.paths.cores.display()))?;
    println!("cores: {} discovered", report.registry.cores.len());
    for core in &report.registry.cores {
        println!(
            "  {} v{} [{}] {}",
            core.library_name,
            core.library_version,
            core.valid_extensions.join("|"),
            core.path.display()
        );
    }
    for failure in &report.failures {
        println!(
            "  scan failure: {} ({})",
            failure.path.display(),
            failure.error
        );
    }

    let bios = scan_system_dir(&config.paths.system).context("scanning BIOS files")?;
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
    let missing = bios
        .checks
        .iter()
        .filter(|check| matches!(check.status, BiosStatus::Missing))
        .count();
    let bad = bios
        .checks
        .iter()
        .filter(|check| matches!(check.status, BiosStatus::BadHash { .. }))
        .count();
    println!("bios: {present} present, {missing} missing, {bad} bad hash");
    for check in bios.missing_required().take(12) {
        println!(
            "  required BIOS not valid: {} / {} ({})",
            check.entry.system, check.entry.name, check.entry.filename
        );
    }

    // Steam config summary.
    println!();
    println!("steam:");
    println!("  wine bin: {}", config.steam.wine_bin.display());
    println!("  launch mode: {:?}", config.steam.launch_mode);
    println!(
        "  capture: {}x{} @ {}fps",
        config.steam.capture_max_width, config.steam.capture_max_height, config.steam.capture_fps
    );
    println!("  games: {}", config.steam.games.len());
    for game in &config.steam.games {
        println!(
            "  app {} : {} — {}",
            game.app_id,
            game.name,
            game.install_dir.display()
        );
    }

    // Transcription setup summary. External models are intentionally never
    // inferred from another application's private cache.
    println!();
    println!("recording:");
    println!("  mic enabled: {}", config.recording.mic_enabled);
    match &config.recording.transcription.external_model {
        Some(path) if path.exists() => {
            println!("  whisper model: {} (found)", path.display());
        }
        Some(path) => {
            println!("  whisper model: {} (NOT FOUND)", path.display());
        }
        None => {
            println!("  transcription model: NOT SET");
        }
    }

    // macOS TCC permission check (informational — can't query TCC programmatically).
    #[cfg(target_os = "macos")]
    {
        println!();
        println!("macOS permissions (Steam capture):");
        println!(
            "  Screen Recording: grant in System Settings → Privacy & Security → Screen Recording"
        );
        println!("  Accessibility: grant in System Settings → Privacy & Security → Accessibility");
        println!(
            "  Input Monitoring: grant in System Settings → Privacy & Security → Input Monitoring"
        );
        println!("  Microphone: grant in System Settings → Privacy & Security → Microphone");
        println!("  (When running via cargo run, grant these to your Terminal app.)");
    }

    Ok(())
}

fn resolve_config(path: &Path, rom: &Path, explicit_core: Option<&Path>) -> Result<()> {
    let config = RetroFeelConfig::load_or_create(path)
        .with_context(|| format!("loading config {}", path.display()))?;
    if let Some(core) = explicit_core {
        println!("resolved core: {}", core.display());
        println!("source: explicit --core");
        return Ok(());
    }
    if let Some(core) = config.core_override_for_rom(rom) {
        println!("resolved core: {}", core.display());
        println!("source: config extension override");
        return Ok(());
    }

    let report = CoreRegistry::scan_dir(&config.paths.cores, &config.paths.system)
        .with_context(|| format!("scanning cores in {}", config.paths.cores.display()))?;
    let core = report
        .registry
        .resolve_rom(rom, Some(&config), None)
        .with_context(|| format!("no discovered core supports ROM {}", rom.display()))?;

    println!("resolved core: {}", core.path.display());
    println!(
        "source: extension match ({})",
        core.valid_extensions.join("|")
    );
    Ok(())
}

fn options_config(path: &Path, core_path: &Path) -> Result<()> {
    let config = RetroFeelConfig::load_or_create(path)
        .with_context(|| format!("loading config {}", path.display()))?;

    // Use `probe_variables` (no `retro_init`) instead of `Core::load`. This
    // avoids a crash on cores that call C `exit()` from `retro_init` when
    // their BIOS is missing (e.g. FB Alpha / MAME). The trade-off: we can't
    // call `set_option` (which needs a live `Core` context), so the "apply"
    // step is skipped — the CLI just inspects + reports.
    let (info, variables) = diagnostics::time_block(
        format!(
            "config.options.probe_variables path={}",
            core_path.display()
        ),
        || Core::probe_variables(core_path, &config.paths.system.to_string_lossy()),
    )
    .with_context(|| format!("probing core {}", core_path.display()))?;

    println!("core: {} v{}", info.library_name, info.library_version);

    if variables.is_empty() {
        println!("options: none declared (or declared during retro_init)");
    } else {
        println!("options:");
        for variable in &variables {
            let parsed = parse_core_variable(variable);
            let default = parsed.default_value.as_deref().unwrap_or("<none>");
            println!(
                "  {}: {} (default: {}, values: {})",
                parsed.key,
                parsed.description,
                default,
                parsed.values.join("|")
            );
        }
    }

    let core_key = info.library_name;
    let overrides = config.core_options_for(&core_key);
    if overrides.is_empty() {
        println!("configured overrides: none");
    } else {
        println!("configured overrides:");
        for (key, value) in &overrides {
            println!("  {key}={value}");
        }
    }

    Ok(())
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(app_id: u32, name: &str, exe_name: &str) -> SteamGameEntry {
        SteamGameEntry {
            app_id,
            name: name.into(),
            install_dir: PathBuf::from(name),
            wine_prefix: PathBuf::new(),
            exe_name: exe_name.into(),
            source: SteamGameSource::GameHub,
        }
    }

    #[test]
    fn zero_argument_capture_commands_parse() {
        let gamehub = Cli::try_parse_from(["retrofeel", "gamehub"]).unwrap();
        assert!(matches!(
            gamehub.command,
            Some(Command::Gamehub(GameHubArgs {
                appid: None,
                no_record: false,
                ..
            }))
        ));

        let steam = Cli::try_parse_from(["retrofeel", "steam"]).unwrap();
        assert!(matches!(
            steam.command,
            Some(Command::Steam(SteamArgs { appid: None, .. }))
        ));
    }

    #[test]
    fn running_game_is_selected_without_memorized_metadata() {
        let games = vec![
            game(42, "Meadow of Lanterns", "MeadowOfLanterns.exe"),
            game(43, "ClockworkValley", "ClockworkValley.exe"),
        ];
        let (selected, pid) = select_running_game(
            games,
            None,
            |exe| (exe == "MeadowOfLanterns.exe").then_some(4242),
            "GameHub",
        )
        .unwrap();

        assert_eq!(selected.app_id, 42);
        assert_eq!(pid, 4242);
    }

    #[test]
    fn app_id_disambiguates_without_requiring_other_metadata() {
        let games = vec![
            game(42, "Meadow of Lanterns", "MeadowOfLanterns.exe"),
            game(43, "ClockworkValley", "ClockworkValley.exe"),
        ];
        let (selected, pid) =
            select_running_game(games, Some(43), |_| Some(4242), "GameHub").unwrap();

        assert_eq!(selected.app_id, 43);
        assert_eq!(pid, 4242);
    }

    #[test]
    fn multiple_running_games_require_only_an_app_id() {
        let games = vec![
            game(42, "Meadow of Lanterns", "MeadowOfLanterns.exe"),
            game(43, "ClockworkValley", "ClockworkValley.exe"),
        ];
        let error = select_running_game(games, None, |_| Some(42), "GameHub").unwrap_err();

        assert!(error.to_string().contains("--appid <ID>"));
        assert!(error.to_string().contains("42"));
        assert!(error.to_string().contains("43"));
    }

    #[test]
    fn steam_capture_primary_window_cannot_take_game_focus() {
        let window = steam_capture_primary_window();

        assert!(!window.visible);
        assert!(!window.focused);
    }
}
