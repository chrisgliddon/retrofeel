//! Frontend context: the shared state callbacks read/write during `retro_run`.
//!
//! libretro callbacks are plain `extern "C" fn`s (no user data), so we route
//! them through a thread-local slot holding the active [`FrontendContext`].
//! This matches the "core runs on a dedicated thread" design from the plan:
//! each thread that calls into a core gets its own slot.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::Arc;

use crate::abi;
use crate::pixel::Frame;
use retrofeel_types::InputState;

/// Output of a single `retro_run()` call, captured from the callbacks.
pub struct RunOutput {
    pub frame: Option<Frame>,
    /// Stereo interleaved PCM samples (L,R,L,R,...) at the core's sample rate.
    pub audio: Vec<i16>,
}

/// A core-defined option (`retro_variable`).
#[derive(Debug, Clone)]
pub struct CoreVariable {
    pub key: String,
    pub option_text: String,
}

/// Frontend context installed for the duration of a `retro_run()` / env probe.
pub struct FrontendContext {
    pub pixel_format: std::cell::Cell<u32>,
    pub system_dir: CString,
    pub save_dir: CString,
    pub core_library_path: CString,
    /// Variables the core declared via SET_VARIABLES.
    pub variables: std::cell::RefCell<Vec<CoreVariable>>,
    /// Current option values (key → value).
    pub options: std::cell::RefCell<BTreeMap<String, String>>,
    pub variables_dirty: std::cell::Cell<bool>,
    /// The input state fed to the core this frame.
    pub input: std::cell::RefCell<InputState>,
    /// The frame produced this run (None until video_refresh fires).
    pub frame: std::cell::RefCell<Option<Frame>>,
    /// Audio produced this run.
    pub audio: std::cell::RefCell<Vec<i16>>,
    pub log: Option<unsafe extern "C" fn(c_int, *const c_char, ...) -> c_int>,
    /// Whether the core can run without content (`SET_SUPPORT_NO_GAME`).
    pub support_no_game: std::cell::Cell<bool>,
    /// Most recent geometry reported by `SET_GEOMETRY` / `SET_SYSTEM_AV_INFO`.
    /// Cores like PS1 change resolution mid-game; the app reads this each frame.
    pub geometry: std::cell::RefCell<Option<GeometryUpdate>>,
    /// Whether `SET_SYSTEM_AV_INFO` has requested a timing change (new fps /
    /// sample rate). The core thread reads this after each `retro_run`.
    pub av_info_changed: std::cell::Cell<bool>,
    /// Set when the core requests frontend shutdown (environment command 7).
    pub shutdown_requested: std::cell::Cell<bool>,
}

/// A geometry/timing update broadcast by the core mid-session.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeometryUpdate {
    pub base_width: u32,
    pub base_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub aspect_ratio: f32,
    pub fps: Option<f64>,
    pub sample_rate: Option<f64>,
}

impl FrontendContext {
    pub fn new(system_dir: &str, core_library_path: &str) -> Arc<Self> {
        Self::new_with_save_dir(system_dir, system_dir, core_library_path)
    }

    /// Build a context with an explicit save directory (distinct from the
    /// BIOS/system dir per libretro's recommendation).
    pub fn new_with_save_dir(
        system_dir: &str,
        save_dir: &str,
        core_library_path: &str,
    ) -> Arc<Self> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(Self {
            pixel_format: std::cell::Cell::new(abi::PIXEL_FORMAT_0RGB1555),
            system_dir: CString::new(system_dir).unwrap_or_default(),
            save_dir: CString::new(save_dir).unwrap_or_default(),
            core_library_path: CString::new(core_library_path).unwrap_or_default(),
            variables: Default::default(),
            options: Default::default(),
            variables_dirty: std::cell::Cell::new(false),
            input: std::cell::RefCell::new(InputState::default()),
            frame: std::cell::RefCell::new(None),
            audio: std::cell::RefCell::new(Vec::new()),
            log: None,
            support_no_game: std::cell::Cell::new(false),
            geometry: std::cell::RefCell::new(None),
            av_info_changed: std::cell::Cell::new(false),
            shutdown_requested: std::cell::Cell::new(false),
        })
    }

    /// Reset per-frame buffers before a `retro_run()`.
    pub fn reset_run(&self, input: InputState) {
        *self.input.borrow_mut() = input;
        *self.frame.borrow_mut() = None;
        self.audio.borrow_mut().clear();
    }

    pub fn set_option(&self, key: &str, value: &str) {
        self.options
            .borrow_mut()
            .insert(key.to_string(), value.to_string());
        self.variables_dirty.set(true);
    }
}

thread_local! {
    /// The context active on this thread. Installed by [`Core::run_frame`]
    /// and by env probes. Callbacks below read it.
    pub(crate) static CURRENT: RefCell<Option<Arc<FrontendContext>>> = const { RefCell::new(None) };
}

#[allow(dead_code)]
pub(crate) fn with_ctx<R>(f: impl FnOnce(&FrontendContext) -> R) -> R {
    CURRENT.with(|c| {
        let opt = c.borrow();
        let ctx = opt
            .as_ref()
            .expect("no active FrontendContext on this thread");
        f(ctx)
    })
}

// ---- Static callbacks installed into the core ------------------------------

pub unsafe extern "C" fn cb_environment(cmd: c_uint, data: *mut c_void) -> bool {
    let ctx = match CURRENT.with(|c| c.borrow().as_ref().map(Arc::clone)) {
        Some(c) => c,
        None => return false,
    };
    // Mask out the experimental/private bits; cores set them when probing and
    // frontends must dispatch on the bare command number.
    match abi::env_cmd_base(cmd) {
        abi::env::SHUTDOWN => {
            ctx.shutdown_requested.set(true);
            true
        }
        abi::env::SET_PIXEL_FORMAT => {
            if data.is_null() {
                return false;
            }
            let pf = unsafe { *(data as *const u32) };
            ctx.pixel_format.set(pf);
            true
        }
        abi::env::GET_SYSTEM_DIRECTORY => {
            if data.is_null() {
                return false;
            }
            unsafe {
                *(data as *mut *const c_char) = ctx.system_dir.as_ptr();
            }
            true
        }
        abi::env::GET_SAVE_DIRECTORY => {
            if data.is_null() {
                return false;
            }
            unsafe {
                *(data as *mut *const c_char) = ctx.save_dir.as_ptr();
            }
            true
        }
        abi::env::GET_LIBRETRO_PATH => {
            if data.is_null() {
                return false;
            }
            unsafe {
                *(data as *mut *const c_char) = ctx.core_library_path.as_ptr();
            }
            true
        }
        abi::env::GET_LOG_INTERFACE => {
            if data.is_null() {
                return false;
            }
            unsafe {
                let cb = data as *mut abi::RetroLogCallback;
                (*cb).log = Some(log_proxy);
            }
            true
        }
        abi::env::SET_VARIABLES => {
            if data.is_null() {
                return false;
            }
            // Array of RetroVariable terminated by a null key.
            let mut vars = ctx.variables.borrow_mut();
            vars.clear();
            let mut ptr = data as *const abi::RetroVariable;
            unsafe {
                while !(*ptr).key.is_null() {
                    let key = CStr::from_ptr((*ptr).key).to_string_lossy().to_string();
                    let val = if (*ptr).value.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr((*ptr).value).to_string_lossy().to_string()
                    };
                    vars.push(CoreVariable {
                        key: key.clone(),
                        option_text: val.clone(),
                    });
                    if let Some(default_value) = default_variable_value(&val) {
                        ctx.options.borrow_mut().entry(key).or_insert(default_value);
                    }
                    ptr = ptr.add(1);
                }
            }
            true
        }
        abi::env::GET_VARIABLE => {
            if data.is_null() {
                return false;
            }
            let v = unsafe { &mut *(data as *mut abi::RetroVariable) };
            if v.key.is_null() {
                return false;
            }
            let key = unsafe { CStr::from_ptr(v.key) }
                .to_string_lossy()
                .to_string();
            let opts = ctx.options.borrow();
            let value = opts.get(&key).cloned();
            if let Some(value) = value {
                // Write back through the caller's pointer (they own the buffer).
                // Per libretro convention, the frontend copies into the provided
                // buffer. We allocate a CString and hand the pointer; the core
                // reads it synchronously during the env call.
                if let Ok(c) = CString::new(value) {
                    // The returned pointer is backed by a thread-local slab.
                    v.value = leaked_cstr_ptr(c);
                    return true;
                }
            }
            false
        }
        abi::env::GET_VARIABLE_UPDATE => {
            let dirty = ctx.variables_dirty.get();
            ctx.variables_dirty.set(false);
            if data.is_null() {
                return dirty;
            }
            unsafe {
                *(data as *mut bool) = dirty;
            }
            true
        }
        abi::env::GET_CAN_DUPE => {
            // We support NULL-frame dupes (the pixel converter returns the
            // previous frame). Cores query this to decide whether they can
            // skip the video_refresh call on unchanged frames.
            if data.is_null() {
                return true;
            }
            unsafe {
                *(data as *mut bool) = true;
            }
            true
        }
        abi::env::SET_SUPPORT_NO_GAME => {
            if data.is_null() {
                return false;
            }
            let v = unsafe { *(data as *const bool) };
            ctx.support_no_game.set(v);
            true
        }
        abi::env::SET_SYSTEM_AV_INFO => {
            if data.is_null() {
                return false;
            }
            let info = unsafe { &*(data as *const abi::RetroSystemAvInfo) };
            *ctx.geometry.borrow_mut() = Some(GeometryUpdate {
                base_width: info.geometry.base_width,
                base_height: info.geometry.base_height,
                max_width: info.geometry.max_width,
                max_height: info.geometry.max_height,
                aspect_ratio: info.geometry.aspect_ratio,
                fps: Some(info.timing.fps),
                sample_rate: Some(info.timing.sample_rate),
            });
            ctx.av_info_changed.set(true);
            true
        }
        abi::env::SET_GEOMETRY => {
            if data.is_null() {
                return false;
            }
            let g = unsafe { &*(data as *const abi::RetroGameGeometry) };
            *ctx.geometry.borrow_mut() = Some(GeometryUpdate {
                base_width: g.base_width,
                base_height: g.base_height,
                max_width: g.max_width,
                max_height: g.max_height,
                aspect_ratio: g.aspect_ratio,
                fps: None,
                sample_rate: None,
            });
            true
        }
        // Known but intentionally unhandled: SET_INPUT_DESCRIPTORS (we have no
        // binding UI hint surface yet), SET_KEYBOARD_CALLBACK (we feed keyboard
        // state via input_state), SET_FRAME_TIME_CALLBACK (the frame is the
        // clock), GET_RUMBLE_INTERFACE (no rumble output), SET_HW_RENDER
        // (software-rendered cores only — stretch S3). Returning false for
        // these matches the libretro contract: the core falls back gracefully.
        _ => false,
    }
}

/// Hand out a pointer to a CString stored in a thread-local slab so it lives
/// long enough for the core to read during the env callback, without a true
/// leak. Cleared on the next call.
fn leaked_cstr_ptr(c: CString) -> *const c_char {
    thread_local! {
        static SLAB: RefCell<Vec<CString>> = const { RefCell::new(Vec::new()) };
    }
    SLAB.with(|s| {
        let mut slab = s.borrow_mut();
        slab.clear();
        slab.push(c);
        slab[0].as_ptr()
    })
}

fn default_variable_value(option_text: &str) -> Option<String> {
    let values = option_text
        .split_once(';')
        .map(|(_, values)| values)
        .unwrap_or(option_text);
    values
        .split('|')
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

unsafe extern "C" fn log_proxy(level: c_int, fmt: *const c_char) -> c_int {
    if fmt.is_null() {
        return 0;
    }
    let msg = unsafe { CStr::from_ptr(fmt) }.to_string_lossy();
    // `retro_log_printf_t` is variadic. Stable Rust cannot define a C-variadic
    // callback, so only forward already-formatted/literal messages; otherwise
    // we'd emit the raw template (mGBA uses "%s: %s" on every frame).
    if msg.contains('%') {
        return 0;
    }
    log::log!(
        match level {
            0 => log::Level::Debug,
            1 => log::Level::Info,
            2 => log::Level::Warn,
            _ => log::Level::Error,
        },
        "core: {}",
        msg
    );
    0
}

pub unsafe extern "C" fn cb_video_refresh(
    data: *const c_void,
    width: u32,
    height: u32,
    pitch: usize,
) {
    let ctx = match CURRENT.with(|c| c.borrow().as_ref().map(Arc::clone)) {
        Some(c) => c,
        None => return,
    };
    let pf = ctx.pixel_format.get();
    match crate::pixel::convert(data, width, height, pitch, pf) {
        Ok(frame) => *ctx.frame.borrow_mut() = Some(frame),
        Err(_) => { /* FrameRepeat: leave previous frame in place */ }
    }
}

pub unsafe extern "C" fn cb_audio_sample(left: i16, right: i16) {
    let ctx = match CURRENT.with(|c| c.borrow().as_ref().map(Arc::clone)) {
        Some(c) => c,
        None => return,
    };
    ctx.audio.borrow_mut().push(left);
    ctx.audio.borrow_mut().push(right);
}

pub unsafe extern "C" fn cb_audio_sample_batch(data: *const i16, frames: usize) -> usize {
    let ctx = match CURRENT.with(|c| c.borrow().as_ref().map(Arc::clone)) {
        Some(c) => c,
        None => return 0,
    };
    if data.is_null() {
        return 0;
    }
    let samples = unsafe { std::slice::from_raw_parts(data, frames * 2) };
    ctx.audio.borrow_mut().extend_from_slice(samples);
    frames
}

pub unsafe extern "C" fn cb_input_poll() {
    // Nothing to do; we feed input state synchronously from the context.
}

pub unsafe extern "C" fn cb_input_state(
    port: c_uint,
    device: c_uint,
    index: c_uint,
    id: c_uint,
) -> i16 {
    let ctx = match CURRENT.with(|c| c.borrow().as_ref().map(Arc::clone)) {
        Some(c) => c,
        None => return 0,
    };
    if port != 0 {
        return 0;
    }
    let input = ctx.input.borrow();
    match device {
        abi::DEVICE_JOYPAD => {
            // libretro allows two query shapes: a single button id (0..15) or
            // RETRO_DEVICE_ID_JOYPAD_MASK (256), which returns a bitmask of all
            // pressed buttons at once. The latter requires the frontend to
            // opt in via GET_INPUT_BITMASKS; we report it regardless so cores
            // that probe it directly still work.
            if id == abi::joypad_id::MASK {
                input.buttons.0 as i16
            } else if id < 16 {
                if input.buttons.has(id as u16) {
                    1
                } else {
                    0
                }
            } else {
                0
            }
        }
        abi::DEVICE_ANALOG => {
            // index 0 = left stick, 1 = right stick, 2 = analog button (trigger);
            // id 0 = X, 1 = Y. Triggers live under index 2.
            match (index, id) {
                (abi::analog_index::LEFT, abi::analog_id::X) => input.analog_l.x,
                (abi::analog_index::LEFT, abi::analog_id::Y) => input.analog_l.y,
                (abi::analog_index::RIGHT, abi::analog_id::X) => input.analog_r.x,
                (abi::analog_index::RIGHT, abi::analog_id::Y) => input.analog_r.y,
                (abi::analog_index::BUTTON, abi::joypad_id::L2) => input.triggers.l,
                (abi::analog_index::BUTTON, abi::joypad_id::R2) => input.triggers.r,
                _ => 0,
            }
        }
        abi::DEVICE_MOUSE => {
            // Real libretro mouse ids: X=0, Y=1, LEFT=2, RIGHT=3, WHEELUP=4,
            // WHEELDOWN=5, MIDDLE=6, horizontal wheel=7/8. Earlier host code
            // treated 0/1 as buttons, which never returned dx/dy and
            // mislabeled the buttons.
            match id {
                abi::mouse_id::X => input.mouse.dx as i16,
                abi::mouse_id::Y => input.mouse.dy as i16,
                abi::mouse_id::LEFT => (input.mouse.buttons & 1) as i16,
                abi::mouse_id::RIGHT => ((input.mouse.buttons >> 1) & 1) as i16,
                abi::mouse_id::MIDDLE => ((input.mouse.buttons >> 2) & 1) as i16,
                // Wheel events are pulses. New recordings carry an explicit
                // signed delta; bits 3/4 remain a compatibility fallback for
                // older producers.
                abi::mouse_id::WHEELUP => {
                    i16::from(input.mouse.wheel_y > 0 || input.mouse.buttons & (1 << 3) != 0)
                }
                abi::mouse_id::WHEELDOWN => {
                    i16::from(input.mouse.wheel_y < 0 || input.mouse.buttons & (1 << 4) != 0)
                }
                abi::mouse_id::HORIZ_WHEELUP => i16::from(input.mouse.wheel_x > 0),
                abi::mouse_id::HORIZ_WHEELDOWN => i16::from(input.mouse.wheel_x < 0),
                _ => 0,
            }
        }
        abi::DEVICE_KEYBOARD => {
            // Sparse: return 1 if the scancode is held.
            let key = id as i32;
            let input = ctx.input.borrow();
            if input.keyboard.keys[..input.keyboard.count as usize].contains(&key) {
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}
