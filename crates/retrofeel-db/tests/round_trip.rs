//! Round-trip tests for the retrofeel-db repos.

use retrofeel_backend::{
    BiosCheck, BiosReport, BiosStatus, CoreCatalogEntry, CoreInstallStatus, KnownBios,
};
use retrofeel_db::cores::CoreRow;
use retrofeel_db::recordings::RecordingRow;
use retrofeel_db::roms::RomRow;
use retrofeel_db::screenshots::ScreenshotRow;
use retrofeel_db::{
    BiosRepo, CatalogRepo, ConfigRepo, CoresRepo, Db, GamesRepo, RecordingsRepo, RomsRepo,
    ScreenshotsRepo, SteamGameRow, SteamRepo,
};
use retrofeel_types::{RetroFeelConfig, ThemePreference};
use std::path::PathBuf;

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

#[test]
fn cores_round_trip() {
    let db = db();
    let repo = CoresRepo::new(&db);
    let rows = vec![CoreRow {
        path: PathBuf::from("/cores/snes9x.dylib"),
        library_name: "Snes9x".into(),
        library_version: "1.0".into(),
        valid_extensions: vec!["sfc".into(), "smc".into()],
        need_fullpath: false,
        block_extract: false,
        source: "bundled".into(),
        bundled: true,
        file_size: 1_000_000,
        file_mtime: 1234567890,
        last_seen: 1234567890,
    }];
    repo.replace_all(&rows).unwrap();
    let loaded = repo.all().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].library_name, "Snes9x");
    assert_eq!(loaded[0].valid_extensions, vec!["sfc", "smc"]);
    assert!(loaded[0].bundled);

    let report = repo.registry().unwrap();
    assert_eq!(report.registry.cores.len(), 1);
    assert_eq!(report.registry.cores[0].library_name, "Snes9x");
}

#[test]
fn catalog_round_trip() {
    let db = db();
    let repo = CatalogRepo::new(&db);
    let entries = vec![
        CoreCatalogEntry {
            slug: "snes9x".into(),
            archive_name: "snes9x_libretro.dylib.zip".into(),
            display_name: "SNES (Snes9x)".into(),
            inferred_system: Some("Super Nintendo".into()),
            download_url: "https://example/snes9x.zip".into(),
            installed_path: None,
            status: CoreInstallStatus::Available,
        },
        CoreCatalogEntry {
            slug: "gambatte".into(),
            archive_name: "gambatte_libretro.dylib.zip".into(),
            display_name: "Game Boy (Gambatte)".into(),
            inferred_system: None,
            download_url: "https://example/gambatte.zip".into(),
            installed_path: Some(PathBuf::from("/cores/gambatte.dylib")),
            status: CoreInstallStatus::Installed,
        },
    ];
    repo.replace_all(&entries, 1000).unwrap();
    let loaded = repo.all().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].slug, "gambatte"); // sorted by display_name
    assert_eq!(loaded[0].status, CoreInstallStatus::Installed);
    assert_eq!(repo.last_fetched_at().unwrap(), Some(1000));
}

#[test]
fn bios_round_trip() {
    let db = db();
    let repo = BiosRepo::new(&db);
    let report = BiosReport {
        checks: vec![BiosCheck {
            entry: KnownBios {
                system_id: "psx".into(),
                system: "Sony PlayStation".into(),
                name: "SCPHE1001".into(),
                filename: "scph1001.bin".into(),
                md5: Some("abc123".into()),
                required: true,
            },
            path: PathBuf::from("/system/scph1001.bin"),
            status: BiosStatus::Present,
        }],
    };
    repo.replace_all(&report, 2000).unwrap();
    let loaded = repo.report().unwrap();
    assert_eq!(loaded.checks.len(), 1);
    assert_eq!(loaded.checks[0].entry.filename, "scph1001.bin");
    assert_eq!(loaded.checks[0].status, BiosStatus::Present);
    assert_eq!(repo.last_scanned_at().unwrap(), Some(2000));
}

#[test]
fn config_round_trip() {
    let db = db();
    let repo = ConfigRepo::new(&db);
    assert!(!repo.config_imported().unwrap());

    let mut config = RetroFeelConfig::default();
    config.appearance.theme = ThemePreference::Light;
    config.video.fullscreen = true;
    config.paths.cores = PathBuf::from("/cores");
    repo.save_config(&config).unwrap();

    let loaded = repo.load_config().unwrap();
    assert_eq!(loaded, config);
    assert!(loaded.video.fullscreen);
    assert_eq!(loaded.appearance.theme, ThemePreference::Light);

    // config_imported is only set by import_from_ron_config, not save_config.
    assert!(!repo.config_imported().unwrap());
}

#[test]
fn config_missing_appearance_defaults_to_system() {
    let db = db();
    let repo = ConfigRepo::new(&db);

    let loaded = repo.load_config().unwrap();

    assert_eq!(loaded.appearance.theme, ThemePreference::System);
}

#[test]
fn config_all_theme_preferences_round_trip() {
    let db = db();
    let repo = ConfigRepo::new(&db);

    for preference in [
        ThemePreference::System,
        ThemePreference::Light,
        ThemePreference::Dark,
    ] {
        let mut config = RetroFeelConfig::default();
        config.appearance.theme = preference;
        repo.save_config(&config).unwrap();
        assert_eq!(repo.load_config().unwrap().appearance.theme, preference);
    }
}

#[test]
fn config_ron_migration_marker() {
    let db = db();
    let repo = ConfigRepo::new(&db);
    let mut config = RetroFeelConfig::default();
    config.steam.default_wine_prefix = Some(PathBuf::from("/wine-prefix"));
    repo.import_from_ron_config(&config).unwrap();
    assert!(repo.config_imported().unwrap());
    // Subsequent load should yield the imported config.
    let loaded = repo.load_config().unwrap();
    assert_eq!(loaded, config);
}

#[test]
fn steam_games_round_trip() {
    let db = db();
    let repo = SteamRepo::new(&db);
    let row = SteamGameRow {
        app_id: 42,
        name: "My Game".into(),
        install_dir: PathBuf::from("/games/my-game"),
        wine_prefix: Some(PathBuf::from("/wine/my-game")),
        exe_name: "game.exe".into(),
        banner_path: Some(PathBuf::from("/covers/42.png")),
        last_played: None,
        added_at: 100,
    };

    repo.upsert(&row).unwrap();
    assert_eq!(repo.get(42).unwrap(), Some(row.clone()));
    assert_eq!(repo.all().unwrap(), vec![row.clone()]);

    repo.touch_played(42, 200).unwrap();
    assert_eq!(repo.get(42).unwrap().unwrap().last_played, Some(200));

    repo.set_banner_path(42, std::path::Path::new("/covers/new.jpg"))
        .unwrap();
    assert_eq!(
        repo.get(42).unwrap().unwrap().banner_path,
        Some(PathBuf::from("/covers/new.jpg"))
    );

    repo.delete(42).unwrap();
    assert!(repo.get(42).unwrap().is_none());
}

#[test]
fn roms_round_trip() {
    let db = db();
    let repo = RomsRepo::new(&db);
    let rows = vec![RomRow {
        path: PathBuf::from("/roms/game.sfc"),
        file_name: "game.sfc".into(),
        system_id: Some("snes".into()),
        file_size: 1000,
        file_mtime: 100,
        sha1: None,
        last_played: None,
        discovered_at: 50,
        game_id: None,
        crc32: None,
        md5: None,
    }];
    repo.replace_all(&rows).unwrap();
    let loaded = repo.all().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].file_name, "game.sfc");
    repo.set_sha1(&PathBuf::from("/roms/game.sfc"), "deadbeef")
        .unwrap();
    repo.set_last_played(&PathBuf::from("/roms/game.sfc"), 999)
        .unwrap();
    let loaded = repo.all().unwrap();
    assert_eq!(loaded[0].sha1.as_deref(), Some("deadbeef"));
    assert_eq!(loaded[0].last_played, Some(999));
}

#[test]
fn recordings_round_trip() {
    let db = db();
    let repo = RecordingsRepo::new(&db);
    let row = RecordingRow {
        session_dir: PathBuf::from("/rec/2024-01-01"),
        core_name: "Snes9x".into(),
        core_version: "1.0".into(),
        rom_path: Some(PathBuf::from("/roms/game.sfc")),
        rom_sha1: Some("abc".into()),
        frame_count: 600,
        dropped_frames: 0,
        fps: 60.0,
        sample_rate: 44100.0,
        start_timestamp: 1000.0,
        video_path: Some(PathBuf::from("video.mkv")),
        input_log_path: PathBuf::from("input.json"),
        manifest_path: PathBuf::from("manifest.json"),
        transcript_text: Some("hello world".into()),
        transcript_path: Some(PathBuf::from("transcript.json")),
        transcript_status: "complete".into(),
        transcription_model: Some("parakeet-tdt-0.6b-v3-int8".into()),
        input_summary_json: Some("{\"keyboard\":2}".into()),
        source_kind: "libretro".into(),
        video_timing_mode: "frame_indexed".into(),
        frame_map_path: None,
        input_transitions_path: Some(PathBuf::from("input-transitions.jsonl")),
        narration_offset_us: Some(123),
        narration_uncertainty_us: Some(4_000),
        narration_status: "complete".into(),
        narration_presence: "present".into(),
        game_audio_presence: "present".into(),
        created_at: 1000,
    };
    repo.insert(&row).unwrap();
    let loaded = repo.all().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].core_name, "Snes9x");
    assert_eq!(loaded[0].frame_count, 600);
    repo.delete(&PathBuf::from("/rec/2024-01-01")).unwrap();
    assert!(repo.all().unwrap().is_empty());
}

#[test]
fn screenshots_round_trip() {
    let db = db();
    let repo = ScreenshotsRepo::new(&db);
    let row = ScreenshotRow {
        path: PathBuf::from("/shots/2024.png"),
        rom_path: Some(PathBuf::from("/roms/game.sfc")),
        core_name: Some("Snes9x".into()),
        captured_at: 2000,
    };
    repo.insert(&row).unwrap();
    let loaded = repo.all().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].core_name.as_deref(), Some("Snes9x"));
    repo.delete(&PathBuf::from("/shots/2024.png")).unwrap();
    assert!(repo.all().unwrap().is_empty());
}

#[test]
fn migrations_are_idempotent() {
    let db = db();
    // Opening a second in-memory DB on the same handle would error, but
    // calling migrations::run again on the same connection should be a no-op.
    db.with_conn(|conn| {
        retrofeel_db::migrations::run(conn).unwrap();
    });
}

#[test]
fn game_favorites_ratings_and_play_statistics_are_real_data() {
    let db = db();
    let games = GamesRepo::new(&db);
    let older = games
        .insert(
            "snes", "Older", None, None, None, None, None, None, None, 100,
        )
        .unwrap();
    let newer = games
        .insert(
            "snes", "Newer", None, None, None, None, None, None, None, 200,
        )
        .unwrap();

    games.set_favorite(older, true).unwrap();
    games.set_rating(older, 5).unwrap();
    games.record_play_session(older, 300, 42).unwrap();
    games.record_play_session(older, 400, 8).unwrap();

    let older = games.get(older).unwrap().unwrap();
    assert!(older.favorite);
    assert_eq!(older.rating, 5);
    assert_eq!(older.last_played, Some(400));
    assert_eq!(older.play_count, 2);
    assert_eq!(older.play_time_seconds, 50);
    assert_eq!(games.recently_added(1).unwrap()[0].id, newer);
    assert_eq!(games.recently_played(1).unwrap()[0].id, older.id);
    assert!(games.set_rating(older.id, 6).is_err());
}

#[test]
fn v4_portable_feature_tables_exist() {
    let db = db();
    db.with_conn(|conn| {
        for table in [
            "collections",
            "collection_games",
            "import_jobs",
            "import_errors",
            "save_states",
            "cheats",
            "per_game_cores",
            "input_profiles",
            "shader_presets",
            "provider_state",
            "artwork_misses",
            "homebrew_entries",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table {table}");
        }
    });
}
