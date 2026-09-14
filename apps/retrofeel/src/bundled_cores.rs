//! Bundled-cores location resolver.
//!
//! When the app is built with the `bundled-cores` Cargo feature, pre-
//! downloaded core archives are copied into `OUT_DIR/cores` by `build.rs`.
//! At runtime, [`BundledCores::dir`] resolves the platform-correct path to
//! those archives:
//!
//! - **macOS**: `Contents/Resources/cores` inside the app bundle (when
//!   packaged as a `.app`), or the `OUT_DIR/cores` path compiled into the
//!   binary (for dev runs via `cargo run`).
//! - **Linux/Windows**: the `cores/` directory next to the executable.
//!
//! When the feature is disabled, [`BundledCores::dir`] returns `None` and the
//! app falls back to the user-configured cores directory (download catalog
//! flow). Wired in during the Phase 2 DB integration — the first-launch
//! seeding reads bundled cores from here and inserts them into the `cores`
//! table.

#![allow(dead_code)]

use std::path::PathBuf;

pub struct BundledCores;

impl BundledCores {
    /// The directory containing bundled core archives, or `None` if the app
    /// was built without the `bundled-cores` feature.
    pub fn dir() -> Option<PathBuf> {
        if !cfg!(feature = "bundled-cores") {
            return None;
        }

        // The build script writes cores to OUT_DIR/cores. OUT_DIR is not
        // available at runtime, but on macOS we can look for the
        // Resources/cores path inside an app bundle, and elsewhere we look
        // next to the executable. As a fallback (dev runs via `cargo run`),
        // the OUT_DIR path is baked in at compile time via an env var set by
        // build.rs.
        if let Some(bundle_dir) = self::macos_bundle_resources_dir() {
            let cores = bundle_dir.join("cores");
            if cores.is_dir() {
                return Some(cores);
            }
        }

        if let Some(exe_dir) = self::exe_dir() {
            let cores = exe_dir.join("cores");
            if cores.is_dir() {
                return Some(cores);
            }
        }

        // Dev-run fallback: the OUT_DIR from build time. build.rs emits this
        // as the `BUNDLED_CORES_OUT_DIR` env var.
        let out_dir = option_env!("BUNDLED_CORES_OUT_DIR");
        out_dir.map(PathBuf::from).filter(|p| p.is_dir())
    }
}

#[cfg(target_os = "macos")]
fn macos_bundle_resources_dir() -> Option<PathBuf> {
    // If running inside a .app bundle, the executable is at
    // `Foo.app/Contents/MacOS/foo` and resources are at
    // `Foo.app/Contents/Resources`.
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()?.to_str()? == "Contents"
        && macos_dir.file_name()?.to_str()? == "MacOS"
    {
        Some(contents_dir.join("Resources"))
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_bundle_resources_dir() -> Option<PathBuf> {
    None
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
}
