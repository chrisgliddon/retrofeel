use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, UNIX_EPOCH};

use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode, RelativeAxisCode};

use crate::config::{PhysicalGamepadMatch, RecorderConfig};
use crate::hid::HidGamepadDecoder;
use crate::model::{
    AbsRange, InputCapabilities, InputCapability, InputDeviceInfo, InputDeviceSource, RawInputEvent,
};
use crate::reader_registry::ReaderRegistry;

const STEAM_VENDOR: u16 = 0x28de;
const STEAM_VIRTUAL_GAMEPAD: u16 = 0x11ff;
const BUS_BLUETOOTH: u16 = 0x0005;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const EVIOCSCLOCKID: libc::Ioctl = 0x4004_45a0 as libc::Ioctl;
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const HIDIOCGRDESCSIZE: libc::Ioctl = 0x8004_4801 as libc::Ioctl;
const HIDIOCGRDESC: libc::Ioctl = 0x9004_4802 as libc::Ioctl;

#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

struct HidrawIdentity {
    bus: u16,
    vendor: u16,
    product: u16,
    name: String,
    unique: Option<String>,
}

#[derive(Debug)]
pub enum InputMessage {
    DeviceAdded {
        device: InputDeviceInfo,
        boottime_us: u64,
    },
    Event(RawInputEvent),
    DeviceRemoved {
        device_id: String,
        event_path: PathBuf,
        boottime_us: u64,
    },
}

pub struct InputMonitor {
    pub receiver: mpsc::Receiver<InputMessage>,
    stop: Arc<AtomicBool>,
    manager: Option<JoinHandle<()>>,
}

impl InputMonitor {
    pub fn start(config: &RecorderConfig) -> Self {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let manager_stop = Arc::clone(&stop);
        let config = config.clone();
        let manager = thread::Builder::new()
            .name("retrofeel-input-discovery".into())
            .spawn(move || discovery_loop(sender, manager_stop, config))
            .expect("input discovery thread should start");
        Self {
            receiver,
            stop,
            manager: Some(manager),
        }
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(manager) = self.manager.take() {
            let _ = manager.join();
        }
    }
}

/// Discover every gamepad/keyboard/mouse evdev candidate through sysfs only.
/// No `/dev/input/event*` descriptor is opened by this probe.
pub fn discover_evdev_candidates() -> io::Result<Vec<InputDeviceInfo>> {
    discover_evdev_candidates_in(Path::new("/sys/class/input"), Path::new("/dev/input"))
}

fn discover_evdev_candidates_in(
    sysfs_input: &Path,
    dev_input: &Path,
) -> io::Result<Vec<InputDeviceInfo>> {
    let mut entries = fs::read_dir(sysfs_input)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let event_name = entry.file_name();
            let event_path = dev_input.join(&event_name);
            inspect_device(&entry.path(), &event_path).ok().flatten()
        })
        .collect())
}

/// Discover only devices admitted for capture. Rejected evdev candidates
/// remain sysfs-only; exact physical gamepad HID descriptors are opened only
/// after their existing allowlist has matched.
pub fn discover_devices(config: &RecorderConfig) -> io::Result<Vec<InputDeviceInfo>> {
    let mut devices = discover_evdev_candidates()?
        .into_iter()
        .filter_map(|device| config.admitted_evdev(device))
        .collect::<Vec<_>>();
    let mut hidraw_paths = fs::read_dir("/dev")?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("hidraw"))
        })
        .collect::<Vec<_>>();
    hidraw_paths.sort();
    for path in hidraw_paths {
        match inspect_hidraw_device(&path, &config.physical_gamepad_allowlist) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {}
            Err(error) => log::warn!("cannot inspect {}: {error}", path.display()),
        }
    }
    Ok(devices)
}

fn discovery_loop(
    sender: mpsc::Sender<InputMessage>,
    stop: Arc<AtomicBool>,
    config: RecorderConfig,
) {
    let active = Arc::new(Mutex::new(ReaderRegistry::default()));
    let mut readers = Vec::new();

    while !stop.load(Ordering::Acquire) {
        match discover_devices(&config) {
            Ok(devices) => {
                for device in devices {
                    let registration = active.lock().ok().and_then(|mut active| {
                        active.register(&device.device_id, &device.event_path)
                    });
                    let Some(registration) = registration else {
                        continue;
                    };
                    if let Some(replaced_path) = &registration.replaced_path {
                        log::info!(
                            "input device {} moved from {} to {}; replacing stale reader",
                            device.device_id,
                            replaced_path.display(),
                            device.event_path.display()
                        );
                    }
                    let reader_sender = sender.clone();
                    let reader_stop = Arc::clone(&stop);
                    let reader_cancel = registration.cancel;
                    let reader_generation = registration.generation;
                    let reader_active = Arc::clone(&active);
                    readers.push(
                        thread::Builder::new()
                            .name(format!("retrofeel-{}", device.device_id))
                            .spawn(move || {
                                let device_id = device.device_id.clone();
                                let event_path = device.event_path.clone();
                                if let Err(error) = read_input_device(
                                    device,
                                    &reader_sender,
                                    &reader_stop,
                                    &reader_cancel,
                                ) {
                                    log::warn!(
                                        "input reader for {} stopped: {error}",
                                        event_path.display()
                                    );
                                }
                                if let Ok(mut active) = reader_active.lock() {
                                    active.finish(&device_id, reader_generation);
                                }
                            })
                            .expect("input reader thread should start"),
                    );
                }
            }
            Err(error) => log::warn!("cannot scan /dev/input: {error}"),
        }

        for _ in 0..10 {
            if stop.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    for reader in readers {
        let _ = reader.join();
    }
}

/// Probe identity and capabilities through sysfs only. Persistent evdev
/// descriptors are created only after recorder policy admits a candidate.
fn inspect_device(sysfs_event: &Path, event_path: &Path) -> io::Result<Option<InputDeviceInfo>> {
    let sysfs_device = sysfs_event.join("device");
    let name = read_trimmed(&sysfs_device.join("name"))?
        .unwrap_or_else(|| "Unnamed input device".to_string());
    let bus_type = read_hex_u16(&sysfs_device.join("id/bustype"))?.unwrap_or_default();
    let vendor = read_hex_u16(&sysfs_device.join("id/vendor"))?.unwrap_or_default();
    let product = read_hex_u16(&sysfs_device.join("id/product"))?.unwrap_or_default();
    let version = read_hex_u16(&sysfs_device.join("id/version"))?.unwrap_or_default();
    let unique = read_trimmed(&sysfs_device.join("uniq"))?;
    let physical_path = read_trimmed(&sysfs_device.join("phys"))?;
    let key_mask = read_trimmed(&sysfs_device.join("capabilities/key"))?.unwrap_or_default();
    let rel_mask = read_trimmed(&sysfs_device.join("capabilities/rel"))?.unwrap_or_default();
    let abs_mask = read_trimmed(&sysfs_device.join("capabilities/abs"))?.unwrap_or_default();

    let has_gamepad_key = capability_contains(&key_mask, KeyCode::BTN_SOUTH.code())
        || (0x120..=0x13f).any(|code| capability_contains(&key_mask, code));
    let has_stick = capability_contains(&abs_mask, AbsoluteAxisCode::ABS_X.0);
    let is_gamepad = has_gamepad_key || has_stick;
    let is_keyboard = capability_contains(&key_mask, KeyCode::KEY_A.code())
        && capability_contains(&key_mask, KeyCode::KEY_SPACE.code());
    let is_mouse = capability_contains(&rel_mask, RelativeAxisCode::REL_X.0)
        && capability_contains(&rel_mask, RelativeAxisCode::REL_Y.0);
    if !is_gamepad && !is_keyboard && !is_mouse {
        return Ok(None);
    }

    let is_virtual = fs::canonicalize(&sysfs_device)
        .ok()
        .is_some_and(|path| path.to_string_lossy().contains("/devices/virtual/input/"));
    let is_steam_virtual = is_steam_virtual_device(is_virtual, &name, vendor, product);
    let source = if is_steam_virtual {
        InputDeviceSource::SteamVirtual
    } else {
        InputDeviceSource::AllowlistedEvdev
    };
    let port = is_steam_virtual
        .then(|| parse_gamepad_port(&name))
        .flatten();
    let event_name = event_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("event");
    let device_id = stable_device_id(
        event_name,
        bus_type,
        vendor,
        product,
        version,
        &name,
        unique.as_deref(),
        physical_path.as_deref(),
        is_steam_virtual,
        port,
        is_gamepad,
        is_keyboard,
        is_mouse,
    );

    Ok(Some(InputDeviceInfo {
        device_id,
        event_path: event_path.to_path_buf(),
        name,
        bus_type,
        vendor,
        product,
        version,
        unique,
        physical_path,
        is_virtual,
        is_steam_virtual,
        port,
        source,
        is_gamepad,
        is_keyboard,
        is_mouse,
        admitted_capabilities: None,
        admission_reason: None,
        abs_ranges: BTreeMap::new(),
    }))
}

fn inspect_hidraw_device(
    path: &Path,
    physical_allowlist: &[PhysicalGamepadMatch],
) -> io::Result<Option<InputDeviceInfo>> {
    let Some(HidrawIdentity {
        bus,
        vendor,
        product,
        name,
        unique,
    }) = hidraw_identity(path)?
    else {
        return Ok(None);
    };
    let Some(candidate) = physical_allowlist.iter().find(|candidate| {
        candidate.vendor == vendor
            && candidate.product == product
            && candidate
                .name_contains
                .as_deref()
                .is_none_or(|fragment| name.contains(fragment))
            && candidate.unique_contains.as_deref().is_none_or(|fragment| {
                unique
                    .as_deref()
                    .is_some_and(|unique| unique.contains(fragment))
            })
    }) else {
        return Ok(None);
    };
    if disallowed_virtual_hidraw(is_virtual_hidraw_device(path), bus, candidate.allow_virtual) {
        return Ok(None);
    }
    let descriptor = read_hidraw_descriptor(path)?;
    if !HidGamepadDecoder::supports_interface(vendor, product, &descriptor) {
        return Ok(None);
    }
    let decoder = HidGamepadDecoder::new(vendor, product, &descriptor)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let event_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hidraw");
    let identity = unique
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(safe_device_id_fragment)
        .unwrap_or_else(|| event_name.to_string());
    let is_virtual = is_virtual_hidraw_device(path);
    Ok(Some(InputDeviceInfo {
        device_id: format!("physical-hid-{:04x}-{:04x}-{identity}", vendor, product),
        event_path: path.to_path_buf(),
        name,
        bus_type: bus,
        vendor,
        product,
        version: 0,
        unique,
        physical_path: None,
        is_virtual,
        is_steam_virtual: false,
        port: Some(0),
        source: InputDeviceSource::PhysicalFallback,
        is_gamepad: true,
        is_keyboard: false,
        is_mouse: false,
        admitted_capabilities: Some(InputCapabilities {
            gamepad: true,
            keyboard: false,
            mouse: false,
        }),
        admission_reason: Some("admitted by physical_gamepad_allowlist".into()),
        abs_ranges: decoder.abs_ranges().clone(),
    }))
}

fn hidraw_identity(path: &Path) -> io::Result<Option<HidrawIdentity>> {
    let Some(device_name) = path.file_name() else {
        return Ok(None);
    };
    let uevent = fs::read_to_string(
        Path::new("/sys/class/hidraw")
            .join(device_name)
            .join("device/uevent"),
    )?;
    let properties = uevent
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let Some(identifier) = properties.get("HID_ID") else {
        return Ok(None);
    };
    let mut parts = identifier.split(':');
    let bus = parts
        .next()
        .and_then(|value| u16::from_str_radix(value, 16).ok());
    let vendor = parts
        .next()
        .and_then(|value| u16::from_str_radix(value, 16).ok());
    let product = parts
        .next()
        .and_then(|value| u16::from_str_radix(value, 16).ok());
    let (Some(bus), Some(vendor), Some(product)) = (bus, vendor, product) else {
        return Ok(None);
    };
    let name = properties
        .get("HID_NAME")
        .copied()
        .unwrap_or("Unnamed HID device")
        .to_string();
    let unique = properties
        .get("HID_UNIQ")
        .filter(|value| !value.is_empty())
        .map(|value| (*value).to_string());
    Ok(Some(HidrawIdentity {
        bus,
        vendor,
        product,
        name,
        unique,
    }))
}

fn disallowed_virtual_hidraw(is_virtual: bool, bus: u16, allow_virtual: bool) -> bool {
    // BlueZ exposes real Bluetooth HID devices through the kernel's UHID
    // subtree, so their sysfs path contains `/devices/virtual/`. Keep the
    // explicit controller identity allowlist as the trust boundary and only
    // require `allow_virtual` for non-Bluetooth synthetic devices.
    is_virtual && bus != BUS_BLUETOOTH && !allow_virtual
}

fn is_virtual_hidraw_device(path: &Path) -> bool {
    let Some(device_name) = path.file_name() else {
        return false;
    };
    fs::canonicalize(
        Path::new("/sys/class/hidraw")
            .join(device_name)
            .join("device"),
    )
    .ok()
    .is_some_and(|path| path.to_string_lossy().contains("/devices/virtual/"))
}

fn read_hidraw_descriptor(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::OpenOptions::new().read(true).open(path)?;
    let mut size = 0i32;
    // SAFETY: both ioctls write only to the provided fixed-size structures,
    // which remain alive for the duration of each call.
    if unsafe { libc::ioctl(file.as_raw_fd(), HIDIOCGRDESCSIZE, &mut size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if size <= 0 || size as usize > HID_MAX_DESCRIPTOR_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid HID descriptor size {size}"),
        ));
    }
    let mut descriptor = HidrawReportDescriptor {
        size: size as u32,
        value: [0; HID_MAX_DESCRIPTOR_SIZE],
    };
    if unsafe { libc::ioctl(file.as_raw_fd(), HIDIOCGRDESC, &mut descriptor) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(descriptor.value[..descriptor.size as usize].to_vec())
}

fn is_steam_virtual_device(is_virtual: bool, name: &str, vendor: u16, product: u16) -> bool {
    if !is_virtual {
        return false;
    }

    vendor == STEAM_VENDOR
        || product == STEAM_VIRTUAL_GAMEPAD
        || name.contains("Steam")
        || name.starts_with("Microsoft X-Box 360 pad")
}

fn read_trimmed(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok((!value.trim().is_empty()).then(|| value.trim().to_string())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_hex_u16(path: &Path) -> io::Result<Option<u16>> {
    read_trimmed(path)?.map_or(Ok(None), |value| {
        u16::from_str_radix(value.trim_start_matches("0x"), 16)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    })
}

fn capability_contains(mask: &str, code: u16) -> bool {
    const WORD_BITS: usize = 64;
    let word_index = code as usize / WORD_BITS;
    let bit_index = code as usize % WORD_BITS;
    mask.split_ascii_whitespace()
        .rev()
        .nth(word_index)
        .and_then(|word| u64::from_str_radix(word, 16).ok())
        .is_some_and(|word| word & (1u64 << bit_index) != 0)
}

#[allow(clippy::too_many_arguments)]
fn stable_device_id(
    event_name: &str,
    bus_type: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: &str,
    unique: Option<&str>,
    physical_path: Option<&str>,
    is_steam_virtual: bool,
    port: Option<u8>,
    is_gamepad: bool,
    is_keyboard: bool,
    is_mouse: bool,
) -> String {
    if is_steam_virtual {
        if let Some(port) = port {
            return format!("steam-{vendor:04x}-{product:04x}-pad-{port}");
        }
    }

    // FNV-1a is deliberately fixed rather than `DefaultHasher`, whose output
    // is not a stable serialization contract. Event-node names participate
    // only as a last-resort identity; policy rejects such non-Steam devices.
    let fallback = if unique.is_none() && physical_path.is_none() && !is_steam_virtual {
        event_name
    } else {
        ""
    };
    let material = format!(
        "{bus_type:04x}:{vendor:04x}:{product:04x}:{version:04x}:{name}:{}:{}:{is_gamepad}:{is_keyboard}:{is_mouse}:{fallback}",
        unique.unwrap_or_default(),
        physical_path.unwrap_or_default(),
    );
    let hash = material.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("evdev-{vendor:04x}-{product:04x}-{hash:016x}")
}

fn safe_device_id_fragment(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "device".into()
    } else {
        value
    }
}

fn parse_gamepad_port(name: &str) -> Option<u8> {
    let (_, suffix) = name.rsplit_once("pad ")?;
    suffix.trim().parse().ok()
}

fn read_input_device(
    info: InputDeviceInfo,
    sender: &mpsc::Sender<InputMessage>,
    stop: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    if info
        .event_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("hidraw"))
    {
        read_hidraw_device(info, sender, stop, cancel)
    } else {
        read_device(info, sender, stop, cancel)
    }
}

fn reader_stopped(stop: &AtomicBool, cancel: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire) || cancel.load(Ordering::Acquire)
}

fn send_device_removed_unless_replaced(
    sender: &mpsc::Sender<InputMessage>,
    info: &InputDeviceInfo,
    cancel: &AtomicBool,
) {
    if !cancel.load(Ordering::Acquire) {
        let _ = sender.send(InputMessage::DeviceRemoved {
            device_id: info.device_id.clone(),
            event_path: info.event_path.clone(),
            boottime_us: boottime_us(),
        });
    }
}

fn read_hidraw_device(
    info: InputDeviceInfo,
    sender: &mpsc::Sender<InputMessage>,
    stop: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let descriptor = read_hidraw_descriptor(&info.event_path)?;
    let mut decoder = HidGamepadDecoder::new(info.vendor, info.product, &descriptor)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut file = fs::OpenOptions::new().read(true).open(&info.event_path)?;
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if sender
        .send(InputMessage::DeviceAdded {
            device: info.clone(),
            boottime_us: boottime_us(),
        })
        .is_err()
    {
        return Ok(());
    }

    let mut buffer = [0u8; HID_MAX_DESCRIPTOR_SIZE];
    while !reader_stopped(stop, cancel) {
        match file.read(&mut buffer) {
            Ok(0) => thread::sleep(Duration::from_millis(4)),
            Ok(length) => {
                let timestamp = boottime_us();
                let decoded = decoder
                    .decode(&buffer[..length])
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                for event in decoded {
                    if reader_stopped(stop, cancel) {
                        break;
                    }
                    if sender
                        .send(InputMessage::Event(RawInputEvent {
                            boottime_us: timestamp,
                            device_id: info.device_id.clone(),
                            event_type: event.event_type,
                            code: event.code,
                            value: event.value,
                            name: Some(event.name),
                            lifecycle: None,
                        }))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(4));
            }
            Err(error) => {
                send_device_removed_unless_replaced(sender, &info, cancel);
                return Err(error);
            }
        }
    }
    send_device_removed_unless_replaced(sender, &info, cancel);
    Ok(())
}

fn read_device(
    mut info: InputDeviceInfo,
    sender: &mpsc::Sender<InputMessage>,
    stop: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut device = open_admitted_device(&mut info)?;
    device.set_nonblocking(true)?;
    let mut clock = set_event_clock(device.as_raw_fd());
    let initial_time = boottime_us();
    if sender
        .send(InputMessage::DeviceAdded {
            device: info.clone(),
            boottime_us: initial_time,
        })
        .is_err()
    {
        return Ok(());
    }
    send_initial_state(&device, &info, initial_time, sender)?;

    let mut reopen_backoff_ms = 50u64;
    const REOPEN_MAX_BACKOFF_MS: u64 = 500;
    const REOPEN_TOTAL_BUDGET_MS: u64 = 30_000;

    while !reader_stopped(stop, cancel) {
        let action: Option<io::Error> = match device.fetch_events() {
            Ok(events) => {
                reopen_backoff_ms = 50;
                for event in events {
                    if reader_stopped(stop, cancel) {
                        break;
                    }
                    let event_type = event.event_type().0;
                    if event_type == EV_SYN || !event_is_admitted(&info, event_type, event.code()) {
                        continue;
                    }
                    let raw_timestamp_us = event
                        .timestamp()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros() as u64;
                    let boottime_us = match clock {
                        EventClock::BootTime => raw_timestamp_us,
                        EventClock::Monotonic => {
                            raw_timestamp_us.saturating_add(boottime_minus_monotonic_us())
                        }
                    };
                    let name = event_name(event.destructure());
                    if sender
                        .send(InputMessage::Event(RawInputEvent {
                            boottime_us,
                            device_id: info.device_id.clone(),
                            event_type,
                            code: event.code(),
                            value: event.value(),
                            name,
                            lifecycle: None,
                        }))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                None
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(4));
                None
            }
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENODEV) | Some(libc::ENOENT)
                ) =>
            {
                Some(error)
            }
            Err(error) => {
                send_device_removed_unless_replaced(sender, &info, cancel);
                return Err(error);
            }
        };
        let Some(churn_error) = action else {
            continue;
        };

        let path_display = info.event_path.display().to_string();
        let mut elapsed_ms = reopen_backoff_ms;
        thread::sleep(Duration::from_millis(reopen_backoff_ms));
        reopen_backoff_ms = reopen_backoff_ms
            .saturating_mul(2)
            .min(REOPEN_MAX_BACKOFF_MS);
        let reopened = loop {
            if reader_stopped(stop, cancel) {
                send_device_removed_unless_replaced(sender, &info, cancel);
                return Ok(());
            }
            let mut reopened_info = info.clone();
            match open_admitted_device(&mut reopened_info) {
                Ok(reopened) => break (reopened, reopened_info),
                Err(reopen_error)
                    if matches!(
                        reopen_error.raw_os_error(),
                        Some(libc::ENODEV) | Some(libc::ENOENT)
                    ) =>
                {
                    if elapsed_ms >= REOPEN_TOTAL_BUDGET_MS {
                        log::warn!(
                            "input reader for {} gave up after {elapsed_ms}ms of churn",
                            path_display
                        );
                        send_device_removed_unless_replaced(sender, &info, cancel);
                        return Err(churn_error);
                    }
                    thread::sleep(Duration::from_millis(reopen_backoff_ms));
                    elapsed_ms = elapsed_ms.saturating_add(reopen_backoff_ms);
                    reopen_backoff_ms = reopen_backoff_ms
                        .saturating_mul(2)
                        .min(REOPEN_MAX_BACKOFF_MS);
                }
                Err(reopen_error) => {
                    log::warn!(
                        "input reader for {} stopped reopening: {reopen_error}",
                        path_display
                    );
                    send_device_removed_unless_replaced(sender, &info, cancel);
                    return Err(churn_error);
                }
            }
        };
        device = reopened.0;
        info = reopened.1;
        device.set_nonblocking(true).ok();
        clock = set_event_clock(device.as_raw_fd());
        let reopened_time = boottime_us();
        if sender
            .send(InputMessage::DeviceAdded {
                device: info.clone(),
                boottime_us: reopened_time,
            })
            .is_err()
        {
            return Ok(());
        }
        send_initial_state(&device, &info, reopened_time, sender)?;
        reopen_backoff_ms = 50;
        log::info!(
            "input reader for {} recovered after ~{elapsed_ms}ms of churn",
            path_display
        );
    }

    send_device_removed_unless_replaced(sender, &info, cancel);
    Ok(())
}

fn open_admitted_device(info: &mut InputDeviceInfo) -> io::Result<Device> {
    let device = Device::open(&info.event_path)?;
    let input_id = device.input_id();
    let actual_name = device.name().unwrap_or("Unnamed input device");
    let actual_unique = device.unique_name().filter(|value| !value.is_empty());
    let actual_physical = device.physical_path().filter(|value| !value.is_empty());
    if actual_name != info.name
        || input_id.bus_type().0 != info.bus_type
        || input_id.vendor() != info.vendor
        || input_id.product() != info.product
        || input_id.version() != info.version
        || actual_unique != info.unique.as_deref()
        || actual_physical != info.physical_path.as_deref()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} changed identity between policy check and evdev open",
                info.event_path.display()
            ),
        ));
    }

    let actual_gamepad = device.supported_keys().is_some_and(|keys| {
        keys.contains(KeyCode::BTN_SOUTH)
            || keys.iter().any(|key| (0x120..=0x13f).contains(&key.code()))
    }) || device
        .supported_absolute_axes()
        .is_some_and(|axes| axes.contains(AbsoluteAxisCode::ABS_X));
    let actual_keyboard = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_SPACE));
    let actual_mouse = device.supported_relative_axes().is_some_and(|axes| {
        axes.contains(RelativeAxisCode::REL_X) && axes.contains(RelativeAxisCode::REL_Y)
    });
    let admitted = info.captured_capabilities();
    if (admitted.gamepad && !actual_gamepad)
        || (admitted.keyboard && !actual_keyboard)
        || (admitted.mouse && !actual_mouse)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} changed capabilities between policy check and evdev open",
                info.event_path.display()
            ),
        ));
    }

    info.abs_ranges = device
        .get_absinfo()
        .map(|ranges| {
            ranges
                .map(|(code, range)| {
                    (
                        code.0,
                        AbsRange {
                            minimum: range.minimum(),
                            maximum: range.maximum(),
                            flat: range.flat(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(device)
}

fn send_initial_state(
    device: &Device,
    info: &InputDeviceInfo,
    initial_time: u64,
    sender: &mpsc::Sender<InputMessage>,
) -> io::Result<()> {
    if info.captured_capabilities().gamepad {
        for (code, value) in device.get_absinfo().into_iter().flatten() {
            sender
                .send(InputMessage::Event(RawInputEvent {
                    boottime_us: initial_time,
                    device_id: info.device_id.clone(),
                    event_type: EV_ABS,
                    code: code.0,
                    value: value.value(),
                    name: Some(format!("{code:?}")),
                    lifecycle: None,
                }))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "input receiver closed"))?;
        }
    }
    for code in device.get_key_state().unwrap_or_default().iter() {
        if !event_is_admitted(info, EV_KEY, code.code()) {
            continue;
        }
        sender
            .send(InputMessage::Event(RawInputEvent {
                boottime_us: initial_time,
                device_id: info.device_id.clone(),
                event_type: EV_KEY,
                code: code.code(),
                value: 1,
                name: Some(format!("{code:?}")),
                lifecycle: None,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "input receiver closed"))?;
    }
    Ok(())
}

fn event_is_admitted(info: &InputDeviceInfo, event_type: u16, code: u16) -> bool {
    let admitted = info.captured_capabilities();
    match event_type {
        EV_KEY => admitted.contains(key_capability(code)),
        EV_REL => admitted.mouse,
        EV_ABS => admitted.gamepad,
        _ => false,
    }
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

#[derive(Clone, Copy)]
enum EventClock {
    BootTime,
    Monotonic,
}

fn set_event_clock(fd: libc::c_int) -> EventClock {
    let boot_time = libc::CLOCK_BOOTTIME;
    // SAFETY: EVIOCSCLOCKID reads one `int` from the supplied pointer and does
    // not retain it. `fd` is an open evdev descriptor owned by `Device`.
    if unsafe { libc::ioctl(fd, EVIOCSCLOCKID, &boot_time) } == 0 {
        return EventClock::BootTime;
    }

    let monotonic = libc::CLOCK_MONOTONIC;
    // SAFETY: same contract as the call above.
    if unsafe { libc::ioctl(fd, EVIOCSCLOCKID, &monotonic) } != 0 {
        log::warn!("EVIOCSCLOCKID failed; assuming the evdev monotonic default");
    }
    EventClock::Monotonic
}

fn boottime_minus_monotonic_us() -> u64 {
    let boot = clock_us(libc::CLOCK_BOOTTIME);
    let monotonic = clock_us(libc::CLOCK_MONOTONIC);
    boot.saturating_sub(monotonic)
}

pub fn boottime_us() -> u64 {
    clock_us(libc::CLOCK_BOOTTIME)
}

fn clock_us(clock: libc::clockid_t) -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` is a valid writable timespec for the duration of the call.
    if unsafe { libc::clock_gettime(clock, &mut value) } != 0 {
        return 0;
    }
    (value.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add((value.tv_nsec as u64) / 1_000)
}

fn event_name(summary: EventSummary) -> Option<String> {
    match summary {
        EventSummary::Key(_, code, _) => Some(format!("{code:?}")),
        EventSummary::RelativeAxis(_, code, _) => Some(format!("{code:?}")),
        EventSummary::AbsoluteAxis(_, code, _) => Some(format!("{code:?}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn capability_mask(codes: &[u16]) -> String {
        let word_count = codes
            .iter()
            .copied()
            .max()
            .map_or(1, |code| code as usize / 64 + 1);
        let mut words = vec![0u64; word_count];
        for code in codes {
            words[*code as usize / 64] |= 1 << (*code as usize % 64);
        }
        words
            .into_iter()
            .rev()
            .map(|word| format!("{word:x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn write_probe(sysfs_root: &Path, event_name: &str) {
        let device = sysfs_root.join(event_name).join("device");
        fs::create_dir_all(device.join("id")).unwrap();
        fs::create_dir_all(device.join("capabilities")).unwrap();
        fs::write(device.join("name"), "Composite test input\n").unwrap();
        fs::write(device.join("id/bustype"), "0003\n").unwrap();
        fs::write(device.join("id/vendor"), "1234\n").unwrap();
        fs::write(device.join("id/product"), "5678\n").unwrap();
        fs::write(device.join("id/version"), "0001\n").unwrap();
        fs::write(device.join("uniq"), "retrofeel-test-1\n").unwrap();
        fs::write(device.join("phys"), "usb-test/input0\n").unwrap();
        fs::write(
            device.join("capabilities/key"),
            capability_mask(&[
                KeyCode::KEY_A.code(),
                KeyCode::KEY_SPACE.code(),
                KeyCode::BTN_SOUTH.code(),
                272,
            ]),
        )
        .unwrap();
        fs::write(
            device.join("capabilities/rel"),
            capability_mask(&[RelativeAxisCode::REL_X.0, RelativeAxisCode::REL_Y.0]),
        )
        .unwrap();
        fs::write(
            device.join("capabilities/abs"),
            capability_mask(&[AbsoluteAxisCode::ABS_X.0]),
        )
        .unwrap();
    }

    #[test]
    fn parses_steam_virtual_gamepad_ports() {
        assert_eq!(parse_gamepad_port("Microsoft X-Box 360 pad 0"), Some(0));
        assert_eq!(parse_gamepad_port("Steam Virtual Gamepad"), None);
    }

    #[test]
    fn permits_allowlisted_bluetooth_hid_through_the_kernel_uhid_path() {
        assert!(!disallowed_virtual_hidraw(true, BUS_BLUETOOTH, false));
        assert!(disallowed_virtual_hidraw(true, 0x0003, false));
        assert!(!disallowed_virtual_hidraw(true, 0x0003, true));
        assert!(!disallowed_virtual_hidraw(false, 0x0003, false));
    }

    #[test]
    fn reads_sysfs_capability_words_in_kernel_order() {
        let mask = capability_mask(&[0, 63, 64, 304, 767]);
        for code in [0, 63, 64, 304, 767] {
            assert!(capability_contains(&mask, code));
        }
        assert!(!capability_contains(&mask, 305));
    }

    #[test]
    fn candidate_discovery_does_not_open_the_event_node() {
        let temporary = tempfile::tempdir().unwrap();
        let sysfs = temporary.path().join("sys-class-input");
        let nonexistent_dev = temporary.path().join("dev-input-does-not-exist");
        fs::create_dir_all(&sysfs).unwrap();
        write_probe(&sysfs, "event99");

        let devices = discover_evdev_candidates_in(&sysfs, &nonexistent_dev).unwrap();
        assert_eq!(devices.len(), 1);
        let device = &devices[0];
        assert_eq!(device.event_path, nonexistent_dev.join("event99"));
        assert!(device.is_gamepad);
        assert!(device.is_keyboard);
        assert!(device.is_mouse);
        assert_eq!(device.unique.as_deref(), Some("retrofeel-test-1"));
    }

    #[test]
    fn stable_identity_ignores_event_node_churn() {
        let first = stable_device_id(
            "event18",
            3,
            0x1234,
            0x5678,
            1,
            "Composite test input",
            Some("retrofeel-test-1"),
            Some("usb-test/input0"),
            false,
            None,
            true,
            true,
            true,
        );
        let recreated = stable_device_id(
            "event27",
            3,
            0x1234,
            0x5678,
            1,
            "Composite test input",
            Some("retrofeel-test-1"),
            Some("usb-test/input0"),
            false,
            None,
            true,
            true,
            true,
        );
        assert_eq!(first, recreated);
    }

    #[test]
    fn steam_composite_default_filters_keyboard_and_mouse_events() {
        let device = InputDeviceInfo {
            device_id: "steam-composite".into(),
            event_path: "/dev/input/event99".into(),
            name: "Microsoft X-Box 360 pad 9".into(),
            bus_type: 3,
            vendor: STEAM_VENDOR,
            product: STEAM_VIRTUAL_GAMEPAD,
            version: 1,
            unique: None,
            physical_path: Some("steam/input9".into()),
            is_virtual: true,
            is_steam_virtual: true,
            port: Some(9),
            source: InputDeviceSource::SteamVirtual,
            is_gamepad: true,
            is_keyboard: true,
            is_mouse: true,
            admitted_capabilities: Some(InputCapabilities {
                gamepad: true,
                keyboard: false,
                mouse: false,
            }),
            admission_reason: Some("test".into()),
            abs_ranges: BTreeMap::new(),
        };

        assert!(event_is_admitted(
            &device,
            EV_KEY,
            KeyCode::BTN_SOUTH.code()
        ));
        assert!(!event_is_admitted(&device, EV_KEY, KeyCode::KEY_A.code()));
        assert!(!event_is_admitted(&device, EV_KEY, 272));
        assert!(!event_is_admitted(
            &device,
            EV_REL,
            RelativeAxisCode::REL_X.0
        ));
    }
}
