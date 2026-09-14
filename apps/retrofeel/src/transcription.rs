//! Curated local transcription choices and verified, resumable installation.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use retrofeel_types::{TranscriptionModelDescriptor, TranscriptionProvider};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub enum ModelInstallEvent {
    Progress { downloaded: u64, total: u64 },
    Complete { model_path: PathBuf },
    Cancelled,
    Failed(String),
}

pub fn model_catalog() -> Vec<TranscriptionModelDescriptor> {
    vec![
        TranscriptionModelDescriptor {
            schema_version: 1,
            id: "parakeet-tdt-0.6b-v3-int8".into(),
            display_name: "Parakeet TDT 0.6B v3 INT8".into(),
            provider: TranscriptionProvider::SherpaOnnx,
            languages: vec!["25 European languages".into()],
            download_bytes: 487_170_055,
            installed_bytes: 760 * 1024 * 1024,
            memory_mib: 2_048,
            source_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2".into(),
            version: "0.6b-v3 / sherpa-onnx 1.13.4".into(),
            license: "CC BY 4.0".into(),
            sha256: "5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf".into(),
            recommended: true,
        },
        TranscriptionModelDescriptor {
            schema_version: 1,
            id: "sensevoice-small-int8-2025-09-09".into(),
            display_name: "SenseVoice Small INT8".into(),
            provider: TranscriptionProvider::SherpaOnnx,
            languages: vec!["Chinese, Cantonese, English, Japanese, Korean".into()],
            download_bytes: 165_783_878,
            installed_bytes: 245 * 1024 * 1024,
            memory_mib: 768,
            source_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2".into(),
            version: "2025-09-09 / sherpa-onnx 1.13.4".into(),
            license: "Apache-2.0 model export; upstream model terms apply".into(),
            sha256: "7305f7905bfcf77fa0b39388a313f3da35c68d971661a65475b56fb2162c8e63".into(),
            recommended: false,
        },
        TranscriptionModelDescriptor {
            schema_version: 1,
            id: "whisper-small-multilingual-int8".into(),
            display_name: "Whisper Small multilingual".into(),
            provider: TranscriptionProvider::SherpaOnnx,
            languages: vec!["Multilingual".into()],
            download_bytes: 639_387_718,
            installed_bytes: 700 * 1024 * 1024,
            memory_mib: 1_536,
            source_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-small.tar.bz2".into(),
            version: "small / sherpa-onnx 1.13.4".into(),
            license: "MIT".into(),
            sha256: "486a46afbb7ba798507190ffe02fea2dd726049af212e774537efac6afb210a6".into(),
            recommended: false,
        },
        TranscriptionModelDescriptor {
            schema_version: 1,
            id: "whisper-large-v3-turbo-q5".into(),
            display_name: "Whisper Large v3 Turbo Q5".into(),
            provider: TranscriptionProvider::WhisperCpp,
            languages: vec!["Multilingual · broadest quality".into()],
            download_bytes: 574_041_195,
            installed_bytes: 574_041_195,
            memory_mib: 2_048,
            source_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin".into(),
            version: "large-v3-turbo-q5_0".into(),
            license: "MIT".into(),
            sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2".into(),
            recommended: false,
        },
    ]
}

/// Download, verify, and atomically install a catalog model. A partial archive
/// is deliberately retained after cancellation or a transient error so the
/// next attempt can resume with an HTTP Range request.
pub fn install_model(
    descriptor: &TranscriptionModelDescriptor,
    models_dir: &Path,
    cancelled: Arc<AtomicBool>,
    report: impl Fn(ModelInstallEvent),
) -> Result<PathBuf, String> {
    if descriptor.license.trim().is_empty() {
        return Err("model license metadata is missing; installation was refused".into());
    }
    if descriptor.sha256.len() != 64 {
        return Err("model catalog does not contain a valid SHA-256 checksum".into());
    }
    fs::create_dir_all(models_dir).map_err(|error| error.to_string())?;
    let downloads = models_dir.join(".downloads");
    fs::create_dir_all(&downloads).map_err(|error| error.to_string())?;
    let partial = downloads.join(format!("{}.part", descriptor.id));
    let already = partial
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let required = descriptor
        .installed_bytes
        .saturating_add(descriptor.download_bytes.saturating_sub(already))
        .saturating_add(256 * 1024 * 1024);
    let available = fs2::available_space(models_dir).map_err(|error| error.to_string())?;
    if available < required {
        return Err(format!(
            "not enough disk space: need about {} MiB, only {} MiB is available",
            required / (1024 * 1024),
            available / (1024 * 1024)
        ));
    }

    download_resumable(descriptor, &partial, &cancelled, &report)?;
    if cancelled.load(Ordering::Acquire) {
        report(ModelInstallEvent::Cancelled);
        return Err("cancelled".into());
    }
    verify_sha256(&partial, &descriptor.sha256)?;

    let install = models_dir.join(&descriptor.id);
    let staging = models_dir.join(format!(
        ".installing-{}-{}",
        descriptor.id,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    if descriptor.source_url.ends_with(".tar.bz2") {
        let archive = File::open(&partial).map_err(|error| error.to_string())?;
        let decoder = bzip2::read::BzDecoder::new(archive);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(&staging)
            .map_err(|error| format!("could not extract model: {error}"))?;
    } else {
        fs::copy(&partial, staging.join("model.bin")).map_err(|error| error.to_string())?;
    }
    let backup = models_dir.join(format!(
        ".previous-{}-{}",
        descriptor.id,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&backup);
    if install.exists() {
        fs::rename(&install, &backup).map_err(|error| error.to_string())?;
    }
    if let Err(error) = fs::rename(&staging, &install) {
        if backup.exists() {
            let _ = fs::rename(&backup, &install);
        }
        return Err(error.to_string());
    }
    let _ = fs::remove_dir_all(&backup);
    if let Err(error) = fs::remove_file(&partial) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.to_string());
        }
    }
    let model_path = if descriptor.provider == TranscriptionProvider::WhisperCpp {
        install.join("model.bin")
    } else {
        find_model_root(&install).unwrap_or(install)
    };
    report(ModelInstallEvent::Complete {
        model_path: model_path.clone(),
    });
    Ok(model_path)
}

fn download_resumable(
    descriptor: &TranscriptionModelDescriptor,
    partial: &Path,
    cancelled: &AtomicBool,
    report: &impl Fn(ModelInstallEvent),
) -> Result<(), String> {
    let mut offset = partial
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if offset == descriptor.download_bytes {
        report(ModelInstallEvent::Progress {
            downloaded: offset,
            total: descriptor.download_bytes,
        });
        return Ok(());
    }
    if offset > descriptor.download_bytes {
        fs::remove_file(partial).map_err(|error| error.to_string())?;
        offset = 0;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let mut last_error = None;
    let mut response = None;
    for attempt in 0..3 {
        if cancelled.load(Ordering::Acquire) {
            report(ModelInstallEvent::Cancelled);
            return Err("cancelled".into());
        }
        let mut request = agent.get(&descriptor.source_url);
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-"));
        }
        match request.call() {
            Ok(value) => {
                response = Some(value);
                break;
            }
            Err(error) => {
                last_error = Some(error.to_string());
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
                }
            }
        }
    }
    let response = response.ok_or_else(|| {
        format!(
            "model download failed after three attempts: {}",
            last_error.unwrap_or_else(|| "unknown network error".into())
        )
    })?;
    let resumed = offset > 0 && response.status() == 206;
    if offset > 0 && !resumed {
        offset = 0;
    }
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!resumed)
        .open(partial)
        .map_err(|error| error.to_string())?;
    if resumed {
        output
            .seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
    }
    let mut input = response.into_reader();
    let mut buffer = [0_u8; 64 * 1024];
    let mut last_percent = u64::MAX;
    loop {
        if cancelled.load(Ordering::Acquire) {
            report(ModelInstallEvent::Cancelled);
            return Err("cancelled".into());
        }
        let count = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| error.to_string())?;
        offset = offset.saturating_add(count as u64);
        let percent = offset.saturating_mul(100) / descriptor.download_bytes.max(1);
        if percent != last_percent {
            last_percent = percent;
            report(ModelInstallEvent::Progress {
                downloaded: offset,
                total: descriptor.download_bytes,
            });
        }
    }
    output.sync_all().map_err(|error| error.to_string())?;
    if offset != descriptor.download_bytes {
        return Err(format!(
            "download was incomplete: received {offset} of {} bytes",
            descriptor.download_bytes
        ));
    }
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    if expected.len() != 64 {
        return Err("model catalog does not contain a valid SHA-256 checksum".into());
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual == expected.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(format!(
            "model checksum mismatch: expected {expected}, got {actual}"
        ))
    }
}

fn find_model_root(install: &Path) -> Option<PathBuf> {
    let mut pending = vec![install.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if dir.join("tokens.txt").is_file() {
            return Some(dir);
        }
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    None
}

pub fn recommended_model_id(locale: Option<&str>) -> &'static str {
    let locale = locale.unwrap_or_default().to_ascii_lowercase();
    if ["zh", "yue", "ja", "ko"]
        .iter()
        .any(|prefix| locale.starts_with(prefix))
    {
        "sensevoice-small-int8-2025-09-09"
    } else if locale.is_empty()
        || [
            "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv",
            "lt", "mt", "pl", "pt", "ro", "ru", "sk", "sl", "es", "sv", "uk",
        ]
        .iter()
        .any(|prefix| locale.starts_with(prefix))
    {
        "parakeet-tdt-0.6b-v3-int8"
    } else {
        "whisper-large-v3-turbo-q5"
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn catalog_contains_the_four_product_choices() {
        let catalog = model_catalog();
        assert_eq!(catalog.len(), 4);
        assert!(catalog.iter().all(|model| !model.languages.is_empty()));
        assert!(catalog.iter().all(|model| model.download_bytes > 0));
        assert!(catalog.iter().all(|model| model.sha256.len() == 64));
    }

    #[test]
    fn locale_recommendations_cover_cjk_european_and_other() {
        assert!(recommended_model_id(Some("ja-JP")).starts_with("sensevoice"));
        assert!(recommended_model_id(Some("fr-FR")).starts_with("parakeet"));
        assert!(recommended_model_id(Some("ar-SA")).contains("turbo"));
    }

    #[test]
    fn checksum_failure_is_actionable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("model.part");
        std::fs::write(&path, b"not a model").unwrap();
        let error = verify_sha256(&path, &"0".repeat(64)).unwrap_err();
        assert!(error.contains("checksum mismatch"));
    }

    #[test]
    fn interrupted_download_resumes_with_a_range_request() {
        let bytes = b"complete-model-archive";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("Range: bytes=8-"));
            let remaining = &bytes[8..];
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                remaining.len()
            )
            .unwrap();
            stream.write_all(remaining).unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("model.part");
        std::fs::write(&partial, &bytes[..8]).unwrap();
        let mut descriptor = model_catalog().remove(0);
        descriptor.source_url = format!("http://{address}/model");
        descriptor.download_bytes = bytes.len() as u64;
        download_resumable(&descriptor, &partial, &AtomicBool::new(false), &|_| {}).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(partial).unwrap(), bytes);
    }

    /// Opt-in release/manual smoke used with a real upstream artifact. The
    /// normal suite never downloads model data.
    #[test]
    #[ignore = "downloads a large real model; set RETROFEEL_MODEL_ID and RETROFEEL_MODEL_DIR"]
    fn installs_a_real_catalog_model() {
        let id = std::env::var("RETROFEEL_MODEL_ID").expect("RETROFEEL_MODEL_ID");
        let dir =
            PathBuf::from(std::env::var_os("RETROFEEL_MODEL_DIR").expect("RETROFEEL_MODEL_DIR"));
        let descriptor = model_catalog()
            .into_iter()
            .find(|descriptor| descriptor.id == id)
            .expect("catalog model ID");
        let path = install_model(
            &descriptor,
            &dir,
            Arc::new(AtomicBool::new(false)),
            |event| eprintln!("{event:?}"),
        )
        .expect("verified model installation");
        assert!(path.exists());
    }
}
