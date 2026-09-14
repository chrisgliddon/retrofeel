use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use retrofeel_types::{
    input_transitions_from_frames, InputFrame, SessionManifest, TrackAlignment,
    TrackAlignmentMetadata, TrackAlignmentStatus, TrackPresence, TranscriptDocument,
    TranscriptionJobState, TranscriptionProvider,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    AlignmentOutcome, AlignmentQuality, AnalysisRun, FeelManifestV1, FeelResource,
    FeelResourceRole, PackageValidation, TranscriptAsset, TranscriptKind, FEEL_FORMAT_VERSION,
    FEEL_KIND,
};

#[derive(Debug, Error)]
pub enum FeelError {
    #[error("{0}")]
    Invalid(String),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

type Result<T> = std::result::Result<T, FeelError>;

#[derive(Debug, Clone, Default)]
pub struct PackageOptions {
    pub title: Option<String>,
    pub context_brief: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct FeelPackage {
    root: PathBuf,
    manifest: FeelManifestV1,
}

impl FeelPackage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(FeelError::Invalid(format!(
                "not a .feel directory package: {}",
                root.display()
            )));
        }
        let manifest_path = root.join("feel.json");
        let manifest: FeelManifestV1 = read_json(&manifest_path)?;
        if manifest.kind != FEEL_KIND {
            return Err(FeelError::Invalid(format!(
                "unsupported package kind {:?}",
                manifest.kind
            )));
        }
        if manifest.format_version != FEEL_FORMAT_VERSION {
            return Err(FeelError::Invalid(format!(
                "unsupported .feel format version {} (this build supports {})",
                manifest.format_version, FEEL_FORMAT_VERSION
            )));
        }
        ensure_safe_relative(Path::new(&manifest.session_manifest))?;
        ensure_safe_relative(Path::new(&manifest.context_brief))?;
        Ok(Self { root, manifest })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &FeelManifestV1 {
        &self.manifest
    }

    pub fn session_manifest_path(&self) -> PathBuf {
        self.root.join(&self.manifest.session_manifest)
    }

    pub fn session_manifest(&self) -> Result<SessionManifest> {
        read_json(&self.session_manifest_path())
    }

    pub fn resolve(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        ensure_safe_relative(relative)?;
        let path = self.root.join(relative);
        reject_symlink_components(&self.root, relative)?;
        Ok(path)
    }

    pub fn primary_transcript(&self) -> Option<&TranscriptAsset> {
        self.manifest
            .transcripts
            .iter()
            .find(|transcript| transcript.primary)
    }

    pub fn validate(&self) -> PackageValidation {
        let mut report = PackageValidation {
            package: self.root.display().to_string(),
            valid: true,
            checked_resources: 0,
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        for resource in &self.manifest.resources {
            match self.validate_file(&resource.path, &resource.sha256, Some(resource.byte_len)) {
                Ok(()) => report.checked_resources += 1,
                Err(error) if resource.required => report.errors.push(error.to_string()),
                Err(error) => report.warnings.push(error.to_string()),
            }
        }
        match self.validate_file(
            &self.manifest.context_brief,
            &self.manifest.context_brief_sha256,
            None,
        ) {
            Ok(()) => report.checked_resources += 1,
            Err(error) => report.errors.push(error.to_string()),
        }
        for transcript in &self.manifest.transcripts {
            for (path, hash) in [
                (Some(&transcript.srt_path), Some(&transcript.srt_sha256)),
                (
                    transcript.json_path.as_ref(),
                    transcript.json_sha256.as_ref(),
                ),
                (
                    transcript.alignment_path.as_ref(),
                    transcript.alignment_sha256.as_ref(),
                ),
            ] {
                if let (Some(path), Some(hash)) = (path, hash) {
                    match self.validate_file(path, hash, None) {
                        Ok(()) => report.checked_resources += 1,
                        Err(error) => report.errors.push(error.to_string()),
                    }
                }
            }
        }
        for run in &self.manifest.analyses {
            for (path, hash) in [
                (&run.result_path, &run.result_sha256),
                (&run.report_path, &run.report_sha256),
                (&run.action_plan_path, &run.action_plan_sha256),
                (&run.agent_path, &run.agent_sha256),
            ] {
                match self.validate_file(path, hash, None) {
                    Ok(()) => report.checked_resources += 1,
                    Err(error) => report.errors.push(error.to_string()),
                }
            }
        }
        if !self.root.join(&self.manifest.session_manifest).is_file() {
            report.errors.push("session manifest is unavailable".into());
        }
        report.valid = report.errors.is_empty();
        report
    }

    fn validate_file(&self, relative: &str, expected: &str, byte_len: Option<u64>) -> Result<()> {
        let path = self.resolve(relative)?;
        let metadata = fs::symlink_metadata(&path).map_err(|source| io(&path, source))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(FeelError::Invalid(format!(
                "package resource is not a regular file: {}",
                path.display()
            )));
        }
        if byte_len.is_some_and(|expected| expected != metadata.len()) {
            return Err(FeelError::Invalid(format!(
                "package resource size mismatch: {}",
                path.display()
            )));
        }
        let actual = sha256_file(&path)?;
        if actual != expected {
            return Err(FeelError::Invalid(format!(
                "package resource hash mismatch: {}",
                path.display()
            )));
        }
        Ok(())
    }

    pub(crate) fn register_analysis(&mut self, run: AnalysisRun) -> Result<()> {
        self.manifest.analyses.push(run);
        self.save_manifest()
    }

    pub(crate) fn save_manifest(&self) -> Result<()> {
        write_json_atomic(&self.root.join("feel.json"), &self.manifest)
    }

    fn refresh_capture_resources(&mut self) -> Result<()> {
        self.manifest.resources = inventory_capture_resources(&self.root)?;
        self.manifest.context_brief_sha256 =
            sha256_file(&self.root.join(&self.manifest.context_brief))?;
        self.save_manifest()
    }
}

pub fn create_package(
    session: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: &PackageOptions,
) -> Result<FeelPackage> {
    let session = session.as_ref();
    let destination = destination.as_ref();
    require_feel_suffix(destination)?;
    if !session.join("manifest.json").is_file() {
        return Err(FeelError::Invalid(format!(
            "session has no manifest.json: {}",
            session.display()
        )));
    }
    if destination.exists() {
        return Err(FeelError::Invalid(format!(
            "refusing to overwrite existing package: {}",
            destination.display()
        )));
    }
    if destination.starts_with(session) {
        return Err(FeelError::Invalid(
            "destination cannot be inside the source session".into(),
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        FeelError::Invalid(format!("package has no parent: {}", destination.display()))
    })?;
    fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    let partial = parent.join(format!(
        ".{}.partial-{}",
        destination
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("recording.feel"),
        random_id()
    ));
    fs::create_dir(&partial).map_err(|source| io(&partial, source))?;
    let result = (|| {
        copy_capture_tree(session, &partial)?;
        ensure_input_transitions(&partial)?;
        let manifest: SessionManifest = read_json(&partial.join("manifest.json"))?;
        let title = options
            .title
            .clone()
            .unwrap_or_else(|| package_title(&manifest));
        write_context_brief(&partial, &title, options.context_brief.as_deref())?;
        initialize_manifest(&partial, title)?;
        fs::rename(&partial, destination).map_err(|source| io(destination, source))?;
        FeelPackage::open(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result
}

/// Copy an already-valid `.feel` package into RetroFeel's library without
/// rewriting its contents. The source remains untouched and the destination
/// is published only after a second full validation succeeds.
pub fn import_package_copy(
    source: impl AsRef<Path>,
    library: impl AsRef<Path>,
) -> Result<FeelPackage> {
    let source = source.as_ref();
    let package = FeelPackage::open(source)?;
    let validation = package.validate();
    if !validation.valid {
        return Err(FeelError::Invalid(format!(
            "source package is invalid: {}",
            validation.errors.join("; ")
        )));
    }
    let library = library.as_ref();
    fs::create_dir_all(library).map_err(|error| io(library, error))?;
    let original_name = source
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.ends_with(".feel"))
        .unwrap_or("Imported Recording.feel");
    let mut destination = library.join(original_name);
    if destination.exists() {
        let stem = destination
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("Imported Recording");
        destination = library.join(format!("{stem}-{}.feel", &random_id()[..8]));
    }
    let partial = library.join(format!(".import-{}.partial", random_id()));
    fs::create_dir(&partial).map_err(|error| io(&partial, error))?;
    let result = (|| {
        copy_full_tree(source, &partial)?;
        let copied = FeelPackage::open(&partial)?;
        let validation = copied.validate();
        if !validation.valid {
            return Err(FeelError::Invalid(format!(
                "imported copy failed validation: {}",
                validation.errors.join("; ")
            )));
        }
        fs::rename(&partial, &destination).map_err(|error| io(&destination, error))?;
        FeelPackage::open(&destination)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result
}

pub fn initialize_native_package(
    root: impl AsRef<Path>,
    title: Option<String>,
) -> Result<FeelPackage> {
    let root = root.as_ref();
    require_feel_suffix(root)?;
    let session: SessionManifest = read_json(&root.join("manifest.json"))?;
    let title = title.unwrap_or_else(|| package_title(&session));
    if !root.join("context/brief.md").is_file() {
        write_context_brief(root, &title, None)?;
    }
    if root.join("feel.json").is_file() {
        let mut package = FeelPackage::open(root)?;
        package.refresh_capture_resources()?;
        sync_native_transcript(&mut package, &session)?;
        return Ok(package);
    }
    initialize_manifest(root, title)?;
    let mut package = FeelPackage::open(root)?;
    sync_native_transcript(&mut package, &session)?;
    Ok(package)
}

fn sync_native_transcript(package: &mut FeelPackage, session: &SessionManifest) -> Result<()> {
    let Some(srt_value) = session.transcript.as_deref() else {
        return Ok(());
    };
    let srt_path = resolve_manifest_path(package.root(), srt_value)?;
    if !srt_path.is_file() {
        return Ok(());
    }
    let srt_relative = srt_path
        .strip_prefix(package.root())
        .map(portable_path)
        .map_err(|_| FeelError::Invalid("native transcript escaped its package".into()))?;
    let json_path = session
        .transcript_json
        .as_deref()
        .map(|value| resolve_manifest_path(package.root(), value))
        .transpose()?
        .filter(|path| path.is_file());
    let json_relative = json_path
        .as_deref()
        .and_then(|path| path.strip_prefix(package.root()).ok().map(portable_path));
    for transcript in &mut package.manifest.transcripts {
        transcript.primary = false;
    }
    let asset = TranscriptAsset {
        id: "native-transcript".into(),
        kind: TranscriptKind::Generated,
        srt_path: srt_relative,
        srt_sha256: sha256_file(&srt_path)?,
        json_path: json_relative,
        json_sha256: json_path.as_deref().map(sha256_file).transpose()?,
        alignment_path: None,
        alignment_sha256: None,
        primary: true,
        source_description: Some("RetroFeel local microphone transcription".into()),
    };
    if let Some(existing) = package
        .manifest
        .transcripts
        .iter_mut()
        .find(|transcript| transcript.id == asset.id)
    {
        *existing = asset;
    } else {
        package.manifest.transcripts.push(asset);
    }
    package.save_manifest()
}

fn initialize_manifest(root: &Path, title: String) -> Result<()> {
    let context = "context/brief.md";
    let manifest = FeelManifestV1 {
        kind: FEEL_KIND.into(),
        format_version: FEEL_FORMAT_VERSION,
        package_id: random_id(),
        title,
        created_at_epoch_seconds: now_epoch_seconds(),
        session_manifest: "manifest.json".into(),
        context_brief: context.into(),
        context_brief_sha256: sha256_file(&root.join(context))?,
        resources: inventory_capture_resources(root)?,
        transcripts: Vec::new(),
        analyses: Vec::new(),
    };
    write_json_atomic(&root.join("feel.json"), &manifest)
}

pub fn import_aligned_transcript(
    package_root: impl AsRef<Path>,
    source_srt: impl AsRef<Path>,
    outcome: &AlignmentOutcome,
    source_description: Option<String>,
) -> Result<TranscriptAsset> {
    let mut package = FeelPackage::open(package_root)?;
    let source_srt = source_srt.as_ref();
    let source_bytes = fs::read(source_srt).map_err(|source| io(source_srt, source))?;
    let stem = source_srt
        .file_stem()
        .and_then(OsStr::to_str)
        .map(safe_name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "external-transcript".into());
    let revision = format!("{}-{}", stem, random_id());
    let source_relative = format!("transcripts/source/{revision}.srt");
    let aligned_dir = format!("transcripts/aligned/{revision}");
    let aligned_srt_relative = format!("{aligned_dir}/narration.srt");
    let aligned_json_relative = format!("{aligned_dir}/narration.json");
    let alignment_relative = format!("{aligned_dir}/alignment.json");
    write_bytes_atomic(&package.root.join(&source_relative), &source_bytes)?;
    write_bytes_atomic(
        &package.root.join(&aligned_srt_relative),
        crate::render_srt(&outcome.segments).as_bytes(),
    )?;
    let document = TranscriptDocument {
        schema_version: retrofeel_types::TRANSCRIPTION_SCHEMA_VERSION,
        language: None,
        provider: TranscriptionProvider::ExternalTranscript,
        model_id: source_description.clone(),
        segments: outcome.segments.clone(),
    };
    write_json_atomic(&package.root.join(&aligned_json_relative), &document)?;
    write_json_atomic(&package.root.join(&alignment_relative), &outcome.report)?;

    for transcript in &mut package.manifest.transcripts {
        transcript.primary = false;
    }
    let source_asset = TranscriptAsset {
        id: format!("{revision}-source"),
        kind: TranscriptKind::Source,
        srt_path: source_relative.clone(),
        srt_sha256: sha256_file(&package.root.join(&source_relative))?,
        json_path: None,
        json_sha256: None,
        alignment_path: None,
        alignment_sha256: None,
        primary: false,
        source_description: source_description.clone(),
    };
    let aligned_asset = TranscriptAsset {
        id: revision,
        kind: TranscriptKind::Aligned,
        srt_path: aligned_srt_relative.clone(),
        srt_sha256: sha256_file(&package.root.join(&aligned_srt_relative))?,
        json_path: Some(aligned_json_relative.clone()),
        json_sha256: Some(sha256_file(&package.root.join(&aligned_json_relative))?),
        alignment_path: Some(alignment_relative.clone()),
        alignment_sha256: Some(sha256_file(&package.root.join(&alignment_relative))?),
        primary: true,
        source_description,
    };
    package.manifest.transcripts.push(source_asset);
    package.manifest.transcripts.push(aligned_asset.clone());

    let manifest_path = package.session_manifest_path();
    let mut session: SessionManifest = read_json(&manifest_path)?;
    session.transcript = Some(aligned_srt_relative);
    session.transcript_json = Some(aligned_json_relative);
    session.transcription_status = TranscriptionJobState::Complete;
    session.transcription_provider = Some(TranscriptionProvider::ExternalTranscript);
    session.transcription_model = Some(outcome.report.tool.clone());
    let narration = TrackAlignment {
        presence: TrackPresence::Present,
        status: if outcome.report.quality == AlignmentQuality::Complete {
            TrackAlignmentStatus::Complete
        } else {
            TrackAlignmentStatus::Degraded
        },
        offset_us: Some((outcome.report.offset_seconds * 1_000_000.0).round() as i64),
        uncertainty_us: outcome
            .report
            .p95_absolute_residual_seconds
            .map(|value| (value * 1_000_000.0).round() as u64),
        clock_source: Some("affine external transcript alignment".into()),
    };
    let embedded_game_audio = probe_embedded_audio(package.root(), &session);
    session.track_alignment = Some(match session.track_alignment.take() {
        Some(mut alignment) => {
            alignment.narration = narration;
            alignment
        }
        None => TrackAlignmentMetadata {
            video: TrackAlignment {
                presence: TrackPresence::Present,
                status: TrackAlignmentStatus::Complete,
                offset_us: Some(0),
                uncertainty_us: Some(0),
                clock_source: Some("encoded video PTS".into()),
            },
            narration,
            game_audio: match embedded_game_audio {
                Some(true) => TrackAlignment {
                    presence: TrackPresence::Present,
                    status: TrackAlignmentStatus::Complete,
                    offset_us: Some(0),
                    uncertainty_us: Some(0),
                    clock_source: Some("encoded audio PTS".into()),
                },
                Some(false) => TrackAlignment {
                    presence: TrackPresence::NotCaptured,
                    status: TrackAlignmentStatus::NotCaptured,
                    offset_us: None,
                    uncertainty_us: None,
                    clock_source: Some("ffprobe media inspection".into()),
                },
                None => TrackAlignment {
                    presence: TrackPresence::Unavailable,
                    status: TrackAlignmentStatus::Unavailable,
                    offset_us: None,
                    uncertainty_us: None,
                    clock_source: None,
                },
            },
        },
    });
    write_json_atomic(&manifest_path, &session)?;
    if package.root.join("manifest.ron").is_file() {
        write_ron_atomic(&package.root.join("manifest.ron"), &session)?;
    }
    package.refresh_capture_resources()?;
    package.save_manifest()?;
    Ok(aligned_asset)
}

fn probe_embedded_audio(root: &Path, session: &SessionManifest) -> Option<bool> {
    let video = resolve_manifest_path(root, session.video.as_deref()?).ok()?;
    let mut candidates = vec![PathBuf::from("ffprobe")];
    candidates.extend(
        [
            "/opt/local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
        ]
        .into_iter()
        .map(|directory| Path::new(directory).join("ffprobe"))
        .filter(|path| path.is_file()),
    );
    for executable in candidates {
        let output = Command::new(executable)
            .args([
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=index",
                "-of",
                "csv=p=0",
            ])
            .arg(&video)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => return Some(!output.stdout.is_empty()),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

pub fn package_title(manifest: &SessionManifest) -> String {
    manifest
        .rom
        .as_ref()
        .and_then(|rom| Path::new(&rom.path).file_stem())
        .and_then(OsStr::to_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            if manifest.core.name.starts_with("steam:") {
                manifest.core.version.clone()
            } else {
                manifest.core.name.clone()
            }
        })
}

fn write_context_brief(root: &Path, title: &str, source: Option<&Path>) -> Result<()> {
    let destination = root.join("context/brief.md");
    if let Some(source) = source {
        let bytes = fs::read(source).map_err(|error| io(source, error))?;
        write_bytes_atomic(&destination, &bytes)
    } else {
        let brief = format!(
            "# Analysis brief\n\nAnalyze the {title} recording as playtest evidence for the project described by the user. Extract transferable UI/UX patterns, game-feel feedback, progression and inventory ideas, points of friction, and concrete implementation actions. Every claim must cite a video time range; distinguish direct evidence from inference.\n"
        );
        write_bytes_atomic(&destination, brief.as_bytes())
    }
}

fn ensure_input_transitions(root: &Path) -> Result<()> {
    let manifest_path = root.join("manifest.json");
    let mut manifest: SessionManifest = read_json(&manifest_path)?;
    let input_path = resolve_manifest_path(root, &manifest.input_log)?;
    let transitions_path = root.join("input-transitions.jsonl");
    if !transitions_path.is_file() {
        let frames: Vec<InputFrame> = read_json(&input_path)?;
        let transitions = input_transitions_from_frames(&frames);
        let parent = transitions_path.parent().unwrap_or(root);
        fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
        let temp = transitions_path.with_extension(format!("jsonl.tmp-{}", random_id()));
        let mut writer = BufWriter::new(File::create(&temp).map_err(|source| io(&temp, source))?);
        for transition in transitions {
            serde_json::to_writer(&mut writer, &transition).map_err(|source| FeelError::Json {
                path: temp.clone(),
                source,
            })?;
            writer
                .write_all(b"\n")
                .map_err(|source| io(&temp, source))?;
        }
        writer.flush().map_err(|source| io(&temp, source))?;
        fs::rename(&temp, &transitions_path).map_err(|source| io(&transitions_path, source))?;
    }
    manifest.input_transitions = Some("input-transitions.jsonl".into());
    write_json_atomic(&manifest_path, &manifest)?;
    if root.join("manifest.ron").is_file() {
        write_ron_atomic(&root.join("manifest.ron"), &manifest)?;
    }
    Ok(())
}

fn inventory_capture_resources(root: &Path) -> Result<Vec<FeelResource>> {
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|relative| {
            let absolute = root.join(&relative);
            let metadata = fs::metadata(&absolute).map_err(|source| io(&absolute, source))?;
            let role = resource_role(&relative);
            let required = matches!(
                role,
                FeelResourceRole::SessionManifest
                    | FeelResourceRole::AuthoritativeInput
                    | FeelResourceRole::Video
            );
            Ok(FeelResource {
                role,
                path: portable_path(&relative),
                media_type: media_type(&relative).into(),
                byte_len: metadata.len(),
                sha256: sha256_file(&absolute)?,
                required,
                immutable: relative != Path::new("manifest.json")
                    && relative != Path::new("manifest.ron"),
            })
        })
        .collect()
}

fn collect_files(root: &Path, current: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(current).map_err(|source| io(current, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io(current, source))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| FeelError::Invalid("package traversal while inventorying".into()))?;
        let first = relative
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(value) => value.to_str(),
                _ => None,
            });
        if matches!(
            first,
            Some("context" | "transcripts" | "analysis" | "exports")
        ) || relative == Path::new("feel.json")
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| io(&path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(FeelError::Invalid(format!(
                "capture contains a symlink: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_files(root, &path, result)?;
        } else if metadata.is_file() {
            result.push(relative.to_path_buf());
        }
    }
    Ok(())
}

fn copy_capture_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source).map_err(|error| io(source, error))? {
        let entry = entry.map_err(|error| io(source, error))?;
        let from = entry.path();
        let name = entry.file_name();
        if matches!(name.to_str(), Some("analysis" | "exports")) || name == "feel.json" {
            continue;
        }
        let to = destination.join(name);
        let metadata = fs::symlink_metadata(&from).map_err(|error| io(&from, error))?;
        if metadata.file_type().is_symlink() {
            return Err(FeelError::Invalid(format!(
                "source session contains a symlink: {}",
                from.display()
            )));
        }
        if metadata.is_dir() {
            fs::create_dir(&to).map_err(|error| io(&to, error))?;
            copy_capture_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).map_err(|error| io(&to, error))?;
        }
    }
    Ok(())
}

fn copy_full_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source).map_err(|error| io(source, error))? {
        let entry = entry.map_err(|error| io(source, error))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from).map_err(|error| io(&from, error))?;
        if metadata.file_type().is_symlink() {
            return Err(FeelError::Invalid(format!(
                "package contains a symlink: {}",
                from.display()
            )));
        }
        if metadata.is_dir() {
            fs::create_dir(&to).map_err(|error| io(&to, error))?;
            copy_full_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).map_err(|error| io(&to, error))?;
        }
    }
    Ok(())
}

fn resource_role(path: &Path) -> FeelResourceRole {
    match portable_path(path).as_str() {
        "manifest.json" => FeelResourceRole::SessionManifest,
        "video.mkv" => FeelResourceRole::Video,
        "input.json" => FeelResourceRole::AuthoritativeInput,
        "input-events.jsonl" => FeelResourceRole::RawInputEvents,
        "input-transitions.jsonl" => FeelResourceRole::InputTransitions,
        "controller-map.json" => FeelResourceRole::ControllerMap,
        "frame-map.json" => FeelResourceRole::FrameMap,
        "capture.json" => FeelResourceRole::CaptureMetadata,
        "initial-state.bin" => FeelResourceRole::InitialState,
        value if value.starts_with("controller-layouts/") => FeelResourceRole::ControllerLayout,
        _ => FeelResourceRole::OtherCapture,
    }
}

fn media_type(path: &Path) -> &'static str {
    match path.extension().and_then(OsStr::to_str) {
        Some("json") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        Some("ron") => "application/vnd.ron",
        Some("mkv") => "video/x-matroska",
        Some("wav") => "audio/wav",
        Some("srt") => "application/x-subrip",
        Some("bin") => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn resolve_manifest_path(root: &Path, value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        let filename = path
            .file_name()
            .ok_or_else(|| FeelError::Invalid(format!("manifest path has no filename: {value}")))?;
        return Ok(root.join(filename));
    }
    ensure_safe_relative(path)?;
    Ok(root.join(path))
}

fn ensure_safe_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(FeelError::Invalid(format!(
            "unsafe package-relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        if let Component::Normal(value) = component {
            path.push(value);
            if path.exists()
                && fs::symlink_metadata(&path)
                    .map_err(|source| io(&path, source))?
                    .file_type()
                    .is_symlink()
            {
                return Err(FeelError::Invalid(format!(
                    "package path traverses a symlink: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn require_feel_suffix(path: &Path) -> Result<()> {
    if path.extension() != Some(OsStr::new("feel")) {
        return Err(FeelError::Invalid(format!(
            "package directory must end in .feel: {}",
            path.display()
        )));
    }
    Ok(())
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn portable_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path).map_err(|source| io(path, source))?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| io(path, source))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        FeelError::Invalid(format!("output path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    let temp = path.with_extension(format!("tmp-{}", random_id()));
    let mut writer = BufWriter::new(File::create(&temp).map_err(|source| io(&temp, source))?);
    serde_json::to_writer_pretty(&mut writer, value).map_err(|source| FeelError::Json {
        path: temp.clone(),
        source,
    })?;
    writer
        .write_all(b"\n")
        .map_err(|source| io(&temp, source))?;
    writer.flush().map_err(|source| io(&temp, source))?;
    fs::rename(&temp, path).map_err(|source| io(path, source))
}

fn write_ron_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let text =
        ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::default()).map_err(|error| {
            FeelError::Invalid(format!("failed serializing {}: {error}", path.display()))
        })?;
    write_bytes_atomic(path, format!("{text}\n").as_bytes())
}

pub(crate) fn write_bytes_atomic(path: &Path, value: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        FeelError::Invalid(format!("output path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    let temp = path.with_extension(format!("tmp-{}", random_id()));
    fs::write(&temp, value).map_err(|source| io(&temp, source))?;
    fs::rename(&temp, path).map_err(|source| io(path, source))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_reader(BufReader::new(
        File::open(path).map_err(|source| io(path, source))?,
    ))
    .map_err(|source| FeelError::Json {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        let fallback = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        bytes.copy_from_slice(&fallback.to_le_bytes());
    }
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

pub(crate) fn now_epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn io(path: &Path, source: std::io::Error) -> FeelError {
    FeelError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retrofeel_types::{CoreInfo, ManifestFormat, TimingInfo};

    fn write_minimal_session(root: &Path) {
        fs::write(root.join("video.mkv"), b"video").unwrap();
        fs::write(root.join("input.json"), b"[]\n").unwrap();
        let manifest = SessionManifest {
            core: CoreInfo {
                name: "steam:1".into(),
                version: "Example Game".into(),
                library_path: "steam".into(),
            },
            rom: None,
            timing: TimingInfo {
                fps: 60.0,
                sample_rate: 48_000.0,
                start_timestamp: 1.0,
            },
            initial_state: None,
            frame_count: 0,
            pause_segments: Vec::new(),
            input_log: "input.json".into(),
            video: Some("video.mkv".into()),
            mic_audio: None,
            transcript: None,
            transcript_json: None,
            transcription_status: TranscriptionJobState::NotRequested,
            transcription_provider: None,
            transcription_model: None,
            binding_map: None,
            capture_provenance: None,
            video_timing: None,
            frame_map: None,
            input_transitions: None,
            track_alignment: None,
            external_capture: None,
            dropped_frames: 0,
            format: ManifestFormat::Json,
        };
        write_json_atomic(&root.join("manifest.json"), &manifest).unwrap();
    }

    #[test]
    fn package_round_trip_preserves_source_and_validates_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_minimal_session(&source);
        let destination = temp.path().join("Example.feel");
        let package = create_package(&source, &destination, &PackageOptions::default()).unwrap();
        assert!(package.validate().valid);
        assert!(!source.join("input-transitions.jsonl").exists());
        assert!(destination.join("input-transitions.jsonl").is_file());
    }

    #[test]
    fn validation_rejects_hash_changes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_minimal_session(&source);
        let destination = temp.path().join("Example.feel");
        let package = create_package(&source, &destination, &PackageOptions::default()).unwrap();
        fs::write(destination.join("video.mkv"), b"changed").unwrap();
        let report = package.validate();
        assert!(!report.valid);
        assert!(report.errors.iter().any(|error| error.contains("mismatch")));
    }

    #[test]
    fn reader_rejects_package_path_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Bad.feel");
        fs::create_dir(&root).unwrap();
        let manifest = FeelManifestV1 {
            kind: FEEL_KIND.into(),
            format_version: FEEL_FORMAT_VERSION,
            package_id: "id".into(),
            title: "Bad".into(),
            created_at_epoch_seconds: 0.0,
            session_manifest: "../manifest.json".into(),
            context_brief: "context/brief.md".into(),
            context_brief_sha256: String::new(),
            resources: Vec::new(),
            transcripts: Vec::new(),
            analyses: Vec::new(),
        };
        write_json_atomic(&root.join("feel.json"), &manifest).unwrap();
        assert!(FeelPackage::open(root).is_err());
    }
}
