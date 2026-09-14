//! Input mapping: Bevy keyboard/gamepad/mouse → RetroPad `InputState`.
//!
//! Phase 2 uses a hardcoded default mapping. Binding UI comes in Phase 4.

use bevy::input::gamepad::{Gamepad, GamepadAxis, GamepadButton};
use bevy::input::mouse::{AccumulatedMouseMotion, MouseButton};
use bevy::prelude::*;
use std::collections::BTreeMap;

use retrofeel_types::device_ids::joypad;
use retrofeel_types::{
    AnalogStick, InputBindingSet, InputState, KeyboardState, MouseState, RawHostInput,
    RetroPadButtonBits, Triggers,
};

/// Default keyboard mapping (RetroPad face buttons on an Xbox-style layout).
const KB_FACE_A: KeyCode = KeyCode::KeyX;
const KB_FACE_B: KeyCode = KeyCode::KeyZ;
const KB_FACE_X: KeyCode = KeyCode::KeyS;
const KB_FACE_Y: KeyCode = KeyCode::KeyA;
const KB_L: KeyCode = KeyCode::KeyQ;
const KB_R: KeyCode = KeyCode::KeyW;
const KB_SELECT: KeyCode = KeyCode::ShiftRight;
const KB_START: KeyCode = KeyCode::Enter;
const KB_DPAD_UP: KeyCode = KeyCode::ArrowUp;
const KB_DPAD_DOWN: KeyCode = KeyCode::ArrowDown;
const KB_DPAD_LEFT: KeyCode = KeyCode::ArrowLeft;
const KB_DPAD_RIGHT: KeyCode = KeyCode::ArrowRight;
const KB_LSTICK_X_NEG: KeyCode = KeyCode::KeyJ;
const KB_LSTICK_X_POS: KeyCode = KeyCode::KeyL;
const KB_LSTICK_Y_NEG: KeyCode = KeyCode::KeyK;
const KB_LSTICK_Y_POS: KeyCode = KeyCode::KeyI;

const ANALOG_MAX: i16 = 0x7fff;

/// Build an `InputState` from the current Bevy input state.
///
/// Populates the full IR: digital buttons, both analog sticks, both triggers,
/// mouse deltas + buttons, and held keyboard scancodes (as libretro RETROK
/// values for `DEVICE_KEYBOARD` cores). Earlier versions filled buttons + the
/// left stick only, leaving `analog_r`, `triggers`, `mouse`, and `keyboard`
/// always zero/empty — which made exports normalize fields that were never
/// populated and gave keyboard-driven cores (DOSBox-class) no input.
pub fn map_input(
    keys: &ButtonInput<KeyCode>,
    gamepads: &Query<&Gamepad>,
    bindings: Option<&InputBindingSet>,
    mouse_buttons: &ButtonInput<MouseButton>,
    mouse_motion: &AccumulatedMouseMotion,
) -> InputState {
    let mut buttons = RetroPadButtonBits::EMPTY;

    // Keyboard → digital buttons.
    let default_bindings = default_input_bindings();
    let binding_set = bindings.unwrap_or(&default_bindings);
    for control in keyboard_controls() {
        if binding_pressed(keys, binding_set, control.name, control.default_key) {
            buttons.set(control.bit);
        }
    }

    // Keyboard → left analog stick (digital stand-in).
    let mut lx: i16 = 0;
    let mut ly: i16 = 0;
    for control in keyboard_analog_controls() {
        if binding_pressed(keys, binding_set, control.name, control.default_key) {
            match control.axis {
                AnalogAxis::LeftX => lx = control.value,
                AnalogAxis::LeftY => ly = control.value,
            }
        }
    }

    let mut analog_l = AnalogStick { x: lx, y: ly };
    let mut analog_r = AnalogStick { x: 0, y: 0 };
    let mut triggers = Triggers::default();

    // First connected gamepad overrides its contributions.
    if let Some(pad) = gamepads.iter().next() {
        for control in gamepad_controls() {
            if gamepad_binding_pressed(pad, binding_set, control.name, control.default_button) {
                buttons.set(control.bit);
            }
        }

        // Analog sticks (libretro: +X right, +Y down).
        let gx = pad.get(GamepadAxis::LeftStickX).unwrap_or(0.0);
        let gy = pad.get(GamepadAxis::LeftStickY).unwrap_or(0.0);
        // Invert Y: gamepad up is +1, libretro down is +.
        analog_l = AnalogStick {
            x: (gx * ANALOG_MAX as f32) as i16,
            y: (-gy * ANALOG_MAX as f32) as i16,
        };
        let rx = pad.get(GamepadAxis::RightStickX).unwrap_or(0.0);
        let ry = pad.get(GamepadAxis::RightStickY).unwrap_or(0.0);
        analog_r = AnalogStick {
            x: (rx * ANALOG_MAX as f32) as i16,
            y: (-ry * ANALOG_MAX as f32) as i16,
        };

        // Triggers: libretro reports analog triggers in [0, 0x7fff] under
        // DEVICE_ANALOG index=2 id=L2/R2. Bevy exposes the analog trigger as
        // the LeftZ/RightZ axis (the digital trigger button is
        // GamepadButton::LeftTrigger2/RightTrigger2).
        let lt = pad.get(GamepadAxis::LeftZ).unwrap_or(0.0);
        let rt = pad.get(GamepadAxis::RightZ).unwrap_or(0.0);
        triggers = Triggers {
            l: (lt.clamp(0.0, 1.0) * ANALOG_MAX as f32) as i16,
            r: (rt.clamp(0.0, 1.0) * ANALOG_MAX as f32) as i16,
        };
    }

    // Mouse → deltas + buttons (libretro DEVICE_MOUSE). dx/dy are the per-frame
    // motion delta; button bits: 0=left, 1=right, 2=middle, 3=wheel-up,
    // 4=wheel-down. Wheel is not exposed here (Bevy's AccumulatedMouseMotion
    // covers only motion); a future pass can add wheel via MouseWheel events.
    let mut mouse_buttons_bits = 0u8;
    if mouse_buttons.pressed(MouseButton::Left) {
        mouse_buttons_bits |= 1;
    }
    if mouse_buttons.pressed(MouseButton::Right) {
        mouse_buttons_bits |= 1 << 1;
    }
    if mouse_buttons.pressed(MouseButton::Middle) {
        mouse_buttons_bits |= 1 << 2;
    }
    let mouse = MouseState {
        dx: mouse_motion.delta.x as i32,
        dy: mouse_motion.delta.y as i32,
        buttons: mouse_buttons_bits,
        ..Default::default()
    };

    // Keyboard scancodes as libretro RETROK values. We map each held Bevy
    // KeyCode to its RETROK_* equivalent so `DEVICE_KEYBOARD` cores (DOSBox,
    // home computers) actually receive input. Capped at 8 simultaneous keys
    // (the IR's fixed buffer).
    let keyboard = keyboard_state(keys);

    let _ = (lx, ly);
    InputState {
        buttons,
        analog_l,
        analog_r,
        triggers,
        mouse,
        keyboard,
    }
}

/// Translate held Bevy keys into the libretro `RETROK_*` scancode space so the
/// IR's `keyboard` field is populated for `DEVICE_KEYBOARD` cores.
fn keyboard_state(keys: &ButtonInput<KeyCode>) -> KeyboardState {
    let mut out = [0i32; 8];
    let mut count = 0u8;
    for key in keys.get_pressed() {
        if count as usize >= out.len() {
            break;
        }
        if let Some(retrok) = keycode_to_retrok(*key) {
            out[count as usize] = retrok;
            count += 1;
        }
    }
    KeyboardState { keys: out, count }
}

/// Map a Bevy `KeyCode` to the corresponding libretro `RETROK_*` value (the
/// subset retrofeel cores are likely to query). `0` is RETROK_UNKNOWN; we
/// skip unmapped keys above so they don't pollute the buffer.
fn keycode_to_retrok(key: KeyCode) -> Option<i32> {
    use KeyCode as K;
    Some(match key {
        K::Backspace => 8,
        K::Tab => 9,
        K::Enter => 13,
        K::Escape => 27,
        K::Space => 32,
        K::Quote => 39,
        K::Comma => 44,
        K::Minus => 45,
        K::Period => 46,
        K::Slash => 47,
        K::Digit0 => 48,
        K::Digit1 => 49,
        K::Digit2 => 50,
        K::Digit3 => 51,
        K::Digit4 => 52,
        K::Digit5 => 53,
        K::Digit6 => 54,
        K::Digit7 => 55,
        K::Digit8 => 56,
        K::Digit9 => 57,
        K::Semicolon => 59,
        K::Equal => 61,
        K::BracketLeft => 91,
        K::Backslash => 92,
        K::BracketRight => 93,
        K::Backquote => 96,
        K::KeyA => 97,
        K::KeyB => 98,
        K::KeyC => 99,
        K::KeyD => 100,
        K::KeyE => 101,
        K::KeyF => 102,
        K::KeyG => 103,
        K::KeyH => 104,
        K::KeyI => 105,
        K::KeyJ => 106,
        K::KeyK => 107,
        K::KeyL => 108,
        K::KeyM => 109,
        K::KeyN => 110,
        K::KeyO => 111,
        K::KeyP => 112,
        K::KeyQ => 113,
        K::KeyR => 114,
        K::KeyS => 115,
        K::KeyT => 116,
        K::KeyU => 117,
        K::KeyV => 118,
        K::KeyW => 119,
        K::KeyX => 120,
        K::KeyY => 121,
        K::KeyZ => 122,
        K::Delete => 127,
        K::Numpad0 => 256,
        K::Numpad1 => 257,
        K::Numpad2 => 258,
        K::Numpad3 => 259,
        K::Numpad4 => 260,
        K::Numpad5 => 261,
        K::Numpad6 => 262,
        K::Numpad7 => 263,
        K::Numpad8 => 264,
        K::Numpad9 => 265,
        K::NumpadDecimal => 266,
        K::NumpadDivide => 267,
        K::NumpadMultiply => 268,
        K::NumpadSubtract => 269,
        K::NumpadAdd => 270,
        K::NumpadEnter => 271,
        K::NumpadEqual => 272,
        K::ArrowUp => 273,
        K::ArrowDown => 274,
        K::ArrowRight => 275,
        K::ArrowLeft => 276,
        K::Insert => 277,
        K::Home => 278,
        K::End => 279,
        K::PageUp => 280,
        K::PageDown => 281,
        K::F1 => 282,
        K::F2 => 283,
        K::F3 => 284,
        K::F4 => 285,
        K::F5 => 286,
        K::F6 => 287,
        K::F7 => 288,
        K::F8 => 289,
        K::F9 => 290,
        K::F10 => 291,
        K::F11 => 292,
        K::F12 => 293,
        K::F13 => 294,
        K::F14 => 295,
        K::F15 => 296,
        K::NumLock => 300,
        K::CapsLock => 301,
        K::ScrollLock => 302,
        K::ShiftRight => 303,
        K::ShiftLeft => 304,
        K::ControlRight => 305,
        K::ControlLeft => 306,
        K::AltRight => 307,
        K::AltLeft => 308,
        _ => return None,
    })
}

pub fn capture_raw_host_input(
    keys: &ButtonInput<KeyCode>,
    gamepads: &Query<&Gamepad>,
    mouse_buttons: &ButtonInput<MouseButton>,
    mouse_motion: &AccumulatedMouseMotion,
) -> RawHostInput {
    let mut keyboard_keys: Vec<String> = keys.get_pressed().map(|key| format!("{key:?}")).collect();
    keyboard_keys.sort();

    let mut gamepad_buttons = Vec::new();
    let mut gamepad_axes = BTreeMap::new();
    if let Some(pad) = gamepads.iter().next() {
        for button in candidate_gamepad_buttons() {
            if pad.pressed(*button) {
                gamepad_buttons.push(gamepad_button_to_binding(*button));
            }
        }
        gamepad_axes.insert(
            "LeftStickX".to_string(),
            pad.get(GamepadAxis::LeftStickX).unwrap_or(0.0),
        );
        gamepad_axes.insert(
            "LeftStickY".to_string(),
            pad.get(GamepadAxis::LeftStickY).unwrap_or(0.0),
        );
        gamepad_axes.insert(
            "RightStickX".to_string(),
            pad.get(GamepadAxis::RightStickX).unwrap_or(0.0),
        );
        gamepad_axes.insert(
            "RightStickY".to_string(),
            pad.get(GamepadAxis::RightStickY).unwrap_or(0.0),
        );
    }

    // Raw mouse state (pre-mapping) so exporters can replay either the mapped
    // RetroPad input or the original host input.
    let mut mouse_bits = 0u8;
    if mouse_buttons.pressed(MouseButton::Left) {
        mouse_bits |= 1;
    }
    if mouse_buttons.pressed(MouseButton::Right) {
        mouse_bits |= 1 << 1;
    }
    if mouse_buttons.pressed(MouseButton::Middle) {
        mouse_bits |= 1 << 2;
    }
    let mouse = Some(MouseState {
        dx: mouse_motion.delta.x as i32,
        dy: mouse_motion.delta.y as i32,
        buttons: mouse_bits,
        ..Default::default()
    });

    RawHostInput {
        keyboard_keys,
        keyboard_key_codes: Vec::new(),
        gamepad_buttons,
        gamepad_axes,
        gamepads: Vec::new(),
        mouse,
    }
}

#[derive(Clone, Copy)]
pub struct KeyboardControl {
    pub name: &'static str,
    pub bit: u16,
    pub default_key: KeyCode,
}

#[derive(Clone, Copy)]
pub struct KeyboardAnalogControl {
    pub name: &'static str,
    pub axis: AnalogAxis,
    pub value: i16,
    pub default_key: KeyCode,
}

#[derive(Clone, Copy)]
pub enum AnalogAxis {
    LeftX,
    LeftY,
}

#[derive(Clone, Copy)]
pub struct GamepadControl {
    pub name: &'static str,
    pub bit: u16,
    pub default_button: GamepadButton,
}

pub fn keyboard_controls() -> &'static [KeyboardControl] {
    &[
        KeyboardControl {
            name: "A",
            bit: joypad::A,
            default_key: KB_FACE_A,
        },
        KeyboardControl {
            name: "B",
            bit: joypad::B,
            default_key: KB_FACE_B,
        },
        KeyboardControl {
            name: "X",
            bit: joypad::X,
            default_key: KB_FACE_X,
        },
        KeyboardControl {
            name: "Y",
            bit: joypad::Y,
            default_key: KB_FACE_Y,
        },
        KeyboardControl {
            name: "L",
            bit: joypad::L,
            default_key: KB_L,
        },
        KeyboardControl {
            name: "R",
            bit: joypad::R,
            default_key: KB_R,
        },
        KeyboardControl {
            name: "Select",
            bit: joypad::SELECT,
            default_key: KB_SELECT,
        },
        KeyboardControl {
            name: "Start",
            bit: joypad::START,
            default_key: KB_START,
        },
        KeyboardControl {
            name: "Up",
            bit: joypad::UP,
            default_key: KB_DPAD_UP,
        },
        KeyboardControl {
            name: "Down",
            bit: joypad::DOWN,
            default_key: KB_DPAD_DOWN,
        },
        KeyboardControl {
            name: "Left",
            bit: joypad::LEFT,
            default_key: KB_DPAD_LEFT,
        },
        KeyboardControl {
            name: "Right",
            bit: joypad::RIGHT,
            default_key: KB_DPAD_RIGHT,
        },
    ]
}

pub fn gamepad_controls() -> &'static [GamepadControl] {
    &[
        GamepadControl {
            name: "A",
            bit: joypad::A,
            default_button: GamepadButton::South,
        },
        GamepadControl {
            name: "B",
            bit: joypad::B,
            default_button: GamepadButton::East,
        },
        GamepadControl {
            name: "X",
            bit: joypad::X,
            default_button: GamepadButton::West,
        },
        GamepadControl {
            name: "Y",
            bit: joypad::Y,
            default_button: GamepadButton::North,
        },
        GamepadControl {
            name: "L",
            bit: joypad::L,
            default_button: GamepadButton::LeftTrigger,
        },
        GamepadControl {
            name: "R",
            bit: joypad::R,
            default_button: GamepadButton::RightTrigger,
        },
        GamepadControl {
            name: "L2",
            bit: joypad::L2,
            default_button: GamepadButton::LeftTrigger2,
        },
        GamepadControl {
            name: "R2",
            bit: joypad::R2,
            default_button: GamepadButton::RightTrigger2,
        },
        GamepadControl {
            name: "Select",
            bit: joypad::SELECT,
            default_button: GamepadButton::Select,
        },
        GamepadControl {
            name: "Start",
            bit: joypad::START,
            default_button: GamepadButton::Start,
        },
        GamepadControl {
            name: "Up",
            bit: joypad::UP,
            default_button: GamepadButton::DPadUp,
        },
        GamepadControl {
            name: "Down",
            bit: joypad::DOWN,
            default_button: GamepadButton::DPadDown,
        },
        GamepadControl {
            name: "Left",
            bit: joypad::LEFT,
            default_button: GamepadButton::DPadLeft,
        },
        GamepadControl {
            name: "Right",
            bit: joypad::RIGHT,
            default_button: GamepadButton::DPadRight,
        },
        GamepadControl {
            name: "L3",
            bit: joypad::L3,
            default_button: GamepadButton::LeftThumb,
        },
        GamepadControl {
            name: "R3",
            bit: joypad::R3,
            default_button: GamepadButton::RightThumb,
        },
    ]
}

pub fn keyboard_analog_controls() -> &'static [KeyboardAnalogControl] {
    &[
        KeyboardAnalogControl {
            name: "LeftStickLeft",
            axis: AnalogAxis::LeftX,
            value: -ANALOG_MAX,
            default_key: KB_LSTICK_X_NEG,
        },
        KeyboardAnalogControl {
            name: "LeftStickRight",
            axis: AnalogAxis::LeftX,
            value: ANALOG_MAX,
            default_key: KB_LSTICK_X_POS,
        },
        KeyboardAnalogControl {
            name: "LeftStickUp",
            axis: AnalogAxis::LeftY,
            value: -ANALOG_MAX,
            default_key: KB_LSTICK_Y_NEG,
        },
        KeyboardAnalogControl {
            name: "LeftStickDown",
            axis: AnalogAxis::LeftY,
            value: ANALOG_MAX,
            default_key: KB_LSTICK_Y_POS,
        },
    ]
}

pub fn default_input_bindings() -> InputBindingSet {
    let mut bindings = default_keyboard_bindings();
    for control in gamepad_controls() {
        bindings.gamepad.insert(
            control.name.to_string(),
            gamepad_button_to_binding(control.default_button),
        );
    }
    bindings
}

pub fn default_keyboard_bindings() -> InputBindingSet {
    let mut bindings = InputBindingSet::default();
    for control in keyboard_controls() {
        bindings.keyboard.insert(
            control.name.to_string(),
            keycode_to_binding(control.default_key),
        );
    }
    for control in keyboard_analog_controls() {
        bindings.keyboard.insert(
            control.name.to_string(),
            keycode_to_binding(control.default_key),
        );
    }
    bindings
}

pub fn default_gamepad_bindings() -> InputBindingSet {
    let mut bindings = InputBindingSet::default();
    for control in gamepad_controls() {
        bindings.gamepad.insert(
            control.name.to_string(),
            gamepad_button_to_binding(control.default_button),
        );
    }
    bindings
}

pub fn keycode_to_binding(key: KeyCode) -> String {
    format!("{key:?}")
}

/// Parse a stored binding string back into a `KeyCode`.
///
/// Bindings are stored via `format!("{key:?}")` (the `KeyCode` Debug name).
/// Earlier code parsed only a hand-written allowlist of letters/digits/arrows,
/// so rebinding to Tab or an F-key silently fell back to the default at play
/// time. This is now a complete reverse map over every `KeyCode` variant so
/// any binding captured by the press-to-bind UI round-trips back exactly.
pub fn keycode_from_binding(value: &str) -> Option<KeyCode> {
    // Generated from the `KeyCode` enum variant names (Bevy 0.18). Keeping this
    // table explicit avoids enabling the `serialize` feature on `bevy_input`,
    // which would change the on-disk config format for existing users.
    Some(match value {
        // Letters.
        "KeyA" => KeyCode::KeyA,
        "KeyB" => KeyCode::KeyB,
        "KeyC" => KeyCode::KeyC,
        "KeyD" => KeyCode::KeyD,
        "KeyE" => KeyCode::KeyE,
        "KeyF" => KeyCode::KeyF,
        "KeyG" => KeyCode::KeyG,
        "KeyH" => KeyCode::KeyH,
        "KeyI" => KeyCode::KeyI,
        "KeyJ" => KeyCode::KeyJ,
        "KeyK" => KeyCode::KeyK,
        "KeyL" => KeyCode::KeyL,
        "KeyM" => KeyCode::KeyM,
        "KeyN" => KeyCode::KeyN,
        "KeyO" => KeyCode::KeyO,
        "KeyP" => KeyCode::KeyP,
        "KeyQ" => KeyCode::KeyQ,
        "KeyR" => KeyCode::KeyR,
        "KeyS" => KeyCode::KeyS,
        "KeyT" => KeyCode::KeyT,
        "KeyU" => KeyCode::KeyU,
        "KeyV" => KeyCode::KeyV,
        "KeyW" => KeyCode::KeyW,
        "KeyX" => KeyCode::KeyX,
        "KeyY" => KeyCode::KeyY,
        "KeyZ" => KeyCode::KeyZ,
        // Digits.
        "Digit0" => KeyCode::Digit0,
        "Digit1" => KeyCode::Digit1,
        "Digit2" => KeyCode::Digit2,
        "Digit3" => KeyCode::Digit3,
        "Digit4" => KeyCode::Digit4,
        "Digit5" => KeyCode::Digit5,
        "Digit6" => KeyCode::Digit6,
        "Digit7" => KeyCode::Digit7,
        "Digit8" => KeyCode::Digit8,
        "Digit9" => KeyCode::Digit9,
        // Whitespace + editing.
        "Enter" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "Space" => KeyCode::Space,
        "Backspace" => KeyCode::Backspace,
        "Insert" => KeyCode::Insert,
        "Delete" => KeyCode::Delete,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        // Arrows.
        "ArrowUp" => KeyCode::ArrowUp,
        "ArrowDown" => KeyCode::ArrowDown,
        "ArrowLeft" => KeyCode::ArrowLeft,
        "ArrowRight" => KeyCode::ArrowRight,
        // Modifiers.
        "ShiftLeft" => KeyCode::ShiftLeft,
        "ShiftRight" => KeyCode::ShiftRight,
        "ControlLeft" => KeyCode::ControlLeft,
        "ControlRight" => KeyCode::ControlRight,
        "AltLeft" => KeyCode::AltLeft,
        "AltRight" => KeyCode::AltRight,
        "SuperLeft" => KeyCode::SuperLeft,
        "SuperRight" => KeyCode::SuperRight,
        // Function row.
        "F1" => KeyCode::F1,
        "F2" => KeyCode::F2,
        "F3" => KeyCode::F3,
        "F4" => KeyCode::F4,
        "F5" => KeyCode::F5,
        "F6" => KeyCode::F6,
        "F7" => KeyCode::F7,
        "F8" => KeyCode::F8,
        "F9" => KeyCode::F9,
        "F10" => KeyCode::F10,
        "F11" => KeyCode::F11,
        "F12" => KeyCode::F12,
        "F13" => KeyCode::F13,
        "F14" => KeyCode::F14,
        "F15" => KeyCode::F15,
        "F16" => KeyCode::F16,
        "F17" => KeyCode::F17,
        "F18" => KeyCode::F18,
        "F19" => KeyCode::F19,
        "F20" => KeyCode::F20,
        "F21" => KeyCode::F21,
        "F22" => KeyCode::F22,
        "F23" => KeyCode::F23,
        "F24" => KeyCode::F24,
        // Lock keys.
        "NumLock" => KeyCode::NumLock,
        "CapsLock" => KeyCode::CapsLock,
        "ScrollLock" => KeyCode::ScrollLock,
        // Escape.
        "Escape" => KeyCode::Escape,
        // Punctuation / symbol keys (variant names match Bevy's KeyCode).
        "Minus" => KeyCode::Minus,
        "Equal" => KeyCode::Equal,
        "LeftBracket" => KeyCode::BracketLeft,
        "RightBracket" => KeyCode::BracketRight,
        "BracketLeft" => KeyCode::BracketLeft,
        "BracketRight" => KeyCode::BracketRight,
        "Backslash" => KeyCode::Backslash,
        "Semicolon" => KeyCode::Semicolon,
        "Apostrophe" => KeyCode::Quote,
        "Quote" => KeyCode::Quote,
        "Grave" => KeyCode::Backquote,
        "Backquote" => KeyCode::Backquote,
        "Comma" => KeyCode::Comma,
        "Period" => KeyCode::Period,
        "Slash" => KeyCode::Slash,
        // Non-US OEM key (102).
        "IntlBackslash" => KeyCode::IntlBackslash,
        // Keypad.
        "Numpad0" => KeyCode::Numpad0,
        "Numpad1" => KeyCode::Numpad1,
        "Numpad2" => KeyCode::Numpad2,
        "Numpad3" => KeyCode::Numpad3,
        "Numpad4" => KeyCode::Numpad4,
        "Numpad5" => KeyCode::Numpad5,
        "Numpad6" => KeyCode::Numpad6,
        "Numpad7" => KeyCode::Numpad7,
        "Numpad8" => KeyCode::Numpad8,
        "Numpad9" => KeyCode::Numpad9,
        "NumpadAdd" => KeyCode::NumpadAdd,
        "NumpadSubtract" => KeyCode::NumpadSubtract,
        "NumpadMultiply" => KeyCode::NumpadMultiply,
        "NumpadDivide" => KeyCode::NumpadDivide,
        "NumpadEnter" => KeyCode::NumpadEnter,
        "NumpadDecimal" => KeyCode::NumpadDecimal,
        "NumpadEqual" => KeyCode::NumpadEqual,
        "NumpadComma" => KeyCode::NumpadComma,
        _ => return None,
    })
}

pub fn gamepad_button_to_binding(button: GamepadButton) -> String {
    format!("{button:?}")
}

pub fn gamepad_button_from_binding(value: &str) -> Option<GamepadButton> {
    Some(match value {
        "South" => GamepadButton::South,
        "East" => GamepadButton::East,
        "West" => GamepadButton::West,
        "North" => GamepadButton::North,
        "LeftTrigger" => GamepadButton::LeftTrigger,
        "RightTrigger" => GamepadButton::RightTrigger,
        "LeftTrigger2" => GamepadButton::LeftTrigger2,
        "RightTrigger2" => GamepadButton::RightTrigger2,
        "Select" => GamepadButton::Select,
        "Start" => GamepadButton::Start,
        "DPadUp" => GamepadButton::DPadUp,
        "DPadDown" => GamepadButton::DPadDown,
        "DPadLeft" => GamepadButton::DPadLeft,
        "DPadRight" => GamepadButton::DPadRight,
        "LeftThumb" => GamepadButton::LeftThumb,
        "RightThumb" => GamepadButton::RightThumb,
        _ => return None,
    })
}

pub fn candidate_gamepad_buttons() -> &'static [GamepadButton] {
    &[
        GamepadButton::South,
        GamepadButton::East,
        GamepadButton::West,
        GamepadButton::North,
        GamepadButton::LeftTrigger,
        GamepadButton::RightTrigger,
        GamepadButton::LeftTrigger2,
        GamepadButton::RightTrigger2,
        GamepadButton::Select,
        GamepadButton::Start,
        GamepadButton::DPadUp,
        GamepadButton::DPadDown,
        GamepadButton::DPadLeft,
        GamepadButton::DPadRight,
        GamepadButton::LeftThumb,
        GamepadButton::RightThumb,
    ]
}

fn binding_pressed(
    keys: &ButtonInput<KeyCode>,
    bindings: &InputBindingSet,
    control: &str,
    default_key: KeyCode,
) -> bool {
    let key = bindings
        .keyboard
        .get(control)
        .and_then(|value| keycode_from_binding(value))
        .unwrap_or(default_key);
    keys.pressed(key)
}

fn gamepad_binding_pressed(
    pad: &Gamepad,
    bindings: &InputBindingSet,
    control: &str,
    default_button: GamepadButton,
) -> bool {
    let button = bindings
        .gamepad
        .get(control)
        .and_then(|value| gamepad_button_from_binding(value))
        .unwrap_or(default_button);
    pad.pressed(button)
}
