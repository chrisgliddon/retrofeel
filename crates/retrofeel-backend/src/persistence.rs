use std::path::{Path, PathBuf};

use libretro_host::{abi, Core, CoreError};
use retrofeel_types::RetroFeelConfig;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("core error: {0}")]
    Core(#[from] CoreError),
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn sram_path(config: &RetroFeelConfig, core_key: &str, rom_path: Option<&Path>) -> PathBuf {
    config
        .paths
        .saves
        .join(safe_component(core_key))
        .join(format!("{}.srm", rom_key(rom_path)))
}

pub fn save_state_path(
    config: &RetroFeelConfig,
    core_key: &str,
    rom_path: Option<&Path>,
    slot: u8,
) -> PathBuf {
    config
        .paths
        .states
        .join(safe_component(core_key))
        .join(rom_key(rom_path))
        .join(format!("slot-{slot:02}.state"))
}

pub fn load_sram(
    core: &mut Core,
    config: &RetroFeelConfig,
    core_key: &str,
    rom_path: Option<&Path>,
) -> Result<Option<usize>, PersistenceError> {
    let path = sram_path(config, core_key, rom_path);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|source| PersistenceError::Read {
        path: path.clone(),
        source,
    })?;
    let len = bytes.len();
    core.set_memory(abi::MEMORY_SAVE_RAM, &bytes)?;
    Ok(Some(len))
}

pub fn flush_sram(
    core: &mut Core,
    config: &RetroFeelConfig,
    core_key: &str,
    rom_path: Option<&Path>,
) -> Result<Option<usize>, PersistenceError> {
    let Some(bytes) = core.memory(abi::MEMORY_SAVE_RAM)? else {
        return Ok(None);
    };
    let path = sram_path(config, core_key, rom_path);
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let len = bytes.len();
    std::fs::write(&path, bytes).map_err(|source| PersistenceError::Write { path, source })?;
    Ok(Some(len))
}

pub fn save_state_slot(
    core: &mut Core,
    config: &RetroFeelConfig,
    core_key: &str,
    rom_path: Option<&Path>,
    slot: u8,
) -> Result<usize, PersistenceError> {
    let state = core.serialize()?;
    let path = save_state_path(config, core_key, rom_path, slot);
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let len = state.len();
    std::fs::write(&path, state).map_err(|source| PersistenceError::Write { path, source })?;
    Ok(len)
}

pub fn load_state_slot(
    core: &mut Core,
    config: &RetroFeelConfig,
    core_key: &str,
    rom_path: Option<&Path>,
    slot: u8,
) -> Result<Option<usize>, PersistenceError> {
    let path = save_state_path(config, core_key, rom_path, slot);
    if !path.exists() {
        return Ok(None);
    }
    let state = std::fs::read(&path).map_err(|source| PersistenceError::Read {
        path: path.clone(),
        source,
    })?;
    let len = state.len();
    core.unserialize(&state)?;
    Ok(Some(len))
}

fn create_dir(path: &Path) -> Result<(), PersistenceError> {
    std::fs::create_dir_all(path).map_err(|source| PersistenceError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}

fn rom_key(rom_path: Option<&Path>) -> String {
    let Some(path) = rom_path else {
        return "no-content".to_string();
    };
    let name = path
        .file_stem()
        .or_else(|| path.file_name())
        .and_then(|name| name.to_str())
        .map(safe_component)
        .unwrap_or_else(|| "rom".to_string());
    let hash = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
    format!("{name}-{}", &hash[..8])
}

fn safe_component(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libretro_host::Core;

    fn mock_core_path() -> PathBuf {
        PathBuf::from(env!("MOCK_CORE_PATH"))
    }

    #[test]
    fn sram_flush_and_load_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let config = RetroFeelConfig::with_data_base(temp.path().join("data"));
        let rom_path = temp.path().join("game.rom");
        std::fs::write(&rom_path, b"rom").unwrap();

        let mut core = Core::load(mock_core_path(), config.paths.system.to_str().unwrap()).unwrap();
        core.load_game(&[], Some(rom_path.to_str().unwrap()))
            .unwrap();
        let mut sram = core.memory(abi::MEMORY_SAVE_RAM).unwrap().unwrap();
        sram[0] = 0x42;
        core.set_memory(abi::MEMORY_SAVE_RAM, &sram).unwrap();

        let len = flush_sram(&mut core, &config, "mock-core", Some(&rom_path))
            .unwrap()
            .unwrap();
        assert_eq!(len, sram.len());

        let mut restored =
            Core::load(mock_core_path(), config.paths.system.to_str().unwrap()).unwrap();
        restored
            .load_game(&[], Some(rom_path.to_str().unwrap()))
            .unwrap();
        load_sram(&mut restored, &config, "mock-core", Some(&rom_path))
            .unwrap()
            .unwrap();

        let restored_sram = restored.memory(abi::MEMORY_SAVE_RAM).unwrap().unwrap();
        assert_eq!(restored_sram[0], 0x42);
    }

    #[test]
    fn save_state_slot_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let config = RetroFeelConfig::with_data_base(temp.path().join("data"));

        let mut core = Core::load(mock_core_path(), config.paths.system.to_str().unwrap()).unwrap();
        core.load_game(&[], None).unwrap();
        core.run_frame(Default::default()).unwrap();
        let saved = save_state_slot(&mut core, &config, "mock-core", None, 3).unwrap();

        core.run_frame(Default::default()).unwrap();
        let loaded = load_state_slot(&mut core, &config, "mock-core", None, 3)
            .unwrap()
            .unwrap();

        assert_eq!(saved, loaded);
    }
}
