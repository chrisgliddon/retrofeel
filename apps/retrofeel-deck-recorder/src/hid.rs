use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use hidreport::{Field, Report, ReportDescriptor};

use crate::model::AbsRange;

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;

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
const BTN_TOUCH: u16 = 330;
const BTN_DPAD_UP: u16 = 544;
const BTN_DPAD_DOWN: u16 = 545;
const BTN_DPAD_LEFT: u16 = 546;
const BTN_DPAD_RIGHT: u16 = 547;
const BTN_GRIPL: u16 = 548;
const BTN_GRIPR: u16 = 549;
const BTN_GRIPL2: u16 = 550;
const BTN_GRIPR2: u16 = 551;

const STEAM_DECK_VENDOR_DESCRIPTOR: &[u8] = &[
    0x06, 0xff, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x40,
    0x09, 0x01, 0x81, 0x02, 0x09, 0x01, 0xb1, 0x02, 0xc0,
];

const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const USAGE_PAGE_BUTTON: u16 = 0x09;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_Z: u16 = 0x32;
const USAGE_RX: u16 = 0x33;
const USAGE_RY: u16 = 0x34;
const USAGE_RZ: u16 = 0x35;
const USAGE_HAT: u16 = 0x39;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedInputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
    pub name: String,
}

pub struct HidGamepadDecoder {
    format: HidReportFormat,
    vendor: u16,
    product: u16,
    abs_ranges: BTreeMap<u16, AbsRange>,
    previous: BTreeMap<(u16, u16), i32>,
}

enum HidReportFormat {
    Descriptor(ReportDescriptor),
    DualSenseBluetooth,
    SteamDeck,
}

impl HidGamepadDecoder {
    pub fn supports_interface(vendor: u16, product: u16, descriptor: &[u8]) -> bool {
        match (vendor, product) {
            (0x2dc8, 0x3013) | (0x054c, 0x0ce6) => true,
            (0x28de, 0x1205) => descriptor == STEAM_DECK_VENDOR_DESCRIPTOR,
            _ => false,
        }
    }

    pub fn new(vendor: u16, product: u16, descriptor: &[u8]) -> Result<Self> {
        let (format, abs_ranges) = match (vendor, product) {
            (0x2dc8, 0x3013) => {
                let descriptor = ReportDescriptor::try_from(descriptor)
                    .context("failed to parse HID report descriptor")?;
                let mut abs_ranges = BTreeMap::new();
                for report in descriptor.input_reports() {
                    for field in report.fields() {
                        let Field::Variable(field) = field else {
                            continue;
                        };
                        let page = u16::from(field.usage.usage_page);
                        let usage = u16::from(field.usage.usage_id);
                        if page != USAGE_PAGE_GENERIC_DESKTOP {
                            continue;
                        }
                        let minimum = i32::from(field.logical_minimum);
                        let maximum = i32::from(field.logical_maximum);
                        if usage == USAGE_HAT {
                            abs_ranges.insert(
                                ABS_HAT0X,
                                AbsRange {
                                    minimum: -1,
                                    maximum: 1,
                                    flat: 0,
                                },
                            );
                            abs_ranges.insert(
                                ABS_HAT0Y,
                                AbsRange {
                                    minimum: -1,
                                    maximum: 1,
                                    flat: 0,
                                },
                            );
                        } else if let Some(code) = axis_code(usage) {
                            abs_ranges.insert(
                                code,
                                AbsRange {
                                    minimum,
                                    maximum,
                                    flat: 0,
                                },
                            );
                        }
                    }
                }
                if abs_ranges.is_empty() {
                    bail!("HID descriptor contains no mapped gamepad axes");
                }
                (HidReportFormat::Descriptor(descriptor), abs_ranges)
            }
            (0x054c, 0x0ce6) => {
                let mut abs_ranges = [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ]
                    .into_iter()
                    .map(|code| {
                        (
                            code,
                            AbsRange {
                                minimum: 0,
                                maximum: 255,
                                flat: 0,
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                abs_ranges.insert(
                    ABS_HAT0X,
                    AbsRange {
                        minimum: -1,
                        maximum: 1,
                        flat: 0,
                    },
                );
                abs_ranges.insert(
                    ABS_HAT0Y,
                    AbsRange {
                        minimum: -1,
                        maximum: 1,
                        flat: 0,
                    },
                );
                (HidReportFormat::DualSenseBluetooth, abs_ranges)
            }
            (0x28de, 0x1205) => {
                if descriptor != STEAM_DECK_VENDOR_DESCRIPTOR {
                    bail!("Steam Deck HID interface is not the 64-byte controller report");
                }
                let mut abs_ranges = [ABS_X, ABS_Y, ABS_RX, ABS_RY]
                    .into_iter()
                    .map(|code| {
                        (
                            code,
                            AbsRange {
                                minimum: -32767,
                                maximum: 32767,
                                flat: 0,
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                for code in [ABS_Z, ABS_RZ] {
                    abs_ranges.insert(
                        code,
                        AbsRange {
                            minimum: 0,
                            maximum: 32767,
                            flat: 0,
                        },
                    );
                }
                (HidReportFormat::SteamDeck, abs_ranges)
            }
            _ => bail!("no HID mapping is defined for {vendor:04x}:{product:04x}"),
        };
        Ok(Self {
            format,
            vendor,
            product,
            abs_ranges,
            previous: BTreeMap::new(),
        })
    }

    pub fn abs_ranges(&self) -> &BTreeMap<u16, AbsRange> {
        &self.abs_ranges
    }

    pub fn decode(&mut self, bytes: &[u8]) -> Result<Vec<DecodedInputEvent>> {
        let values = match &self.format {
            HidReportFormat::Descriptor(descriptor) => {
                let report = descriptor
                    .find_input_report(bytes)
                    .context("HID report did not match the descriptor")?;
                let mut values = Vec::new();
                for field in report.fields() {
                    let Field::Variable(field) = field else {
                        continue;
                    };
                    let page = u16::from(field.usage.usage_page);
                    let usage = u16::from(field.usage.usage_id);
                    let value = i32::from(
                        field
                            .extract(bytes)
                            .context("failed to extract HID report field")?,
                    );
                    match (page, usage) {
                        (USAGE_PAGE_BUTTON, usage) => {
                            let code = button_code(usage);
                            values.push((EV_KEY, code, value, format!("HID_BUTTON_{usage}")));
                        }
                        (USAGE_PAGE_GENERIC_DESKTOP, USAGE_HAT) => {
                            let minimum = i32::from(field.logical_minimum);
                            let (x, y) = hat_xy(value.saturating_sub(minimum));
                            values.push((EV_ABS, ABS_HAT0X, x, "ABS_HAT0X".into()));
                            values.push((EV_ABS, ABS_HAT0Y, y, "ABS_HAT0Y".into()));
                        }
                        (USAGE_PAGE_GENERIC_DESKTOP, usage) => {
                            if let Some(code) = axis_code(usage) {
                                values.push((EV_ABS, code, value, axis_name(code).into()));
                            }
                        }
                        _ => {}
                    }
                }
                values
            }
            HidReportFormat::DualSenseBluetooth => dualsense_bluetooth_values(bytes)?,
            HidReportFormat::SteamDeck => steam_deck_values(bytes)?,
        };

        let mut events = Vec::new();
        for (event_type, code, value, name) in values {
            if self.previous.insert((event_type, code), value) != Some(value) {
                events.push(DecodedInputEvent {
                    event_type,
                    code,
                    value,
                    name,
                });
            }
        }
        Ok(events)
    }

    pub fn identity(&self) -> (u16, u16) {
        (self.vendor, self.product)
    }
}

fn steam_deck_values(bytes: &[u8]) -> Result<Vec<(u16, u16, i32, String)>> {
    // Valve's 28de:1205 vendor report is documented by Linux's hid-steam
    // driver in drivers/hid/hid-steam.c. Steam reads this same layered hidraw
    // interface while native Steam Input actions are active.
    if bytes.len() != 64 || bytes[..3] != [0x01, 0x00, 0x09] {
        bail!("Steam Deck controller report is missing or malformed");
    }

    let b8 = bytes[8];
    let b9 = bytes[9];
    let b10 = bytes[10];
    let b11 = bytes[11];
    let b13 = bytes[13];
    let b14 = bytes[14];
    let mut values = vec![
        (EV_ABS, ABS_X, steam_deck_i16(bytes, 48), "ABS_X".into()),
        (EV_ABS, ABS_Y, -steam_deck_i16(bytes, 50), "ABS_Y".into()),
        (EV_ABS, ABS_RX, steam_deck_i16(bytes, 52), "ABS_RX".into()),
        (EV_ABS, ABS_RY, -steam_deck_i16(bytes, 54), "ABS_RY".into()),
        (
            EV_ABS,
            ABS_Z,
            i32::from(u16::from_le_bytes([bytes[44], bytes[45]])),
            "ABS_Z".into(),
        ),
        (
            EV_ABS,
            ABS_RZ,
            i32::from(u16::from_le_bytes([bytes[46], bytes[47]])),
            "ABS_RZ".into(),
        ),
    ];
    for (code, pressed, name) in [
        (BTN_TR2, b8 & 0x01 != 0, "BTN_TR2"),
        (BTN_TL2, b8 & 0x02 != 0, "BTN_TL2"),
        (BTN_TR, b8 & 0x04 != 0, "BTN_TR"),
        (BTN_TL, b8 & 0x08 != 0, "BTN_TL"),
        (BTN_Y, b8 & 0x10 != 0, "BTN_Y"),
        (BTN_EAST, b8 & 0x20 != 0, "BTN_EAST"),
        (BTN_X, b8 & 0x40 != 0, "BTN_X"),
        (BTN_SOUTH, b8 & 0x80 != 0, "BTN_SOUTH"),
        (BTN_DPAD_UP, b9 & 0x01 != 0, "BTN_DPAD_UP"),
        (BTN_DPAD_RIGHT, b9 & 0x02 != 0, "BTN_DPAD_RIGHT"),
        (BTN_DPAD_LEFT, b9 & 0x04 != 0, "BTN_DPAD_LEFT"),
        (BTN_DPAD_DOWN, b9 & 0x08 != 0, "BTN_DPAD_DOWN"),
        (BTN_SELECT, b9 & 0x10 != 0, "BTN_SELECT"),
        (BTN_MODE, b9 & 0x20 != 0, "BTN_MODE"),
        (BTN_START, b9 & 0x40 != 0, "BTN_START"),
        (BTN_GRIPL2, b9 & 0x80 != 0, "BTN_GRIPL2"),
        (BTN_GRIPR2, b10 & 0x01 != 0, "BTN_GRIPR2"),
        (BTN_THUMB, b10 & 0x02 != 0, "BTN_THUMB"),
        (BTN_THUMB2, b10 & 0x04 != 0, "BTN_THUMB2"),
        (BTN_THUMBL, b10 & 0x40 != 0, "BTN_THUMBL"),
        (BTN_THUMBR, b11 & 0x04 != 0, "BTN_THUMBR"),
        (BTN_GRIPL, b13 & 0x02 != 0, "BTN_GRIPL"),
        (BTN_GRIPR, b13 & 0x04 != 0, "BTN_GRIPR"),
        (BTN_BASE, b14 & 0x04 != 0, "BTN_BASE"),
    ] {
        values.push((EV_KEY, code, i32::from(pressed), name.into()));
    }
    Ok(values)
}

fn steam_deck_i16(bytes: &[u8], offset: usize) -> i32 {
    let value = i16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    i32::from(if value == i16::MIN { -32767 } else { value })
}

fn dualsense_bluetooth_values(bytes: &[u8]) -> Result<Vec<(u16, u16, i32, String)>> {
    if bytes.len() < 12 || bytes[0] != 0x31 {
        bail!("DualSense Bluetooth input report is missing or truncated");
    }
    // The experimental BT audio driver documents two non-gamepad 0x31 packet
    // forms. Neither may be interpreted as controller state.
    if bytes[1] & 0x02 != 0 || bytes[3..6] == [0xd4, 0xff, 0xfe] {
        return Ok(Vec::new());
    }

    let mut values = vec![
        (EV_ABS, ABS_X, i32::from(bytes[2]), "ABS_X".into()),
        (EV_ABS, ABS_Y, i32::from(bytes[3]), "ABS_Y".into()),
        (EV_ABS, ABS_RX, i32::from(bytes[4]), "ABS_RX".into()),
        (EV_ABS, ABS_RY, i32::from(bytes[5]), "ABS_RY".into()),
        (EV_ABS, ABS_Z, i32::from(bytes[6]), "ABS_Z".into()),
        (EV_ABS, ABS_RZ, i32::from(bytes[7]), "ABS_RZ".into()),
    ];
    let face = bytes[9];
    let (hat_x, hat_y) = hat_xy(i32::from(face & 0x0f));
    values.push((EV_ABS, ABS_HAT0X, hat_x, "ABS_HAT0X".into()));
    values.push((EV_ABS, ABS_HAT0Y, hat_y, "ABS_HAT0Y".into()));
    for (code, pressed, name) in [
        (BTN_X, face & 0x10 != 0, "BTN_X"),
        (BTN_SOUTH, face & 0x20 != 0, "BTN_SOUTH"),
        (BTN_EAST, face & 0x40 != 0, "BTN_EAST"),
        (BTN_Y, face & 0x80 != 0, "BTN_Y"),
        (BTN_TL, bytes[10] & 0x01 != 0, "BTN_TL"),
        (BTN_TR, bytes[10] & 0x02 != 0, "BTN_TR"),
        (BTN_TL2, bytes[10] & 0x04 != 0, "BTN_TL2"),
        (BTN_TR2, bytes[10] & 0x08 != 0, "BTN_TR2"),
        (BTN_SELECT, bytes[10] & 0x10 != 0, "BTN_SELECT"),
        (BTN_START, bytes[10] & 0x20 != 0, "BTN_START"),
        (BTN_THUMBL, bytes[10] & 0x40 != 0, "BTN_THUMBL"),
        (BTN_THUMBR, bytes[10] & 0x80 != 0, "BTN_THUMBR"),
        (BTN_MODE, bytes[11] & 0x01 != 0, "BTN_MODE"),
        (BTN_TOUCH, bytes[11] & 0x02 != 0, "BTN_TOUCH"),
    ] {
        values.push((EV_KEY, code, i32::from(pressed), name.into()));
    }
    Ok(values)
}

fn axis_code(usage: u16) -> Option<u16> {
    // The 2dc8:3013 DInput mapping published by SDL/Steam is:
    // leftx:a0,lefty:a1,rightx:a2,righty:a3,righttrigger:a4,lefttrigger:a5.
    // HID exposes those axes as X,Y,Z,Rx,Ry,Rz; normalize them to the
    // standard Xbox evdev codes consumed by the existing timeline sampler.
    match usage {
        USAGE_X => Some(ABS_X),
        USAGE_Y => Some(ABS_Y),
        USAGE_Z => Some(ABS_RX),
        USAGE_RX => Some(ABS_RY),
        USAGE_RY => Some(ABS_RZ),
        USAGE_RZ => Some(ABS_Z),
        _ => None,
    }
}

fn axis_name(code: u16) -> &'static str {
    match code {
        ABS_X => "ABS_X",
        ABS_Y => "ABS_Y",
        ABS_Z => "ABS_Z",
        ABS_RX => "ABS_RX",
        ABS_RY => "ABS_RY",
        ABS_RZ => "ABS_RZ",
        _ => "ABS_UNKNOWN",
    }
}

fn button_code(usage: u16) -> u16 {
    match usage {
        1 => BTN_SOUTH,
        2 => BTN_EAST,
        // This 8BitDo DInput report numbers Y as button 4 and X as button 5.
        4 => BTN_Y,
        5 => BTN_X,
        7 => BTN_TL,
        8 => BTN_TR,
        11 => BTN_SELECT,
        12 => BTN_START,
        13 => BTN_MODE,
        14 => BTN_THUMBL,
        15 => BTN_THUMBR,
        // Preserve every additional button in the raw stream using the
        // kernel's contiguous joystick-button range.
        usage => 0x120u16.saturating_add(usage.saturating_sub(1)),
    }
}

fn hat_xy(value: i32) -> (i32, i32) {
    match value {
        0 => (0, -1),
        1 => (1, -1),
        2 => (1, 0),
        3 => (1, 1),
        4 => (0, 1),
        5 => (-1, 1),
        6 => (-1, 0),
        7 => (-1, -1),
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAMEPAD_DESCRIPTOR: &[u8] = &[
        0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, // Generic Desktop / Gamepad
        0x05, 0x09, 0x19, 0x01, 0x29, 0x0f, // Buttons 1..15
        0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0f, 0x81, 0x02, 0x75, 0x01, 0x95, 0x01, 0x81,
        0x03, // one padding bit
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x33, 0x09, 0x34, 0x09,
        0x35, // X, Y, Z, Rx, Ry, Rz
        0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00,
        0x25, 0x07, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x75, 0x04, 0x95, 0x01, 0x81, 0x03, 0xc0,
    ];

    // The Deck's third HID interface advertises one 64-byte vendor report.
    // This is the exact descriptor observed on the test Deck's 28de:1205
    // hidraw interface.
    const STEAM_DECK_DESCRIPTOR: &[u8] = &[
        0x06, 0xff, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95,
        0x40, 0x09, 0x01, 0x81, 0x02, 0x09, 0x01, 0xb1, 0x02, 0xc0,
    ];

    #[test]
    fn decodes_8bitdo_dinput_report_to_xbox_semantics() {
        let mut decoder = HidGamepadDecoder::new(0x2dc8, 0x3013, GAMEPAD_DESCRIPTOR).unwrap();
        let events = decoder
            .decode(&[0b0000_1001, 0, 128, 64, 200, 20, 255, 0, 2])
            .unwrap();

        assert!(events.iter().any(|event| {
            (event.event_type, event.code, event.value) == (EV_KEY, BTN_SOUTH, 1)
        }));
        assert!(events
            .iter()
            .any(|event| { (event.event_type, event.code, event.value) == (EV_KEY, BTN_Y, 1) }));
        assert!(events
            .iter()
            .any(|event| { (event.event_type, event.code, event.value) == (EV_ABS, ABS_RX, 200) }));
        assert!(events
            .iter()
            .any(|event| { (event.event_type, event.code, event.value) == (EV_ABS, ABS_RZ, 255) }));
        assert!(events.iter().any(|event| {
            (event.event_type, event.code, event.value) == (EV_ABS, ABS_HAT0X, 1)
        }));
        assert!(events.iter().any(|event| {
            (event.event_type, event.code, event.value) == (EV_ABS, ABS_HAT0Y, 0)
        }));
        assert!(decoder
            .decode(&[0b0000_1001, 0, 128, 64, 200, 20, 255, 0, 2])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn decodes_steam_deck_vendor_report_to_xbox_semantics() {
        let mut decoder = HidGamepadDecoder::new(0x28de, 0x1205, STEAM_DECK_DESCRIPTOR).unwrap();
        let mut report = [0u8; 64];
        report[..4].copy_from_slice(&[0x01, 0x00, 0x09, 0x40]);
        report[8] = 0b1111_1100; // A/B/X/Y and both shoulders.
        report[9] = 0b0111_0011; // D-pad up/right, select, mode, and start.
        report[10] = 0b0100_0111; // Left stick, both pads, and R5.
        report[11] = 0b0000_0100; // Right-stick click.
        report[13] = 0b0000_0110; // L4 and R4.
        report[14] = 0b0000_0100; // Quick Access.
        report[44..46].copy_from_slice(&1234u16.to_le_bytes());
        report[46..48].copy_from_slice(&2345u16.to_le_bytes());
        report[48..50].copy_from_slice(&(-12_345i16).to_le_bytes());
        report[50..52].copy_from_slice(&6789i16.to_le_bytes());
        report[52..54].copy_from_slice(&22_222i16.to_le_bytes());
        report[54..56].copy_from_slice(&(-11_111i16).to_le_bytes());

        let events = decoder.decode(&report).unwrap();
        for expected in [
            (EV_ABS, ABS_X, -12_345),
            (EV_ABS, ABS_Y, -6789),
            (EV_ABS, ABS_RX, 22_222),
            (EV_ABS, ABS_RY, 11_111),
            (EV_ABS, ABS_Z, 1234),
            (EV_ABS, ABS_RZ, 2345),
            (EV_KEY, BTN_SOUTH, 1),
            (EV_KEY, BTN_EAST, 1),
            (EV_KEY, BTN_X, 1),
            (EV_KEY, BTN_Y, 1),
            (EV_KEY, BTN_TL, 1),
            (EV_KEY, BTN_TR, 1),
            (EV_KEY, BTN_SELECT, 1),
            (EV_KEY, BTN_START, 1),
            (EV_KEY, BTN_MODE, 1),
            (EV_KEY, BTN_THUMBL, 1),
            (EV_KEY, BTN_THUMBR, 1),
            (EV_KEY, BTN_THUMB, 1),
            (EV_KEY, BTN_THUMB2, 1),
            (EV_KEY, BTN_GRIPR2, 1),
            (EV_KEY, BTN_GRIPL, 1),
            (EV_KEY, BTN_GRIPR, 1),
            (EV_KEY, BTN_BASE, 1),
        ] {
            assert!(
                events
                    .iter()
                    .any(|event| (event.event_type, event.code, event.value) == expected),
                "missing decoded Steam Deck event {expected:?}"
            );
        }

        assert!(decoder.decode(&report).unwrap().is_empty());
    }

    #[test]
    fn rejects_unconfigured_hid_devices() {
        assert!(HidGamepadDecoder::new(0x1234, 0x5678, GAMEPAD_DESCRIPTOR).is_err());
        assert!(HidGamepadDecoder::new(0x28de, 0x1205, GAMEPAD_DESCRIPTOR).is_err());
        assert!(!HidGamepadDecoder::supports_interface(
            0x28de,
            0x1205,
            GAMEPAD_DESCRIPTOR
        ));
        assert!(HidGamepadDecoder::supports_interface(
            0x28de,
            0x1205,
            STEAM_DECK_DESCRIPTOR
        ));
    }

    #[test]
    fn decodes_dualsense_bluetooth_gamepad_reports_and_ignores_audio_frames() {
        let mut decoder = HidGamepadDecoder::new(0x054c, 0x0ce6, &[]).unwrap();
        let mut report = [0u8; 78];
        report[0] = 0x31;
        report[2..8].copy_from_slice(&[10, 20, 30, 40, 50, 60]);
        report[9] = 0x10 | 0x20 | 2; // Square + Cross + D-pad right.
        report[10] = 0x01 | 0x02 | 0x10 | 0x20; // L1 + R1 + Create + Options.
        report[11] = 0x01; // PS/Home.

        let events = decoder.decode(&report).unwrap();
        for expected in [
            (EV_ABS, ABS_X, 10),
            (EV_ABS, ABS_Y, 20),
            (EV_ABS, ABS_RX, 30),
            (EV_ABS, ABS_RY, 40),
            (EV_ABS, ABS_Z, 50),
            (EV_ABS, ABS_RZ, 60),
            (EV_ABS, ABS_HAT0X, 1),
            (EV_ABS, ABS_HAT0Y, 0),
            (EV_KEY, BTN_X, 1),
            (EV_KEY, BTN_SOUTH, 1),
            (EV_KEY, BTN_TL, 1),
            (EV_KEY, BTN_TR, 1),
            (EV_KEY, BTN_SELECT, 1),
            (EV_KEY, BTN_START, 1),
            (EV_KEY, BTN_MODE, 1),
        ] {
            assert!(
                events
                    .iter()
                    .any(|event| { (event.event_type, event.code, event.value) == expected }),
                "missing decoded DualSense event {expected:?}"
            );
        }

        let mut feedback = report;
        feedback[3..6].copy_from_slice(&[0xd4, 0xff, 0xfe]);
        assert!(decoder.decode(&feedback).unwrap().is_empty());

        let mut microphone = report;
        microphone[1] |= 0x02;
        assert!(decoder.decode(&microphone).unwrap().is_empty());
    }
}
