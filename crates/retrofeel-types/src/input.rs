//! InputFrame IR — one record per emulated frame, per port.

use crate::RetroPadButtonBits;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Analog stick state, axes in `[-0x7fff, 0x7fff]` (libretro convention:
/// +X right, +Y down).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalogStick {
    pub x: i16,
    pub y: i16,
}

/// Triggers in `[0, 0x7fff]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triggers {
    pub l: i16,
    pub r: i16,
}

/// Mouse motion, wheel deltas, and held button bits.
///
/// `dx`/`dy` and `wheel_x`/`wheel_y` are accumulated between consecutive
/// frame samples, then reset. Wheel deltas use the source platform's discrete
/// tick convention; Linux evdev reports positive `wheel_y` for wheel-up and
/// positive `wheel_x` for wheel-right. Button bits are held state (bit 0 =
/// left, bit 1 = right, bit 2 = middle).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseState {
    pub dx: i32,
    pub dy: i32,
    #[serde(default)]
    pub wheel_x: i32,
    #[serde(default)]
    pub wheel_y: i32,
    pub buttons: u8,
}

/// Raw keyboard scancodes held down this frame (libretro `RETROK_*` values).
/// Fixed-size buffer (max 8 simultaneous keys) so `InputState` is `Copy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardState {
    /// Held scancodes; trailing entries are 0 (unused).
    pub keys: [i32; 8],
    pub count: u8,
}

/// The full input state fed to the core for one frame on one port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputState {
    /// Digital RetroPad button bitmask (see `device_ids::joypad`).
    pub buttons: RetroPadButtonBits,
    pub analog_l: AnalogStick,
    pub analog_r: AnalogStick,
    pub triggers: Triggers,
    pub mouse: MouseState,
    pub keyboard: KeyboardState,
}

/// One captured frame of input. `frame` is the 0-indexed emulated frame number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFrame {
    pub frame: u64,
    /// Presentation time relative to the first encoded video frame.
    ///
    /// Libretro recordings use a fixed frame clock and leave this unset.
    /// External variable-frame-rate captures set it so exporters can preserve
    /// the source video's actual presentation timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_us: Option<u64>,
    pub port: u8,
    pub state: InputState,
    #[serde(default)]
    pub raw_host: Option<RawHostInput>,
}

/// A compact, deterministic index of a held-input state transition.
///
/// `input.json` remains authoritative. A recording writer creates these only
/// after finalizing that log, and consumers can reproduce this exact sequence
/// from the input frames without guessing a nominal frame rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputTransition {
    pub frame: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_us: Option<u64>,
    pub state: InputState,
    #[serde(default)]
    pub raw_host: Option<RawHostInput>,
}

/// Extract the initial held state and every subsequent changed state from an
/// authoritative per-frame input log.
pub fn input_transitions_from_frames(frames: &[InputFrame]) -> Vec<InputTransition> {
    let mut previous: Option<&InputFrame> = None;
    let mut transitions = Vec::new();
    for frame in frames {
        if previous
            .is_none_or(|prior| prior.state != frame.state || prior.raw_host != frame.raw_host)
        {
            transitions.push(InputTransition {
                frame: frame.frame,
                elapsed_us: frame.elapsed_us,
                state: frame.state,
                raw_host: frame.raw_host.clone(),
            });
        }
        previous = Some(frame);
    }
    transitions
}

/// One controller track retained by the capture host.
///
/// Steam Input can publish several virtual pads at once, and external capture
/// can also retain exact-allowlisted physical tracks. The legacy
/// `RawHostInput.gamepad_*` fields continue to mirror one compatibility-primary
/// device, while this collection preserves every retained device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawGamepadInput {
    pub device_id: String,
    pub port: Option<u8>,
    pub name: String,
    pub buttons: Vec<String>,
    /// Named axes normalized to `[-1, 1]` (sticks, with positive Y up) or
    /// `[0, 1]` (triggers), matching the legacy raw host fields.
    pub axes: BTreeMap<String, f32>,
}

/// Pre-mapping host input held during an emulated frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawHostInput {
    pub keyboard_keys: Vec<String>,
    /// Linux evdev key codes corresponding by index to `keyboard_keys`.
    ///
    /// Capture paths without a stable numeric host-code vocabulary leave this
    /// empty. The field is additive so older recordings remain readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyboard_key_codes: Vec<u16>,
    pub gamepad_buttons: Vec<String>,
    pub gamepad_axes: BTreeMap<String, f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gamepads: Vec<RawGamepadInput>,
    pub mouse: Option<MouseState>,
}

/// Named RetroPad buttons for human-readable exports/UI. Mirrors `device_ids::joypad`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetroPadButtons {
    B,
    Y,
    Select,
    Start,
    Up,
    Down,
    Left,
    Right,
    A,
    X,
    L,
    R,
    L2,
    R2,
    L3,
    R3,
}

impl RetroPadButtons {
    pub fn bit(self) -> u16 {
        self as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_input_logs_default_external_timing_and_multi_pad_state() {
        let json = r#"{
          "frame": 7,
          "port": 0,
          "state": {
            "buttons": 0,
            "analog_l": {"x": 0, "y": 0},
            "analog_r": {"x": 0, "y": 0},
            "triggers": {"l": 0, "r": 0},
            "mouse": {"dx": 0, "dy": 0, "buttons": 0},
            "keyboard": {"keys": [0,0,0,0,0,0,0,0], "count": 0}
          },
          "raw_host": {
            "keyboard_keys": [],
            "gamepad_buttons": ["South"],
            "gamepad_axes": {},
            "mouse": null
          }
        }"#;
        let frame: InputFrame = serde_json::from_str(json).unwrap();
        assert_eq!(frame.elapsed_us, None);
        let raw = frame.raw_host.unwrap();
        assert!(raw.gamepads.is_empty());
        assert!(raw.keyboard_key_codes.is_empty());
    }

    #[test]
    fn older_mouse_state_defaults_explicit_wheel_deltas() {
        let mouse: MouseState = serde_json::from_str(r#"{"dx":4,"dy":-2,"buttons":1}"#).unwrap();
        assert_eq!(mouse.wheel_x, 0);
        assert_eq!(mouse.wheel_y, 0);
    }

    #[test]
    fn mouse_wheel_axes_have_an_explicit_serialized_representation() {
        let mouse = MouseState {
            dx: 4,
            dy: -2,
            wheel_x: -1,
            wheel_y: 3,
            buttons: 5,
        };
        let value = serde_json::to_value(mouse).unwrap();
        assert_eq!(value["wheel_x"], -1);
        assert_eq!(value["wheel_y"], 3);
        assert_eq!(serde_json::from_value::<MouseState>(value).unwrap(), mouse);
    }

    #[test]
    fn transitions_are_derived_only_from_changed_held_state() {
        let first = InputFrame {
            frame: 0,
            elapsed_us: Some(0),
            port: 0,
            state: InputState::default(),
            raw_host: Some(RawHostInput::default()),
        };
        let mut pressed = first.clone();
        pressed.frame = 2;
        pressed.elapsed_us = Some(33_333);
        pressed.raw_host.as_mut().unwrap().keyboard_keys = vec!["A".into()];
        let mut same = pressed.clone();
        same.frame = 3;
        same.elapsed_us = Some(50_000);
        let transitions = input_transitions_from_frames(&[first, pressed.clone(), same]);
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0].frame, 0);
        assert_eq!(transitions[1].frame, pressed.frame);
    }
}
