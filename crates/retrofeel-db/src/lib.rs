//! Embedded SQLite persistence for retrofeel.
//!
//! This crate owns the on-disk database (`retrofeel.db` under the app's data
//! directory) and the repositories that read/write it. It has **no Bevy
//! dependency** so it stays testable in isolation, mirroring `retrofeel-backend`.
//!
//! ## What lives in the DB
//!
//! - **Core registry cache** (`cores` table) — replaces the per-launch
//!   `CoreRegistry::scan_dir` that re-`dlopen`s every core on startup. With
//!   197 bundled cores that's 197 `Core::probe` calls; the cache lets a warm
//!   launch read them from SQLite in one query.
//! - **Core catalog cache** (`core_catalog` table) — the 197-entry buildbot
//!   catalog, so "Refresh" is instant on subsequent launches instead of
//!   re-fetching HTML + `info.zip` every time.
//! - **BIOS status cache** (`bios_status` table) — which expected BIOS files
//!   are present/missing/bad, so launches stop re-hashing every BIOS file.
//! - **ROM library cache** (`roms` table) — replaces the per-launch filesystem
//!   scan of rom dirs.
//! - **Recordings index** (`recordings` table) — session metadata in the DB
//!   (media files stay on the filesystem — they're large binaries, not
//!   relational). Replaces the per-launch scan that read every `manifest.json`.
//! - **Screenshots index** (`screenshots` table).
//! - **User config** (`config` table, key → JSON value) — replaces
//!   `retrofeel.ron`. The `RetroFeelConfig` struct in `retrofeel-types` stays
//!   the in-memory shape; the headless runner and exporters keep their RON
//!   path for standalone use. Existing users get a one-shot RON→SQLite import
//!   on first launch.
//!
//! ## What stays on the filesystem
//!
//! SRAM saves, save states, recording media (`.mkv`/`.wav`/`input.json`),
//! screenshots (`.png`), and the recording manifest files — these are large
//! binaries or exporter-facing files that don't benefit from being in SQLite.

pub mod bios;
pub mod catalog;
pub mod config;
pub mod connection;
pub mod cores;
pub mod error;
pub mod games;
pub mod migrations;
pub mod recordings;
pub mod roms;
pub mod screenshots;
pub mod steam;

pub use bios::BiosRepo;
pub use catalog::CatalogRepo;
pub use config::ConfigRepo;
pub use connection::Db;
pub use cores::CoresRepo;
pub use error::DbError;
pub use games::{GameRow, GamesRepo};
pub use recordings::RecordingsRepo;
pub use roms::RomsRepo;
pub use screenshots::ScreenshotsRepo;
pub use steam::{SteamGameRow, SteamRepo};
