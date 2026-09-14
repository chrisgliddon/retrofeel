use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownBios {
    /// Canonical system id, matching `retrofeel_types::system` ids (e.g. `"psx"`).
    /// Used to group the BIOS manager and to answer "which BIOS does this ROM's
    /// system need?" for the pre-launch check.
    #[serde(default)]
    pub system_id: String,
    /// Human-readable system name for display (e.g. `"Sony PlayStation"`).
    pub system: String,
    pub name: String,
    pub filename: String,
    /// Expected MD5 (lowercase hex). `None` means presence-only — the file's
    /// existence is checked but its contents are not verified (used to grow the
    /// database with entries whose trusted hash we don't have).
    #[serde(default)]
    pub md5: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiosReport {
    pub checks: Vec<BiosCheck>,
}

impl BiosReport {
    pub fn missing_required(&self) -> impl Iterator<Item = &BiosCheck> {
        self.checks.iter().filter(|check| {
            check.entry.required
                && matches!(
                    check.status,
                    BiosStatus::Missing | BiosStatus::BadHash { .. }
                )
        })
    }

    pub fn has_required_failures(&self) -> bool {
        self.missing_required().next().is_some()
    }

    /// Required BIOS entries for a specific system id whose file is missing or
    /// has a bad hash. Drives the pre-launch "this game needs BIOS X" warning.
    pub fn missing_required_for_system<'a>(
        &'a self,
        system_id: &'a str,
    ) -> impl Iterator<Item = &'a BiosCheck> + 'a {
        self.checks.iter().filter(move |check| {
            check.entry.system_id == system_id
                && check.entry.required
                && matches!(
                    check.status,
                    BiosStatus::Missing | BiosStatus::BadHash { .. }
                )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiosCheck {
    pub entry: KnownBios,
    pub path: PathBuf,
    pub status: BiosStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BiosStatus {
    /// File exists and its MD5 matches the expected hash.
    Present,
    /// File exists but the database has no trusted hash to verify it against.
    PresentUnverified,
    Missing,
    BadHash {
        actual_md5: String,
    },
}

#[derive(Debug, Error)]
pub enum BiosError {
    #[error("failed to parse bundled BIOS database: {0}")]
    ParseDatabase(#[from] serde_json::Error),
    #[error("failed to read BIOS file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn known_bioses() -> Result<Vec<KnownBios>, BiosError> {
    parse_known_bioses(include_str!("../data/bios.json"))
}

pub fn parse_known_bioses(json: &str) -> Result<Vec<KnownBios>, BiosError> {
    serde_json::from_str(json).map_err(BiosError::ParseDatabase)
}

pub fn scan_system_dir(system_dir: impl AsRef<Path>) -> Result<BiosReport, BiosError> {
    scan_system_dir_with_db(system_dir, &known_bioses()?)
}

pub fn scan_system_dir_with_db(
    system_dir: impl AsRef<Path>,
    database: &[KnownBios],
) -> Result<BiosReport, BiosError> {
    let system_dir = system_dir.as_ref();
    let mut checks = Vec::with_capacity(database.len());

    for entry in database {
        let path = system_dir.join(&entry.filename);
        let status = if !path.exists() {
            BiosStatus::Missing
        } else if let Some(expected_md5) = &entry.md5 {
            let bytes = std::fs::read(&path).map_err(|source| BiosError::ReadFile {
                path: path.clone(),
                source,
            })?;
            let actual_md5 = format!("{:x}", md5::compute(bytes));
            if actual_md5.eq_ignore_ascii_case(expected_md5) {
                BiosStatus::Present
            } else {
                BiosStatus::BadHash { actual_md5 }
            }
        } else {
            // No trusted hash on file — confirm presence only.
            BiosStatus::PresentUnverified
        };

        checks.push(BiosCheck {
            entry: entry.clone(),
            path,
            status,
        });
    }

    checks.sort_by(|a, b| {
        a.entry
            .system
            .cmp(&b.entry.system)
            .then_with(|| a.entry.name.cmp(&b.entry.name))
    });

    Ok(BiosReport { checks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_db(md5: String) -> Vec<KnownBios> {
        vec![KnownBios {
            system_id: "test".into(),
            system: "Test System".into(),
            name: "test bios".into(),
            filename: "test.bin".into(),
            md5: Some(md5),
            required: true,
        }]
    }

    #[test]
    fn reports_present_bios_when_hash_matches() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"known bios bytes";
        std::fs::write(temp.path().join("test.bin"), bytes).unwrap();
        let report = scan_system_dir_with_db(
            temp.path(),
            &fixture_db(format!("{:x}", md5::compute(bytes))),
        )
        .unwrap();

        assert_eq!(report.checks[0].status, BiosStatus::Present);
        assert!(!report.has_required_failures());
    }

    #[test]
    fn reports_bad_hash_and_missing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("test.bin"), b"wrong").unwrap();
        let report = scan_system_dir_with_db(temp.path(), &fixture_db("0000".into())).unwrap();

        assert!(matches!(
            report.checks[0].status,
            BiosStatus::BadHash { .. }
        ));
        assert!(report.has_required_failures());

        std::fs::remove_file(temp.path().join("test.bin")).unwrap();
        let report = scan_system_dir_with_db(temp.path(), &fixture_db("0000".into())).unwrap();
        assert_eq!(report.checks[0].status, BiosStatus::Missing);
    }

    #[test]
    fn bundled_database_parses_and_is_tagged() {
        let db = known_bioses().expect("bundled bios.json must parse");
        assert!(db.iter().any(|entry| entry.system_id == "psx"));
        // Every entry must carry a canonical system id so grouping + the
        // per-ROM required-BIOS lookup work.
        assert!(db.iter().all(|entry| !entry.system_id.is_empty()));
    }

    #[test]
    fn present_but_unverified_when_no_hash() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nohash.bin"), b"anything").unwrap();
        let db = vec![KnownBios {
            system_id: "test".into(),
            system: "Test System".into(),
            name: "no-hash bios".into(),
            filename: "nohash.bin".into(),
            md5: None,
            required: true,
        }];
        let report = scan_system_dir_with_db(temp.path(), &db).unwrap();
        assert_eq!(report.checks[0].status, BiosStatus::PresentUnverified);
        // Presence-only entries are not "required failures".
        assert!(!report.has_required_failures());
    }

    #[test]
    fn missing_required_for_system_filters_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let db = vec![
            KnownBios {
                system_id: "psx".into(),
                system: "Sony PlayStation".into(),
                name: "psx bios".into(),
                filename: "psx.bin".into(),
                md5: None,
                required: true,
            },
            KnownBios {
                system_id: "gba".into(),
                system: "Game Boy Advance".into(),
                name: "gba bios".into(),
                filename: "gba.bin".into(),
                md5: None,
                required: true,
            },
        ];
        // Neither file exists → both missing.
        let report = scan_system_dir_with_db(temp.path(), &db).unwrap();
        let psx_missing: Vec<_> = report.missing_required_for_system("psx").collect();
        assert_eq!(psx_missing.len(), 1);
        assert_eq!(psx_missing[0].entry.system_id, "psx");
        assert_eq!(report.missing_required_for_system("nes").count(), 0);
    }
}
