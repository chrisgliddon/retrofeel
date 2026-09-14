//! ROM library cache: the `roms` table.
//!
//! Replaces the per-launch filesystem scan of rom dirs.

use std::path::PathBuf;

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomRow {
    pub path: PathBuf,
    pub file_name: String,
    pub system_id: Option<String>,
    pub file_size: u64,
    pub file_mtime: u64,
    pub sha1: Option<String>,
    pub last_played: Option<u64>,
    pub discovered_at: u64,
    pub game_id: Option<i64>,
    pub crc32: Option<String>,
    pub md5: Option<String>,
}

pub struct RomsRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> RomsRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Replace the entire `roms` table in one transaction.
    pub fn replace_all(&self, rows: &[RomRow]) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute("BEGIN", [])?;
            conn.execute("DELETE FROM roms", [])?;
            let mut stmt = conn.prepare(
                "INSERT INTO roms (path, file_name, system_id, file_size, file_mtime, sha1,
                    last_played, discovered_at, game_id, crc32, md5)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for row in rows {
                stmt.execute(rusqlite::params![
                    row.path.to_string_lossy(),
                    row.file_name,
                    row.system_id,
                    row.file_size as i64,
                    row.file_mtime as i64,
                    row.sha1,
                    row.last_played.map(|t| t as i64),
                    row.discovered_at as i64,
                    row.game_id,
                    row.crc32,
                    row.md5,
                ])?;
            }
            drop(stmt);
            conn.execute("COMMIT", [])?;
            Ok(())
        })
    }

    /// Load all ROMs, ordered by `file_name`.
    pub fn all(&self) -> Result<Vec<RomRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, file_name, system_id, file_size, file_mtime, sha1, last_played,
                        discovered_at, game_id, crc32, md5
                 FROM roms
                 ORDER BY file_name",
            )?;
            let rows = stmt.query_map([], |row| {
                let path: String = row.get(0)?;
                Ok(RomRow {
                    path: PathBuf::from(path),
                    file_name: row.get(1)?,
                    system_id: row.get(2)?,
                    file_size: row.get::<_, i64>(3)? as u64,
                    file_mtime: row.get::<_, i64>(4)? as u64,
                    sha1: row.get(5)?,
                    last_played: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                    discovered_at: row.get::<_, i64>(7)? as u64,
                    game_id: row.get(8)?,
                    crc32: row.get(9)?,
                    md5: row.get(10)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Record the SHA-1 of a ROM (lazily computed on first launch).
    pub fn set_sha1(&self, path: &std::path::Path, sha1: &str) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE roms SET sha1 = ?1 WHERE path = ?2",
                rusqlite::params![sha1, path.to_string_lossy()],
            )?;
            Ok(())
        })
    }

    /// Record when a ROM was last played.
    pub fn set_last_played(&self, path: &std::path::Path, when: u64) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE roms SET last_played = ?1 WHERE path = ?2",
                rusqlite::params![when as i64, path.to_string_lossy()],
            )?;
            Ok(())
        })
    }
}
