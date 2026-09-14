//! Lightweight synchronized evidence viewer for `.feel` recordings.
//!
//! FFmpeg supplies seekable preview frames while ffplay supplies the package's
//! existing audio track.  Keeping decoding outside Bevy avoids adding a second
//! native media stack to the application and works with the MKV recordings
//! produced by both RetroFeel and Steam Game Recording.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::image::ImageSampler;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use retrofeel_feel::{analyze_package, AgentAdapterKind, AnalyzeOptions};

use crate::recording::find_media_tool;
use crate::ui::{
    FrontendModel, RecordingPlaybackLabel, RecordingTimeLabel, RecordingTimelineLabel, UiAction,
    UiActionRequest,
};

const PREVIEW_INTERVAL: Duration = Duration::from_millis(400);

type RecordingTimelineLabels<'w, 's> =
    Query<'w, 's, &'static mut Text, (With<RecordingTimelineLabel>, Without<RecordingTimeLabel>)>;
type RecordingPlaybackLabels<'w, 's> = Query<
    'w,
    's,
    &'static mut Text,
    (
        With<RecordingPlaybackLabel>,
        Without<RecordingTimeLabel>,
        Without<RecordingTimelineLabel>,
    ),
>;

#[derive(Debug)]
struct PreviewRequest {
    video: PathBuf,
    position_ms: u64,
}

#[derive(Debug)]
struct PreviewResult {
    video: PathBuf,
    position_ms: u64,
    png: Result<Vec<u8>, String>,
}

/// Playback state shared by the recording-detail UI and update systems.
#[derive(Resource)]
pub struct RecordingViewer {
    pub preview: Handle<Image>,
    package: Option<PathBuf>,
    video: Option<PathBuf>,
    duration_ms: u64,
    position_ms: u64,
    playing_since: Option<(Instant, u64)>,
    audio: Option<Child>,
    requested_at: Option<Instant>,
    last_requested_ms: Option<u64>,
    requests: Sender<PreviewRequest>,
    results: Receiver<PreviewResult>,
}

impl RecordingViewer {
    pub fn new(images: &mut Assets<Image>) -> Self {
        let mut placeholder = Image::new_fill(
            Extent3d {
                width: 16,
                height: 9,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[12, 15, 22, 255],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        placeholder.sampler = ImageSampler::linear();
        let preview = images.add(placeholder);
        let (request_tx, request_rx) = bounded::<PreviewRequest>(1);
        let (result_tx, result_rx) = bounded::<PreviewResult>(1);
        std::thread::Builder::new()
            .name("retrofeel-feel-preview".into())
            .spawn(move || preview_worker(request_rx, result_tx))
            .expect("failed to start .feel preview worker");
        Self {
            preview,
            package: None,
            video: None,
            duration_ms: 0,
            position_ms: 0,
            playing_since: None,
            audio: None,
            requested_at: None,
            last_requested_ms: None,
            requests: request_tx,
            results: result_rx,
        }
    }

    pub fn position_seconds(&self) -> f64 {
        self.current_position_ms() as f64 / 1_000.0
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_ms as f64 / 1_000.0
    }

    pub fn is_playing(&self) -> bool {
        self.playing_since.is_some()
    }

    fn current_position_ms(&self) -> u64 {
        self.playing_since
            .map(|(started, origin)| {
                origin
                    .saturating_add(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
            })
            .unwrap_or(self.position_ms)
            .min(self.duration_ms)
    }

    fn open(&mut self, package: PathBuf, video: Option<PathBuf>, duration_seconds: f64) {
        self.stop_audio();
        self.package = Some(package);
        self.video = video;
        self.duration_ms = (duration_seconds.max(0.0) * 1_000.0).round() as u64;
        self.position_ms = 0;
        self.playing_since = None;
        self.last_requested_ms = None;
        self.request_preview(true);
    }

    fn clear(&mut self) {
        self.stop_audio();
        self.package = None;
        self.video = None;
        self.duration_ms = 0;
        self.position_ms = 0;
        self.playing_since = None;
        self.last_requested_ms = None;
    }

    fn toggle_playback(&mut self) -> Result<(), String> {
        if self.is_playing() {
            self.position_ms = self.current_position_ms();
            self.playing_since = None;
            self.stop_audio();
            self.request_preview(true);
            return Ok(());
        }
        let Some(video) = self.video.as_ref() else {
            return Err("This recording has no playable video".into());
        };
        if self.duration_ms > 0 && self.position_ms >= self.duration_ms {
            self.position_ms = 0;
        }
        let executable = find_media_tool("ffplay")
            .ok_or_else(|| "ffplay is required for recording audio playback".to_string())?;
        let child = Command::new(executable)
            .args(["-v", "error", "-nodisp", "-autoexit", "-ss"])
            .arg(format!("{:.3}", self.position_ms as f64 / 1_000.0))
            .arg("-i")
            .arg(video)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start ffplay: {error}"))?;
        self.audio = Some(child);
        self.playing_since = Some((Instant::now(), self.position_ms));
        Ok(())
    }

    fn seek_to(&mut self, position_ms: u64) -> Result<(), String> {
        let was_playing = self.is_playing();
        self.stop_audio();
        self.playing_since = None;
        self.position_ms = position_ms.min(self.duration_ms);
        self.last_requested_ms = None;
        self.request_preview(true);
        if was_playing {
            self.toggle_playback()?;
        }
        Ok(())
    }

    fn seek_relative(&mut self, delta_ms: i64) -> Result<(), String> {
        let current = i128::from(self.current_position_ms());
        let target = (current + i128::from(delta_ms)).clamp(0, i128::from(self.duration_ms));
        self.seek_to(target as u64)
    }

    fn stop_audio(&mut self) {
        if let Some(mut child) = self.audio.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn request_preview(&mut self, force: bool) {
        let Some(video) = self.video.as_ref() else {
            return;
        };
        let position_ms = self.current_position_ms();
        if !force
            && (self
                .requested_at
                .is_some_and(|at| at.elapsed() < PREVIEW_INTERVAL)
                || self
                    .last_requested_ms
                    .is_some_and(|last| last.abs_diff(position_ms) < 250))
        {
            return;
        }
        match self.requests.try_send(PreviewRequest {
            video: video.clone(),
            position_ms,
        }) {
            Ok(()) => {
                self.requested_at = Some(Instant::now());
                self.last_requested_ms = Some(position_ms);
            }
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                log::error!(".feel preview worker disconnected")
            }
        }
    }
}

impl Drop for RecordingViewer {
    fn drop(&mut self) {
        self.stop_audio();
    }
}

#[derive(Resource, Default)]
pub struct PendingFeelAnalysis {
    receiver: Option<Receiver<AnalysisJobResult>>,
}

struct AnalysisJobResult {
    package: PathBuf,
    adapter: AgentAdapterKind,
    result: Result<(), String>,
}

pub fn install_viewer(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    commands.insert_resource(RecordingViewer::new(&mut images));
}

pub fn sync_selected_recording(model: Res<FrontendModel>, mut viewer: ResMut<RecordingViewer>) {
    let selected = model.selected_recording.as_ref();
    if selected == viewer.package.as_ref() {
        return;
    }
    let Some(selected) = selected else {
        viewer.clear();
        return;
    };
    let Some(recording) = model
        .recordings
        .iter()
        .find(|entry| &entry.path == selected)
    else {
        viewer.clear();
        return;
    };
    let video = recording.manifest.as_ref().and_then(|manifest| {
        manifest
            .video
            .as_deref()
            .map(|value| resolve_artifact(&recording.path, value))
            .filter(|path| path.is_file())
    });
    viewer.open(recording.path.clone(), video, recording.duration_seconds);
}

pub fn handle_viewer_actions(
    mut requests: MessageReader<UiActionRequest>,
    mut viewer: ResMut<RecordingViewer>,
    mut analysis: ResMut<PendingFeelAnalysis>,
    mut model: ResMut<FrontendModel>,
) {
    for request in requests.read() {
        let result = match &request.action {
            UiAction::ToggleRecordingPlayback => viewer.toggle_playback(),
            UiAction::SeekRecordingRelative(delta_ms) => viewer.seek_relative(*delta_ms),
            UiAction::SeekRecordingTo(position_ms) => viewer.seek_to(*position_ms),
            UiAction::AnalyzeRecording { package, adapter } => {
                if analysis.receiver.is_some() {
                    Err("An analysis is already running".into())
                } else {
                    let package = package.clone();
                    let adapter = *adapter;
                    let (sender, receiver) = bounded(1);
                    match std::thread::Builder::new()
                        .name(format!("retrofeel-feel-analysis-{}", adapter.id()))
                        .spawn(move || {
                            let result = analyze_package(
                                &package,
                                &AnalyzeOptions {
                                    adapter,
                                    ..Default::default()
                                },
                            )
                            .map(|_| ())
                            .map_err(|error| error.to_string());
                            let _ = sender.send(AnalysisJobResult {
                                package,
                                adapter,
                                result,
                            });
                        }) {
                        Ok(_) => {
                            analysis.receiver = Some(receiver);
                            model.status = format!("Analyzing package with {adapter}…");
                            Ok(())
                        }
                        Err(error) => Err(format!("Could not start analysis: {error}")),
                    }
                }
            }
            _ => continue,
        };
        if let Err(error) = result {
            model.status = error;
        }
    }
}

pub fn poll_analysis(
    mut commands: Commands,
    mut analysis: ResMut<PendingFeelAnalysis>,
    mut model: ResMut<FrontendModel>,
    icons: Res<crate::icons::IconAssets>,
    viewer: Res<RecordingViewer>,
    roots: Query<Entity, With<crate::ui::UiRoot>>,
) {
    let Some(receiver) = analysis.receiver.as_ref() else {
        return;
    };
    let job = match receiver.try_recv() {
        Ok(job) => job,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => {
            analysis.receiver = None;
            model.status = "Analysis worker stopped unexpectedly".into();
            return;
        }
    };
    analysis.receiver = None;
    match job.result {
        Ok(()) => {
            if let Some(updated) = crate::ui::load_recording_entry(job.package.clone()) {
                if let Some(existing) = model
                    .recordings
                    .iter_mut()
                    .find(|entry| entry.path == job.package)
                {
                    *existing = updated;
                }
            }
            model.status = format!("{} analysis completed", job.adapter);
            for entity in &roots {
                commands.entity(entity).despawn();
            }
            crate::ui::spawn_library(&mut commands, &model, &icons, &viewer);
        }
        Err(error) => model.status = format!("{} analysis failed: {error}", job.adapter),
    }
}

pub fn update_viewer(
    mut viewer: ResMut<RecordingViewer>,
    mut images: ResMut<Assets<Image>>,
    model: Res<FrontendModel>,
    mut time_labels: Query<&mut Text, With<RecordingTimeLabel>>,
    mut timeline_labels: RecordingTimelineLabels,
    mut playback_labels: RecordingPlaybackLabels,
) {
    let position_ms = viewer.current_position_ms();
    if viewer.is_playing() {
        viewer.position_ms = position_ms;
        if position_ms >= viewer.duration_ms {
            viewer.playing_since = None;
            viewer.stop_audio();
        }
    }
    viewer.request_preview(false);
    while let Ok(frame) = viewer.results.try_recv() {
        if viewer.video.as_ref() != Some(&frame.video) {
            continue;
        }
        match frame.png.and_then(decode_preview) {
            Ok(image) => {
                if let Some(existing) = images.get_mut(&viewer.preview) {
                    *existing = image;
                }
            }
            Err(error) => log::warn!(
                ".feel preview failed at {:.3}s: {error}",
                frame.position_ms as f64 / 1_000.0
            ),
        }
    }
    let position_seconds = position_ms as f64 / 1_000.0;
    for mut label in &mut time_labels {
        **label = format!(
            "{} / {}",
            timestamp(position_seconds),
            timestamp(viewer.duration_seconds())
        );
    }
    for mut label in &mut playback_labels {
        **label = if viewer.is_playing() {
            "Pause".into()
        } else {
            "Play".into()
        };
    }
    let cue = model
        .selected_recording
        .as_ref()
        .and_then(|selected| {
            model
                .recordings
                .iter()
                .find(|entry| &entry.path == selected)
        })
        .map(|recording| timeline_cue(recording, position_seconds))
        .unwrap_or_default();
    for mut label in &mut timeline_labels {
        **label = cue.clone();
    }
}

fn timeline_cue(recording: &crate::ui::RecordingEntry, position: f64) -> String {
    let transcript = recording
        .transcript
        .as_ref()
        .and_then(|document| {
            document.segments.iter().find(|segment| {
                segment.start_seconds <= position && segment.end_seconds >= position
            })
        })
        .map(|segment| segment.text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("No transcript at this moment");
    let input = recording
        .input_changes
        .iter()
        .rev()
        .find(|change| change.elapsed_seconds <= position)
        .map(|change| format!("mapped {} · raw {}", change.mapped, change.raw))
        .unwrap_or_else(|| "no controller change yet".into());
    format!("Narration: {transcript}\nController: {input}")
}

fn preview_worker(requests: Receiver<PreviewRequest>, results: Sender<PreviewResult>) {
    let Some(ffmpeg) = find_media_tool("ffmpeg") else {
        return;
    };
    while let Ok(request) = requests.recv() {
        let output = Command::new(&ffmpeg)
            .args(["-v", "error", "-ss"])
            .arg(format!("{:.3}", request.position_ms as f64 / 1_000.0))
            .arg("-i")
            .arg(&request.video)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "scale=960:-2",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("could not run ffmpeg: {error}"))
            .and_then(|output| {
                if output.status.success() && !output.stdout.is_empty() {
                    Ok(output.stdout)
                } else {
                    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
                }
            });
        if results
            .send(PreviewResult {
                video: request.video,
                position_ms: request.position_ms,
                png: output,
            })
            .is_err()
        {
            break;
        }
    }
}

fn decode_preview(bytes: Vec<u8>) -> Result<Image, String> {
    let decoded = image::load_from_memory(&bytes)
        .map_err(|error| format!("invalid ffmpeg preview image: {error}"))?
        .to_rgba8();
    let mut image = Image::new(
        Extent3d {
            width: decoded.width(),
            height: decoded.height(),
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        decoded.into_raw(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.sampler = ImageSampler::linear();
    Ok(image)
}

fn resolve_artifact(package: &Path, value: &str) -> PathBuf {
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        candidate
            .file_name()
            .map(|name| package.join(name))
            .unwrap_or_else(|| package.to_path_buf())
    } else {
        package.join(candidate)
    }
}

fn timestamp(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3_600,
        (total / 60) % 60,
        total % 60
    )
}
