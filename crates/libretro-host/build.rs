//! Build script: compile the in-repo mock libretro core (cdylib) directly with
//! rustc into `OUT_DIR`, so integration tests can locate it regardless of the
//! active `CARGO_TARGET_DIR`. We avoid invoking `cargo` (which deadlocks when
//! nested inside a build script).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let mock_src = PathBuf::from(&manifest_dir).join("tests/mock-core/src/lib.rs");

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

    // Compile the mock core as a cdylib. We pass the mock-core source directly;
    // it has no external dependencies (only std), so a single rustc invocation
    // is sufficient.
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
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cargo:warning=failed to compile mock-core: {e}");
            return;
        }
    };
    if !status.success() {
        eprintln!("cargo:warning=mock-core compilation failed");
        return;
    }

    println!("cargo:rustc-env=MOCK_CORE_PATH={}", dst.display());
    println!("cargo:rerun-if-changed=tests/mock-core/src/lib.rs");
    println!("cargo:rerun-if-changed=tests/mock-core/Cargo.toml");
}
