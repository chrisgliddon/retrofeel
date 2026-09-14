//! Steam game library: the `steam_games` table.
//!
//! Stores discovered Steam games (from GameHub steamapps .acf files) and
//! manually added entries. Each row maps an app_id to its install dir, Wine
//! prefix, and exe name — enough to launch + capture via `steam_thread`.

use std::path::PathBuf;

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamGameRow {
    pub app_id: u32,
    pub name: String,
    pub install_dir: PathBuf,
    pub wine_prefix: Option<PathBuf>,
    pub exe_name: String,
    pub banner_path: Option<PathBuf>,
    pub last_played: Option<u64>,
    pub added_at: u64,
}

pub struct SteamRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> SteamRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Upsert a single Steam game (insert or replace by app_id).
    pub fn upsert(&self, row: &SteamGameRow) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO steam_games
                 (app_id, name, install_dir, wine_prefix, exe_name, banner_path,
                  last_played, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    row.app_id as i64,
                    row.name,
                    row.install_dir.to_string_lossy(),
                    row.wine_prefix.as_ref().map(|p| p.to_string_lossy()),
                    row.exe_name,
                    row.banner_path.as_ref().map(|p| p.to_string_lossy()),
                    row.last_played.map(|t| t as i64),
                    row.added_at as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// Load all Steam games, ordered by name.
    pub fn all(&self) -> Result<Vec<SteamGameRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT app_id, name, install_dir, wine_prefix, exe_name, banner_path,
                        last_played, added_at
                 FROM steam_games ORDER BY name",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(SteamGameRow {
                    app_id: row.get::<_, i64>(0)? as u32,
                    name: row.get(1)?,
                    install_dir: PathBuf::from(row.get::<_, String>(2)?),
                    wine_prefix: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    exe_name: row.get(4)?,
                    banner_path: row.get::<_, Option<String>>(5)?.map(PathBuf::from),
                    last_played: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                    added_at: row.get::<_, i64>(7)? as u64,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Get a single game by app_id.
    pub fn get(&self, app_id: u32) -> Result<Option<SteamGameRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT app_id, name, install_dir, wine_prefix, exe_name, banner_path,
                        last_played, added_at
                 FROM steam_games WHERE app_id = ?1",
            )?;
            let mut rows = stmt.query_map([app_id as i64], |row| {
                Ok(SteamGameRow {
                    app_id: row.get::<_, i64>(0)? as u32,
                    name: row.get(1)?,
                    install_dir: PathBuf::from(row.get::<_, String>(2)?),
                    wine_prefix: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    exe_name: row.get(4)?,
                    banner_path: row.get::<_, Option<String>>(5)?.map(PathBuf::from),
                    last_played: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                    added_at: row.get::<_, i64>(7)? as u64,
                })
            })?;
            if let Some(row) = rows.next() {
                Ok(Some(row?))
            } else {
                Ok(None)
            }
        })
    }

    /// Update last_played timestamp for a game.
    pub fn touch_played(&self, app_id: u32, timestamp: u64) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE steam_games SET last_played = ?1 WHERE app_id = ?2",
                rusqlite::params![timestamp as i64, app_id as i64],
            )?;
            Ok(())
        })
    }

    /// Persist the atomically cached Steam header path without replacing the
    /// rest of the discovered game's launch metadata.
    pub fn set_banner_path(&self, app_id: u32, path: &std::path::Path) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE steam_games SET banner_path = ?1 WHERE app_id = ?2",
                rusqlite::params![path.to_string_lossy(), app_id as i64],
            )?;
            Ok(())
        })
    }

    /// Delete a game by app_id.
    pub fn delete(&self, app_id: u32) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute("DELETE FROM steam_games WHERE app_id = ?1", [app_id as i64])?;
            Ok(())
        })
    }
}
