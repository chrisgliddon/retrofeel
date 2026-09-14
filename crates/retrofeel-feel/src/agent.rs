use std::env;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use retrofeel_types::{input_transitions_from_frames, InputFrame};
use serde::{Deserialize, Serialize};

use crate::package::{
    now_epoch_seconds, random_id, sha256_file, write_bytes_atomic, write_json_atomic, FeelError,
};
use crate::{ActionItem, AnalysisEvidence, AnalysisResultV2, AnalysisRun, FeelPackage};

type Result<T> = std::result::Result<T, FeelError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAdapterKind {
    Codex,
    Claude,
    OpenCode,
    Kimi,
}

impl AgentAdapterKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Kimi => "kimi",
        }
    }

    fn command(self) -> &'static str {
        self.id()
    }
}

impl std::str::FromStr for AgentAdapterKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" | "claude-code" => Ok(Self::Claude),
            "opencode" | "open-code" => Ok(Self::OpenCode),
            "kimi" | "kimi-code" => Ok(Self::Kimi),
            _ => Err(format!("unknown agent adapter: {value}")),
        }
    }
}

impl std::fmt::Display for AgentAdapterKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDescriptor {
    pub adapter: AgentAdapterKind,
    pub executable: Option<PathBuf>,
    pub version: Option<String>,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    pub adapter: AgentAdapterKind,
    pub model: Option<String>,
    pub max_frames: usize,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            adapter: AgentAdapterKind::Codex,
            model: None,
            max_frames: 12,
        }
    }
}

#[derive(Debug, Serialize)]
struct AgentRunMetadata {
    schema_version: u32,
    adapter: AgentAdapterKind,
    executable: String,
    version: Option<String>,
    model: Option<String>,
    started_at_epoch_seconds: f64,
    finished_at_epoch_seconds: f64,
    attempts: u8,
    permissions: Vec<String>,
    disclosed_files: Vec<String>,
}

pub fn discover_agents() -> Vec<AgentDescriptor> {
    [
        AgentAdapterKind::Codex,
        AgentAdapterKind::Claude,
        AgentAdapterKind::OpenCode,
        AgentAdapterKind::Kimi,
    ]
    .into_iter()
    .map(|adapter| {
        let executable = find_executable(adapter.command()).or_else(|| {
            (adapter == AgentAdapterKind::Kimi)
                .then(|| dirs_home().join(".kimi-code/bin/kimi"))
                .filter(|path| path.is_file())
        });
        let version = executable.as_deref().and_then(command_version);
        AgentDescriptor {
            adapter,
            available: executable.is_some(),
            executable,
            version,
        }
    })
    .collect()
}

pub fn analyze_package(
    package_root: impl AsRef<Path>,
    options: &AnalyzeOptions,
) -> Result<AnalysisResultV2> {
    let mut package = FeelPackage::open(package_root)?;
    let validation = package.validate();
    if !validation.valid {
        return Err(FeelError::Invalid(format!(
            "refusing to analyze an invalid package: {}",
            validation.errors.join("; ")
        )));
    }
    let descriptor = discover_agents()
        .into_iter()
        .find(|descriptor| descriptor.adapter == options.adapter)
        .filter(|descriptor| descriptor.available)
        .ok_or_else(|| {
            FeelError::Invalid(format!(
                "{} CLI is not installed or not on PATH",
                options.adapter
            ))
        })?;
    let executable = descriptor.executable.clone().unwrap();
    let run_id = random_id();
    let run_relative = format!("analysis/runs/{run_id}");
    let run_dir = package.root().join(&run_relative);
    fs::create_dir_all(&run_dir).map_err(|source| io(&run_dir, source))?;

    let started = now_epoch_seconds();
    let evidence = build_evidence(&package, &run_dir, options.max_frames.min(12))?;
    let schema_path = run_dir.join("result.schema.json");
    write_json_atomic(&schema_path, &analysis_schema())?;
    let prompt = analysis_prompt(&package, &evidence);
    write_json_atomic(
        &run_dir.join("request.json"),
        &serde_json::json!({
            "schema_version": 1,
            "adapter": options.adapter,
            "model": options.model,
            "prompt": prompt,
            "evidence_count": evidence.len(),
        }),
    )?;

    let mut attempts = 1;
    let raw = invoke_agent(
        options.adapter,
        &executable,
        options.model.as_deref(),
        &run_dir,
        &schema_path,
        &prompt,
    )?;
    let result = match parse_validated_analysis_result(&raw) {
        Ok(result) => result,
        Err(first_error) => {
            attempts = 2;
            let repair_prompt = format!(
                "The previous response failed the required JSON contract: {first_error}. Return only a corrected JSON object matching result.schema.json. Preserve evidence indices and do not add markdown fences. Previous response:\n{}",
                truncate(&raw, 24_000)
            );
            let repaired = invoke_agent(
                options.adapter,
                &executable,
                options.model.as_deref(),
                &run_dir,
                &schema_path,
                &repair_prompt,
            )?;
            parse_validated_analysis_result(&repaired).map_err(|error| {
                FeelError::Invalid(format!(
                    "{} returned invalid analysis JSON after repair: {error}",
                    options.adapter
                ))
            })?
        }
    };
    let result_path = run_dir.join("result.json");
    let report_path = run_dir.join("report.md");
    let action_plan_path = run_dir.join("action-plan.md");
    let agent_path = run_dir.join("agent.json");
    write_json_atomic(&result_path, &result)?;
    write_bytes_atomic(&report_path, render_report(&result).as_bytes())?;
    write_bytes_atomic(
        &action_plan_path,
        render_action_plan(&result.actions).as_bytes(),
    )?;
    let metadata = AgentRunMetadata {
        schema_version: 1,
        adapter: options.adapter,
        executable: executable.display().to_string(),
        version: descriptor.version,
        model: options.model.clone(),
        started_at_epoch_seconds: started,
        finished_at_epoch_seconds: now_epoch_seconds(),
        attempts,
        permissions: vec![
            "read staged evidence".into(),
            "read-only sandbox; no persistence or web requested".into(),
        ],
        disclosed_files: evidence_disclosure(&run_dir),
    };
    write_json_atomic(&agent_path, &metadata)?;

    let run = AnalysisRun {
        id: run_id,
        created_at_epoch_seconds: started,
        adapter: options.adapter.to_string(),
        model: options.model.clone(),
        result_path: portable_relative(&package, &result_path)?,
        result_sha256: sha256_file(&result_path)?,
        report_path: portable_relative(&package, &report_path)?,
        report_sha256: sha256_file(&report_path)?,
        action_plan_path: portable_relative(&package, &action_plan_path)?,
        action_plan_sha256: sha256_file(&action_plan_path)?,
        agent_path: portable_relative(&package, &agent_path)?,
        agent_sha256: sha256_file(&agent_path)?,
    };
    package.register_analysis(run)?;
    Ok(result)
}

fn build_evidence(
    package: &FeelPackage,
    run_dir: &Path,
    max_frames: usize,
) -> Result<Vec<AnalysisEvidence>> {
    let evidence_dir = run_dir.join("evidence");
    let frames_dir = evidence_dir.join("frames");
    fs::create_dir_all(&frames_dir).map_err(|source| io(&frames_dir, source))?;
    fs::copy(
        package.resolve(&package.manifest().context_brief)?,
        evidence_dir.join("brief.md"),
    )
    .map_err(|source| io(&evidence_dir.join("brief.md"), source))?;
    let session = package.session_manifest()?;
    write_json_atomic(&evidence_dir.join("session-summary.json"), &session)?;

    if let Some(transcript) = package.primary_transcript() {
        if let Some(json) = transcript.json_path.as_ref() {
            fs::copy(package.resolve(json)?, evidence_dir.join("transcript.json"))
                .map_err(|source| io(&evidence_dir.join("transcript.json"), source))?;
        }
        fs::copy(
            package.resolve(&transcript.srt_path)?,
            evidence_dir.join("transcript.srt"),
        )
        .map_err(|source| io(&evidence_dir.join("transcript.srt"), source))?;
    }

    let input_path = resolve_session_artifact(package, &session.input_log)?;
    let frames: Vec<InputFrame> = serde_json::from_reader(BufReader::new(
        File::open(&input_path).map_err(|source| io(&input_path, source))?,
    ))
    .map_err(|source| FeelError::Json {
        path: input_path.clone(),
        source,
    })?;
    let transition_path = evidence_dir.join("input-transitions.jsonl");
    let mut writer = BufWriter::new(
        File::create(&transition_path).map_err(|source| io(&transition_path, source))?,
    );
    for transition in input_transitions_from_frames(&frames) {
        serde_json::to_writer(&mut writer, &transition).map_err(|source| FeelError::Json {
            path: transition_path.clone(),
            source,
        })?;
        writer
            .write_all(b"\n")
            .map_err(|source| io(&transition_path, source))?;
    }
    writer
        .flush()
        .map_err(|source| io(&transition_path, source))?;

    let duration = frames
        .last()
        .and_then(|frame| frame.elapsed_us)
        .map(|value| value as f64 / 1_000_000.0)
        .unwrap_or_else(|| {
            if session.timing.fps > 0.0 {
                session.frame_count as f64 / session.timing.fps
            } else {
                0.0
            }
        })
        .max(0.001);
    let transcript = package
        .primary_transcript()
        .and_then(|asset| asset.json_path.as_ref())
        .and_then(|path| package.resolve(path).ok())
        .and_then(|path| File::open(path).ok())
        .and_then(|file| {
            serde_json::from_reader::<_, retrofeel_types::TranscriptDocument>(file).ok()
        });
    let video = session
        .video
        .as_deref()
        .map(|path| resolve_session_artifact(package, path))
        .transpose()?;
    let selected = max_frames.max(1);
    let mut evidence = Vec::new();
    for index in 0..selected {
        let at = duration * index as f64 / (selected - 1).max(1) as f64;
        let frame_relative = format!("evidence/frames/frame-{index:02}-{at:.3}s.png");
        let frame_path = run_dir.join(&frame_relative);
        if let Some(video) = video.as_ref() {
            extract_frame(video, at, &frame_path)?;
        }
        let input = nearest_input(&frames, at);
        let controls = input.map(active_controls).unwrap_or_default();
        let excerpt = transcript.as_ref().and_then(|document| {
            document
                .segments
                .iter()
                .find(|segment| at >= segment.start_seconds && at <= segment.end_seconds)
                .map(|segment| segment.text.clone())
        });
        evidence.push(AnalysisEvidence {
            start_seconds: at,
            end_seconds: (at + 0.25).min(duration),
            description: format!("Representative frame at {at:.3} seconds"),
            transcript_excerpt: excerpt,
            controls,
            frame_path: frame_path
                .is_file()
                .then(|| frame_relative.replace('\\', "/")),
        });
    }
    write_json_atomic(&evidence_dir.join("evidence.json"), &evidence)?;
    Ok(evidence)
}

fn resolve_session_artifact(package: &FeelPackage, manifest_path: &str) -> Result<PathBuf> {
    let path = Path::new(manifest_path);
    if path.is_absolute() {
        let filename = path.file_name().ok_or_else(|| {
            FeelError::Invalid(format!("manifest path has no filename: {manifest_path}"))
        })?;
        return package.resolve(Path::new(filename));
    }
    package.resolve(path)
}

fn nearest_input(frames: &[InputFrame], seconds: f64) -> Option<&InputFrame> {
    let target = (seconds * 1_000_000.0).max(0.0) as u64;
    let index = frames.partition_point(|frame| frame.elapsed_us.unwrap_or(0) <= target);
    frames
        .get(index.saturating_sub(1))
        .or_else(|| frames.first())
}

fn active_controls(frame: &InputFrame) -> Vec<String> {
    let Some(raw) = frame.raw_host.as_ref() else {
        return Vec::new();
    };
    let mut controls = raw.keyboard_keys.clone();
    controls.extend(raw.gamepad_buttons.clone());
    if let Some(gamepad) = raw.gamepads.iter().find(|gamepad| gamepad.port == Some(0)) {
        controls.extend(gamepad.buttons.iter().cloned());
    }
    controls.sort();
    controls.dedup();
    controls
}

fn extract_frame(video: &Path, at: f64, output: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-ss"])
        .arg(format!("{at:.6}"))
        .arg("-i")
        .arg(video)
        .args(["-frames:v", "1", "-vf", "scale='min(1280,iw)':-2"])
        .arg(output)
        .status()
        .map_err(|source| io(output, source))?;
    if !status.success() || !output.is_file() {
        return Err(FeelError::Invalid(format!(
            "ffmpeg could not extract evidence frame at {at:.3}s"
        )));
    }
    Ok(())
}

fn invoke_agent(
    adapter: AgentAdapterKind,
    executable: &Path,
    model: Option<&str>,
    run_dir: &Path,
    schema_path: &Path,
    prompt: &str,
) -> Result<String> {
    // Agent processes change their working directory to the staged evidence
    // folder. Canonical paths keep schema, output, profile, and image arguments
    // valid even when the caller opened the package through a relative path.
    let run_dir = fs::canonicalize(run_dir).map_err(|source| io(run_dir, source))?;
    let schema_path = fs::canonicalize(schema_path).map_err(|source| io(schema_path, source))?;
    let evidence_dir = run_dir.join("evidence");
    let output_path = run_dir.join(format!("raw-{}.txt", random_id()));
    let mut command = Command::new(executable);
    command.current_dir(&evidence_dir).stdin(Stdio::piped());
    match adapter {
        AgentAdapterKind::Codex => {
            command.args([
                "--ask-for-approval",
                "never",
                "exec",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--output-schema",
            ]);
            command.arg(schema_path).args(["--output-last-message"]);
            command.arg(&output_path).arg("-C").arg(&evidence_dir);
            if let Some(model) = model {
                command.arg("--model").arg(model);
            }
            for frame in evidence_frames(&evidence_dir) {
                command.arg("--image").arg(frame);
            }
            command.arg("-");
        }
        AgentAdapterKind::Claude => {
            command.args([
                "--print",
                "--no-session-persistence",
                "--permission-mode",
                "plan",
                "--tools",
                "Read",
                "--json-schema",
            ]);
            command.arg(analysis_schema().to_string());
            if let Some(model) = model {
                command.arg("--model").arg(model);
            }
            command.arg(prompt);
        }
        AgentAdapterKind::OpenCode => {
            command.args(["run", "--pure", "--dir"]);
            command.arg(&evidence_dir).arg("--format").arg("default");
            if let Some(model) = model {
                command.arg("--model").arg(model);
            }
            for frame in evidence_frames(&evidence_dir) {
                command.arg("--file").arg(frame);
            }
            command.arg(prompt).env(
                "OPENCODE_CONFIG_CONTENT",
                r#"{"permission":{"*":"deny","read":"allow"}}"#,
            );
        }
        AgentAdapterKind::Kimi => {
            let profile = run_dir.join("kimi-readonly.md");
            let profile_text = "---\nname: retrofeel-analysis\ndescription: Analyze staged RetroFeel evidence without modifying it.\ntools: Read, ReadMediaFile\nsubagents: []\n---\nRead only the supplied evidence. Do not use shell, write, edit, web, or sub-agent tools. Return the requested JSON result.\n";
            write_bytes_atomic(&profile, profile_text.as_bytes())?;
            command
                .arg("--prompt")
                .arg(prompt)
                .args(["--output-format", "text"]);
            command.arg("--agent-file").arg(&profile);
            if let Some(model) = model {
                command.arg("--model").arg(model);
            }
        }
    }
    let mut child = command.spawn().map_err(|source| io(executable, source))?;
    if matches!(adapter, AgentAdapterKind::Codex) {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .map_err(|source| io(executable, source))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|source| io(executable, source))?;
    if !output.status.success() {
        return Err(FeelError::Invalid(format!(
            "{} analysis failed with {}: {}",
            adapter,
            output.status,
            truncate(&String::from_utf8_lossy(&output.stderr), 4_000)
        )));
    }
    if adapter == AgentAdapterKind::Codex && output_path.is_file() {
        return fs::read_to_string(&output_path).map_err(|source| io(&output_path, source));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if adapter == AgentAdapterKind::Claude {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&stdout) {
            if let Some(structured) = value.get("structured_output") {
                return Ok(structured.to_string());
            }
            if let Some(result) = value.get("result").and_then(serde_json::Value::as_str) {
                return Ok(result.to_string());
            }
        }
    }
    Ok(stdout)
}

fn parse_analysis_result(raw: &str) -> std::result::Result<AnalysisResultV2, String> {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(result) = serde_json::from_str(trimmed) {
        return Ok(result);
    }
    let start = trimmed
        .find('{')
        .ok_or_else(|| "response contains no JSON object".to_string())?;
    let end = trimmed
        .rfind('}')
        .ok_or_else(|| "response contains no complete JSON object".to_string())?;
    serde_json::from_str(&trimmed[start..=end]).map_err(|error| error.to_string())
}

fn parse_validated_analysis_result(raw: &str) -> std::result::Result<AnalysisResultV2, String> {
    let result = parse_analysis_result(raw)?;
    validate_result_references(&result).map_err(|error| error.to_string())?;
    Ok(result)
}

fn validate_result_references(result: &AnalysisResultV2) -> Result<()> {
    if result.schema_version != 2 {
        return Err(FeelError::Invalid(format!(
            "analysis schema version {} is unsupported",
            result.schema_version
        )));
    }
    if result.summary.trim().is_empty()
        || result.evidence.is_empty()
        || result.observations.is_empty()
        || result.insights.is_empty()
        || result.actions.is_empty()
    {
        return Err(FeelError::Invalid(
            "analysis must contain a summary, evidence, observations, insights, and actions".into(),
        ));
    }
    for (index, evidence) in result.evidence.iter().enumerate() {
        if !evidence.start_seconds.is_finite()
            || !evidence.end_seconds.is_finite()
            || evidence.start_seconds < 0.0
            || evidence.end_seconds < evidence.start_seconds
            || evidence.description.trim().is_empty()
        {
            return Err(FeelError::Invalid(format!(
                "analysis evidence {index} has an invalid time range or description"
            )));
        }
    }
    if result
        .observations
        .iter()
        .any(|item| item.evidence.is_empty())
        || result.insights.iter().any(|item| item.evidence.is_empty())
        || result.actions.iter().any(|item| item.evidence.is_empty())
    {
        return Err(FeelError::Invalid(
            "every observation, insight, and action must cite evidence".into(),
        ));
    }
    let evidence_count = result.evidence.len();
    let invalid = result
        .observations
        .iter()
        .flat_map(|item| item.evidence.iter())
        .chain(result.insights.iter().flat_map(|item| item.evidence.iter()))
        .chain(result.actions.iter().flat_map(|item| item.evidence.iter()))
        .find(|index| **index >= evidence_count);
    if let Some(index) = invalid {
        return Err(FeelError::Invalid(format!(
            "analysis references missing evidence index {index}"
        )));
    }
    Ok(())
}

fn analysis_prompt(package: &FeelPackage, evidence: &[AnalysisEvidence]) -> String {
    format!(
        "Analyze the RetroFeel evidence package titled {:?}. Work only from brief.md, session-summary.json, transcript.json/transcript.srt when present, input-transitions.jsonl, evidence.json, and the staged frame images. Produce transferable product/design insights and a concrete action plan for the project described in brief.md. Distinguish observation from inference. Every observation, insight, and action must reference zero-based indices into your returned evidence array. Evidence entries need exact video time ranges. Return only one JSON object matching result.schema.json, with schema_version 2. The staged evidence contains {} representative timestamps.",
        package.manifest().title,
        evidence.len()
    )
}

fn analysis_schema() -> serde_json::Value {
    serde_json::json!({
      "type": "object",
      "additionalProperties": false,
      "required": ["schema_version", "summary", "evidence", "observations", "insights", "actions", "open_questions"],
      "properties": {
        "schema_version": {"type": "integer", "const": 2},
        "summary": {"type": "string"},
        "evidence": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["start_seconds", "end_seconds", "description", "transcript_excerpt", "controls", "frame_path"], "properties": {
          "start_seconds": {"type": "number"}, "end_seconds": {"type": "number"}, "description": {"type": "string"},
          "transcript_excerpt": {"type": ["string", "null"]}, "controls": {"type": "array", "items": {"type": "string"}},
          "frame_path": {"type": ["string", "null"]}
        }}},
        "observations": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["category", "polarity", "statement", "confidence", "evidence"], "properties": {
          "category": {"type": "string"}, "polarity": {"type": "string"}, "statement": {"type": "string"}, "confidence": {"type": "number"}, "evidence": {"type": "array", "items": {"type": "integer", "minimum": 0}}
        }}},
        "insights": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["title", "implication", "project_relevance", "evidence"], "properties": {
          "title": {"type": "string"}, "implication": {"type": "string"}, "project_relevance": {"type": "string"}, "evidence": {"type": "array", "items": {"type": "integer", "minimum": 0}}
        }}},
        "actions": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["title", "rationale", "priority", "effort", "acceptance_criteria", "evidence"], "properties": {
          "title": {"type": "string"}, "rationale": {"type": "string"}, "priority": {"type": "string"}, "effort": {"type": "string"}, "acceptance_criteria": {"type": "array", "items": {"type": "string"}}, "evidence": {"type": "array", "items": {"type": "integer", "minimum": 0}}
        }}},
        "open_questions": {"type": "array", "items": {"type": "string"}}
      }
    })
}

fn render_report(result: &AnalysisResultV2) -> String {
    let mut output = format!(
        "# Recording analysis\n\n{}\n\n## Observations\n\n",
        result.summary
    );
    for observation in &result.observations {
        output.push_str(&format!(
            "- **{} / {}:** {} _(confidence {:.0}%; evidence {})_\n",
            observation.category,
            observation.polarity,
            observation.statement,
            observation.confidence.clamp(0.0, 1.0) * 100.0,
            evidence_labels(&observation.evidence)
        ));
    }
    output.push_str("\n## Transferable insights\n\n");
    for insight in &result.insights {
        output.push_str(&format!(
            "### {}\n\n{}\n\nProject relevance: {}\n\nEvidence: {}\n\n",
            insight.title,
            insight.implication,
            insight.project_relevance,
            evidence_labels(&insight.evidence)
        ));
    }
    if !result.open_questions.is_empty() {
        output.push_str("## Open questions\n\n");
        for question in &result.open_questions {
            output.push_str(&format!("- {question}\n"));
        }
    }
    output
}

fn render_action_plan(actions: &[ActionItem]) -> String {
    let mut output = "# Action plan\n\n".to_string();
    for (index, action) in actions.iter().enumerate() {
        output.push_str(&format!(
            "## {}. {}\n\nPriority: {} · Effort: {} · Evidence: {}\n\n{}\n\nAcceptance criteria:\n\n",
            index + 1,
            action.title,
            action.priority,
            action.effort,
            evidence_labels(&action.evidence),
            action.rationale
        ));
        for criterion in &action.acceptance_criteria {
            output.push_str(&format!("- [ ] {criterion}\n"));
        }
        output.push('\n');
    }
    output
}

fn evidence_labels(indices: &[usize]) -> String {
    if indices.is_empty() {
        "none".into()
    } else {
        indices
            .iter()
            .map(|index| format!("E{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn evidence_frames(evidence_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(evidence_dir.join("frames")) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn evidence_disclosure(run_dir: &Path) -> Vec<String> {
    let evidence = run_dir.join("evidence");
    let mut result = vec![
        "evidence/brief.md".into(),
        "evidence/session-summary.json".into(),
        "evidence/input-transitions.jsonl".into(),
        "evidence/evidence.json".into(),
    ];
    if evidence.join("transcript.json").is_file() {
        result.push("evidence/transcript.json".into());
    }
    if evidence.join("transcript.srt").is_file() {
        result.push("evidence/transcript.srt".into());
    }
    if run_dir.join("result.schema.json").is_file() {
        result.push("result.schema.json".into());
    }
    result.extend(evidence_frames(&evidence).into_iter().filter_map(|path| {
        path.strip_prefix(run_dir)
            .ok()
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
    }));
    result
}

fn portable_relative(package: &FeelPackage, path: &Path) -> Result<String> {
    path.strip_prefix(package.root())
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|_| FeelError::Invalid(format!("analysis escaped package: {}", path.display())))
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn command_version(executable: &Path) -> Option<String> {
    let output = Command::new(executable).arg("--version").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn dirs_home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

fn truncate(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        value.to_string()
    } else {
        let mut end = maximum;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
    }
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

    #[test]
    fn response_schema_is_strict_for_agent_providers() {
        fn assert_strict(value: &serde_json::Value) {
            if value.get("type").and_then(serde_json::Value::as_str) == Some("object")
                && value.get("additionalProperties") == Some(&serde_json::Value::Bool(false))
            {
                let properties = value["properties"].as_object().unwrap();
                let required = value["required"].as_array().unwrap();
                for key in properties.keys() {
                    assert!(
                        required
                            .iter()
                            .any(|required| required.as_str() == Some(key)),
                        "strict object schema omitted {key:?} from required"
                    );
                }
            }
            if let Some(properties) = value
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                for child in properties.values() {
                    assert_strict(child);
                }
            }
            if let Some(items) = value.get("items") {
                assert_strict(items);
            }
        }

        assert_strict(&analysis_schema());
    }

    #[test]
    fn extracts_json_from_markdown_fence() {
        let raw = r#"```json
{"schema_version":2,"summary":"ok","evidence":[],"observations":[],"insights":[],"actions":[],"open_questions":[]}
```"#;
        assert_eq!(parse_analysis_result(raw).unwrap().summary, "ok");
    }

    fn valid_result() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2, "summary": "A control needs clearer feedback",
            "evidence": [{"start_seconds": 1.0, "end_seconds": 2.0, "description": "A control is pressed", "transcript_excerpt": null, "controls": [], "frame_path": null}],
            "observations": [{"category": "input", "polarity": "neutral", "statement": "The input is visible", "confidence": 0.8, "evidence": [0]}],
            "insights": [{"title": "Feedback", "implication": "Show input acknowledgement", "project_relevance": "Help playtesters see the response", "evidence": [0]}],
            "actions": [{"title": "Add feedback", "rationale": "Make the response visible", "priority": "medium", "effort": "small", "acceptance_criteria": ["Response is visible"], "evidence": [0]}],
            "open_questions": []
        })
    }

    #[test]
    fn analysis_v2_roundtrips_and_reports_project_relevance() {
        let value = valid_result();
        let result = parse_validated_analysis_result(&value.to_string()).unwrap();
        let encoded = serde_json::to_string(&result).unwrap();
        assert_eq!(parse_validated_analysis_result(&encoded).unwrap(), result);
        assert!(render_report(&result).contains("Project relevance: Help playtesters"));
        assert_eq!(
            analysis_schema()["properties"]["schema_version"]["const"],
            2
        );
    }

    #[test]
    fn analysis_rejects_old_versions_foreign_fields_and_invalid_references() {
        let mut value = valid_result();
        value["schema_version"] = serde_json::json!(1);
        assert!(parse_validated_analysis_result(&value.to_string()).is_err());
        value = valid_result();
        value["insights"][0]["unrecognized_relevance"] = serde_json::json!("unexpected");
        assert!(parse_validated_analysis_result(&value.to_string()).is_err());
        value = valid_result();
        value["insights"][0]
            .as_object_mut()
            .unwrap()
            .remove("project_relevance");
        assert!(parse_validated_analysis_result(&value.to_string()).is_err());
        value = valid_result();
        value["actions"][0]["evidence"] = serde_json::json!([9]);
        assert!(parse_validated_analysis_result(&value.to_string()).is_err());
        value = valid_result();
        value["evidence"][0]["end_seconds"] = serde_json::json!(0);
        assert!(parse_validated_analysis_result(&value.to_string()).is_err());
    }

    #[test]
    fn result_rejects_missing_evidence_reference() {
        let result = AnalysisResultV2 {
            schema_version: 2,
            summary: String::new(),
            evidence: Vec::new(),
            observations: Vec::new(),
            insights: Vec::new(),
            actions: vec![ActionItem {
                title: "Action".into(),
                rationale: "Why".into(),
                priority: "high".into(),
                effort: "small".into(),
                acceptance_criteria: Vec::new(),
                evidence: vec![0],
            }],
            open_questions: Vec::new(),
        };
        assert!(validate_result_references(&result).is_err());
    }
}
