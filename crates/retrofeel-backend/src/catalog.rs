//! Libretro buildbot catalog and managed core installation.
//!
//! The network-facing functions are intentionally small wrappers around pure
//! parsing/extraction helpers so tests can cover catalog parsing and archive
//! safety without depending on the live buildbot.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use libretro_host::{Core, CoreError};
use retrofeel_types::system_for_core_name;
use scraper::{Html, Selector};
use thiserror::Error;

use crate::registry::{looks_like_core_library, CoreRegistry, CoreScanReport, RegistryError};

const BUILDBOT_BASE_URL: &str = "https://buildbot.libretro.com";
const INFO_ZIP_URL: &str = "https://buildbot.libretro.com/assets/frontend/info.zip";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreCatalogEntry {
    pub slug: String,
    pub archive_name: String,
    pub display_name: String,
    pub inferred_system: Option<String>,
    pub installed_path: Option<PathBuf>,
    pub status: CoreInstallStatus,
    pub download_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreInstallStatus {
    Available,
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildbotPlatform {
    pub os: String,
    pub arch: String,
    pub buildbot_path: String,
    pub library_extension: String,
}

impl BuildbotPlatform {
    pub fn detect() -> Result<Self, CoreCatalogError> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            CoreCatalogError::UnsupportedPlatform {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
            }
        })
    }

    pub fn for_target(os: &str, arch: &str) -> Option<Self> {
        let (buildbot_path, library_extension) = match (os, arch) {
            ("macos", "aarch64") | ("macos", "arm64") => ("apple/osx/arm64", "dylib"),
            ("macos", "x86_64") => ("apple/osx/x86_64", "dylib"),
            ("linux", "x86_64") => ("linux/x86_64", "so"),
            ("linux", "aarch64") => ("linux/aarch64", "so"),
            ("windows", "x86_64") => ("windows/x86_64", "dll"),
            _ => return None,
        };
        Some(Self {
            os: os.to_string(),
            arch: arch.to_string(),
            buildbot_path: buildbot_path.to_string(),
            library_extension: library_extension.to_string(),
        })
    }

    pub fn index_url(&self) -> String {
        format!(
            "{BUILDBOT_BASE_URL}/nightly/{}/latest/",
            self.buildbot_path.trim_matches('/')
        )
    }

    fn archive_suffix(&self) -> String {
        format!("_libretro.{}.zip", self.library_extension)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreInstallResult {
    pub installed_path: PathBuf,
    pub scan_report: CoreScanReport,
}

#[derive(Debug, Error)]
pub enum CoreCatalogError {
    #[error("unsupported buildbot platform: {os}/{arch}")]
    UnsupportedPlatform { os: String, arch: String },
    #[error("HTTP request failed for {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse zip archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("buildbot index did not contain parseable anchors")]
    BadIndexSelector,
    #[error("archive entry uses an unsafe path: {entry}")]
    UnsafeArchivePath { entry: String },
    #[error("core archive did not contain a libretro dynamic library")]
    NoCoreLibraryInArchive,
    #[error("installed core {path} did not load: {source}")]
    InstallValidation {
        path: PathBuf,
        #[source]
        source: CoreError,
    },
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

pub fn refresh_core_catalog(
    cores_dir: impl AsRef<Path>,
    system_dir: impl AsRef<Path>,
) -> Result<Vec<CoreCatalogEntry>, CoreCatalogError> {
    let platform = BuildbotPlatform::detect()?;
    refresh_core_catalog_for_platform(cores_dir, system_dir, &platform)
}

pub fn refresh_core_catalog_for_platform(
    cores_dir: impl AsRef<Path>,
    _system_dir: impl AsRef<Path>,
    platform: &BuildbotPlatform,
) -> Result<Vec<CoreCatalogEntry>, CoreCatalogError> {
    let index_url = platform.index_url();
    let html = download_text(&index_url)?;
    let metadata = match download_bytes(INFO_ZIP_URL).and_then(|bytes| parse_info_zip(&bytes)) {
        Ok(metadata) => metadata,
        Err(error) => {
            log::warn!("core info metadata unavailable: {error}");
            BTreeMap::new()
        }
    };
    let installed = installed_core_paths(cores_dir)?;
    parse_core_catalog(&html, platform, &metadata, &installed)
}

pub fn parse_core_catalog(
    html: &str,
    platform: &BuildbotPlatform,
    metadata: &BTreeMap<String, CoreInfoMetadata>,
    installed: &BTreeMap<String, PathBuf>,
) -> Result<Vec<CoreCatalogEntry>, CoreCatalogError> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a").map_err(|_| CoreCatalogError::BadIndexSelector)?;
    let suffix = platform.archive_suffix();
    let base_url = platform.index_url();
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();

    for anchor in document.select(&selector) {
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let archive_name = href.rsplit('/').next().unwrap_or(href).trim().to_string();
        if !archive_name.ends_with(&suffix) || !seen.insert(archive_name.clone()) {
            continue;
        }

        let slug = archive_name
            .trim_end_matches(".zip")
            .trim_end_matches(&format!("_libretro.{}", platform.library_extension))
            .to_string();
        let info_name = format!("{slug}_libretro.info");
        let info = metadata.get(&info_name);
        let display_name = info
            .and_then(|info| info.display_name.clone())
            .unwrap_or_else(|| titleize_slug(&slug));
        let inferred_system = info
            .and_then(|info| info.system_name.clone())
            .or_else(|| info.and_then(|info| info.database.clone()))
            .or_else(|| {
                system_for_core_name(
                    info.and_then(|info| info.core_name.as_deref())
                        .unwrap_or(display_name.as_str()),
                )
                .map(|system| system.name.to_string())
            });
        let archive_stem = archive_name.trim_end_matches(".zip");
        let library_stem = archive_stem
            .trim_end_matches(&format!(".{}", platform.library_extension))
            .to_ascii_lowercase();
        let installed_path = installed
            .get(&archive_stem.to_ascii_lowercase())
            .cloned()
            .or_else(|| installed.get(&library_stem).cloned())
            .or_else(|| installed.get(&slug.to_ascii_lowercase()).cloned());
        let status = if installed_path.is_some() {
            CoreInstallStatus::Installed
        } else {
            CoreInstallStatus::Available
        };
        entries.push(CoreCatalogEntry {
            slug,
            archive_name: archive_name.clone(),
            display_name,
            inferred_system,
            installed_path,
            status,
            download_url: join_url(&base_url, &archive_name),
        });
    }

    entries.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.archive_name.cmp(&b.archive_name))
    });
    Ok(entries)
}

pub fn install_core(
    entry: &CoreCatalogEntry,
    cores_dir: impl AsRef<Path>,
    system_dir: impl AsRef<Path>,
) -> Result<CoreInstallResult, CoreCatalogError> {
    let bytes = download_bytes(&entry.download_url)?;
    install_core_from_zip(entry, &bytes, cores_dir, system_dir)
}

pub fn install_core_from_zip(
    _entry: &CoreCatalogEntry,
    bytes: &[u8],
    cores_dir: impl AsRef<Path>,
    system_dir: impl AsRef<Path>,
) -> Result<CoreInstallResult, CoreCatalogError> {
    let cores_dir = cores_dir.as_ref();
    let system_dir = system_dir.as_ref();
    create_dir_all(cores_dir)?;

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut extracted = None;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let safe_path = safe_archive_path(file.name())?;
        if file.is_dir() {
            continue;
        }
        let Some(file_name) = safe_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !looks_like_core_library(Path::new(file_name)) {
            continue;
        }

        let dest = cores_dir.join(file_name);
        let part = cores_dir.join(format!(".{file_name}.part"));
        {
            let mut out = std::fs::File::create(&part).map_err(|source| CoreCatalogError::Io {
                path: part.clone(),
                source,
            })?;
            std::io::copy(&mut file, &mut out).map_err(|source| CoreCatalogError::Io {
                path: part.clone(),
                source,
            })?;
            out.flush().map_err(|source| CoreCatalogError::Io {
                path: part.clone(),
                source,
            })?;
        }
        if dest.exists() {
            std::fs::remove_file(&dest).map_err(|source| CoreCatalogError::Io {
                path: dest.clone(),
                source,
            })?;
        }
        std::fs::rename(&part, &dest).map_err(|source| CoreCatalogError::Io {
            path: dest.clone(),
            source,
        })?;
        extracted = Some(dest);
        break;
    }

    let installed_path = extracted.ok_or(CoreCatalogError::NoCoreLibraryInArchive)?;
    // Validate the extracted core via `Core::probe` (static info only — no
    // `retro_init`, no callbacks) rather than `Core::load`. This is the crash
    // fix: cores like FB Alpha 2012 Neo Geo call C `exit()` from inside
    // `retro_init` when their required BIOS is missing, and a C `exit()` from
    // a worker thread kills the whole process (no Rust `catch_unwind` can
    // intercept it). `retro_get_system_info` is a pure static-data call that
    // doesn't touch the filesystem, so it's safe to run on the install worker
    // thread. Full initialization happens lazily in `Core::load` on the
    // dedicated core thread when the user actually launches a game, where a
    // crash is recoverable.
    Core::probe(&installed_path).map_err(|source| CoreCatalogError::InstallValidation {
        path: installed_path.clone(),
        source,
    })?;
    let scan_report = CoreRegistry::scan_dir(cores_dir, system_dir)?;
    Ok(CoreInstallResult {
        installed_path,
        scan_report,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreInfoMetadata {
    pub display_name: Option<String>,
    pub core_name: Option<String>,
    pub system_name: Option<String>,
    pub database: Option<String>,
}

pub fn parse_info_zip(
    bytes: &[u8],
) -> Result<BTreeMap<String, CoreInfoMetadata>, CoreCatalogError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut metadata = BTreeMap::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if file.is_dir() || !file.name().ends_with("_libretro.info") {
            continue;
        }
        let name = file
            .name()
            .rsplit('/')
            .next()
            .unwrap_or(file.name())
            .to_string();
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|source| CoreCatalogError::Io {
                path: PathBuf::from(name.clone()),
                source,
            })?;
        metadata.insert(name, parse_info_metadata(&text));
    }
    Ok(metadata)
}

fn parse_info_metadata(text: &str) -> CoreInfoMetadata {
    let mut info = CoreInfoMetadata::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "display_name" => info.display_name = Some(value),
            "corename" => info.core_name = Some(value),
            "systemname" => info.system_name = Some(value),
            "database" => info.database = value.split('|').next().map(|s| s.to_string()),
            _ => {}
        }
    }
    info
}

fn installed_core_paths(
    cores_dir: impl AsRef<Path>,
) -> Result<BTreeMap<String, PathBuf>, CoreCatalogError> {
    let cores_dir = cores_dir.as_ref();
    let mut installed = BTreeMap::new();
    let entries = match std::fs::read_dir(cores_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(installed),
        Err(source) => {
            return Err(CoreCatalogError::Io {
                path: cores_dir.to_path_buf(),
                source,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| CoreCatalogError::Io {
            path: cores_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() || !looks_like_core_library(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let stem = name
            .trim_end_matches(".so")
            .trim_end_matches(".dll")
            .trim_end_matches(".dylib")
            .to_ascii_lowercase();
        installed.insert(stem.clone(), path.clone());
        if let Some(slug) = stem.strip_suffix("_libretro") {
            installed.insert(slug.to_string(), path);
        }
    }
    Ok(installed)
}

fn safe_archive_path(entry: &str) -> Result<PathBuf, CoreCatalogError> {
    let path = Path::new(entry);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(CoreCatalogError::UnsafeArchivePath {
            entry: entry.to_string(),
        });
    }
    Ok(path.to_path_buf())
}

fn create_dir_all(path: &Path) -> Result<(), CoreCatalogError> {
    std::fs::create_dir_all(path).map_err(|source| CoreCatalogError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn download_text(url: &str) -> Result<String, CoreCatalogError> {
    let bytes = download_bytes(url)?;
    String::from_utf8(bytes).map_err(|source| CoreCatalogError::Io {
        path: PathBuf::from(url),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })
}

fn download_bytes(url: &str) -> Result<Vec<u8>, CoreCatalogError> {
    let response = ureq::get(url)
        .call()
        .map_err(|source| CoreCatalogError::Http {
            url: url.to_string(),
            source: Box::new(source),
        })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|source| CoreCatalogError::Io {
            path: PathBuf::from(url),
            source,
        })?;
    Ok(bytes)
}

fn join_url(base: &str, name: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        name.trim_start_matches('/')
    )
}

fn titleize_slug(slug: &str) -> String {
    slug.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mock_core_path() -> PathBuf {
        PathBuf::from(env!("MOCK_CORE_PATH"))
    }

    #[test]
    fn maps_supported_buildbot_platforms() {
        assert_eq!(
            BuildbotPlatform::for_target("macos", "aarch64")
                .unwrap()
                .buildbot_path,
            "apple/osx/arm64"
        );
        assert_eq!(
            BuildbotPlatform::for_target("macos", "x86_64")
                .unwrap()
                .buildbot_path,
            "apple/osx/x86_64"
        );
        assert_eq!(
            BuildbotPlatform::for_target("linux", "x86_64")
                .unwrap()
                .buildbot_path,
            "linux/x86_64"
        );
        assert_eq!(
            BuildbotPlatform::for_target("linux", "aarch64")
                .unwrap()
                .buildbot_path,
            "linux/aarch64"
        );
        assert_eq!(
            BuildbotPlatform::for_target("windows", "x86_64")
                .unwrap()
                .buildbot_path,
            "windows/x86_64"
        );
        assert!(BuildbotPlatform::for_target("freebsd", "x86_64").is_none());
    }

    #[test]
    fn parses_catalog_html_with_info_metadata_and_installed_status() {
        let platform = BuildbotPlatform::for_target("linux", "x86_64").unwrap();
        let html = r#"
            <html><body>
            <a href="..">Parent Directory</a>
            <a href="/nightly/linux/x86_64/latest/snes9x_libretro.so.zip">snes9x_libretro.so.zip</a>
            <a href="gambatte_libretro.so.zip">gambatte_libretro.so.zip</a>
            </body></html>
        "#;
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "snes9x_libretro.info".into(),
            CoreInfoMetadata {
                display_name: Some("Nintendo - SNES / SFC (Snes9x)".into()),
                core_name: Some("Snes9x".into()),
                system_name: Some("Super Nintendo Entertainment System".into()),
                database: None,
            },
        );
        let mut installed = BTreeMap::new();
        installed.insert(
            "gambatte_libretro".into(),
            PathBuf::from("/cores/gambatte.so"),
        );

        let entries = parse_core_catalog(html, &platform, &metadata, &installed).unwrap();

        assert_eq!(entries.len(), 2);
        let snes = entries.iter().find(|entry| entry.slug == "snes9x").unwrap();
        assert_eq!(snes.display_name, "Nintendo - SNES / SFC (Snes9x)");
        assert_eq!(
            snes.inferred_system.as_deref(),
            Some("Super Nintendo Entertainment System")
        );
        assert_eq!(snes.status, CoreInstallStatus::Available);
        let gambatte = entries
            .iter()
            .find(|entry| entry.slug == "gambatte")
            .unwrap();
        assert_eq!(gambatte.status, CoreInstallStatus::Installed);
        assert_eq!(
            gambatte.installed_path.as_deref(),
            Some(Path::new("/cores/gambatte.so"))
        );
    }

    #[test]
    fn safe_archive_path_rejects_zip_slip_entries() {
        assert!(safe_archive_path("nested/core.so").is_ok());
        assert!(matches!(
            safe_archive_path("../evil.so"),
            Err(CoreCatalogError::UnsafeArchivePath { .. })
        ));
        assert!(matches!(
            safe_archive_path("/tmp/evil.so"),
            Err(CoreCatalogError::UnsafeArchivePath { .. })
        ));
    }

    #[test]
    fn installing_zip_slip_archive_is_rejected() {
        let bytes = zip_bytes(&[("../evil.so", b"nope".as_slice())]);
        let temp = tempfile::tempdir().unwrap();
        let entry = test_entry("evil_libretro.so.zip");

        let error = install_core_from_zip(&entry, &bytes, temp.path(), temp.path()).unwrap_err();

        assert!(matches!(error, CoreCatalogError::UnsafeArchivePath { .. }));
    }

    #[test]
    fn installs_zipped_mock_core_and_scans_registry() {
        let temp = tempfile::tempdir().unwrap();
        let cores_dir = temp.path().join("cores");
        let system_dir = temp.path().join("system");
        std::fs::create_dir_all(&system_dir).unwrap();
        let mock = std::fs::read(mock_core_path()).unwrap();
        let file_name = mock_core_path()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        let archive_name = format!("{file_name}.zip");
        let bytes = zip_bytes(&[(&file_name, mock.as_slice())]);
        let entry = test_entry(&archive_name);

        let result = install_core_from_zip(&entry, &bytes, &cores_dir, &system_dir).unwrap();

        assert!(result.installed_path.is_file());
        assert_eq!(result.scan_report.registry.cores.len(), 1);
        assert_eq!(
            result.scan_report.registry.cores[0].library_name,
            "mock-core"
        );
    }

    fn test_entry(archive_name: &str) -> CoreCatalogEntry {
        CoreCatalogEntry {
            slug: "mock".into(),
            archive_name: archive_name.into(),
            display_name: "Mock".into(),
            inferred_system: None,
            installed_path: None,
            status: CoreInstallStatus::Available,
            download_url: format!("https://example.invalid/{archive_name}"),
        }
    }

    fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            for (name, bytes) in files {
                writer.start_file(*name, options).unwrap();
                writer.write_all(bytes).unwrap();
            }
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }
}
