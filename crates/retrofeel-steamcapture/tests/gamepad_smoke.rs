//! Live gamepad smoke test — requires a connected controller (e.g. the Xbox
//! Wireless Controller over Bluetooth). `#[ignore]`d because CI has no pads.
//!
//! ```sh
//! cargo test -p retrofeel-steamcapture --test gamepad_smoke -- --ignored --nocapture
//! ```

#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use objc2_game_controller::{GCController, GCDevice};

#[test]
#[ignore]
#[cfg(not(target_os = "macos"))]
fn gamepad_smoke() {
    panic!("the GameController smoke test is macOS-only");
}

#[test]
#[ignore]
#[cfg(target_os = "macos")]
fn gamepad_smoke() {
    unsafe {
        GCController::setShouldMonitorBackgroundEvents(true);
    }

    // Give GameController a moment to publish already-connected devices.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut pads: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        let controllers = unsafe { GCController::controllers() };
        pads = (&*controllers)
            .into_iter()
            .map(|controller| {
                let name = unsafe { controller.vendorName() }
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| "Game Controller".into());
                let profile = if unsafe { controller.extendedGamepad() }.is_some() {
                    "extended"
                } else {
                    "unsupported"
                };
                format!("{name} ({profile})")
            })
            .collect();
        if !pads.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    println!("gamepads: {pads:#?}");
    assert!(
        !pads.is_empty(),
        "no gamepads visible through GameController — check the controller connection"
    );
}
