//! Future-only, crash-safe YouTube publishing for verified Deck MP4 archives.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use retrofeel_types::{ExternalCaptureStatus, SessionManifest, TranscriptionJobState};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use url::Url;

use crate::archive::current_archive_verification;
use crate::config::{
    DeckArchiveFormat, DeckArchiveRule, DeckYoutubeConfig, DeckYoutubePrivacy, RecorderConfig,
};
use crate::session::SessionSummary;

const YOUTUBE_RECEIPT_VERSION: u32 = 2;
const OAUTH_TOKEN_VERSION: u32 = 1;
const RESUMABLE_STATE_VERSION: u32 = 1;
const ROLLOUT_POLICY_VERSION: u32 = 2;
const QUOTA_LEDGER_VERSION: u32 = 1;
const OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/youtube.force-ssl";
const OAUTH_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const YOUTUBE_API_BASE: &str = "https://www.googleapis.com/youtube/v3";
const YOUTUBE_UPLOAD_BASE: &str = "https://www.googleapis.com/upload/youtube/v3";
const MP4_CONTENT_TYPE: &str = "video/mp4";
const UPLOAD_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAX_API_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const OAUTH_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REPLACEMENT_GRACE_SECONDS: i64 = 15 * 60;
const MAX_UPLOAD_INTERRUPTS_PER_SYNC: u8 = 5;
const DAILY_VIDEO_INSERT_LIMIT: u32 = 95;
const DAILY_ORDINARY_QUOTA_LIMIT: u32 = 8_800;
const CAPTION_INSERT_COST: u32 = 400;
const CAPTION_UPDATE_COST: u32 = 450;

#[derive(Debug, Default, Serialize)]
pub struct YoutubeStatusReport {
    pub publisher_id: Option<String>,
    pub configured: bool,
    pub enabled: bool,
    pub authenticated: bool,
    pub token_expires_at_unix_seconds: Option<i64>,
    pub expected_channel_id: Option<String>,
    pub channel_matches: Option<bool>,
    pub state_dir: Option<PathBuf>,
    pub upload_receipts: usize,
    pub published_videos: usize,
    pub pending_uploads: usize,
    pub pruned_videos: usize,
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct YoutubeSyncReport {
    pub publisher_id: Option<String>,
    pub configured: bool,
    pub enabled: bool,
    pub dry_run: bool,
    pub already_running: bool,
    pub deferred_for_activity: bool,
    pub scanned_sessions: usize,
    pub cutoff_ineligible: usize,
    pub unverified_archives: usize,
    pub would_upload: usize,
    pub initiated_uploads: usize,
    pub resumed_uploads: usize,
    pub uploaded_videos: usize,
    pub reconciled_videos: usize,
    pub processing_pending: usize,
    pub privacy_mismatches: usize,
    pub captions_uploaded: usize,
    pub captions_updated: usize,
    pub terminal_no_caption: usize,
    pub pruned_videos: usize,
    pub quota_deferred: usize,
    pub failures: Vec<String>,
}

impl YoutubeSyncReport {
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty() || self.privacy_mismatches > 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum YoutubeProcessingState {
    #[default]
    AwaitingUpload,
    Uploading,
    Processing,
    Succeeded,
    Failed,
    PrivacyMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NoCaptionReason {
    Empty,
    Failed,
    Unavailable,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CaptionState {
    Pending {
        existing_caption_id: Option<String>,
    },
    Uploaded {
        caption_id: String,
        srt_sha256: String,
    },
    NoCaption {
        reason: NoCaptionReason,
        existing_caption_id: Option<String>,
    },
}

impl Default for CaptionState {
    fn default() -> Self {
        Self::Pending {
            existing_caption_id: None,
        }
    }
}

impl CaptionState {
    const fn is_terminal(&self) -> bool {
        matches!(self, Self::Uploaded { .. } | Self::NoCaption { .. })
    }

    fn existing_caption_id(&self) -> Option<String> {
        match self {
            Self::Uploaded { caption_id, .. } => Some(caption_id.clone()),
            Self::Pending {
                existing_caption_id,
            }
            | Self::NoCaption {
                existing_caption_id,
                ..
            } => existing_caption_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct YoutubeReceipt {
    format_version: u32,
    #[serde(default = "legacy_publisher_id")]
    publisher_id: String,
    #[serde(default)]
    expected_channel_id: String,
    #[serde(default = "default_unlisted_privacy")]
    requested_privacy_status: String,
    session_id: String,
    video_id: Option<String>,
    upload_started_at_unix_seconds: Option<i64>,
    upload_completed_at_unix_seconds: Option<i64>,
    processing_state: YoutubeProcessingState,
    #[serde(alias = "confirmed_unlisted_at_unix_seconds")]
    confirmed_privacy_at_unix_seconds: Option<i64>,
    caption: CaptionState,
    prune_after_unix_seconds: Option<i64>,
    prune_tombstone_at_unix_seconds: Option<i64>,
    pruned_at_unix_seconds: Option<i64>,
    replacement_not_before_unix_seconds: Option<i64>,
    last_error: Option<String>,
}

impl YoutubeReceipt {
    fn new(session_id: &str, config: &DeckYoutubeConfig) -> Self {
        Self {
            format_version: YOUTUBE_RECEIPT_VERSION,
            publisher_id: config.publisher_id.clone(),
            expected_channel_id: config.expected_channel_id.clone(),
            requested_privacy_status: config.privacy_status.as_api_str().into(),
            session_id: session_id.into(),
            video_id: None,
            upload_started_at_unix_seconds: None,
            upload_completed_at_unix_seconds: None,
            processing_state: YoutubeProcessingState::AwaitingUpload,
            confirmed_privacy_at_unix_seconds: None,
            caption: CaptionState::default(),
            prune_after_unix_seconds: None,
            prune_tombstone_at_unix_seconds: None,
            pruned_at_unix_seconds: None,
            replacement_not_before_unix_seconds: None,
            last_error: None,
        }
    }

    fn validate_for(&self, session_id: &str, config: &DeckYoutubeConfig) -> Result<()> {
        let legacy_matches = self.format_version == 1 && config.publisher_id == "legacy";
        let current_matches = self.format_version == YOUTUBE_RECEIPT_VERSION
            && self.publisher_id == config.publisher_id
            && self.expected_channel_id == config.expected_channel_id;
        if (!legacy_matches && !current_matches) || self.session_id != session_id {
            bail!("YouTube receipt does not match the archive session");
        }
        Ok(())
    }
}

fn legacy_publisher_id() -> String {
    "legacy".into()
}

fn default_unlisted_privacy() -> String {
    "unlisted".into()
}

#[derive(Deserialize, Serialize)]
struct OauthToken {
    format_version: u32,
    access_token: String,
    refresh_token: String,
    token_type: String,
    scope: String,
    expires_at_unix_seconds: i64,
}

impl OauthToken {
    fn usable_at(&self, now: i64) -> bool {
        self.format_version == OAUTH_TOKEN_VERSION
            && self
                .scope
                .split_ascii_whitespace()
                .any(|scope| scope == OAUTH_SCOPE)
            && self.expires_at_unix_seconds > now.saturating_add(60)
            && !self.access_token.is_empty()
            && !self.refresh_token.is_empty()
    }
}

#[derive(Deserialize)]
struct OauthClientFile {
    installed: OauthClient,
}

#[derive(Deserialize)]
struct OauthClient {
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
    #[serde(default)]
    scope: String,
    #[serde(default = "default_bearer")]
    token_type: String,
}

fn default_bearer() -> String {
    "Bearer".into()
}

#[derive(Deserialize, Serialize)]
struct ResumableUploadState {
    format_version: u32,
    session_id: String,
    video_size_bytes: u64,
    video_modified_unix_nanos: u64,
    upload_url: String,
    next_byte: u64,
    started_at_unix_seconds: i64,
}

#[derive(Deserialize, Serialize)]
struct RolloutPolicy {
    format_version: u32,
    #[serde(default = "legacy_publisher_id")]
    publisher_id: String,
    #[serde(default)]
    game_ids: Vec<String>,
    upload_not_before_unix_millis: i64,
    expected_channel_id: String,
}

#[derive(Default, Deserialize, Serialize)]
struct QuotaLedger {
    format_version: u32,
    pacific_date: String,
    video_inserts: u32,
    ordinary_units: u32,
}

enum QuotaReservation {
    VideoInsert,
    OrdinaryUnits(u32),
}

impl ResumableUploadState {
    fn matches(&self, candidate: &ArchiveCandidate) -> Result<bool> {
        let metadata = fs::metadata(&candidate.video_path)?;
        Ok(self.format_version == RESUMABLE_STATE_VERSION
            && self.session_id == candidate.session.id
            && self.video_size_bytes == metadata.len()
            && self.video_modified_unix_nanos == modified_unix_nanos(&metadata))
    }
}

struct HttpRequest {
    method: &'static str,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn new(method: &'static str, url: String) -> Self {
        Self {
            method,
            url,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

trait HttpTransport: Send + Sync {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse>;
}

struct UreqTransport;

impl HttpTransport for UreqTransport {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let mut builder = ureq::request(request.method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.set(name, value);
        }
        let result = if request.body.is_empty() && matches!(request.method, "GET" | "DELETE") {
            builder.call()
        } else {
            builder.send_bytes(&request.body)
        };
        let response = match result {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(_)) => {
                bail!("YouTube request failed before receiving a response")
            }
        };
        let status = response.status();
        let mut headers = BTreeMap::new();
        for name in ["location", "range", "retry-after"] {
            if let Some(value) = response.header(name) {
                headers.insert(name.into(), value.into());
            }
        }
        let mut body = Vec::new();
        response
            .into_reader()
            .take(MAX_API_RESPONSE_BYTES)
            .read_to_end(&mut body)
            .context("failed to read YouTube response")?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

struct YoutubeApi<'a, T: HttpTransport> {
    transport: &'a T,
    access_token: &'a str,
}

impl<'a, T: HttpTransport> YoutubeApi<'a, T> {
    fn authorized(&self, method: &'static str, url: String) -> HttpRequest {
        HttpRequest::new(method, url)
            .header("Authorization", format!("Bearer {}", self.access_token))
    }

    fn send_json<R: DeserializeOwned>(&self, request: HttpRequest, operation: &str) -> Result<R> {
        let response = self.transport.send(request)?;
        if !(200..300).contains(&response.status) {
            bail!("{operation} failed with HTTP {}", response.status);
        }
        serde_json::from_slice(&response.body)
            .with_context(|| format!("{operation} returned invalid JSON"))
    }
}

#[derive(Debug, Clone)]
struct ChannelContext {
    uploads_playlist_id: String,
}

#[derive(Deserialize)]
struct ChannelListResponse {
    #[serde(default)]
    items: Vec<ChannelResource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelResource {
    id: String,
    content_details: ChannelContentDetails,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelContentDetails {
    related_playlists: RelatedPlaylists,
}

#[derive(Deserialize)]
struct RelatedPlaylists {
    uploads: String,
}

fn fetch_expected_channel<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    expected_channel_id: &str,
) -> Result<ChannelContext> {
    let url = api_url(
        YOUTUBE_API_BASE,
        "channels",
        &[("part", "id,contentDetails"), ("mine", "true")],
    )?;
    let response: ChannelListResponse =
        api.send_json(api.authorized("GET", url), "YouTube channel lookup")?;
    let channel = response
        .items
        .into_iter()
        .find(|channel| channel.id == expected_channel_id)
        .ok_or_else(|| {
            anyhow!("authenticated YouTube channel does not match expected channel ID")
        })?;
    Ok(ChannelContext {
        uploads_playlist_id: channel.content_details.related_playlists.uploads,
    })
}

/// Authorize the configured desktop OAuth client through a PKCE loopback flow.
pub fn youtube_auth(config: &RecorderConfig) -> Result<YoutubeStatusReport> {
    let youtube = configured_youtube(config)?;
    ensure_private_state(youtube)?;
    ensure_rollout_policy(youtube, false)?;
    let client = load_oauth_client(&youtube.oauth_client_path)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to bind the YouTube OAuth loopback listener")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_base64url(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_base64url(24)?;
    let mut authorization_url = Url::parse(OAUTH_AUTH_URL)?;
    authorization_url
        .query_pairs_mut()
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    let opened = Command::new("xdg-open")
        .arg(authorization_url.as_str())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to open the system browser for YouTube authorization")?;
    if !opened.success() {
        bail!("the system browser could not be opened for YouTube authorization");
    }
    let code = receive_oauth_redirect(&listener, &state)?;
    let transport = UreqTransport;
    let token = exchange_authorization_code(
        &transport,
        &client,
        &code,
        &verifier,
        &redirect_uri,
        unix_now(),
    )?;
    let api = YoutubeApi {
        transport: &transport,
        access_token: &token.access_token,
    };
    fetch_expected_channel(&api, &youtube.expected_channel_id)?;
    ensure_rollout_policy(youtube, true)?;
    write_private_json_atomic(&youtube.oauth_token_path, &token)?;
    youtube_status(config)
}

/// Authorize one explicitly selected publisher profile.
pub fn youtube_auth_for(
    config: &RecorderConfig,
    publisher_id: &str,
) -> Result<YoutubeStatusReport> {
    let scoped = scoped_publisher_config(config, publisher_id, false)?;
    youtube_auth(&scoped)
}

/// Report every configured publisher, or one explicitly selected profile.
pub fn youtube_status_all(
    config: &RecorderConfig,
    publisher_id: Option<&str>,
) -> Result<Vec<YoutubeStatusReport>> {
    selected_publishers(config, publisher_id)?
        .into_iter()
        .map(|publisher| youtube_status(&scoped_config(config, publisher, false)))
        .collect()
}

/// Report authentication/channel state without exposing credential material.
pub fn youtube_status(config: &RecorderConfig) -> Result<YoutubeStatusReport> {
    let Some(youtube) = config.youtube.as_ref() else {
        return Ok(YoutubeStatusReport::default());
    };
    let mut report = YoutubeStatusReport {
        publisher_id: Some(youtube.publisher_id.clone()),
        configured: true,
        enabled: youtube.enabled,
        expected_channel_id: Some(youtube.expected_channel_id.clone()),
        state_dir: Some(youtube.state_dir()),
        ..Default::default()
    };
    scan_receipt_status(config, &mut report)?;
    if let Err(error) = ensure_rollout_policy(youtube, false) {
        report.error = Some(redact_sensitive(&error.to_string()));
        return Ok(report);
    }
    if !youtube.oauth_token_path.is_file() {
        return Ok(report);
    }
    ensure_private_state(youtube)?;
    let transport = UreqTransport;
    match access_token(&transport, youtube, unix_now()) {
        Ok(token) => {
            report.authenticated = true;
            report.token_expires_at_unix_seconds = Some(token.expires_at_unix_seconds);
            let api = YoutubeApi {
                transport: &transport,
                access_token: &token.access_token,
            };
            match fetch_expected_channel(&api, &youtube.expected_channel_id) {
                Ok(_) => report.channel_matches = Some(true),
                Err(error) => {
                    report.channel_matches = Some(false);
                    report.error = Some(redact_sensitive(&error.to_string()));
                }
            }
        }
        Err(error) => report.error = Some(redact_sensitive(&error.to_string())),
    }
    Ok(report)
}

fn scan_receipt_status(config: &RecorderConfig, report: &mut YoutubeStatusReport) -> Result<()> {
    let youtube = configured_youtube(config)?;
    for rule in &config.recording_archives {
        if !rule.destination_dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&rule.destination_dir)?.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.ends_with(".youtube.json"))
            {
                continue;
            }
            let Ok(receipt) = read_json::<YoutubeReceipt>(&path) else {
                continue;
            };
            if receipt.format_version >= 2
                && (receipt.publisher_id != youtube.publisher_id
                    || receipt.expected_channel_id != youtube.expected_channel_id)
            {
                continue;
            }
            report.upload_receipts += 1;
            if receipt.video_id.is_some() {
                report.published_videos += 1;
            } else {
                report.pending_uploads += 1;
            }
            if receipt.pruned_at_unix_seconds.is_some() {
                report.pruned_videos += 1;
            }
        }
    }
    Ok(())
}

fn configured_youtube(config: &RecorderConfig) -> Result<&DeckYoutubeConfig> {
    config
        .youtube
        .as_ref()
        .context("YouTube publishing is not configured")
}

fn selected_publishers<'a>(
    config: &'a RecorderConfig,
    publisher_id: Option<&str>,
) -> Result<Vec<&'a DeckYoutubeConfig>> {
    let publishers = config.configured_youtube_publishers();
    if publishers.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(publisher_id) = publisher_id {
        let publisher = publishers
            .into_iter()
            .find(|publisher| publisher.publisher_id == publisher_id)
            .with_context(|| format!("YouTube publisher is not configured: {publisher_id}"))?;
        return Ok(vec![publisher]);
    }
    Ok(publishers)
}

fn scoped_publisher_config(
    config: &RecorderConfig,
    publisher_id: &str,
    private_canary: bool,
) -> Result<RecorderConfig> {
    let publisher = selected_publishers(config, Some(publisher_id))?
        .into_iter()
        .next()
        .context("YouTube publishing is not configured")?;
    if private_canary && publisher.enabled {
        bail!("a Private canary requires the selected publisher to remain disabled");
    }
    Ok(scoped_config(config, publisher, private_canary))
}

fn scoped_config(
    config: &RecorderConfig,
    publisher: &DeckYoutubeConfig,
    private_canary: bool,
) -> RecorderConfig {
    let mut scoped = config.clone();
    let mut publisher = publisher.clone();
    if private_canary {
        publisher.privacy_status = DeckYoutubePrivacy::Private;
    }
    if !publisher.game_ids.is_empty() {
        scoped
            .recording_archives
            .retain(|rule| publisher.game_ids.contains(&rule.game_id));
    }
    scoped.youtube = Some(publisher);
    scoped.youtube_publishers.clear();
    scoped
}

fn session_after_cutoff(session_start: f64, cutoff: f64) -> bool {
    session_start.is_finite() && cutoff.is_finite() && session_start > cutoff
}

fn claim_unpublished_candidate(automatic_enabled: bool, manual_claimed: &mut bool) -> bool {
    if automatic_enabled {
        return true;
    }
    if *manual_claimed {
        return false;
    }
    *manual_claimed = true;
    true
}

fn load_oauth_client(path: &Path) -> Result<OauthClient> {
    harden_private_file(path)?;
    let file: OauthClientFile = read_json(path)
        .with_context(|| format!("failed to read OAuth client file {}", path.display()))?;
    if file.installed.client_id.trim().is_empty() || file.installed.client_secret.trim().is_empty()
    {
        bail!("OAuth client file has empty desktop client credentials");
    }
    Ok(file.installed)
}

fn harden_private_file(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("private credential file is unavailable: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("private credential path is not a regular file");
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn exchange_authorization_code<T: HttpTransport>(
    transport: &T,
    client: &OauthClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    now: i64,
) -> Result<OauthToken> {
    let body = form_body(&[
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.as_str()),
        ("code", code),
        ("code_verifier", verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ]);
    let response = transport.send(
        HttpRequest::new("POST", OAUTH_TOKEN_URL.into())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.into_bytes()),
    )?;
    if response.status != 200 {
        bail!(
            "OAuth authorization-code exchange failed with HTTP {}",
            response.status
        );
    }
    let token: TokenResponse =
        serde_json::from_slice(&response.body).context("OAuth token response was invalid")?;
    let refresh_token = token
        .refresh_token
        .filter(|value| !value.is_empty())
        .context("OAuth response did not include a refresh token")?;
    let token = OauthToken {
        format_version: OAUTH_TOKEN_VERSION,
        access_token: token.access_token,
        refresh_token,
        token_type: token.token_type,
        scope: if token.scope.is_empty() {
            OAUTH_SCOPE.into()
        } else {
            token.scope
        },
        expires_at_unix_seconds: now.saturating_add(token.expires_in.max(0)),
    };
    if !token.usable_at(now) {
        bail!("OAuth response is missing a usable token or the required YouTube scope");
    }
    Ok(token)
}

fn access_token<T: HttpTransport>(
    transport: &T,
    config: &DeckYoutubeConfig,
    now: i64,
) -> Result<OauthToken> {
    harden_private_file(&config.oauth_token_path)?;
    let mut token: OauthToken = read_json(&config.oauth_token_path)
        .context("YouTube OAuth token is unavailable; run youtube-auth")?;
    if token.usable_at(now) {
        return Ok(token);
    }
    let client = load_oauth_client(&config.oauth_client_path)?;
    let body = form_body(&[
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.as_str()),
        ("refresh_token", token.refresh_token.as_str()),
        ("grant_type", "refresh_token"),
    ]);
    let response = transport.send(
        HttpRequest::new("POST", OAUTH_TOKEN_URL.into())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.into_bytes()),
    )?;
    if response.status != 200 {
        bail!("OAuth token refresh failed with HTTP {}", response.status);
    }
    let refreshed: TokenResponse =
        serde_json::from_slice(&response.body).context("OAuth refresh response was invalid")?;
    token.access_token = refreshed.access_token;
    token.expires_at_unix_seconds = now.saturating_add(refreshed.expires_in.max(0));
    token.token_type = refreshed.token_type;
    if !refreshed.scope.is_empty() {
        token.scope = refreshed.scope;
    }
    if let Some(refresh_token) = refreshed.refresh_token.filter(|value| !value.is_empty()) {
        token.refresh_token = refresh_token;
    }
    if !token.usable_at(now) {
        bail!("refreshed OAuth token is missing the required YouTube scope");
    }
    write_private_json_atomic(&config.oauth_token_path, &token)?;
    Ok(token)
}

fn receive_oauth_redirect(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let deadline = Instant::now() + OAUTH_TIMEOUT;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let result = parse_oauth_request(&stream, expected_state);
                write_oauth_browser_response(&mut stream, result.is_ok())?;
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error).context("OAuth loopback listener failed"),
        }
    }
    bail!("YouTube authorization timed out")
}

fn parse_oauth_request(stream: &TcpStream, expected_state: &str) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let reader = BufReader::new(stream);
    let mut limited = reader.take(8 * 1024);
    let mut request_line = String::new();
    limited.read_line(&mut request_line)?;
    let target = request_line
        .split_ascii_whitespace()
        .nth(1)
        .context("invalid OAuth callback request")?;
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let values = callback.query_pairs().collect::<BTreeMap<_, _>>();
    if values.get("state").map(|value| value.as_ref()) != Some(expected_state) {
        bail!("OAuth callback state did not match");
    }
    if values.contains_key("error") {
        bail!("YouTube authorization was declined or failed");
    }
    values
        .get("code")
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .context("OAuth callback contained no authorization code")
}

fn write_oauth_browser_response(stream: &mut TcpStream, success: bool) -> Result<()> {
    let message = if success {
        "YouTube authorization completed. You can close this window."
    } else {
        "YouTube authorization failed. Return to RetroFeel for details."
    };
    let body =
        format!("<!doctype html><meta charset=utf-8><title>RetroFeel</title><p>{message}</p>");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

fn random_base64url(bytes: usize) -> Result<String> {
    let mut value = vec![0_u8; bytes];
    getrandom::getrandom(&mut value).map_err(|_| anyhow!("secure random generation failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn form_body(values: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in values {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

fn api_url(base: &str, path: &str, query: &[(&str, &str)]) -> Result<String> {
    let mut url = Url::parse(&format!("{base}/{path}"))?;
    url.query_pairs_mut().extend_pairs(query.iter().copied());
    Ok(url.into())
}

fn ensure_private_state(config: &DeckYoutubeConfig) -> Result<()> {
    ensure_private_dir(&config.state_dir())?;
    ensure_private_dir(&config.state_dir().join("uploads"))?;
    Ok(())
}

fn ensure_rollout_policy(config: &DeckYoutubeConfig, create: bool) -> Result<()> {
    let configured = DateTime::parse_from_rfc3339(&config.upload_not_before)
        .context("invalid YouTube upload cutoff")?
        .timestamp_millis();
    let path = config.state_dir().join("rollout-policy.json");
    if path.is_file() {
        harden_private_file(&path)?;
        let persisted: RolloutPolicy = read_json(&path)?;
        let legacy_matches = persisted.format_version == 1
            && config.publisher_id == "legacy"
            && config.game_ids.is_empty();
        let current_matches = persisted.format_version == ROLLOUT_POLICY_VERSION
            && persisted.publisher_id == config.publisher_id
            && persisted.game_ids == config.game_ids;
        if (!legacy_matches && !current_matches)
            || persisted.upload_not_before_unix_millis != configured
            || persisted.expected_channel_id != config.expected_channel_id
        {
            bail!("YouTube cutoff or channel differs from the immutable rollout policy");
        }
    } else if create {
        write_private_json_atomic(
            &path,
            &RolloutPolicy {
                format_version: ROLLOUT_POLICY_VERSION,
                publisher_id: config.publisher_id.clone(),
                game_ids: config.game_ids.clone(),
                upload_not_before_unix_millis: configured,
                expected_channel_id: config.expected_channel_id.clone(),
            },
        )?;
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| {
        format!(
            "failed to create private state directory {}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_private_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("private state path has no parent")?;
    ensure_private_dir(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("private state path has no UTF-8 filename")?;
    let partial = path.with_file_name(format!(".{name}.partial"));
    if partial.is_file() {
        fs::remove_file(&partial)?;
    } else if partial.exists() {
        bail!("private partial state path is not a file");
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&partial)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o600))?;
    fs::rename(&partial, path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn reserve_daily_quota(
    config: &DeckYoutubeConfig,
    reservation: QuotaReservation,
    now: i64,
) -> Result<bool> {
    let state_dir = config
        .oauth_client_path
        .parent()
        .context("YouTube OAuth client path has no parent")?;
    ensure_private_dir(state_dir)?;
    let _lock = QuotaLedgerLock::acquire(state_dir)?;
    let path = state_dir.join("quota-ledger.json");
    let timezone: Tz = "America/Vancouver"
        .parse()
        .expect("fixed YouTube quota timezone is valid");
    let date = Utc
        .timestamp_opt(now, 0)
        .single()
        .context("YouTube quota timestamp is out of range")?
        .with_timezone(&timezone)
        .format("%Y-%m-%d")
        .to_string();
    let mut ledger = if path.is_file() {
        harden_private_file(&path)?;
        read_json::<QuotaLedger>(&path)?
    } else {
        QuotaLedger::default()
    };
    if ledger.format_version != QUOTA_LEDGER_VERSION || ledger.pacific_date != date {
        ledger = QuotaLedger {
            format_version: QUOTA_LEDGER_VERSION,
            pacific_date: date,
            ..Default::default()
        };
    }
    let available = match reservation {
        QuotaReservation::VideoInsert if ledger.video_inserts < DAILY_VIDEO_INSERT_LIMIT => {
            ledger.video_inserts += 1;
            true
        }
        QuotaReservation::OrdinaryUnits(units)
            if ledger.ordinary_units.saturating_add(units) <= DAILY_ORDINARY_QUOTA_LIMIT =>
        {
            ledger.ordinary_units += units;
            true
        }
        QuotaReservation::VideoInsert | QuotaReservation::OrdinaryUnits(_) => false,
    };
    if available {
        write_private_json_atomic(&path, &ledger)?;
    }
    Ok(available)
}

struct QuotaLedgerLock {
    file: File,
}

impl QuotaLedgerLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(".quota.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        #[cfg(unix)]
        {
            // SAFETY: the descriptor remains owned by this guard until Drop.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to acquire YouTube quota lock");
            }
        }
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for QuotaLedgerLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid throughout Drop.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_reader(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", path.display()))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn modified_unix_nanos(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn redact_sensitive(message: &str) -> String {
    let mut redacted = message.to_string();
    for marker in [
        "access_token",
        "refresh_token",
        "client_secret",
        "upload_id",
        "Authorization: Bearer",
        "code=",
    ] {
        if let Some(index) = redacted.find(marker) {
            redacted.truncate(index);
            redacted.push_str("[redacted]");
        }
    }
    redacted
}

struct YoutubeSyncLock {
    file: File,
}

impl YoutubeSyncLock {
    fn try_acquire(state_dir: &Path) -> Result<Option<Self>> {
        ensure_private_dir(state_dir)?;
        let path = state_dir.join(".sync.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Some(Self { file }));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            Err(error).context("failed to acquire YouTube sync lock")
        }
        #[cfg(not(unix))]
        Ok(Some(Self { file }))
    }
}

#[cfg(unix)]
impl Drop for YoutubeSyncLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

struct ArchiveCandidate {
    rule: DeckArchiveRule,
    session: SessionSummary,
    transcription_state: TranscriptionJobState,
    video_path: PathBuf,
    archive_receipt_path: PathBuf,
    transcript_path: PathBuf,
    youtube_receipt_path: PathBuf,
}

/// Reconcile verified future archives with YouTube and apply safe retention.
pub fn sync_youtube(config: &RecorderConfig, dry_run: bool) -> Result<YoutubeSyncReport> {
    sync_youtube_with(config, dry_run, &UreqTransport, unix_now())
}

/// Reconcile one selected publisher or every configured publisher.
pub fn sync_youtube_publishers(
    config: &RecorderConfig,
    publisher_id: Option<&str>,
    dry_run: bool,
    private_canary: bool,
) -> Result<Vec<YoutubeSyncReport>> {
    if private_canary && publisher_id.is_none() {
        bail!("a Private canary requires --publisher");
    }
    let publishers = selected_publishers(config, publisher_id)?;
    let mut reports = Vec::with_capacity(publishers.len());
    for publisher in publishers {
        let scoped = if private_canary {
            scoped_publisher_config(config, &publisher.publisher_id, true)?
        } else {
            scoped_config(config, publisher, false)
        };
        reports.push(sync_youtube(&scoped, dry_run)?);
    }
    Ok(reports)
}

fn sync_youtube_with<T: HttpTransport>(
    config: &RecorderConfig,
    dry_run: bool,
    transport: &T,
    now: i64,
) -> Result<YoutubeSyncReport> {
    let Some(youtube) = config.youtube.as_ref() else {
        return Ok(YoutubeSyncReport {
            dry_run,
            ..Default::default()
        });
    };
    let mut report = YoutubeSyncReport {
        publisher_id: Some(youtube.publisher_id.clone()),
        configured: true,
        enabled: youtube.enabled,
        dry_run,
        ..Default::default()
    };
    let cutoff = DateTime::parse_from_rfc3339(&youtube.upload_not_before)
        .context("invalid YouTube upload cutoff")?
        .timestamp_millis() as f64
        / 1_000.0;
    let candidates = scan_archive_candidates(config, &mut report)?;
    ensure_rollout_policy(youtube, false)?;

    if dry_run {
        for candidate in &candidates {
            let receipt = load_youtube_receipt(candidate, youtube)?;
            if receipt.video_id.is_some() {
                continue;
            }
            if !session_after_cutoff(candidate.session.start_timestamp, cutoff) {
                report.cutoff_ineligible += 1;
                continue;
            }
            if verified_candidate(candidate)?.is_some() {
                report.would_upload += 1;
            } else {
                report.unverified_archives += 1;
            }
        }
        return Ok(report);
    }
    if youtube.idle_only && !deck_is_idle()? {
        report.deferred_for_activity = true;
        return Ok(report);
    }
    ensure_private_state(youtube)?;
    let supervisor_state_dir = youtube
        .oauth_client_path
        .parent()
        .context("YouTube OAuth client path has no parent")?;
    ensure_private_dir(supervisor_state_dir)?;
    let Some(_lock) = YoutubeSyncLock::try_acquire(supervisor_state_dir)? else {
        report.already_running = true;
        return Ok(report);
    };
    let token = access_token(transport, youtube, now)?;
    let api = YoutubeApi {
        transport,
        access_token: &token.access_token,
    };
    let channel = fetch_expected_channel(&api, &youtube.expected_channel_id)?;
    ensure_rollout_policy(youtube, true)?;
    let mut recent_uploads = list_recent_session_uploads(&api, &channel)?;
    let mut manual_unpublished_candidate_claimed = false;

    for candidate in candidates {
        let mut receipt = match load_youtube_receipt(&candidate, youtube) {
            Ok(receipt) => receipt,
            Err(error) => {
                report.failures.push(format!(
                    "{}: {}",
                    candidate.session.id,
                    redact_sensitive(&error.to_string())
                ));
                continue;
            }
        };
        if receipt.video_id.is_none() {
            if !session_after_cutoff(candidate.session.start_timestamp, cutoff) {
                report.cutoff_ineligible += 1;
                continue;
            }
            if verified_candidate(&candidate)?.is_none() {
                report.unverified_archives += 1;
                continue;
            }
            // `enabled` governs the automatic worker. While it is disabled,
            // one explicit sync may advance at most one unpublished session,
            // which keeps smoke/canary rollout bounded.
            if !claim_unpublished_candidate(
                youtube.enabled,
                &mut manual_unpublished_candidate_claimed,
            ) {
                continue;
            }
        }
        if let Err(error) = sync_candidate(
            &api,
            youtube,
            &candidate,
            &mut receipt,
            &mut recent_uploads,
            &mut report,
            now,
        ) {
            let message = redact_sensitive(&error.to_string());
            receipt.last_error = Some(message.clone());
            let _ = write_youtube_receipt(&candidate.youtube_receipt_path, &receipt);
            report
                .failures
                .push(format!("{}: {message}", candidate.session.id));
        }
    }
    Ok(report)
}

/// Run publishing on its own thread so network activity never reaches capture,
/// archive creation, or transcription execution.
pub(crate) fn spawn_youtube_worker(config: RecorderConfig) {
    if !config
        .configured_youtube_publishers()
        .into_iter()
        .any(|youtube| youtube.enabled)
    {
        return;
    }
    let interval = Duration::from_secs(config.archive_sync_interval_seconds.max(1));
    let spawn = thread::Builder::new()
        .name("retrofeel-youtube-publisher".into())
        .spawn(move || loop {
            let enabled = config
                .configured_youtube_publishers()
                .into_iter()
                .filter(|publisher| publisher.enabled)
                .cloned()
                .collect::<Vec<_>>();
            for publisher in enabled {
                let scoped = scoped_config(&config, &publisher, false);
                match sync_youtube(&scoped, false) {
                    Ok(report) => {
                        if report.uploaded_videos > 0
                            || report.captions_uploaded > 0
                            || report.captions_updated > 0
                            || report.pruned_videos > 0
                        {
                            log::info!(
                                "YouTube publisher {} uploaded {} video(s), inserted {} caption(s), updated {} caption(s), and pruned {} MP4(s)",
                                publisher.publisher_id,
                                report.uploaded_videos,
                                report.captions_uploaded,
                                report.captions_updated,
                                report.pruned_videos
                            );
                        }
                        for failure in report.failures {
                            log::warn!(
                                "YouTube publisher {} deferred: {}",
                                publisher.publisher_id,
                                redact_sensitive(&failure)
                            );
                        }
                    }
                    Err(error) => log::warn!(
                        "YouTube publisher {} reconciliation failed: {}",
                        publisher.publisher_id,
                        redact_sensitive(&error.to_string())
                    ),
                }
            }
            thread::sleep(interval);
        });
    if let Err(error) = spawn {
        log::error!("could not start YouTube publishing worker: {error}");
    }
}

fn scan_archive_candidates(
    config: &RecorderConfig,
    report: &mut YoutubeSyncReport,
) -> Result<Vec<ArchiveCandidate>> {
    let youtube = configured_youtube(config)?;
    let mut by_session = BTreeMap::new();
    for rule in &config.recording_archives {
        if rule.format != DeckArchiveFormat::Mp4 || !rule.destination_dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&rule.destination_dir)?.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".manifest.json") else {
                continue;
            };
            let manifest: SessionManifest = match read_json(&entry.path()) {
                Ok(manifest) => manifest,
                Err(error) => {
                    report
                        .failures
                        .push(format!("{stem}: archived manifest is invalid: {error:#}"));
                    continue;
                }
            };
            let Some(external) = manifest.external_capture.as_ref() else {
                continue;
            };
            if !youtube.game_ids.is_empty() && !youtube.game_ids.contains(&external.game_id) {
                continue;
            }
            if external.status != ExternalCaptureStatus::Complete {
                continue;
            }
            let expected_session_suffix = format!("__{}", external.recording_id);
            if !stem.ends_with(&expected_session_suffix)
                || stem.len() == expected_session_suffix.len()
            {
                report.failures.push(format!(
                    "{stem}: archive filename does not match its manifest session ID"
                ));
                continue;
            }
            let video_path = rule.destination_dir.join(format!("{stem}.mp4"));
            let session = SessionSummary {
                id: external.recording_id.clone(),
                game_id: external.game_id.clone(),
                start_timestamp: manifest.timing.start_timestamp,
                frame_count: manifest.frame_count,
                status: external.status,
                directory: rule.destination_dir.clone(),
                video_source: Some(video_path.clone()),
            };
            if by_session.contains_key(&session.id) {
                continue;
            }
            let transcription_state = manifest
                .external_capture
                .map(|external| external.audio_transcription_status)
                .unwrap_or_default();
            by_session.insert(
                session.id.clone(),
                ArchiveCandidate {
                    rule: rule.clone(),
                    session,
                    transcription_state,
                    archive_receipt_path: rule.destination_dir.join(format!("{stem}.archive.json")),
                    transcript_path: rule.destination_dir.join(format!("{stem}.transcript.srt")),
                    youtube_receipt_path: rule.destination_dir.join(format!("{stem}.youtube.json")),
                    video_path,
                },
            );
        }
    }
    report.scanned_sessions = by_session.len();
    let mut candidates = by_session.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .session
            .start_timestamp
            .partial_cmp(&left.session.start_timestamp)
            .unwrap_or(Ordering::Equal)
            .then_with(|| right.session.id.cmp(&left.session.id))
    });
    Ok(candidates)
}

fn verified_candidate(candidate: &ArchiveCandidate) -> Result<Option<()>> {
    Ok(current_archive_verification(
        &candidate.video_path,
        &candidate.archive_receipt_path,
        &candidate.session,
        DeckArchiveFormat::Mp4,
    )?
    .map(|_| ()))
}

fn load_youtube_receipt(
    candidate: &ArchiveCandidate,
    config: &DeckYoutubeConfig,
) -> Result<YoutubeReceipt> {
    if !candidate.youtube_receipt_path.is_file() {
        return Ok(YoutubeReceipt::new(&candidate.session.id, config));
    }
    let mut receipt: YoutubeReceipt = read_json(&candidate.youtube_receipt_path)?;
    receipt.validate_for(&candidate.session.id, config)?;
    if receipt.format_version == 1 {
        receipt.format_version = YOUTUBE_RECEIPT_VERSION;
        receipt.publisher_id = config.publisher_id.clone();
        receipt.expected_channel_id = config.expected_channel_id.clone();
        receipt.requested_privacy_status = config.privacy_status.as_api_str().into();
    }
    Ok(receipt)
}

fn write_youtube_receipt(path: &Path, receipt: &YoutubeReceipt) -> Result<()> {
    let parent = path
        .parent()
        .context("YouTube receipt path has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("YouTube receipt path has no UTF-8 filename")?;
    let partial = path.with_file_name(format!(".{name}.partial"));
    if partial.is_file() {
        fs::remove_file(&partial)?;
    } else if partial.exists() {
        bail!("YouTube receipt partial path is not a file");
    }
    let bytes = serde_json::to_vec_pretty(receipt)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o644);
    let mut file = options.open(&partial)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&partial, path)?;
    Ok(())
}

#[derive(Deserialize)]
struct PlaylistItemsResponse {
    #[serde(default)]
    items: Vec<PlaylistItem>,
}

#[derive(Deserialize)]
struct PlaylistItem {
    snippet: PlaylistSnippet,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistSnippet {
    #[serde(default)]
    description: String,
    resource_id: PlaylistResourceId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistResourceId {
    video_id: String,
}

fn list_recent_session_uploads<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    channel: &ChannelContext,
) -> Result<BTreeMap<String, String>> {
    let url = api_url(
        YOUTUBE_API_BASE,
        "playlistItems",
        &[
            ("part", "snippet"),
            ("playlistId", &channel.uploads_playlist_id),
            ("maxResults", "50"),
        ],
    )?;
    let response: PlaylistItemsResponse =
        api.send_json(api.authorized("GET", url), "recent upload reconciliation")?;
    let mut sessions = BTreeMap::new();
    for item in response.items {
        if let Some(session_id) = session_id_from_description(&item.snippet.description) {
            sessions
                .entry(session_id.into())
                .or_insert(item.snippet.resource_id.video_id);
        }
    }
    Ok(sessions)
}

fn session_id_from_description(description: &str) -> Option<&str> {
    description.lines().find_map(|line| {
        line.strip_prefix("RetroFeel-Session-ID: ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn sync_candidate<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    receipt: &mut YoutubeReceipt,
    recent_uploads: &mut BTreeMap<String, String>,
    report: &mut YoutubeSyncReport,
    now: i64,
) -> Result<()> {
    if receipt.pruned_at_unix_seconds.is_some() {
        if receipt.video_id.is_some() {
            reconcile_caption(api, config, candidate, receipt, report, now)?;
        }
        return Ok(());
    }
    if receipt.video_id.is_none() {
        if let Some(video_id) = recent_uploads.get(&candidate.session.id) {
            receipt.video_id = Some(video_id.clone());
            receipt.upload_completed_at_unix_seconds.get_or_insert(now);
            receipt.processing_state = YoutubeProcessingState::Processing;
            receipt.last_error = None;
            write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
            report.reconciled_videos += 1;
        } else if receipt
            .replacement_not_before_unix_seconds
            .is_some_and(|deadline| now < deadline)
        {
            return Ok(());
        } else {
            upload_candidate(api, config, candidate, receipt, report, now)?;
            if let Some(video_id) = &receipt.video_id {
                recent_uploads.insert(candidate.session.id.clone(), video_id.clone());
            } else {
                return Ok(());
            }
        }
    }

    if receipt.processing_state != YoutubeProcessingState::Succeeded
        || receipt.confirmed_privacy_at_unix_seconds.is_none()
    {
        reconcile_processing(api, config, candidate, receipt, report, now)?;
    }
    reconcile_caption(api, config, candidate, receipt, report, now)?;
    apply_retention(config, candidate, receipt, report, now)?;
    write_youtube_receipt(&candidate.youtube_receipt_path, receipt)
}

fn upload_candidate<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    receipt: &mut YoutubeReceipt,
    report: &mut YoutubeSyncReport,
    now: i64,
) -> Result<()> {
    let state_path = upload_state_path(config, &candidate.session.id);
    let mut state = if state_path.is_file() {
        harden_private_file(&state_path)?;
        let state: ResumableUploadState =
            read_json(&state_path).context("persisted resumable upload state is invalid")?;
        if !state.matches(candidate)? {
            fs::remove_file(&state_path)?;
            receipt.replacement_not_before_unix_seconds =
                Some(now.saturating_add(REPLACEMENT_GRACE_SECONDS));
            receipt.last_error = Some(
                "resumable state no longer matches the verified MP4; replacement delayed for duplicate reconciliation"
                    .into(),
            );
            write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
            return Ok(());
        }
        report.resumed_uploads += 1;
        state
    } else {
        if !reserve_daily_quota(config, QuotaReservation::VideoInsert, now)? {
            report.quota_deferred += 1;
            return Ok(());
        }
        receipt.requested_privacy_status = config.privacy_status.as_api_str().into();
        let state = initiate_resumable_upload(api, config, candidate, now)?;
        write_private_json_atomic(&state_path, &state)?;
        receipt.processing_state = YoutubeProcessingState::Uploading;
        receipt.upload_started_at_unix_seconds = Some(now);
        receipt.replacement_not_before_unix_seconds = None;
        receipt.last_error = None;
        write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
        report.initiated_uploads += 1;
        state
    };

    match run_resumable_upload(api, config, candidate, &state_path, &mut state)? {
        ResumableResult::Completed(video_id) => {
            // This receipt write is deliberately the first action after the
            // successful response. A crash after it cannot cause a replacement.
            receipt.video_id = Some(video_id);
            receipt.upload_completed_at_unix_seconds = Some(now);
            receipt.processing_state = YoutubeProcessingState::Processing;
            receipt.replacement_not_before_unix_seconds = None;
            receipt.last_error = None;
            write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
            if state_path.is_file() {
                fs::remove_file(&state_path)?;
            }
            report.uploaded_videos += 1;
        }
        ResumableResult::Expired => {
            if state_path.is_file() {
                fs::remove_file(&state_path)?;
            }
            receipt.processing_state = YoutubeProcessingState::AwaitingUpload;
            receipt.replacement_not_before_unix_seconds =
                Some(now.saturating_add(REPLACEMENT_GRACE_SECONDS));
            receipt.last_error = Some(
                "resumable upload expired; replacement delayed for duplicate reconciliation".into(),
            );
            write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
        }
        ResumableResult::PausedForActivity => {
            report.deferred_for_activity = true;
        }
    }
    Ok(())
}

fn initiate_resumable_upload<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    now: i64,
) -> Result<ResumableUploadState> {
    let metadata = fs::metadata(&candidate.video_path)?;
    let youtube_metadata = youtube_metadata(config, candidate)?;
    let url = api_url(
        YOUTUBE_UPLOAD_BASE,
        "videos",
        &[
            ("uploadType", "resumable"),
            ("part", "snippet,status"),
            ("notifySubscribers", "false"),
        ],
    )?;
    let body = serde_json::to_vec(&youtube_metadata)?;
    let response = api.transport.send(
        api.authorized("POST", url)
            .header("Content-Type", "application/json; charset=UTF-8")
            .header("X-Upload-Content-Length", metadata.len().to_string())
            .header("X-Upload-Content-Type", MP4_CONTENT_TYPE)
            .body(body),
    )?;
    if response.status != 200 {
        bail!(
            "resumable upload initiation failed with HTTP {}",
            response.status
        );
    }
    let upload_url = response
        .header("location")
        .filter(|value| !value.is_empty())
        .context("resumable upload initiation returned no session location")?
        .to_string();
    Ok(ResumableUploadState {
        format_version: RESUMABLE_STATE_VERSION,
        session_id: candidate.session.id.clone(),
        video_size_bytes: metadata.len(),
        video_modified_unix_nanos: modified_unix_nanos(&metadata),
        upload_url,
        next_byte: 0,
        started_at_unix_seconds: now,
    })
}

fn youtube_metadata(
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
) -> Result<serde_json::Value> {
    Ok(json!({
        "snippet": {
            "title": youtube_title(config, &candidate.rule.display_name, candidate.session.start_timestamp)?,
            "description": youtube_description(candidate)?,
            "categoryId": config.category.api_id(),
        },
        "status": {
            "privacyStatus": config.privacy_status.as_api_str(),
            "selfDeclaredMadeForKids": false,
        }
    }))
}

fn youtube_title(config: &DeckYoutubeConfig, display_name: &str, timestamp: f64) -> Result<String> {
    let timezone: Tz = config
        .timezone
        .parse()
        .context("invalid YouTube timezone")?;
    let utc = utc_from_unix_seconds(timestamp)?;
    Ok(format!(
        "{} — Development History — {} PT",
        display_name,
        utc.with_timezone(&timezone).format("%Y-%m-%d %H:%M:%S")
    ))
}

fn youtube_description(candidate: &ArchiveCandidate) -> Result<String> {
    let recorded = utc_from_unix_seconds(candidate.session.start_timestamp)?
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    Ok(format!(
        "Game: {}\nRecorded: {recorded}\nRetroFeel-Session-ID: {}\n\nCaptions, when available, were automatically transcribed from Steam's mixed game/system/microphone audio.",
        candidate.rule.display_name, candidate.session.id
    ))
}

fn utc_from_unix_seconds(timestamp: f64) -> Result<DateTime<Utc>> {
    if !timestamp.is_finite() || timestamp < 0.0 {
        bail!("recording start timestamp is invalid");
    }
    let seconds = timestamp.floor() as i64;
    let nanos = ((timestamp - seconds as f64) * 1_000_000_000.0).round() as u32;
    Utc.timestamp_opt(seconds, nanos)
        .single()
        .context("recording start timestamp is out of range")
}

fn upload_state_path(config: &DeckYoutubeConfig, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(session_id.as_bytes());
    config
        .state_dir()
        .join("uploads")
        .join(format!("{}.json", lowercase_hex(&digest)))
}

enum ResumableResult {
    Completed(String),
    Expired,
    PausedForActivity,
}

enum UploadServerState {
    Completed(String),
    Incomplete(u64),
    Expired,
    Retry(Duration),
}

fn run_resumable_upload<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    state_path: &Path,
    state: &mut ResumableUploadState,
) -> Result<ResumableResult> {
    if config.idle_only && !deck_is_idle()? {
        return Ok(ResumableResult::PausedForActivity);
    }
    let mut file = File::open(&candidate.video_path)?;
    let total = state.video_size_bytes;
    let mut next_byte = match query_resumable_upload(api, state, total)? {
        UploadServerState::Completed(video_id) => return Ok(ResumableResult::Completed(video_id)),
        UploadServerState::Incomplete(next) => next,
        UploadServerState::Expired => return Ok(ResumableResult::Expired),
        UploadServerState::Retry(_) => unreachable!("query helper consumes retry responses"),
    };
    state.next_byte = next_byte;
    write_private_json_atomic(state_path, state)?;
    let mut interruptions = 0_u8;

    loop {
        if config.idle_only && !deck_is_idle()? {
            state.next_byte = next_byte;
            write_private_json_atomic(state_path, state)?;
            return Ok(ResumableResult::PausedForActivity);
        }
        if next_byte >= total {
            match query_resumable_upload(api, state, total)? {
                UploadServerState::Completed(video_id) => {
                    return Ok(ResumableResult::Completed(video_id));
                }
                UploadServerState::Incomplete(next) => next_byte = next,
                UploadServerState::Expired => return Ok(ResumableResult::Expired),
                UploadServerState::Retry(_) => unreachable!("query helper consumes retries"),
            }
            continue;
        }
        file.seek(SeekFrom::Start(next_byte))?;
        let remaining: usize = (total - next_byte)
            .min(UPLOAD_CHUNK_BYTES as u64)
            .try_into()
            .expect("upload chunk is bounded by usize constant");
        let mut chunk = vec![0_u8; remaining];
        file.read_exact(&mut chunk)?;
        let last_byte = next_byte + remaining as u64 - 1;
        let request = api
            .authorized("PUT", state.upload_url.clone())
            .header("Content-Type", MP4_CONTENT_TYPE)
            .header("Content-Length", remaining.to_string())
            .header(
                "Content-Range",
                format!("bytes {next_byte}-{last_byte}/{total}"),
            )
            .body(chunk);
        let response = match api.transport.send(request) {
            Ok(response) => response,
            Err(_) => {
                interruptions = interruptions.saturating_add(1);
                if interruptions > MAX_UPLOAD_INTERRUPTS_PER_SYNC {
                    bail!("resumable upload was interrupted repeatedly; progress was preserved");
                }
                thread::sleep(exponential_backoff(interruptions));
                next_byte = match query_resumable_upload(api, state, total)? {
                    UploadServerState::Completed(video_id) => {
                        return Ok(ResumableResult::Completed(video_id));
                    }
                    UploadServerState::Incomplete(next) => next,
                    UploadServerState::Expired => return Ok(ResumableResult::Expired),
                    UploadServerState::Retry(_) => {
                        unreachable!("query helper consumes retry responses")
                    }
                };
                state.next_byte = next_byte;
                write_private_json_atomic(state_path, state)?;
                continue;
            }
        };
        match classify_upload_response(&response, total)? {
            UploadServerState::Completed(video_id) => {
                return Ok(ResumableResult::Completed(video_id));
            }
            UploadServerState::Incomplete(next) => {
                next_byte = next;
                state.next_byte = next;
                write_private_json_atomic(state_path, state)?;
                interruptions = 0;
            }
            UploadServerState::Expired => return Ok(ResumableResult::Expired),
            UploadServerState::Retry(delay) => {
                interruptions = interruptions.saturating_add(1);
                if interruptions > MAX_UPLOAD_INTERRUPTS_PER_SYNC {
                    bail!(
                        "YouTube repeatedly deferred the resumable upload; progress was preserved"
                    );
                }
                thread::sleep(delay.max(exponential_backoff(interruptions)));
                next_byte = match query_resumable_upload(api, state, total)? {
                    UploadServerState::Completed(video_id) => {
                        return Ok(ResumableResult::Completed(video_id));
                    }
                    UploadServerState::Incomplete(next) => next,
                    UploadServerState::Expired => return Ok(ResumableResult::Expired),
                    UploadServerState::Retry(_) => {
                        unreachable!("query helper consumes retry responses")
                    }
                };
                state.next_byte = next_byte;
                write_private_json_atomic(state_path, state)?;
            }
        }
    }
}

fn command_line_is_steam_game(command_line: &[u8]) -> bool {
    let arguments = command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    arguments.iter().any(|argument| {
        Path::new(std::str::from_utf8(argument).unwrap_or_default())
            .file_name()
            .and_then(|name| name.to_str())
            == Some("reaper")
    }) && arguments.iter().any(|argument| *argument == b"SteamLaunch")
        && arguments
            .iter()
            .any(|argument| argument.starts_with(b"AppId="))
}

#[cfg(target_os = "linux")]
fn deck_is_idle() -> Result<bool> {
    for entry in fs::read_dir("/proc")?.filter_map(Result::ok) {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let command_line = match fs::read(entry.path().join("cmdline")) {
            Ok(command_line) => command_line,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("failed to inspect Deck process activity"),
        };
        if command_line_is_steam_game(&command_line) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
fn deck_is_idle() -> Result<bool> {
    Ok(true)
}

fn query_resumable_upload<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    state: &ResumableUploadState,
    total: u64,
) -> Result<UploadServerState> {
    for attempt in 1..=MAX_UPLOAD_INTERRUPTS_PER_SYNC {
        let request = api
            .authorized("PUT", state.upload_url.clone())
            .header("Content-Length", "0")
            .header("Content-Range", format!("bytes */{total}"));
        match api.transport.send(request) {
            Ok(response) => match classify_upload_response(&response, total)? {
                UploadServerState::Retry(delay) => {
                    thread::sleep(delay.max(exponential_backoff(attempt)));
                }
                state => return Ok(state),
            },
            Err(_) => thread::sleep(exponential_backoff(attempt)),
        }
    }
    bail!("resumable upload status could not be recovered; progress was preserved")
}

fn classify_upload_response(response: &HttpResponse, total: u64) -> Result<UploadServerState> {
    match response.status {
        200 | 201 => {
            #[derive(Deserialize)]
            struct UploadedVideo {
                id: String,
            }
            let video: UploadedVideo = serde_json::from_slice(&response.body)
                .context("completed upload response was invalid")?;
            if video.id.is_empty() {
                bail!("completed upload response had no video ID");
            }
            Ok(UploadServerState::Completed(video.id))
        }
        308 => {
            let next = response
                .header("range")
                .map(parse_uploaded_range)
                .transpose()?
                .unwrap_or(0);
            if next > total {
                bail!("resumable upload server reported progress beyond the MP4 size");
            }
            Ok(UploadServerState::Incomplete(next))
        }
        404 | 410 => Ok(UploadServerState::Expired),
        429 | 500 | 502 | 503 | 504 => Ok(UploadServerState::Retry(retry_after(response))),
        status => bail!("resumable upload failed permanently with HTTP {status}"),
    }
}

fn parse_uploaded_range(value: &str) -> Result<u64> {
    let end = value
        .strip_prefix("bytes=")
        .and_then(|value| value.split_once('-'))
        .map(|(_, end)| end)
        .context("resumable upload returned an invalid Range header")?
        .parse::<u64>()
        .context("resumable upload returned a non-numeric Range header")?;
    end.checked_add(1).context("resumable Range overflow")
}

fn retry_after(response: &HttpResponse) -> Duration {
    retry_after_at(response, unix_now())
}

fn retry_after_at(response: &HttpResponse, now: i64) -> Duration {
    let Some(value) = response.header("retry-after") else {
        return Duration::ZERO;
    };
    if let Ok(seconds) = value.parse::<u64>() {
        return Duration::from_secs(seconds);
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .and_then(|date| date.timestamp().checked_sub(now))
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map(Duration::from_secs)
        .unwrap_or_default()
}

fn exponential_backoff(attempt: u8) -> Duration {
    Duration::from_secs(1_u64 << attempt.saturating_sub(1).min(6))
}

#[derive(Deserialize)]
struct VideoListResponse {
    #[serde(default)]
    items: Vec<VideoResource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoResource {
    status: VideoStatus,
    #[serde(default)]
    processing_details: Option<VideoProcessingDetails>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoStatus {
    privacy_status: String,
    #[serde(default)]
    upload_status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoProcessingDetails {
    processing_status: String,
}

fn reconcile_processing<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    receipt: &mut YoutubeReceipt,
    report: &mut YoutubeSyncReport,
    now: i64,
) -> Result<()> {
    let video_id = receipt
        .video_id
        .as_deref()
        .context("YouTube receipt has no video ID")?;
    let url = api_url(
        YOUTUBE_API_BASE,
        "videos",
        &[("part", "status,processingDetails"), ("id", video_id)],
    )?;
    let response: VideoListResponse =
        api.send_json(api.authorized("GET", url), "video processing lookup")?;
    let video = response
        .items
        .into_iter()
        .next()
        .context("uploaded YouTube video could not be found")?;

    if video.status.privacy_status != receipt.requested_privacy_status {
        receipt.processing_state = YoutubeProcessingState::PrivacyMismatch;
        receipt.last_error = Some(format!(
            "YouTube privacy is {}, expected {}",
            video.status.privacy_status, receipt.requested_privacy_status
        ));
        report.privacy_mismatches += 1;
        report.failures.push(format!(
            "{}: YouTube privacy is {}, expected {}",
            candidate.session.id, video.status.privacy_status, receipt.requested_privacy_status
        ));
        write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
        return Ok(());
    }

    match video
        .processing_details
        .as_ref()
        .map(|details| details.processing_status.as_str())
    {
        Some("succeeded") => {
            receipt.processing_state = YoutubeProcessingState::Succeeded;
            receipt.last_error = None;
            if receipt.confirmed_privacy_at_unix_seconds.is_none() {
                receipt.confirmed_privacy_at_unix_seconds = Some(now);
                receipt.prune_after_unix_seconds =
                    Some(now.saturating_add(i64::from(config.retention_days) * 24 * 60 * 60));
            }
        }
        Some("failed" | "terminated") => {
            receipt.processing_state = YoutubeProcessingState::Failed;
            receipt.last_error = Some("YouTube processing failed".into());
            report.failures.push(format!(
                "{}: YouTube processing failed",
                candidate.session.id
            ));
        }
        Some(_) | None if video.status.upload_status == "failed" => {
            receipt.processing_state = YoutubeProcessingState::Failed;
            receipt.last_error = Some("YouTube upload processing failed".into());
            report.failures.push(format!(
                "{}: YouTube upload processing failed",
                candidate.session.id
            ));
        }
        Some(_) | None => {
            receipt.processing_state = YoutubeProcessingState::Processing;
            receipt.last_error = None;
            report.processing_pending += 1;
        }
    }
    write_youtube_receipt(&candidate.youtube_receipt_path, receipt)
}

enum CaptionDecision {
    Pending,
    NoCaption(NoCaptionReason),
    Upload { bytes: Vec<u8>, sha256: String },
}

fn caption_decision(candidate: &ArchiveCandidate) -> Result<CaptionDecision> {
    if candidate.transcript_path.is_file() {
        let bytes = fs::read(&candidate.transcript_path)?;
        if bytes.len() > 100 * 1024 * 1024 {
            bail!("SRT exceeds YouTube's 100 MiB caption limit");
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(CaptionDecision::NoCaption(NoCaptionReason::Empty));
        }
        let sha256 = lowercase_hex(&Sha256::digest(&bytes));
        return Ok(CaptionDecision::Upload { bytes, sha256 });
    }
    Ok(match candidate.transcription_state {
        TranscriptionJobState::Queued | TranscriptionJobState::Running { .. } => {
            CaptionDecision::Pending
        }
        TranscriptionJobState::Failed { .. } => CaptionDecision::NoCaption(NoCaptionReason::Failed),
        TranscriptionJobState::Cancelled => CaptionDecision::NoCaption(NoCaptionReason::Cancelled),
        TranscriptionJobState::NotRequested | TranscriptionJobState::Complete => {
            CaptionDecision::NoCaption(NoCaptionReason::Unavailable)
        }
    })
}

fn reconcile_caption<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    receipt: &mut YoutubeReceipt,
    report: &mut YoutubeSyncReport,
    now: i64,
) -> Result<()> {
    match caption_decision(candidate)? {
        CaptionDecision::Pending => {
            let existing_caption_id = receipt.caption.existing_caption_id();
            receipt.caption = CaptionState::Pending {
                existing_caption_id,
            };
        }
        CaptionDecision::NoCaption(reason) => {
            let existing_caption_id = receipt.caption.existing_caption_id();
            receipt.caption = CaptionState::NoCaption {
                reason,
                existing_caption_id,
            };
            report.terminal_no_caption += 1;
        }
        CaptionDecision::Upload { bytes, sha256 } => {
            if matches!(
                &receipt.caption,
                CaptionState::Uploaded {
                    srt_sha256,
                    ..
                } if srt_sha256 == &sha256
            ) {
                return Ok(());
            }
            let existing_caption_id = receipt.caption.existing_caption_id();
            // Persist pending before the API call so a crash never lets an old
            // hash satisfy retention after a regenerated transcript appears.
            receipt.caption = CaptionState::Pending {
                existing_caption_id: existing_caption_id.clone(),
            };
            write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
            let quota_cost = if existing_caption_id.is_some() {
                CAPTION_UPDATE_COST
            } else {
                CAPTION_INSERT_COST
            };
            if !reserve_daily_quota(config, QuotaReservation::OrdinaryUnits(quota_cost), now)? {
                report.quota_deferred += 1;
                return Ok(());
            }
            let caption_id = upload_caption(
                api,
                config,
                receipt
                    .video_id
                    .as_deref()
                    .context("caption upload requires a YouTube video ID")?,
                existing_caption_id.as_deref(),
                &bytes,
            )?;
            receipt.caption = CaptionState::Uploaded {
                caption_id,
                srt_sha256: sha256,
            };
            if existing_caption_id.is_some() {
                report.captions_updated += 1;
            } else {
                report.captions_uploaded += 1;
            }
        }
    }
    write_youtube_receipt(&candidate.youtube_receipt_path, receipt)
}

fn upload_caption<T: HttpTransport>(
    api: &YoutubeApi<'_, T>,
    config: &DeckYoutubeConfig,
    video_id: &str,
    existing_caption_id: Option<&str>,
    srt: &[u8],
) -> Result<String> {
    let boundary = format!("retrofeel-{}", &lowercase_hex(&Sha256::digest(srt))[..24]);
    let (method, part, metadata) = if let Some(caption_id) = existing_caption_id {
        ("PUT", "id", json!({ "id": caption_id }))
    } else {
        (
            "POST",
            "snippet",
            json!({
                "snippet": {
                    "videoId": video_id,
                    "language": config.caption_language,
                    "name": "RetroFeel automatic transcript",
                    "isDraft": false,
                }
            }),
        )
    };
    let body = multipart_caption_body(&boundary, &serde_json::to_vec(&metadata)?, srt);
    let url = api_url(
        YOUTUBE_UPLOAD_BASE,
        "captions",
        &[
            ("uploadType", "multipart"),
            ("part", part),
            ("sync", "false"),
        ],
    )?;
    let response = api.transport.send(
        api.authorized(method, url)
            .header(
                "Content-Type",
                format!("multipart/related; boundary={boundary}"),
            )
            .body(body),
    )?;
    if !(200..300).contains(&response.status) {
        bail!("caption upload failed with HTTP {}", response.status);
    }
    #[derive(Deserialize)]
    struct CaptionResource {
        id: String,
    }
    let caption: CaptionResource =
        serde_json::from_slice(&response.body).context("caption response was invalid")?;
    if caption.id.is_empty() {
        bail!("caption response contained no caption ID");
    }
    Ok(caption.id)
}

fn multipart_caption_body(boundary: &str, metadata: &[u8], srt: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(metadata.len() + srt.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(srt);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn apply_retention(
    _config: &DeckYoutubeConfig,
    candidate: &ArchiveCandidate,
    receipt: &mut YoutubeReceipt,
    report: &mut YoutubeSyncReport,
    now: i64,
) -> Result<()> {
    if receipt.processing_state != YoutubeProcessingState::Succeeded
        || !receipt.caption.is_terminal()
        || matches!(
            candidate.transcription_state,
            TranscriptionJobState::Queued | TranscriptionJobState::Running { .. }
        )
        || receipt
            .prune_after_unix_seconds
            .is_none_or(|deadline| now < deadline)
        || receipt.pruned_at_unix_seconds.is_some()
    {
        return Ok(());
    }

    // Persist the tombstone before removing the MP4. The archive reconciler
    // therefore cannot recreate it if the process crashes between these steps.
    receipt.prune_tombstone_at_unix_seconds.get_or_insert(now);
    write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
    if candidate.video_path.is_file() {
        fs::remove_file(&candidate.video_path)
            .with_context(|| format!("failed to prune {}", candidate.video_path.display()))?;
    } else if candidate.video_path.exists() {
        bail!("retention target is not a regular MP4 file");
    }
    receipt.pruned_at_unix_seconds = Some(now);
    write_youtube_receipt(&candidate.youtube_receipt_path, receipt)?;
    report.pruned_videos += 1;
    Ok(())
}

fn lowercase_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::config::{DeckYoutubeCategory, DeckYoutubePrivacy};

    #[derive(Default)]
    struct FakeTransport {
        responses: Mutex<VecDeque<Result<HttpResponse>>>,
        requests: Mutex<Vec<CapturedRequest>>,
    }

    struct CapturedRequest {
        method: &'static str,
        url: String,
        headers: BTreeMap<String, String>,
        body_len: usize,
    }

    impl FakeTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(Ok).collect()),
                requests: Mutex::default(),
            }
        }

        fn requests(&self) -> std::sync::MutexGuard<'_, Vec<CapturedRequest>> {
            self.requests.lock().unwrap()
        }
    }

    impl HttpTransport for FakeTransport {
        fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(CapturedRequest {
                method: request.method,
                url: request.url,
                headers: request
                    .headers
                    .into_iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value))
                    .collect(),
                body_len: request.body.len(),
            });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .context("fake transport has no queued response")?
        }
    }

    fn response(status: u16, body: serde_json::Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn response_with_headers(
        status: u16,
        headers: &[(&str, &str)],
        body: serde_json::Value,
    ) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), (*value).into()))
                .collect(),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn youtube_config(directory: &Path) -> DeckYoutubeConfig {
        DeckYoutubeConfig {
            publisher_id: "variant-hunter".into(),
            game_ids: vec!["9223372058363166720".into()],
            enabled: true,
            upload_not_before: "2026-08-26T16:26:02Z".into(),
            oauth_client_path: directory.join("oauth-client.json"),
            oauth_token_path: directory.join("oauth-token.json"),
            expected_channel_id: "UC1111111111111111111111".into(),
            privacy_status: DeckYoutubePrivacy::Unlisted,
            category: DeckYoutubeCategory::Gaming,
            caption_language: "en".into(),
            timezone: "America/Vancouver".into(),
            retention_days: 30,
            idle_only: false,
        }
    }

    fn candidate(directory: &Path) -> ArchiveCandidate {
        let rule = DeckArchiveRule {
            game_id: "9223372058363166720".into(),
            display_name: "Variant Hunter".into(),
            destination_dir: directory.to_path_buf(),
            format: DeckArchiveFormat::Mp4,
        };
        let session = SessionSummary {
            id: "fg_9223372058363166720_20260826_162602".into(),
            game_id: rule.game_id.clone(),
            start_timestamp: 1_777_000_000.0,
            frame_count: 60,
            status: retrofeel_types::ExternalCaptureStatus::Complete,
            directory: directory.to_path_buf(),
            video_source: None,
        };
        let stem = crate::archive::archive_stem(&rule, &session.id);
        ArchiveCandidate {
            rule,
            session,
            transcription_state: TranscriptionJobState::NotRequested,
            video_path: directory.join(format!("{stem}.mp4")),
            archive_receipt_path: directory.join(format!("{stem}.archive.json")),
            transcript_path: directory.join(format!("{stem}.transcript.srt")),
            youtube_receipt_path: directory.join(format!("{stem}.youtube.json")),
        }
    }

    #[test]
    fn title_uses_pacific_wall_time_across_dst() {
        let temporary = tempfile::tempdir().unwrap();
        let config = youtube_config(temporary.path());
        let summer = Utc
            .with_ymd_and_hms(2026, 8, 26, 16, 26, 2)
            .single()
            .unwrap()
            .timestamp() as f64;
        let winter = Utc
            .with_ymd_and_hms(2026, 1, 26, 17, 26, 2)
            .single()
            .unwrap()
            .timestamp() as f64;

        assert_eq!(
            youtube_title(&config, "Variant Hunter", summer).unwrap(),
            "Variant Hunter — Development History — 2026-08-26 09:26:02 PT"
        );
        assert_eq!(
            youtube_title(&config, "Variant Hunter", winter).unwrap(),
            "Variant Hunter — Development History — 2026-01-26 09:26:02 PT"
        );
    }

    #[test]
    fn publisher_scan_includes_complete_legacy_game_ids_in_the_same_archive() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("archive");
        fs::create_dir(&archive).unwrap();
        let rule = DeckArchiveRule {
            game_id: "9223372058363166720".into(),
            display_name: "Variant Hunter".into(),
            destination_dir: archive.clone(),
            format: DeckArchiveFormat::Mp4,
        };
        let mut youtube = youtube_config(temporary.path());
        youtube.game_ids.push("9223372049773232128".into());
        let config = RecorderConfig {
            recording_archives: vec![rule],
            youtube: Some(youtube),
            ..RecorderConfig::default()
        };
        let session_id = "fg_9223372049773232128_20260823_002142";
        let manifest = json!({
            "core": {"name": "Steam Game Recording", "version": "1", "library_path": ""},
            "rom": null,
            "timing": {"fps": 60.0, "sample_rate": 48000.0, "start_timestamp": 1_777_000_000.0},
            "initial_state": null,
            "frame_count": 60,
            "pause_segments": [],
            "input_log": "input.json",
            "video": "video.mkv",
            "mic_audio": null,
            "transcript": null,
            "binding_map": null,
            "external_capture": {
                "kind": "steam_game_recording",
                "game_id": "9223372049773232128",
                "recording_id": session_id,
                "status": "complete"
            },
            "dropped_frames": 0,
            "format": "Json"
        });
        fs::write(
            archive.join(format!(
                "Variant-Hunter_version-pinned_legacy__{session_id}.manifest.json"
            )),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let mut report = YoutubeSyncReport::default();
        let candidates = scan_archive_candidates(&config, &mut report).unwrap();

        assert!(report.failures.is_empty());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session.id, session_id);
        assert_eq!(candidates[0].session.game_id, "9223372049773232128");
        assert_eq!(candidates[0].rule.display_name, "Variant Hunter");
    }

    #[test]
    fn cutoff_is_strict_and_rejects_invalid_timestamps() {
        assert!(!session_after_cutoff(100.0, 100.0));
        assert!(!session_after_cutoff(99.999, 100.0));
        assert!(session_after_cutoff(100.001, 100.0));
        assert!(!session_after_cutoff(f64::NAN, 100.0));
    }

    #[test]
    fn disabled_automation_limits_explicit_sync_to_one_unpublished_session() {
        let mut claimed = false;
        assert!(claim_unpublished_candidate(false, &mut claimed));
        assert!(!claim_unpublished_candidate(false, &mut claimed));
        assert!(claim_unpublished_candidate(true, &mut claimed));
    }

    #[test]
    fn persisted_rollout_cutoff_cannot_drift() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = youtube_config(temporary.path());
        ensure_rollout_policy(&config, true).unwrap();
        ensure_rollout_policy(&config, false).unwrap();

        config.upload_not_before = "2026-08-27T16:26:02Z".into();
        assert!(ensure_rollout_policy(&config, false)
            .unwrap_err()
            .to_string()
            .contains("immutable"));

        config.upload_not_before = "2026-08-26T16:26:02Z".into();
        config.expected_channel_id = "UC0000000000000000000000".into();
        assert!(ensure_rollout_policy(&config, false)
            .unwrap_err()
            .to_string()
            .contains("immutable"));
    }

    #[cfg(unix)]
    #[test]
    fn public_youtube_receipt_does_not_harden_archive_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("archive");
        fs::create_dir(&archive).unwrap();
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o755)).unwrap();
        let path = archive.join("session.youtube.json");

        let config = youtube_config(temporary.path());
        write_youtube_receipt(&path, &YoutubeReceipt::new("session", &config)).unwrap();

        assert_eq!(
            fs::metadata(&archive).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn oauth_refresh_is_persisted_without_exposing_tokens() {
        let temporary = tempfile::tempdir().unwrap();
        let config = youtube_config(temporary.path());
        fs::write(
            &config.oauth_client_path,
            br#"{"installed":{"client_id":"client-id","client_secret":"client-secret"}}"#,
        )
        .unwrap();
        write_private_json_atomic(
            &config.oauth_token_path,
            &OauthToken {
                format_version: OAUTH_TOKEN_VERSION,
                access_token: "expired-access".into(),
                refresh_token: "refresh-secret".into(),
                token_type: "Bearer".into(),
                scope: OAUTH_SCOPE.into(),
                expires_at_unix_seconds: 1,
            },
        )
        .unwrap();
        let transport = FakeTransport::with_responses(vec![response(
            200,
            json!({
                "access_token": "new-access-secret",
                "expires_in": 3600,
                "token_type": "Bearer"
            }),
        )]);

        let token = access_token(&transport, &config, 100).unwrap();
        assert_eq!(token.access_token, "new-access-secret");
        assert_eq!(token.refresh_token, "refresh-secret");
        let persisted: OauthToken = read_json(&config.oauth_token_path).unwrap();
        assert_eq!(persisted.access_token, "new-access-secret");
        let redacted = redact_sensitive(
            "refresh failed access_token=new-access-secret refresh_token=refresh-secret",
        );
        assert!(!redacted.contains("new-access-secret"));
        assert!(!redacted.contains("refresh-secret"));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&config.oauth_token_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn authorization_rejects_a_token_without_caption_scope() {
        let transport = FakeTransport::with_responses(vec![response(
            200,
            json!({
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "expires_in": 3600,
                "scope": "https://www.googleapis.com/auth/youtube.upload",
                "token_type": "Bearer"
            }),
        )]);
        let client = OauthClient {
            client_id: "client-id".into(),
            client_secret: "client-secret".into(),
        };

        let error = exchange_authorization_code(
            &transport,
            &client,
            "authorization-code",
            "pkce-verifier",
            "http://127.0.0.1:7352",
            100,
        )
        .err()
        .expect("scope mismatch should be rejected");

        assert!(error.to_string().contains("required YouTube scope"));
        assert!(!error.to_string().contains("access-secret"));
        assert!(!error.to_string().contains("refresh-secret"));
    }

    #[test]
    fn expected_channel_mismatch_fails_closed() {
        let transport = FakeTransport::with_responses(vec![response(
            200,
            json!({
                "items": [{
                    "id": "UC0000000000000000000000",
                    "contentDetails": {"relatedPlaylists": {"uploads": "UU_other"}}
                }]
            }),
        )]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        assert!(fetch_expected_channel(&api, "UC1111111111111111111111")
            .unwrap_err()
            .to_string()
            .contains("does not match"));
    }

    #[test]
    fn resumable_upload_recovers_from_reported_partial_progress() {
        let temporary = tempfile::tempdir().unwrap();
        let config = youtube_config(temporary.path());
        let candidate = candidate(temporary.path());
        let total = UPLOAD_CHUNK_BYTES + 4;
        fs::write(&candidate.video_path, vec![0_u8; total]).unwrap();
        let metadata = fs::metadata(&candidate.video_path).unwrap();
        let mut state = ResumableUploadState {
            format_version: RESUMABLE_STATE_VERSION,
            session_id: candidate.session.id.clone(),
            video_size_bytes: metadata.len(),
            video_modified_unix_nanos: modified_unix_nanos(&metadata),
            upload_url: "https://upload.invalid/session?upload_id=secret".into(),
            next_byte: 0,
            started_at_unix_seconds: 100,
        };
        let range = format!("bytes=0-{}", UPLOAD_CHUNK_BYTES - 1);
        let transport = FakeTransport::with_responses(vec![
            response(308, json!({})),
            response_with_headers(308, &[("range", &range)], json!({})),
            response(201, json!({"id": "video-id"})),
        ]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let state_path = temporary.path().join("upload.json");

        let result =
            run_resumable_upload(&api, &config, &candidate, &state_path, &mut state).unwrap();
        assert!(matches!(result, ResumableResult::Completed(id) if id == "video-id"));
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].body_len, 0);
        assert_eq!(requests[1].body_len, UPLOAD_CHUNK_BYTES);
        assert_eq!(requests[2].body_len, 4);
        let final_range = format!("bytes {}-{}/{}", UPLOAD_CHUNK_BYTES, total - 1, total);
        assert_eq!(
            requests[2].headers.get("content-range").map(String::as_str),
            Some(final_range.as_str())
        );
    }

    #[test]
    fn completed_status_recovery_preserves_video_id_without_another_query() {
        let temporary = tempfile::tempdir().unwrap();
        let config = youtube_config(temporary.path());
        let candidate = candidate(temporary.path());
        fs::write(&candidate.video_path, b"mp4").unwrap();
        let metadata = fs::metadata(&candidate.video_path).unwrap();
        let mut state = ResumableUploadState {
            format_version: RESUMABLE_STATE_VERSION,
            session_id: candidate.session.id.clone(),
            video_size_bytes: metadata.len(),
            video_modified_unix_nanos: modified_unix_nanos(&metadata),
            upload_url: "https://upload.invalid/recover-complete".into(),
            next_byte: 0,
            started_at_unix_seconds: 100,
        };
        let transport = FakeTransport::with_responses(vec![
            response(308, json!({})),
            response(503, json!({})),
            response(200, json!({"id": "recovered-video-id"})),
        ]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };

        let result = run_resumable_upload(
            &api,
            &config,
            &candidate,
            &temporary.path().join("upload.json"),
            &mut state,
        )
        .unwrap();

        assert!(matches!(
            result,
            ResumableResult::Completed(id) if id == "recovered-video-id"
        ));
        assert_eq!(transport.requests().len(), 3);
    }

    #[test]
    fn expired_resumable_session_is_detected_without_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let config = youtube_config(temporary.path());
        let candidate = candidate(temporary.path());
        fs::write(&candidate.video_path, b"mp4").unwrap();
        let metadata = fs::metadata(&candidate.video_path).unwrap();
        let mut state = ResumableUploadState {
            format_version: RESUMABLE_STATE_VERSION,
            session_id: candidate.session.id.clone(),
            video_size_bytes: metadata.len(),
            video_modified_unix_nanos: modified_unix_nanos(&metadata),
            upload_url: "https://upload.invalid/expired".into(),
            next_byte: 0,
            started_at_unix_seconds: 100,
        };
        let transport = FakeTransport::with_responses(vec![response(404, json!({}))]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let result = run_resumable_upload(
            &api,
            &config,
            &candidate,
            &temporary.path().join("upload.json"),
            &mut state,
        )
        .unwrap();
        assert!(matches!(result, ResumableResult::Expired));
    }

    #[test]
    fn rate_limits_and_server_failures_are_retryable() {
        let limited = response_with_headers(429, &[("retry-after", "7")], json!({}));
        assert!(matches!(
            classify_upload_response(&limited, 10).unwrap(),
            UploadServerState::Retry(delay) if delay == Duration::from_secs(7)
        ));
        assert!(matches!(
            classify_upload_response(&response(503, json!({})), 10).unwrap(),
            UploadServerState::Retry(_)
        ));
        assert!(classify_upload_response(&response(400, json!({})), 10).is_err());

        let dated = response_with_headers(
            503,
            &[("retry-after", "Wed, 21 Oct 2015 07:28:10 GMT")],
            json!({}),
        );
        assert_eq!(
            retry_after_at(&dated, 1_445_412_480),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn forced_private_processing_response_never_starts_retention() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        let config = youtube_config(temporary.path());
        let transport = FakeTransport::with_responses(vec![response(
            200,
            json!({
                "items": [{
                    "status": {"privacyStatus": "private", "uploadStatus": "uploaded"},
                    "processingDetails": {"processingStatus": "succeeded"}
                }]
            }),
        )]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        let mut report = YoutubeSyncReport::default();

        reconcile_processing(&api, &config, &candidate, &mut receipt, &mut report, 100).unwrap();
        assert_eq!(
            receipt.processing_state,
            YoutubeProcessingState::PrivacyMismatch
        );
        assert!(receipt.prune_after_unix_seconds.is_none());
        assert_eq!(report.privacy_mismatches, 1);
        assert!(report.has_failures());
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn delayed_and_empty_transcripts_have_explicit_states() {
        let temporary = tempfile::tempdir().unwrap();
        let mut candidate = candidate(temporary.path());
        candidate.transcription_state = TranscriptionJobState::Running {
            progress_percent: 50,
        };
        assert!(matches!(
            caption_decision(&candidate).unwrap(),
            CaptionDecision::Pending
        ));

        fs::write(&candidate.transcript_path, b" \n\t").unwrap();
        assert!(matches!(
            caption_decision(&candidate).unwrap(),
            CaptionDecision::NoCaption(NoCaptionReason::Empty)
        ));
    }

    #[test]
    fn regenerated_srt_updates_the_existing_caption_by_hash() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        fs::write(
            &candidate.transcript_path,
            b"1\n00:00:00,000 --> 00:00:01,000\nnew words\n",
        )
        .unwrap();
        let config = youtube_config(temporary.path());
        let transport =
            FakeTransport::with_responses(vec![response(200, json!({"id": "caption-id"}))]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        receipt.caption = CaptionState::Uploaded {
            caption_id: "caption-id".into(),
            srt_sha256: "old-hash".into(),
        };
        let mut report = YoutubeSyncReport::default();

        reconcile_caption(&api, &config, &candidate, &mut receipt, &mut report, 100).unwrap();
        assert!(matches!(
            receipt.caption,
            CaptionState::Uploaded { ref srt_sha256, .. } if srt_sha256 != "old-hash"
        ));
        assert_eq!(report.captions_updated, 1);
        assert_eq!(transport.requests()[0].method, "PUT");
    }

    #[test]
    fn caption_quota_failure_remains_pending_and_blocks_pruning() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        fs::write(
            &candidate.transcript_path,
            b"1\n00:00:00,000 --> 00:00:01,000\nwords\n",
        )
        .unwrap();
        let config = youtube_config(temporary.path());
        let transport = FakeTransport::with_responses(vec![response(403, json!({}))]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        receipt.processing_state = YoutubeProcessingState::Succeeded;
        receipt.prune_after_unix_seconds = Some(1);
        let mut report = YoutubeSyncReport::default();

        assert!(
            reconcile_caption(&api, &config, &candidate, &mut receipt, &mut report, 100,).is_err()
        );
        assert_eq!(receipt.caption, CaptionState::default());
        assert!(!receipt.caption.is_terminal());
    }

    #[test]
    fn recent_session_marker_recovers_video_id_without_duplicate_upload() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        fs::write(&candidate.video_path, b"mp4").unwrap();
        let config = youtube_config(temporary.path());
        let transport = FakeTransport::with_responses(vec![response(
            200,
            json!({
                "items": [{
                    "status": {"privacyStatus": "unlisted", "uploadStatus": "processed"},
                    "processingDetails": {"processingStatus": "succeeded"}
                }]
            }),
        )]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        let mut recent = BTreeMap::from([(candidate.session.id.clone(), "video-id".into())]);
        let mut report = YoutubeSyncReport::default();

        sync_candidate(
            &api,
            &config,
            &candidate,
            &mut receipt,
            &mut recent,
            &mut report,
            100,
        )
        .unwrap();
        assert_eq!(receipt.video_id.as_deref(), Some("video-id"));
        assert_eq!(report.reconciled_videos, 1);
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].url.contains("/upload/"));
    }

    #[test]
    fn confirmed_video_is_not_polled_again_before_retention() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        let config = youtube_config(temporary.path());
        let transport = FakeTransport::default();
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        receipt.processing_state = YoutubeProcessingState::Succeeded;
        receipt.confirmed_privacy_at_unix_seconds = Some(100);
        receipt.prune_after_unix_seconds = Some(10_000);
        let mut recent = BTreeMap::new();
        let mut report = YoutubeSyncReport::default();

        sync_candidate(
            &api,
            &config,
            &candidate,
            &mut receipt,
            &mut recent,
            &mut report,
            200,
        )
        .unwrap();

        assert!(transport.requests().is_empty());
        assert!(matches!(
            receipt.caption,
            CaptionState::NoCaption {
                reason: NoCaptionReason::Unavailable,
                ..
            }
        ));
    }

    #[test]
    fn retention_writes_tombstone_before_removing_only_the_mp4() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        fs::write(&candidate.video_path, b"mp4").unwrap();
        fs::write(&candidate.transcript_path, b"captions remain").unwrap();
        let config = youtube_config(temporary.path());
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        receipt.processing_state = YoutubeProcessingState::Succeeded;
        receipt.caption = CaptionState::NoCaption {
            reason: NoCaptionReason::Unavailable,
            existing_caption_id: None,
        };
        receipt.prune_after_unix_seconds = Some(99);
        let mut report = YoutubeSyncReport::default();

        apply_retention(&config, &candidate, &mut receipt, &mut report, 100).unwrap();
        assert!(!candidate.video_path.exists());
        assert!(candidate.transcript_path.is_file());
        assert_eq!(receipt.prune_tombstone_at_unix_seconds, Some(100));
        assert_eq!(receipt.pruned_at_unix_seconds, Some(100));
        let persisted: YoutubeReceipt = read_json(&candidate.youtube_receipt_path).unwrap();
        assert_eq!(persisted.pruned_at_unix_seconds, Some(100));
        assert_eq!(report.pruned_videos, 1);
    }

    #[test]
    fn steam_reaper_command_line_blocks_idle_uploads() {
        assert!(command_line_is_steam_game(
            b"/home/deck/.local/share/Steam/ubuntu12_32/reaper\0SteamLaunch\0AppId=2147483653\0--\0/game\0"
        ));
        assert!(!command_line_is_steam_game(
            b"/usr/bin/steamwebhelper\0--type=renderer\0"
        ));
        assert!(!command_line_is_steam_game(
            b"/home/deck/.local/share/Steam/ubuntu12_32/reaper\0OtherMode\0AppId=1\0"
        ));
    }

    #[test]
    fn publisher_scope_routes_only_its_exact_game_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let variant = DeckArchiveRule {
            game_id: "1".into(),
            display_name: "Variant Hunter".into(),
            destination_dir: temporary.path().join("variant"),
            format: DeckArchiveFormat::Mp4,
        };
        let secondary = DeckArchiveRule {
            game_id: "2".into(),
            display_name: "Example Meadow Blocks".into(),
            destination_dir: temporary.path().join("secondary"),
            format: DeckArchiveFormat::Mp4,
        };
        let mut variant_publisher = youtube_config(temporary.path());
        variant_publisher.publisher_id = "variant-hunter".into();
        variant_publisher.game_ids = vec!["1".into()];
        let mut secondary_publisher = youtube_config(temporary.path());
        secondary_publisher.publisher_id = "example-meadow-world".into();
        secondary_publisher.game_ids = vec!["2".into()];
        secondary_publisher.enabled = false;
        secondary_publisher.oauth_token_path = temporary.path().join("secondary-token.json");
        secondary_publisher.expected_channel_id = "UC0000000000000000000000".into();
        let config = RecorderConfig {
            recording_archives: vec![variant, secondary],
            youtube: None,
            youtube_publishers: vec![variant_publisher, secondary_publisher],
            ..RecorderConfig::default()
        };

        let scoped = scoped_publisher_config(&config, "example-meadow-world", true).unwrap();
        assert_eq!(scoped.recording_archives.len(), 1);
        assert_eq!(scoped.recording_archives[0].game_id, "2");
        let selected = scoped.youtube.unwrap();
        assert_eq!(selected.publisher_id, "example-meadow-world");
        assert_eq!(selected.privacy_status, DeckYoutubePrivacy::Private);
    }

    #[test]
    fn receipts_cannot_cross_publishers_or_channels() {
        let temporary = tempfile::tempdir().unwrap();
        let first = youtube_config(temporary.path());
        let receipt = YoutubeReceipt::new("session", &first);
        let mut second = first.clone();
        second.publisher_id = "example-meadow-world".into();
        second.expected_channel_id = "UC0000000000000000000000".into();

        assert!(receipt.validate_for("session", &first).is_ok());
        assert!(receipt.validate_for("session", &second).is_err());
    }

    #[test]
    fn shared_quota_ledger_caps_calls_and_resets_on_the_next_pacific_day() {
        let temporary = tempfile::tempdir().unwrap();
        let config = youtube_config(temporary.path());
        let day_one = 1_777_000_000;
        for _ in 0..DAILY_VIDEO_INSERT_LIMIT {
            assert!(reserve_daily_quota(&config, QuotaReservation::VideoInsert, day_one).unwrap());
        }
        assert!(!reserve_daily_quota(&config, QuotaReservation::VideoInsert, day_one).unwrap());
        for _ in 0..(DAILY_ORDINARY_QUOTA_LIMIT / CAPTION_INSERT_COST) {
            assert!(reserve_daily_quota(
                &config,
                QuotaReservation::OrdinaryUnits(CAPTION_INSERT_COST),
                day_one,
            )
            .unwrap());
        }
        assert!(!reserve_daily_quota(
            &config,
            QuotaReservation::OrdinaryUnits(CAPTION_INSERT_COST),
            day_one,
        )
        .unwrap());
        assert!(reserve_daily_quota(
            &config,
            QuotaReservation::VideoInsert,
            day_one + 36 * 60 * 60,
        )
        .unwrap());
    }

    #[test]
    fn private_receipt_accepts_successful_private_processing() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        let mut config = youtube_config(temporary.path());
        config.privacy_status = DeckYoutubePrivacy::Private;
        config.retention_days = 7;
        let transport = FakeTransport::with_responses(vec![response(
            200,
            json!({
                "items": [{
                    "status": {"privacyStatus": "private", "uploadStatus": "uploaded"},
                    "processingDetails": {"processingStatus": "succeeded"}
                }]
            }),
        )]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        let mut report = YoutubeSyncReport::default();

        reconcile_processing(&api, &config, &candidate, &mut receipt, &mut report, 100).unwrap();
        assert_eq!(receipt.processing_state, YoutubeProcessingState::Succeeded);
        assert_eq!(receipt.confirmed_privacy_at_unix_seconds, Some(100));
        assert_eq!(receipt.prune_after_unix_seconds, Some(604_900));
    }

    #[test]
    fn late_srt_is_attached_after_the_local_mp4_was_pruned() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = candidate(temporary.path());
        fs::write(
            &candidate.transcript_path,
            b"1\n00:00:00,000 --> 00:00:01,000\nlate words\n",
        )
        .unwrap();
        let config = youtube_config(temporary.path());
        let transport =
            FakeTransport::with_responses(vec![response(200, json!({"id": "caption-id"}))]);
        let api = YoutubeApi {
            transport: &transport,
            access_token: "secret",
        };
        let mut receipt = YoutubeReceipt::new(&candidate.session.id, &config);
        receipt.video_id = Some("video-id".into());
        receipt.processing_state = YoutubeProcessingState::Succeeded;
        receipt.pruned_at_unix_seconds = Some(50);
        receipt.caption = CaptionState::NoCaption {
            reason: NoCaptionReason::Unavailable,
            existing_caption_id: None,
        };
        let mut recent = BTreeMap::new();
        let mut report = YoutubeSyncReport::default();

        sync_candidate(
            &api,
            &config,
            &candidate,
            &mut receipt,
            &mut recent,
            &mut report,
            100,
        )
        .unwrap();
        assert!(matches!(receipt.caption, CaptionState::Uploaded { .. }));
        assert_eq!(report.captions_uploaded, 1);
    }
}
