//! Steam game discovery across GameHub and the platform Steam client.
//!
//! An `.acf` file is a Valve Data Format (VDF) key-value text file. We parse
//! it with a minimal line-based parser (no external VDF dependency) to extract
//! `appid`, `name`, and `installdir`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use retrofeel_types::{SteamConfig, SteamGameEntry, SteamGameSource};

/// Scan a GameHub `steamapps` directory for `.acf` files and return the
/// discovered game entries.
///
/// Each entry's `install_dir` is `steamapps/common/<installdir>`. The
/// `wine_prefix` and `exe_name` are left as defaults — the caller (UI/config)
/// fills those in based on the user's Wine setup.
pub fn scan_steamapps(steamapps_dir: &Path) -> Vec<SteamGameEntry> {
    scan_steamapps_with_source(steamapps_dir, SteamGameSource::GameHub)
}

fn scan_steamapps_with_source(
    steamapps_dir: &Path,
    source: SteamGameSource,
) -> Vec<SteamGameEntry> {
    let mut entries = Vec::new();
    let Ok(dir) = std::fs::read_dir(steamapps_dir) else {
        log::warn!(
            "steam: failed to read steamapps dir: {}",
            steamapps_dir.display()
        );
        return entries;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "acf") {
            match parse_acf(&path, source) {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {} // Steamworks Shared etc. — skip
                Err(e) => log::warn!("steam: failed to parse {}: {e}", path.display()),
            }
        }
    }
    entries
}

/// Discover installed games from configured GameHub manifests and every
/// standard native Steam library, deduplicated by App ID. Explicit configured
/// entries win over GameHub, and GameHub wins over a duplicate native install.
pub fn discover_installed_steam_games(config: &SteamConfig) -> Vec<SteamGameEntry> {
    let configured = config
        .games
        .iter()
        .filter(|game| game.source == SteamGameSource::Configured)
        .cloned()
        .collect::<Vec<_>>();
    let gamehub = scan_steamapps(&config.gamehub_steamapps);
    let native = scan_native_steam_libraries();
    let counts = (configured.len(), gamehub.len(), native.len());
    let merged = merge_installs(configured, gamehub, native);
    log::debug!(
        "steam.discovery configured={} gamehub={} native={} merged={}",
        counts.0,
        counts.1,
        counts.2,
        merged.len()
    );
    merged
}

fn merge_installs(
    configured: Vec<SteamGameEntry>,
    gamehub: Vec<SteamGameEntry>,
    native: Vec<SteamGameEntry>,
) -> Vec<SteamGameEntry> {
    let mut by_app_id = BTreeMap::new();
    for game in native {
        by_app_id.insert(game.app_id, game);
    }
    for game in gamehub {
        by_app_id.insert(game.app_id, game);
    }
    for game in configured {
        by_app_id.insert(game.app_id, game);
    }
    let mut games: Vec<_> = by_app_id.into_values().collect();
    games.sort_by_cached_key(|game| game.name.to_lowercase());
    games
}

/// Scan the conventional platform Steam roots and any extra libraries listed
/// by `libraryfolders.vdf`. This is intentionally bounded and never crawls the
/// user's disk.
pub fn scan_native_steam_libraries() -> Vec<SteamGameEntry> {
    let mut by_app_id = BTreeMap::new();
    let mut seen_roots = HashSet::new();
    for root in native_steam_roots() {
        if !seen_roots.insert(root.clone()) {
            continue;
        }
        for game in scan_native_steam_root(&root) {
            by_app_id.insert(game.app_id, game);
        }
    }
    by_app_id.into_values().collect()
}

fn native_steam_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(home) = &home {
        roots.push(home.join("Library/Application Support/Steam"));
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            roots.push(PathBuf::from(data_home).join("Steam"));
        }
        if let Some(home) = &home {
            roots.extend([
                home.join(".local/share/Steam"),
                home.join(".steam/steam"),
                home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
            ]);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = windows_steam_root_from_registry() {
            roots.push(path);
        }
        for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(path) = std::env::var_os(variable) {
                roots.push(PathBuf::from(path).join("Steam"));
            }
        }
    }
    roots
}

#[cfg(target_os = "windows")]
fn windows_steam_root_from_registry() -> Option<PathBuf> {
    let output = std::process::Command::new("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        line.split_once("REG_SZ")
            .map(|(_, path)| PathBuf::from(path.trim()))
    })
}

fn scan_native_steam_root(steam_root: &Path) -> Vec<SteamGameEntry> {
    let mut library_roots = vec![steam_root.to_path_buf()];
    for manifest in [
        steam_root.join("steamapps/libraryfolders.vdf"),
        steam_root.join("config/libraryfolders.vdf"),
    ] {
        let Ok(text) = std::fs::read_to_string(manifest) else {
            continue;
        };
        library_roots.extend(
            parse_vdf_values(&text, "path")
                .into_iter()
                .map(|path| PathBuf::from(path.replace("\\\\", "\\"))),
        );
    }

    let mut seen = HashSet::new();
    let mut games = Vec::new();
    for root in library_roots {
        if !seen.insert(root.clone()) {
            continue;
        }
        let steamapps = if root.file_name().is_some_and(|name| name == "steamapps") {
            root
        } else {
            root.join("steamapps")
        };
        if !steamapps.is_dir() {
            continue;
        }
        games.extend(scan_steamapps_with_source(
            &steamapps,
            SteamGameSource::NativeSteam,
        ));
    }
    games
}

/// Parse a single `.acf` file into a `SteamGameEntry`, or None for non-game
/// entries (like Steamworks Shared Redistributables).
fn parse_acf(path: &Path, source: SteamGameSource) -> std::io::Result<Option<SteamGameEntry>> {
    let text = std::fs::read_to_string(path)?;
    let app_id = parse_vdf_value(&text, "appid")?;
    let name = parse_vdf_value(&text, "name")?;
    let installdir = parse_vdf_value(&text, "installdir")?;

    let app_id: u32 = match app_id.parse() {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };

    // Skip Steamworks Shared Redistributables and other non-game entries.
    if name.contains("Steamworks") || name.contains("Redistributables") {
        return Ok(None);
    }

    let install_dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .join("common")
        .join(&installdir);

    if source == SteamGameSource::NativeSteam {
        if let Ok(state_flags) = parse_vdf_value(&text, "StateFlags") {
            if state_flags.parse::<u32>().unwrap_or(0) & 4 == 0 {
                return Ok(None);
            }
        }
        if !install_dir.is_dir() {
            return Ok(None);
        }
    }

    // GameHub uses its own Wine environment, so RetroFeel only needs a
    // reliable process-name needle for attach. Prefer known game render
    // executables before generic launcher/config-window candidates.
    let exe_name = if source == SteamGameSource::NativeSteam {
        String::new()
    } else {
        find_main_exe(&install_dir).unwrap_or_else(|| format!("{}.exe", installdir))
    };

    Ok(Some(SteamGameEntry {
        app_id,
        name,
        install_dir,
        wine_prefix: PathBuf::new(), // Filled in by caller
        exe_name,
        source,
    }))
}

/// Extract a quoted value for `key` from VDF text. VDF format:
/// ```text
/// "AppState"
/// {
///   "appid"   "43"
///   "name"   "CLOCKWORK VALLEY™ 2"
///   ...
/// }
/// ```
fn parse_vdf_value(text: &str, key: &str) -> std::io::Result<String> {
    parse_vdf_values(text, key)
        .into_iter()
        .next()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("VDF key '{key}' not found"),
            )
        })
}

fn parse_vdf_values(text: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let lines: Vec<_> = text.lines().collect();
    let mut values = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with(&needle) {
            // The value is the next quoted string after the key.
            let rest = &trimmed[needle.len()..];
            if let Some(start) = rest.find('"') {
                let after_start = &rest[start + 1..];
                if let Some(end) = after_start.find('"') {
                    values.push(after_start[..end].to_string());
                    continue;
                }
            }
            // Value might be on the next line.
            let next_line = lines.get(i + 1).copied().unwrap_or("").trim();
            if let Some(start) = next_line.find('"') {
                let after_start = &next_line[start + 1..];
                if let Some(end) = after_start.find('"') {
                    values.push(after_start[..end].to_string());
                }
            }
        }
    }
    values
}

/// Find the game executable in a GameHub install. A shallow recursive scan is
/// enough for common Steam layouts while keeping discovery bounded.
fn find_main_exe(install_dir: &Path) -> Option<String> {
    let mut exes = Vec::new();
    collect_executables(install_dir, install_dir, 0, &mut exes);
    if exes.is_empty() {
        return None;
    }

    // Avoid selecting a launcher/configuration window merely because it is
    // large. The remaining largest executable is the generic fallback.
    exes.retain(|(path, _)| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        ![
            "launcher",
            "config",
            "setup",
            "unins",
            "crashreport",
            "redist",
        ]
        .iter()
        .any(|needle| name.contains(needle))
    });
    exes.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    exes.first()
        .map(|(path, _)| path.to_string_lossy().to_string())
}

fn collect_executables(root: &Path, directory: &Path, depth: u8, exes: &mut Vec<(PathBuf, u64)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < 2 {
            collect_executables(root, &path, depth + 1, exes);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            exes.push((relative, size));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_acf_and_selects_largest_executable() {
        let root = tempfile::tempdir().unwrap();
        let steamapps = root.path();
        let install_dir = steamapps.join("common/My Game");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("launcher.exe"), [0; 4]).unwrap();
        std::fs::write(install_dir.join("game.exe"), [0; 16]).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_42.acf"),
            r#""AppState"
{
    "appid" "42"
    "name" "My Game"
    "installdir" "My Game"
}"#,
        )
        .unwrap();

        let games = scan_steamapps(steamapps);

        assert_eq!(games.len(), 1);
        assert_eq!(games[0].app_id, 42);
        assert_eq!(games[0].name, "My Game");
        assert_eq!(games[0].install_dir, install_dir);
        assert_eq!(games[0].exe_name, "game.exe");
        assert_eq!(games[0].source, SteamGameSource::GameHub);
    }

    #[test]
    fn gamehub_discovery_filters_launchers_for_arbitrary_titles() {
        let root = tempfile::tempdir().unwrap();
        let steamapps = root.path();
        let fields = steamapps.join("common/Meadow of Lanterns");
        let clockworkvalley = steamapps.join("common/CLOCKWORK VALLEY 2");
        std::fs::create_dir_all(&fields).unwrap();
        std::fs::create_dir_all(&clockworkvalley).unwrap();
        std::fs::write(fields.join("MeadowOfLanterns.exe"), [0; 4]).unwrap();
        std::fs::write(fields.join("launcher.exe"), [0; 32]).unwrap();
        std::fs::write(clockworkvalley.join("ClockworkValley.exe"), [0; 4]).unwrap();
        std::fs::write(clockworkvalley.join("ClockworkValleyConfig.exe"), [0; 32]).unwrap();
        write_manifest(steamapps, 42, "Meadow of Lanterns", "Meadow of Lanterns");
        write_manifest(steamapps, 43, "CLOCKWORK VALLEY 2", "CLOCKWORK VALLEY 2");

        let games = scan_steamapps(steamapps);
        assert_eq!(
            games
                .iter()
                .find(|game| game.app_id == 42)
                .unwrap()
                .exe_name,
            "MeadowOfLanterns.exe"
        );
        assert_eq!(
            games
                .iter()
                .find(|game| game.app_id == 43)
                .unwrap()
                .exe_name,
            "ClockworkValley.exe"
        );
    }

    #[test]
    fn skips_shared_redistributables() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("appmanifest_100.acf"),
            r#""AppState"
{
    "appid" "100"
    "name" "Steamworks Common Redistributables"
    "installdir" "Steamworks Shared"
}"#,
        )
        .unwrap();

        assert!(scan_steamapps(root.path()).is_empty());
    }

    #[test]
    fn scans_native_primary_and_additional_libraries() {
        let root = tempfile::tempdir().unwrap();
        let steam_root = root.path().join("Steam");
        let extra_root = root.path().join("Extra Library");
        std::fs::create_dir_all(steam_root.join("steamapps/common/Primary Game")).unwrap();
        std::fs::create_dir_all(extra_root.join("steamapps/common/Extra Game")).unwrap();
        std::fs::create_dir_all(steam_root.join("config")).unwrap();
        std::fs::write(
            steam_root.join("config/libraryfolders.vdf"),
            format!(
                r#""libraryfolders"
{{
    "0" {{ "path" "{}" }}
    "1"
    {{
        "path" "{}"
    }}
}}"#,
                steam_root.display(),
                extra_root.display()
            ),
        )
        .unwrap();
        write_native_manifest(&steam_root, 10, "Primary Game", "Primary Game", 4);
        write_native_manifest(&extra_root, 20, "Extra Game", "Extra Game", 4);

        let games = scan_native_steam_root(&steam_root);

        assert_eq!(games.len(), 2);
        assert!(games
            .iter()
            .all(|game| game.source == SteamGameSource::NativeSteam));
        assert!(games.iter().all(|game| game.exe_name.is_empty()));
        assert!(games.iter().any(|game| game.app_id == 10));
        assert!(games.iter().any(|game| game.app_id == 20));
    }

    #[test]
    fn skips_native_manifests_that_are_not_installed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("steamapps/common/Updating")).unwrap();
        write_native_manifest(root.path(), 30, "Updating", "Updating", 2);
        write_native_manifest(root.path(), 40, "Missing Files", "Missing Files", 4);

        assert!(scan_native_steam_root(root.path()).is_empty());
    }

    #[test]
    fn configured_entries_win_gamehub_and_native_duplicates() {
        let configured = game(42, "Configured", SteamGameSource::Configured);
        let gamehub = game(42, "GameHub", SteamGameSource::GameHub);
        let native = game(42, "Native", SteamGameSource::NativeSteam);

        let merged = merge_installs(vec![configured], vec![gamehub], vec![native]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "Configured");
        assert_eq!(merged[0].source, SteamGameSource::Configured);
    }

    #[test]
    fn gamehub_entries_win_native_duplicates() {
        let gamehub = game(42, "GameHub", SteamGameSource::GameHub);
        let native = game(42, "Native", SteamGameSource::NativeSteam);

        let merged = merge_installs(Vec::new(), vec![gamehub], vec![native]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, SteamGameSource::GameHub);
    }

    fn write_native_manifest(
        steam_root: &Path,
        app_id: u32,
        name: &str,
        install_dir: &str,
        state_flags: u32,
    ) {
        let steamapps = steam_root.join("steamapps");
        std::fs::create_dir_all(&steamapps).unwrap();
        std::fs::write(
            steamapps.join(format!("appmanifest_{app_id}.acf")),
            format!(
                r#""AppState"
{{
    "appid" "{app_id}"
    "name" "{name}"
    "installdir" "{install_dir}"
    "StateFlags" "{state_flags}"
}}"#
            ),
        )
        .unwrap();
    }

    fn write_manifest(steamapps: &Path, app_id: u32, name: &str, install_dir: &str) {
        std::fs::write(
            steamapps.join(format!("appmanifest_{app_id}.acf")),
            format!(
                r#""AppState"
{{
    "appid" "{app_id}"
    "name" "{name}"
    "installdir" "{install_dir}"
}}"#
            ),
        )
        .unwrap();
    }

    fn game(app_id: u32, name: &str, source: SteamGameSource) -> SteamGameEntry {
        SteamGameEntry {
            app_id,
            name: name.to_string(),
            install_dir: PathBuf::from(name),
            wine_prefix: PathBuf::new(),
            exe_name: String::new(),
            source,
        }
    }
}
