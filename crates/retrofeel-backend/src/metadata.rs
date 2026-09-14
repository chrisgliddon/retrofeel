//! Metadata lookup: hash-based ROM identification using an OpenVGDB-style
//! SQLite database.
//!
//! Given a ROM's CRC32 or MD5 hash, looks up the game title, description,
//! genre, publisher, release date, and box art URL from a read-only metadata
//! database. The database is opened read-only and cached for the process
//! lifetime.
//!
//! Systems that don't have an OpenVGDB mapping (`System.openvgdb_system_id`
//! is empty) are skipped — the caller falls back to filename-based title
//! matching.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

/// Metadata for a single game, looked up by ROM hash.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GameMetadata {
    pub title: String,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub publisher: Option<String>,
    pub developer: Option<String>,
    pub release_date: Option<String>,
    pub cover_url: Option<String>,
}

/// A read-only OpenVGDB-compatible metadata database.
pub struct MetadataDb {
    conn: Connection,
}

impl MetadataDb {
    /// Open a metadata database at the given path (read-only).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { conn })
    }

    /// Look up game metadata by CRC32 hash and OpenVGDB system short name.
    ///
    /// The `system_short_name` is the OpenVGDB `SYSTEMS.systemShortName` value
    /// (e.g. `"NES"`, `"SNES"`), mapped from `System.openvgdb_system_id`.
    pub fn lookup_by_crc32(
        &self,
        system_short_name: &str,
        crc32: &str,
    ) -> anyhow::Result<Option<GameMetadata>> {
        self.lookup_by_hash(system_short_name, "romHashCRC", crc32)
    }

    /// Look up game metadata by MD5 hash and OpenVGDB system short name.
    pub fn lookup_by_md5(
        &self,
        system_short_name: &str,
        md5: &str,
    ) -> anyhow::Result<Option<GameMetadata>> {
        self.lookup_by_hash(system_short_name, "romHashMD5", md5)
    }

    fn lookup_by_hash(
        &self,
        system_short_name: &str,
        hash_column: &str,
        hash_value: &str,
    ) -> anyhow::Result<Option<GameMetadata>> {
        // Join ROMs → RELEASES to get the metadata.
        let system_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT systemID FROM SYSTEMS WHERE systemShortName = ?1",
                [system_short_name],
                |row| row.get(0),
            )
            .optional()?;

        let Some(system_id) = system_id else {
            return Ok(None);
        };

        let rom_id: Option<i64> = self
            .conn
            .query_row(
                &format!(
                    "SELECT romID FROM ROMs WHERE systemID = ?1 AND {hash_column} = ?2 LIMIT 1"
                ),
                rusqlite::params![system_id, hash_value],
                |row| row.get(0),
            )
            .optional()?;

        let Some(rom_id) = rom_id else {
            return Ok(None);
        };

        let meta = self
            .conn
            .query_row(
                "SELECT releaseTitleName, releaseDescription, releaseGenre,
                        releasePublisher, releaseDeveloper, releaseDate,
                        releaseCoverFront
                 FROM RELEASES WHERE romID = ?1 LIMIT 1",
                [rom_id],
                |row| {
                    Ok(GameMetadata {
                        title: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        description: row.get(1)?,
                        genre: row.get(2)?,
                        publisher: row.get(3)?,
                        developer: row.get(4)?,
                        release_date: row.get(5)?,
                        cover_url: row.get(6)?,
                    })
                },
            )
            .optional()?;

        Ok(meta)
    }

    /// Check if the database has any entries for the given system.
    pub fn has_system(&self, system_short_name: &str) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM ROMs r JOIN SYSTEMS s ON r.systemID = s.systemID
                 WHERE s.systemShortName = ?1",
                [system_short_name],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .unwrap_or(false)
    }
}
