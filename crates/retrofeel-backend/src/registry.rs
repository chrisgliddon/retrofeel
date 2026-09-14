use std::path::{Path, PathBuf};

use libretro_host::Core;
use retrofeel_types::RetroFeelConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreDescriptor {
    pub path: PathBuf,
    pub library_name: String,
    pub library_version: String,
    pub valid_extensions: Vec<String>,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

impl CoreDescriptor {
    pub fn supports_extension(&self, extension: &str) -> bool {
        let wanted = normalize_extension(extension);
        self.valid_extensions
            .iter()
            .any(|ext| normalize_extension(ext) == wanted)
    }

    pub fn key(&self) -> &str {
        &self.library_name
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreRegistry {
    pub cores: Vec<CoreDescriptor>,
}

impl CoreRegistry {
    /// Scan a cores directory and build a registry of [`CoreDescriptor`]s.
    ///
    /// Uses [`Core::probe`] (static info only — no `retro_init`, no callbacks)
    /// rather than [`Core::load`], so a core that hard-`exit()`s during init
    /// (e.g. FB Alpha / MAME when their BIOS is missing) cannot take the
    /// process down during the per-launch scan. `system_dir` is retained in
    /// the signature for API stability and future use but is not consulted
    /// during probing.
    pub fn scan_dir(
        cores_dir: impl AsRef<Path>,
        _system_dir: impl AsRef<Path>,
    ) -> Result<CoreScanReport, RegistryError> {
        let cores_dir = cores_dir.as_ref();
        let entries = std::fs::read_dir(cores_dir).map_err(|source| RegistryError::ReadDir {
            path: cores_dir.to_path_buf(),
            source,
        })?;

        let mut cores = Vec::new();
        let mut failures = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    failures.push(CoreScanFailure {
                        path: cores_dir.to_path_buf(),
                        error: source.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() || !looks_like_core_library(&path) {
                continue;
            }

            match Core::probe(&path) {
                Ok(info) => {
                    cores.push(CoreDescriptor {
                        path,
                        library_name: info.library_name.clone(),
                        library_version: info.library_version.clone(),
                        valid_extensions: info
                            .valid_extensions
                            .iter()
                            .map(|ext| normalize_extension(ext))
                            .collect(),
                        need_fullpath: info.need_fullpath,
                        block_extract: info.block_extract,
                    });
                }
                Err(source) => failures.push(CoreScanFailure {
                    path,
                    error: source.to_string(),
                }),
            }
        }

        cores.sort_by(|a, b| {
            a.library_name
                .cmp(&b.library_name)
                .then_with(|| a.path.cmp(&b.path))
        });

        Ok(CoreScanReport {
            registry: CoreRegistry { cores },
            failures,
        })
    }

    pub fn find_by_path(&self, path: impl AsRef<Path>) -> Option<&CoreDescriptor> {
        let path = path.as_ref();
        self.cores.iter().find(|core| paths_match(&core.path, path))
    }

    pub fn resolve_rom<'a>(
        &'a self,
        rom_path: impl AsRef<Path>,
        config: Option<&RetroFeelConfig>,
        explicit_core: Option<&Path>,
    ) -> Option<&'a CoreDescriptor> {
        if let Some(core_path) = explicit_core {
            if let Some(core) = self.find_by_path(core_path) {
                return Some(core);
            }
        }

        let rom_path = rom_path.as_ref();
        if let Some(core_path) = config.and_then(|config| config.core_override_for_rom(rom_path)) {
            if let Some(core) = self.find_by_path(core_path) {
                return Some(core);
            }
        }

        let ext = rom_path.extension()?.to_string_lossy();
        self.cores
            .iter()
            .find(|core| core.supports_extension(ext.as_ref()))
    }

    pub fn resolve_rom_path(
        &self,
        rom_path: impl AsRef<Path>,
        config: Option<&RetroFeelConfig>,
        explicit_core: Option<&Path>,
    ) -> Option<PathBuf> {
        if let Some(core_path) = explicit_core {
            return Some(core_path.to_path_buf());
        }
        if let Some(core_path) =
            config.and_then(|config| config.core_override_for_rom(rom_path.as_ref()))
        {
            return Some(core_path.clone());
        }
        self.resolve_rom(rom_path, config, None)
            .map(|core| core.path.clone())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreScanReport {
    pub registry: CoreRegistry,
    pub failures: Vec<CoreScanFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreScanFailure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("failed to read cores directory {path}: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn looks_like_core_library(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "so" | "dll" | "dylib")
}

fn normalize_extension(extension: &str) -> String {
    extension.trim_start_matches('.').to_ascii_lowercase()
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_core_path() -> PathBuf {
        PathBuf::from(env!("MOCK_CORE_PATH"))
    }

    #[test]
    fn scans_cores_and_resolves_rom_by_extension() {
        let temp = tempfile::tempdir().unwrap();
        let cores_dir = temp.path().join("cores");
        let system_dir = temp.path().join("system");
        std::fs::create_dir_all(&cores_dir).unwrap();
        std::fs::create_dir_all(&system_dir).unwrap();
        let core_path = cores_dir.join(mock_core_path().file_name().unwrap());
        std::fs::copy(mock_core_path(), &core_path).unwrap();

        let report = CoreRegistry::scan_dir(&cores_dir, &system_dir).unwrap();

        assert!(report.failures.is_empty());
        assert_eq!(report.registry.cores.len(), 1);
        let core = report
            .registry
            .resolve_rom(temp.path().join("game.rom"), None, None)
            .unwrap();
        assert_eq!(core.library_name, "mock-core");
    }

    #[test]
    fn config_override_takes_precedence_over_extension_scan() {
        let temp = tempfile::tempdir().unwrap();
        let core_a = temp.path().join("mock_a.so");
        let core_b = temp.path().join("mock_b.so");
        let registry = CoreRegistry {
            cores: vec![
                CoreDescriptor {
                    path: core_a.clone(),
                    library_name: "a".into(),
                    library_version: "1".into(),
                    valid_extensions: vec!["rom".into()],
                    need_fullpath: false,
                    block_extract: false,
                },
                CoreDescriptor {
                    path: core_b.clone(),
                    library_name: "b".into(),
                    library_version: "1".into(),
                    valid_extensions: vec!["rom".into()],
                    need_fullpath: false,
                    block_extract: false,
                },
            ],
        };
        let mut config = RetroFeelConfig::default();
        config.set_core_override_for_extension("rom", &core_b);

        let resolved = registry
            .resolve_rom("game.rom", Some(&config), None)
            .unwrap();

        assert_eq!(resolved.path, core_b);
    }
}
