//! macOS capture implementation: ScreenCaptureKit video + CGEventTap input.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crossbeam_channel::{bounded, Sender};
use retrofeel_types::{MouseState, RawGamepadInput, RawHostInput, VideoFrame};

use crate::{CaptureConfig, CaptureError, CaptureHandle, CapturedFrame};

/// Start a macOS capture session.
pub fn start_capture(config: CaptureConfig) -> Result<CaptureHandle, CaptureError> {
    let (frame_sender, frame_receiver) = bounded::<CapturedFrame>(30);
    let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    let join = thread::Builder::new()
        .name("retrofeel-steamcapture".into())
        .spawn(move || {
            if let Err(e) = run_capture(config, frame_sender, thread_stop, startup_sender) {
                log::error!("steamcapture: capture thread error: {e}");
            }
        })
        .map_err(|e| CaptureError::Thread(e.to_string()))?;

    match startup_receiver.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            stop.store(true, Ordering::SeqCst);
            let _ = join.join();
            return Err(error);
        }
        Err(error) => {
            stop.store(true, Ordering::SeqCst);
            let _ = join.join();
            return Err(CaptureError::Thread(format!(
                "capture thread exited during startup: {error}"
            )));
        }
    }

    Ok(CaptureHandle {
        receiver: frame_receiver,
        stop,
        join: Some(join),
    })
}

/// The shared input accumulator. The CGEventTap callback writes into this,
/// and the SCK frame callback reads + resets it each frame to build a
/// `RawHostInput` snapshot.
struct InputAccumulator {
    /// Keycodes held down (names like "A", "Return", "Shift").
    keyboard_keys: BTreeSet<String>,
    mouse_dx: i32,
    mouse_dy: i32,
    /// Bit 0 = left, 1 = right, 2 = middle.
    mouse_buttons: u8,
}

impl InputAccumulator {
    fn new() -> Self {
        Self {
            keyboard_keys: BTreeSet::new(),
            mouse_dx: 0,
            mouse_dy: 0,
            mouse_buttons: 0,
        }
    }

    /// Take a snapshot of accumulated deltas (keyboard list + mouse deltas).
    /// Button state is held (not reset) since the tap updates it on down/up.
    fn snapshot_and_reset(&mut self) -> RawHostInput {
        // Keyboard keys are held state, not just transitions since the last
        // SCK callback. Mouse movement remains a per-sample delta.
        let keyboard_keys = self.keyboard_keys.iter().cloned().collect();
        let mouse = MouseState {
            dx: self.mouse_dx,
            dy: self.mouse_dy,
            buttons: self.mouse_buttons,
            ..Default::default()
        };
        self.mouse_dx = 0;
        self.mouse_dy = 0;
        RawHostInput {
            keyboard_keys,
            keyboard_key_codes: Vec::new(),
            gamepad_buttons: Vec::new(),
            gamepad_axes: BTreeMap::new(),
            gamepads: Vec::new(),
            mouse: Some(mouse),
        }
    }
}

type SharedInput = Arc<Mutex<InputAccumulator>>;

fn run_capture(
    config: CaptureConfig,
    sender: Sender<CapturedFrame>,
    stop: Arc<AtomicBool>,
    startup: mpsc::SyncSender<Result<(), CaptureError>>,
) -> Result<(), CaptureError> {
    let input_acc: SharedInput = Arc::new(Mutex::new(InputAccumulator::new()));

    // Warm the gamepad poller so pads are enumerated (and logged) at session
    // start instead of on the first captured frame.
    warm_gamepad_observer(config.capture_gamepads);

    // Set up the SCK stream targeting the process by PID.
    let (mut stream, _filter) = match create_sck_stream(&config) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };

    // Start the CGEvent tap only after the target window exists so retries do
    // not create orphaned run-loop threads.
    let tap_input = input_acc.clone();
    let event_tap_thread = match create_event_tap_thread(tap_input, stop.clone(), config.target_pid)
    {
        Ok(thread) => thread,
        Err(error) => {
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };

    // Frame handler: converts CMSampleBuffer → VideoFrame, snapshots input,
    // optionally polls shared GameController state, and sends a CapturedFrame.
    // The SCK callback IS the capture clock — one callback = one frame = one
    // input sample.
    let handler_input = input_acc.clone();
    let frame_index = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dropped_frame_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let handler_stop = stop.clone();
    let fallback_fps = config.fps.max(1);
    let capture_gamepads = config.capture_gamepads;
    let pts_mach_anchor = Arc::new(Mutex::new(None::<(u64, u64)>));
    let handler_pts_mach_anchor = pts_mach_anchor.clone();

    stream.add_output_handler(
        move |sample: screencapturekit::prelude::CMSampleBuffer,
              _of_type: screencapturekit::prelude::SCStreamOutputType| {
            if handler_stop.load(Ordering::SeqCst) {
                return;
            }

            let idx = frame_index.fetch_add(1, Ordering::SeqCst);
            let video_frame = sample_to_video_frame(&sample).map(Arc::new);
            let source_pts_us = sample_presentation_time_us(&sample)
                .unwrap_or_else(|| idx.saturating_mul(1_000_000) / fallback_fps as u64);
            let source_mach_us = source_pts_to_mach_us(source_pts_us, &handler_pts_mach_anchor)
                // Fallback preserves a declared monotonic clock on the rare path
                // where mach conversion itself cannot be queried.
                .unwrap_or(source_pts_us);

            let mut raw_host = match handler_input.lock() {
                Ok(mut acc) => acc.snapshot_and_reset(),
                Err(_) => RawHostInput::default(),
            };

            // GameHub/Wine owns its controller route through Steam Input.
            // Those sessions deliberately do not initialize another macOS
            // controller client; their translated keyboard/mouse output is
            // still captured by the listen-only event tap.
            capture_gamepad_snapshot(capture_gamepads, &mut raw_host);

            let captured = CapturedFrame {
                frame_index: idx,
                source_pts_us,
                source_mach_us,
                video: video_frame,
                raw_host,
            };

            if sender.try_send(captured).is_err() {
                let dropped = dropped_frame_count.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped.is_multiple_of(60) {
                    log::warn!(
                        "steamcapture: frame queue full, dropping frame {idx} ({dropped} source drops)"
                    );
                }
            }
        },
        screencapturekit::prelude::SCStreamOutputType::Screen,
    );

    if let Err(error) = stream.start_capture() {
        stop.store(true, Ordering::SeqCst);
        let _ = event_tap_thread.join();
        let _ = startup.send(Err(CaptureError::StartCapture(error.to_string())));
        return Ok(());
    }

    startup
        .send(Ok(()))
        .map_err(|error| CaptureError::Thread(error.to_string()))?;

    log::info!(
        "steamcapture: started capture for PID {} at {}x{} @ {}fps",
        config.target_pid,
        config.max_width,
        config.max_height,
        config.fps
    );

    // Park until stop is signalled. The SCK stream runs on its dispatch queue.
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_millis(100));
    }

    let _ = stream.stop_capture();
    let _ = event_tap_thread.join();
    log::info!("steamcapture: stopped");
    Ok(())
}

/// Convert the ScreenCaptureKit presentation timeline to mach host time by
/// anchoring its first delivered PTS to mach absolute time. Subsequent values
/// retain the true PTS deltas, avoiding callback scheduling jitter as a clock.
fn source_pts_to_mach_us(
    source_pts_us: u64,
    anchor: &Arc<Mutex<Option<(u64, u64)>>>,
) -> Option<u64> {
    let mut anchor = anchor.lock().ok()?;
    let (first_pts_us, first_mach_us) = match *anchor {
        Some(anchor) => anchor,
        None => {
            let now = mach_host_time_us()?;
            *anchor = Some((source_pts_us, now));
            (source_pts_us, now)
        }
    };
    Some(first_mach_us.saturating_add(source_pts_us.saturating_sub(first_pts_us)))
}

#[allow(deprecated)] // `mach2` exposes the same C calls; libc is already shared here.
fn mach_host_time_us() -> Option<u64> {
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    if unsafe { libc::mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
        return None;
    }
    let ticks = unsafe { libc::mach_absolute_time() } as u128;
    u64::try_from(ticks * timebase.numer as u128 / timebase.denom as u128 / 1_000).ok()
}

/// Convert an SCK `CMSampleBuffer` to a `VideoFrame` (RGBA8).
fn sample_to_video_frame(sample: &screencapturekit::prelude::CMSampleBuffer) -> Option<VideoFrame> {
    use screencapturekit::prelude::CMSampleBufferExt;

    let buffer = sample.image_buffer()?;

    let guard = buffer
        .lock(screencapturekit::cv::CVPixelBufferLockFlags::READ_ONLY)
        .ok()?;

    let width = guard.width() as u32;
    let height = guard.height() as u32;
    let bytes_per_row = guard.bytes_per_row();

    // SCK delivers BGRA; convert to RGBA8.
    let src = guard.as_slice();
    let expected = (width as usize) * (height as usize) * 4;
    if src.len() < expected {
        log::warn!(
            "steamcapture: pixel buffer too short: {} < {}",
            src.len(),
            expected
        );
        return None;
    }

    // If bytes_per_row == width*4, we can do a straight swap. Otherwise
    // (padded rows) we copy row by row.
    let mut rgba = vec![0u8; expected];
    let row_bytes = (width as usize) * 4;

    if bytes_per_row == row_bytes {
        bgra_to_rgba(src, &mut rgba);
    } else {
        for y in 0..(height as usize) {
            let src_row = &src[y * bytes_per_row..y * bytes_per_row + row_bytes];
            let dst_row = &mut rgba[y * row_bytes..(y + 1) * row_bytes];
            bgra_to_rgba(src_row, dst_row);
        }
    }

    Some(VideoFrame {
        width,
        height,
        rgba,
    })
}

/// Preserve ScreenCaptureKit's actual presentation timestamp rather than
/// treating callback arrival time or the requested stream rate as a clock.
fn sample_presentation_time_us(sample: &screencapturekit::prelude::CMSampleBuffer) -> Option<u64> {
    let time = sample.presentation_timestamp();
    if time.value < 0 || time.timescale <= 0 {
        return None;
    }
    u64::try_from((time.value as u128 * 1_000_000_u128) / time.timescale as u128).ok()
}

/// In-place BGRA → RGBA conversion.
fn bgra_to_rgba(src: &[u8], dst: &mut [u8]) {
    let n = src.len() / 4;
    for i in 0..n {
        let s = i * 4;
        let d = i * 4;
        dst[d] = src[s + 2]; // R ← B
        dst[d + 1] = src[s + 1]; // G
        dst[d + 2] = src[s]; // B ← R
        dst[d + 3] = src[s + 3]; // A
    }
}

/// Latest gamepad state, refreshed by the process-lifetime poller thread.
#[derive(Debug, Default, Clone)]
struct GamepadSnapshot {
    gamepads: Vec<RawGamepadInput>,
}

/// The shared slot the poller publishes into and frame callbacks read from.
static GAMEPAD_STATE: std::sync::OnceLock<Arc<Mutex<GamepadSnapshot>>> = std::sync::OnceLock::new();

/// Get the shared gamepad state, spawning the poller thread on first use.
///
/// Do not use gilrs/IOHIDManager here. Opening another raw reader made wired
/// Xbox pads reset at the USB layer on macOS 26. This GameController observer
/// is reserved for native Steam capture; GameHub/Wine sessions never call this
/// initializer because Wine/Steam Input already owns their controller route.
fn gamepad_state() -> Arc<Mutex<GamepadSnapshot>> {
    GAMEPAD_STATE
        .get_or_init(|| {
            let slot = Arc::new(Mutex::new(GamepadSnapshot::default()));
            let thread_slot = slot.clone();
            let spawned = thread::Builder::new()
                .name("retrofeel-gamepad".into())
                .spawn(move || gamepad_poll_loop(thread_slot));
            if let Err(e) = spawned {
                log::warn!("steamcapture: gamepad poller thread failed to spawn: {e}");
            }
            slot
        })
        .clone()
}

fn gamepad_poll_loop(slot: Arc<Mutex<GamepadSnapshot>>) {
    use objc2_game_controller::GCController;

    // RetroFeel deliberately stays in the background while the game is
    // frontmost. Without this opt-in, GameController freezes values as soon as
    // focus returns to Wine.
    unsafe { GCController::setShouldMonitorBackgroundEvents(true) };

    let mut previous_ids = BTreeSet::new();
    let mut warned_without_pad = false;
    loop {
        let controllers = unsafe { GCController::controllers() };
        let mut gamepads = (&*controllers)
            .into_iter()
            .enumerate()
            .filter_map(|(index, controller)| snapshot_gamepad(index, &controller))
            .collect::<Vec<_>>();
        gamepads.sort_by(|left, right| left.device_id.cmp(&right.device_id));
        for (port, gamepad) in gamepads.iter_mut().enumerate() {
            gamepad.port = u8::try_from(port).ok();
        }
        let current_ids = gamepads
            .iter()
            .map(|gamepad| gamepad.device_id.clone())
            .collect::<BTreeSet<_>>();
        for removed in previous_ids.difference(&current_ids) {
            // The snapshot is rebuilt from currently visible devices, so this
            // also purges every held button/axis from a removed controller.
            log::info!("steamcapture: gamepad removed: {removed}; held state purged");
        }
        for added in current_ids.difference(&previous_ids) {
            log::info!("steamcapture: gamepad added: {added}");
        }
        if current_ids.is_empty() && !warned_without_pad {
            log::warn!("steamcapture: no visible gamepad; keyboard/mouse capture continues and pads may be added later");
            warned_without_pad = true;
        }
        if !current_ids.is_empty() {
            warned_without_pad = false;
        }
        previous_ids = current_ids;
        let snap = GamepadSnapshot { gamepads };
        if let Ok(mut state) = slot.lock() {
            *state = snap;
        }
        thread::sleep(std::time::Duration::from_millis(4));
    }
}

fn snapshot_gamepad(
    index: usize,
    controller: &objc2_game_controller::GCController,
) -> Option<RawGamepadInput> {
    use objc2_game_controller::GCDevice;

    let gamepad = unsafe { controller.extendedGamepad()? };
    let name = unsafe { controller.vendorName() }
        .map(|name| name.to_string())
        .unwrap_or_else(|| "Game Controller".to_string());
    let mut buttons = Vec::new();
    macro_rules! push_button {
        ($name:literal, $button:expr) => {
            if unsafe { $button.isPressed() } {
                buttons.push($name.to_string());
            }
        };
    }
    push_button!("South", gamepad.buttonA());
    push_button!("East", gamepad.buttonB());
    push_button!("West", gamepad.buttonX());
    push_button!("North", gamepad.buttonY());
    push_button!("LeftTrigger", gamepad.leftShoulder());
    push_button!("RightTrigger", gamepad.rightShoulder());
    push_button!("LeftTrigger2", gamepad.leftTrigger());
    push_button!("RightTrigger2", gamepad.rightTrigger());
    push_button!("Start", gamepad.buttonMenu());
    if let Some(button) = unsafe { gamepad.buttonOptions() } {
        push_button!("Select", button);
    }
    if let Some(button) = unsafe { gamepad.leftThumbstickButton() } {
        push_button!("LeftThumb", button);
    }
    if let Some(button) = unsafe { gamepad.rightThumbstickButton() } {
        push_button!("RightThumb", button);
    }
    let dpad = unsafe { gamepad.dpad() };
    push_button!("DPadUp", dpad.up());
    push_button!("DPadDown", dpad.down());
    push_button!("DPadLeft", dpad.left());
    push_button!("DPadRight", dpad.right());

    let mut axes = BTreeMap::new();
    let mut push_axis = |name: &str, value: f32| {
        if value != 0.0 {
            axes.insert(name.to_string(), value);
        }
    };
    let left = unsafe { gamepad.leftThumbstick() };
    let right = unsafe { gamepad.rightThumbstick() };
    push_axis("LeftStickX", unsafe { left.xAxis().value() });
    push_axis("LeftStickY", unsafe { left.yAxis().value() });
    push_axis("RightStickX", unsafe { right.xAxis().value() });
    push_axis("RightStickY", unsafe { right.yAxis().value() });
    push_axis("LeftZ", unsafe { gamepad.leftTrigger().value() });
    push_axis("RightZ", unsafe { gamepad.rightTrigger().value() });

    Some(RawGamepadInput {
        device_id: format!("gamecontroller:{index}:{name}"),
        port: None,
        name,
        buttons,
        axes,
    })
}

/// Fill in the gamepad portion of `raw_host` from the shared poller state.
fn poll_gamepad_into(raw_host: &mut RawHostInput) {
    let snap = gamepad_state()
        .lock()
        .map(|state| state.clone())
        .unwrap_or_default();
    let primary = snap
        .gamepads
        .iter()
        .find(|gamepad| gamepad.port == Some(0))
        .or_else(|| snap.gamepads.first());
    raw_host.gamepad_buttons = primary
        .map(|gamepad| gamepad.buttons.clone())
        .unwrap_or_default();
    raw_host.gamepad_axes = primary
        .map(|gamepad| gamepad.axes.clone())
        .unwrap_or_default();
    raw_host.gamepads = snap.gamepads;
}

fn capture_gamepad_snapshot(enabled: bool, raw_host: &mut RawHostInput) {
    if enabled {
        poll_gamepad_into(raw_host);
    }
}

fn warm_gamepad_observer(enabled: bool) {
    if enabled {
        let _ = gamepad_state();
    }
}

/// Spawn the CGEventTap on a dedicated thread that runs a CFRunLoop.
fn create_event_tap_thread(
    input: SharedInput,
    stop: Arc<AtomicBool>,
    target_pid: u32,
) -> Result<thread::JoinHandle<()>, CaptureError> {
    let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("retrofeel-eventtap".into())
        .spawn(move || {
            if let Err(e) = run_event_tap(input, stop, target_pid, startup_sender) {
                log::error!("steamcapture: event tap error: {e}");
            }
        })
        .map_err(|e| CaptureError::EventTap(format!("thread spawn: {e}")))?;

    match startup_receiver.recv() {
        Ok(Ok(())) => Ok(join),
        Ok(Err(error)) => {
            let _ = join.join();
            Err(error)
        }
        Err(error) => {
            let _ = join.join();
            Err(CaptureError::EventTap(format!(
                "event tap thread exited during startup: {error}"
            )))
        }
    }
}

/// Create the CGEventTap, add it to the current thread's CFRunLoop, and run
/// the loop. This blocks until the process exits (the tap has no stop flag —
/// it lives for process lifetime, like the cpal audio stream).
fn run_event_tap(
    input: SharedInput,
    stop: Arc<AtomicBool>,
    target_pid: u32,
    startup: mpsc::SyncSender<Result<(), CaptureError>>,
) -> Result<(), CaptureError> {
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    };

    let tap_input = input.clone();
    let tap = match CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ],
        move |_proxy, event_type, event: &core_graphics::event::CGEvent| {
            handle_event(event_type, event, target_pid, &tap_input);
            None
        },
    ) {
        Ok(tap) => tap,
        Err(error) => {
            let _ = startup.send(Err(CaptureError::EventTap(format!(
                "CGEventTap::new: {error:?}"
            ))));
            return Ok(());
        }
    };

    // Enable the tap.
    tap.enable();

    // Create a run-loop source from the tap's mach port and add it to the
    // current thread's run loop.
    let run_loop = CFRunLoop::get_current();
    let source = match tap.mach_port.create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
            let _ = startup.send(Err(CaptureError::EventTap(
                "failed to create run loop source".into(),
            )));
            return Ok(());
        }
    };
    unsafe {
        run_loop.add_source(&source, kCFRunLoopCommonModes);
    }

    startup
        .send(Ok(()))
        .map_err(|error| CaptureError::EventTap(error.to_string()))?;
    log::info!("steamcapture: CGEventTap installed, entering run loop");
    while !stop.load(Ordering::SeqCst) {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            std::time::Duration::from_millis(100),
            true,
        );
    }

    Ok(())
}

/// Handle a single CGEvent: extract keyboard/mouse info and accumulate it.
fn handle_event(
    event_type: core_graphics::event::CGEventType,
    event: &core_graphics::event::CGEvent,
    target_pid: u32,
    input: &SharedInput,
) {
    use core_graphics::event::{CGEventType, EventField};

    let mut acc = match input.lock() {
        Ok(acc) => acc,
        Err(_) => return,
    };

    // A session-level tap sees input directed at every application. Keep the
    // recording authoritative for the selected game: input aimed at Terminal,
    // chat, or another window must not appear as if the game received it.
    // Native-Steam title fallback uses PID 0 because no exact process is known,
    // so only exact GameHub/Wine attaches can apply this filter.
    let event_target_pid =
        event.get_integer_value_field(EventField::EVENT_TARGET_UNIX_PROCESS_ID) as u32;
    let targets_game = event_targets_game(event_target_pid, target_pid);
    if !targets_game && !is_release_event(event_type) {
        // Ignore presses and motion aimed at another process. Do not purge all
        // held game state here: an unrelated event between a physical keydown
        // and its macOS autorepeat keydown would otherwise manufacture a
        // release followed by a second press in the recording.
        return;
    }

    if matches!(event_type, CGEventType::KeyDown | CGEventType::FlagsChanged) {
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
        let key_name = keycode_to_name(keycode);
        acc.keyboard_keys.insert(key_name);
    } else if matches!(event_type, CGEventType::KeyUp) {
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
        let key_name = keycode_to_name(keycode);
        acc.keyboard_keys.remove(&key_name);
    } else if matches!(
        event_type,
        CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged
    ) {
        let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X);
        let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y);
        acc.mouse_dx += dx as i32;
        acc.mouse_dy += dy as i32;
    } else if matches!(event_type, CGEventType::LeftMouseDown) {
        acc.mouse_buttons |= 1;
    } else if matches!(event_type, CGEventType::LeftMouseUp) {
        acc.mouse_buttons &= !1;
    } else if matches!(event_type, CGEventType::RightMouseDown) {
        acc.mouse_buttons |= 2;
    } else if matches!(event_type, CGEventType::RightMouseUp) {
        acc.mouse_buttons &= !2;
    } else if matches!(event_type, CGEventType::OtherMouseDown) {
        acc.mouse_buttons |= 4;
    } else if matches!(event_type, CGEventType::OtherMouseUp) {
        acc.mouse_buttons &= !4;
    }
}

fn event_targets_game(event_target_pid: u32, target_pid: u32) -> bool {
    target_pid == 0 || event_target_pid == target_pid
}

fn is_release_event(event_type: core_graphics::event::CGEventType) -> bool {
    use core_graphics::event::CGEventType;

    matches!(
        event_type,
        CGEventType::KeyUp
            | CGEventType::LeftMouseUp
            | CGEventType::RightMouseUp
            | CGEventType::OtherMouseUp
    )
}

/// Map a macOS virtual keycode to a human-readable name for the input log.
fn keycode_to_name(keycode: u16) -> String {
    match keycode {
        0 => "A",
        1 => "S",
        2 => "D",
        3 => "F",
        4 => "H",
        5 => "G",
        6 => "Z",
        7 => "X",
        8 => "C",
        9 => "V",
        11 => "B",
        12 => "Q",
        13 => "W",
        14 => "E",
        15 => "R",
        16 => "Y",
        17 => "T",
        18 => "1",
        19 => "2",
        20 => "3",
        21 => "4",
        22 => "6",
        23 => "5",
        24 => "Equals",
        25 => "9",
        26 => "7",
        27 => "Minus",
        28 => "8",
        29 => "0",
        30 => "RightBracket",
        31 => "O",
        32 => "U",
        33 => "LeftBracket",
        34 => "I",
        35 => "P",
        36 => "Return",
        37 => "L",
        38 => "J",
        39 => "Quote",
        40 => "K",
        41 => "Semicolon",
        42 => "Backslash",
        43 => "Comma",
        44 => "Slash",
        45 => "N",
        46 => "M",
        47 => "Period",
        48 => "Tab",
        49 => "Space",
        50 => "Grave",
        51 => "Delete",
        53 => "Escape",
        55 => "Command",
        56 => "Shift",
        57 => "CapsLock",
        58 => "Option",
        59 => "Control",
        60 => "RightShift",
        61 => "RightOption",
        62 => "RightControl",
        63 => "Function",
        122 => "F1",
        120 => "F2",
        99 => "F3",
        118 => "F4",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        100 => "F8",
        101 => "F9",
        109 => "F10",
        103 => "F11",
        111 => "F12",
        123 => "LeftArrow",
        124 => "RightArrow",
        125 => "DownArrow",
        126 => "UpArrow",
        _ => "Unknown",
    }
    .to_string()
}

/// Create an SCK stream targeting a Wine game window.
///
/// Targeting strategy (in order):
/// 1. Find an on-screen window whose owning application PID matches
///    `config.target_pid`.
/// 2. If no PID match (common for Wine — its windows often lack an
///    `SCRunningApplication`), find an on-screen window whose title contains
///    `config.window_title_hint`.
/// 3. If neither matches, return `NoTargetWindow`.
///
/// The filter captures only the matched window, on whatever display it's on.
fn create_sck_stream(
    config: &CaptureConfig,
) -> Result<
    (
        screencapturekit::prelude::SCStream,
        screencapturekit::prelude::SCContentFilter,
    ),
    CaptureError,
> {
    use screencapturekit::prelude::*;

    let content =
        SCShareableContent::get().map_err(|e| CaptureError::ShareableContent(e.to_string()))?;

    let windows = content.windows();
    let target_pid = config.target_pid as i32;
    let exclude_pid = config.exclude_pid.map(|p| p as i32);

    // Log all on-screen windows for debugging targeting failures.
    if log::log_enabled!(log::Level::Debug) {
        for w in &windows {
            if w.is_on_screen() {
                let app_pid = w.owning_application().map(|a| a.process_id());
                log::debug!(
                    "steamcapture: on-screen window id={} title={:?} owning_pid={:?} target_pid={} excluded={}",
                    w.window_id(),
                    w.title(),
                    app_pid,
                    target_pid,
                    app_pid.is_some_and(|p| Some(p) == exclude_pid)
                );
            }
        }
    }

    // Strategy 1: find a window whose owning app has our PID.
    let target_window: Option<&SCWindow> = windows.iter().find(|w| {
        if !w.is_on_screen() {
            return false;
        }
        w.owning_application()
            .is_some_and(|app| app.process_id() == target_pid)
    });

    // Strategy 2: fall back to window title matching. Exclude our own PID and
    // any product mirror window. Another instance carries the game's name in
    // its title ("Capture — Meadow of Lanterns") and `exclude_pid` cannot catch
    // it, so without the prefix check a hint match creates a feedback loop.
    let target_window = target_window.or_else(|| {
        let hint = config.window_title_hint.as_ref()?;
        // Strip trademark glyphs so a hint of "CLOCKWORK VALLEY 2"
        // still matches a window titled "CLOCKWORK VALLEY™ 2".
        let normalize = |s: &str| s.to_lowercase().replace(['™', '®'], "");
        let hint_norm = normalize(hint);
        let candidate = |w: &&SCWindow| -> bool {
            if !w.is_on_screen() {
                return false;
            }
            if let Some(excl) = exclude_pid {
                if w.owning_application()
                    .is_some_and(|app| app.process_id() == excl)
                {
                    return false;
                }
            }
            !w.title()
                .is_some_and(|t| t.starts_with("Capture — ") || t.starts_with("RetroFeel"))
        };
        // Prefer an exact title match over a substring match so a launcher
        // or settings window ("ClockworkValley Config") never wins over the game.
        windows
            .iter()
            .filter(candidate)
            .find(|w| w.title().is_some_and(|t| normalize(&t) == hint_norm))
            .or_else(|| {
                windows.iter().filter(candidate).find(|w| {
                    w.title()
                        .is_some_and(|t| normalize(&t).contains(&hint_norm))
                })
            })
    });

    let target_window = target_window.ok_or(CaptureError::NoTargetWindow(config.target_pid))?;

    log::info!(
        "steamcapture: targeting window {:?} ({})",
        target_window.title(),
        target_window.window_id()
    );

    // Single-window capture filter: captures just this window (no display
    // needed — `with_window` implies the display it's on).
    let filter = SCContentFilter::create().with_window(target_window).build();

    let frame_interval = screencapturekit::cm::CMTime::new(1, config.fps as i32);
    let stream_config = SCStreamConfiguration::new()
        .with_width(config.max_width)
        .with_height(config.max_height)
        .with_pixel_format(screencapturekit::stream::configuration::pixel_format::PixelFormat::BGRA)
        .with_minimum_frame_interval(&frame_interval)
        .with_shows_cursor(true);

    let stream = SCStream::new(&filter, &stream_config);

    Ok((stream, filter))
}

#[cfg(test)]
mod tests {
    use super::{
        capture_gamepad_snapshot, event_targets_game, warm_gamepad_observer, GAMEPAD_STATE,
    };
    use retrofeel_types::RawHostInput;

    #[test]
    fn exact_attach_records_only_events_targeted_at_the_game() {
        assert!(event_targets_game(42, 42));
        assert!(!event_targets_game(7, 42));
    }

    #[test]
    fn title_fallback_keeps_global_capture_when_pid_is_unknown() {
        assert!(event_targets_game(7, 0));
    }

    #[test]
    fn off_target_releases_can_end_held_game_input() {
        use core_graphics::event::CGEventType;

        assert!(super::is_release_event(CGEventType::KeyUp));
        assert!(super::is_release_event(CGEventType::LeftMouseUp));
        assert!(super::is_release_event(CGEventType::RightMouseUp));
        assert!(super::is_release_event(CGEventType::OtherMouseUp));
    }

    #[test]
    fn off_target_presses_and_motion_cannot_split_held_game_input() {
        use core_graphics::event::CGEventType;

        assert!(!super::is_release_event(CGEventType::KeyDown));
        assert!(!super::is_release_event(CGEventType::LeftMouseDown));
        assert!(!super::is_release_event(CGEventType::MouseMoved));
    }

    #[test]
    fn disabled_gamepad_capture_does_not_initialize_controller_observation() {
        let mut raw_host = RawHostInput::default();

        warm_gamepad_observer(false);
        capture_gamepad_snapshot(false, &mut raw_host);

        assert!(GAMEPAD_STATE.get().is_none());
        assert!(raw_host.gamepads.is_empty());
        assert!(raw_host.gamepad_buttons.is_empty());
        assert!(raw_host.gamepad_axes.is_empty());
    }
}
