//! Offline transcription of Steam's completed mixed audio track.

use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use retrofeel_types::{
    ExternalAudioSource, ManifestFormat, SessionManifest, TranscriptionJobState,
    TranscriptionProvider,
};
use tempfile::Builder;

use crate::config::RecorderConfig;
use crate::session::{list_sessions, SessionSummary};

const MANIFEST_JSON: &str = "manifest.json";
const MANIFEST_RON: &str = "manifest.ron";
const TRANSCRIPT_SRT: &str = "steam-audio-transcript.srt";
const TRANSCRIPT_JSON: &str = "steam-audio-transcript.json";

pub fn transcribe_steam_audio(
    config: &RecorderConfig,
    session_id: &str,
    force: bool,
) -> Result<PathBuf> {
    let session = find_session(config, session_id)?;
    transcribe_session_directory(config, &session, force)
}

pub(crate) fn schedule_steam_audio_transcription(config: &RecorderConfig, directory: &Path) {
    if !config.transcription.automatic {
        return;
    }
    let config = config.clone();
    let directory = directory.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("retrofeel-deck-transcriber".into())
        .spawn(move || {
            let session = match session_summary_at(&directory) {
                Ok(session) => session,
                Err(error) => {
                    log::warn!(
                        "could not schedule Steam mixed-audio transcription for {}: {error:#}",
                        directory.display()
                    );
                    return;
                }
            };
            match transcribe_session_directory(&config, &session, false) {
                Ok(path) => log::info!("wrote Steam mixed-audio transcript at {}", path.display()),
                Err(error) => log::warn!(
                    "Steam mixed-audio transcription failed for {}: {error:#}",
                    session.id
                ),
            }
        });
}

fn transcribe_session_directory(
    config: &RecorderConfig,
    session: &SessionSummary,
    force: bool,
) -> Result<PathBuf> {
    let srt_path = session.directory.join(TRANSCRIPT_SRT);
    if !force && completed_transcript(&session.directory, &srt_path)? {
        return Ok(srt_path);
    }

    let executable = &config.transcription.executable;
    if !executable.is_file() {
        bail!(
            "whisper.cpp executable is missing: {}",
            executable.display()
        );
    }
    let model = config
        .transcription
        .model
        .as_deref()
        .ok_or_else(|| anyhow!("no whisper.cpp model is configured"))?;
    if !model.is_file() {
        bail!("whisper.cpp model is missing: {}", model.display());
    }
    let source = session
        .video_source
        .as_deref()
        .ok_or_else(|| anyhow!("session {} has no Steam audio/video source", session.id))?;
    if !source.is_file() {
        bail!("Steam recording source is missing: {}", source.display());
    }

    update_status(
        &session.directory,
        TranscriptionJobState::Running {
            progress_percent: 1,
        },
        None,
        None,
        config,
    )?;
    let result = run_transcription(config, session, source);
    match result {
        Ok((temporary_srt, temporary_json)) => {
            let json_path = session.directory.join(TRANSCRIPT_JSON);
            replace_file(&temporary_srt, &srt_path)?;
            let installed_json = temporary_json
                .as_deref()
                .filter(|path| path.is_file())
                .map(|path| {
                    replace_file(path, &json_path)?;
                    Ok::<_, anyhow::Error>(TRANSCRIPT_JSON.to_string())
                })
                .transpose()?;
            update_status(
                &session.directory,
                TranscriptionJobState::Complete,
                Some(TRANSCRIPT_SRT.into()),
                installed_json,
                config,
            )?;
            Ok(srt_path)
        }
        Err(error) => {
            let message = format!("{error:#}");
            let _ = update_status(
                &session.directory,
                TranscriptionJobState::Failed {
                    message: message.clone(),
                },
                None,
                None,
                config,
            );
            Err(anyhow!(message))
        }
    }
}

fn run_transcription(
    config: &RecorderConfig,
    session: &SessionSummary,
    source: &Path,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let wav = Builder::new()
        .prefix(".steam-audio-")
        .suffix(".wav")
        .tempfile_in(&session.directory)?;
    let ffmpeg = Command::new(&config.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .arg("-i")
        .arg(source)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(wav.path())
        .output()
        .context("failed to start ffmpeg for Steam audio extraction")?;
    if !ffmpeg.status.success() {
        bail!(
            "ffmpeg could not extract Steam audio: {}",
            String::from_utf8_lossy(&ffmpeg.stderr).trim()
        );
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let prefix = session.directory.join(format!(
        ".steam-audio-transcript-{}-{stamp}",
        std::process::id()
    ));
    let output = Command::new(&config.transcription.executable)
        .arg("--model")
        .arg(
            config
                .transcription
                .model
                .as_deref()
                .expect("model was validated before transcription"),
        )
        .arg("--file")
        .arg(wav.path())
        .args(["--output-srt", "--output-json", "--no-prints"])
        .arg("--output-file")
        .arg(&prefix)
        .arg("--language")
        .arg(&config.transcription.language)
        .arg("--threads")
        .arg(config.transcription.threads.max(1).to_string())
        .arg("--suppress-nst")
        .output()
        .context("failed to start whisper.cpp")?;
    if !output.status.success() {
        bail!(
            "whisper.cpp exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let temporary_srt = prefix.with_extension("srt");
    if !temporary_srt.is_file() {
        bail!("whisper.cpp did not produce an SRT file");
    }
    let temporary_json = prefix.with_extension("json");
    Ok((
        temporary_srt,
        temporary_json.is_file().then_some(temporary_json),
    ))
}

fn completed_transcript(directory: &Path, srt_path: &Path) -> Result<bool> {
    if !srt_path.is_file() {
        return Ok(false);
    }
    let manifest = read_manifest(directory)?;
    Ok(manifest.external_capture.is_some_and(|external| {
        external.audio_transcription_status == TranscriptionJobState::Complete
            && external.audio_transcript.as_deref() == Some(TRANSCRIPT_SRT)
    }))
}

fn update_status(
    directory: &Path,
    status: TranscriptionJobState,
    transcript: Option<String>,
    transcript_json: Option<String>,
    config: &RecorderConfig,
) -> Result<()> {
    let mut manifest = read_manifest(directory)?;
    let external = manifest
        .external_capture
        .as_mut()
        .context("session is not a Steam external capture")?;
    external.audio_transcription_status = status;
    external.audio_transcription_provider = Some(TranscriptionProvider::WhisperCpp);
    external.audio_transcription_model = Some(config.transcription.model_id.clone());
    if transcript.is_some() {
        external.audio_transcript = transcript;
        external.audio_transcript_json = transcript_json;
        external.audio_transcript_source = Some(ExternalAudioSource::SteamMixedAudio);
    }
    write_manifests(directory, &manifest)
}

fn read_manifest(directory: &Path) -> Result<SessionManifest> {
    let path = directory.join(MANIFEST_JSON);
    serde_json::from_reader(BufReader::new(
        File::open(&path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))
}

fn write_manifests(directory: &Path, manifest: &SessionManifest) -> Result<()> {
    let mut json_manifest = manifest.clone();
    json_manifest.format = ManifestFormat::Json;
    atomic_write(&directory.join(MANIFEST_JSON), |file| {
        serde_json::to_writer_pretty(file, &json_manifest)?;
        Ok(())
    })?;

    let mut ron_manifest = manifest.clone();
    ron_manifest.format = ManifestFormat::Ron;
    let ron = ron::ser::to_string_pretty(&ron_manifest, ron::ser::PrettyConfig::default())?;
    atomic_write(&directory.join(MANIFEST_RON), |file| {
        file.write_all(ron.as_bytes())?;
        Ok(())
    })
}

fn atomic_write(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let parent = path.parent().context("output path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    write(temporary.as_file_mut())?;
    temporary.as_file_mut().flush()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).with_context(|| {
        format!(
            "failed to install transcript {} at {}",
            source.display(),
            destination.display()
        )
    })
}

fn find_session(config: &RecorderConfig, id: &str) -> Result<SessionSummary> {
    list_sessions(config)?
        .into_iter()
        .find(|session| session.id == id)
        .ok_or_else(|| anyhow!("recording session not found: {id}"))
}

fn session_summary_at(directory: &Path) -> Result<SessionSummary> {
    let manifest = read_manifest(directory)?;
    let external = manifest
        .external_capture
        .context("session is not a Steam external capture")?;
    Ok(SessionSummary {
        id: external.recording_id,
        game_id: external.game_id,
        start_timestamp: manifest.timing.start_timestamp,
        frame_count: manifest.frame_count,
        status: external.status,
        directory: directory.to_path_buf(),
        video_source: external.source_video.map(PathBuf::from),
    })
}
