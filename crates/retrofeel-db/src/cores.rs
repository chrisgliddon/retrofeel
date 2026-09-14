//! Core registry cache: the `cores` table.
//!
//! Replaces the per-launch `CoreRegistry::scan_dir` that re-`dlopen`s every
//! core on startup. With 197 bundled cores that's 197 `Core::probe` calls;
//! the cache lets a warm launch read them from SQLite in one query. The
//! `rescan_if_changed` method walks the cores dir, compares `file_mtime` +
//! `file_size` to the cached rows, and only re-probes new/changed cores.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use retrofeel_backend::{CoreDescriptor, CoreRegistry, CoreScanReport};

use crate::error::DbError;

/// Row in the `cores` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreRow {
    pub path: PathBuf,
    pub library_name: String,
    pub library_version: String,
    pub valid_extensions: Vec<String>,
    pub need_fullpath: bool,
    pub block_extract: bool,
    pub source: String,
    pub bundled: bool,
    pub file_size: u64,
    pub file_mtime: u64,
    pub last_seen: u64,
}

impl From<&CoreRow> for CoreDescriptor {
    fn from(row: &CoreRow) -> Self {
        CoreDescriptor {
            path: row.path.clone(),
            library_name: row.library_name.clone(),
            library_version: row.library_version.clone(),
            valid_extensions: row.valid_extensions.clone(),
            need_fullpath: row.need_fullpath,
            block_extract: row.block_extract,
        }
    }
}

pub struct CoresRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> CoresRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Replace the entire `cores` table with `rows` in one transaction.
    /// Used by `rescan_if_changed` after a filesystem walk.
    pub fn replace_all(&self, rows: &[CoreRow]) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute("BEGIN", [])?;
            conn.execute("DELETE FROM cores", [])?;
            let mut stmt = conn.prepare(
                "INSERT INTO cores (path, library_name, library_version, valid_extensions,
                    need_fullpath, block_extract, source, bundled, file_size, file_mtime, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for row in rows {
                stmt.execute(rusqlite::params![
                    row.path.to_string_lossy(),
                    row.library_name,
                    row.library_version,
                    row.valid_extensions.join("|"),
                    row.need_fullpath as i64,
                    row.block_extract as i64,
                    row.source,
                    row.bundled as i64,
                    row.file_size as i64,
                    row.file_mtime as i64,
                    row.last_seen as i64,
                ])?;
            }
            drop(stmt);
            conn.execute("COMMIT", [])?;
            Ok(())
        })
    }

    /// Load all cached cores, sorted by `library_name` then `path` (matching
    /// `CoreRegistry::scan_dir`'s sort order).
    pub fn all(&self) -> Result<Vec<CoreRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, library_name, library_version, valid_extensions,
                        need_fullpath, block_extract, source, bundled, file_size, file_mtime,
                        last_seen
                 FROM cores
                 ORDER BY library_name, path",
            )?;
            let rows = stmt.query_map([], |row| {
                let path: String = row.get(0)?;
                let exts: String = row.get(3)?;
                Ok(CoreRow {
                    path: PathBuf::from(path),
                    library_name: row.get(1)?,
                    library_version: row.get(2)?,
                    valid_extensions: exts
                        .split('|')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect(),
                    need_fullpath: row.get::<_, i64>(4)? != 0,
                    block_extract: row.get::<_, i64>(5)? != 0,
                    source: row.get(6)?,
                    bundled: row.get::<_, i64>(7)? != 0,
                    file_size: row.get::<_, i64>(8)? as u64,
                    file_mtime: row.get::<_, i64>(9)? as u64,
                    last_seen: row.get::<_, i64>(10)? as u64,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Build a `CoreRegistry` + `CoreScanReport` from the cache (no
    /// filesystem access, no `Core::probe` calls). Use this on a warm launch.
    pub fn registry(&self) -> Result<CoreScanReport, DbError> {
        let rows = self.all()?;
        let cores: Vec<CoreDescriptor> = rows.iter().map(CoreDescriptor::from).collect();
        Ok(CoreScanReport {
            registry: CoreRegistry { cores },
            failures: Vec::new(),
        })
    }
}

/// Read a file's `(size, mtime)` from the filesystem, or `None` if it's gone.
pub fn file_metadata(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((size, mtime))
}

/// Current epoch seconds, or 0 if the clock is before the epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
