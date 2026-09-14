//! Safe wrapper over a loaded libretro core.
//!
//! One [`Core`] = one dlopen'd `.so`/`.dll`/`.dylib` plus its frontend context.
//! Cores are not `Send`/`Sync` by default (libretro cores are single-threaded),
//! so [`Core`] lives on the thread that created it. The Phase 2 design will run
//! each core on a dedicated thread; here we simply guard against cross-thread
//! use with a runtime check.

use std::ffi::{CStr, CString};
use std::os::raw::{c_uint, c_void};
use std::path::Path;
use std::sync::Arc;

use libloading::{Library, Symbol};
use retrofeel_types::InputState;
use thiserror::Error;

use crate::abi;
use crate::context::{self, FrontendContext, RunOutput};
use crate::pixel::Frame;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("failed to load core library {path}: {source}")]
    Load {
        path: String,
        #[source]
        source: libloading::Error,
    },
    #[error("missing required libretro symbol {0}")]
    MissingSymbol(&'static str),
    #[error("core reported unsupported API version {0}")]
    BadApiVersion(u32),
    #[error("core failed to load game")]
    LoadGameFailed,
    #[error("save state size mismatch: expected {expected}, got {got}")]
    SerializeSize { expected: usize, got: usize },
    #[error("save state round-trip failed")]
    SerializeRoundTrip,
    #[error("no frame produced by retro_run (core did not call video_refresh)")]
    NoFrame,
    #[error("core used from a different thread than it was created on")]
    WrongThread,
    #[error("cheat code contains an embedded NUL byte")]
    InvalidCheatCode,
}

/// Optional libretro features exposed by a loaded core/frontend pairing.
/// Callers must gate controls on these flags instead of assuming every core
/// implements every part of the ABI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreCapabilities {
    pub save_states: bool,
    pub cheats: bool,
    pub controller_port_devices: bool,
    pub disk_control: bool,
    pub rumble: bool,
    pub hardware_render: bool,
}

/// Static info from `retro_get_system_info`.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub library_name: String,
    pub library_version: String,
    pub valid_extensions: Vec<String>,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

/// A/V info from `retro_get_system_av_info`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAvInfo {
    pub base_width: u32,
    pub base_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub aspect_ratio: f32,
    pub fps: f64,
    pub sample_rate: f64,
}

/// A loaded libretro core.
pub struct Core {
    _lib: Library,
    system_info: SystemInfo,
    av_info: SystemAvInfo,
    ctx: Arc<FrontendContext>,
    thread_id: std::thread::ThreadId,
    // Function pointers held for the core's lifetime.
    retro_run: Symbol<'static, abi::RetroRun>,
    retro_reset: Symbol<'static, abi::RetroReset>,
    retro_load_game: Symbol<'static, abi::RetroLoadGame>,
    retro_unload_game: Symbol<'static, abi::RetroUnloadGame>,
    retro_serialize_size: Symbol<'static, abi::RetroSerializeSize>,
    retro_serialize: Symbol<'static, abi::RetroSerialize>,
    retro_unserialize: Symbol<'static, abi::RetroUnserialize>,
    retro_get_memory_data: Symbol<'static, abi::RetroGetMemoryData>,
    retro_get_memory_size: Symbol<'static, abi::RetroGetMemorySize>,
    retro_get_system_av_info: Symbol<'static, abi::RetroGetSystemAvInfo>,
    retro_deinit: Option<Symbol<'static, abi::RetroDeinit>>,
    retro_cheat_reset: Option<Symbol<'static, abi::RetroCheatReset>>,
    retro_cheat_set: Option<Symbol<'static, abi::RetroCheatSet>>,
    retro_set_controller_port_device: Option<Symbol<'static, abi::RetroSetControllerPortDevice>>,
}

impl Core {
    /// Probe a core's static info without initializing it.
    ///
    /// Calls only `retro_api_version` + `retro_get_system_info` — **no**
    /// `retro_init`, no callback installation, no frontend context. This is
    /// safe to call on arbitrary cores (including ones that hard-`exit()` in
    /// `retro_init` when their BIOS is missing, like FB Alpha / MAME) because
    /// `retro_get_system_info` is a pure static-data function that most cores
    /// implement without touching the filesystem or calling `exit`.
    ///
    /// Used by `CoreRegistry::scan_dir` (the per-launch scan) and by
    /// `install_core_from_zip` (post-download validation) so neither path
    /// risks a C-level `exit()` taking the process down. Full initialization
    /// (`retro_init` + callbacks + av-info probe) happens in [`Core::load`]
    /// and only when the user actually launches a game, on the dedicated core
    /// thread where a crash is recoverable.
    pub fn probe(path: impl AsRef<Path>) -> Result<SystemInfo, CoreError> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy().to_string();
        let lib = unsafe { Library::new(path) }.map_err(|source| CoreError::Load {
            path: path_str.clone(),
            source,
        })?;

        unsafe fn sym<'a, T>(
            lib: &'a Library,
            name: &'static [u8],
        ) -> Result<Symbol<'static, T>, CoreError> {
            let s: Symbol<'a, T> = lib
                .get(name)
                .map_err(|_| CoreError::MissingSymbol(static_name(name)))?;
            Ok(std::mem::transmute::<Symbol<'a, T>, Symbol<'static, T>>(s))
        }

        let api_version: Symbol<'static, abi::RetroApiVersion> =
            unsafe { sym(&lib, b"retro_api_version\0")? };
        let api = unsafe { api_version() };
        if api != abi::RETRO_API_VERSION {
            return Err(CoreError::BadApiVersion(api));
        }

        let retro_get_system_info: Symbol<'static, abi::RetroGetSystemInfo> =
            unsafe { sym(&lib, b"retro_get_system_info\0")? };

        // `retro_get_system_info` does not require an active frontend context
        // for most cores, but a few probe env during this call. Install a
        // throwaway context briefly so any env callback has somewhere to write
        // (it's discarded immediately after).
        let mut sys_info_raw: abi::RetroSystemInfo = unsafe { std::mem::zeroed() };
        let ctx = FrontendContext::new("", &path_str);
        context::CURRENT.with(|c| *c.borrow_mut() = Some(ctx));
        unsafe { retro_get_system_info(&mut sys_info_raw) };
        context::CURRENT.with(|c| *c.borrow_mut() = None);

        Ok(SystemInfo {
            library_name: cstr_to_string(sys_info_raw.library_name),
            library_version: cstr_to_string(sys_info_raw.library_version),
            valid_extensions: cstr_to_string(sys_info_raw.valid_extensions)
                .split('|')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
            need_fullpath: sys_info_raw.need_fullpath,
            block_extract: sys_info_raw.block_extract,
        })
    }

    /// Probe a core's declared options (variables) without full
    /// initialization.
    ///
    /// Calls `retro_api_version`, `retro_get_system_info`, then
    /// `retro_set_environment` (which triggers `SET_VARIABLES` for most
    /// cores) — but **not** `retro_init`. This is safe for cores that
    /// hard-`exit()` in `retro_init` (like FB Alpha / MAME when BIOS is
    /// missing), because `retro_set_environment` is a simple callback
    /// registration that most cores implement without touching the
    /// filesystem.
    ///
    /// **Limitation:** a minority of cores declare variables during
    /// `retro_init` rather than `retro_set_environment`. For those,
    /// `probe_variables` returns an empty `Vec` — the UI should show
    /// "options unavailable, launch the core to configure" in that case.
    /// This is far better than crashing the process.
    ///
    /// Used by `spawn_core_options` (the Settings UI) and `config options`
    /// (the CLI) so neither risks a C-level `exit()` taking the process
    /// down. Full initialization with variables happens in [`Core::load`]
    /// and only when the user actually launches a game, on the dedicated
    /// core thread.
    pub fn probe_variables(
        path: impl AsRef<Path>,
        system_dir: &str,
    ) -> Result<(SystemInfo, Vec<crate::context::CoreVariable>), CoreError> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy().to_string();
        let lib = unsafe { Library::new(path) }.map_err(|source| CoreError::Load {
            path: path_str.clone(),
            source,
        })?;

        unsafe fn sym<'a, T>(
            lib: &'a Library,
            name: &'static [u8],
        ) -> Result<Symbol<'static, T>, CoreError> {
            let s: Symbol<'a, T> = lib
                .get(name)
                .map_err(|_| CoreError::MissingSymbol(static_name(name)))?;
            Ok(std::mem::transmute::<Symbol<'a, T>, Symbol<'static, T>>(s))
        }

        let api_version: Symbol<'static, abi::RetroApiVersion> =
            unsafe { sym(&lib, b"retro_api_version\0")? };
        let api = unsafe { api_version() };
        if api != abi::RETRO_API_VERSION {
            return Err(CoreError::BadApiVersion(api));
        }

        let retro_get_system_info: Symbol<'static, abi::RetroGetSystemInfo> =
            unsafe { sym(&lib, b"retro_get_system_info\0")? };
        let retro_set_environment: Symbol<'static, abi::RetroSetEnvironment> =
            unsafe { sym(&lib, b"retro_set_environment\0")? };

        // Install a frontend context so the environment callback has
        // somewhere to write variables. The context's `system_dir` is set
        // so cores that probe it during `retro_set_environment` don't get a
        // null pointer.
        let ctx = FrontendContext::new(system_dir, &path_str);
        let mut sys_info_raw: abi::RetroSystemInfo = unsafe { std::mem::zeroed() };
        context::CURRENT.with(|c| *c.borrow_mut() = Some(ctx.clone()));
        unsafe {
            retro_get_system_info(&mut sys_info_raw);
            // Install our environment callback. Cores that call
            // `SET_VARIABLES` from inside `retro_set_environment` will
            // populate `ctx.variables` here — no `retro_init` needed.
            retro_set_environment(context::cb_environment);
        }
        let variables = ctx.variables.borrow().clone();
        context::CURRENT.with(|c| *c.borrow_mut() = None);

        Ok((
            SystemInfo {
                library_name: cstr_to_string(sys_info_raw.library_name),
                library_version: cstr_to_string(sys_info_raw.library_version),
                valid_extensions: cstr_to_string(sys_info_raw.valid_extensions)
                    .split('|')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
                need_fullpath: sys_info_raw.need_fullpath,
                block_extract: sys_info_raw.block_extract,
            },
            variables,
        ))
    }

    /// Load a core from a dynamic library path.
    pub fn load(path: impl AsRef<Path>, system_dir: &str) -> Result<Self, CoreError> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy().to_string();
        let lib = unsafe { Library::new(path) }.map_err(|source| CoreError::Load {
            path: path_str.clone(),
            source,
        })?;

        // Stash the function pointers with 'static lifetimes by leaking the
        // Library into our ownership — we hold the Library in `self._lib`, so
        // the symbols are valid for as long as `self` lives. We transmute the
        // borrowed symbols to 'static; this is sound because `self._lib`
        // outlives `self` and we never hand the symbols out.
        unsafe fn sym<'a, T>(
            lib: &'a Library,
            name: &'static [u8],
        ) -> Result<Symbol<'static, T>, CoreError> {
            let s: Symbol<'a, T> = lib
                .get(name)
                .map_err(|_| CoreError::MissingSymbol(static_name(name)))?;
            // SAFETY: the Library is owned by `Core` and outlives the symbols.
            Ok(std::mem::transmute::<Symbol<'a, T>, Symbol<'static, T>>(s))
        }

        let api_version: Symbol<'static, abi::RetroApiVersion> =
            unsafe { sym(&lib, b"retro_api_version\0")? };
        let api = unsafe { api_version() };
        if api != abi::RETRO_API_VERSION {
            return Err(CoreError::BadApiVersion(api));
        }

        let retro_init: Symbol<'static, abi::RetroInit> = unsafe { sym(&lib, b"retro_init\0")? };
        let retro_deinit: Option<Symbol<'static, abi::RetroDeinit>> =
            unsafe { sym(&lib, b"retro_deinit\0").ok() };
        let retro_get_system_info: Symbol<'static, abi::RetroGetSystemInfo> =
            unsafe { sym(&lib, b"retro_get_system_info\0")? };
        let retro_get_system_av_info: Symbol<'static, abi::RetroGetSystemAvInfo> =
            unsafe { sym(&lib, b"retro_get_system_av_info\0")? };
        let retro_set_environment: Symbol<'static, abi::RetroSetEnvironment> =
            unsafe { sym(&lib, b"retro_set_environment\0")? };
        let retro_set_video_refresh: Symbol<'static, abi::RetroSetVideoRefresh> =
            unsafe { sym(&lib, b"retro_set_video_refresh\0")? };
        let retro_set_audio_sample: Symbol<'static, abi::RetroSetAudioSample> =
            unsafe { sym(&lib, b"retro_set_audio_sample\0")? };
        let retro_set_audio_sample_batch: Symbol<'static, abi::RetroSetAudioSampleBatch> =
            unsafe { sym(&lib, b"retro_set_audio_sample_batch\0")? };
        let retro_set_input_poll: Symbol<'static, abi::RetroSetInputPoll> =
            unsafe { sym(&lib, b"retro_set_input_poll\0")? };
        let retro_set_input_state: Symbol<'static, abi::RetroSetInputState> =
            unsafe { sym(&lib, b"retro_set_input_state\0")? };
        let retro_set_controller_port_device =
            unsafe { sym(&lib, b"retro_set_controller_port_device\0").ok() };
        let retro_cheat_reset = unsafe { sym(&lib, b"retro_cheat_reset\0").ok() };
        let retro_cheat_set = unsafe { sym(&lib, b"retro_cheat_set\0").ok() };
        let retro_run: Symbol<'static, abi::RetroRun> = unsafe { sym(&lib, b"retro_run\0")? };
        let retro_reset: Symbol<'static, abi::RetroReset> = unsafe { sym(&lib, b"retro_reset\0")? };
        let retro_load_game: Symbol<'static, abi::RetroLoadGame> =
            unsafe { sym(&lib, b"retro_load_game\0")? };
        let retro_unload_game: Symbol<'static, abi::RetroUnloadGame> =
            unsafe { sym(&lib, b"retro_unload_game\0")? };
        let retro_serialize_size: Symbol<'static, abi::RetroSerializeSize> =
            unsafe { sym(&lib, b"retro_serialize_size\0")? };
        let retro_serialize: Symbol<'static, abi::RetroSerialize> =
            unsafe { sym(&lib, b"retro_serialize\0")? };
        let retro_unserialize: Symbol<'static, abi::RetroUnserialize> =
            unsafe { sym(&lib, b"retro_unserialize\0")? };
        let retro_get_memory_data: Symbol<'static, abi::RetroGetMemoryData> =
            unsafe { sym(&lib, b"retro_get_memory_data\0")? };
        let retro_get_memory_size: Symbol<'static, abi::RetroGetMemorySize> =
            unsafe { sym(&lib, b"retro_get_memory_size\0")? };

        let ctx = FrontendContext::new(system_dir, &path_str);

        // Read system info (before installing callbacks — some cores probe env).
        let mut sys_info_raw: abi::RetroSystemInfo = unsafe { std::mem::zeroed() };
        context::CURRENT.with(|c| *c.borrow_mut() = Some(ctx.clone()));
        unsafe { retro_get_system_info(&mut sys_info_raw) };
        context::CURRENT.with(|c| *c.borrow_mut() = None);

        let system_info = SystemInfo {
            library_name: cstr_to_string(sys_info_raw.library_name),
            library_version: cstr_to_string(sys_info_raw.library_version),
            valid_extensions: cstr_to_string(sys_info_raw.valid_extensions)
                .split('|')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
            need_fullpath: sys_info_raw.need_fullpath,
            block_extract: sys_info_raw.block_extract,
        };

        // Install our callbacks into the core. Per the libretro API, the
        // environment callback must be installed before `retro_init` so the
        // core can call `SET_VARIABLES` / `SET_PIXEL_FORMAT` etc. during
        // init. The previous order (`retro_init` then `retro_set_environment`)
        // was backwards and meant variables declared during `retro_init` were
        // lost.
        context::CURRENT.with(|c| *c.borrow_mut() = Some(ctx.clone()));
        unsafe {
            retro_set_environment(context::cb_environment);
            retro_init();
            retro_set_video_refresh(context::cb_video_refresh);
            retro_set_audio_sample(context::cb_audio_sample);
            retro_set_audio_sample_batch(context::cb_audio_sample_batch);
            retro_set_input_poll(context::cb_input_poll);
            retro_set_input_state(context::cb_input_state);
        }
        context::CURRENT.with(|c| *c.borrow_mut() = None);

        Ok(Core {
            _lib: lib,
            system_info,
            // The libretro contract permits A/V geometry to depend on the
            // loaded content. Some cores (including mGBA) dereference game
            // state in `retro_get_system_av_info`, so querying it before
            // `retro_load_game` can segfault the whole frontend.
            av_info: SystemAvInfo::default(),
            ctx,
            thread_id: std::thread::current().id(),
            retro_run,
            retro_reset,
            retro_load_game,
            retro_unload_game,
            retro_serialize_size,
            retro_serialize,
            retro_unserialize,
            retro_get_memory_data,
            retro_get_memory_size,
            retro_get_system_av_info,
            retro_deinit,
            retro_cheat_reset,
            retro_cheat_set,
            retro_set_controller_port_device,
        })
    }

    fn assert_thread(&self) -> Result<(), CoreError> {
        if std::thread::current().id() != self.thread_id {
            return Err(CoreError::WrongThread);
        }
        Ok(())
    }

    pub fn system_info(&self) -> &SystemInfo {
        &self.system_info
    }

    pub fn av_info(&self) -> SystemAvInfo {
        self.av_info
    }

    /// Take any pending geometry/timing update broadcast by the core via
    /// `SET_GEOMETRY` or `SET_SYSTEM_AV_INFO`. Returns `None` if the core
    /// hasn't signaled a change since the last call.
    ///
    /// The core thread should call this after each `retro_run` and apply the
    /// new geometry to the texture/timing; PS1-class cores change resolution
    /// mid-game. If `fps` or `sample_rate` is `Some`, the pacing and audio
    /// stream should be reconfigured.
    pub fn take_geometry_update(&self) -> Option<crate::context::GeometryUpdate> {
        // Reset the timing-change flag regardless of whether a geometry was
        // posted; the app reads geometry every frame and the flag is only used
        // to decide whether to reconfigure pacing/audio.
        let _ = self.ctx.av_info_changed.replace(false);
        self.ctx.geometry.borrow_mut().take()
    }

    /// Return and clear a core-requested graceful frontend shutdown.
    pub fn take_shutdown_requested(&self) -> bool {
        self.ctx.shutdown_requested.replace(false)
    }

    /// The active frontend context (for option get/set).
    pub fn context(&self) -> &Arc<FrontendContext> {
        &self.ctx
    }

    pub fn capabilities(&mut self) -> Result<CoreCapabilities, CoreError> {
        self.assert_thread()?;
        Ok(CoreCapabilities {
            save_states: self.serialize_size()? > 0,
            cheats: self.retro_cheat_reset.is_some() && self.retro_cheat_set.is_some(),
            controller_port_devices: self.retro_set_controller_port_device.is_some(),
            // These become true when the frontend context implements and the
            // core negotiates their environment interfaces.
            disk_control: false,
            rumble: false,
            hardware_render: false,
        })
    }

    pub fn reset_cheats(&mut self) -> Result<bool, CoreError> {
        self.assert_thread()?;
        let Some(reset) = self.retro_cheat_reset.as_ref() else {
            return Ok(false);
        };
        context::CURRENT.with(|current| *current.borrow_mut() = Some(self.ctx.clone()));
        unsafe { reset() };
        context::CURRENT.with(|current| *current.borrow_mut() = None);
        Ok(true)
    }

    pub fn set_cheat(&mut self, index: u32, enabled: bool, code: &str) -> Result<bool, CoreError> {
        self.assert_thread()?;
        let Some(set) = self.retro_cheat_set.as_ref() else {
            return Ok(false);
        };
        let code = CString::new(code).map_err(|_| CoreError::InvalidCheatCode)?;
        context::CURRENT.with(|current| *current.borrow_mut() = Some(self.ctx.clone()));
        unsafe { set(index, enabled, code.as_ptr()) };
        context::CURRENT.with(|current| *current.borrow_mut() = None);
        Ok(true)
    }

    pub fn set_controller_port_device(
        &mut self,
        port: u32,
        device: u32,
    ) -> Result<bool, CoreError> {
        self.assert_thread()?;
        let Some(set_device) = self.retro_set_controller_port_device.as_ref() else {
            return Ok(false);
        };
        context::CURRENT.with(|current| *current.borrow_mut() = Some(self.ctx.clone()));
        unsafe { set_device(port, device) };
        context::CURRENT.with(|current| *current.borrow_mut() = None);
        Ok(true)
    }

    /// Load a ROM. Pass the raw bytes; pass an empty slice for no-content cores.
    ///
    /// If the core declared `need_fullpath` at load time, the data pointer is
    /// set to NULL and only `path` is handed to the core — the core reads the
    /// file itself. Slurping a 700 MB disc image into RAM (then SHA-1 hashing
    /// it on record start) is not acceptable for those cores.
    pub fn load_game(&mut self, rom: &[u8], rom_path: Option<&str>) -> Result<(), CoreError> {
        self.assert_thread()?;
        let path_c = rom_path.map(|p| CString::new(p).unwrap());
        let path_ptr = path_c
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null());
        let data_ptr = if self.system_info.need_fullpath {
            std::ptr::null()
        } else {
            rom.as_ptr() as *const c_void
        };
        let info = abi::RetroGameInfo {
            path: path_ptr,
            data: data_ptr,
            size: rom.len(),
            meta: std::ptr::null(),
        };
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        let ok = unsafe { (self.retro_load_game)(&info) };
        if !ok {
            context::CURRENT.with(|c| *c.borrow_mut() = None);
            return Err(CoreError::LoadGameFailed);
        }
        // Re-probe av info — geometry may change after load_game.
        let mut av_raw: abi::RetroSystemAvInfo = unsafe { std::mem::zeroed() };
        unsafe { (self.retro_get_system_av_info)(&mut av_raw) };
        self.av_info = SystemAvInfo {
            base_width: av_raw.geometry.base_width,
            base_height: av_raw.geometry.base_height,
            max_width: av_raw.geometry.max_width,
            max_height: av_raw.geometry.max_height,
            aspect_ratio: av_raw.geometry.aspect_ratio,
            fps: av_raw.timing.fps,
            sample_rate: av_raw.timing.sample_rate,
        };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        Ok(())
    }

    /// Run one frame with the given input. Returns the produced frame + audio.
    pub fn run_frame(&mut self, input: InputState) -> Result<RunOutput, CoreError> {
        self.assert_thread()?;
        self.ctx.reset_run(input);
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        unsafe { (self.retro_run)() };
        let frame = self.ctx.frame.borrow_mut().take();
        let audio = self.ctx.audio.borrow_mut().split_off(0);
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        Ok(RunOutput { frame, audio })
    }

    /// Convenience: run a frame and require a frame buffer (fails if core didn't render).
    pub fn run_frame_required(&mut self, input: InputState) -> Result<Frame, CoreError> {
        let out = self.run_frame(input)?;
        out.frame.ok_or(CoreError::NoFrame)
    }

    pub fn reset(&mut self) -> Result<(), CoreError> {
        self.assert_thread()?;
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        unsafe { (self.retro_reset)() };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        Ok(())
    }

    /// Size in bytes of a serialized save state.
    pub fn serialize_size(&mut self) -> Result<usize, CoreError> {
        self.assert_thread()?;
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        let s = unsafe { (self.retro_serialize_size)() };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        Ok(s)
    }

    /// Serialize the current state to bytes.
    pub fn serialize(&mut self) -> Result<Vec<u8>, CoreError> {
        self.assert_thread()?;
        let size = self.serialize_size()?;
        let mut buf = vec![0u8; size];
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        let ok = unsafe { (self.retro_serialize)(buf.as_mut_ptr() as *mut c_void, size) };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        if !ok {
            return Err(CoreError::SerializeRoundTrip);
        }
        Ok(buf)
    }

    /// Restore state from bytes.
    pub fn unserialize(&mut self, data: &[u8]) -> Result<(), CoreError> {
        self.assert_thread()?;
        let size = self.serialize_size()?;
        if data.len() != size {
            return Err(CoreError::SerializeSize {
                expected: size,
                got: data.len(),
            });
        }
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        let ok = unsafe { (self.retro_unserialize)(data.as_ptr() as *const c_void, data.len()) };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        if !ok {
            return Err(CoreError::SerializeRoundTrip);
        }
        Ok(())
    }

    /// Read a memory region (`SAVE_RAM`, `SYSTEM_RAM`, etc.) as a borrowed slice.
    ///
    /// Returns `None` if the core doesn't expose this region.
    pub fn memory(&mut self, region: u32) -> Result<Option<Vec<u8>>, CoreError> {
        self.assert_thread()?;
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        let size = unsafe { (self.retro_get_memory_size)(region as c_uint) };
        let ptr = unsafe { (self.retro_get_memory_data)(region as c_uint) };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        if ptr.is_null() || size == 0 {
            return Ok(None);
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) };
        Ok(Some(slice.to_vec()))
    }

    /// Write bytes back into a memory region (e.g. SRAM). Length must match.
    pub fn set_memory(&mut self, region: u32, data: &[u8]) -> Result<(), CoreError> {
        self.assert_thread()?;
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        let size = unsafe { (self.retro_get_memory_size)(region as c_uint) };
        let ptr = unsafe { (self.retro_get_memory_data)(region as c_uint) };
        context::CURRENT.with(|c| *c.borrow_mut() = None);
        if ptr.is_null() || size == 0 {
            return Ok(());
        }
        if data.len() != size {
            return Err(CoreError::SerializeSize {
                expected: size,
                got: data.len(),
            });
        }
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, size) };
        Ok(())
    }

    /// Variables the core declared (`retro_set_variables`).
    pub fn variables(&self) -> Vec<crate::context::CoreVariable> {
        self.ctx.variables.borrow().clone()
    }

    /// Set a core option (applied on next env probe).
    pub fn set_option(&self, key: &str, value: &str) {
        self.ctx.set_option(key, value);
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Best-effort teardown; ignore thread issues during drop.
        let _ = self.assert_thread();
        context::CURRENT.with(|c| *c.borrow_mut() = Some(self.ctx.clone()));
        unsafe {
            (self.retro_unload_game)();
            if let Some(deinit) = &self.retro_deinit {
                deinit();
            }
        }
        context::CURRENT.with(|c| *c.borrow_mut() = None);
    }
}

fn cstr_to_string(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string()
}

fn static_name(name: &'static [u8]) -> &'static str {
    match std::str::from_utf8(name) {
        Ok(s) => s.trim_end_matches('\0'),
        Err(_) => "<bad utf8>",
    }
}
