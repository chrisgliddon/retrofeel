//! Bevy integration for the embedded SQLite database.
//!
//! [`DbPlugin`] opens the database at startup and inserts the [`Db`] resource
//! (a cheap-to-clone `Arc<Mutex<Connection>>` wrapper from `retrofeel_db`).
//! The startup system then runs the one-shot RON→SQLite config migration if
//! needed, loads the config from the DB, and seeds bundled cores.
//!
//! The individual repos (`CoresRepo`, `CatalogRepo`, etc.) are constructed
//! on demand from the `Db` handle — they're cheap borrows, not resources — so
//! any system that needs DB access just takes `Res<Db>` and constructs the
//! repo it needs.

use std::path::PathBuf;

use bevy::prelude::{Plugin, Resource};
use retrofeel_db::Db;
use retrofeel_types::RetroFeelConfig;

use crate::diagnostics;

pub struct DbPlugin;

impl Plugin for DbPlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        use bevy::prelude::IntoScheduleConfigs;
        app.add_systems(
            bevy::prelude::Startup,
            open_db.into_configs().before(crate::plugin::startup),
        );
    }
}

/// The open database, inserted as a Bevy resource by [`open_db`].
#[derive(Resource, Clone)]
#[allow(dead_code)]
pub struct DbResource {
    pub db: Db,
}

/// The path the DB was opened at (for diagnostics).
#[derive(Resource)]
#[allow(dead_code)]
pub struct DbPath {
    pub path: PathBuf,
}

/// Startup system: open the DB, run the RON→SQLite migration if needed, load
/// config from the DB, and insert the `DbResource`. Runs before
/// `plugin::startup` so the `FrontendModel` can read from the DB.
fn open_db(
    mut commands: bevy::prelude::Commands,
    args: bevy::prelude::Res<crate::plugin::RetrofeelArgs>,
) {
    let db_path = db_path_for(&args.config);
    log::info!("db.path path=\"{}\"", db_path.display());

    if let Some(parent) = db_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            log::error!(
                "db.create_dir_failed path=\"{}\" error=\"{error}\"",
                parent.display()
            );
        }
    }

    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(error) => {
            log::error!(
                "db.open_failed path=\"{}\" error=\"{error}\"",
                db_path.display()
            );
            return;
        }
    };

    // One-shot RON→SQLite config migration. Normal launches never create a
    // RON file; when a legacy platform RON exists we import it once, retain a
    // `.migrated` backup, and seed defaults otherwise. An explicit `--config`
    // launch is intentionally file-backed and does not alter the default DB.
    let config_repo = retrofeel_db::ConfigRepo::new(&db);
    if !args.config_file_override && !config_repo.config_imported().unwrap_or(false) {
        let legacy_path = RetroFeelConfig::platform_config_path().ok();
        let legacy = legacy_path
            .as_ref()
            .filter(|path| path.is_file())
            .and_then(|path| RetroFeelConfig::load_from_path(path).ok());
        if let Some(legacy) = legacy {
            log::info!("db.migrating_config_from_ron");
            if let Err(error) = config_repo.import_from_ron_config(&legacy) {
                log::error!("db.config_migration_failed error=\"{error}\"");
            } else if let Some(ron_path) = legacy_path {
                let backup = ron_path.with_extension("ron.migrated");
                if let Err(error) = std::fs::rename(&ron_path, &backup) {
                    log::warn!(
                        "db.ron_rename_failed from=\"{}\" to=\"{}\" error=\"{error}\"",
                        ron_path.display(),
                        backup.display()
                    );
                }
            }
        } else if let Err(error) = config_repo.import_from_ron_config(&args.config) {
            // `import_from_ron_config` is also the atomic field-by-field seed
            // operation; no RON file is written in this branch.
            log::error!("db.config_seed_failed error=\"{error}\"");
        }
    }

    // Seed bundled cores into the DB on first launch (if the bundled-cores
    // feature is on and the cores table is empty).
    seed_bundled_cores(&db);

    commands.insert_resource(DbResource { db });
    commands.insert_resource(DbPath { path: db_path });
}

/// Resolve the DB path: `<data_dir>/retrofeel.db`. When `--no-config` is
/// passed (config is `Default` and `config_path` is `None`), use a temp dir
/// so dev runs don't pollute the platform data dir.
pub(crate) fn db_path_for(config: &RetroFeelConfig) -> PathBuf {
    // The config's `paths` all live under the data dir; the DB goes there too.
    // Use the cores dir's parent as the data dir (all config paths are under
    // the same data base per `RetroFeelConfig::with_data_base`).
    let cores_dir = &config.paths.cores;
    if let Some(data_dir) = cores_dir.parent() {
        data_dir.join("retrofeel.db")
    } else {
        std::env::temp_dir().join("retrofeel.db")
    }
}

/// Seed bundled cores into the `cores` table if the table is empty and the
/// `bundled-cores` feature is enabled. Extracts each `.zip` archive in the
/// bundled-cores dir, probes the core via `Core::probe`, and inserts a row.
fn seed_bundled_cores(db: &Db) {
    let Some(bundled_dir) = crate::bundled_cores::BundledCores::dir() else {
        return; // feature is off or no bundled cores present
    };

    let cores_repo = retrofeel_db::CoresRepo::new(db);
    if !cores_repo.all().unwrap_or_default().is_empty() {
        return; // already seeded
    }

    log::info!("db.seeding_bundled_cores dir=\"{}\"", bundled_dir.display());

    let user_cores_dir = bundled_dir.join("extracted");
    if let Err(error) = std::fs::create_dir_all(&user_cores_dir) {
        log::error!("db.seed_bundled_cores.create_dir_failed error=\"{error}\"");
        return;
    }

    let _timing = diagnostics::Timing::start("db.seed_bundled_cores");
    let mut rows = Vec::new();
    let now = retrofeel_db::cores::now_secs();

    let entries = match std::fs::read_dir(&bundled_dir) {
        Ok(entries) => entries,
        Err(error) => {
            log::error!(
                "db.seed_bundled_cores.read_dir_failed dir=\"{}\" error=\"{error}\"",
                bundled_dir.display()
            );
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("db.seed_bundled_cores.skip error=\"{error}\"");
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "zip" {
            continue;
        }

        // Extract the core library from the zip.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!(
                    "db.seed_bundled_cores.read_failed path=\"{}\" error=\"{error}\"",
                    path.display()
                );
                continue;
            }
        };
        let archive_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let slug = archive_name
            .trim_end_matches(".zip")
            .trim_end_matches("_libretro.dylib")
            .trim_end_matches("_libretro.so")
            .trim_end_matches("_libretro.dll");

        // Extract the .dylib/.so/.dll from the zip into user_cores_dir.
        let extracted_path =
            user_cores_dir.join(format!("{slug}_libretro.{}", library_extension()));
        if let Err(error) = extract_core_from_zip(&bytes, &extracted_path) {
            log::warn!("db.seed_bundled_cores.extract_failed slug=\"{slug}\" error=\"{error}\"");
            continue;
        }

        // Probe the extracted core (static info only — no retro_init, safe).
        let info = match libretro_host::Core::probe(&extracted_path) {
            Ok(info) => info,
            Err(error) => {
                log::warn!("db.seed_bundled_cores.probe_failed slug=\"{slug}\" error=\"{error}\"");
                continue;
            }
        };

        let meta = retrofeel_db::cores::file_metadata(&extracted_path);
        let (file_size, file_mtime) = meta.unwrap_or((0, now));

        rows.push(retrofeel_db::cores::CoreRow {
            path: extracted_path,
            library_name: info.library_name,
            library_version: info.library_version,
            valid_extensions: info.valid_extensions,
            need_fullpath: info.need_fullpath,
            block_extract: info.block_extract,
            source: "bundled".to_string(),
            bundled: true,
            file_size,
            file_mtime,
            last_seen: now,
        });
    }

    let count = rows.len();
    if let Err(error) = cores_repo.replace_all(&rows) {
        log::error!("db.seed_bundled_cores.persist_failed error=\"{error}\"");
        return;
    }
    log::info!("db.seed_bundled_cores.done count={count}");
}

fn library_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}

/// Extract the core library from a zip archive into `dest`. Mirrors
/// `install_core_from_zip`'s extraction logic but without validation (the
/// seed path probes via `Core::probe` after extraction).
fn extract_core_from_zip(bytes: &[u8], dest: &std::path::Path) -> Result<(), String> {
    use std::io::Cursor;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("zip parse: {e}"))?;

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| format!("zip entry {index}: {e}"))?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let path = std::path::Path::new(&name);
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Reject unsafe paths (zip-slip).
        if path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        }) {
            continue;
        }
        // Only extract the core library file.
        let looks_like_core = file_name.ends_with(".dylib")
            || file_name.ends_with(".so")
            || file_name.ends_with(".dll");
        if !looks_like_core {
            continue;
        }

        let dest_file = dest.with_file_name(file_name);
        let mut out = std::fs::File::create(&dest_file)
            .map_err(|e| format!("create {}: {e}", dest_file.display()))?;
        std::io::copy(&mut file, &mut out).map_err(|e| format!("copy: {e}"))?;
        return Ok(());
    }
    Err("no core library found in archive".to_string())
}
