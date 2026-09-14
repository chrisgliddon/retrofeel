//! Core catalog cache: the `core_catalog` table.
//!
//! Caches the 197-entry libretro buildbot catalog so "Refresh" is instant on
//! subsequent launches instead of re-fetching HTML + `info.zip` every time.

use std::path::PathBuf;

use retrofeel_backend::{CoreCatalogEntry, CoreInstallStatus};

use crate::error::DbError;

pub struct CatalogRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> CatalogRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Replace the entire `core_catalog` table with `entries` in one
    /// transaction. Called after a network refresh.
    pub fn replace_all(
        &self,
        entries: &[CoreCatalogEntry],
        fetched_at: u64,
    ) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute("BEGIN", [])?;
            conn.execute("DELETE FROM core_catalog", [])?;
            let mut stmt = conn.prepare(
                "INSERT INTO core_catalog (slug, archive_name, display_name, inferred_system,
                    download_url, installed_path, status, catalog_fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for entry in entries {
                stmt.execute(rusqlite::params![
                    entry.slug,
                    entry.archive_name,
                    entry.display_name,
                    entry.inferred_system,
                    entry.download_url,
                    entry.installed_path.as_ref().map(|p| p.to_string_lossy()),
                    match entry.status {
                        CoreInstallStatus::Available => "available",
                        CoreInstallStatus::Installed => "installed",
                    },
                    fetched_at as i64,
                ])?;
            }
            drop(stmt);
            conn.execute("COMMIT", [])?;
            Ok(())
        })
    }

    /// Load all cached catalog entries, sorted by `display_name` then
    /// `archive_name` (matching `parse_core_catalog`'s sort order).
    pub fn all(&self) -> Result<Vec<CoreCatalogEntry>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT slug, archive_name, display_name, inferred_system, download_url,
                        installed_path, status
                 FROM core_catalog
                 ORDER BY display_name, archive_name",
            )?;
            let rows = stmt.query_map([], |row| {
                let installed_path: Option<String> = row.get(5)?;
                let status: String = row.get(6)?;
                Ok(CoreCatalogEntry {
                    slug: row.get(0)?,
                    archive_name: row.get(1)?,
                    display_name: row.get(2)?,
                    inferred_system: row.get(3)?,
                    download_url: row.get(4)?,
                    installed_path: installed_path.map(PathBuf::from),
                    status: if status == "installed" {
                        CoreInstallStatus::Installed
                    } else {
                        CoreInstallStatus::Available
                    },
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// The epoch-seconds timestamp of the last network refresh, or `None` if
    /// the catalog has never been fetched.
    pub fn last_fetched_at(&self) -> Result<Option<u64>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT MAX(catalog_fetched_at) FROM core_catalog")?;
            let ts: Option<i64> = stmt.query_row([], |row| row.get(0))?;
            Ok(ts.map(|t| t as u64))
        })
    }
}
