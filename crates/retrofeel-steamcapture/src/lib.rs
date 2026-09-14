//! macOS screen + input capture for Steam/Wine game recording.
//!
//! This crate provides the capture glue that lets RetroFeel record Steam games
//! running under Wine with the same artifacts as a libretro session: a
//! frame-indexed video, a per-frame input log (`RawHostInput`), and (via the
//! app's `MicRecorder`) a mic track + SRT transcript.
//!
//! Three capture mechanisms, all macOS-specific:
//!
//! - **Video**: ScreenCaptureKit `SCStream` targeting the Wine process by PID.
//!   Each `CMSampleBuffer` is converted to a `VideoFrame` (RGBA8) and published
//!   via a channel. The SCK frame callback is the capture clock — one delivered
//!   frame = one input sample, preserving the 1:1 video↔input-log invariant.
//!
//! - **Keyboard + mouse**: a `CGEventTap` installed at the session-event-tap
//!   level sees events system-wide (before Wine) regardless of window focus.
//!   The tap accumulates keycodes + mouse deltas/buttons into `RawHostInput`.
//!   Requires Accessibility + Input Monitoring TCC permissions.
//!
//! - **Gamepad**: native Steam capture can observe connected pads through
//!   Apple's GameController framework. GameHub/Wine capture leaves controller
//!   ownership entirely to Wine/Steam Input and records translated
//!   keyboard/mouse output passively.
//!
//! On non-macOS targets the crate compiles but all public functions return
//! `CaptureError::UnsupportedPlatform`.

#![allow(clippy::large_enum_variant)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::Receiver;
use retrofeel_types::{RawHostInput, VideoFrame};
use thiserror::Error;

mod cfr;
pub use cfr::{CfrResampler, CfrStats, CfrTick};

/// A single captured frame: the RGBA8 video buffer + the raw host input state
/// accumulated during this frame's capture window.
#[derive(Debug, Clone, Default)]
pub struct CapturedFrame {
    pub frame_index: u64,
    /// Native ScreenCaptureKit presentation timestamp in microseconds. The
    /// source timeline may be irregular; `CfrResampler` preserves it in the
    /// frame map while driving an explicit encoded CFR grid.
    pub source_pts_us: u64,
    /// `source_pts_us` rebased onto mach host time at the first callback.
    /// This gives narration and video one declared clock without replacing
    /// the original PTS preserved in `frame-map.json`.
    pub source_mach_us: u64,
    /// Shared because one source frame can supply multiple CFR grid ticks and
    /// the render and recording consumers. Cloning an 8 MiB 1080p RGBA buffer
    /// for each of those consumers was a major source of capture pressure.
    pub video: Option<Arc<VideoFrame>>,
    pub raw_host: RawHostInput,
}

/// Configuration for a capture session.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// PID of the process whose windows we capture (the Wine process).
    /// Used as the first targeting attempt: we look for a window whose
    /// owning application has this PID.
    pub target_pid: u32,
    /// PID of the app itself, used to exclude our own window from title-based
    /// matching. Without this, a title hint like "Meadow of Lanterns" would
    /// match the mirror window "Capture — Meadow of Lanterns" instead of the
    /// game window.
    pub exclude_pid: Option<u32>,
    /// Optional window title fragment. If PID targeting fails (Wine windows
    /// often have no registered `SCRunningApplication`), we fall back to
    /// matching any on-screen window whose title contains this string,
    /// excluding windows owned by `exclude_pid`.
    pub window_title_hint: Option<String>,
    /// Maximum capture width (downscaled if the window is larger).
    pub max_width: u32,
    /// Maximum capture height (downscaled if the window is larger).
    pub max_height: u32,
    /// Target frame rate for the SCK stream.
    pub fps: u32,
    /// Observe controllers through Apple's GameController framework.
    ///
    /// This must stay disabled for GameHub/Wine captures. Wine/Steam Input
    /// already owns the controller route there, and starting a second macOS
    /// controller client can perturb that route. Keyboard and mouse capture
    /// remains available through the listen-only event tap.
    pub capture_gamepads: bool,
}

/// Errors from the capture session.
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("retrofeel-steamcapture is only supported on macOS")]
    UnsupportedPlatform,
    #[error("failed to enumerate shareable content: {0}")]
    ShareableContent(String),
    #[error("no matching application/window found for PID {0}")]
    NoTargetWindow(u32),
    #[error("failed to create SCStream: {0}")]
    CreateStream(String),
    #[error("failed to start capture: {0}")]
    StartCapture(String),
    #[error("failed to create CGEventTap: {0}")]
    EventTap(String),
    #[error("capture thread error: {0}")]
    Thread(String),
}

/// A handle to a running capture session. Drop it to stop capture and join the
/// thread. Frames are consumed via [`CaptureHandle::receiver`].
pub struct CaptureHandle {
    pub receiver: Receiver<CapturedFrame>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl CaptureHandle {
    /// The receiver for captured frames. Each item is one SCK video frame +
    /// the raw host input accumulated during that frame's capture window.
    pub fn receiver(&self) -> &Receiver<CapturedFrame> {
        &self.receiver
    }

    /// Signal the capture thread to stop. The thread will stop the SCK stream,
    /// disable the event tap, and exit.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start a capture session targeting the given PID. Returns a handle whose
/// receiver yields `CapturedFrame`s at the capture frame rate.
///
/// On non-macOS targets this returns `CaptureError::UnsupportedPlatform`.
pub fn start_capture(config: CaptureConfig) -> Result<CaptureHandle, CaptureError> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::start_capture(config)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config;
        Err(CaptureError::UnsupportedPlatform)
    }
}

/// Resolve the PID of a Wine process running `exe_name` by scanning `ps`.
/// Returns the first matching PID, or None if not found. This is used when
/// launching through GameHub (we don't own the child process directly).
pub fn find_wine_pid(exe_name: &str) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["aux"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let own_pid = std::process::id();
    for line in stdout.lines() {
        if !line.contains(exe_name) || line.contains("grep") {
            continue;
        }
        // Skip RetroFeel processes: our own command line contains the exe
        // name via --exe/--attach, and another instance's would too.
        if line.contains("retrofeel") {
            continue;
        }
        let mut cols = line.split_whitespace();
        cols.next(); // USER
        if let Some(pid_str) = cols.next() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid != own_pid {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Bring an externally launched game process back to the foreground after
/// RetroFeel has attached to it.
///
/// `retrofeel gamehub` is normally invoked from Terminal, which necessarily
/// leaves Terminal frontmost. The capture app itself is non-activating, so it
/// must explicitly return focus to the exact game PID once ScreenCaptureKit
/// has proved that PID owns the selected game window.
pub fn activate_process(pid: u32) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

        let pid = i32::try_from(pid).map_err(|_| format!("invalid game PID {pid}"))?;
        let application = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .ok_or_else(|| format!("macOS has no running application for PID {pid}"))?;
        if application.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows) {
            Ok(())
        } else {
            Err(format!("macOS refused to activate game PID {pid}"))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        Err("activating an external game is only supported on macOS".into())
    }
}

/// Launch a Wine process with the given prefix and exe path. Returns the child
/// handle so the caller can extract the PID and wait for exit.
pub fn launch_wine(
    wine_bin: &PathBuf,
    wine_prefix: &PathBuf,
    exe_path: &PathBuf,
) -> std::io::Result<std::process::Child> {
    std::process::Command::new(wine_bin)
        .env("WINEPREFIX", wine_prefix)
        .arg(exe_path)
        .spawn()
}

#[cfg(target_os = "macos")]
mod macos;
