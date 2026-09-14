//! Backend services for phase 3.
//!
//! This crate intentionally has no Bevy dependency. It is the app-facing layer
//! for configuration-backed core discovery, BIOS validation, core option
//! application, and save persistence.

pub mod bios;
pub mod catalog;
pub mod metadata;
pub mod options;
pub mod persistence;
pub mod registry;
pub mod steam;

pub use bios::{known_bioses, scan_system_dir, BiosCheck, BiosReport, BiosStatus, KnownBios};
pub use catalog::{
    install_core, install_core_from_zip, parse_core_catalog, parse_info_zip, refresh_core_catalog,
    refresh_core_catalog_for_platform, BuildbotPlatform, CoreCatalogEntry, CoreCatalogError,
    CoreInfoMetadata, CoreInstallResult, CoreInstallStatus,
};
pub use metadata::{GameMetadata, MetadataDb};
pub use options::{
    apply_configured_core_options, parse_core_variable, CoreOptionApplyReport, InvalidCoreOption,
    ParsedCoreOption,
};
pub use persistence::{
    flush_sram, load_sram, load_state_slot, save_state_path, save_state_slot, sram_path,
    PersistenceError,
};
pub use registry::{CoreDescriptor, CoreRegistry, CoreScanFailure, CoreScanReport, RegistryError};
pub use steam::{discover_installed_steam_games, scan_native_steam_libraries, scan_steamapps};
