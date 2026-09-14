//! Safe Rust wrapper over the libretro C ABI.
//!
//! Phase 1 scope: load any software-rendered core, run frames headless,
//! produce RGBA8 frames + PCM audio, serialize/unserialize save states,
//! read/write memory regions, and enumerate/set core options.
//!
//! Cores are single-threaded by libretro contract; a [`Core`] lives on the
//! thread that created it and panics if used elsewhere (Phase 2 runs each core
//! on a dedicated thread).

#![allow(clippy::missing_safety_doc)]

pub mod abi;
pub mod context;
pub mod core;
pub mod pixel;

pub use context::{CoreVariable, FrontendContext, GeometryUpdate, RunOutput};
pub use core::{Core, CoreCapabilities, CoreError, SystemAvInfo, SystemInfo};
pub use pixel::{rgba8_to_xrgb8888, Frame, FrameRepeat};
