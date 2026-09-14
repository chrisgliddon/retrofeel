//! Integration tests against the in-repo mock libretro core.
//!
//! These exercise the full FFI path: dlopen → callbacks → pixel conversion →
//! save states → memory regions. The mock core is built as a cdylib and its
//! path is exposed by the `mock-core` dev-dependency.

use std::path::PathBuf;

use libretro_host::{abi, Core};
use retrofeel_types::device_ids::joypad;
use retrofeel_types::{InputState, RetroPadButtonBits};

/// Resolve the compiled `libmock_core` cdylib. The build script copies it to
/// `OUT_DIR` and exposes the path via the `MOCK_CORE_PATH` env var.
fn mock_core_path() -> PathBuf {
    PathBuf::from(env!("MOCK_CORE_PATH"))
}

fn load_core() -> Core {
    let path = mock_core_path();
    assert!(
        path.exists(),
        "mock core not built; expected at {}. Run `cargo build -p mock-core` first.",
        path.display()
    );
    Core::load(&path, "system").expect("load mock core")
}

fn empty_input() -> InputState {
    InputState {
        buttons: RetroPadButtonBits::EMPTY,
        ..Default::default()
    }
}

#[test]
fn loads_and_reports_system_info() {
    let core = load_core();
    let info = core.system_info();
    assert_eq!(info.library_name, "mock-core");
    assert_eq!(info.library_version, "0.1.0");
    assert!(!info.need_fullpath);
}

#[test]
fn av_info_is_queried_only_after_content_loads() {
    let mut core = load_core();
    assert_eq!(core.av_info().base_width, 0);
    assert_eq!(core.av_info().fps, 0.0);

    core.load_game(&[], None).expect("load game");

    assert_eq!(core.av_info().base_width, 256);
    assert_eq!(core.av_info().base_height, 224);
    assert_eq!(core.av_info().fps, 60.0);
}

#[test]
fn optional_cheat_and_controller_capabilities_are_gated() {
    let mut core = load_core();
    core.load_game(&[], None).unwrap();
    let capabilities = core.capabilities().unwrap();
    assert!(capabilities.save_states);
    assert!(capabilities.cheats);
    assert!(capabilities.controller_port_devices);
    assert!(core.set_cheat(0, true, "0123-4567").unwrap());
    assert!(core.reset_cheats().unwrap());
    assert!(core
        .set_controller_port_device(0, retrofeel_types::device_ids::DEVICE_JOYPAD)
        .unwrap());
    assert!(core.set_cheat(0, true, "bad\0code").is_err());
}

#[test]
fn runs_frames_and_produces_rgba() {
    let mut core = load_core();
    core.load_game(&[], None).expect("load game");
    let frame = core.run_frame_required(empty_input()).expect("run frame");
    assert_eq!(frame.width, 256);
    assert_eq!(frame.height, 224);
    assert_eq!(frame.rgba.len(), 256 * 224 * 4);
    // First frame: alpha is 0xFF everywhere.
    assert!(frame.rgba.chunks_exact(4).all(|c| c[3] == 0xFF));
}

#[test]
fn determinism_identical_input_yields_identical_framebuffers() {
    // The mock core uses thread-local state with no per-instance id (libretro
    // provides none), so we exercise determinism on a single core instance:
    // run a scripted sequence, capture the final frame; reset; run the same
    // sequence; the final frame must be byte-identical.
    let mut core = load_core();
    core.load_game(&[], None).unwrap();

    let run_sequence = |core: &mut Core| -> Vec<u8> {
        let mut last = Vec::new();
        for f in 0..30u32 {
            let input = if f == 10 {
                let mut btns = RetroPadButtonBits::EMPTY;
                btns.set(joypad::START);
                InputState {
                    buttons: btns,
                    ..Default::default()
                }
            } else {
                empty_input()
            };
            last = core.run_frame_required(input).unwrap().rgba;
        }
        last
    };

    let first = run_sequence(&mut core);
    core.reset().unwrap();
    let second = run_sequence(&mut core);
    assert_eq!(first, second, "determinism violated: reset+replay differs");
}

#[test]
fn save_state_round_trips() {
    let mut core = load_core();
    core.load_game(&[], None).unwrap();
    // Run a few frames to make state non-trivial.
    for _ in 0..10 {
        core.run_frame(empty_input()).unwrap();
    }
    let state = core.serialize().expect("serialize");
    let frame_after_a = core.run_frame_required(empty_input()).unwrap();

    // Restore and run the same next frame — must match.
    core.unserialize(&state).expect("unserialize");
    let frame_after_b = core.run_frame_required(empty_input()).unwrap();
    assert_eq!(frame_after_a.rgba, frame_after_b.rgba);
}

#[test]
fn save_state_size_is_stable() {
    let mut core = load_core();
    core.load_game(&[], None).unwrap();
    let s1 = core.serialize_size().unwrap();
    for _ in 0..5 {
        core.run_frame(empty_input()).unwrap();
    }
    let s2 = core.serialize_size().unwrap();
    assert_eq!(s1, s2);
    assert!(s1 > 0);
}

#[test]
fn save_ram_round_trips() {
    let mut core = load_core();
    core.load_game(&[], None).unwrap();
    let initial = core.memory(abi::MEMORY_SAVE_RAM).unwrap().unwrap();
    assert!(!initial.is_empty());

    let mut modified = initial.clone();
    modified[0] = 0xAB;
    modified[1] = 0xCD;
    core.set_memory(abi::MEMORY_SAVE_RAM, &modified).unwrap();

    let read_back = core.memory(abi::MEMORY_SAVE_RAM).unwrap().unwrap();
    assert_eq!(read_back[0], 0xAB);
    assert_eq!(read_back[1], 0xCD);
}

#[test]
fn input_state_is_visible_to_core() {
    // Pressing START should change the top-left pixel (mock-core marks it white).
    let mut core = load_core();
    core.load_game(&[], None).unwrap();

    let idle = core.run_frame_required(empty_input()).unwrap();
    let mut btns = RetroPadButtonBits::EMPTY;
    btns.set(joypad::START);
    let pressed = core
        .run_frame_required(InputState {
            buttons: btns,
            ..Default::default()
        })
        .unwrap();

    let idle_tl = &idle.rgba[0..3];
    let pressed_tl = &pressed.rgba[0..3];
    // The mock marks the first pixel white when START is held.
    assert_eq!(pressed_tl, [0xFF, 0xFF, 0xFF]);
    // Idle top-left should NOT be pure white (frame index 1, ramp).
    assert_ne!(idle_tl, [0xFF, 0xFF, 0xFF]);
}

#[test]
fn reset_returns_frame_count_to_zero() {
    let mut core = load_core();
    core.load_game(&[], None).unwrap();
    for _ in 0..5 {
        core.run_frame(empty_input()).unwrap();
    }
    core.reset().unwrap();
    let after_reset = core.run_frame_required(empty_input()).unwrap();
    // Frame index 1 after reset: first pixel = ((0+1)&0xFF)=1 for R, etc.
    assert_eq!(after_reset.rgba[0], 1);
}
