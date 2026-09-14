//! Build script for the retrofeel app.
//!
//! When the `bundled-cores` feature is enabled, copies pre-downloaded core
//! archives from `apps/retrofeel/bundled-cores/` into `OUT_DIR/cores/` so the
//! app can find them at runtime via `BundledCores::dir()`. The archives are
//! extracted at runtime on first launch (not at build time) to keep the build
//! fast and avoid bundling `.dylib` files that might not survive code signing.
//!
//! When the feature is disabled, this is a no-op — dev and CI builds don't
//! bundle cores.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=bundled-cores");
    println!("cargo:rerun-if-changed=Cargo.toml");

    if !cfg!(feature = "bundled-cores") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let cores_out = out_dir.join("cores");
    let bundled_src = PathBuf::from("bundled-cores");

    // Bake the OUT_DIR/cores path into the binary so `BundledCores::dir` can
    // find the cores during dev runs (cargo run) where there's no app bundle.
    println!(
        "cargo:rustc-env=BUNDLED_CORES_OUT_DIR={}",
        cores_out.display()
    );

    std::fs::create_dir_all(&cores_out).expect("could not create OUT_DIR/cores");

    if !bundled_src.exists() {
        println!(
            "cargo:warning=bundled-cores feature is enabled but apps/retrofeel/bundled-cores/ \
             does not exist. Run scripts/fetch-bundled-cores.sh first."
        );
        return;
    }

    let mut count = 0;
    for entry in std::fs::read_dir(&bundled_src).expect("could not read bundled-cores dir") {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                println!("cargo:warning=skipping bundled-cores entry: {error}");
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let dest = cores_out.join(entry.file_name());
        if let Err(error) = std::fs::copy(&path, &dest) {
            println!("cargo:warning=failed to copy {}: {error}", path.display());
            continue;
        }
        count += 1;
    }

    println!("cargo:warning=bundled {count} core archives into OUT_DIR/cores");
}
