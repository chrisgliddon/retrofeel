//! JSON-lines worker for crash-isolated sherpa-onnx inference.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use retrofeel_types::{
    TranscriptDocument, TranscriptSegment, TranscriptionConfig, TranscriptionWorkerEvent,
    TranscriptionWorkerRequest, MAX_TRANSCRIPTION_MESSAGE_BYTES, TRANSCRIPTION_SCHEMA_VERSION,
    WORKER_PROTOCOL_VERSION,
};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig,
    OfflineTransducerModelConfig, OfflineWhisperModelConfig,
};

type Output = Arc<Mutex<std::io::Stdout>>;

fn main() -> Result<()> {
    let output = Arc::new(Mutex::new(std::io::stdout()));
    send(
        &output,
        &TranscriptionWorkerEvent::Ready {
            protocol_version: WORKER_PROTOCOL_VERSION,
        },
    );

    let mut jobs: HashMap<String, Arc<AtomicBool>> = HashMap::new();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes > MAX_TRANSCRIPTION_MESSAGE_BYTES {
            send(
                &output,
                &TranscriptionWorkerEvent::Failed {
                    job_id: "unknown".into(),
                    message: "Worker request exceeded the 4 MiB protocol limit".into(),
                },
            );
            continue;
        }
        let request: TranscriptionWorkerRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                send(
                    &output,
                    &TranscriptionWorkerEvent::Failed {
                        job_id: "unknown".into(),
                        message: format!("Invalid worker request: {error}"),
                    },
                );
                continue;
            }
        };
        match request {
            TranscriptionWorkerRequest::Transcribe {
                job_id,
                mic_wav,
                output_dir,
                config,
            } => {
                if jobs.contains_key(&job_id) {
                    send(
                        &output,
                        &TranscriptionWorkerEvent::Failed {
                            job_id,
                            message: "A job with this ID is already running".into(),
                        },
                    );
                    continue;
                }
                let cancelled = Arc::new(AtomicBool::new(false));
                jobs.insert(job_id.clone(), Arc::clone(&cancelled));
                let job_output = Arc::clone(&output);
                std::thread::Builder::new()
                    .name(format!("transcription-{job_id}"))
                    .spawn(move || {
                        if let Err(message) = run_job(
                            &job_id,
                            &mic_wav,
                            &output_dir,
                            &config,
                            &cancelled,
                            &job_output,
                        ) {
                            send(
                                &job_output,
                                &TranscriptionWorkerEvent::Failed { job_id, message },
                            );
                        }
                    })?;
            }
            TranscriptionWorkerRequest::Cancel { job_id } => {
                if let Some(cancelled) = jobs.get(&job_id) {
                    cancelled.store(true, Ordering::Release);
                } else {
                    send(
                        &output,
                        &TranscriptionWorkerEvent::Failed {
                            job_id,
                            message: "Transcription job is not running".into(),
                        },
                    );
                }
            }
            TranscriptionWorkerRequest::Shutdown => break,
        }
    }
    Ok(())
}

fn run_job(
    job_id: &str,
    mic_wav: &Path,
    output_dir: &Path,
    config: &TranscriptionConfig,
    cancelled: &AtomicBool,
    output: &Output,
) -> std::result::Result<(), String> {
    progress(output, job_id, 5);
    let model_dir = config
        .external_model
        .as_deref()
        .ok_or_else(|| "No installed sherpa-onnx model directory was selected".to_string())?;
    if !model_dir.is_dir() {
        return Err(format!(
            "Selected model directory does not exist: {}",
            model_dir.display()
        ));
    }
    let (samples, sample_rate) = read_wav_mono(mic_wav).map_err(|error| error.to_string())?;
    if cancelled.load(Ordering::Acquire) {
        send(
            output,
            &TranscriptionWorkerEvent::Cancelled {
                job_id: job_id.into(),
            },
        );
        return Ok(());
    }
    progress(output, job_id, 20);

    let recognizer_config = recognizer_config(model_dir, config)?;
    let recognizer = OfflineRecognizer::create(&recognizer_config)
        .ok_or_else(|| "sherpa-onnx rejected the selected model files".to_string())?;
    let stream = recognizer.create_stream();
    stream.accept_waveform(sample_rate as i32, &samples);
    progress(output, job_id, 35);
    recognizer.decode(&stream);
    if cancelled.load(Ordering::Acquire) {
        send(
            output,
            &TranscriptionWorkerEvent::Cancelled {
                job_id: job_id.into(),
            },
        );
        return Ok(());
    }
    let result = stream
        .get_result()
        .ok_or_else(|| "sherpa-onnx returned no transcription result".to_string())?;
    let text = result.text.trim().to_string();
    let duration = samples.len() as f64 / f64::from(sample_rate.max(1));
    let start = result
        .timestamps
        .as_ref()
        .and_then(|timestamps| timestamps.first().copied())
        .map(f64::from)
        .unwrap_or(0.0);
    let end = result
        .timestamps
        .as_ref()
        .and_then(|timestamps| timestamps.last().copied())
        .map(f64::from)
        .unwrap_or(duration)
        .max(start);
    let segment = TranscriptSegment {
        start_seconds: start,
        end_seconds: end,
        text,
    };
    send(
        output,
        &TranscriptionWorkerEvent::Segment {
            job_id: job_id.into(),
            segment: segment.clone(),
        },
    );
    progress(output, job_id, 85);

    std::fs::create_dir_all(output_dir).map_err(|error| error.to_string())?;
    let document = TranscriptDocument {
        schema_version: TRANSCRIPTION_SCHEMA_VERSION,
        language: config.language.clone(),
        provider: config.provider,
        model_id: config.selected_model_id.clone(),
        segments: vec![segment],
    };
    let json_path = output_dir.join("transcript.json");
    atomic_write(&json_path, |file| {
        serde_json::to_writer_pretty(file, &document)?;
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    let srt_path = output_dir.join("transcript.srt");
    atomic_write(&srt_path, |file| {
        writeln!(file, "1")?;
        writeln!(file, "{} --> {}", srt_timestamp(start), srt_timestamp(end))?;
        writeln!(file, "{}", document.segments[0].text)?;
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    progress(output, job_id, 100);
    send(
        output,
        &TranscriptionWorkerEvent::Complete {
            job_id: job_id.into(),
            transcript_json: json_path,
            transcript_srt: srt_path,
        },
    );
    Ok(())
}

fn recognizer_config(
    model_dir: &Path,
    config: &TranscriptionConfig,
) -> std::result::Result<OfflineRecognizerConfig, String> {
    let id = config.selected_model_id.as_deref().unwrap_or_default();
    let tokens = required(model_dir, "tokens.txt")?;
    let mut recognizer = OfflineRecognizerConfig::default();
    recognizer.model_config.tokens = Some(path_string(&tokens));
    recognizer.model_config.num_threads = 2;
    if id.starts_with("parakeet") {
        recognizer.model_config.transducer = OfflineTransducerModelConfig {
            encoder: Some(path_string(&required(model_dir, "encoder.int8.onnx")?)),
            decoder: Some(path_string(&required(model_dir, "decoder.int8.onnx")?)),
            joiner: Some(path_string(&required(model_dir, "joiner.int8.onnx")?)),
        };
        recognizer.model_config.model_type = Some("nemo_transducer".into());
    } else if id.starts_with("sensevoice") {
        recognizer.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(path_string(&required(model_dir, "model.int8.onnx")?)),
            language: Some(config.language.clone().unwrap_or_else(|| "auto".into())),
            use_itn: true,
        };
    } else if id.starts_with("whisper") {
        let encoder = find_suffix(model_dir, "encoder.int8.onnx")
            .or_else(|| find_suffix(model_dir, "encoder.onnx"))
            .ok_or_else(|| "Whisper encoder ONNX file is missing".to_string())?;
        let decoder = find_suffix(model_dir, "decoder.int8.onnx")
            .or_else(|| find_suffix(model_dir, "decoder.onnx"))
            .ok_or_else(|| "Whisper decoder ONNX file is missing".to_string())?;
        recognizer.model_config.whisper = OfflineWhisperModelConfig {
            encoder: Some(path_string(&encoder)),
            decoder: Some(path_string(&decoder)),
            language: config.language.clone(),
            task: Some("transcribe".into()),
            enable_token_timestamps: true,
            enable_segment_timestamps: true,
            ..Default::default()
        };
    } else {
        return Err(format!("Unsupported transcription model ID: {id}"));
    }
    Ok(recognizer)
}

fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("Could not read microphone WAV {}", path.display()))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let interleaved = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|sample| sample.map(|value| f32::from(value) / 32_768.0))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    let mono = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / frame.len() as f32)
        .collect();
    Ok((mono, spec.sample_rate))
}

fn required(dir: &Path, filename: &str) -> std::result::Result<PathBuf, String> {
    let path = dir.join(filename);
    path.is_file()
        .then_some(path)
        .ok_or_else(|| format!("Model file is missing: {filename}"))
}

fn find_suffix(dir: &Path, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn progress(output: &Output, job_id: &str, percent: u8) {
    send(
        output,
        &TranscriptionWorkerEvent::Progress {
            job_id: job_id.into(),
            percent,
        },
    );
}

fn send(output: &Output, event: &TranscriptionWorkerEvent) {
    let Ok(mut output) = output.lock() else {
        return;
    };
    if serde_json::to_writer(&mut *output, event).is_ok() {
        let _ = output.write_all(b"\n");
        let _ = output.flush();
    }
}

fn atomic_write(path: &Path, write: impl FnOnce(&mut std::fs::File) -> Result<()>) -> Result<()> {
    let parent = path.parent().context("output path has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    write(temp.as_file_mut())?;
    temp.as_file_mut().flush()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn srt_timestamp(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1_000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        millis / 3_600_000,
        (millis / 60_000) % 60,
        (millis / 1_000) % 60,
        millis % 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_srt_compatible() {
        assert_eq!(srt_timestamp(62.25), "00:01:02,250");
    }

    #[test]
    fn unsupported_models_fail_before_native_initialization() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tokens.txt"), b"tokens").unwrap();
        let config = TranscriptionConfig {
            selected_model_id: Some("unknown".into()),
            ..Default::default()
        };
        assert!(recognizer_config(dir.path(), &config)
            .unwrap_err()
            .contains("Unsupported"));
    }
}
