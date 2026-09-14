//! Box art for the library grid, fetched from the libretro-thumbnails project.
//!
//! Tiles spawn with the procedural placeholder poster (`spawn_poster_art`) and
//! a [`BoxArtSlot`] tag. Each frame, [`update_box_art`] swaps in real cover
//! art as it becomes available: art is fetched by a background thread from
//! `https://thumbnails.libretro.com/<system>/Named_Boxarts/<game>.png`, cached
//! under `<data_dir>/boxart/`, decoded to RGBA on the main thread, and applied
//! by inserting an `ImageNode` on the poster node (replacing the placeholder
//! children). A 404 writes a `.missing` marker next to the cache path so
//! absent art is not re-requested on every launch; transient network errors
//! are only remembered in-memory so the next launch retries.
//!
//! Matching starts with the ROM filename stem, then normalizes common region,
//! translation, version, and hack suffixes into No-Intro-style candidates.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

/// Marks a poster node that wants real box art. Removed once art is applied
/// or known to be missing.
#[derive(Component)]
pub struct BoxArtSlot {
    /// libretro-thumbnails directory name (`System::libretro_thumbnail_dir`).
    pub system_dir: &'static str,
    /// ROM display name (filename stem).
    pub game_name: String,
}

/// Steam library header image, fetched from Steam's public App-ID CDN path.
#[derive(Component)]
pub struct SteamArtSlot {
    pub app_id: u32,
}

struct FetchRequest {
    urls: Vec<String>,
    cache_path: PathBuf,
    marker_path: PathBuf,
}

enum FetchResult {
    /// Art is on disk at `cache_path`, ready to decode.
    Cached { cache_path: PathBuf },
    /// No art (404 or fetch failure); keep the placeholder.
    Missing { cache_path: PathBuf },
}

#[derive(Resource)]
pub struct BoxArt {
    cache_dir: PathBuf,
    loaded: HashMap<PathBuf, (Handle<Image>, f32)>,
    missing: HashSet<PathBuf>,
    requested: HashSet<PathBuf>,
    sender: Sender<FetchRequest>,
    results: Receiver<FetchResult>,
}

impl BoxArt {
    /// Create the resource and spawn the fetch thread. `cache_dir` is created
    /// lazily by the fetcher; failures degrade to placeholder posters.
    pub fn new(cache_dir: PathBuf) -> Self {
        let (request_sender, request_receiver) = unbounded::<FetchRequest>();
        let (result_sender, result_receiver) = unbounded::<FetchResult>();
        if let Err(error) = thread::Builder::new()
            .name("retrofeel-boxart".into())
            .spawn(move || fetch_thread(request_receiver, result_sender))
        {
            log::warn!("box art: failed to spawn fetch thread: {error}");
        }
        Self {
            cache_dir,
            loaded: HashMap::new(),
            missing: HashSet::new(),
            requested: HashSet::new(),
            sender: request_sender,
            results: result_receiver,
        }
    }

    fn cache_path(&self, slot: &BoxArtSlot) -> PathBuf {
        self.cache_dir
            .join(slot.system_dir)
            .join(format!("{}.png", thumbnail_name(&slot.game_name)))
    }
}

/// Drain finished fetches into image assets, then resolve every tagged poster:
/// swap in loaded art, drop the tag for known-missing art, and request art
/// that hasn't been asked for yet.
pub fn update_box_art(
    mut boxart: ResMut<BoxArt>,
    mut images: ResMut<Assets<Image>>,
    mut commands: Commands,
    slots: Query<(Entity, &BoxArtSlot)>,
    steam_slots: Query<(Entity, &SteamArtSlot)>,
    db: Option<Res<crate::db::DbResource>>,
) {
    while let Ok(result) = boxart.results.try_recv() {
        match result {
            FetchResult::Cached { cache_path } => match decode_cached_art(&cache_path) {
                Some((image, aspect)) => {
                    let handle = images.add(image);
                    if let (Some(db), Some(app_id)) =
                        (db.as_deref(), steam_app_id_from_cache_path(&cache_path))
                    {
                        if let Err(error) = retrofeel_db::SteamRepo::new(&db.db)
                            .set_banner_path(app_id, &cache_path)
                        {
                            log::warn!("steam art: could not persist banner path: {error}");
                        }
                    }
                    boxart.loaded.insert(cache_path, (handle, aspect));
                }
                None => {
                    boxart.missing.insert(cache_path);
                }
            },
            FetchResult::Missing { cache_path } => {
                boxart.missing.insert(cache_path);
            }
        }
    }

    for (entity, slot) in &slots {
        let cache_path = boxart.cache_path(slot);
        if let Some((handle, aspect)) = boxart.loaded.get(&cache_path) {
            commands
                .entity(entity)
                .insert(ImageNode::new(handle.clone()))
                .insert(Node {
                    aspect_ratio: Some(*aspect),
                    ..default()
                })
                .remove::<BoxArtSlot>()
                .despawn_related::<Children>();
        } else if boxart.missing.contains(&cache_path) {
            commands.entity(entity).remove::<BoxArtSlot>();
        } else if !boxart.requested.contains(&cache_path) {
            boxart.requested.insert(cache_path.clone());
            let urls = artwork_title_candidates(&slot.game_name)
                .into_iter()
                .map(|candidate| {
                    format!(
                        "https://thumbnails.libretro.com/{}/Named_Boxarts/{}.png",
                        url_encode(slot.system_dir),
                        url_encode(&thumbnail_name(&candidate)),
                    )
                })
                .collect();
            let marker_path = cache_path.with_extension("missing");
            let _ = boxart.sender.send(FetchRequest {
                urls,
                cache_path,
                marker_path,
            });
        }
    }

    for (entity, slot) in &steam_slots {
        let cache_path = boxart
            .cache_dir
            .join("steam")
            .join(format!("{}.jpg", slot.app_id));
        if let Some((handle, aspect)) = boxart.loaded.get(&cache_path) {
            commands
                .entity(entity)
                .insert(ImageNode::new(handle.clone()))
                .insert(Node {
                    aspect_ratio: Some(*aspect),
                    ..default()
                })
                .remove::<SteamArtSlot>()
                .despawn_related::<Children>();
        } else if boxart.missing.contains(&cache_path) {
            commands.entity(entity).remove::<SteamArtSlot>();
        } else if !boxart.requested.contains(&cache_path) {
            boxart.requested.insert(cache_path.clone());
            let marker_path = cache_path.with_extension("missing");
            let _ = boxart.sender.send(FetchRequest {
                urls: vec![format!(
                    "https://cdn.akamai.steamstatic.com/steam/apps/{}/header.jpg",
                    slot.app_id
                )],
                cache_path,
                marker_path,
            });
        }
    }
}

fn steam_app_id_from_cache_path(path: &Path) -> Option<u32> {
    (path.parent()?.file_name()? == "steam").then(|| path.file_stem()?.to_str()?.parse().ok())?
}

fn fetch_thread(requests: Receiver<FetchRequest>, results: Sender<FetchResult>) {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    while let Ok(request) = requests.recv() {
        let result = fetch_one(&agent, &request);
        if results.send(result).is_err() {
            return;
        }
    }
}

fn fetch_one(agent: &ureq::Agent, request: &FetchRequest) -> FetchResult {
    if request.cache_path.exists() {
        return FetchResult::Cached {
            cache_path: request.cache_path.clone(),
        };
    }
    if request.marker_path.exists() {
        if std::fs::read(&request.marker_path).ok().as_deref() == Some(b"v2") {
            return FetchResult::Missing {
                cache_path: request.cache_path.clone(),
            };
        }
        // Empty v1 markers represented a miss for only the exact ROM name.
        // Discard them once so the expanded candidate matcher can retry.
        let _ = std::fs::remove_file(&request.marker_path);
    }
    if let Some(parent) = request.cache_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            log::warn!("box art: failed to create {}: {error}", parent.display());
            return FetchResult::Missing {
                cache_path: request.cache_path.clone(),
            };
        }
    }
    for url in &request.urls {
        match agent.get(url).call() {
            Ok(response) => {
                let bytes = match read_art_bytes(response.into_reader()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        log::warn!("box art: read failed for {url}: {error}");
                        return FetchResult::Missing {
                            cache_path: request.cache_path.clone(),
                        };
                    }
                };
                if let Err(error) = persist_cache_file(&request.cache_path, &bytes) {
                    log::warn!(
                        "box art: write failed for {}: {error}",
                        request.cache_path.display()
                    );
                    return FetchResult::Missing {
                        cache_path: request.cache_path.clone(),
                    };
                }
                return FetchResult::Cached {
                    cache_path: request.cache_path.clone(),
                };
            }
            Err(ureq::Error::Status(404, _)) => continue,
            Err(error) => {
                // Transient (network/offline): don't write a marker so the
                // next launch retries.
                log::debug!("box art: fetch failed for {url}: {error}");
                return FetchResult::Missing {
                    cache_path: request.cache_path.clone(),
                };
            }
        }
    }
    let _ = std::fs::write(&request.marker_path, b"v2");
    FetchResult::Missing {
        cache_path: request.cache_path.clone(),
    }
}

const MAX_ART_BYTES: usize = 8 * 1024 * 1024;

fn read_art_bytes(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_ART_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ART_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "box art exceeds the 8 MiB cache limit",
        ));
    }
    Ok(bytes)
}

fn persist_cache_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("cache path has no parent: {}", path.display()),
        )
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.persist(path).map(|_| ()).map_err(|error| error.error)
}

/// Tile cover art renders at ~150 logical px tall; decode to at most 2.5x
/// that so a large library doesn't pin hundreds of MB of full-size RGBA
/// textures (thumbnail PNGs are commonly 512+ px tall).
const MAX_ART_HEIGHT: u32 = 384;

fn decode_cached_art(path: &Path) -> Option<(Image, f32)> {
    decode_art(path, true)
}

fn decode_art(path: &Path, evict_invalid: bool) -> Option<(Image, f32)> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!("box art: read failed for {}: {error}", path.display());
            if evict_invalid {
                evict_invalid_cache_entry(path);
            }
            return None;
        }
    };
    let mut decoded = match image::load_from_memory(&bytes) {
        Ok(decoded) => decoded.to_rgba8(),
        Err(error) => {
            log::warn!("box art: decode failed for {}: {error}", path.display());
            if evict_invalid {
                evict_invalid_cache_entry(path);
            }
            return None;
        }
    };
    let aspect = decoded.width() as f32 / decoded.height() as f32;
    if decoded.height() > MAX_ART_HEIGHT {
        let width =
            (decoded.width() as f32 * MAX_ART_HEIGHT as f32 / decoded.height() as f32) as u32;
        decoded = image::imageops::resize(
            &decoded,
            width.max(1),
            MAX_ART_HEIGHT,
            image::imageops::FilterType::Triangle,
        );
    }
    Some((
        Image::new(
            Extent3d {
                width: decoded.width(),
                height: decoded.height(),
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            decoded.into_raw(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        ),
        aspect,
    ))
}

fn evict_invalid_cache_entry(path: &Path) {
    for stale_path in [path.to_path_buf(), path.with_extension("missing")] {
        if let Err(error) = std::fs::remove_file(&stale_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "box art: failed to evict invalid cache entry {}: {error}",
                    stale_path.display()
                );
            }
        }
    }
}

fn artwork_title_candidates(game_name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique(&mut candidates, game_name.trim().to_string());

    let normalized = game_name
        .replace("(U)", "(USA)")
        .replace("(E)", "(Europe)")
        .replace("(J)", "(Japan)");
    push_unique(&mut candidates, normalized.clone());

    let stripped = strip_release_qualifiers(&normalized);
    push_unique(&mut candidates, stripped.clone());

    let has_region = ["(USA)", "(Europe)", "(Japan)", "(World)"]
        .iter()
        .any(|region| stripped.contains(region));
    if !has_region {
        for region in ["USA", "Europe", "Japan", "World"] {
            push_unique(&mut candidates, format!("{stripped} ({region})"));
        }
        // A small number of GBA catalogs include their language set after the
        // region. This still derives from the normalized title; it is not a
        // game-specific alias.
        push_unique(
            &mut candidates,
            format!("{stripped} (Japan) (En,Ja,Fr,De,Es,It)"),
        );
    }
    candidates
}

fn strip_release_qualifiers(value: &str) -> String {
    let mut without_brackets = String::with_capacity(value.len());
    let mut bracket_depth = 0_u32;
    for ch in value.chars() {
        match ch {
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ if bracket_depth == 0 => without_brackets.push(ch),
            _ => {}
        }
    }

    let mut title = without_brackets.trim().to_string();
    while let Some(start) = title.rfind('(') {
        let Some(end_offset) = title[start..].find(')') else {
            break;
        };
        let end = start + end_offset;
        if !title[end + 1..].trim().is_empty() {
            break;
        }
        let qualifier = title[start + 1..end].trim().to_ascii_lowercase();
        let removable = qualifier.starts_with("english")
            || qualifier.starts_with("retranslation")
            || qualifier.starts_with("rev ")
            || qualifier
                .strip_prefix('v')
                .and_then(|rest| rest.chars().next())
                .is_some_and(|first| first.is_ascii_digit());
        if !removable {
            break;
        }
        title.truncate(start);
        title = title.trim().to_string();
    }
    if let Some(base) = title.strip_suffix(" Retranslation") {
        title = base.trim().to_string();
    }
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

/// Apply the libretro-thumbnails filename substitutions: the characters
/// ``&*/:`<>?\|"`` become `_` in thumbnail filenames.
fn thumbnail_name(game_name: &str) -> String {
    game_name
        .chars()
        .map(|c| match c {
            '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"' => '_',
            other => other,
        })
        .collect()
}

/// Minimal percent-encoding for URL path segments (keeps unreserved chars and
/// parentheses, which No-Intro names use heavily).
fn url_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'(' | b')' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_name_applies_libretro_substitutions() {
        assert_eq!(
            thumbnail_name("Aster's Cloud Walk 2 (USA, Europe)"),
            "Aster's Cloud Walk 2 (USA, Europe)"
        );
        assert_eq!(
            thumbnail_name("Q*bot & Friends: Redux"),
            "Q_bot _ Friends_ Redux"
        );
    }

    #[test]
    fn url_encode_escapes_spaces_and_commas() {
        assert_eq!(
            url_encode("Lantern Meadow (USA)"),
            "Lantern%20Meadow%20(USA)"
        );
        assert_eq!(url_encode("a,b'c"), "a%2Cb%27c");
    }

    #[test]
    fn steam_cache_keys_are_stable_and_cached_art_wins_offline() {
        let temp = tempfile::tempdir().unwrap();
        let cache_path = temp.path().join("steam/42.jpg");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, b"cached").unwrap();
        let request = FetchRequest {
            urls: vec!["http://127.0.0.1:1/unreachable".into()],
            marker_path: cache_path.with_extension("missing"),
            cache_path: cache_path.clone(),
        };
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_millis(10))
            .build();
        assert!(matches!(
            fetch_one(&agent, &request),
            FetchResult::Cached { cache_path: path } if path == cache_path
        ));
        assert_eq!(steam_app_id_from_cache_path(&cache_path), Some(42));
        assert_eq!(
            steam_app_id_from_cache_path(&temp.path().join("steam/43.jpg")),
            Some(43)
        );
    }

    #[test]
    fn artwork_candidates_normalize_common_rom_labels() {
        assert!(
            artwork_title_candidates("Chronicles of the Three Valleys II (U)")
                .contains(&"Chronicles of the Three Valleys II (USA)".to_string())
        );
        assert!(
            artwork_title_candidates("CloudTown (USA) [FastROM hack by ExampleAuthor v1.0]")
                .contains(&"CloudTown (USA)".to_string())
        );
        assert!(
            artwork_title_candidates("3x3 Skies - Cloud Journey (English v1.01)")
                .contains(&"3x3 Skies - Cloud Journey (Japan)".to_string())
        );
        assert!(
            artwork_title_candidates("Marble Marble Fever Retranslation (v1.0)")
                .contains(&"Marble Marble Fever (Japan) (En,Ja,Fr,De,Es,It)".to_string())
        );
    }

    #[test]
    fn invalid_cached_art_and_stale_marker_are_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("broken.png");
        let marker_path = cache_path.with_extension("missing");
        std::fs::write(&cache_path, b"not a png").unwrap();
        std::fs::write(&marker_path, b"").unwrap();

        assert!(decode_cached_art(&cache_path).is_none());
        assert!(!cache_path.exists());
        assert!(!marker_path.exists());
    }

    #[test]
    fn oversized_art_is_rejected_instead_of_truncated() {
        let bytes = vec![0u8; MAX_ART_BYTES + 1];
        let error = read_art_bytes(std::io::Cursor::new(bytes)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Network test against the live thumbnail server — run manually:
    /// `cargo test -p retrofeel boxart -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fetches_and_decodes_real_cover_art() {
        let title = std::env::var("RETROFEEL_BOXART_TEST_TITLE")
            .expect("set RETROFEEL_BOXART_TEST_TITLE to an upstream catalog title");
        let system = std::env::var("RETROFEEL_BOXART_TEST_SYSTEM")
            .expect("set RETROFEEL_BOXART_TEST_SYSTEM to the upstream system directory");
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cover.png");
        let request = FetchRequest {
            urls: vec![format!(
                "https://thumbnails.libretro.com/{}/Named_Boxarts/{}.png",
                url_encode(&system),
                url_encode(&title),
            )],
            cache_path: cache_path.clone(),
            marker_path: cache_path.with_extension("missing"),
        };
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(15))
            .build();
        match fetch_one(&agent, &request) {
            FetchResult::Cached { cache_path } => {
                let (image, _aspect) =
                    decode_cached_art(&cache_path).expect("cached art should decode");
                assert!(image.height() <= MAX_ART_HEIGHT);
                assert!(image.width() > 0);
            }
            FetchResult::Missing { .. } => panic!("expected cover art to exist upstream"),
        }
    }
}
