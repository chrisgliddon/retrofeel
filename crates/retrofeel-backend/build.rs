//! Build the in-repo mock libretro core for backend integration tests.
//!
//! This mirrors `libretro-host/build.rs` but resolves the fixture through the
//! sibling host crate. The source has no external dependencies, so a direct
//! rustc invocation avoids nested cargo builds.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mock_src = manifest_dir
        .join("../libretro-host/tests/mock-core/src/lib.rs")
        .canonicalize()
        .expect("mock core source");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let prefix = if cfg!(target_os = "windows") {
        ""
    } else {
        "lib"
    };
    let name = format!("{prefix}mock_core.{ext}");
    let dst = out_dir.join(&name);

    let status = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
        .arg(&mock_src)
        .arg("--edition=2021")
        .arg("--crate-type=cdylib")
        .arg("--crate-name=mock_core")
        .arg("-C")
        .arg("opt-level=1")
        .arg("-o")
        .arg(&dst)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-env=MOCK_CORE_PATH={}", dst.display());
        }
        Ok(_) => {
            eprintln!("cargo:warning=mock-core compilation failed");
        }
        Err(e) => {
            eprintln!("cargo:warning=failed to compile mock-core: {e}");
        }
    }

    println!("cargo:rerun-if-changed={}", mock_src.display());
}
