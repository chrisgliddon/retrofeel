//! Recordings index: the `recordings` table.
//!
//! Session metadata lives in the DB for fast library queries; the media files
//! (`.mkv`, `.wav`, `input.json`, `manifest.json`) stay on the filesystem —
//! they're large binaries and exporter-facing, not relational.

use std::path::PathBuf;

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq)]
pub struct RecordingRow {
    pub session_dir: PathBuf,
    pub core_name: String,
    pub core_version: String,
    pub rom_path: Option<PathBuf>,
    pub rom_sha1: Option<String>,
    pub frame_count: u64,
    pub dropped_frames: u64,
    pub fps: f64,
    pub sample_rate: f64,
    pub start_timestamp: f64,
    pub video_path: Option<PathBuf>,
    pub input_log_path: PathBuf,
    pub manifest_path: PathBuf,
    pub transcript_text: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub transcript_status: String,
    pub transcription_model: Option<String>,
    pub input_summary_json: Option<String>,
    pub source_kind: String,
    pub video_timing_mode: String,
    pub frame_map_path: Option<PathBuf>,
    pub input_transitions_path: Option<PathBuf>,
    pub narration_offset_us: Option<i64>,
    pub narration_uncertainty_us: Option<u64>,
    pub narration_status: String,
    pub narration_presence: String,
    pub game_audio_presence: String,
    pub created_at: u64,
}

pub struct RecordingsRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> RecordingsRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Insert a recording row.
    pub fn insert(&self, row: &RecordingRow) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO recordings (session_dir, core_name, core_version,
                    rom_path, rom_sha1, frame_count, dropped_frames, fps, sample_rate,
                    start_timestamp, video_path, input_log_path, manifest_path,
                    transcript_text, transcript_path, transcript_status, transcription_model,
                    input_summary_json, source_kind, video_timing_mode, frame_map_path,
                    input_transitions_path, narration_offset_us, narration_uncertainty_us,
                    narration_status, narration_presence, game_audio_presence, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
                rusqlite::params![
                    row.session_dir.to_string_lossy(),
                    row.core_name,
                    row.core_version,
                    row.rom_path.as_ref().map(|p| p.to_string_lossy()),
                    row.rom_sha1,
                    row.frame_count as i64,
                    row.dropped_frames as i64,
                    row.fps,
                    row.sample_rate,
                    row.start_timestamp,
                    row.video_path.as_ref().map(|p| p.to_string_lossy()),
                    row.input_log_path.to_string_lossy(),
                    row.manifest_path.to_string_lossy(),
                    row.transcript_text,
                    row.transcript_path.as_ref().map(|p| p.to_string_lossy()),
                    row.transcript_status,
                    row.transcription_model,
                    row.input_summary_json,
                    row.source_kind,
                    row.video_timing_mode,
                    row.frame_map_path.as_ref().map(|p| p.to_string_lossy()),
                    row.input_transitions_path
                        .as_ref()
                        .map(|p| p.to_string_lossy()),
                    row.narration_offset_us,
                    row.narration_uncertainty_us.map(|value| value as i64),
                    row.narration_status,
                    row.narration_presence,
                    row.game_audio_presence,
                    row.created_at as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// Load all recordings, newest first.
    pub fn all(&self) -> Result<Vec<RecordingRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT session_dir, core_name, core_version, rom_path, rom_sha1, frame_count,
                        dropped_frames, fps, sample_rate, start_timestamp, video_path,
                        input_log_path, manifest_path, transcript_text, transcript_path,
                        transcript_status, transcription_model, input_summary_json, created_at
                        , source_kind, video_timing_mode, frame_map_path, input_transitions_path,
                        narration_offset_us, narration_uncertainty_us, narration_status,
                        narration_presence, game_audio_presence
                 FROM recordings
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                let session_dir: String = row.get(0)?;
                let rom_path: Option<String> = row.get(3)?;
                let video_path: Option<String> = row.get(10)?;
                let input_log_path: String = row.get(11)?;
                let manifest_path: String = row.get(12)?;
                Ok(RecordingRow {
                    session_dir: PathBuf::from(session_dir),
                    core_name: row.get(1)?,
                    core_version: row.get(2)?,
                    rom_path: rom_path.map(PathBuf::from),
                    rom_sha1: row.get(4)?,
                    frame_count: row.get::<_, i64>(5)? as u64,
                    dropped_frames: row.get::<_, i64>(6)? as u64,
                    fps: row.get(7)?,
                    sample_rate: row.get(8)?,
                    start_timestamp: row.get(9)?,
                    video_path: video_path.map(PathBuf::from),
                    input_log_path: PathBuf::from(input_log_path),
                    manifest_path: PathBuf::from(manifest_path),
                    transcript_text: row.get(13)?,
                    transcript_path: row.get::<_, Option<String>>(14)?.map(PathBuf::from),
                    transcript_status: row.get(15)?,
                    transcription_model: row.get(16)?,
                    input_summary_json: row.get(17)?,
                    created_at: row.get::<_, i64>(18)? as u64,
                    source_kind: row.get(19)?,
                    video_timing_mode: row.get(20)?,
                    frame_map_path: row.get::<_, Option<String>>(21)?.map(PathBuf::from),
                    input_transitions_path: row.get::<_, Option<String>>(22)?.map(PathBuf::from),
                    narration_offset_us: row.get(23)?,
                    narration_uncertainty_us: row
                        .get::<_, Option<i64>>(24)?
                        .map(|value| value as u64),
                    narration_status: row.get(25)?,
                    narration_presence: row.get(26)?,
                    game_audio_presence: row.get(27)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Delete a recording row by session dir. (Does not delete media files —
    /// the caller is responsible for filesystem cleanup.)
    pub fn delete(&self, session_dir: &std::path::Path) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "DELETE FROM recordings WHERE session_dir = ?1",
                [session_dir.to_string_lossy()],
            )?;
            Ok(())
        })
    }
}
