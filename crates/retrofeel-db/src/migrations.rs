//! Schema migrations for the retrofeel database.
//!
//! Migrations are forward-only and tracked by SQLite's `user_version` PRAGMA.
//! Each migration is a transactional batch of DDL. The v1 schema creates all
//! tables in one pass; future migrations bump `user_version` incrementally.

use crate::error::DbError;

/// The current schema version. Bump this when adding a new migration.
pub const CURRENT_VERSION: u32 = 6;

/// Run all pending migrations. Idempotent: if the DB is already at
/// [`CURRENT_VERSION`], this is a no-op.
pub fn run(conn: &rusqlite::Connection) -> Result<(), DbError> {
    let mut current: u32 =
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
    if current >= CURRENT_VERSION {
        return Ok(());
    }

    if current < 1 {
        apply_v1(conn)?;
        conn.pragma_update(None, "user_version", 1)?;
        current = 1;
    }

    if current < 2 {
        apply_v2(conn)?;
        conn.pragma_update(None, "user_version", 2)?;
        current = 2;
    }

    if current < 3 {
        apply_v3(conn)?;
        conn.pragma_update(None, "user_version", 3)?;
        current = 3;
    }

    if current < 4 {
        apply_v4(conn)?;
        conn.pragma_update(None, "user_version", 4)?;
        current = 4;
    }
    if current < 5 {
        apply_v5(conn)?;
        conn.pragma_update(None, "user_version", 5)?;
        current = 5;
    }
    if current < 6 {
        apply_v6(conn)?;
        conn.pragma_update(None, "user_version", 6)?;
    }
    Ok(())
}

/// v1: the initial schema. Creates all tables in one transaction.
fn apply_v1(conn: &rusqlite::Connection) -> Result<(), DbError> {
    apply_schema_transaction(conn, V1_SCHEMA)
}

const V1_SCHEMA: &str = r#"
-- Core registry cache: replaces the per-launch Core::probe scan.
CREATE TABLE IF NOT EXISTS cores (
  path             TEXT PRIMARY KEY,
  library_name     TEXT NOT NULL,
  library_version  TEXT NOT NULL,
  valid_extensions TEXT NOT NULL,
  need_fullpath    INTEGER NOT NULL,
  block_extract    INTEGER NOT NULL,
  source           TEXT NOT NULL,
  bundled          INTEGER NOT NULL,
  file_size        INTEGER NOT NULL,
  file_mtime       INTEGER NOT NULL,
  last_seen        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cores_library_name ON cores(library_name);

-- Core catalog cache: the buildbot catalog, so "Refresh" is instant.
CREATE TABLE IF NOT EXISTS core_catalog (
  slug              TEXT PRIMARY KEY,
  archive_name      TEXT NOT NULL,
  display_name      TEXT NOT NULL,
  inferred_system   TEXT,
  download_url      TEXT NOT NULL,
  installed_path    TEXT,
  status            TEXT NOT NULL,
  catalog_fetched_at INTEGER NOT NULL
);

-- BIOS status cache: which expected BIOS files are present/missing/bad.
CREATE TABLE IF NOT EXISTS bios_status (
  filename       TEXT NOT NULL,
  system_id      TEXT NOT NULL,
  present        INTEGER NOT NULL,
  md5_verified   INTEGER NOT NULL,
  expected_md5   TEXT,
  actual_md5     TEXT,
  file_size      INTEGER,
  scanned_at     INTEGER NOT NULL,
  PRIMARY KEY (system_id, filename)
);

-- User config: key → JSON value. Replaces retrofeel.ron.
CREATE TABLE IF NOT EXISTS config (
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL
);

-- ROM library cache: replaces the per-launch filesystem scan of rom dirs.
CREATE TABLE IF NOT EXISTS roms (
  path           TEXT PRIMARY KEY,
  file_name      TEXT NOT NULL,
  system_id      TEXT,
  file_size      INTEGER NOT NULL,
  file_mtime     INTEGER NOT NULL,
  sha1           TEXT,
  last_played    INTEGER,
  discovered_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_roms_system_id ON roms(system_id);
CREATE INDEX IF NOT EXISTS idx_roms_last_played ON roms(last_played);

-- Recordings index: metadata in DB, media files stay on the filesystem.
CREATE TABLE IF NOT EXISTS recordings (
  session_dir     TEXT PRIMARY KEY,
  core_name       TEXT NOT NULL,
  core_version    TEXT NOT NULL,
  rom_path        TEXT,
  rom_sha1        TEXT,
  frame_count     INTEGER NOT NULL,
  dropped_frames  INTEGER NOT NULL DEFAULT 0,
  fps             REAL NOT NULL,
  sample_rate     REAL NOT NULL,
  start_timestamp REAL NOT NULL,
  video_path      TEXT,
  input_log_path  TEXT NOT NULL,
  manifest_path   TEXT NOT NULL,
  created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_recordings_rom_path ON recordings(rom_path);
CREATE INDEX IF NOT EXISTS idx_recordings_created_at ON recordings(created_at);

-- Screenshots index.
CREATE TABLE IF NOT EXISTS screenshots (
  path         TEXT PRIMARY KEY,
  rom_path     TEXT,
  core_name    TEXT,
  captured_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_screenshots_captured_at ON screenshots(captured_at);
"#;

/// v2: adds the steam_games table for Steam game library entries.
fn apply_v2(conn: &rusqlite::Connection) -> Result<(), DbError> {
    apply_schema_transaction(conn, V2_SCHEMA)
}

const V2_SCHEMA: &str = r#"
-- Steam game library: discovered from GameHub steamapps .acf files or added manually.
CREATE TABLE IF NOT EXISTS steam_games (
  app_id       INTEGER PRIMARY KEY,
  name         TEXT NOT NULL,
  install_dir  TEXT NOT NULL,
  wine_prefix  TEXT,
  exe_name     TEXT NOT NULL,
  banner_path  TEXT,
  last_played  INTEGER,
  added_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_steam_games_last_played ON steam_games(last_played);
"#;

/// v3: adds the `games` table (metadata identity separate from the physical
/// ROM file, separating canonical game metadata from discovered ROM files) and extends `roms` with
/// hash + game_id columns for hash-based identification.
fn apply_v3(conn: &rusqlite::Connection) -> Result<(), DbError> {
    apply_schema_transaction(conn, V3_SCHEMA)
}

const V3_SCHEMA: &str = r#"
-- Game metadata identity: one row per logical game (title + system),
-- independent of the physical ROM file. Populated by hash-based metadata
-- lookup (OpenVGDB) or by filename fallback. One game can have multiple ROM
-- dumps (different regions, hacks) via the `roms.game_id` FK.
CREATE TABLE IF NOT EXISTS games (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  system_id    TEXT NOT NULL,
  title        TEXT NOT NULL,
  description  TEXT,
  genre        TEXT,
  publisher    TEXT,
  developer    TEXT,
  release_date TEXT,
  cover_url    TEXT,
  cover_path   TEXT,
  created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_games_system_id ON games(system_id);
CREATE INDEX IF NOT EXISTS idx_games_title ON games(title);

-- Extend roms with hash columns (CRC32 + MD5 for OpenVGDB compatibility;
-- SHA1 already exists) and a FK to the games table.
ALTER TABLE roms ADD COLUMN game_id INTEGER REFERENCES games(id);
ALTER TABLE roms ADD COLUMN crc32 TEXT;
ALTER TABLE roms ADD COLUMN md5 TEXT;
"#;

/// v4: portable library/state/input/service persistence. All media and large
/// state payloads remain filesystem-backed; SQLite stores identity, metadata,
/// relationships, compatibility information, and durable job errors.
fn apply_v4(conn: &rusqlite::Connection) -> Result<(), DbError> {
    apply_schema_transaction(conn, V4_SCHEMA)
}

fn apply_schema_transaction(conn: &rusqlite::Connection, schema: &str) -> Result<(), DbError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    if let Err(error) = conn.execute_batch(schema) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(error.into());
    }
    if let Err(error) = conn.execute_batch("COMMIT") {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(error.into());
    }
    Ok(())
}

const V4_SCHEMA: &str = r#"
ALTER TABLE games ADD COLUMN sort_title TEXT;
ALTER TABLE games ADD COLUMN user_title TEXT;
ALTER TABLE games ADD COLUMN metadata_locked INTEGER NOT NULL DEFAULT 0;
ALTER TABLE games ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0;
ALTER TABLE games ADD COLUMN rating INTEGER NOT NULL DEFAULT 0 CHECK (rating BETWEEN 0 AND 5);
ALTER TABLE games ADD COLUMN imported_at INTEGER;
ALTER TABLE games ADD COLUMN last_played INTEGER;
ALTER TABLE games ADD COLUMN play_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE games ADD COLUMN play_time_seconds INTEGER NOT NULL DEFAULT 0;
ALTER TABLE games ADD COLUMN artwork_provider TEXT;
ALTER TABLE games ADD COLUMN artwork_revision TEXT;
CREATE INDEX IF NOT EXISTS idx_games_imported_at ON games(imported_at);
CREATE INDEX IF NOT EXISTS idx_games_last_played ON games(last_played);
CREATE INDEX IF NOT EXISTS idx_games_favorite ON games(favorite);

CREATE TABLE IF NOT EXISTS collections (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  name        TEXT NOT NULL COLLATE NOCASE UNIQUE,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS collection_games (
  collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
  game_id       INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
  position      INTEGER NOT NULL DEFAULT 0,
  added_at      INTEGER NOT NULL,
  PRIMARY KEY (collection_id, game_id)
);
CREATE INDEX IF NOT EXISTS idx_collection_games_game ON collection_games(game_id);

CREATE TABLE IF NOT EXISTS import_jobs (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  source_path    TEXT NOT NULL,
  mode           TEXT NOT NULL CHECK (mode IN ('reference', 'copy')),
  status         TEXT NOT NULL,
  discovered     INTEGER NOT NULL DEFAULT 0,
  completed      INTEGER NOT NULL DEFAULT 0,
  cancelled      INTEGER NOT NULL DEFAULT 0,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS import_errors (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id      INTEGER NOT NULL REFERENCES import_jobs(id) ON DELETE CASCADE,
  path        TEXT,
  kind        TEXT NOT NULL,
  message     TEXT NOT NULL,
  recoverable INTEGER NOT NULL DEFAULT 1,
  created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_import_errors_job ON import_errors(job_id);

CREATE TABLE IF NOT EXISTS save_states (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  game_id         INTEGER REFERENCES games(id) ON DELETE CASCADE,
  rom_path        TEXT NOT NULL,
  kind            TEXT NOT NULL CHECK (kind IN ('named', 'quick', 'auto')),
  slot            INTEGER,
  name            TEXT,
  state_path      TEXT NOT NULL UNIQUE,
  screenshot_path TEXT,
  core_name       TEXT NOT NULL,
  core_version    TEXT NOT NULL,
  state_size      INTEGER NOT NULL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_save_states_game ON save_states(game_id, created_at);

CREATE TABLE IF NOT EXISTS cheats (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  game_id     INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  code        TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 0,
  position    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_cheats_game ON cheats(game_id);

CREATE TABLE IF NOT EXISTS per_game_cores (
  game_id     INTEGER PRIMARY KEY REFERENCES games(id) ON DELETE CASCADE,
  core_path   TEXT NOT NULL,
  core_name   TEXT,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS input_profiles (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  system_id         TEXT NOT NULL,
  player            INTEGER NOT NULL,
  port              INTEGER NOT NULL,
  device_id         TEXT NOT NULL,
  profile_json      TEXT NOT NULL,
  frontend_hotkeys_json TEXT NOT NULL DEFAULT '{}',
  updated_at        INTEGER NOT NULL,
  UNIQUE (system_id, player, port, device_id)
);

CREATE TABLE IF NOT EXISTS shader_presets (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  name          TEXT NOT NULL,
  source_path   TEXT NOT NULL UNIQUE,
  system_id     TEXT,
  game_id       INTEGER REFERENCES games(id) ON DELETE CASCADE,
  parameters_json TEXT NOT NULL DEFAULT '{}',
  imported_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS provider_state (
  provider      TEXT PRIMARY KEY,
  revision      TEXT NOT NULL,
  last_success  INTEGER,
  last_attempt  INTEGER,
  error         TEXT
);
CREATE TABLE IF NOT EXISTS artwork_misses (
  game_id       INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
  provider      TEXT NOT NULL,
  revision      TEXT NOT NULL,
  attempted_at  INTEGER NOT NULL,
  PRIMARY KEY (game_id, provider, revision)
);

CREATE TABLE IF NOT EXISTS homebrew_entries (
  id              TEXT PRIMARY KEY,
  feed_version    TEXT NOT NULL,
  title           TEXT NOT NULL,
  system_id       TEXT NOT NULL,
  download_url    TEXT NOT NULL,
  license         TEXT NOT NULL,
  sha256          TEXT NOT NULL,
  metadata_json   TEXT NOT NULL,
  artwork_url     TEXT,
  cached_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_homebrew_system ON homebrew_entries(system_id);
"#;

/// v5: searchable transcription and input-summary metadata. Session folders
/// remain authoritative and can repopulate these columns during re-indexing.
fn apply_v5(conn: &rusqlite::Connection) -> Result<(), DbError> {
    apply_schema_transaction(conn, V5_SCHEMA)
}

const V5_SCHEMA: &str = r#"
ALTER TABLE recordings ADD COLUMN transcript_text TEXT;
ALTER TABLE recordings ADD COLUMN transcript_path TEXT;
ALTER TABLE recordings ADD COLUMN transcript_status TEXT NOT NULL DEFAULT 'not_requested';
ALTER TABLE recordings ADD COLUMN transcription_model TEXT;
ALTER TABLE recordings ADD COLUMN input_summary_json TEXT;
CREATE INDEX IF NOT EXISTS idx_recordings_transcript_status ON recordings(transcript_status);
"#;

/// v6: source-aware capture and alignment metadata. Existing rows get
/// conservative legacy values and are refreshed from authoritative manifests
/// whenever the recording index runs.
fn apply_v6(conn: &rusqlite::Connection) -> Result<(), DbError> {
    apply_schema_transaction(conn, V6_SCHEMA)
}

const V6_SCHEMA: &str = r#"
ALTER TABLE recordings ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'legacy';
ALTER TABLE recordings ADD COLUMN video_timing_mode TEXT NOT NULL DEFAULT 'legacy_frame_indexed';
ALTER TABLE recordings ADD COLUMN frame_map_path TEXT;
ALTER TABLE recordings ADD COLUMN input_transitions_path TEXT;
ALTER TABLE recordings ADD COLUMN narration_offset_us INTEGER;
ALTER TABLE recordings ADD COLUMN narration_uncertainty_us INTEGER;
ALTER TABLE recordings ADD COLUMN narration_status TEXT NOT NULL DEFAULT 'unavailable';
ALTER TABLE recordings ADD COLUMN narration_presence TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE recordings ADD COLUMN game_audio_presence TEXT NOT NULL DEFAULT 'unknown';
CREATE INDEX IF NOT EXISTS idx_recordings_source_kind ON recordings(source_kind);
CREATE INDEX IF NOT EXISTS idx_recordings_narration_status ON recordings(narration_status);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_migration_rolls_back_ddl_and_keeps_previous_version() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // Pretend this is a v3 database, but pre-create a v4 column so v4
        // fails after applying its first few ALTER statements.
        conn.execute_batch(
            "CREATE TABLE games (id INTEGER PRIMARY KEY, favorite INTEGER NOT NULL DEFAULT 0);
             PRAGMA user_version = 3;",
        )
        .unwrap();

        assert!(run(&conn).is_err());
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(games)").unwrap();
            stmt.query_map([], |row| row.get(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(columns, vec!["id", "favorite"]);
    }

    #[test]
    fn v5_adds_searchable_recording_metadata() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        apply_v1(&conn).unwrap();
        apply_v2(&conn).unwrap();
        apply_v3(&conn).unwrap();
        apply_v4(&conn).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        run(&conn).unwrap();
        let columns = {
            let mut stmt = conn.prepare("PRAGMA table_info(recordings)").unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(columns.contains(&"transcript_text".to_string()));
        assert!(columns.contains(&"input_summary_json".to_string()));
    }

    #[test]
    fn v6_adds_source_aware_recording_metadata() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        apply_v1(&conn).unwrap();
        apply_v2(&conn).unwrap();
        apply_v3(&conn).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
        run(&conn).unwrap();
        let columns = {
            let mut stmt = conn.prepare("PRAGMA table_info(recordings)").unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(columns.contains(&"frame_map_path".to_string()));
        assert!(columns.contains(&"narration_uncertainty_us".to_string()));
    }
}
