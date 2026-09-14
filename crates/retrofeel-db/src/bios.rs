//! BIOS status cache: the `bios_status` table.
//!
//! Caches which expected BIOS files are present/missing/bad so launches stop
//! re-hashing every BIOS file in the system dir.

use std::path::PathBuf;

use retrofeel_backend::{BiosCheck, BiosReport, BiosStatus, KnownBios};

use crate::error::DbError;

pub struct BiosRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> BiosRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Replace the entire `bios_status` table with `report`'s checks.
    pub fn replace_all(&self, report: &BiosReport, scanned_at: u64) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute("BEGIN", [])?;
            conn.execute("DELETE FROM bios_status", [])?;
            let mut stmt = conn.prepare(
                "INSERT INTO bios_status (filename, system_id, present, md5_verified,
                    expected_md5, actual_md5, file_size, scanned_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for check in &report.checks {
                let (present, actual_md5, md5_verified) = match &check.status {
                    BiosStatus::Present => (true, None, true),
                    BiosStatus::PresentUnverified => (true, None, false),
                    BiosStatus::Missing => (false, None, false),
                    BiosStatus::BadHash { actual_md5 } => (true, Some(actual_md5.clone()), false),
                };
                let file_size = std::fs::metadata(&check.path).ok().map(|m| m.len() as i64);
                stmt.execute(rusqlite::params![
                    check.entry.filename,
                    check.entry.system_id,
                    present as i64,
                    md5_verified as i64,
                    check.entry.md5,
                    actual_md5,
                    file_size,
                    scanned_at as i64,
                ])?;
            }
            drop(stmt);
            conn.execute("COMMIT", [])?;
            Ok(())
        })
    }

    /// Load a `BiosReport` from the cache (no filesystem hashing).
    pub fn report(&self) -> Result<BiosReport, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT filename, system_id, present, md5_verified, expected_md5, actual_md5,
                        file_size
                 FROM bios_status
                 ORDER BY system_id, filename",
            )?;
            let rows = stmt.query_map([], |row| {
                let filename: String = row.get(0)?;
                let system_id: String = row.get(1)?;
                let present: bool = row.get::<_, i64>(2)? != 0;
                let md5_verified: bool = row.get::<_, i64>(3)? != 0;
                let expected_md5: Option<String> = row.get(4)?;
                let actual_md5: Option<String> = row.get(5)?;

                let status = if !present {
                    BiosStatus::Missing
                } else if md5_verified {
                    BiosStatus::Present
                } else if let Some(actual) = actual_md5 {
                    BiosStatus::BadHash { actual_md5: actual }
                } else {
                    BiosStatus::PresentUnverified
                };
                Ok(BiosCheck {
                    entry: KnownBios {
                        system_id,
                        system: String::new(),
                        name: String::new(),
                        filename,
                        md5: expected_md5,
                        required: false,
                    },
                    path: PathBuf::new(),
                    status,
                })
            })?;
            let checks: Vec<BiosCheck> = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(BiosReport { checks })
        })
    }

    /// The epoch-seconds timestamp of the last BIOS scan, or `None` if never.
    pub fn last_scanned_at(&self) -> Result<Option<u64>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT MAX(scanned_at) FROM bios_status")?;
            let ts: Option<i64> = stmt.query_row([], |row| row.get(0))?;
            Ok(ts.map(|t| t as u64))
        })
    }
}
