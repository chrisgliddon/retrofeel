//! User-supplied smoke test slot.
//!
//! Phase 1 ships tests against the in-repo mock core. To verify against a real
//! libretro core (e.g. the buildbot `gong` core), drop the core binary
//! somewhere on disk and set the `RETROFEEL_CORE` env var to its path before
//! running:
//!
//! ```sh
//! RETROFEEL_CORE=/path/to/gong_libretro.so cargo test -p libretro-host --test user_smoke -- --ignored --nocapture
//! ```
//!
//! These tests are `#[ignore]`d by default so CI doesn't require external cores.

use std::path::PathBuf;

use libretro_host::Core;
use retrofeel_types::InputState;

fn core_path() -> Option<PathBuf> {
    std::env::var("RETROFEEL_CORE").ok().map(PathBuf::from)
}

#[test]
#[ignore]
fn user_core_loads_and_runs() {
    let path = core_path().expect("set RETROFEEL_CORE to a core path");
    let mut core = Core::load(&path, "system").expect("load core");
    println!(
        "loaded: {} v{}",
        core.system_info().library_name,
        core.system_info().library_version
    );
    core.load_game(&[], None).expect("load game");
    let frame = core
        .run_frame_required(InputState::default())
        .expect("run frame");
    println!(
        "frame: {}x{} ({} bytes)",
        frame.width,
        frame.height,
        frame.rgba.len()
    );
    assert!(!frame.rgba.is_empty());
}

#[test]
#[ignore]
fn user_core_deterministic() {
    let path = core_path().expect("set RETROFEEL_CORE to a core path");
    let mut core = Core::load(&path, "system").expect("load core");
    core.load_game(&[], None).expect("load game");
    let run = |c: &mut Core| -> Vec<u8> {
        let mut last = Vec::new();
        for _ in 0..30 {
            last = c.run_frame_required(InputState::default()).unwrap().rgba;
        }
        last
    };
    let a = run(&mut core);
    core.reset().unwrap();
    let b = run(&mut core);
    assert_eq!(a, b, "user core is not deterministic with idle input");
}

#[test]
#[ignore]
fn user_core_state_round_trip() {
    let path = core_path().expect("set RETROFEEL_CORE to a core path");
    let mut core = Core::load(&path, "system").expect("load core");
    core.load_game(&[], None).expect("load game");
    for _ in 0..10 {
        core.run_frame(InputState::default()).unwrap();
    }
    let state = core.serialize().expect("serialize");
    let after_a = core.run_frame_required(InputState::default()).unwrap();
    core.unserialize(&state).expect("unserialize");
    let after_b = core.run_frame_required(InputState::default()).unwrap();
    assert_eq!(after_a.rgba, after_b.rgba);
}
