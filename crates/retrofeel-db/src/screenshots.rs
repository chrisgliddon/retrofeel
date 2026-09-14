//! Screenshots index: the `screenshots` table.

use std::path::PathBuf;

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotRow {
    pub path: PathBuf,
    pub rom_path: Option<PathBuf>,
    pub core_name: Option<String>,
    pub captured_at: u64,
}

pub struct ScreenshotsRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> ScreenshotsRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Insert a screenshot row.
    pub fn insert(&self, row: &ScreenshotRow) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO screenshots (path, rom_path, core_name, captured_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    row.path.to_string_lossy(),
                    row.rom_path.as_ref().map(|p| p.to_string_lossy()),
                    row.core_name,
                    row.captured_at as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// Load all screenshots, newest first.
    pub fn all(&self) -> Result<Vec<ScreenshotRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, rom_path, core_name, captured_at
                 FROM screenshots
                 ORDER BY captured_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                let path: String = row.get(0)?;
                let rom_path: Option<String> = row.get(1)?;
                Ok(ScreenshotRow {
                    path: PathBuf::from(path),
                    rom_path: rom_path.map(PathBuf::from),
                    core_name: row.get(2)?,
                    captured_at: row.get::<_, i64>(3)? as u64,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Delete a screenshot row by path. (Does not delete the image file —
    /// the caller is responsible for filesystem cleanup.)
    pub fn delete(&self, path: &std::path::Path) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "DELETE FROM screenshots WHERE path = ?1",
                [path.to_string_lossy()],
            )?;
            Ok(())
        })
    }
}
