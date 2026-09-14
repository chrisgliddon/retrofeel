//! Console/system taxonomy.
//!
//! Maps a ROM's file extension (and, as a fallback, the resolved core's
//! `library_name`) to a canonical game system. This is the shared primitive
//! behind two features: the library's console grouping and per-system BIOS
//! requirements. Both `retrofeel-backend` (BIOS) and the Bevy app (library)
//! resolve a ROM's system through here so the mapping lives in exactly one place.

use std::path::Path;

/// A game console/system retrofeel knows how to categorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct System {
    /// Canonical, stable id (lowercase, no spaces) — e.g. `"snes"`. Used as the
    /// join key for BIOS entries (`KnownBios.system_id`) and persisted config.
    pub id: &'static str,
    /// Human-readable display name — e.g. `"Super Nintendo (SNES)"`.
    pub name: &'static str,
    /// ROM file extensions (lowercase, no leading dot) that map to this system.
    pub extensions: &'static [&'static str],
    /// Directory name used by the libretro-thumbnails project
    /// (`https://thumbnails.libretro.com/<dir>/Named_Boxarts/<game>.png`).
    /// Used by the app's box-art fetcher; must match the upstream repo names.
    pub libretro_thumbnail_dir: &'static str,
    /// OpenVGDB `systemShortName` (the `ROMs.systemID` column key). Used for
    /// hash-based metadata lookup. Empty string = no OpenVGDB mapping.
    pub openvgdb_system_id: &'static str,
}

impl System {
    /// Resolve the libretro-thumbnails catalog for a specific ROM. Most
    /// systems have one catalog, but Neo Geo Pocket and WonderSwan keep color
    /// releases in extension-specific upstream directories.
    pub fn thumbnail_dir_for_rom(&self, path: &Path) -> &'static str {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        match (self.id, extension.as_deref()) {
            ("ngp", Some("ngc")) => "SNK - Neo Geo Pocket Color",
            ("ws", Some("wsc")) => "Bandai - WonderSwan Color",
            _ => self.libretro_thumbnail_dir,
        }
    }
}

/// Static table of known systems. Extension lists favor the unambiguous
/// cartridge extensions; disc-based systems that share generic extensions
/// (`.bin`, `.cue`, `.chd`, `.iso`, `.pbp`) are disambiguated by core name (see
/// [`system_for_core_name`]) rather than by extension, so their `extensions`
/// lists are deliberately narrow or empty.
pub const SYSTEMS: &[System] = &[
    System {
        id: "nes",
        name: "Nintendo (NES)",
        extensions: &["nes", "unif", "unf"],
        libretro_thumbnail_dir: "Nintendo - Nintendo Entertainment System",
        openvgdb_system_id: "NES",
    },
    System {
        id: "fds",
        name: "Famicom Disk System",
        extensions: &["fds"],
        libretro_thumbnail_dir: "Nintendo - Family Computer Disk System",
        openvgdb_system_id: "FDS",
    },
    System {
        id: "snes",
        name: "Super Nintendo (SNES)",
        extensions: &["sfc", "smc", "swc", "fig", "bs"],
        libretro_thumbnail_dir: "Nintendo - Super Nintendo Entertainment System",
        openvgdb_system_id: "SNES",
    },
    System {
        id: "n64",
        name: "Nintendo 64",
        extensions: &["n64", "z64", "v64", "ndd"],
        libretro_thumbnail_dir: "Nintendo - Nintendo 64",
        openvgdb_system_id: "N64",
    },
    System {
        id: "gb",
        name: "Game Boy",
        extensions: &["gb"],
        libretro_thumbnail_dir: "Nintendo - Game Boy",
        openvgdb_system_id: "GB",
    },
    System {
        id: "gbc",
        name: "Game Boy Color",
        extensions: &["gbc"],
        libretro_thumbnail_dir: "Nintendo - Game Boy Color",
        openvgdb_system_id: "GBC",
    },
    System {
        id: "gba",
        name: "Game Boy Advance",
        extensions: &["gba", "srl"],
        libretro_thumbnail_dir: "Nintendo - Game Boy Advance",
        openvgdb_system_id: "GBA",
    },
    System {
        id: "nds",
        name: "Nintendo DS",
        extensions: &["nds"],
        libretro_thumbnail_dir: "Nintendo - Nintendo DS",
        openvgdb_system_id: "NDS",
    },
    System {
        id: "genesis",
        name: "Sega Genesis",
        extensions: &["md", "gen", "smd"],
        libretro_thumbnail_dir: "Sega - Mega Drive - Genesis",
        openvgdb_system_id: "MD",
    },
    System {
        id: "sms",
        name: "Sega Master System",
        extensions: &["sms"],
        libretro_thumbnail_dir: "Sega - Master System - Mark III",
        openvgdb_system_id: "SMS",
    },
    System {
        id: "gg",
        name: "Game Gear",
        extensions: &["gg"],
        libretro_thumbnail_dir: "Sega - Game Gear",
        openvgdb_system_id: "GG",
    },
    System {
        id: "sg1000",
        name: "SG-1000",
        extensions: &["sg"],
        libretro_thumbnail_dir: "Sega - SG-1000",
        openvgdb_system_id: "SG1000",
    },
    System {
        id: "sega32x",
        name: "Sega 32X",
        extensions: &["32x"],
        libretro_thumbnail_dir: "Sega - 32X",
        openvgdb_system_id: "32X",
    },
    System {
        id: "saturn",
        name: "Sega Saturn",
        extensions: &[],
        libretro_thumbnail_dir: "Sega - Saturn",
        openvgdb_system_id: "Saturn",
    },
    System {
        id: "segacd",
        name: "Sega CD",
        extensions: &[],
        libretro_thumbnail_dir: "Sega - Mega-CD - Sega CD",
        openvgdb_system_id: "SCD",
    },
    System {
        id: "psx",
        name: "Sony PlayStation",
        extensions: &["pbp"],
        libretro_thumbnail_dir: "Sony - PlayStation",
        openvgdb_system_id: "PSX",
    },
    System {
        id: "pce",
        name: "TurboGrafx-16",
        extensions: &["pce"],
        libretro_thumbnail_dir: "NEC - PC Engine - TurboGrafx 16",
        openvgdb_system_id: "PCE",
    },
    System {
        id: "sgx",
        name: "SuperGrafx",
        extensions: &["sgx"],
        libretro_thumbnail_dir: "NEC - PC Engine SuperGrafx",
        openvgdb_system_id: "SuperGrafx",
    },
    System {
        id: "pcfx",
        name: "PC-FX",
        extensions: &[],
        libretro_thumbnail_dir: "NEC - PC-FX",
        openvgdb_system_id: "PCFX",
    },
    System {
        id: "ngp",
        name: "Neo Geo Pocket",
        extensions: &["ngp", "ngc"],
        libretro_thumbnail_dir: "SNK - Neo Geo Pocket",
        openvgdb_system_id: "NGP",
    },
    System {
        id: "ws",
        name: "WonderSwan",
        extensions: &["ws", "wsc"],
        libretro_thumbnail_dir: "Bandai - WonderSwan",
        openvgdb_system_id: "",
    },
    System {
        id: "lynx",
        name: "Atari Lynx",
        extensions: &["lnx"],
        libretro_thumbnail_dir: "Atari - Lynx",
        openvgdb_system_id: "Lynx",
    },
    System {
        id: "a2600",
        name: "Atari 2600",
        extensions: &["a26"],
        libretro_thumbnail_dir: "Atari - 2600",
        openvgdb_system_id: "2600",
    },
    System {
        id: "a7800",
        name: "Atari 7800",
        extensions: &["a78"],
        libretro_thumbnail_dir: "Atari - 7800",
        openvgdb_system_id: "7800",
    },
    System {
        id: "vb",
        name: "Virtual Boy",
        extensions: &["vb"],
        libretro_thumbnail_dir: "Nintendo - Virtual Boy",
        openvgdb_system_id: "VB",
    },
    System {
        id: "colecovision",
        name: "ColecoVision",
        extensions: &["col"],
        libretro_thumbnail_dir: "Coleco - ColecoVision",
        openvgdb_system_id: "ColecoVision",
    },
    System {
        id: "intellivision",
        name: "Intellivision",
        extensions: &["int"],
        libretro_thumbnail_dir: "Mattel - Intellivision",
        openvgdb_system_id: "Intellivision",
    },
    System {
        id: "vectrex",
        name: "Vectrex",
        extensions: &["vec"],
        libretro_thumbnail_dir: "GCE - Vectrex",
        openvgdb_system_id: "Vectrex",
    },
    System {
        id: "msx",
        name: "MSX",
        extensions: &["msx"],
        libretro_thumbnail_dir: "Microsoft - MSX",
        openvgdb_system_id: "MSX",
    },
];

/// Look up a system by its canonical id.
pub fn system_by_id(id: &str) -> Option<&'static System> {
    SYSTEMS.iter().find(|system| system.id == id)
}

/// The default libretro buildbot core used when a library system has no
/// installed compatible core. These slugs match the downloadable core
/// catalog and intentionally select one predictable core per system.
pub fn preferred_core_slug(system_id: &str) -> Option<&'static str> {
    Some(match system_id {
        "nes" | "fds" => "fceumm",
        "snes" => "bsnes",
        "n64" => "mupen64plus_next",
        "gb" | "gbc" | "gba" => "mgba",
        "nds" => "melonds",
        "genesis" => "clownmdemu",
        "sms" | "gg" | "sg1000" => "gearsystem",
        "sega32x" | "segacd" => "picodrive",
        "saturn" => "mednafen_saturn",
        "psx" => "pcsx_rearmed",
        "pce" | "sgx" => "mednafen_pce",
        "pcfx" => "mednafen_pcfx",
        "ngp" => "mednafen_ngp",
        "ws" => "mednafen_wswan",
        "lynx" => "handy",
        "a2600" => "stella",
        "a7800" => "prosystem",
        "vb" => "mednafen_vb",
        "colecovision" => "gearcoleco",
        "intellivision" => "freeintv",
        "vectrex" => "vecx",
        "msx" => "fmsx",
        _ => return None,
    })
}

/// Resolve a system from a ROM file extension (with or without a leading dot,
/// any case). Returns `None` for generic/disc extensions that need a core-name
/// hint to disambiguate.
pub fn system_for_extension(extension: &str) -> Option<&'static System> {
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    SYSTEMS
        .iter()
        .find(|system| system.extensions.iter().any(|ext| *ext == extension))
}

/// Disambiguate a system from a core's libretro `library_name`. Used when the
/// extension is generic (disc images) — the core that plays a ROM implies its
/// system. Matching is a case-insensitive substring against known core-name
/// fragments; the first (most-specific) match wins, so more-specific fragments
/// are ordered before their prefixes (e.g. `mesen-s` before `mesen`).
pub fn system_for_core_name(library_name: &str) -> Option<&'static System> {
    let name = library_name.to_ascii_lowercase();
    const HINTS: &[(&str, &str)] = &[
        // PlayStation
        ("pcsx", "psx"),
        ("beetle psx", "psx"),
        ("swanstation", "psx"),
        ("duckstation", "psx"),
        // Sega Saturn
        ("beetle saturn", "saturn"),
        ("mednafen_saturn", "saturn"),
        ("yabause", "saturn"),
        ("kronos", "saturn"),
        // Sega CD (Genesis-family cores also handle plain Genesis; those ROMs
        // resolve by extension before reaching here, so the disc case wins).
        ("segacd", "segacd"),
        // Sega Genesis / 32X
        ("genesis plus gx", "genesis"),
        ("genesis_plus_gx", "genesis"),
        ("picodrive", "genesis"),
        ("blastem", "genesis"),
        // PC Engine family (order specific → generic)
        ("beetle supergrafx", "sgx"),
        ("beetle pcfx", "pcfx"),
        ("mednafen_pcfx", "pcfx"),
        ("beetle pce", "pce"),
        ("mednafen_pce", "pce"),
        // Super Nintendo (before the NES `mesen` fragment)
        ("snes9x", "snes"),
        ("bsnes", "snes"),
        ("mesen-s", "snes"),
        ("mesen_s", "snes"),
        // Nintendo (NES)
        ("nestopia", "nes"),
        ("fceumm", "nes"),
        ("quicknes", "nes"),
        ("mesen", "nes"),
        // Nintendo 64
        ("mupen64", "n64"),
        ("parallel_n64", "n64"),
        // Game Boy / GBA / DS
        ("gambatte", "gb"),
        ("sameboy", "gb"),
        ("mgba", "gba"),
        ("vbam", "gba"),
        ("desmume", "nds"),
        ("melonds", "nds"),
        // Handhelds / others
        ("beetle lynx", "lynx"),
        ("handy", "lynx"),
        ("beetle wswan", "ws"),
        ("beetle ngp", "ngp"),
        ("beetle vb", "vb"),
        ("mednafen_vb", "vb"),
        ("stella", "a2600"),
        ("prosystem", "a7800"),
        ("bluemsx", "msx"),
        ("fmsx", "msx"),
        ("gearcoleco", "colecovision"),
        ("freeintv", "intellivision"),
        ("vecx", "vectrex"),
    ];
    HINTS
        .iter()
        .find(|(fragment, _)| name.contains(fragment))
        .and_then(|(_, id)| system_by_id(id))
}

/// Resolve a ROM's system: extension first (definitive for cartridge systems),
/// then the resolved core's name as a fallback (disambiguates disc systems).
pub fn system_for_rom(path: &Path, core_name: Option<&str>) -> Option<&'static System> {
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        if let Some(system) = system_for_extension(extension) {
            return Some(system);
        }
    }
    core_name.and_then(system_for_core_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_system_has_a_unique_id() {
        for (i, system) in SYSTEMS.iter().enumerate() {
            assert!(
                SYSTEMS[..i].iter().all(|other| other.id != system.id),
                "duplicate system id: {}",
                system.id
            );
        }
    }

    #[test]
    fn every_visible_system_has_a_preferred_core() {
        for system in SYSTEMS {
            assert!(
                preferred_core_slug(system.id).is_some(),
                "missing preferred core for {}",
                system.id
            );
        }
    }

    #[test]
    fn extension_maps_to_system_case_insensitively() {
        assert_eq!(system_for_extension("nes").unwrap().id, "nes");
        assert_eq!(system_for_extension(".SFC").unwrap().id, "snes");
        assert_eq!(system_for_extension("GBA").unwrap().id, "gba");
        assert_eq!(system_for_extension("md").unwrap().id, "genesis");
        assert!(system_for_extension("zip").is_none());
        assert!(system_for_extension("bin").is_none());
    }

    #[test]
    fn core_name_disambiguates_disc_and_shared_names() {
        assert_eq!(system_for_core_name("PCSX-ReARMed").unwrap().id, "psx");
        assert_eq!(system_for_core_name("Beetle Saturn").unwrap().id, "saturn");
        // "Mesen-S" must resolve to SNES even though it contains "mesen".
        assert_eq!(system_for_core_name("Mesen-S").unwrap().id, "snes");
        assert_eq!(system_for_core_name("Mesen").unwrap().id, "nes");
        assert!(system_for_core_name("totally-unknown-core").is_none());
    }

    #[test]
    fn rom_prefers_extension_then_core() {
        use std::path::Path;
        // Extension wins even if the core hint disagrees.
        assert_eq!(
            system_for_rom(Path::new("game.sfc"), Some("PCSX-ReARMed"))
                .unwrap()
                .id,
            "snes"
        );
        // Generic disc extension falls back to the core hint.
        assert_eq!(
            system_for_rom(Path::new("game.bin"), Some("PCSX-ReARMed"))
                .unwrap()
                .id,
            "psx"
        );
        // Unknown extension and no core hint → None.
        assert!(system_for_rom(Path::new("game.bin"), None).is_none());
    }

    #[test]
    fn color_handheld_roms_use_color_thumbnail_catalogs() {
        let ngp = system_by_id("ngp").unwrap();
        assert_eq!(
            ngp.thumbnail_dir_for_rom(Path::new("game.ngp")),
            "SNK - Neo Geo Pocket"
        );
        assert_eq!(
            ngp.thumbnail_dir_for_rom(Path::new("game.NGC")),
            "SNK - Neo Geo Pocket Color"
        );

        let ws = system_by_id("ws").unwrap();
        assert_eq!(
            ws.thumbnail_dir_for_rom(Path::new("game.ws")),
            "Bandai - WonderSwan"
        );
        assert_eq!(
            ws.thumbnail_dir_for_rom(Path::new("game.WSC")),
            "Bandai - WonderSwan Color"
        );
    }
}
