//! Idempotent, game-allowlisted archives for completed Steam Deck recordings.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDateTime;
use retrofeel_types::{ExternalCaptureStatus, SessionManifest};
use serde::{Deserialize, Serialize};

use crate::config::{DeckArchiveFormat, DeckArchiveRule, RecorderConfig};
use crate::session::{list_sessions, SessionSummary};

const ARCHIVE_SIDECARS: [(&str, &str); 4] = [
    ("manifest.json", "manifest.json"),
    ("controller-map.json", "controller-map.json"),
    ("steam-audio-transcript.srt", "transcript.srt"),
    ("steam-audio-transcript.json", "transcript.json"),
];
const ARCHIVE_RECEIPT_VERSION: u32 = 3;
const YOUTUBE_FRAME_RATE: u64 = 60;
const DURATION_TOLERANCE_SECONDS: f64 = 0.25;
// MP4 encoding normalizes small track-start offsets. Steam's archived AAC
// starts are packet-aligned and can trail video by several 1024-sample frames,
// so accept that bounded normalization while still requiring duration parity,
// the delivery profile, and a full decode pass.
const AV_START_TOLERANCE_SECONDS: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationOutcome {
    Verified,
}

/// Durable evidence that a specific archive file passed every publishing check.
///
/// Size and modification time bind the receipt to the exact file. Any later
/// mutation invalidates the receipt and forces the expensive probe/decode pass
/// before the archive can be uploaded or an MKV source can be deleted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ArchiveVerification {
    pub(crate) format_version: u32,
    pub(crate) session_id: String,
    pub(crate) session_start_unix_seconds: f64,
    pub(crate) archive_format: DeckArchiveFormat,
    pub(crate) container: String,
    pub(crate) video_codec: String,
    #[serde(default)]
    pub(crate) video_profile: String,
    #[serde(default)]
    pub(crate) pixel_format: String,
    #[serde(default)]
    pub(crate) video_frame_rate: String,
    pub(crate) audio_codec: String,
    #[serde(default)]
    pub(crate) audio_profile: String,
    #[serde(default)]
    pub(crate) audio_sample_rate_hz: u32,
    pub(crate) expected_frame_count: u64,
    #[serde(default)]
    pub(crate) source_video_packet_count: u64,
    pub(crate) video_packet_count: u64,
    pub(crate) duration_seconds: f64,
    pub(crate) reference_duration_seconds: f64,
    pub(crate) duration_delta_seconds: f64,
    pub(crate) av_start_offset_seconds: f64,
    pub(crate) reference_av_start_offset_seconds: f64,
    pub(crate) av_start_delta_seconds: f64,
    pub(crate) audio_present: bool,
    pub(crate) fast_start: Option<bool>,
    pub(crate) edit_lists_absent: Option<bool>,
    pub(crate) full_decode_passed: bool,
    pub(crate) video_size_bytes: u64,
    pub(crate) video_modified_unix_nanos: u64,
    pub(crate) verification_outcome: VerificationOutcome,
}

#[derive(Debug, Default, Serialize)]
pub struct ArchiveSyncReport {
    pub configured_rules: usize,
    pub already_running: bool,
    pub matched_sessions: usize,
    pub archived_videos: usize,
    pub existing_videos: usize,
    pub copied_sidecars: usize,
    pub unavailable_sources: usize,
    pub intentionally_pruned: usize,
    pub failures: Vec<String>,
}

impl ArchiveSyncReport {
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    fn log_progress(&self) {
        if self.archived_videos > 0 || self.copied_sidecars > 0 {
            log::info!(
                "recording archive sync created {} video(s), retained {} existing video(s), and copied {} sidecar(s)",
                self.archived_videos,
                self.existing_videos,
                self.copied_sidecars
            );
        }
        for failure in &self.failures {
            log::warn!("recording archive sync failed: {failure}");
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ArchiveMigrationReport {
    pub target_format: Option<DeckArchiveFormat>,
    pub dry_run: bool,
    pub delete_verified_mkv: bool,
    pub already_running: bool,
    pub discovered_mkvs: usize,
    pub planned_migrations: usize,
    pub migrated_mp4s: usize,
    pub existing_verified_mp4s: usize,
    pub deleted_mkvs: usize,
    pub retained_mkvs: usize,
    pub failures: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ArchiveRepairReport {
    pub dry_run: bool,
    pub already_running: bool,
    pub discovered_mp4s: usize,
    pub planned_repairs: usize,
    pub repaired_mp4s: usize,
    pub reverified_mp4s: usize,
    pub already_current_mp4s: usize,
    pub failures: Vec<String>,
}

impl ArchiveRepairReport {
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }
}

impl ArchiveMigrationReport {
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Reconcile every configured archive against finalized companion sessions.
///
/// Only exact game-ID matches with complete frame alignment are eligible.
/// Existing files require a current verification receipt, making repeated
/// passes safe while avoiding a full decode on every background interval.
pub fn sync_recording_archives(config: &RecorderConfig) -> Result<ArchiveSyncReport> {
    let mut report = ArchiveSyncReport {
        configured_rules: config.recording_archives.len(),
        ..Default::default()
    };
    if config.recording_archives.is_empty() {
        return Ok(report);
    }

    fs::create_dir_all(&config.recordings_dir).with_context(|| {
        format!(
            "failed to create recording directory {}",
            config.recordings_dir.display()
        )
    })?;
    let Some(_lock) = ArchiveSyncLock::try_acquire(&config.recordings_dir)? else {
        report.already_running = true;
        return Ok(report);
    };

    let sessions = list_sessions(config)?;
    for rule in &config.recording_archives {
        if let Err(error) = fs::create_dir_all(&rule.destination_dir) {
            report.failures.push(format!(
                "could not create {} for {}: {error}",
                rule.destination_dir.display(),
                rule.display_name
            ));
            continue;
        }
        for session in sessions
            .iter()
            .filter(|session| archive_matches(rule, session))
        {
            report.matched_sessions += 1;
            match archive_session(config, rule, session) {
                Ok(ArchiveOutcome::Archived { sidecars }) => {
                    report.archived_videos += 1;
                    report.copied_sidecars += sidecars;
                }
                Ok(ArchiveOutcome::Existing { sidecars }) => {
                    report.existing_videos += 1;
                    report.copied_sidecars += sidecars;
                }
                Ok(ArchiveOutcome::Unavailable) => report.unavailable_sources += 1,
                Ok(ArchiveOutcome::Pruned { sidecars }) => {
                    report.intentionally_pruned += 1;
                    report.copied_sidecars += sidecars;
                }
                Err(error) => report.failures.push(format!(
                    "{} ({}) to {}: {error:#}",
                    session.id,
                    rule.display_name,
                    rule.destination_dir.display()
                )),
            }
        }
    }
    Ok(report)
}

/// Sequentially replace every MKV in a configured archive directory with a
/// fully verified MP4 file.
///
/// At most one partial MP4 exists at a time. The exact MKV being processed is
/// removed only after the MP4 was atomically published and its current receipt
/// was durably written. Sidecars are deliberately untouched. Historical files
/// with legacy display labels, game IDs, or degraded manifests are included;
/// they remain outside the publisher's stricter current-rule candidate scan.
pub fn migrate_recording_archives(
    config: &RecorderConfig,
    target_format: DeckArchiveFormat,
    delete_verified_mkv: bool,
    dry_run: bool,
) -> Result<ArchiveMigrationReport> {
    if target_format != DeckArchiveFormat::Mp4 {
        bail!("archive migration currently supports only mp4");
    }
    let mut report = ArchiveMigrationReport {
        target_format: Some(target_format),
        dry_run,
        delete_verified_mkv,
        ..Default::default()
    };
    if config.recording_archives.is_empty() {
        return Ok(report);
    }

    fs::create_dir_all(&config.recordings_dir)?;
    let Some(_lock) = ArchiveSyncLock::try_acquire(&config.recordings_dir)? else {
        report.already_running = true;
        return Ok(report);
    };

    let mut seen = std::collections::BTreeSet::new();
    for rule in &config.recording_archives {
        if !rule.destination_dir.is_dir() {
            continue;
        }
        let mut candidates = archive_mkv_candidates(rule)?;
        candidates.sort();
        for source in candidates {
            if !seen.insert(source.clone()) {
                continue;
            }
            report.discovered_mkvs += 1;
            report.planned_migrations += 1;
            if dry_run {
                report.retained_mkvs += 1;
                continue;
            }

            match migrate_one_archive(config, rule, &source, delete_verified_mkv) {
                Ok(MigrationOutcome {
                    created,
                    source_deleted,
                }) => {
                    if created {
                        report.migrated_mp4s += 1;
                    } else {
                        report.existing_verified_mp4s += 1;
                    }
                    if source_deleted {
                        report.deleted_mkvs += 1;
                    } else {
                        report.retained_mkvs += 1;
                    }
                }
                Err(error) => {
                    report.retained_mkvs += 1;
                    report
                        .failures
                        .push(format!("{}: {error:#}", source.display()));
                }
            }
        }
    }
    Ok(report)
}

/// Sequentially replace configured MP4 archives with the current YouTube
/// delivery profile while preserving each archive's wall-clock duration.
///
/// The existing MP4 is itself the verified reference. One sibling partial is
/// encoded and fully checked before a rename replaces the derived archive;
/// Steam source media and archive sidecars are never changed.
pub fn repair_recording_archives(
    config: &RecorderConfig,
    dry_run: bool,
) -> Result<ArchiveRepairReport> {
    let mut report = ArchiveRepairReport {
        dry_run,
        ..Default::default()
    };
    if config.recording_archives.is_empty() {
        return Ok(report);
    }

    fs::create_dir_all(&config.recordings_dir)?;
    let Some(_lock) = ArchiveSyncLock::try_acquire(&config.recordings_dir)? else {
        report.already_running = true;
        return Ok(report);
    };

    let mut seen = std::collections::BTreeSet::new();
    for rule in config
        .recording_archives
        .iter()
        .filter(|rule| rule.format == DeckArchiveFormat::Mp4 && rule.destination_dir.is_dir())
    {
        let mut candidates = archive_mp4_candidates(rule)?;
        candidates.sort();
        for video in candidates {
            if !seen.insert(video.clone()) {
                continue;
            }
            report.discovered_mp4s += 1;
            let receipt = video.with_extension("archive.json");
            let session = match repair_session_summary(rule, &video, &receipt) {
                Ok(session) => session,
                Err(error) => {
                    report
                        .failures
                        .push(format!("{}: {error:#}", video.display()));
                    continue;
                }
            };
            if current_archive_verification(&video, &receipt, &session, DeckArchiveFormat::Mp4)?
                .is_some()
            {
                report.already_current_mp4s += 1;
                continue;
            }
            report.planned_repairs += 1;
            if dry_run {
                continue;
            }

            match repair_one_archive(config, &video, &receipt, &session) {
                Ok(RepairOutcome::Repaired) => report.repaired_mp4s += 1,
                Ok(RepairOutcome::Reverified) => report.reverified_mp4s += 1,
                Err(error) => report
                    .failures
                    .push(format!("{}: {error:#}", video.display())),
            }
        }
    }
    Ok(report)
}

struct ArchiveSyncLock {
    file: File,
}

impl ArchiveSyncLock {
    fn try_acquire(recordings_dir: &Path) -> Result<Option<Self>> {
        let path = recordings_dir.join(".archive-sync.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open archive lock {}", path.display()))?;

        #[cfg(unix)]
        {
            // SAFETY: `file` owns a valid descriptor for the duration of the
            // call and remains alive in the returned guard while locked.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Some(Self { file }));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            Err(error).with_context(|| format!("failed to acquire archive lock {}", path.display()))
        }

        #[cfg(not(unix))]
        Ok(Some(Self { file }))
    }
}

#[cfg(unix)]
impl Drop for ArchiveSyncLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until `self.file` is dropped
        // immediately after this method returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Start a detached reconciliation worker for the long-running watcher.
///
/// Archive remuxing and verification can take minutes, so neither runs on the
/// capture/log thread. A full scan also repairs missed work after restarts and
/// picks up transcript sidecars that finish asynchronously.
pub(crate) fn spawn_archive_worker(config: RecorderConfig) {
    if config.recording_archives.is_empty() {
        return;
    }
    let interval = Duration::from_secs(config.archive_sync_interval_seconds.max(1));
    let spawn = thread::Builder::new()
        .name("retrofeel-recording-archive".into())
        .spawn(move || loop {
            match sync_recording_archives(&config) {
                Ok(report) => report.log_progress(),
                Err(error) => log::warn!("recording archive reconciliation failed: {error:#}"),
            }
            thread::sleep(interval);
        });
    if let Err(error) = spawn {
        log::error!("could not start recording archive worker: {error}");
    }
}

fn archive_matches(rule: &DeckArchiveRule, session: &SessionSummary) -> bool {
    rule.game_id == session.game_id && session.status == ExternalCaptureStatus::Complete
}

enum ArchiveOutcome {
    Archived { sidecars: usize },
    Existing { sidecars: usize },
    Unavailable,
    Pruned { sidecars: usize },
}

fn archive_session(
    config: &RecorderConfig,
    rule: &DeckArchiveRule,
    session: &SessionSummary,
) -> Result<ArchiveOutcome> {
    let stem = archive_stem(rule, &session.id);
    let extension = rule.format.extension();
    let output = rule.destination_dir.join(format!("{stem}.{extension}"));
    let verification_path = rule.destination_dir.join(format!("{stem}.archive.json"));
    let youtube_receipt = rule.destination_dir.join(format!("{stem}.youtube.json"));
    let video_exists = is_nonempty_file(&output);
    if output.exists() && !video_exists {
        bail!(
            "archive destination is not a non-empty file: {}",
            output.display()
        );
    }
    if !video_exists
        && rule.format == DeckArchiveFormat::Mp4
        && has_retention_tombstone(&youtube_receipt)?
    {
        let sidecars = sync_sidecars(&session.directory, &rule.destination_dir, &stem)?;
        return Ok(ArchiveOutcome::Pruned { sidecars });
    }

    if !video_exists {
        let Some(source) = session.video_source.as_ref().filter(|path| path.is_file()) else {
            return Ok(ArchiveOutcome::Unavailable);
        };
        let partial = rule
            .destination_dir
            .join(format!(".{stem}.partial.{extension}"));
        remove_stale_partial(&partial)?;
        if let Err(error) = create_archive_video(config, source, &partial, rule.format, true) {
            let _ = remove_stale_partial(&partial);
            return Err(error);
        }
        let verification = match verify_archive_video(
            config,
            &partial,
            source,
            session,
            rule.format,
            ReferenceFramePolicy::Any,
        ) {
            Ok(verification) => verification,
            Err(error) => {
                let _ = remove_stale_partial(&partial);
                return Err(error);
            }
        };
        if let Err(error) = publish_archive_file(&partial, &output) {
            let _ = remove_stale_partial(&partial);
            return Err(error);
        }
        write_archive_verification(&output, &verification_path, session, verification)?;
    } else {
        verify_existing_archive(
            config,
            &output,
            &verification_path,
            session,
            rule.format,
            session.video_source.as_deref(),
            ReferenceFramePolicy::Any,
        )?;
    }

    let sidecars = sync_sidecars(&session.directory, &rule.destination_dir, &stem)?;
    if video_exists {
        Ok(ArchiveOutcome::Existing { sidecars })
    } else {
        Ok(ArchiveOutcome::Archived { sidecars })
    }
}

struct MigrationOutcome {
    created: bool,
    source_deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairOutcome {
    Repaired,
    Reverified,
}

fn migrate_one_archive(
    config: &RecorderConfig,
    rule: &DeckArchiveRule,
    source: &Path,
    delete_verified_mkv: bool,
) -> Result<MigrationOutcome> {
    let session = migration_session_summary(config, rule, source)?;
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .context("managed MKV has no UTF-8 stem")?;
    let output = source.with_file_name(format!("{stem}.mp4"));
    let verification_path = source.with_file_name(format!("{stem}.archive.json"));
    let mut created = false;

    if output.exists() && !is_nonempty_file(&output) {
        bail!(
            "MP4 destination is not a non-empty file: {}",
            output.display()
        );
    }

    if is_nonempty_file(&output) {
        verify_existing_archive(
            config,
            &output,
            &verification_path,
            &session,
            DeckArchiveFormat::Mp4,
            Some(source),
            ReferenceFramePolicy::ExactSession,
        )?;
    } else {
        let partial = source.with_file_name(format!(".{stem}.partial.mp4"));
        remove_stale_partial(&partial)?;
        if let Err(error) =
            create_archive_video(config, source, &partial, DeckArchiveFormat::Mp4, false)
        {
            let _ = remove_stale_partial(&partial);
            return Err(error);
        }
        let verification = match verify_archive_video(
            config,
            &partial,
            source,
            &session,
            DeckArchiveFormat::Mp4,
            ReferenceFramePolicy::ExactSession,
        ) {
            Ok(verification) => verification,
            Err(error) => {
                let _ = remove_stale_partial(&partial);
                return Err(error);
            }
        };
        if let Err(error) = publish_archive_file(&partial, &output) {
            let _ = remove_stale_partial(&partial);
            return Err(error);
        }
        write_archive_verification(&output, &verification_path, &session, verification)?;
        created = true;
    }

    let source_deleted = if delete_verified_mkv {
        // Re-check the receipt after publication. This is intentionally the
        // final gate before deleting the exact source path.
        if current_archive_verification(
            &output,
            &verification_path,
            &session,
            DeckArchiveFormat::Mp4,
        )?
        .is_none()
        {
            bail!("published MP4 receipt was not current; original MKV retained");
        }
        fs::remove_file(source).context("verified MP4 was retained but MKV deletion failed")?;
        true
    } else {
        false
    };

    Ok(MigrationOutcome {
        created,
        source_deleted,
    })
}

fn repair_one_archive(
    config: &RecorderConfig,
    video: &Path,
    verification_path: &Path,
    session: &SessionSummary,
) -> Result<RepairOutcome> {
    if let Ok(verification) = verify_archive_video(
        config,
        video,
        video,
        session,
        DeckArchiveFormat::Mp4,
        ReferenceFramePolicy::Any,
    ) {
        write_archive_verification(video, verification_path, session, verification)?;
        return Ok(RepairOutcome::Reverified);
    }

    let stem = video
        .file_stem()
        .and_then(|value| value.to_str())
        .context("managed MP4 has no UTF-8 stem")?;
    let partial = video.with_file_name(format!(".{stem}.repair.partial.mp4"));
    remove_stale_partial(&partial)?;
    if let Err(error) = create_archive_video(config, video, &partial, DeckArchiveFormat::Mp4, false)
    {
        let _ = remove_stale_partial(&partial);
        return Err(error);
    }
    let verification = match verify_archive_video(
        config,
        &partial,
        video,
        session,
        DeckArchiveFormat::Mp4,
        ReferenceFramePolicy::Any,
    ) {
        Ok(verification) => verification,
        Err(error) => {
            let _ = remove_stale_partial(&partial);
            return Err(error);
        }
    };
    if let Err(error) = publish_archive_file(&partial, video) {
        let _ = remove_stale_partial(&partial);
        return Err(error);
    }
    write_archive_verification(video, verification_path, session, verification)?;
    Ok(RepairOutcome::Repaired)
}

fn archive_mkv_candidates(rule: &DeckArchiveRule) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&rule.destination_dir)
        .with_context(|| format!("failed to read {}", rule.destination_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("mkv")
            || path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with('.') && name.ends_with(".partial.mkv"))
        {
            continue;
        }
        candidates.push(path);
    }
    Ok(candidates)
}

fn archive_mp4_candidates(rule: &DeckArchiveRule) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&rule.destination_dir)
        .with_context(|| format!("failed to read {}", rule.destination_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("mp4")
            && !path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with('.') && name.contains(".partial.mp4"))
        {
            candidates.push(path);
        }
    }
    Ok(candidates)
}

fn repair_session_summary(
    rule: &DeckArchiveRule,
    video: &Path,
    verification_path: &Path,
) -> Result<SessionSummary> {
    let verification: ArchiveVerification =
        serde_json::from_reader(File::open(verification_path).with_context(|| {
            format!(
                "missing verification receipt {}",
                verification_path.display()
            )
        })?)
        .with_context(|| format!("failed to parse {}", verification_path.display()))?;
    if verification.expected_frame_count == 0 {
        bail!("verification receipt has no expected source frames");
    }
    if !verification.session_start_unix_seconds.is_finite()
        || verification.session_start_unix_seconds <= 0.0
    {
        bail!("verification receipt has an invalid session start");
    }
    let metadata = fs::metadata(video)?;
    if verification.video_size_bytes != metadata.len()
        || verification.video_modified_unix_nanos != modified_unix_nanos(&metadata)
        || !verification.full_decode_passed
        || verification.verification_outcome != VerificationOutcome::Verified
    {
        bail!("verification receipt is not bound to the current archive file");
    }
    let game_id = verification
        .session_id
        .strip_prefix("fg_")
        .or_else(|| verification.session_id.strip_prefix("desktop_"))
        .and_then(|value| value.split_once('_'))
        .map(|(game_id, _)| game_id)
        .filter(|game_id| !game_id.is_empty() && game_id.bytes().all(|byte| byte.is_ascii_digit()))
        .context("verification receipt has an invalid Steam recording ID")?
        .to_owned();
    Ok(SessionSummary {
        id: verification.session_id,
        game_id,
        start_timestamp: verification.session_start_unix_seconds,
        frame_count: verification.expected_frame_count,
        status: ExternalCaptureStatus::Complete,
        directory: rule.destination_dir.clone(),
        video_source: Some(video.to_path_buf()),
    })
}

/// Build migration metadata from the archived source itself.
///
/// Current archives have a strict manifest that agrees with their configured
/// rule. A small number of older, deliberately labeled diagnostic archives do
/// not: some use an earlier game ID or a degraded companion manifest, and a
/// crash-recovery sample has no manifest at all. Migration still has a strong
/// media contract for those files by counting the source MKV packets before
/// remuxing and binding that count into the v2 receipt.
fn migration_session_summary(
    config: &RecorderConfig,
    rule: &DeckArchiveRule,
    source: &Path,
) -> Result<SessionSummary> {
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .context("archive MKV has no UTF-8 stem")?;
    let source_media = probe_media(config, source, true)?;
    validate_container(DeckArchiveFormat::Matroska, &source_media.format_name)?;
    if source_media.video_stream_count == 0 {
        bail!("source contains no video stream");
    }
    if source_media.video_stream_count != 1 {
        bail!(
            "source must contain exactly one video stream, found {}",
            source_media.video_stream_count
        );
    }
    if source_media.audio_stream_count == 0 {
        bail!("source contains no audio stream");
    }
    if source_media.audio_stream_count != 1 {
        bail!(
            "source must contain exactly one audio stream, found {}",
            source_media.audio_stream_count
        );
    }
    let source_packet_count = source_media
        .video_packet_count
        .context("ffprobe did not report the source video packet count")?;
    let manifest_path = source.with_file_name(format!("{stem}.manifest.json"));
    if manifest_path.is_file() {
        let manifest: SessionManifest = serde_json::from_reader(
            File::open(&manifest_path)
                .with_context(|| format!("failed to open {}", manifest_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        let external = manifest
            .external_capture
            .as_ref()
            .context("archived manifest has no external capture metadata")?;
        if manifest.frame_count != source_packet_count {
            bail!(
                "source video packet count mismatch: manifest expected {}, got {source_packet_count}",
                manifest.frame_count
            );
        }
        return Ok(SessionSummary {
            id: external.recording_id.clone(),
            game_id: external.game_id.clone(),
            start_timestamp: manifest.timing.start_timestamp,
            frame_count: manifest.frame_count,
            status: external.status,
            directory: rule.destination_dir.clone(),
            video_source: Some(source.to_path_buf()),
        });
    }

    let session_id = stem
        .rsplit_once("__")
        .map(|(_, session_id)| session_id)
        .context("manifest-less archive filename has no RetroFeel session delimiter")?;
    let (game_id, start_timestamp) = historical_session_identity(session_id)?;
    Ok(SessionSummary {
        id: session_id.into(),
        game_id,
        start_timestamp,
        frame_count: source_packet_count,
        status: ExternalCaptureStatus::Complete,
        directory: rule.destination_dir.clone(),
        video_source: Some(source.to_path_buf()),
    })
}

fn historical_session_identity(session_id: &str) -> Result<(String, f64)> {
    let session = session_id
        .strip_prefix("fg_")
        .context("manifest-less archive does not contain an on-demand session ID")?;
    let (game_and_date, time) = session
        .rsplit_once('_')
        .context("manifest-less session ID has no UTC time")?;
    let (game_id, date) = game_and_date
        .rsplit_once('_')
        .context("manifest-less session ID has no UTC date")?;
    if game_id.is_empty() || !game_id.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("manifest-less session ID has a non-numeric game ID");
    }
    let recorded = NaiveDateTime::parse_from_str(&format!("{date}_{time}"), "%Y%m%d_%H%M%S")
        .context("manifest-less session ID has an invalid UTC timestamp")?;
    Ok((game_id.into(), recorded.and_utc().timestamp() as f64))
}

fn verify_existing_archive(
    config: &RecorderConfig,
    video: &Path,
    verification_path: &Path,
    session: &SessionSummary,
    format: DeckArchiveFormat,
    reference: Option<&Path>,
    reference_frame_policy: ReferenceFramePolicy,
) -> Result<()> {
    if current_archive_verification(video, verification_path, session, format)?.is_some() {
        return Ok(());
    }
    let reference = reference.ok_or_else(|| {
        anyhow!(
            "archive {} needs re-verification but its source is unavailable; file was retained",
            video.display()
        )
    })?;
    let verification = verify_archive_video(
        config,
        video,
        reference,
        session,
        format,
        reference_frame_policy,
    )
    .with_context(|| format!("existing archive {} was retained", video.display()))?;
    write_archive_verification(video, verification_path, session, verification)
}

pub(crate) fn current_archive_verification(
    video: &Path,
    verification_path: &Path,
    session: &SessionSummary,
    format: DeckArchiveFormat,
) -> Result<Option<ArchiveVerification>> {
    if !verification_path.is_file() || !video.is_file() {
        return Ok(None);
    }
    let verification: ArchiveVerification =
        match serde_json::from_reader(File::open(verification_path)?) {
            Ok(verification) => verification,
            Err(_) => return Ok(None),
        };
    let metadata = fs::metadata(video)?;
    let mp4_checks_match = format != DeckArchiveFormat::Mp4
        || (verification.fast_start == Some(true)
            && verification.video_codec == "h264"
            && verification.video_profile == "High"
            && matches!(verification.pixel_format.as_str(), "yuv420p" | "yuvj420p")
            && verification.video_frame_rate == "60/1"
            && verification.audio_codec == "aac"
            && verification.audio_profile == "LC"
            && verification.audio_sample_rate_hz == 48_000);
    let current = verification.format_version == ARCHIVE_RECEIPT_VERSION
        && verification.session_id == session.id
        && verification.session_start_unix_seconds == session.start_timestamp
        && verification.archive_format == format
        && verification.container == format.container_name()
        && verification.expected_frame_count == session.frame_count
        && verification.source_video_packet_count > 0
        && verification.video_packet_count > 0
        && verification.duration_seconds.is_finite()
        && verification.duration_seconds > 0.0
        && verification.duration_delta_seconds <= DURATION_TOLERANCE_SECONDS
        && verification.av_start_delta_seconds <= AV_START_TOLERANCE_SECONDS
        && verification.audio_present
        && mp4_checks_match
        && verification.full_decode_passed
        && verification.video_size_bytes == metadata.len()
        && verification.video_modified_unix_nanos == modified_unix_nanos(&metadata)
        && verification.verification_outcome == VerificationOutcome::Verified;
    Ok(current.then_some(verification))
}

#[derive(Debug)]
struct MediaVerification {
    container: String,
    video_codec: String,
    video_profile: String,
    pixel_format: String,
    video_frame_rate: String,
    audio_codec: String,
    audio_profile: String,
    audio_sample_rate_hz: u32,
    source_video_packet_count: u64,
    video_packet_count: u64,
    duration_seconds: f64,
    reference_duration_seconds: f64,
    duration_delta_seconds: f64,
    av_start_offset_seconds: f64,
    reference_av_start_offset_seconds: f64,
    av_start_delta_seconds: f64,
    audio_present: bool,
    fast_start: Option<bool>,
    edit_lists_absent: Option<bool>,
    full_decode_passed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceFramePolicy {
    ExactSession,
    Any,
}

fn verify_archive_video(
    config: &RecorderConfig,
    video: &Path,
    reference: &Path,
    session: &SessionSummary,
    format: DeckArchiveFormat,
    reference_frame_policy: ReferenceFramePolicy,
) -> Result<MediaVerification> {
    if !is_nonempty_file(video) {
        bail!("ffmpeg produced an empty archive");
    }
    let media = probe_media(config, video, true)?;
    let reference_media = probe_media(config, reference, true)?;
    validate_container(format, &media.format_name)?;
    if media.video_stream_count != 1 {
        bail!(
            "archive must contain exactly one video stream, found {}",
            media.video_stream_count
        );
    }
    if media.audio_stream_count != 1 {
        bail!(
            "archive must contain exactly one audio stream, found {}",
            media.audio_stream_count
        );
    }
    let packet_count = media
        .video_packet_count
        .context("ffprobe did not report a video packet count")?;
    let source_packet_count = reference_media
        .video_packet_count
        .context("ffprobe did not report the reference video packet count")?;
    if reference_frame_policy == ReferenceFramePolicy::ExactSession
        && source_packet_count != session.frame_count
    {
        bail!(
            "reference video packet count mismatch: expected {}, got {source_packet_count}",
            session.frame_count,
        );
    }
    // Container-level duration is not authoritative for Steam DASH. Its MPD
    // can expose a rounded period duration that differs materially from the
    // actual variable-rate packet timeline. Compare the first/last video
    // packet span on both sides instead.
    let duration_seconds = probe_video_packet_duration(config, video)?;
    let reference_duration_seconds = probe_video_packet_duration(config, reference)?;
    let duration_delta_seconds = (duration_seconds - reference_duration_seconds).abs();
    if duration_delta_seconds > DURATION_TOLERANCE_SECONDS {
        bail!(
            "archive duration mismatch: reference {reference_duration_seconds:.6}s, output {duration_seconds:.6}s (tolerance {DURATION_TOLERANCE_SECONDS:.3}s)"
        );
    }
    let av_start_offset_seconds = media
        .av_start_offset_seconds()
        .context("archive A/V start timestamps are unavailable")?;
    let reference_av_start_offset_seconds = reference_media
        .av_start_offset_seconds()
        .context("reference A/V start timestamps are unavailable")?;
    let av_start_delta_seconds =
        (av_start_offset_seconds - reference_av_start_offset_seconds).abs();
    if av_start_delta_seconds > AV_START_TOLERANCE_SECONDS {
        bail!(
            "archive A/V start offset changed: reference {reference_av_start_offset_seconds:.6}s, output {av_start_offset_seconds:.6}s (tolerance {AV_START_TOLERANCE_SECONDS:.3}s)"
        );
    }

    let (fast_start, edit_lists_absent) = if format == DeckArchiveFormat::Mp4 {
        validate_youtube_mp4_profile(&media, packet_count, reference_duration_seconds)?;
        let layout = inspect_mp4_layout(video)?;
        if !layout.fast_start {
            bail!("MP4 metadata atom is not before media data (Fast Start missing)");
        }
        (Some(true), Some(layout.edit_lists_absent))
    } else {
        if packet_count != session.frame_count {
            bail!(
                "archive video packet count mismatch: expected {}, got {packet_count}",
                session.frame_count,
            );
        }
        (None, None)
    };

    run_full_decode(config, video)?;
    Ok(MediaVerification {
        container: format.container_name().into(),
        video_codec: media.video_codec.context("video codec is unavailable")?,
        video_profile: media.video_profile.unwrap_or_default(),
        pixel_format: media.video_pixel_format.unwrap_or_default(),
        video_frame_rate: media.video_average_frame_rate.unwrap_or_default(),
        audio_codec: media.audio_codec.context("audio codec is unavailable")?,
        audio_profile: media.audio_profile.unwrap_or_default(),
        audio_sample_rate_hz: media.audio_sample_rate_hz.unwrap_or_default(),
        source_video_packet_count: source_packet_count,
        video_packet_count: packet_count,
        duration_seconds,
        reference_duration_seconds,
        duration_delta_seconds,
        av_start_offset_seconds,
        reference_av_start_offset_seconds,
        av_start_delta_seconds,
        audio_present: true,
        fast_start,
        edit_lists_absent,
        full_decode_passed: true,
    })
}

fn validate_youtube_mp4_profile(
    media: &MediaProbe,
    packet_count: u64,
    reference_duration_seconds: f64,
) -> Result<()> {
    if media.video_codec.as_deref() != Some("h264") {
        bail!("YouTube MP4 video codec must be H.264");
    }
    if media.video_profile.as_deref() != Some("High") {
        bail!("YouTube MP4 video profile must be H.264 High");
    }
    if !matches!(
        media.video_pixel_format.as_deref(),
        Some("yuv420p" | "yuvj420p")
    ) {
        bail!("YouTube MP4 pixel format must use 4:2:0 chroma subsampling");
    }
    if media.video_nominal_frame_rate.as_deref() != Some("60/1")
        || media.video_average_frame_rate.as_deref() != Some("60/1")
    {
        bail!("YouTube MP4 video must have an exact 60/1 CFR cadence");
    }
    let expected_packets = (reference_duration_seconds * YOUTUBE_FRAME_RATE as f64).round() as u64;
    if packet_count.abs_diff(expected_packets) > 1 {
        bail!(
            "YouTube MP4 frame count does not match its 60 fps duration: expected about {expected_packets}, got {packet_count}"
        );
    }
    if media.audio_codec.as_deref() != Some("aac")
        || media.audio_profile.as_deref() != Some("LC")
        || media.audio_sample_rate_hz != Some(48_000)
    {
        bail!("YouTube MP4 audio must be AAC-LC at 48 kHz");
    }
    Ok(())
}

fn create_archive_video(
    config: &RecorderConfig,
    source: &Path,
    output: &Path,
    format: DeckArchiveFormat,
    split_steam_inputs: bool,
) -> Result<()> {
    let source_media = probe_media(config, source, false)?;
    if source_media.video_stream_count == 0 {
        bail!("source contains no video stream");
    }
    if source_media.audio_stream_count == 0 {
        bail!("source contains no audio stream");
    }

    let mut command = Command::new(&config.ffmpeg);
    command.args(["-v", "error", "-i"]).arg(source);
    if split_steam_inputs {
        // Steam's DASH demuxer can stop the video adaptation set at the audio
        // boundary when both are mapped from one input. Independent inputs
        // preserve every variable-rate video packet and the mixed audio track.
        command.args(["-i"]).arg(source);
        command.args(["-map", "0:v:0", "-map", "1:a:0"]);
    } else {
        command.args(["-map", "0:v:0", "-map", "0:a:0"]);
    }
    command.args(["-map_metadata", "-1", "-map_chapters", "-1"]);

    match format {
        DeckArchiveFormat::Matroska => {
            command.args(["-c", "copy", "-f", "matroska"]);
        }
        DeckArchiveFormat::Mp4 => {
            command.args([
                "-vf",
                "fps=60",
                "-c:v",
                "libx264",
                "-preset",
                "medium",
                "-crf",
                "18",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-fps_mode:v",
                "cfr",
                "-tag:v",
                "avc1",
                "-c:a",
                "aac",
                "-profile:a",
                "aac_low",
                "-b:a",
                "192k",
                "-ar",
                "48000",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ]);
        }
    }
    command.arg("-y").arg(output).stdout(Stdio::null());
    let result = command
        .output()
        .with_context(|| format!("failed to start ffmpeg for {}", source.display()))?;
    if !result.status.success() {
        bail!(
            "ffmpeg failed for {}: {}",
            source.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeDocument {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: FfprobeFormat,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_type: String,
    codec_name: Option<String>,
    profile: Option<String>,
    pix_fmt: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    sample_rate: Option<String>,
    start_time: Option<String>,
    nb_read_packets: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    format_name: String,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobePacketDocument {
    #[serde(default)]
    packets: Vec<FfprobePacket>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobePacket {
    pts_time: Option<String>,
    dts_time: Option<String>,
    duration_time: Option<String>,
}

#[derive(Debug)]
struct MediaProbe {
    format_name: String,
    video_stream_count: usize,
    audio_stream_count: usize,
    video_codec: Option<String>,
    video_profile: Option<String>,
    video_pixel_format: Option<String>,
    video_nominal_frame_rate: Option<String>,
    video_average_frame_rate: Option<String>,
    audio_codec: Option<String>,
    audio_profile: Option<String>,
    audio_sample_rate_hz: Option<u32>,
    video_packet_count: Option<u64>,
    video_start_seconds: Option<f64>,
    audio_start_seconds: Option<f64>,
}

impl MediaProbe {
    fn av_start_offset_seconds(&self) -> Option<f64> {
        Some(self.video_start_seconds? - self.audio_start_seconds?)
    }
}

fn probe_media(config: &RecorderConfig, path: &Path, count_packets: bool) -> Result<MediaProbe> {
    let mut command = Command::new(&config.ffprobe);
    command.args(["-v", "error"]);
    if count_packets {
        command.arg("-count_packets");
    }
    let output = command
        .args([
            "-show_entries",
            "format=format_name:stream=codec_type,codec_name,profile,pix_fmt,r_frame_rate,avg_frame_rate,sample_rate,start_time,nb_read_packets",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("failed to start ffprobe for {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let document: FfprobeDocument = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("invalid ffprobe JSON for {}", path.display()))?;
    let video_streams = document
        .streams
        .iter()
        .filter(|stream| stream.codec_type == "video")
        .collect::<Vec<_>>();
    let audio_streams = document
        .streams
        .iter()
        .filter(|stream| stream.codec_type == "audio")
        .collect::<Vec<_>>();
    let video = video_streams.first().copied();
    Ok(MediaProbe {
        format_name: document.format.format_name,
        video_stream_count: video_streams.len(),
        audio_stream_count: audio_streams.len(),
        video_codec: video.and_then(|stream| stream.codec_name.clone()),
        video_profile: video.and_then(|stream| stream.profile.clone()),
        video_pixel_format: video.and_then(|stream| stream.pix_fmt.clone()),
        video_nominal_frame_rate: video.and_then(|stream| stream.r_frame_rate.clone()),
        video_average_frame_rate: video.and_then(|stream| stream.avg_frame_rate.clone()),
        audio_codec: audio_streams
            .first()
            .and_then(|stream| stream.codec_name.clone()),
        audio_profile: audio_streams
            .first()
            .and_then(|stream| stream.profile.clone()),
        audio_sample_rate_hz: audio_streams
            .first()
            .and_then(|stream| stream.sample_rate.as_deref())
            .and_then(|value| value.parse().ok()),
        video_packet_count: video
            .and_then(|stream| stream.nb_read_packets.as_deref())
            .and_then(|value| value.parse().ok()),
        video_start_seconds: video.and_then(|stream| parse_finite(stream.start_time.as_deref())),
        audio_start_seconds: audio_streams
            .first()
            .and_then(|stream| parse_finite(stream.start_time.as_deref())),
    })
}

fn probe_video_packet_duration(config: &RecorderConfig, path: &Path) -> Result<f64> {
    let output = Command::new(&config.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,dts_time,duration_time",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("failed to probe packet timeline for {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "ffprobe packet timeline failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let document: FfprobePacketDocument = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("invalid ffprobe packet JSON for {}", path.display()))?;
    packet_timeline_duration(&document.packets)
        .with_context(|| format!("video duration is unavailable for {}", path.display()))
}

fn packet_timeline_duration(packets: &[FfprobePacket]) -> Result<f64> {
    let mut first_timestamp = f64::INFINITY;
    let mut final_timestamp = f64::NEG_INFINITY;
    for packet in packets {
        let Some(timestamp) = parse_finite(packet.pts_time.as_deref())
            .or_else(|| parse_finite(packet.dts_time.as_deref()))
        else {
            continue;
        };
        let duration = parse_nonnegative_finite(packet.duration_time.as_deref()).unwrap_or(0.0);
        first_timestamp = first_timestamp.min(timestamp);
        final_timestamp = final_timestamp.max(timestamp + duration);
    }
    let duration = final_timestamp - first_timestamp;
    if !duration.is_finite() || duration <= 0.0 {
        bail!("packet timeline has no positive duration");
    }
    Ok(duration)
}

fn parse_finite(value: Option<&str>) -> Option<f64> {
    value
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn parse_nonnegative_finite(value: Option<&str>) -> Option<f64> {
    parse_finite(value).filter(|value| *value >= 0.0)
}

fn validate_container(format: DeckArchiveFormat, probed: &str) -> Result<()> {
    let names = probed.split(',').collect::<Vec<_>>();
    let valid = match format {
        DeckArchiveFormat::Matroska => names.contains(&"matroska"),
        DeckArchiveFormat::Mp4 => names.contains(&"mp4"),
    };
    if !valid {
        bail!(
            "archive container mismatch: expected {}, ffprobe reported {probed}",
            format.container_name()
        );
    }
    Ok(())
}

fn run_full_decode(config: &RecorderConfig, video: &Path) -> Result<()> {
    let output = Command::new(&config.ffmpeg)
        .args(["-v", "error", "-xerror", "-i"])
        .arg(video)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .stdout(Stdio::null())
        .output()
        .with_context(|| format!("failed to start full decode for {}", video.display()))?;
    if !output.status.success() {
        bail!(
            "full decode failed for {}: {}",
            video.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Mp4Layout {
    fast_start: bool,
    edit_lists_absent: bool,
}

fn inspect_mp4_layout(path: &Path) -> Result<Mp4Layout> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut offset = 0_u64;
    let mut moov = None;
    let mut mdat = None;
    while offset < file_len {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header)
            .with_context(|| format!("truncated MP4 atom at byte {offset}"))?;
        let size32 = u32::from_be_bytes(header[..4].try_into().expect("four-byte atom size"));
        let atom_type: [u8; 4] = header[4..8].try_into().expect("four-byte atom type");
        let (size, header_len) = match size32 {
            0 => (file_len - offset, 8_u64),
            1 => {
                let mut extended = [0_u8; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16_u64)
            }
            value => (u64::from(value), 8_u64),
        };
        if size < header_len || offset.saturating_add(size) > file_len {
            bail!("invalid MP4 atom size at byte {offset}");
        }
        match &atom_type {
            b"moov" => moov = Some((offset, size, header_len)),
            b"mdat" => {
                mdat.get_or_insert(offset);
            }
            _ => {}
        }
        offset = offset
            .checked_add(size)
            .context("MP4 atom offset overflow")?;
    }
    let (moov_offset, moov_size, moov_header_len) = moov.context("MP4 has no moov atom")?;
    let mdat_offset = mdat.context("MP4 has no mdat atom")?;
    let payload_len: usize = (moov_size - moov_header_len)
        .try_into()
        .context("MP4 moov atom is too large to inspect")?;
    let mut payload = vec![0_u8; payload_len];
    file.seek(SeekFrom::Start(moov_offset + moov_header_len))?;
    file.read_exact(&mut payload)?;
    Ok(Mp4Layout {
        fast_start: moov_offset < mdat_offset,
        edit_lists_absent: !contains_bounded_atom(&payload, b"edts")
            && !contains_bounded_atom(&payload, b"elst"),
    })
}

fn contains_bounded_atom(data: &[u8], atom_type: &[u8; 4]) -> bool {
    data.windows(4).enumerate().any(|(index, window)| {
        if window != atom_type || index < 4 {
            return false;
        }
        let size = u32::from_be_bytes(
            data[index - 4..index]
                .try_into()
                .expect("four-byte atom size window"),
        ) as usize;
        size >= 8 && index - 4 + size <= data.len()
    })
}

fn write_archive_verification(
    video: &Path,
    verification_path: &Path,
    session: &SessionSummary,
    media: MediaVerification,
) -> Result<()> {
    let metadata = fs::metadata(video)?;
    let verification = ArchiveVerification {
        format_version: ARCHIVE_RECEIPT_VERSION,
        session_id: session.id.clone(),
        session_start_unix_seconds: session.start_timestamp,
        archive_format: if media.fast_start.is_some() {
            DeckArchiveFormat::Mp4
        } else {
            DeckArchiveFormat::Matroska
        },
        container: media.container,
        video_codec: media.video_codec,
        video_profile: media.video_profile,
        pixel_format: media.pixel_format,
        video_frame_rate: media.video_frame_rate,
        audio_codec: media.audio_codec,
        audio_profile: media.audio_profile,
        audio_sample_rate_hz: media.audio_sample_rate_hz,
        expected_frame_count: session.frame_count,
        source_video_packet_count: media.source_video_packet_count,
        video_packet_count: media.video_packet_count,
        duration_seconds: media.duration_seconds,
        reference_duration_seconds: media.reference_duration_seconds,
        duration_delta_seconds: media.duration_delta_seconds,
        av_start_offset_seconds: media.av_start_offset_seconds,
        reference_av_start_offset_seconds: media.reference_av_start_offset_seconds,
        av_start_delta_seconds: media.av_start_delta_seconds,
        audio_present: media.audio_present,
        fast_start: media.fast_start,
        edit_lists_absent: media.edit_lists_absent,
        full_decode_passed: media.full_decode_passed,
        video_size_bytes: metadata.len(),
        video_modified_unix_nanos: modified_unix_nanos(&metadata),
        verification_outcome: VerificationOutcome::Verified,
    };
    write_json_atomic(verification_path, &verification)
}

fn write_json_atomic<T: Serialize>(destination: &Path, value: &T) -> Result<()> {
    let partial = partial_sidecar_path(destination)?;
    remove_stale_partial(&partial)?;
    fs::write(&partial, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("failed to stage {}", destination.display()))?;
    sync_file(&partial)?;
    fs::rename(&partial, destination)
        .with_context(|| format!("failed to publish {}", destination.display()))
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

fn sync_sidecars(source_dir: &Path, destination_dir: &Path, stem: &str) -> Result<usize> {
    let mut copied = 0;
    for (source_name, archive_suffix) in ARCHIVE_SIDECARS {
        let source = source_dir.join(source_name);
        if !source.is_file() {
            continue;
        }
        let destination = destination_dir.join(format!("{stem}.{archive_suffix}"));
        if files_equal(&source, &destination)? {
            continue;
        }
        let partial = partial_sidecar_path(&destination)?;
        remove_stale_partial(&partial)?;
        fs::copy(&source, &partial).with_context(|| {
            format!(
                "failed to stage archive sidecar {} from {}",
                partial.display(),
                source.display()
            )
        })?;
        sync_file(&partial)?;
        fs::rename(&partial, &destination).with_context(|| {
            format!(
                "failed to publish archive sidecar {}",
                destination.display()
            )
        })?;
        copied += 1;
    }
    Ok(copied)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    if !right.is_file() {
        return Ok(false);
    }
    let left_metadata = fs::metadata(left)?;
    let right_metadata = fs::metadata(right)?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    Ok(fs::read(left)? == fs::read(right)?)
}

fn has_retention_tombstone(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let value: serde_json::Value = serde_json::from_reader(File::open(path)?)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("prune_tombstone_at_unix_seconds")
        .is_some_and(|value| !value.is_null()))
}

fn is_nonempty_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn remove_stale_partial(path: &Path) -> Result<()> {
    if path.is_file() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove stale partial {}", path.display()))?;
    } else if path.exists() {
        bail!("partial archive path is not a file: {}", path.display());
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

fn publish_archive_file(partial: &Path, destination: &Path) -> Result<()> {
    sync_file(partial)?;
    fs::rename(partial, destination).with_context(|| {
        format!(
            "failed to publish archive {} from {}",
            destination.display(),
            partial.display()
        )
    })
}

fn partial_sidecar_path(destination: &Path) -> Result<PathBuf> {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("archive sidecar destination has no UTF-8 file name")?;
    Ok(destination.with_file_name(format!(".{file_name}.partial")))
}

pub(crate) fn archive_stem(rule: &DeckArchiveRule, session_id: &str) -> String {
    format!(
        "{}__{}",
        safe_component(&rule.display_name),
        safe_component(session_id)
    )
}

fn safe_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            output.push(character);
            separator = false;
        } else if !separator && !output.is_empty() {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "Recording".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retrofeel_types::{
        CaptureProvenance, CaptureSourceKind, CoreInfo, ExternalCaptureInfo, ExternalCaptureKind,
        ExternalVideoClock, ManifestFormat, TimingInfo, TrackAlignment, TrackAlignmentMetadata,
        TrackAlignmentStatus, TrackPresence, TranscriptionJobState, VideoTiming, VideoTimingKind,
    };

    fn rule(destination_dir: PathBuf) -> DeckArchiveRule {
        DeckArchiveRule {
            game_id: "9223372041183297536".into(),
            display_name: "Example Meadow World".into(),
            destination_dir,
            format: DeckArchiveFormat::Mp4,
        }
    }

    fn session(directory: PathBuf) -> SessionSummary {
        SessionSummary {
            id: "fg_9223372041183297536_20260824_010203".into(),
            game_id: "9223372041183297536".into(),
            start_timestamp: 1_777_000_000.0,
            frame_count: 60,
            status: ExternalCaptureStatus::Complete,
            directory,
            video_source: None,
        }
    }

    fn verified_media(format: DeckArchiveFormat, frame_count: u64) -> MediaVerification {
        MediaVerification {
            container: format.container_name().into(),
            video_codec: "h264".into(),
            video_profile: "High".into(),
            pixel_format: "yuv420p".into(),
            video_frame_rate: "60/1".into(),
            audio_codec: "aac".into(),
            audio_profile: "LC".into(),
            audio_sample_rate_hz: 48_000,
            source_video_packet_count: frame_count,
            video_packet_count: frame_count,
            duration_seconds: 1.0,
            reference_duration_seconds: 1.0,
            duration_delta_seconds: 0.0,
            av_start_offset_seconds: 0.0,
            reference_av_start_offset_seconds: 0.0,
            av_start_delta_seconds: 0.0,
            audio_present: true,
            fast_start: (format == DeckArchiveFormat::Mp4).then_some(true),
            edit_lists_absent: (format == DeckArchiveFormat::Mp4).then_some(false),
            full_decode_passed: true,
        }
    }

    #[test]
    fn archive_matching_is_exact_and_complete_only() {
        let temporary = tempfile::tempdir().unwrap();
        let rule = rule(temporary.path().join("archive"));
        let mut session = session(temporary.path().join("session"));
        assert!(archive_matches(&rule, &session));

        session.game_id = "9223372058363166720".into();
        assert!(!archive_matches(&rule, &session));
        session.game_id = rule.game_id.clone();
        session.status = ExternalCaptureStatus::DegradedAlignment;
        assert!(!archive_matches(&rule, &session));
    }

    #[test]
    fn archive_names_are_human_readable_and_path_safe() {
        let temporary = tempfile::tempdir().unwrap();
        let rule = DeckArchiveRule {
            game_id: "1".into(),
            display_name: "Variant Hunter / Pilot".into(),
            destination_dir: temporary.path().to_path_buf(),
            format: DeckArchiveFormat::Mp4,
        };
        assert_eq!(
            archive_stem(&rule, "fg_1/2026:08:24"),
            "Variant-Hunter-Pilot__fg_1-2026-08-24"
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_lock_rejects_an_overlapping_reconciliation() {
        let temporary = tempfile::tempdir().unwrap();
        let first = ArchiveSyncLock::try_acquire(temporary.path())
            .unwrap()
            .expect("first archive lock");
        assert!(ArchiveSyncLock::try_acquire(temporary.path())
            .unwrap()
            .is_none());
        drop(first);
        assert!(ArchiveSyncLock::try_acquire(temporary.path())
            .unwrap()
            .is_some());
    }

    #[test]
    fn receipt_is_bound_to_format_size_and_modification_time() {
        let temporary = tempfile::tempdir().unwrap();
        let video = temporary.path().join("archive.mp4");
        let receipt = temporary.path().join("archive.archive.json");
        fs::write(&video, b"video").unwrap();
        let session = session(temporary.path().to_path_buf());
        write_archive_verification(
            &video,
            &receipt,
            &session,
            verified_media(DeckArchiveFormat::Mp4, session.frame_count),
        )
        .unwrap();
        assert!(
            current_archive_verification(&video, &receipt, &session, DeckArchiveFormat::Mp4)
                .unwrap()
                .is_some()
        );
        assert!(current_archive_verification(
            &video,
            &receipt,
            &session,
            DeckArchiveFormat::Matroska
        )
        .unwrap()
        .is_none());

        fs::write(&video, b"different video size").unwrap();
        assert!(
            current_archive_verification(&video, &receipt, &session, DeckArchiveFormat::Mp4)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn repair_accepts_a_bound_historical_desktop_receipt() {
        let temporary = tempfile::tempdir().unwrap();
        let rule = rule(temporary.path().to_path_buf());
        let video = temporary
            .path()
            .join("Variant-Hunter_degraded__fg_9223372058363166720_20260823_040011.mp4");
        let receipt = video.with_extension("archive.json");
        fs::write(&video, b"verified historical video").unwrap();
        let mut session = session(temporary.path().to_path_buf());
        session.id = "desktop_9223372058363166720_1787457610".into();
        write_archive_verification(
            &video,
            &receipt,
            &session,
            verified_media(DeckArchiveFormat::Mp4, session.frame_count),
        )
        .unwrap();

        let repaired = repair_session_summary(&rule, &video, &receipt).unwrap();

        assert_eq!(repaired.id, session.id);
        assert_eq!(repaired.game_id, "9223372058363166720");
    }

    #[test]
    fn bounded_atom_detection_ignores_unbounded_payload_text() {
        let mut data = vec![0_u8; 24];
        data[4..8].copy_from_slice(b"edts");
        assert!(!contains_bounded_atom(&data, b"edts"));
        data[..4].copy_from_slice(&12_u32.to_be_bytes());
        assert!(contains_bounded_atom(&data, b"edts"));
    }

    #[test]
    fn packet_timeline_supplies_duration_when_a_container_omits_it() {
        let packets = vec![
            FfprobePacket {
                pts_time: Some("0.200000".into()),
                duration_time: Some("0.100000".into()),
                ..Default::default()
            },
            FfprobePacket {
                pts_time: Some("0.000000".into()),
                duration_time: Some("0.100000".into()),
                ..Default::default()
            },
        ];

        assert!((packet_timeline_duration(&packets).unwrap() - 0.3).abs() < f64::EPSILON);
        assert!(packet_timeline_duration(&[]).is_err());
    }

    #[test]
    fn migration_dry_run_never_modifies_managed_mkv() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let session = session(archive_dir.clone());
        let stem = archive_stem(&rule, &session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        fs::write(&mkv, b"not decoded during dry run").unwrap();
        write_archived_manifest(&archive_dir.join(format!("{stem}.manifest.json")), &session);
        let config = RecorderConfig {
            recordings_dir: temporary.path().join("sessions"),
            recording_archives: vec![rule],
            ..RecorderConfig::default()
        };

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, true).unwrap();
        assert_eq!(report.discovered_mkvs, 1);
        assert_eq!(report.planned_migrations, 1);
        assert_eq!(report.deleted_mkvs, 0);
        assert!(mkv.is_file());
        assert!(!archive_dir.join(format!("{stem}.mp4")).exists());
    }

    #[test]
    fn migration_dry_run_includes_legacy_labeled_mkvs() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let current =
            archive_dir.join("Example-Meadow-World__fg_9223372041183297536_20260824_010203.mkv");
        let legacy = archive_dir
            .join("Example-Workshop-Pilot_legacy__fg_9223372054068199424_20260822_222138.mkv");
        let partial = archive_dir
            .join(".Example-Meadow-World__fg_9223372041183297536_20260824_010203.partial.mkv");
        fs::write(&current, b"dry run does not decode").unwrap();
        fs::write(&legacy, b"dry run does not decode").unwrap();
        fs::write(&partial, b"interrupted archive staging file").unwrap();
        let config = RecorderConfig {
            recordings_dir: temporary.path().join("sessions"),
            recording_archives: vec![rule(archive_dir)],
            ..RecorderConfig::default()
        };

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, true).unwrap();

        assert_eq!(report.discovered_mkvs, 2);
        assert_eq!(report.planned_migrations, 2);
        assert_eq!(report.retained_mkvs, 2);
        assert!(current.is_file());
        assert!(legacy.is_file());
        assert!(partial.is_file());
    }

    #[test]
    fn repair_dry_run_reports_legacy_profile_without_writing() {
        let Some(config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 10;
        let stem = archive_stem(&rule, &session.id);
        let video = archive_dir.join(format!("{stem}.mp4"));
        let receipt = archive_dir.join(format!("{stem}.archive.json"));
        make_media_fixture(&config, &video, true, true).unwrap();
        write_archive_verification(
            &video,
            &receipt,
            &session,
            verified_media(DeckArchiveFormat::Mp4, session.frame_count),
        )
        .unwrap();
        set_receipt_version(&receipt, 2);
        let original = fs::read(&video).unwrap();
        let config = RecorderConfig {
            recordings_dir: temporary.path().join("sessions"),
            recording_archives: vec![rule],
            ..config
        };

        let report = repair_recording_archives(&config, true).unwrap();

        assert_eq!(report.discovered_mp4s, 1);
        assert_eq!(report.planned_repairs, 1);
        assert_eq!(report.repaired_mp4s, 0);
        assert_eq!(fs::read(video).unwrap(), original);
        let verification: ArchiveVerification =
            serde_json::from_reader(File::open(receipt).unwrap()).unwrap();
        assert_eq!(verification.format_version, 2);
    }

    #[test]
    fn repair_reencodes_and_atomically_replaces_a_legacy_mp4() {
        let Some(config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 10;
        let stem = archive_stem(&rule, &session.id);
        let video = archive_dir.join(format!("{stem}.mp4"));
        let receipt = archive_dir.join(format!("{stem}.archive.json"));
        let sidecar = archive_dir.join(format!("{stem}.transcript.srt"));
        make_media_fixture(&config, &video, true, true).unwrap();
        write_archive_verification(
            &video,
            &receipt,
            &session,
            verified_media(DeckArchiveFormat::Mp4, session.frame_count),
        )
        .unwrap();
        set_receipt_version(&receipt, 2);
        fs::write(&sidecar, b"preserved").unwrap();
        let config = RecorderConfig {
            recordings_dir: temporary.path().join("sessions"),
            recording_archives: vec![rule],
            ..config
        };

        let report = repair_recording_archives(&config, false).unwrap();

        assert!(!report.has_failures());
        assert_eq!(report.repaired_mp4s, 1);
        assert_eq!(fs::read(sidecar).unwrap(), b"preserved");
        assert!(!archive_dir
            .join(format!(".{stem}.repair.partial.mp4"))
            .exists());
        assert!(
            current_archive_verification(&video, &receipt, &session, DeckArchiveFormat::Mp4,)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn repair_only_reverifies_an_already_current_profile() {
        let Some(config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 10;
        let stem = archive_stem(&rule, &session.id);
        let source = archive_dir.join("source.mkv");
        let video = archive_dir.join(format!("{stem}.mp4"));
        let receipt = archive_dir.join(format!("{stem}.archive.json"));
        make_media_fixture(&config, &source, true, true).unwrap();
        create_archive_video(&config, &source, &video, DeckArchiveFormat::Mp4, false).unwrap();
        let verification = verify_archive_video(
            &config,
            &video,
            &source,
            &session,
            DeckArchiveFormat::Mp4,
            ReferenceFramePolicy::ExactSession,
        )
        .unwrap();
        write_archive_verification(&video, &receipt, &session, verification).unwrap();
        set_receipt_version(&receipt, 2);
        let video_metadata = fs::metadata(&video).unwrap();
        let config = RecorderConfig {
            recordings_dir: temporary.path().join("sessions"),
            recording_archives: vec![rule],
            ..config
        };

        let report = repair_recording_archives(&config, false).unwrap();

        assert!(!report.has_failures());
        assert_eq!(report.reverified_mp4s, 1);
        assert_eq!(report.repaired_mp4s, 0);
        assert_eq!(fs::metadata(&video).unwrap().len(), video_metadata.len());
        assert_eq!(
            modified_unix_nanos(&fs::metadata(&video).unwrap()),
            modified_unix_nanos(&video_metadata),
        );
        assert!(
            current_archive_verification(&video, &receipt, &session, DeckArchiveFormat::Mp4,)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn successful_migration_verifies_mp4_before_deleting_only_the_mkv() {
        let Some(mut config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 10;
        let stem = archive_stem(&rule, &session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        let mp4 = archive_dir.join(format!("{stem}.mp4"));
        let receipt = archive_dir.join(format!("{stem}.archive.json"));
        let manifest = archive_dir.join(format!("{stem}.manifest.json"));
        let transcript = archive_dir.join(format!("{stem}.transcript.srt"));
        let controller_map = archive_dir.join(format!("{stem}.controller-map.json"));
        make_media_fixture(&config, &mkv, true, true).unwrap();
        write_archived_manifest(&manifest, &session);
        fs::write(&transcript, b"preserved transcript").unwrap();
        fs::write(&controller_map, b"preserved controller map").unwrap();
        config.recordings_dir = temporary.path().join("sessions");
        config.recording_archives.push(rule);

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();

        assert!(!report.has_failures());
        assert_eq!(report.migrated_mp4s, 1);
        assert_eq!(report.deleted_mkvs, 1);
        assert!(!mkv.exists());
        assert!(mp4.is_file());
        assert_eq!(fs::read(&transcript).unwrap(), b"preserved transcript");
        assert_eq!(
            fs::read(&controller_map).unwrap(),
            b"preserved controller map"
        );
        assert!(manifest.is_file());
        assert!(
            current_archive_verification(&mp4, &receipt, &session, DeckArchiveFormat::Mp4)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn migration_accepts_a_legacy_label_and_manifest_game_id() {
        let Some(mut config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut legacy_session = session(archive_dir.clone());
        legacy_session.id = "fg_9223372054068199424_20260822_222138".into();
        legacy_session.game_id = "9223372054068199424".into();
        legacy_session.frame_count = 10;
        let stem = format!("Example-Workshop-Pilot_legacy__{}", legacy_session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        let mp4 = archive_dir.join(format!("{stem}.mp4"));
        let receipt = archive_dir.join(format!("{stem}.archive.json"));
        make_media_fixture(&config, &mkv, true, true).unwrap();
        write_archived_manifest(
            &archive_dir.join(format!("{stem}.manifest.json")),
            &legacy_session,
        );
        config.recordings_dir = temporary.path().join("sessions");
        config.recording_archives.push(rule);

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();

        assert!(!report.has_failures());
        assert_eq!(report.migrated_mp4s, 1);
        assert_eq!(report.deleted_mkvs, 1);
        assert!(!mkv.exists());
        assert!(mp4.is_file());
        assert!(current_archive_verification(
            &mp4,
            &receipt,
            &legacy_session,
            DeckArchiveFormat::Mp4,
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn migration_uses_source_packets_for_a_manifestless_crash_archive() {
        let Some(mut config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let stem = "Variant-Hunter_crash-partial__fg_9223372058363166720_20260823_035315";
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        let mp4 = archive_dir.join(format!("{stem}.mp4"));
        let receipt = archive_dir.join(format!("{stem}.archive.json"));
        make_media_fixture(&config, &mkv, true, true).unwrap();
        config.recordings_dir = temporary.path().join("sessions");
        config.recording_archives.push(rule);

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();

        assert!(!report.has_failures());
        assert_eq!(report.migrated_mp4s, 1);
        assert_eq!(report.deleted_mkvs, 1);
        assert!(!mkv.exists());
        assert!(mp4.is_file());
        let verification: ArchiveVerification =
            serde_json::from_reader(File::open(receipt).unwrap()).unwrap();
        assert_eq!(
            verification.session_id,
            "fg_9223372058363166720_20260823_035315"
        );
        assert_eq!(verification.expected_frame_count, 10);
        assert_eq!(verification.source_video_packet_count, 10);
        // AAC priming can add one final CFR frame with different FFmpeg releases.
        assert!((60..=61).contains(&verification.video_packet_count));
        assert!(verification.session_start_unix_seconds > 0.0);
    }

    #[test]
    fn destination_collision_retains_original_mkv() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let session = session(archive_dir.clone());
        let stem = archive_stem(&rule, &session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        fs::write(&mkv, b"source").unwrap();
        write_archived_manifest(&archive_dir.join(format!("{stem}.manifest.json")), &session);
        fs::create_dir(archive_dir.join(format!("{stem}.mp4"))).unwrap();
        let config = RecorderConfig {
            recordings_dir: temporary.path().join("sessions"),
            recording_archives: vec![rule],
            ..RecorderConfig::default()
        };

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.deleted_mkvs, 0);
        assert!(mkv.is_file());
    }

    #[test]
    fn retention_tombstone_prevents_archive_recreation() {
        let temporary = tempfile::tempdir().unwrap();
        let session_dir = temporary.path().join("session");
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(session_dir);
        let source = temporary.path().join("still-available.mpd");
        fs::write(&source, b"source that must not be read").unwrap();
        session.video_source = Some(source);
        let stem = archive_stem(&rule, &session.id);
        fs::write(
            archive_dir.join(format!("{stem}.youtube.json")),
            br#"{"prune_tombstone_at_unix_seconds":1777000000}"#,
        )
        .unwrap();

        let outcome = archive_session(&RecorderConfig::default(), &rule, &session).unwrap();
        assert!(matches!(outcome, ArchiveOutcome::Pruned { sidecars: 0 }));
        assert!(!archive_dir.join(format!("{stem}.mp4")).exists());
    }

    #[test]
    fn youtube_mp4_is_fast_start_and_constant_60_fps() {
        let Some(config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.mkv");
        make_media_fixture(&config, &source, true, true).unwrap();
        let output = temporary.path().join("output.mp4");
        create_archive_video(&config, &source, &output, DeckArchiveFormat::Mp4, false).unwrap();
        let mut session = session(temporary.path().to_path_buf());
        session.frame_count = 10;
        let verified = verify_archive_video(
            &config,
            &output,
            &source,
            &session,
            DeckArchiveFormat::Mp4,
            ReferenceFramePolicy::ExactSession,
        )
        .unwrap();

        assert_eq!(verified.video_codec, "h264");
        assert_eq!(verified.audio_codec, "aac");
        assert_eq!(verified.video_profile, "High");
        assert!(matches!(
            verified.pixel_format.as_str(),
            "yuv420p" | "yuvj420p"
        ));
        assert_eq!(verified.video_frame_rate, "60/1");
        assert_eq!(verified.audio_profile, "LC");
        assert_eq!(verified.audio_sample_rate_hz, 48_000);
        assert_eq!(verified.source_video_packet_count, 10);
        // AAC priming can add one final CFR frame with different FFmpeg releases.
        assert!((60..=61).contains(&verified.video_packet_count));
        assert_eq!(verified.fast_start, Some(true));
        assert_eq!(verified.edit_lists_absent, Some(false));
        assert!(verified.av_start_delta_seconds <= AV_START_TOLERANCE_SECONDS);
    }

    #[test]
    fn incompatible_streams_fall_back_to_h264_and_aac() {
        let Some(config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.mkv");
        make_media_fixture(&config, &source, false, true).unwrap();
        let output = temporary.path().join("output.mp4");
        create_archive_video(&config, &source, &output, DeckArchiveFormat::Mp4, false).unwrap();
        let mut session = session(temporary.path().to_path_buf());
        session.frame_count = 10;
        let verified = verify_archive_video(
            &config,
            &output,
            &source,
            &session,
            DeckArchiveFormat::Mp4,
            ReferenceFramePolicy::ExactSession,
        )
        .unwrap();

        assert_eq!(verified.video_codec, "h264");
        assert_eq!(verified.audio_codec, "aac");
        assert_eq!(verified.source_video_packet_count, 10);
        // AAC priming can add one final CFR frame with different FFmpeg releases.
        assert!((60..=61).contains(&verified.video_packet_count));
    }

    #[test]
    fn packet_mismatch_retains_mkv_and_removes_partial_mp4() {
        let Some(mut config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 11;
        let stem = archive_stem(&rule, &session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        make_media_fixture(&config, &mkv, true, true).unwrap();
        write_archived_manifest(&archive_dir.join(format!("{stem}.manifest.json")), &session);
        config.recordings_dir = temporary.path().join("sessions");
        config.recording_archives.push(rule);

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("packet count mismatch"));
        assert!(mkv.is_file());
        assert!(!archive_dir.join(format!("{stem}.mp4")).exists());
        assert!(!archive_dir.join(format!(".{stem}.partial.mp4")).exists());
    }

    #[test]
    fn missing_audio_retains_mkv() {
        let Some(mut config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 10;
        let stem = archive_stem(&rule, &session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        make_media_fixture(&config, &mkv, true, false).unwrap();
        write_archived_manifest(&archive_dir.join(format!("{stem}.manifest.json")), &session);
        config.recordings_dir = temporary.path().join("sessions");
        config.recording_archives.push(rule);

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("no audio stream"));
        assert!(mkv.is_file());
        assert!(!archive_dir.join(format!("{stem}.mp4")).exists());
    }

    #[test]
    fn failed_full_decode_retains_mkv() {
        let Some(mut config) = media_config() else {
            return;
        };
        let temporary = tempfile::tempdir().unwrap();
        let archive_dir = temporary.path().join("archive");
        fs::create_dir_all(&archive_dir).unwrap();
        let rule = rule(archive_dir.clone());
        let mut session = session(archive_dir.clone());
        session.frame_count = 10;
        let stem = archive_stem(&rule, &session.id);
        let mkv = archive_dir.join(format!("{stem}.mkv"));
        let mp4 = archive_dir.join(format!("{stem}.mp4"));
        make_media_fixture(&config, &mkv, true, true).unwrap();
        create_archive_video(&config, &mkv, &mp4, DeckArchiveFormat::Mp4, false).unwrap();
        write_archived_manifest(&archive_dir.join(format!("{stem}.manifest.json")), &session);
        config.recordings_dir = temporary.path().join("sessions");
        config.recording_archives.push(rule);
        config.ffmpeg = PathBuf::from("/bin/false");

        let report =
            migrate_recording_archives(&config, DeckArchiveFormat::Mp4, true, false).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("full decode failed"));
        assert!(mkv.is_file());
        assert!(mp4.is_file());
    }

    #[test]
    fn failed_atomic_rename_leaves_source_and_removes_no_file() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.mkv");
        let partial = temporary.path().join(".source.partial.mp4");
        let destination = temporary.path().join("source.mp4");
        fs::write(&source, b"original").unwrap();
        fs::write(&partial, b"candidate").unwrap();
        fs::create_dir(&destination).unwrap();

        assert!(publish_archive_file(&partial, &destination).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"original");
        assert!(partial.is_file());
    }

    fn media_config() -> Option<RecorderConfig> {
        let available = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
            && Command::new("ffprobe")
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
        if !available {
            eprintln!("ffmpeg/ffprobe unavailable; skipping media fixture test");
            return None;
        }
        Some(RecorderConfig {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            ..RecorderConfig::default()
        })
    }

    fn set_receipt_version(path: &Path, version: u32) {
        let mut value: serde_json::Value =
            serde_json::from_reader(File::open(path).unwrap()).unwrap();
        value["format_version"] = version.into();
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn make_media_fixture(
        config: &RecorderConfig,
        output: &Path,
        compatible_codecs: bool,
        with_audio: bool,
    ) -> Result<()> {
        let mut command = Command::new(&config.ffmpeg);
        command.args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=10:duration=1",
        ]);
        if with_audio {
            command.args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
            ]);
        } else {
            command.args(["-map", "0:v:0"]);
        }
        if compatible_codecs {
            command.args(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
            if with_audio {
                command.args(["-c:a", "aac"]);
            }
        } else {
            command.args(["-c:v", "mpeg4"]);
            if with_audio {
                command.args(["-c:a", "pcm_s16le"]);
            }
        }
        let result = command
            .args(["-shortest", "-f", "matroska"])
            .arg(output)
            .output()?;
        if !result.status.success() {
            bail!(
                "failed to generate media fixture: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }
        Ok(())
    }

    fn write_archived_manifest(path: &Path, session: &SessionSummary) {
        let aligned = TrackAlignment {
            presence: TrackPresence::Present,
            status: TrackAlignmentStatus::Complete,
            offset_us: Some(0),
            uncertainty_us: Some(0),
            clock_source: Some("test".into()),
        };
        let manifest = SessionManifest {
            core: CoreInfo {
                name: "Steam Game Recording".into(),
                version: "external".into(),
                library_path: String::new(),
            },
            rom: None,
            timing: TimingInfo {
                fps: 60.0,
                sample_rate: 48_000.0,
                start_timestamp: session.start_timestamp,
            },
            initial_state: None,
            frame_count: session.frame_count,
            pause_segments: Vec::new(),
            input_log: "input.json".into(),
            video: None,
            mic_audio: None,
            transcript: None,
            transcript_json: None,
            transcription_status: TranscriptionJobState::NotRequested,
            transcription_provider: None,
            transcription_model: None,
            binding_map: None,
            capture_provenance: Some(CaptureProvenance {
                kind: CaptureSourceKind::SteamGameRecording,
                game_audio_policy: Some("Steam mixed audio".into()),
                source_description: None,
            }),
            video_timing: Some(VideoTiming {
                kind: VideoTimingKind::ExternalVariableFrameRate,
                output_fps: None,
                source_clock: Some("CLOCK_BOOTTIME".into()),
            }),
            frame_map: None,
            input_transitions: None,
            track_alignment: Some(TrackAlignmentMetadata {
                video: aligned.clone(),
                narration: aligned.clone(),
                game_audio: aligned,
            }),
            external_capture: Some(ExternalCaptureInfo {
                kind: ExternalCaptureKind::SteamGameRecording,
                game_id: session.game_id.clone(),
                recording_id: session.id.clone(),
                clip_id: None,
                timeline_id: None,
                source_video: None,
                video_clock: Some(ExternalVideoClock {
                    video_pts_zero_boottime_us: 0,
                    source_pts_us: 0,
                    normalized_pts_us: 0,
                }),
                controller_map: None,
                raw_event_log: None,
                audio_transcript: None,
                audio_transcript_json: None,
                audio_transcript_source: None,
                audio_transcription_status: TranscriptionJobState::NotRequested,
                audio_transcription_provider: None,
                audio_transcription_model: None,
                status: ExternalCaptureStatus::Complete,
            }),
            dropped_frames: 0,
            format: ManifestFormat::Json,
        };
        fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }
}
