use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::model::{InputCapabilities, InputCapability, InputDeviceInfo};

/// An exact, opt-in match for an evdev input device.
///
/// Vendor, product, and name always match exactly. `unique_id` and
/// `physical_path` add exact constraints when configured. Virtual devices are
/// denied unless the matching rule explicitly opts in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputDeviceRule {
    pub vendor: u16,
    pub product: u16,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_path: Option<String>,
    pub capabilities: Vec<InputCapability>,
    #[serde(default)]
    pub allow_virtual: bool,
}

impl InputDeviceRule {
    pub fn matches_identity(&self, device: &InputDeviceInfo) -> bool {
        self.vendor == device.vendor
            && self.product == device.product
            && self.name == device.name
            && self
                .unique_id
                .as_ref()
                .is_none_or(|unique| device.unique.as_ref() == Some(unique))
            && self
                .physical_path
                .as_ref()
                .is_none_or(|physical| device.physical_path.as_ref() == Some(physical))
    }

    pub(crate) fn match_issues(&self, device: &InputDeviceInfo) -> Vec<String> {
        if !self.matches_identity(device) {
            return Vec::new();
        }

        let mut issues = Vec::new();
        if device.is_virtual && !self.allow_virtual {
            issues.push("virtual device requires allow_virtual=true".into());
        }
        let available = device.capabilities();
        let mut unavailable = self
            .capabilities
            .iter()
            .copied()
            .filter(|capability| !available.contains(*capability))
            .map(capability_label)
            .collect::<Vec<_>>();
        unavailable.sort_unstable();
        unavailable.dedup();
        if !unavailable.is_empty() {
            issues.push(format!(
                "device does not expose requested {} capability",
                unavailable.join("+")
            ));
        }
        issues
    }
}

fn capability_label(capability: InputCapability) -> &'static str {
    match capability {
        InputCapability::Gamepad => "gamepad",
        InputCapability::Keyboard => "keyboard",
        InputCapability::Mouse => "mouse",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAdmission {
    pub capabilities: InputCapabilities,
    pub reason: String,
}

impl DeviceAdmission {
    pub fn is_admitted(&self) -> bool {
        !self.capabilities.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalGamepadMatch {
    pub vendor: u16,
    pub product: u16,
    #[serde(default)]
    pub name_contains: Option<String>,
    #[serde(default)]
    pub unique_contains: Option<String>,
    /// Test-only escape hatch for a deliberately allowlisted virtual HID device.
    /// Real fallback controllers must come from physical hidraw hardware.
    #[serde(default)]
    pub allow_virtual: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeckTranscriptionConfig {
    /// Transcribe Steam's completed mixed audio track after finalization.
    pub automatic: bool,
    pub executable: PathBuf,
    pub model: Option<PathBuf>,
    pub model_id: String,
    pub language: String,
    pub threads: u8,
}

impl Default for DeckTranscriptionConfig {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/deck"));
        Self {
            automatic: false,
            executable: home.join(".local/bin/retrofeel-whisper-cli"),
            model: None,
            model_id: "whisper-base.en".into(),
            language: "en".into(),
            threads: 4,
        }
    }
}

/// An exact opt-in for mirroring portable copies of completed recordings.
///
/// Archive rules match Steam's full game ID, including non-Steam shortcut IDs.
/// Keeping the destination and display name in configuration makes the archive
/// worker reusable without teaching RetroFeel about individual games.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeckArchiveFormat {
    /// Compatibility default for configurations written before MP4 publishing.
    #[default]
    Matroska,
    /// YouTube-ready ISO Base Media container with the metadata atom first.
    Mp4,
}

impl DeckArchiveFormat {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Matroska => "mkv",
            Self::Mp4 => "mp4",
        }
    }

    pub const fn container_name(self) -> &'static str {
        match self {
            Self::Matroska => "matroska",
            Self::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckArchiveRule {
    pub game_id: String,
    pub display_name: String,
    pub destination_dir: PathBuf,
    /// Omitted by older configurations, which must continue producing MKV.
    #[serde(default)]
    pub format: DeckArchiveFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeckYoutubePrivacy {
    #[default]
    Unlisted,
    Private,
}

impl DeckYoutubePrivacy {
    pub const fn as_api_str(self) -> &'static str {
        match self {
            Self::Unlisted => "unlisted",
            Self::Private => "private",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeckYoutubeCategory {
    #[default]
    Gaming,
}

impl DeckYoutubeCategory {
    pub const fn api_id(self) -> &'static str {
        match self {
            Self::Gaming => "20",
        }
    }
}

/// Optional, future-only YouTube publishing policy for verified MP4 archives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeckYoutubeConfig {
    /// Stable local identity used to select this publisher from the CLI and to
    /// bind public receipts to one OAuth/channel route.
    pub publisher_id: String,
    /// Exact archive game IDs routed to this channel. An empty list retains
    /// the legacy single-publisher behavior of considering every MP4 rule.
    pub game_ids: Vec<String>,
    /// The service may authenticate and report status while automatic uploads
    /// remain disabled during API compliance review.
    pub enabled: bool,
    /// Immutable rollout boundary. Sessions must begin strictly after this
    /// RFC 3339 instant; migrated historical archives are never eligible.
    pub upload_not_before: String,
    pub oauth_client_path: PathBuf,
    pub oauth_token_path: PathBuf,
    pub expected_channel_id: String,
    pub privacy_status: DeckYoutubePrivacy,
    pub category: DeckYoutubeCategory,
    pub caption_language: String,
    pub timezone: String,
    pub retention_days: u32,
    /// Defer uploads while a Steam-launched game is running on the Deck.
    pub idle_only: bool,
}

impl Default for DeckYoutubeConfig {
    fn default() -> Self {
        let state = default_youtube_state_dir();
        Self {
            publisher_id: "legacy".into(),
            game_ids: Vec::new(),
            enabled: false,
            upload_not_before: String::new(),
            oauth_client_path: state.join("oauth-client.json"),
            oauth_token_path: state.join("oauth-token.json"),
            expected_channel_id: String::new(),
            privacy_status: DeckYoutubePrivacy::Unlisted,
            category: DeckYoutubeCategory::Gaming,
            caption_language: "en".into(),
            timezone: "America/Vancouver".into(),
            retention_days: 30,
            idle_only: false,
        }
    }
}

impl DeckYoutubeConfig {
    pub fn state_dir(&self) -> PathBuf {
        self.oauth_token_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_youtube_state_dir)
    }
}

fn default_youtube_state_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/deck"));
    home.join(".local/state/retrofeel/youtube")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecorderConfig {
    pub steam_root: PathBuf,
    pub recordings_dir: PathBuf,
    pub ffprobe: PathBuf,
    pub ffmpeg: PathBuf,
    pub input_ring_seconds: u64,
    pub clip_timeout_seconds: u64,
    /// Compatibility-only storage for pre-universal fallback configs.
    /// Physical fallback policy no longer reads game IDs.
    #[serde(rename = "physical_fallback_game_ids", skip_serializing)]
    pub legacy_physical_fallback_game_ids: Vec<String>,
    pub physical_gamepad_allowlist: Vec<PhysicalGamepadMatch>,
    /// Exact opt-ins for non-default evdev devices and keyboard/mouse
    /// capabilities. Steam virtual gamepads remain admitted by default; their
    /// keyboard/mouse capabilities do not.
    pub input_device_allowlist: Vec<InputDeviceRule>,
    pub finalize_retry_interval_seconds: u64,
    pub finalize_retry_attempts: u32,
    /// Exact game IDs whose completed recordings should be remuxed into an
    /// additional user-facing archive directory.
    pub recording_archives: Vec<DeckArchiveRule>,
    /// Delay between background archive reconciliation passes.
    pub archive_sync_interval_seconds: u64,
    pub transcription: DeckTranscriptionConfig,
    /// Omitted entirely until YouTube publishing is configured.
    pub youtube: Option<DeckYoutubeConfig>,
    /// Multiple independently authorized YouTube destinations. This is
    /// mutually exclusive with the legacy `youtube` field.
    pub youtube_publishers: Vec<DeckYoutubeConfig>,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/deck"));
        Self {
            steam_root: home.join(".local/share/Steam"),
            recordings_dir: home.join("retrofeel-recordings"),
            ffprobe: PathBuf::from("/usr/bin/ffprobe"),
            ffmpeg: PathBuf::from("/usr/bin/ffmpeg"),
            input_ring_seconds: 5,
            clip_timeout_seconds: 60,
            legacy_physical_fallback_game_ids: Vec::new(),
            physical_gamepad_allowlist: Vec::new(),
            input_device_allowlist: Vec::new(),
            finalize_retry_interval_seconds: 2,
            finalize_retry_attempts: 10,
            recording_archives: Vec::new(),
            archive_sync_interval_seconds: 60,
            transcription: DeckTranscriptionConfig::default(),
            youtube: None,
            youtube_publishers: Vec::new(),
        }
    }
}

impl RecorderConfig {
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/deck"));
        home.join(".config/retrofeel/deck-recorder.ron")
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Self =
            ron::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        if !config.legacy_physical_fallback_game_ids.is_empty() {
            log::warn!(
                "physical_fallback_game_ids is deprecated and ignored; exact allowlisted physical controllers are now recorded alongside Steam virtual pads for every game"
            );
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (index, rule) in self.input_device_allowlist.iter().enumerate() {
            if rule.name.trim().is_empty() {
                bail!(
                    "input_device_allowlist entry {} has an empty name",
                    index + 1
                );
            }
            if rule.capabilities.is_empty() {
                bail!(
                    "input_device_allowlist entry {} has no capabilities",
                    index + 1
                );
            }
        }
        if !self.recording_archives.is_empty() && self.archive_sync_interval_seconds == 0 {
            bail!("archive_sync_interval_seconds must be greater than zero when archives are configured");
        }
        let mut archive_destinations = BTreeSet::new();
        for (index, rule) in self.recording_archives.iter().enumerate() {
            if rule.game_id.is_empty() || !rule.game_id.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!(
                    "recording_archives entry {} has a non-numeric game_id",
                    index + 1
                );
            }
            if rule.display_name.trim().is_empty() {
                bail!(
                    "recording_archives entry {} has an empty display_name",
                    index + 1
                );
            }
            if !rule.destination_dir.is_absolute() {
                bail!(
                    "recording_archives entry {} destination_dir must be absolute",
                    index + 1
                );
            }
            let identity = (rule.game_id.clone(), rule.destination_dir.clone());
            if !archive_destinations.insert(identity) {
                bail!(
                    "recording_archives entry {} duplicates an earlier game/destination pair",
                    index + 1
                );
            }
        }
        if self.youtube.is_some() && !self.youtube_publishers.is_empty() {
            bail!("configure either legacy youtube or youtube_publishers, not both");
        }
        let publishers = self
            .youtube_publishers
            .iter()
            .chain(self.youtube.iter())
            .collect::<Vec<_>>();
        let archive_game_ids = self
            .recording_archives
            .iter()
            .map(|rule| rule.game_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut publisher_ids = BTreeSet::new();
        let mut routed_game_ids = BTreeSet::new();
        let mut token_paths = BTreeSet::new();
        for youtube in publishers {
            if youtube.publisher_id.trim().is_empty()
                || !youtube
                    .publisher_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                bail!("youtube publisher_id must use lowercase ASCII letters, digits, and dashes");
            }
            if !publisher_ids.insert(youtube.publisher_id.as_str()) {
                bail!("youtube publisher_id values must be unique");
            }
            if !self.youtube_publishers.is_empty() && youtube.game_ids.is_empty() {
                bail!("youtube_publishers entries must route at least one game_id");
            }
            if !youtube.game_ids.is_empty()
                && !youtube
                    .game_ids
                    .iter()
                    .any(|game_id| archive_game_ids.contains(game_id.as_str()))
            {
                bail!("youtube publisher must include at least one configured archive game_id");
            }
            for game_id in &youtube.game_ids {
                if game_id.is_empty() || !game_id.bytes().all(|byte| byte.is_ascii_digit()) {
                    bail!("youtube publisher game_id values must be numeric");
                }
                if !routed_game_ids.insert(game_id.as_str()) {
                    bail!("an archive game_id may be routed to only one YouTube publisher");
                }
            }
            if DateTime::parse_from_rfc3339(&youtube.upload_not_before).is_err() {
                bail!("youtube upload_not_before must be a non-empty RFC 3339 timestamp");
            }
            if youtube.expected_channel_id.len() != 24
                || !youtube.expected_channel_id.starts_with("UC")
                || !youtube
                    .expected_channel_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                bail!("youtube expected_channel_id must be an exact 24-character UC channel ID");
            }
            if !youtube.oauth_client_path.is_absolute() || !youtube.oauth_token_path.is_absolute() {
                bail!("youtube OAuth client and token paths must be absolute");
            }
            if youtube.oauth_client_path == youtube.oauth_token_path {
                bail!("youtube OAuth client and token paths must be different");
            }
            if !token_paths.insert(youtube.oauth_token_path.as_path()) {
                bail!("youtube publishers must use distinct OAuth token paths");
            }
            if youtube.caption_language != "en" {
                bail!("youtube caption_language is fixed to en");
            }
            if youtube.timezone != "America/Vancouver" {
                bail!("youtube timezone is fixed to America/Vancouver");
            }
            if !(1..=365).contains(&youtube.retention_days) {
                bail!("youtube retention_days must be between 1 and 365");
            }
        }
        Ok(())
    }

    pub fn configured_youtube_publishers(&self) -> Vec<&DeckYoutubeConfig> {
        if self.youtube_publishers.is_empty() {
            self.youtube.iter().collect()
        } else {
            self.youtube_publishers.iter().collect()
        }
    }

    pub fn device_admission(&self, device: &InputDeviceInfo) -> DeviceAdmission {
        let available = device.capabilities();
        let mut admitted = InputCapabilities::default();
        let mut sources = Vec::new();
        let mut notes = Vec::new();

        if device.is_steam_virtual && available.gamepad {
            admitted.gamepad = true;
            sources.push("Steam virtual gamepad policy".to_string());
        }

        let mut exact_matches = 0usize;
        for (index, rule) in self.input_device_allowlist.iter().enumerate() {
            if !rule.matches_identity(device) {
                continue;
            }
            exact_matches += 1;
            if device.is_virtual && !rule.allow_virtual {
                notes.push(format!(
                    "allowlist entry {} blocked: virtual device requires allow_virtual=true",
                    index + 1
                ));
                continue;
            }
            let mut unavailable = rule
                .capabilities
                .iter()
                .copied()
                .filter(|capability| !available.contains(*capability))
                .map(capability_label)
                .collect::<Vec<_>>();
            unavailable.sort_unstable();
            unavailable.dedup();
            if !unavailable.is_empty() {
                notes.push(format!(
                    "allowlist entry {} blocked: unavailable {} capability",
                    index + 1,
                    unavailable.join("+")
                ));
                continue;
            }
            let mut granted = Vec::new();
            for capability in rule.capabilities.iter().copied() {
                admitted.insert(capability);
                granted.push(capability_label(capability));
            }
            granted.sort_unstable();
            granted.dedup();
            if !granted.is_empty() {
                sources.push(format!(
                    "allowlist entry {} ({})",
                    index + 1,
                    granted.join("+")
                ));
            }
        }

        if !admitted.is_empty() && !device.has_stable_identity() {
            return DeviceAdmission {
                capabilities: InputCapabilities::default(),
                reason: "rejected: device exposes neither a unique ID, physical path, nor stable Steam port"
                    .into(),
            };
        }

        let reason = if admitted.is_empty() {
            if !notes.is_empty() {
                format!("rejected: {}", notes.join("; "))
            } else if exact_matches > 0 {
                "rejected: exact rule requested no capabilities exposed by this device".into()
            } else if available.keyboard || available.mouse {
                "rejected: keyboard/mouse capture requires an exact input_device_allowlist entry"
                    .into()
            } else {
                "rejected: device is not a known Steam virtual gamepad and no exact allowlist entry matched"
                    .into()
            }
        } else {
            let mut reason = format!(
                "admitted {} via {}",
                admitted.labels().join("+"),
                sources.join("; ")
            );
            if !notes.is_empty() {
                reason.push_str("; ");
                reason.push_str(&notes.join("; "));
            }
            reason
        };

        DeviceAdmission {
            capabilities: admitted,
            reason,
        }
    }

    pub fn admitted_evdev(&self, mut device: InputDeviceInfo) -> Option<InputDeviceInfo> {
        let admission = self.device_admission(&device);
        if !admission.is_admitted() {
            return None;
        }
        device.admitted_capabilities = Some(admission.capabilities);
        device.admission_reason = Some(admission.reason);
        Some(device)
    }

    pub fn physical_gamepad_match(
        &self,
        vendor: u16,
        product: u16,
        name: &str,
        unique: Option<&str>,
    ) -> Option<&PhysicalGamepadMatch> {
        self.physical_gamepad_priority(vendor, product, name, unique)
            .map(|priority| &self.physical_gamepad_allowlist[priority])
    }

    pub fn physical_gamepad_priority(
        &self,
        vendor: u16,
        product: u16,
        name: &str,
        unique: Option<&str>,
    ) -> Option<usize> {
        self.physical_gamepad_allowlist
            .iter()
            .position(|candidate| {
                candidate.vendor == vendor
                    && candidate.product == product
                    && candidate
                        .name_contains
                        .as_deref()
                        .is_none_or(|fragment| name.contains(fragment))
                    && candidate.unique_contains.as_deref().is_none_or(|fragment| {
                        unique.is_some_and(|unique| unique.contains(fragment))
                    })
            })
    }

    pub fn physical_gamepad_allowed(
        &self,
        vendor: u16,
        product: u16,
        name: &str,
        unique: Option<&str>,
    ) -> bool {
        self.physical_gamepad_match(vendor, product, name, unique)
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::InputDeviceSource;

    fn device() -> InputDeviceInfo {
        InputDeviceInfo {
            device_id: "evdev-test".into(),
            event_path: "/dev/input/event9".into(),
            name: "Test Composite".into(),
            bus_type: 3,
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
            unique: Some("unit-1".into()),
            physical_path: Some("usb-test/input0".into()),
            is_virtual: false,
            is_steam_virtual: false,
            port: None,
            source: InputDeviceSource::AllowlistedEvdev,
            is_gamepad: true,
            is_keyboard: true,
            is_mouse: true,
            admitted_capabilities: None,
            admission_reason: None,
            abs_ranges: Default::default(),
        }
    }

    fn rule(capabilities: Vec<InputCapability>) -> InputDeviceRule {
        InputDeviceRule {
            vendor: 0x1234,
            product: 0x5678,
            name: "Test Composite".into(),
            unique_id: Some("unit-1".into()),
            physical_path: None,
            capabilities,
            allow_virtual: false,
        }
    }

    #[test]
    fn packaged_example_config_parses() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/deck-recorder.example.ron");
        let config = RecorderConfig::load(Some(&path)).unwrap();
        assert_eq!(
            config.recordings_dir,
            PathBuf::from("/home/deck/retrofeel-recordings")
        );
    }

    #[test]
    fn legacy_game_ids_parse_but_only_the_controller_allowlist_is_active() {
        let config: RecorderConfig = ron::from_str(
            r#"(
                physical_fallback_game_ids: ["43"],
                physical_gamepad_allowlist: [(
                    vendor: 11720,
                    product: 12307,
                    name_contains: Some("8BitDo Ultimate wireless Controller for PC"),
                    unique_contains: Some("E417D8FA7671"),
                )],
            )"#,
        )
        .unwrap();

        assert_eq!(config.legacy_physical_fallback_game_ids, ["43"]);
        assert!(config.physical_gamepad_allowed(
            0x2dc8,
            0x3013,
            "8BitDo 8BitDo Ultimate wireless Controller for PC",
            Some("E417D8FA7671")
        ));
        assert!(!config.physical_gamepad_allowed(
            0x2dc8,
            0x3013,
            "8BitDo 8BitDo Ultimate wireless Controller for PC",
            Some("another-controller")
        ));
        assert!(!config.physical_gamepad_allowed(
            0x2dc8,
            0x3013,
            "Unrelated controller",
            Some("E417D8FA7671")
        ));
        assert!(!config.physical_gamepad_allowed(
            0x28de,
            0x11ff,
            "Microsoft X-Box 360 pad 0",
            None
        ));
    }

    #[test]
    fn retry_defaults_are_bounded_and_nonzero() {
        let config = RecorderConfig::default();
        assert!(config.finalize_retry_interval_seconds > 0);
        assert!(config.finalize_retry_attempts > 0);
        assert!(config.recording_archives.is_empty());
        assert!(config.archive_sync_interval_seconds > 0);
        assert!(!config.transcription.automatic);
        assert!(config.transcription.model.is_none());
        assert!(config.youtube.is_none());
        assert!(config.youtube_publishers.is_empty());
    }

    #[test]
    fn archive_rules_require_exact_ids_and_absolute_unique_destinations() {
        let mut config = RecorderConfig::default();
        config.recording_archives.push(DeckArchiveRule {
            game_id: "9223372041183297536".into(),
            display_name: "Example Meadow World".into(),
            destination_dir: PathBuf::from("/home/deck/Videos/Example-Meadow-World-History"),
            format: DeckArchiveFormat::Mp4,
        });
        assert!(config.validate().is_ok());

        config
            .recording_archives
            .push(config.recording_archives[0].clone());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicates"));
        config.recording_archives.pop();

        config.recording_archives[0].game_id = "example-meadow".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("non-numeric"));
        config.recording_archives[0].game_id = "9223372041183297536".into();
        config.recording_archives[0].destination_dir = PathBuf::from("Videos");
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must be absolute"));
    }

    #[test]
    fn older_archive_rules_default_to_matroska() {
        let config: RecorderConfig = ron::from_str(
            r#"(
                recording_archives: [(
                    game_id: "9223372058363166720",
                    display_name: "Variant Hunter",
                    destination_dir: "/home/deck/Videos/Variant-Hunter-History",
                )],
            )"#,
        )
        .unwrap();

        assert_eq!(
            config.recording_archives[0].format,
            DeckArchiveFormat::Matroska
        );
    }

    #[test]
    fn youtube_config_is_optional_and_validates_fixed_contracts() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = RecorderConfig {
            youtube: Some(DeckYoutubeConfig {
                enabled: false,
                upload_not_before: "2026-08-26T16:26:02Z".into(),
                oauth_client_path: temporary.path().join("client.json"),
                oauth_token_path: temporary.path().join("token.json"),
                expected_channel_id: "UC1111111111111111111111".into(),
                ..DeckYoutubeConfig::default()
            }),
            ..RecorderConfig::default()
        };
        assert!(config.validate().is_ok());

        config.youtube.as_mut().unwrap().caption_language = "fr".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("fixed to en"));
        config.youtube.as_mut().unwrap().caption_language = "en".into();
        config.youtube.as_mut().unwrap().upload_not_before = "tomorrow".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("RFC 3339"));
        config.youtube.as_mut().unwrap().upload_not_before = "2026-08-26T16:26:02Z".into();
        config.youtube.as_mut().unwrap().expected_channel_id = "@variant-hunter".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("24-character UC channel ID"));
        config.youtube.as_mut().unwrap().expected_channel_id = "UC1111111111111111111111".into();
        config.youtube.as_mut().unwrap().retention_days = 0;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("between 1 and 365"));
    }

    #[test]
    fn multi_publisher_routes_are_unique_and_use_separate_tokens() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = |game_id: &str, name: &str| DeckArchiveRule {
            game_id: game_id.into(),
            display_name: name.into(),
            destination_dir: PathBuf::from(format!("/home/deck/Videos/{name}")),
            format: DeckArchiveFormat::Mp4,
        };
        let publisher = |id: &str, game_ids: Vec<String>| DeckYoutubeConfig {
            publisher_id: id.into(),
            game_ids,
            upload_not_before: "2026-07-30T00:00:00Z".into(),
            oauth_client_path: temporary.path().join("client.json"),
            oauth_token_path: temporary.path().join(id).join("token.json"),
            expected_channel_id: if id == "variant-hunter" {
                "UC1111111111111111111111".into()
            } else {
                "UC0000000000000000000000".into()
            },
            retention_days: 7,
            idle_only: true,
            ..DeckYoutubeConfig::default()
        };
        let mut config = RecorderConfig {
            recording_archives: vec![archive("1", "Variant-Hunter"), archive("2", "secondary")],
            youtube_publishers: vec![
                publisher("variant-hunter", vec!["1".into()]),
                publisher("example-meadow-world", vec!["2".into()]),
            ],
            ..RecorderConfig::default()
        };

        assert!(config.validate().is_ok());
        config.youtube_publishers[0].game_ids.push("3".into());
        assert!(config.validate().is_ok());
        config.youtube_publishers[0].game_ids.pop();
        config.youtube_publishers[1].game_ids = vec!["1".into()];
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("only one"));
        config.youtube_publishers[1].game_ids = vec!["2".into()];
        config.youtube_publishers[1].oauth_token_path =
            config.youtube_publishers[0].oauth_token_path.clone();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("distinct OAuth token"));
    }

    #[test]
    fn keyboard_and_mouse_are_rejected_without_an_exact_rule() {
        let admission = RecorderConfig::default().device_admission(&device());
        assert!(!admission.is_admitted());
        assert!(admission.reason.contains("requires an exact"));
    }

    #[test]
    fn exact_rule_admits_only_requested_composite_capabilities() {
        let mut config = RecorderConfig::default();
        config.input_device_allowlist.push(rule(vec![
            InputCapability::Gamepad,
            InputCapability::Keyboard,
            InputCapability::Mouse,
        ]));
        let admission = config.device_admission(&device());
        assert_eq!(
            admission.capabilities,
            InputCapabilities {
                gamepad: true,
                keyboard: true,
                mouse: true,
            }
        );

        let mut different = device();
        different.name = "Almost Test Composite".into();
        assert!(!config.device_admission(&different).is_admitted());
    }

    #[test]
    fn virtual_match_requires_explicit_virtual_consent() {
        let mut virtual_device = device();
        virtual_device.is_virtual = true;
        let mut config = RecorderConfig::default();
        config.input_device_allowlist.push(rule(vec![
            InputCapability::Keyboard,
            InputCapability::Mouse,
        ]));
        let denied = config.device_admission(&virtual_device);
        assert!(!denied.is_admitted());
        assert!(denied.reason.contains("allow_virtual=true"));

        config.input_device_allowlist[0].allow_virtual = true;
        let admitted = config.device_admission(&virtual_device);
        assert!(admitted.capabilities.keyboard);
        assert!(admitted.capabilities.mouse);
    }

    #[test]
    fn steam_virtual_default_admits_gamepad_but_not_keyboard_or_mouse() {
        let mut steam = device();
        steam.is_virtual = true;
        steam.is_steam_virtual = true;
        steam.port = Some(0);
        steam.source = InputDeviceSource::SteamVirtual;
        let admission = RecorderConfig::default().device_admission(&steam);
        assert_eq!(
            admission.capabilities,
            InputCapabilities {
                gamepad: true,
                keyboard: false,
                mouse: false,
            }
        );
    }

    #[test]
    fn blocked_virtual_rule_does_not_hide_default_gamepad_admission() {
        let mut steam = device();
        steam.is_virtual = true;
        steam.is_steam_virtual = true;
        steam.port = Some(0);
        steam.source = InputDeviceSource::SteamVirtual;
        let mut config = RecorderConfig::default();
        config
            .input_device_allowlist
            .push(rule(vec![InputCapability::Keyboard]));

        let admission = config.device_admission(&steam);
        assert!(admission.capabilities.gamepad);
        assert!(!admission.capabilities.keyboard);
        assert!(admission.reason.contains("allow_virtual=true"));
        assert_eq!(
            config.input_device_allowlist[0].match_issues(&steam),
            ["virtual device requires allow_virtual=true"]
        );
    }

    #[test]
    fn exact_rule_fails_closed_when_a_requested_capability_is_missing() {
        let mut keyboard_only = device();
        keyboard_only.is_gamepad = false;
        keyboard_only.is_mouse = false;
        let keyboard_and_mouse = rule(vec![InputCapability::Keyboard, InputCapability::Mouse]);

        assert_eq!(
            keyboard_and_mouse.match_issues(&keyboard_only),
            ["device does not expose requested mouse capability"]
        );
        let mut config = RecorderConfig::default();
        config.input_device_allowlist.push(keyboard_and_mouse);
        let admission = config.device_admission(&keyboard_only);
        assert!(!admission.capabilities.keyboard);
        assert!(!admission.capabilities.mouse);
        assert!(admission.reason.contains("unavailable mouse"));
    }

    #[test]
    fn exact_rule_rejects_a_non_steam_device_without_stable_identity() {
        let mut unstable = device();
        unstable.unique = None;
        unstable.physical_path = None;
        let mut keyboard_rule = rule(vec![InputCapability::Keyboard]);
        keyboard_rule.unique_id = None;
        let mut config = RecorderConfig::default();
        config.input_device_allowlist.push(keyboard_rule);

        let admission = config.device_admission(&unstable);
        assert!(!admission.is_admitted());
        assert!(admission.reason.contains("stable Steam port"));
    }
}
