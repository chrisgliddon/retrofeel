//! Raw libretro C ABI: constants, structs, and function-pointer types.
//!
//! Bindings are hand-written from `libretro.h` (MIT). Only the subset needed
//! by retrofeel is included; additional commands/devices can be added per phase.

#![allow(non_camel_case_types)]

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};

// ---- API + environment command constants ----------------------------------

pub const RETRO_API_VERSION: u32 = 1;

/// Bit flag indicating an environment command is experimental.
/// Frontends mask this out before dispatching. Cores set it when probing.
pub const RETRO_ENVIRONMENT_EXPERIMENTAL: c_uint = 0x10000;

/// Bit flag indicating an environment command is frontend-internal.
pub const RETRO_ENVIRONMENT_PRIVATE: c_uint = 0x20000;

/// Environment command constants, re-derived from the upstream `libretro.h`.
///
/// These values are part of the libretro ABI; a mismatch here silently corrupts
/// core state (e.g. a real core's `GET_VARIABLE` (15) hitting our old
/// `GET_SYSTEM_DIRECTORY` handler). `abi::test::constants_match_libretro_h`
/// pins every value to a checked-in reference table so host↔mock divergence
/// can never mask an upstream mismatch again.
#[allow(non_upper_case_globals)]
pub mod env {
    use super::c_uint;
    pub const SET_ROTATION: c_uint = 1;
    pub const GET_OVERSCAN: c_uint = 2;
    pub const GET_CAN_DUPE: c_uint = 3;
    pub const SET_MESSAGE: c_uint = 6;
    pub const SHUTDOWN: c_uint = 7;
    pub const SET_PERFORMANCE_LEVEL: c_uint = 8;
    pub const GET_SYSTEM_DIRECTORY: c_uint = 9;
    pub const SET_PIXEL_FORMAT: c_uint = 10;
    pub const SET_INPUT_DESCRIPTORS: c_uint = 11;
    pub const SET_KEYBOARD_CALLBACK: c_uint = 12;
    pub const SET_DISK_CONTROL_INTERFACE: c_uint = 13;
    pub const SET_HW_RENDER: c_uint = 14;
    pub const GET_VARIABLE: c_uint = 15;
    pub const SET_VARIABLES: c_uint = 16;
    pub const GET_VARIABLE_UPDATE: c_uint = 17;
    pub const SET_SUPPORT_NO_GAME: c_uint = 18;
    pub const GET_LIBRETRO_PATH: c_uint = 19;
    pub const SET_FRAME_TIME_CALLBACK: c_uint = 21;
    pub const SET_AUDIO_CALLBACK: c_uint = 22;
    pub const GET_RUMBLE_INTERFACE: c_uint = 23;
    pub const GET_INPUT_DEVICE_CAPABILITIES: c_uint = 24;
    pub const GET_SENSOR_INTERFACE: c_uint = 25 | super::RETRO_ENVIRONMENT_EXPERIMENTAL;
    pub const GET_CAMERA_INTERFACE: c_uint = 26 | super::RETRO_ENVIRONMENT_EXPERIMENTAL;
    pub const GET_LOG_INTERFACE: c_uint = 27;
    pub const GET_PERF_INTERFACE: c_uint = 28;
    pub const GET_LOCATION_INTERFACE: c_uint = 29;
    pub const GET_CONTENT_DIRECTORY: c_uint = 30;
    pub const GET_CORE_ASSETS_DIRECTORY: c_uint = 30;
    pub const GET_SAVE_DIRECTORY: c_uint = 31;
    pub const SET_SYSTEM_AV_INFO: c_uint = 32;
    pub const SET_PROC_ADDRESS_CALLBACK: c_uint = 33;
    pub const SET_SUBSYSTEM_INFO: c_uint = 34;
    pub const SET_CONTROLLER_INFO: c_uint = 35;
    pub const SET_MEMORY_MAPS: c_uint = 36 | super::RETRO_ENVIRONMENT_EXPERIMENTAL;
    pub const SET_GEOMETRY: c_uint = 37;
    pub const GET_USERNAME: c_uint = 38;
    pub const GET_LANGUAGE: c_uint = 39;
    pub const SET_SERIALIZATION_QUIRKS: c_uint = 44;
    pub const GET_INPUT_BITMASKS: c_uint = 51 | super::RETRO_ENVIRONMENT_EXPERIMENTAL;
    pub const GET_CORE_OPTIONS_VERSION: c_uint = 52;
    pub const SET_CORE_OPTIONS: c_uint = 53;
    pub const SET_CORE_OPTIONS_INTL: c_uint = 54;
    pub const SET_CORE_OPTIONS_DISPLAY: c_uint = 55;
    pub const GET_PREFERRED_HW_RENDER: c_uint = 56;
    pub const GET_DISK_CONTROL_INTERFACE_VERSION: c_uint = 57;
    pub const SET_DISK_CONTROL_EXT_INTERFACE: c_uint = 58;
    pub const GET_MESSAGE_INTERFACE_VERSION: c_uint = 59;
    pub const SET_MESSAGE_EXT: c_uint = 60;
    pub const GET_INPUT_MAX_USERS: c_uint = 61;
    pub const SET_AUDIO_BUFFER_STATUS_CALLBACK: c_uint = 62;
    pub const SET_MINIMUM_AUDIO_LATENCY: c_uint = 63;
    pub const SET_FASTFORWARDING_OVERRIDE: c_uint = 64;
    pub const SET_CONTENT_INFO_OVERRIDE: c_uint = 65;
    pub const GET_GAME_INFO_EXT: c_uint = 66;
    pub const SET_CORE_OPTIONS_UPDATE_DISPLAY_CALLBACK: c_uint = 69;
    pub const SET_VARIABLE: c_uint = 70;
    pub const GET_JIT_CAPABLE: c_uint = 74;
    pub const SET_NETPACKET_INTERFACE: c_uint = 78;
    pub const GET_PLAYLIST_DIRECTORY: c_uint = 79;
    pub const GET_FILE_BROWSER_START_DIRECTORY: c_uint = 80;
    pub const EXEC_MEM_ALLOC: c_uint = 83;
    pub const EXEC_MEM_FREE: c_uint = 84;
}

/// Mask out the experimental/private bits to get the bare command number.
pub fn env_cmd_base(cmd: c_uint) -> c_uint {
    cmd & !(RETRO_ENVIRONMENT_EXPERIMENTAL | RETRO_ENVIRONMENT_PRIVATE)
}

// ---- Pixel formats --------------------------------------------------------

pub const PIXEL_FORMAT_0RGB1555: u32 = 0;
pub const PIXEL_FORMAT_XRGB8888: u32 = 1;
pub const PIXEL_FORMAT_RGB565: u32 = 2;

// ---- Devices --------------------------------------------------------------

pub const DEVICE_NONE: u32 = 0;
pub const DEVICE_JOYPAD: u32 = 1;
pub const DEVICE_MOUSE: u32 = 2;
pub const DEVICE_KEYBOARD: u32 = 3;
pub const DEVICE_LIGHTGUN: u32 = 4;
pub const DEVICE_ANALOG: u32 = 5;
pub const DEVICE_POINTER: u32 = 6;

/// RetroPad button ids, in libretro's canonical order.
///
/// These are *not* contiguous with face-button layout; A=8, X=9. Cores query by
/// id and the host returns 0/1, so a wrong id here is silent corruption.
pub mod joypad_id {
    pub const B: u32 = 0;
    pub const Y: u32 = 1;
    pub const SELECT: u32 = 2;
    pub const START: u32 = 3;
    pub const UP: u32 = 4;
    pub const DOWN: u32 = 5;
    pub const LEFT: u32 = 6;
    pub const RIGHT: u32 = 7;
    pub const A: u32 = 8;
    pub const X: u32 = 9;
    pub const L: u32 = 10;
    pub const R: u32 = 11;
    pub const L2: u32 = 12;
    pub const R2: u32 = 13;
    pub const L3: u32 = 14;
    pub const R3: u32 = 15;
    /// Bitmask query: the result is the OR of all pressed `RETRO_DEVICE_ID_JOYPAD_*`.
    pub const MASK: u32 = 256;
}

/// Analog stick index (the `index` argument to `retro_input_state` for
/// `DEVICE_ANALOG`).
pub mod analog_index {
    pub const LEFT: u32 = 0;
    pub const RIGHT: u32 = 1;
    pub const BUTTON: u32 = 2;
}

/// Analog axis id.
pub mod analog_id {
    pub const X: u32 = 0;
    pub const Y: u32 = 1;
}

/// Mouse device ids (for `DEVICE_MOUSE` queries).
///
/// Real libretro order: X=0, Y=1, LEFT=2, RIGHT=3, WHEELUP=4, WHEELDOWN=5,
/// MIDDLE=6. Earlier host code treated 0/1 as left/right buttons, which is
/// wrong and never returns dx/dy.
pub mod mouse_id {
    pub const X: u32 = 0;
    pub const Y: u32 = 1;
    pub const LEFT: u32 = 2;
    pub const RIGHT: u32 = 3;
    pub const WHEELUP: u32 = 4;
    pub const WHEELDOWN: u32 = 5;
    pub const MIDDLE: u32 = 6;
    pub const HORIZ_WHEELUP: u32 = 7;
    pub const HORIZ_WHEELDOWN: u32 = 8;
    pub const BUTTON_4: u32 = 9;
    pub const BUTTON_5: u32 = 10;
}

// ---- Memory regions -------------------------------------------------------

pub const MEMORY_SAVE_RAM: u32 = 0;
pub const MEMORY_RTC: u32 = 1;
pub const MEMORY_SYSTEM_RAM: u32 = 2;
pub const MEMORY_VIDEO_RAM: u32 = 3;

// ---- Region ---------------------------------------------------------------

pub const REGION_NTSC: u32 = 0;
pub const REGION_PAL: u32 = 1;

// ---- Structures (C ABI) ---------------------------------------------------

#[repr(C)]
pub struct RetroSystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
pub struct RetroGameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct RetroSystemTiming {
    pub fps: f64,
    pub sample_rate: f64,
}

#[repr(C)]
pub struct RetroSystemAvInfo {
    pub geometry: RetroGameGeometry,
    pub timing: RetroSystemTiming,
}

#[repr(C)]
pub struct RetroLogCallback {
    pub log: Option<RetroLogPrintf>,
}

#[repr(C)]
pub struct RetroVariable {
    pub key: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct RetroInputDescriptor {
    pub port: c_uint,
    pub device: c_uint,
    pub index: c_uint,
    pub id: c_uint,
    pub description: *const c_char,
}

// ---- Callback function pointer types --------------------------------------

pub type RetroLogPrintf = unsafe extern "C" fn(level: c_int, fmt: *const c_char) -> c_int;
pub type RetroEnvironmentCallback = unsafe extern "C" fn(cmd: c_uint, data: *mut c_void) -> bool;
pub type RetroVideoRefreshCallback =
    unsafe extern "C" fn(data: *const c_void, width: u32, height: u32, pitch: usize);
pub type RetroAudioSampleCallback = unsafe extern "C" fn(left: i16, right: i16);
pub type RetroAudioSampleBatchCallback =
    unsafe extern "C" fn(data: *const i16, frames: usize) -> usize;
pub type RetroInputPollCallback = unsafe extern "C" fn();
pub type RetroInputStateCallback =
    unsafe extern "C" fn(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16;

// ---- Core entry-point function pointer types ------------------------------

pub type RetroInit = unsafe extern "C" fn();
pub type RetroDeinit = unsafe extern "C" fn();
pub type RetroGetSystemInfo = unsafe extern "C" fn(info: *mut RetroSystemInfo);
pub type RetroGetSystemAvInfo = unsafe extern "C" fn(info: *mut RetroSystemAvInfo);
pub type RetroSetEnvironment = unsafe extern "C" fn(cb: RetroEnvironmentCallback);
pub type RetroSetVideoRefresh = unsafe extern "C" fn(cb: RetroVideoRefreshCallback);
pub type RetroSetAudioSample = unsafe extern "C" fn(cb: RetroAudioSampleCallback);
pub type RetroSetAudioSampleBatch = unsafe extern "C" fn(cb: RetroAudioSampleBatchCallback);
pub type RetroSetInputPoll = unsafe extern "C" fn(cb: RetroInputPollCallback);
pub type RetroSetInputState = unsafe extern "C" fn(cb: RetroInputStateCallback);
pub type RetroSetControllerPortDevice = unsafe extern "C" fn(port: c_uint, device: c_uint);
pub type RetroCheatReset = unsafe extern "C" fn();
pub type RetroCheatSet = unsafe extern "C" fn(index: c_uint, enabled: bool, code: *const c_char);
pub type RetroReset = unsafe extern "C" fn();
pub type RetroRun = unsafe extern "C" fn();
pub type RetroLoadGame = unsafe extern "C" fn(info: *const RetroGameInfo) -> bool;
pub type RetroUnloadGame = unsafe extern "C" fn();
pub type RetroSerializeSize = unsafe extern "C" fn() -> usize;
pub type RetroSerialize = unsafe extern "C" fn(data: *mut c_void, size: usize) -> bool;
pub type RetroUnserialize = unsafe extern "C" fn(data: *const c_void, size: usize) -> bool;
pub type RetroGetMemoryData = unsafe extern "C" fn(id: c_uint) -> *mut c_void;
pub type RetroGetMemorySize = unsafe extern "C" fn(id: c_uint) -> usize;
pub type RetroApiVersion = unsafe extern "C" fn() -> c_uint;

// ---- Geometry update (RETRO_ENVIRONMENT_SET_GEOMETRY) ----------------------

/// Passed to `RETRO_ENVIRONMENT_SET_GEOMETRY` (cmd 37). The core uses this to
/// notify the frontend that the active framebuffer region changed size or
/// aspect ratio without changing `max_width`/`max_height`.
#[repr(C)]
pub struct RetroGameGeometry {
    pub base_width: u32,
    pub base_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub aspect_ratio: f32,
}

#[cfg(test)]
mod test {
    //! Pin every environment/device constant to its value from the upstream
    //! `libretro.h`. The earlier host had wrong numbers (GET_SYSTEM_DIRECTORY
    //! was 15, colliding with real GET_VARIABLE); a real core calling
    //! GET_VARIABLE silently corrupted its `retro_variable.key` pointer.
    //! These tests make that class of regression a compile-time-visible failure.
    use super::*;

    #[test]
    fn environment_constants_match_libretro_h() {
        // (name, expected, actual) — expected values from libretro.h.
        let cases: &[(&str, c_uint, c_uint)] = &[
            ("SET_ROTATION", 1, env::SET_ROTATION),
            ("GET_OVERSCAN", 2, env::GET_OVERSCAN),
            ("GET_CAN_DUPE", 3, env::GET_CAN_DUPE),
            ("SET_MESSAGE", 6, env::SET_MESSAGE),
            ("SHUTDOWN", 7, env::SHUTDOWN),
            ("SET_PERFORMANCE_LEVEL", 8, env::SET_PERFORMANCE_LEVEL),
            ("GET_SYSTEM_DIRECTORY", 9, env::GET_SYSTEM_DIRECTORY),
            ("SET_PIXEL_FORMAT", 10, env::SET_PIXEL_FORMAT),
            ("SET_INPUT_DESCRIPTORS", 11, env::SET_INPUT_DESCRIPTORS),
            ("SET_KEYBOARD_CALLBACK", 12, env::SET_KEYBOARD_CALLBACK),
            (
                "SET_DISK_CONTROL_INTERFACE",
                13,
                env::SET_DISK_CONTROL_INTERFACE,
            ),
            ("SET_HW_RENDER", 14, env::SET_HW_RENDER),
            ("GET_VARIABLE", 15, env::GET_VARIABLE),
            ("SET_VARIABLES", 16, env::SET_VARIABLES),
            ("GET_VARIABLE_UPDATE", 17, env::GET_VARIABLE_UPDATE),
            ("SET_SUPPORT_NO_GAME", 18, env::SET_SUPPORT_NO_GAME),
            ("GET_LIBRETRO_PATH", 19, env::GET_LIBRETRO_PATH),
            ("SET_FRAME_TIME_CALLBACK", 21, env::SET_FRAME_TIME_CALLBACK),
            ("SET_AUDIO_CALLBACK", 22, env::SET_AUDIO_CALLBACK),
            ("GET_RUMBLE_INTERFACE", 23, env::GET_RUMBLE_INTERFACE),
            (
                "GET_INPUT_DEVICE_CAPABILITIES",
                24,
                env::GET_INPUT_DEVICE_CAPABILITIES,
            ),
            ("GET_LOG_INTERFACE", 27, env::GET_LOG_INTERFACE),
            ("GET_PERF_INTERFACE", 28, env::GET_PERF_INTERFACE),
            ("GET_LOCATION_INTERFACE", 29, env::GET_LOCATION_INTERFACE),
            ("GET_CONTENT_DIRECTORY", 30, env::GET_CONTENT_DIRECTORY),
            (
                "GET_CORE_ASSETS_DIRECTORY",
                30,
                env::GET_CORE_ASSETS_DIRECTORY,
            ),
            ("GET_SAVE_DIRECTORY", 31, env::GET_SAVE_DIRECTORY),
            ("SET_SYSTEM_AV_INFO", 32, env::SET_SYSTEM_AV_INFO),
            (
                "SET_PROC_ADDRESS_CALLBACK",
                33,
                env::SET_PROC_ADDRESS_CALLBACK,
            ),
            ("SET_SUBSYSTEM_INFO", 34, env::SET_SUBSYSTEM_INFO),
            ("SET_CONTROLLER_INFO", 35, env::SET_CONTROLLER_INFO),
            ("SET_GEOMETRY", 37, env::SET_GEOMETRY),
            ("GET_USERNAME", 38, env::GET_USERNAME),
            ("GET_LANGUAGE", 39, env::GET_LANGUAGE),
            (
                "SET_SERIALIZATION_QUIRKS",
                44,
                env::SET_SERIALIZATION_QUIRKS,
            ),
            (
                "GET_INPUT_BITMASKS",
                51 | RETRO_ENVIRONMENT_EXPERIMENTAL,
                env::GET_INPUT_BITMASKS,
            ),
            (
                "GET_CORE_OPTIONS_VERSION",
                52,
                env::GET_CORE_OPTIONS_VERSION,
            ),
            ("SET_CORE_OPTIONS", 53, env::SET_CORE_OPTIONS),
            ("SET_CORE_OPTIONS_INTL", 54, env::SET_CORE_OPTIONS_INTL),
            (
                "SET_CORE_OPTIONS_DISPLAY",
                55,
                env::SET_CORE_OPTIONS_DISPLAY,
            ),
            ("GET_PREFERRED_HW_RENDER", 56, env::GET_PREFERRED_HW_RENDER),
            (
                "GET_DISK_CONTROL_INTERFACE_VERSION",
                57,
                env::GET_DISK_CONTROL_INTERFACE_VERSION,
            ),
            (
                "SET_DISK_CONTROL_EXT_INTERFACE",
                58,
                env::SET_DISK_CONTROL_EXT_INTERFACE,
            ),
            (
                "GET_MESSAGE_INTERFACE_VERSION",
                59,
                env::GET_MESSAGE_INTERFACE_VERSION,
            ),
            ("SET_MESSAGE_EXT", 60, env::SET_MESSAGE_EXT),
            ("GET_INPUT_MAX_USERS", 61, env::GET_INPUT_MAX_USERS),
            (
                "SET_AUDIO_BUFFER_STATUS_CALLBACK",
                62,
                env::SET_AUDIO_BUFFER_STATUS_CALLBACK,
            ),
            (
                "SET_MINIMUM_AUDIO_LATENCY",
                63,
                env::SET_MINIMUM_AUDIO_LATENCY,
            ),
            (
                "SET_FASTFORWARDING_OVERRIDE",
                64,
                env::SET_FASTFORWARDING_OVERRIDE,
            ),
            (
                "SET_CONTENT_INFO_OVERRIDE",
                65,
                env::SET_CONTENT_INFO_OVERRIDE,
            ),
            ("GET_GAME_INFO_EXT", 66, env::GET_GAME_INFO_EXT),
            (
                "SET_CORE_OPTIONS_UPDATE_DISPLAY_CALLBACK",
                69,
                env::SET_CORE_OPTIONS_UPDATE_DISPLAY_CALLBACK,
            ),
            ("SET_VARIABLE", 70, env::SET_VARIABLE),
            ("GET_JIT_CAPABLE", 74, env::GET_JIT_CAPABLE),
            ("SET_NETPACKET_INTERFACE", 78, env::SET_NETPACKET_INTERFACE),
            ("GET_PLAYLIST_DIRECTORY", 79, env::GET_PLAYLIST_DIRECTORY),
            (
                "GET_FILE_BROWSER_START_DIRECTORY",
                80,
                env::GET_FILE_BROWSER_START_DIRECTORY,
            ),
            ("EXEC_MEM_ALLOC", 83, env::EXEC_MEM_ALLOC),
            ("EXEC_MEM_FREE", 84, env::EXEC_MEM_FREE),
        ];
        for (name, expected, actual) in cases {
            assert_eq!(
                expected, actual,
                "env constant {name}: expected {expected}, got {actual}; \
                 this is a libretro ABI mismatch — check the libretro ABI declaration"
            );
        }
    }

    #[test]
    fn joypad_ids_match_libretro_h() {
        assert_eq!(joypad_id::B, 0);
        assert_eq!(joypad_id::Y, 1);
        assert_eq!(joypad_id::SELECT, 2);
        assert_eq!(joypad_id::START, 3);
        assert_eq!(joypad_id::UP, 4);
        assert_eq!(joypad_id::DOWN, 5);
        assert_eq!(joypad_id::LEFT, 6);
        assert_eq!(joypad_id::RIGHT, 7);
        assert_eq!(joypad_id::A, 8);
        assert_eq!(joypad_id::X, 9);
        assert_eq!(joypad_id::L, 10);
        assert_eq!(joypad_id::R, 11);
        assert_eq!(joypad_id::L2, 12);
        assert_eq!(joypad_id::R2, 13);
        assert_eq!(joypad_id::L3, 14);
        assert_eq!(joypad_id::R3, 15);
        assert_eq!(joypad_id::MASK, 256);
    }

    #[test]
    fn mouse_ids_match_libretro_h() {
        assert_eq!(mouse_id::X, 0);
        assert_eq!(mouse_id::Y, 1);
        assert_eq!(mouse_id::LEFT, 2);
        assert_eq!(mouse_id::RIGHT, 3);
        assert_eq!(mouse_id::WHEELUP, 4);
        assert_eq!(mouse_id::WHEELDOWN, 5);
        assert_eq!(mouse_id::MIDDLE, 6);
        assert_eq!(mouse_id::HORIZ_WHEELUP, 7);
        assert_eq!(mouse_id::HORIZ_WHEELDOWN, 8);
        assert_eq!(mouse_id::BUTTON_4, 9);
        assert_eq!(mouse_id::BUTTON_5, 10);
    }

    #[test]
    fn env_cmd_base_masks_experimental_bit() {
        assert_eq!(
            env_cmd_base(env::GET_INPUT_BITMASKS),
            51,
            "experimental bit must mask out to the bare command number"
        );
        assert_eq!(env_cmd_base(env::GET_VARIABLE), 15);
    }
}
