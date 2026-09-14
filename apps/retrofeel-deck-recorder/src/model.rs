use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputCapability {
    Gamepad,
    Keyboard,
    Mouse,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputCapabilities {
    pub gamepad: bool,
    pub keyboard: bool,
    pub mouse: bool,
}

impl InputCapabilities {
    pub fn is_empty(self) -> bool {
        !self.gamepad && !self.keyboard && !self.mouse
    }

    pub fn insert(&mut self, capability: InputCapability) {
        match capability {
            InputCapability::Gamepad => self.gamepad = true,
            InputCapability::Keyboard => self.keyboard = true,
            InputCapability::Mouse => self.mouse = true,
        }
    }

    pub fn contains(self, capability: InputCapability) -> bool {
        match capability {
            InputCapability::Gamepad => self.gamepad,
            InputCapability::Keyboard => self.keyboard,
            InputCapability::Mouse => self.mouse,
        }
    }

    pub fn labels(self) -> Vec<&'static str> {
        [
            (self.gamepad, "gamepad"),
            (self.keyboard, "keyboard"),
            (self.mouse, "mouse"),
        ]
        .into_iter()
        .filter_map(|(present, label)| present.then_some(label))
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsRange {
    pub minimum: i32,
    pub maximum: i32,
    pub flat: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeviceSource {
    /// Steam Input's virtual output device. This remains the compatibility primary.
    #[default]
    SteamVirtual,
    /// An exact, configured physical gamepad recorded alongside virtual output.
    /// The serialized name is retained for compatibility with older captures.
    PhysicalFallback,
    /// An exact, configured evdev identity. Keyboard/mouse capabilities are
    /// available only through this explicit opt-in path.
    AllowlistedEvdev,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDeviceInfo {
    pub device_id: String,
    pub event_path: PathBuf,
    pub name: String,
    #[serde(default)]
    pub bus_type: u16,
    pub vendor: u16,
    pub product: u16,
    #[serde(default)]
    pub version: u16,
    #[serde(
        default,
        rename = "unique_id",
        alias = "unique",
        skip_serializing_if = "Option::is_none"
    )]
    pub unique: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_path: Option<String>,
    #[serde(default)]
    pub is_virtual: bool,
    #[serde(default)]
    pub is_steam_virtual: bool,
    pub port: Option<u8>,
    #[serde(default)]
    pub source: InputDeviceSource,
    pub is_gamepad: bool,
    pub is_keyboard: bool,
    pub is_mouse: bool,
    /// Capabilities admitted by recorder policy. Older sidecars omit this and
    /// therefore fall back to the device's discovered capability flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_capabilities: Option<InputCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_reason: Option<String>,
    pub abs_ranges: BTreeMap<u16, AbsRange>,
}

impl InputDeviceInfo {
    pub fn capabilities(&self) -> InputCapabilities {
        InputCapabilities {
            gamepad: self.is_gamepad,
            keyboard: self.is_keyboard,
            mouse: self.is_mouse,
        }
    }

    pub fn captured_capabilities(&self) -> InputCapabilities {
        self.admitted_capabilities
            .unwrap_or_else(|| self.capabilities())
    }

    pub fn has_stable_identity(&self) -> bool {
        self.is_steam_virtual
            || self.port.is_some()
            || self.unique.is_some()
            || self.physical_path.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeviceLifecycle {
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawInputEvent {
    pub boottime_us: u64,
    pub device_id: String,
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Device churn marker. Input events leave this unset. Add/remove markers
    /// reset held state during frame sampling without overloading evdev types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<InputDeviceLifecycle>,
}

impl RawInputEvent {
    pub fn lifecycle(boottime_us: u64, device_id: String, lifecycle: InputDeviceLifecycle) -> Self {
        Self {
            boottime_us,
            device_id,
            event_type: 0,
            code: 0,
            value: 0,
            name: None,
            lifecycle: Some(lifecycle),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerBinding {
    pub source_group: String,
    pub input: String,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerLayoutMap {
    pub controller: u32,
    pub title: String,
    pub description: String,
    pub controller_type: String,
    pub source_file: String,
    pub bindings: Vec<ControllerBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ControllerMap {
    pub game_id: String,
    pub layouts: Vec<ControllerLayoutMap>,
}
