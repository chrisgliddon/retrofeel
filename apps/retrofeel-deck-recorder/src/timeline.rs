use std::collections::{BTreeMap, HashMap};

use retrofeel_types::{
    device_ids::joypad, AnalogStick, InputFrame, InputState, KeyboardState, MouseState,
    RawGamepadInput, RawHostInput, RetroPadButtonBits, Triggers,
};

use crate::model::{AbsRange, InputCapability, InputDeviceInfo, InputDeviceSource, RawInputEvent};

const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;

const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;

const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const ABS_Z: u16 = 2;
const ABS_RX: u16 = 3;
const ABS_RY: u16 = 4;
const ABS_RZ: u16 = 5;
const ABS_HAT0X: u16 = 16;
const ABS_HAT0Y: u16 = 17;

const BTN_SOUTH: u16 = 304;
const BTN_EAST: u16 = 305;
// Linux's historical Xbox aliases are not geometric: BTN_X is 0x133
// (`BTN_NORTH`) and BTN_Y is 0x134 (`BTN_WEST`). Use the printed-letter
// aliases here, then expose RetroFeel's geometric West/North names below.
const BTN_X: u16 = 307;
const BTN_Y: u16 = 308;
const BTN_TL: u16 = 310;
const BTN_TR: u16 = 311;
const BTN_TL2: u16 = 312;
const BTN_TR2: u16 = 313;
const BTN_SELECT: u16 = 314;
const BTN_START: u16 = 315;
const BTN_MODE: u16 = 316;
const BTN_THUMBL: u16 = 317;
const BTN_THUMBR: u16 = 318;
const BTN_THUMB: u16 = 289;
const BTN_THUMB2: u16 = 290;
const BTN_BASE: u16 = 294;
const BTN_DPAD_UP: u16 = 544;
const BTN_DPAD_DOWN: u16 = 545;
const BTN_DPAD_LEFT: u16 = 546;
const BTN_DPAD_RIGHT: u16 = 547;
const BTN_GRIPL: u16 = 548;
const BTN_GRIPR: u16 = 549;
const BTN_GRIPL2: u16 = 550;
const BTN_GRIPR2: u16 = 551;

const BTN_LEFT: u16 = 272;
const BTN_RIGHT: u16 = 273;
const BTN_MIDDLE: u16 = 274;

#[derive(Debug, Default)]
struct DeviceState {
    keys: BTreeMap<u16, String>,
    axes: BTreeMap<u16, i32>,
    rel_x: i32,
    rel_y: i32,
    wheel_x: i32,
    wheel_y: i32,
}

pub fn sample_input_frames(
    frame_pts_us: &[u64],
    first_frame_boottime_us: u64,
    events: &[RawInputEvent],
    devices: &[InputDeviceInfo],
) -> Vec<InputFrame> {
    let mut events = events.to_vec();
    events.sort_by_key(|event| (event.boottime_us, event.device_id.clone()));
    let mut states = devices
        .iter()
        .map(|device| (device.device_id.clone(), DeviceState::default()))
        .collect::<HashMap<_, _>>();
    let mut event_cursor = 0;
    let mut frames = Vec::with_capacity(frame_pts_us.len());

    for (frame_index, elapsed_us) in frame_pts_us.iter().copied().enumerate() {
        let sample_time = first_frame_boottime_us.saturating_add(elapsed_us);
        while let Some(event) = events.get(event_cursor) {
            if event.boottime_us > sample_time {
                break;
            }
            if let Some(state) = states.get_mut(&event.device_id) {
                apply_event(state, event);
            }
            event_cursor += 1;
        }

        let mut ordered_gamepads = devices
            .iter()
            .filter(|device| device.captured_capabilities().gamepad)
            .filter_map(|device| states.get(&device.device_id).map(|state| (device, state)))
            .collect::<Vec<_>>();
        ordered_gamepads.sort_by_key(|(device, _)| {
            (
                match device.source {
                    InputDeviceSource::SteamVirtual => 0,
                    InputDeviceSource::PhysicalFallback => 1,
                    InputDeviceSource::AllowlistedEvdev => 2,
                },
                device.port.unwrap_or(u8::MAX),
                device.device_id.as_str(),
            )
        });

        let gamepads = ordered_gamepads
            .iter()
            .map(|(device, state)| raw_gamepad(device, state))
            .collect::<Vec<_>>();
        let primary = ordered_gamepads
            .iter()
            .find(|(device, _)| device.port == Some(0))
            .copied()
            .or_else(|| ordered_gamepads.first().copied());

        let mut keyboard = BTreeMap::<u16, String>::new();
        let mut mouse_dx: i32 = 0;
        let mut mouse_dy: i32 = 0;
        let mut wheel_x: i32 = 0;
        let mut wheel_y: i32 = 0;
        let mut mouse_buttons = 0;
        let mut has_mouse = false;
        for device in devices {
            let Some(state) = states.get(&device.device_id) else {
                continue;
            };
            let captured = device.captured_capabilities();
            if captured.keyboard {
                keyboard.extend(
                    state
                        .keys
                        .iter()
                        .filter(|(code, _)| key_capability(**code) == InputCapability::Keyboard)
                        .map(|(code, name)| (*code, name.clone())),
                );
            }
            if captured.mouse {
                has_mouse = true;
                mouse_dx = mouse_dx.saturating_add(state.rel_x);
                mouse_dy = mouse_dy.saturating_add(state.rel_y);
                wheel_x = wheel_x.saturating_add(state.wheel_x);
                wheel_y = wheel_y.saturating_add(state.wheel_y);
                mouse_buttons |= mouse_button_bits(state);
            }
        }

        let (state, legacy_buttons, legacy_axes) = if let Some((device, gamepad_state)) = primary {
            let state = mapped_input(device, gamepad_state);
            let raw = raw_gamepad(device, gamepad_state);
            (state, raw.buttons, raw.axes)
        } else {
            (
                InputState {
                    mouse: MouseState {
                        dx: mouse_dx,
                        dy: mouse_dy,
                        wheel_x,
                        wheel_y,
                        buttons: mouse_buttons,
                    },
                    ..Default::default()
                },
                Vec::new(),
                BTreeMap::new(),
            )
        };
        let mut state = state;
        state.mouse = MouseState {
            dx: mouse_dx,
            dy: mouse_dy,
            wheel_x,
            wheel_y,
            buttons: mouse_buttons,
        };
        state.keyboard = KeyboardState::default();

        frames.push(InputFrame {
            frame: frame_index as u64,
            elapsed_us: Some(elapsed_us),
            port: 0,
            state,
            raw_host: Some(RawHostInput {
                keyboard_keys: keyboard.values().cloned().collect(),
                keyboard_key_codes: keyboard.keys().copied().collect(),
                gamepad_buttons: legacy_buttons,
                gamepad_axes: legacy_axes,
                gamepads,
                mouse: has_mouse.then_some(state.mouse),
            }),
        });

        for state in states.values_mut() {
            state.rel_x = 0;
            state.rel_y = 0;
            state.wheel_x = 0;
            state.wheel_y = 0;
        }
    }
    frames
}

fn apply_event(state: &mut DeviceState, event: &RawInputEvent) {
    if event.lifecycle.is_some() {
        *state = DeviceState::default();
        return;
    }
    match event.event_type {
        EV_KEY => {
            if event.value == 0 {
                state.keys.remove(&event.code);
            } else {
                state.keys.insert(
                    event.code,
                    event
                        .name
                        .clone()
                        .unwrap_or_else(|| key_name(event.code).to_string()),
                );
            }
        }
        EV_ABS => {
            state.axes.insert(event.code, event.value);
        }
        EV_REL if event.code == REL_X => {
            state.rel_x = state.rel_x.saturating_add(event.value);
        }
        EV_REL if event.code == REL_Y => {
            state.rel_y = state.rel_y.saturating_add(event.value);
        }
        EV_REL if event.code == REL_HWHEEL => {
            state.wheel_x = state.wheel_x.saturating_add(event.value);
        }
        EV_REL if event.code == REL_WHEEL => {
            state.wheel_y = state.wheel_y.saturating_add(event.value);
        }
        _ => {}
    }
}

fn mapped_input(device: &InputDeviceInfo, state: &DeviceState) -> InputState {
    let mut buttons = RetroPadButtonBits::EMPTY;
    for code in state.keys.keys().copied() {
        if let Some(bit) = gamepad_button_bit(code) {
            buttons.set(bit);
        }
    }
    match state.axes.get(&ABS_HAT0X).copied().unwrap_or(0) {
        value if value < 0 => buttons.set(joypad::LEFT),
        value if value > 0 => buttons.set(joypad::RIGHT),
        _ => {}
    }
    match state.axes.get(&ABS_HAT0Y).copied().unwrap_or(0) {
        value if value < 0 => buttons.set(joypad::UP),
        value if value > 0 => buttons.set(joypad::DOWN),
        _ => {}
    }

    InputState {
        buttons,
        analog_l: AnalogStick {
            x: signed_axis(device, state, ABS_X),
            y: signed_axis(device, state, ABS_Y),
        },
        analog_r: AnalogStick {
            x: signed_axis(device, state, ABS_RX),
            y: signed_axis(device, state, ABS_RY),
        },
        triggers: Triggers {
            l: unsigned_axis(device, state, ABS_Z),
            r: unsigned_axis(device, state, ABS_RZ),
        },
        ..Default::default()
    }
}

fn raw_gamepad(device: &InputDeviceInfo, state: &DeviceState) -> RawGamepadInput {
    let mut buttons = state
        .keys
        .iter()
        .filter(|(code, _)| is_raw_gamepad_button(**code))
        .map(|(code, name)| friendly_gamepad_name(*code, name))
        .collect::<Vec<_>>();
    if state.axes.get(&ABS_HAT0X).copied().unwrap_or(0) < 0 {
        buttons.push("DPadLeft".into());
    }
    if state.axes.get(&ABS_HAT0X).copied().unwrap_or(0) > 0 {
        buttons.push("DPadRight".into());
    }
    if state.axes.get(&ABS_HAT0Y).copied().unwrap_or(0) < 0 {
        buttons.push("DPadUp".into());
    }
    if state.axes.get(&ABS_HAT0Y).copied().unwrap_or(0) > 0 {
        buttons.push("DPadDown".into());
    }
    buttons.sort();
    buttons.dedup();

    let mut axes = BTreeMap::new();
    for (code, label) in [
        (ABS_X, "LeftStickX"),
        (ABS_Y, "LeftStickY"),
        (ABS_RX, "RightStickX"),
        (ABS_RY, "RightStickY"),
    ] {
        let mut value = signed_axis(device, state, code) as f32 / 32767.0;
        if matches!(code, ABS_Y | ABS_RY) {
            // RawHostInput follows Bevy's gamepad convention: stick up is
            // positive. Mapped InputState follows libretro: down is positive.
            value = -value;
        }
        axes.insert(label.into(), value);
    }
    for (code, label) in [(ABS_Z, "LeftTrigger"), (ABS_RZ, "RightTrigger")] {
        axes.insert(
            label.into(),
            unsigned_axis(device, state, code) as f32 / 32767.0,
        );
    }

    RawGamepadInput {
        device_id: device.device_id.clone(),
        port: device.port,
        name: device.name.clone(),
        buttons,
        axes,
    }
}

fn is_raw_gamepad_button(code: u16) -> bool {
    gamepad_button_bit(code).is_some()
        || matches!(
            code,
            BTN_MODE
                | BTN_THUMB
                | BTN_THUMB2
                | BTN_BASE
                | BTN_GRIPL
                | BTN_GRIPR
                | BTN_GRIPL2
                | BTN_GRIPR2
        )
}

fn signed_axis(device: &InputDeviceInfo, state: &DeviceState, code: u16) -> i16 {
    let value = state.axes.get(&code).copied().unwrap_or(0);
    let range = device.abs_ranges.get(&code).copied().unwrap_or(AbsRange {
        minimum: -32768,
        maximum: 32767,
        flat: 0,
    });
    if range.maximum <= range.minimum {
        return 0;
    }
    let center = (range.minimum as f64 + range.maximum as f64) / 2.0;
    let scale = if value as f64 >= center {
        (range.maximum as f64 - center).max(1.0)
    } else {
        (center - range.minimum as f64).max(1.0)
    };
    (((value as f64 - center) / scale) * 32767.0)
        .round()
        .clamp(-32767.0, 32767.0) as i16
}

fn unsigned_axis(device: &InputDeviceInfo, state: &DeviceState, code: u16) -> i16 {
    let value = state.axes.get(&code).copied().unwrap_or(0);
    let range = device.abs_ranges.get(&code).copied().unwrap_or(AbsRange {
        minimum: 0,
        maximum: 255,
        flat: 0,
    });
    let width = (range.maximum - range.minimum).max(1) as f64;
    (((value - range.minimum) as f64 / width) * 32767.0)
        .round()
        .clamp(0.0, 32767.0) as i16
}

fn gamepad_button_bit(code: u16) -> Option<u16> {
    Some(match code {
        BTN_SOUTH => joypad::A,
        BTN_EAST => joypad::B,
        BTN_X => joypad::X,
        BTN_Y => joypad::Y,
        BTN_TL => joypad::L,
        BTN_TR => joypad::R,
        BTN_TL2 => joypad::L2,
        BTN_TR2 => joypad::R2,
        BTN_SELECT => joypad::SELECT,
        BTN_START => joypad::START,
        BTN_THUMBL => joypad::L3,
        BTN_THUMBR => joypad::R3,
        BTN_DPAD_UP => joypad::UP,
        BTN_DPAD_DOWN => joypad::DOWN,
        BTN_DPAD_LEFT => joypad::LEFT,
        BTN_DPAD_RIGHT => joypad::RIGHT,
        _ => return None,
    })
}

fn friendly_gamepad_name(code: u16, fallback: &str) -> String {
    match code {
        BTN_SOUTH => "South",
        BTN_EAST => "East",
        BTN_X => "West",
        BTN_Y => "North",
        BTN_TL => "LeftShoulder",
        BTN_TR => "RightShoulder",
        BTN_TL2 => "LeftTriggerButton",
        BTN_TR2 => "RightTriggerButton",
        BTN_SELECT => "Select",
        BTN_START => "Start",
        BTN_MODE => "Mode",
        BTN_THUMBL => "LeftThumb",
        BTN_THUMBR => "RightThumb",
        BTN_THUMB => "LeftPadClick",
        BTN_THUMB2 => "RightPadClick",
        BTN_BASE => "QuickAccess",
        BTN_DPAD_UP => "DPadUp",
        BTN_DPAD_DOWN => "DPadDown",
        BTN_DPAD_LEFT => "DPadLeft",
        BTN_DPAD_RIGHT => "DPadRight",
        BTN_GRIPL => "L4",
        BTN_GRIPR => "R4",
        BTN_GRIPL2 => "L5",
        BTN_GRIPR2 => "R5",
        _ => fallback,
    }
    .to_string()
}

fn mouse_button_bits(state: &DeviceState) -> u8 {
    let mut bits = 0;
    if state.keys.contains_key(&BTN_LEFT) {
        bits |= 1;
    }
    if state.keys.contains_key(&BTN_RIGHT) {
        bits |= 1 << 1;
    }
    if state.keys.contains_key(&BTN_MIDDLE) {
        bits |= 1 << 2;
    }
    bits
}

fn key_capability(code: u16) -> InputCapability {
    if (0x110..=0x11f).contains(&code) {
        InputCapability::Mouse
    } else if (0x120..=0x15f).contains(&code)
        || (0x220..=0x223).contains(&code)
        || (0x2c0..=0x2ff).contains(&code)
    {
        InputCapability::Gamepad
    } else {
        InputCapability::Keyboard
    }
}

fn key_name(code: u16) -> &'static str {
    match code {
        BTN_SOUTH => "BTN_SOUTH",
        BTN_EAST => "BTN_EAST",
        BTN_X => "BTN_X",
        BTN_Y => "BTN_Y",
        BTN_TL => "BTN_TL",
        BTN_TR => "BTN_TR",
        BTN_TL2 => "BTN_TL2",
        BTN_TR2 => "BTN_TR2",
        BTN_SELECT => "BTN_SELECT",
        BTN_START => "BTN_START",
        BTN_MODE => "BTN_MODE",
        BTN_THUMBL => "BTN_THUMBL",
        BTN_THUMBR => "BTN_THUMBR",
        BTN_THUMB => "BTN_THUMB",
        BTN_THUMB2 => "BTN_THUMB2",
        BTN_BASE => "BTN_BASE",
        BTN_GRIPL => "BTN_GRIPL",
        BTN_GRIPR => "BTN_GRIPR",
        BTN_GRIPL2 => "BTN_GRIPL2",
        BTN_GRIPR2 => "BTN_GRIPR2",
        BTN_LEFT => "BTN_LEFT",
        BTN_RIGHT => "BTN_RIGHT",
        BTN_MIDDLE => "BTN_MIDDLE",
        _ => "KEY_UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AbsRange, InputCapabilities, InputDeviceInfo, InputDeviceLifecycle, RawInputEvent,
    };

    const KEY_A: u16 = 30;

    fn pad() -> InputDeviceInfo {
        InputDeviceInfo {
            device_id: "steam-pad-0".into(),
            event_path: "/dev/input/event18".into(),
            name: "Microsoft X-Box 360 pad 0".into(),
            bus_type: 3,
            vendor: 0x28de,
            product: 0x11ff,
            version: 1,
            unique: None,
            physical_path: Some("steam/input0".into()),
            is_virtual: true,
            is_steam_virtual: true,
            port: Some(0),
            source: crate::model::InputDeviceSource::SteamVirtual,
            is_gamepad: true,
            is_keyboard: false,
            is_mouse: false,
            admitted_capabilities: Some(InputCapabilities {
                gamepad: true,
                keyboard: false,
                mouse: false,
            }),
            admission_reason: Some("test".into()),
            abs_ranges: [
                (
                    ABS_X,
                    AbsRange {
                        minimum: -32768,
                        maximum: 32767,
                        flat: 128,
                    },
                ),
                (
                    ABS_Z,
                    AbsRange {
                        minimum: 0,
                        maximum: 255,
                        flat: 0,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn event(time: u64, event_type: u16, code: u16, value: i32) -> RawInputEvent {
        RawInputEvent {
            boottime_us: time,
            device_id: "steam-pad-0".into(),
            event_type,
            code,
            value,
            name: None,
            lifecycle: None,
        }
    }

    fn composite() -> InputDeviceInfo {
        let mut device = pad();
        device.name = "Composite test input".into();
        device.source = crate::model::InputDeviceSource::AllowlistedEvdev;
        device.is_keyboard = true;
        device.is_mouse = true;
        device.admitted_capabilities = Some(InputCapabilities {
            gamepad: true,
            keyboard: true,
            mouse: true,
        });
        device
    }

    fn named_event(time: u64, event_type: u16, code: u16, value: i32, name: &str) -> RawInputEvent {
        let mut event = event(time, event_type, code, value);
        event.name = Some(name.into());
        event
    }

    #[test]
    fn samples_held_state_at_variable_frame_times() {
        let events = vec![
            event(1_005_000, EV_KEY, BTN_SOUTH, 1),
            event(1_018_000, EV_ABS, ABS_X, 32767),
            event(1_025_000, EV_KEY, BTN_SOUTH, 0),
            event(1_030_000, EV_ABS, ABS_Z, 255),
        ];
        let frames =
            sample_input_frames(&[0, 10_000, 20_000, 40_000], 1_000_000, &events, &[pad()]);
        assert_eq!(frames.len(), 4);
        assert!(!frames[0].state.buttons.has(joypad::A));
        assert!(frames[1].state.buttons.has(joypad::A));
        assert!(frames[2].state.analog_l.x > 32_000);
        assert!(!frames[3].state.buttons.has(joypad::A));
        assert_eq!(frames[3].state.triggers.l, 32767);
        assert_eq!(frames[3].elapsed_us, Some(40_000));
    }

    #[test]
    fn retains_all_virtual_gamepads_but_mirrors_port_zero() {
        let mut second = pad();
        second.device_id = "steam-pad-1".into();
        second.name = "Microsoft X-Box 360 pad 1".into();
        second.port = Some(1);
        let mut second_event = event(1_000_000, EV_KEY, BTN_EAST, 1);
        second_event.device_id = second.device_id.clone();
        let frames = sample_input_frames(&[0], 1_000_000, &[second_event], &[pad(), second]);
        let raw = frames[0].raw_host.as_ref().unwrap();
        assert_eq!(raw.gamepads.len(), 2);
        assert!(raw.gamepad_buttons.is_empty());
        assert!(raw.gamepads[1].buttons.contains(&"East".to_string()));
    }

    #[test]
    fn retains_physical_track_but_keeps_virtual_as_compatibility_primary() {
        let virtual_pad = pad();
        let mut physical_pad = pad();
        physical_pad.device_id = "physical-pad".into();
        physical_pad.name = "Allowlisted physical controller".into();
        physical_pad.source = crate::model::InputDeviceSource::PhysicalFallback;

        let virtual_event = event(1_000_000, EV_KEY, BTN_SOUTH, 1);
        let mut physical_event = event(1_000_000, EV_KEY, BTN_EAST, 1);
        physical_event.device_id = physical_pad.device_id.clone();

        let frames = sample_input_frames(
            &[0],
            1_000_000,
            &[physical_event, virtual_event],
            &[physical_pad, virtual_pad],
        );
        let frame = &frames[0];
        let raw = frame.raw_host.as_ref().unwrap();

        assert_eq!(raw.gamepads.len(), 2);
        assert!(raw
            .gamepads
            .iter()
            .any(|gamepad| gamepad.device_id == "physical-pad"
                && gamepad.buttons.contains(&"East".to_string())));
        assert_eq!(raw.gamepad_buttons, ["South"]);
        assert!(frame.state.buttons.has(joypad::A));
        assert!(!frame.state.buttons.has(joypad::B));
    }

    #[test]
    fn maps_linux_xbox_btn_x_and_btn_y_to_geometric_names() {
        // Linux aliases BTN_NORTH (0x133) to BTN_X and BTN_WEST (0x134) to
        // BTN_Y. RetroFeel's portable names are geometric: X is West and Y
        // is North.
        let events = vec![
            event(1_000_000, EV_KEY, BTN_X, 1),
            event(1_010_000, EV_KEY, BTN_X, 0),
            event(1_020_000, EV_KEY, BTN_Y, 1),
        ];
        let frames = sample_input_frames(&[0, 10_000, 20_000], 1_000_000, &events, &[pad()]);

        let xbox_x = &frames[0];
        assert!(xbox_x.state.buttons.has(joypad::X));
        assert!(!xbox_x.state.buttons.has(joypad::Y));
        assert_eq!(xbox_x.raw_host.as_ref().unwrap().gamepad_buttons, ["West"]);

        let xbox_y = &frames[2];
        assert!(xbox_y.state.buttons.has(joypad::Y));
        assert!(!xbox_y.state.buttons.has(joypad::X));
        assert_eq!(xbox_y.raw_host.as_ref().unwrap().gamepad_buttons, ["North"]);
    }

    #[test]
    fn preserves_steam_deck_grips_pads_and_quick_access_as_raw_buttons() {
        let events = [
            (BTN_GRIPL, "L4"),
            (BTN_GRIPR, "R4"),
            (BTN_GRIPL2, "L5"),
            (BTN_GRIPR2, "R5"),
            (BTN_THUMB, "LeftPadClick"),
            (BTN_THUMB2, "RightPadClick"),
            (BTN_BASE, "QuickAccess"),
        ]
        .into_iter()
        .map(|(code, _)| event(1_000_000, EV_KEY, code, 1))
        .collect::<Vec<_>>();
        let frames = sample_input_frames(&[0], 1_000_000, &events, &[pad()]);
        let buttons = &frames[0].raw_host.as_ref().unwrap().gamepad_buttons;

        for expected in [
            "L4",
            "R4",
            "L5",
            "R5",
            "LeftPadClick",
            "RightPadClick",
            "QuickAccess",
        ] {
            assert!(
                buttons.contains(&expected.to_string()),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn holds_keyboard_codes_and_names_until_release() {
        let mut keyboard = composite();
        keyboard.is_gamepad = false;
        keyboard.is_mouse = false;
        keyboard.admitted_capabilities = Some(InputCapabilities {
            gamepad: false,
            keyboard: true,
            mouse: false,
        });
        let events = vec![
            named_event(1_005_000, EV_KEY, KEY_A, 1, "KEY_A"),
            named_event(1_025_000, EV_KEY, KEY_A, 0, "KEY_A"),
        ];
        let frames = sample_input_frames(
            &[0, 10_000, 20_000, 30_000],
            1_000_000,
            &events,
            &[keyboard],
        );
        for frame in &frames[1..3] {
            let raw = frame.raw_host.as_ref().unwrap();
            assert_eq!(raw.keyboard_keys, ["KEY_A"]);
            assert_eq!(raw.keyboard_key_codes, [KEY_A]);
        }
        assert!(frames[0]
            .raw_host
            .as_ref()
            .unwrap()
            .keyboard_keys
            .is_empty());
        assert!(frames[3]
            .raw_host
            .as_ref()
            .unwrap()
            .keyboard_keys
            .is_empty());
    }

    #[test]
    fn accumulates_relative_motion_at_event_rate_then_resets_per_frame() {
        let mut mouse = composite();
        mouse.is_gamepad = false;
        mouse.is_keyboard = false;
        mouse.admitted_capabilities = Some(InputCapabilities {
            gamepad: false,
            keyboard: false,
            mouse: true,
        });
        let events = vec![
            event(1_001_000, EV_REL, REL_X, 2),
            event(1_002_000, EV_REL, REL_X, 3),
            event(1_008_000, EV_REL, REL_Y, -4),
            event(1_015_000, EV_REL, REL_X, 7),
        ];
        let frames =
            sample_input_frames(&[0, 10_000, 20_000, 30_000], 1_000_000, &events, &[mouse]);
        assert_eq!(frames[1].state.mouse.dx, 5);
        assert_eq!(frames[1].state.mouse.dy, -4);
        assert_eq!(frames[2].state.mouse.dx, 7);
        assert_eq!(frames[2].state.mouse.dy, 0);
        assert_eq!(frames[3].state.mouse.dx, 0);
    }

    #[test]
    fn samples_held_mouse_buttons_and_both_wheel_axes() {
        let mut mouse = composite();
        mouse.admitted_capabilities = Some(InputCapabilities {
            gamepad: false,
            keyboard: false,
            mouse: true,
        });
        let events = vec![
            named_event(1_001_000, EV_KEY, BTN_LEFT, 1, "BTN_LEFT"),
            named_event(1_002_000, EV_REL, REL_WHEEL, 1, "REL_WHEEL"),
            named_event(1_003_000, EV_REL, REL_HWHEEL, -2, "REL_HWHEEL"),
            named_event(1_018_000, EV_KEY, BTN_LEFT, 0, "BTN_LEFT"),
        ];
        let frames = sample_input_frames(&[0, 10_000, 20_000], 1_000_000, &events, &[mouse]);
        assert_eq!(frames[1].state.mouse.buttons, 1);
        assert_eq!(frames[1].state.mouse.wheel_y, 1);
        assert_eq!(frames[1].state.mouse.wheel_x, -2);
        assert_eq!(frames[2].state.mouse.buttons, 0);
        assert_eq!(frames[2].state.mouse.wheel_y, 0);
        assert_eq!(frames[2].state.mouse.wheel_x, 0);
    }

    #[test]
    fn composite_device_contributes_each_capability_once() {
        let events = vec![
            named_event(1_001_000, EV_KEY, BTN_SOUTH, 1, "BTN_SOUTH"),
            named_event(1_002_000, EV_KEY, KEY_A, 1, "KEY_A"),
            named_event(1_003_000, EV_KEY, BTN_LEFT, 1, "BTN_LEFT"),
            named_event(1_004_000, EV_REL, REL_X, 9, "REL_X"),
        ];
        let frames = sample_input_frames(&[10_000], 1_000_000, &events, &[composite()]);
        let raw = frames[0].raw_host.as_ref().unwrap();
        assert_eq!(raw.gamepads.len(), 1);
        assert_eq!(raw.gamepads[0].buttons, ["South"]);
        assert_eq!(raw.keyboard_keys, ["KEY_A"]);
        assert_eq!(raw.keyboard_key_codes, [KEY_A]);
        assert_eq!(raw.mouse.unwrap().buttons, 1);
        assert_eq!(raw.mouse.unwrap().dx, 9);
    }

    #[test]
    fn lifecycle_churn_resets_held_state_for_a_recreated_identity() {
        let mut removed = RawInputEvent::lifecycle(
            1_015_000,
            "steam-pad-0".into(),
            InputDeviceLifecycle::Removed,
        );
        let added =
            RawInputEvent::lifecycle(1_025_000, "steam-pad-0".into(), InputDeviceLifecycle::Added);
        removed.name = Some("old event path removed".into());
        let events = vec![
            named_event(1_005_000, EV_KEY, KEY_A, 1, "KEY_A"),
            removed,
            added,
            named_event(1_030_000, EV_KEY, KEY_A, 1, "KEY_A"),
        ];
        let frames = sample_input_frames(
            &[10_000, 20_000, 28_000, 35_000],
            1_000_000,
            &events,
            &[composite()],
        );
        assert_eq!(
            frames[0].raw_host.as_ref().unwrap().keyboard_keys,
            ["KEY_A"]
        );
        assert!(frames[1]
            .raw_host
            .as_ref()
            .unwrap()
            .keyboard_keys
            .is_empty());
        assert!(frames[2]
            .raw_host
            .as_ref()
            .unwrap()
            .keyboard_keys
            .is_empty());
        assert_eq!(
            frames[3].raw_host.as_ref().unwrap().keyboard_keys,
            ["KEY_A"]
        );
    }

    #[test]
    fn admitted_pre_roll_event_is_present_in_the_first_frame() {
        let event = named_event(990_000, EV_KEY, KEY_A, 1, "KEY_A");
        let frames = sample_input_frames(&[0], 1_000_000, &[event], &[composite()]);
        assert_eq!(
            frames[0].raw_host.as_ref().unwrap().keyboard_keys,
            ["KEY_A"]
        );
    }
}
