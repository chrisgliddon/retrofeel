//! User config: the `config` table (key → JSON value).
//!
//! Replaces `retrofeel.ron`. The `RetroFeelConfig` struct in `retrofeel-types`
//! stays the in-memory shape; this repo persists it field-by-field so a
//! partial read is cheap. The headless runner and exporters keep their RON
//! path for standalone use.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::DbError;

/// Config keys used in the `config` table. See `lib.rs` for the mapping from
/// `RetroFeelConfig` fields to these keys.
pub mod keys {
    pub const APPEARANCE: &str = "appearance";
    pub const LIBRARY: &str = "library";
    pub const METADATA: &str = "metadata";
    pub const REWIND: &str = "rewind";
    pub const WINDOW_BEHAVIOR: &str = "window_behavior";
    pub const SHADERS: &str = "shaders";
    pub const PATHS: &str = "paths";
    pub const VIDEO: &str = "video";
    pub const AUDIO: &str = "audio";
    pub const RECORDING: &str = "recording";
    pub const CORE_OPTIONS: &str = "core_options";
    pub const CORE_OVERRIDES_BY_EXTENSION: &str = "core_overrides_by_extension";
    pub const GLOBAL_INPUT_BINDINGS: &str = "global_input_bindings";
    pub const PER_CORE_INPUT_BINDINGS: &str = "per_core_input_bindings";
    pub const RECENT_ROMS: &str = "recent_roms";
    pub const STEAM: &str = "steam";
    /// Marker key set to `"true"` once the RON→SQLite migration has run, so it
    /// only happens once.
    pub const CONFIG_IMPORTED: &str = "__config_imported_from_ron";
}

pub struct ConfigRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> ConfigRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Read a key as a deserialized `T`. Returns `None` if the key is absent.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT value_json FROM config WHERE key = ?1")?;
            let json: Option<String> = match stmt.query_row([key], |row| row.get(0)) {
                Ok(json) => Some(json),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(error) => return Err(error.into()),
            };
            match json {
                None => Ok(None),
                Some(json) => serde_json::from_str::<T>(&json)
                    .map(Some)
                    .map_err(|source| DbError::ConfigDeserialize {
                        key: key.to_string(),
                        source,
                    }),
            }
        })
    }

    /// Write a key with a serialized `value`. Upserts.
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), DbError> {
        let json = serde_json::to_string(value).map_err(|source| DbError::ConfigSerialize {
            key: key.to_string(),
            source,
        })?;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO config (key, value_json) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                rusqlite::params![key, json],
            )?;
            Ok(())
        })
    }

    /// Delete a key. No-op if absent.
    pub fn delete(&self, key: &str) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute("DELETE FROM config WHERE key = ?1", [key])?;
            Ok(())
        })
    }

    /// List all keys (for diagnostics / migration tooling).
    pub fn keys(&self) -> Result<Vec<String>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT key FROM config ORDER BY key")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Whether the RON→SQLite one-shot migration has already run.
    pub fn config_imported(&self) -> Result<bool, DbError> {
        Ok(self.get::<String>(keys::CONFIG_IMPORTED)?.is_some())
    }

    /// Import a `RetroFeelConfig` into the `config` table, field-by-field.
    /// Called once on first launch when a `retrofeel.ron` exists but the DB
    /// has no config rows yet. After a successful import, the marker key
    /// [`keys::CONFIG_IMPORTED`] is set so this never runs twice. The caller
    /// is responsible for renaming the RON file to `retrofeel.ron.migrated`
    /// as a backup.
    pub fn import_from_ron_config(
        &self,
        config: &retrofeel_types::RetroFeelConfig,
    ) -> Result<(), DbError> {
        self.set(keys::APPEARANCE, &config.appearance)?;
        self.set(keys::LIBRARY, &config.library)?;
        self.set(keys::METADATA, &config.metadata)?;
        self.set(keys::REWIND, &config.rewind)?;
        self.set(keys::WINDOW_BEHAVIOR, &config.window_behavior)?;
        self.set(keys::SHADERS, &config.shaders)?;
        self.set(keys::PATHS, &config.paths)?;
        self.set(keys::VIDEO, &config.video)?;
        self.set(keys::AUDIO, &config.audio)?;
        self.set(keys::RECORDING, &config.recording)?;
        self.set(keys::CORE_OPTIONS, &config.core_options)?;
        self.set(
            keys::CORE_OVERRIDES_BY_EXTENSION,
            &config.core_overrides_by_extension,
        )?;
        self.set(keys::GLOBAL_INPUT_BINDINGS, &config.global_input_bindings)?;
        self.set(
            keys::PER_CORE_INPUT_BINDINGS,
            &config.per_core_input_bindings,
        )?;
        self.set(keys::RECENT_ROMS, &config.recent_roms)?;
        self.set(keys::STEAM, &config.steam)?;
        self.set(keys::CONFIG_IMPORTED, &"true")?;
        Ok(())
    }

    /// Load a full `RetroFeelConfig` from the `config` table, falling back to
    /// `Default` for any missing keys (so a fresh DB yields the default
    /// config without error).
    pub fn load_config(&self) -> Result<retrofeel_types::RetroFeelConfig, DbError> {
        use retrofeel_types::{
            AppearanceConfig, AudioConfig, InputBindingSet, LibraryConfig, MetadataConfig,
            PathsConfig, RecordingConfig, RetroFeelConfig, RewindConfig, ShaderConfig, SteamConfig,
            VideoConfig, WindowBehaviorConfig,
        };
        let mut config = RetroFeelConfig {
            appearance: self
                .get::<AppearanceConfig>(keys::APPEARANCE)?
                .unwrap_or_default(),
            library: self
                .get::<LibraryConfig>(keys::LIBRARY)?
                .unwrap_or_default(),
            metadata: self
                .get::<MetadataConfig>(keys::METADATA)?
                .unwrap_or_default(),
            rewind: self.get::<RewindConfig>(keys::REWIND)?.unwrap_or_default(),
            window_behavior: self
                .get::<WindowBehaviorConfig>(keys::WINDOW_BEHAVIOR)?
                .unwrap_or_default(),
            shaders: self.get::<ShaderConfig>(keys::SHADERS)?.unwrap_or_default(),
            paths: self.get::<PathsConfig>(keys::PATHS)?.unwrap_or_default(),
            video: self.get::<VideoConfig>(keys::VIDEO)?.unwrap_or_default(),
            audio: self.get::<AudioConfig>(keys::AUDIO)?.unwrap_or_default(),
            recording: self
                .get::<RecordingConfig>(keys::RECORDING)?
                .unwrap_or_default(),
            core_options: self.get(keys::CORE_OPTIONS)?.unwrap_or_default(),
            core_overrides_by_extension: self
                .get(keys::CORE_OVERRIDES_BY_EXTENSION)?
                .unwrap_or_default(),
            global_input_bindings: self
                .get::<InputBindingSet>(keys::GLOBAL_INPUT_BINDINGS)?
                .unwrap_or_default(),
            per_core_input_bindings: self.get(keys::PER_CORE_INPUT_BINDINGS)?.unwrap_or_default(),
            recent_roms: self.get(keys::RECENT_ROMS)?.unwrap_or_default(),
            steam: self.get::<SteamConfig>(keys::STEAM)?.unwrap_or_default(),
        };
        config.normalize_legacy();
        Ok(config)
    }

    /// Persist a full `RetroFeelConfig` to the `config` table, field-by-field.
    pub fn save_config(&self, config: &retrofeel_types::RetroFeelConfig) -> Result<(), DbError> {
        self.set(keys::APPEARANCE, &config.appearance)?;
        self.set(keys::LIBRARY, &config.library)?;
        self.set(keys::METADATA, &config.metadata)?;
        self.set(keys::REWIND, &config.rewind)?;
        self.set(keys::WINDOW_BEHAVIOR, &config.window_behavior)?;
        self.set(keys::SHADERS, &config.shaders)?;
        self.set(keys::PATHS, &config.paths)?;
        self.set(keys::VIDEO, &config.video)?;
        self.set(keys::AUDIO, &config.audio)?;
        self.set(keys::RECORDING, &config.recording)?;
        self.set(keys::CORE_OPTIONS, &config.core_options)?;
        self.set(
            keys::CORE_OVERRIDES_BY_EXTENSION,
            &config.core_overrides_by_extension,
        )?;
        self.set(keys::GLOBAL_INPUT_BINDINGS, &config.global_input_bindings)?;
        self.set(
            keys::PER_CORE_INPUT_BINDINGS,
            &config.per_core_input_bindings,
        )?;
        self.set(keys::RECENT_ROMS, &config.recent_roms)?;
        self.set(keys::STEAM, &config.steam)?;
        Ok(())
    }
}
