#![allow(warnings)]

//! A minimal libretro core for testing `libretro-host` without external binaries.
//!
//! Implements the required libretro entry points and produces a deterministic
//! frame: a vertical color ramp that advances one pixel per `retro_run()` call,
//! so tests can verify both rendering and determinism.

#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::{c_char, c_uint, c_void, CStr};
use std::os::raw::{c_int, c_long, c_ulong};

// ---- libretro ABI surface (subset we implement) ----------------------------

pub const RETRO_API_VERSION: u32 = 1;
pub const RETRO_DEVICE_JOYPAD: u32 = 1;
pub const RETRO_DEVICE_NONE: u32 = 0;
pub const RETRO_PIXEL_FORMAT_0RGB1555: u32 = 0;
pub const RETRO_PIXEL_FORMAT_XRGB8888: u32 = 1;
pub const RETRO_PIXEL_FORMAT_RGB565: u32 = 2;
// Environment command constants — must match the upstream libretro.h.
// Earlier mock copies had GET_SYSTEM_DIRECTORY=15 and GET_VARIABLE=17, which
// collided with the host's wrong values and masked the ABI bug. With both
// sides now using the real libretro.h numbers, host↔mock agreement no longer
// hides a divergence from real cores.
pub const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: u32 = 10;
pub const RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY: u32 = 9;
pub const RETRO_ENVIRONMENT_SET_VARIABLES: u32 = 16;
pub const RETRO_ENVIRONMENT_GET_VARIABLE: u32 = 15;
pub const RETRO_ENVIRONMENT_GET_LOG_INTERFACE: u32 = 27;
pub const RETRO_MEMORY_SAVE_RAM: u32 = 0;

type RetroLogPrintf = extern "C" fn(level: i32, fmt: *const c_char) -> c_int;

#[repr(C)]
pub struct RetroLogCallback {
    pub log: Option<RetroLogPrintf>,
}

#[repr(C)]
pub struct RetroGameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct RetroSystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
pub struct RetroVariable {
    pub key: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct RetroSystemAvInfo {
    pub geometry: RetroGameGeometry,
    pub timing: RetroSystemTiming,
}

#[repr(C)]
pub struct RetroGameGeometry {
    pub base_width: u32,
    pub base_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub aspect_ratio: f32,
}

#[repr(C)]
pub struct RetroSystemTiming {
    pub fps: f64,
    pub sample_rate: f64,
}

struct CoreState {
    frame_count: u32,
    width: u32,
    height: u32,
    last_input: [i16; 16],
    save_ram: Vec<u8>,
    palette: Palette,
    log: Option<RetroLogPrintf>,
}

#[derive(Clone, Copy)]
enum Palette {
    Blue,
    Green,
    Red,
}

thread_local! {
    static STATE: RefCell<CoreState> = RefCell::new(CoreState {
        frame_count: 0,
        width: 256,
        height: 224,
        last_input: [0; 16],
        save_ram: vec![0u8; 256],
        palette: Palette::Blue,
        log: None,
    });
}

const MOCK_PALETTE_KEY: &[u8] = b"mock_palette\0";
const MOCK_PALETTE_VALUE: &[u8] = b"Palette; blue|green|red\0";

fn log(state: &CoreState, msg: &str) {
    if let Some(pf) = state.log {
        // Safety: we only pass a NUL-terminated string we own to a printf-like fn.
        let c = std::ffi::CString::new(msg).unwrap();
        unsafe { pf(0, c.as_ptr()) };
    }
}

// ---- Joypad button indices (subset) ---------------------------------------
// Real libretro ids: B=0, Y=1, SELECT=2, START=3, ..., A=8, X=9. Earlier mock
// code had BTN_A=1 (which is Y), unexercised by tests but wrong nonetheless.
const BTN_B: u32 = 0;
const BTN_A: u32 = 8;
const BTN_START: u32 = 3;

// ---- Exported libretro entry points ---------------------------------------

#[no_mangle]
pub unsafe extern "C" fn retro_api_version() -> c_uint {
    RETRO_API_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn retro_get_system_info(info: *mut RetroSystemInfo) {
    if info.is_null() {
        return;
    }
    let name = b"mock-core\0";
    let ver = b"0.1.0\0";
    let ext = b"bin|rom\0";
    unsafe {
        (*info).library_name = name.as_ptr() as *const c_char;
        (*info).library_version = ver.as_ptr() as *const c_char;
        (*info).valid_extensions = ext.as_ptr() as *const c_char;
        (*info).need_fullpath = false;
        (*info).block_extract = false;
    }
}

#[no_mangle]
pub unsafe extern "C" fn retro_get_system_av_info(info: *mut RetroSystemAvInfo) {
    if info.is_null() {
        return;
    }
    STATE.with(|s| {
        let st = s.borrow();
        unsafe {
            (*info).geometry = RetroGameGeometry {
                base_width: st.width,
                base_height: st.height,
                max_width: st.width,
                max_height: st.height,
                aspect_ratio: st.width as f32 / st.height as f32,
            };
            (*info).timing = RetroSystemTiming {
                fps: 60.0,
                sample_rate: 44100.0,
            };
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_environment(cb: RetroEnvironmentCallback) {
    ENV_CB.with(|c| *c.borrow_mut() = Some(cb));
    // Request XRGB8888 pixel format.
    let mut pixel: u32 = RETRO_PIXEL_FORMAT_XRGB8888;
    STATE.with(|s| {
        let st = s.borrow();
        if let Some(env) = ENV_CB.with(|c| *c.borrow()) {
            unsafe {
                env(
                    RETRO_ENVIRONMENT_SET_PIXEL_FORMAT as u32,
                    &mut pixel as *mut u32 as *mut c_void,
                );
            }
            let variables = [
                RetroVariable {
                    key: MOCK_PALETTE_KEY.as_ptr() as *const c_char,
                    value: MOCK_PALETTE_VALUE.as_ptr() as *const c_char,
                },
                RetroVariable {
                    key: std::ptr::null(),
                    value: std::ptr::null(),
                },
            ];
            unsafe {
                env(
                    RETRO_ENVIRONMENT_SET_VARIABLES,
                    variables.as_ptr() as *mut c_void,
                );
            }
            let _ = st;
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_video_refresh(cb: RetroVideoRefreshCallback) {
    VIDEO_CB.with(|c| *c.borrow_mut() = Some(cb));
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_audio_sample(cb: RetroAudioSampleCallback) {
    AUDIO_SAMPLE_CB.with(|c| *c.borrow_mut() = Some(cb));
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_audio_sample_batch(cb: RetroAudioSampleBatchCallback) {
    AUDIO_BATCH_CB.with(|c| *c.borrow_mut() = Some(cb));
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_input_poll(_cb: RetroInputPollCallback) {
    // No-op; we don't need poll notifications for the mock.
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_input_state(cb: RetroInputStateCallback) {
    INPUT_STATE_CB.with(|c| *c.borrow_mut() = Some(cb));
}

#[no_mangle]
pub unsafe extern "C" fn retro_init() {
    // Opt-in process-isolation fixtures. Real native cores may terminate or
    // abort during initialization; these let the GUI worker smoke tests prove
    // that either failure is contained in the child process.
    if std::env::var_os("RETROFEEL_MOCK_EXIT_ON_INIT").is_some() {
        std::process::exit(86);
    }
    if std::env::var_os("RETROFEEL_MOCK_ABORT_ON_INIT").is_some() {
        std::process::abort();
    }
    // Reset state so each Core::load (which calls retro_init) starts clean,
    // even though TLS is shared across dlopen handles on some platforms.
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.frame_count = 0;
        st.last_input = [0; 16];
        st.save_ram.iter_mut().for_each(|b| *b = 0);
        st.palette = Palette::Blue;
    });
}

#[no_mangle]
pub unsafe extern "C" fn retro_deinit() {}

#[no_mangle]
pub unsafe extern "C" fn retro_set_controller_port_device(_port: c_uint, _device: c_uint) {}

#[no_mangle]
pub unsafe extern "C" fn retro_cheat_reset() {}

#[no_mangle]
pub unsafe extern "C" fn retro_cheat_set(_index: c_uint, _enabled: bool, _code: *const c_char) {}

#[no_mangle]
pub unsafe extern "C" fn retro_reset() {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.frame_count = 0;
        st.last_input = [0; 16];
    });
}

#[no_mangle]
pub unsafe extern "C" fn retro_load_game(info: *const RetroGameInfo) -> bool {
    // Accept any/no content.
    let _ = info;
    if let Some(env) = ENV_CB.with(|c| *c.borrow()) {
        let mut variable = RetroVariable {
            key: MOCK_PALETTE_KEY.as_ptr() as *const c_char,
            value: std::ptr::null(),
        };
        let ok = unsafe {
            env(
                RETRO_ENVIRONMENT_GET_VARIABLE,
                &mut variable as *mut RetroVariable as *mut c_void,
            )
        };
        if ok && !variable.value.is_null() {
            let value = unsafe { CStr::from_ptr(variable.value) }.to_string_lossy();
            STATE.with(|s| {
                s.borrow_mut().palette = match value.as_ref() {
                    "green" => Palette::Green,
                    "red" => Palette::Red,
                    _ => Palette::Blue,
                };
            });
        }
    }
    true
}

#[no_mangle]
pub unsafe extern "C" fn retro_unload_game() {}

#[no_mangle]
pub unsafe extern "C" fn retro_run() {
    // Poll input: read A and START so tests can exercise input state.
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        if let (Some(env), Some(state_cb)) = (
            ENV_CB.with(|c| *c.borrow()),
            INPUT_STATE_CB.with(|c| *c.borrow()),
        ) {
            let _ = env;
            for (i, slot) in st.last_input.iter_mut().enumerate() {
                *slot = unsafe { state_cb(0, RETRO_DEVICE_JOYPAD, 0, i as c_uint) };
            }
        }
        st.frame_count = st.frame_count.wrapping_add(1);
    });

    // Render: paint a frame whose first pixel encodes frame_count, so two runs
    // with identical input produce byte-identical buffers.
    STATE.with(|s| {
        let st = s.borrow();
        let w = st.width as usize;
        let h = st.height as usize;
        let frame = st.frame_count;
        let pressed_a = st.last_input[BTN_A as usize] != 0;
        let pressed_start = st.last_input[BTN_START as usize] != 0;
        let pressed_b = st.last_input[BTN_B as usize] != 0;

        let mut buf = vec![0u32; w * h];
        for (i, px) in buf.iter_mut().enumerate() {
            let x = (i % w) as u32;
            let y = (i / w) as u32;
            let mut r = ((x + frame) & 0xFF) as u32;
            let mut g = ((y + frame) & 0xFF) as u32;
            let mut b = ((x ^ y) & 0xFF) as u32;
            match st.palette {
                Palette::Blue => b = 0xEE,
                Palette::Green => g = 0xEE,
                Palette::Red => r = 0xEE,
            }
            if pressed_a {
                r = 0xFF;
            }
            if pressed_b {
                g = 0xFF;
            }
            if pressed_start && i == 0 {
                r = 0xFF;
                g = 0xFF;
                b = 0xFF;
            }
            *px = (0xFF << 24) | (r << 16) | (g << 8) | b;
        }

        if let Some(vcb) = VIDEO_CB.with(|c| *c.borrow()) {
            unsafe {
                vcb(
                    buf.as_ptr() as *const c_void,
                    st.width,
                    st.height,
                    (st.width * 4) as usize,
                );
            }
        }
    });

    // Emit a short deterministic tone so audio paths get exercised.
    // One frame at 60 fps / 44100 Hz = 735 samples per channel.
    if let Some(batch) = AUDIO_BATCH_CB.with(|c| *c.borrow()) {
        let n = (44100.0 / 60.0) as usize;
        let mut samples = vec![0i16; n * 2];
        STATE.with(|s| {
            let st = s.borrow();
            let f = st.frame_count;
            for i in 0..n {
                // 440 Hz square wave at low volume, frame-indexed phase.
                let phase = (f as usize * i) / 100;
                let val: i16 = if phase % 2 == 0 { 1024 } else { -1024 };
                samples[i * 2] = val;
                samples[i * 2 + 1] = val;
            }
        });
        unsafe {
            batch(samples.as_ptr(), n);
        }
    } else if let Some(sample_cb) = AUDIO_SAMPLE_CB.with(|c| *c.borrow()) {
        STATE.with(|s| {
            let st = s.borrow();
            let phase = st.frame_count as usize;
            let val: i16 = if phase % 2 == 0 { 1024 } else { -1024 };
            unsafe {
                sample_cb(val, val);
            }
        });
    }
}

#[no_mangle]
pub unsafe extern "C" fn retro_serialize_size() -> usize {
    STATE.with(|s| {
        let st = s.borrow();
        // frame_count (4) + last_input (16 * 2 = 32) = 36
        4 + st.last_input.len() * 2
    })
}

#[no_mangle]
pub unsafe extern "C" fn retro_serialize(data: *mut c_void, size: usize) -> bool {
    STATE.with(|s| {
        let st = s.borrow();
        let needed = 4 + st.last_input.len() * 2;
        if size < needed || data.is_null() {
            return false;
        }
        unsafe {
            let bytes = data as *mut u8;
            std::ptr::copy_nonoverlapping(&st.frame_count as *const u32 as *const u8, bytes, 4);
            std::ptr::copy_nonoverlapping(
                st.last_input.as_ptr() as *const u8,
                bytes.add(4),
                st.last_input.len() * 2,
            );
        }
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn retro_unserialize(data: *const c_void, size: usize) -> bool {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        let needed = 4 + st.last_input.len() * 2;
        if size < needed || data.is_null() {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                data as *const u8,
                &mut st.frame_count as *mut u32 as *mut u8,
                4,
            );
            std::ptr::copy_nonoverlapping(
                (data as *const u8).add(4),
                st.last_input.as_mut_ptr() as *mut u8,
                st.last_input.len() * 2,
            );
        }
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn retro_get_memory_data(id: c_uint) -> *mut c_void {
    if id != RETRO_MEMORY_SAVE_RAM {
        return std::ptr::null_mut();
    }
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.save_ram.as_mut_ptr() as *mut c_void
    })
}

#[no_mangle]
pub unsafe extern "C" fn retro_get_memory_size(id: c_uint) -> usize {
    if id != RETRO_MEMORY_SAVE_RAM {
        return 0;
    }
    STATE.with(|s| s.borrow().save_ram.len())
}

// ---- Callback type aliases (must match the host's view) -------------------

type RetroEnvironmentCallback = extern "C" fn(cmd: u32, data: *mut c_void) -> bool;
type RetroVideoRefreshCallback =
    extern "C" fn(data: *const c_void, width: u32, height: u32, pitch: usize);
type RetroAudioSampleCallback = extern "C" fn(left: i16, right: i16);
type RetroAudioSampleBatchCallback = extern "C" fn(data: *const i16, frames: usize) -> usize;
type RetroInputPollCallback = extern "C" fn();
type RetroInputStateCallback =
    extern "C" fn(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16;

thread_local! {
    static ENV_CB: RefCell<Option<RetroEnvironmentCallback>> = const { RefCell::new(None) };
    static VIDEO_CB: RefCell<Option<RetroVideoRefreshCallback>> = const { RefCell::new(None) };
    static INPUT_STATE_CB: RefCell<Option<RetroInputStateCallback>> = const { RefCell::new(None) };
    static AUDIO_SAMPLE_CB: RefCell<Option<RetroAudioSampleCallback>> = const { RefCell::new(None) };
    static AUDIO_BATCH_CB: RefCell<Option<RetroAudioSampleBatchCallback>> = const { RefCell::new(None) };
}

// Allow the host to register the video + input callbacks after load via a
// side channel — libretro normally sets these through retro_set_* but our
// host installs them through the same symbols, so the RefCells above get
// populated automatically. Nothing extra needed.
//
// Suppress unused warnings for the c_long/c_ulong imports kept for ABI parity.
const _: fn() = || {
    let _x: c_long = 0;
    let _y: c_ulong = 0;
};
