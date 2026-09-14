//! Embedded branding and procedurally-generated UI icons.
//!
//! The default Bevy font has no media-control glyphs, so instead of shipping an
//! icon font we rasterize a small icon set into RGBA textures at startup and
//! display them via `ImageNode`. Each icon is drawn in solid white at a
//! supersampled resolution, then downscaled with a triangle filter for cheap
//! anti-aliasing; callers tint the result through `ImageNode.color`.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use image::{imageops, Rgba, RgbaImage};
use retrofeel_types::SYSTEMS;

/// Final icon size in logical pixels (also the displayed `Node` size).
pub const ICON_PX: f32 = 22.0;
/// Supersampled render resolution before downscaling (5× for smooth edges).
const R: u32 = 110;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Brand,
    Record,
    Stop,
    Play,
    Pause,
    Reset,
    Save,
    Load,
    VolumeDown,
    VolumeUp,
    Fullscreen,
    Menu,
    Quit,
    Library,
    Settings,
    Bios,
    Grid,
    List,
    Search,
    Plus,
    Star,
    Controller,
    Chip,
    Console,
    Clock,
    Refresh,
    Close,
}

const ALL: [Icon; 27] = [
    Icon::Brand,
    Icon::Record,
    Icon::Stop,
    Icon::Play,
    Icon::Pause,
    Icon::Reset,
    Icon::Save,
    Icon::Load,
    Icon::VolumeDown,
    Icon::VolumeUp,
    Icon::Fullscreen,
    Icon::Menu,
    Icon::Quit,
    Icon::Library,
    Icon::Settings,
    Icon::Bios,
    Icon::Grid,
    Icon::List,
    Icon::Search,
    Icon::Plus,
    Icon::Star,
    Icon::Controller,
    Icon::Chip,
    Icon::Console,
    Icon::Clock,
    Icon::Refresh,
    Icon::Close,
];

/// Startup-built map of icon → texture handle. Insert as a resource.
#[derive(Resource)]
pub struct IconAssets {
    map: HashMap<Icon, Handle<Image>>,
    _systems: HashMap<&'static str, Handle<Image>>,
}

impl IconAssets {
    pub fn get(&self, icon: Icon) -> Handle<Image> {
        self.map.get(&icon).cloned().unwrap_or_default()
    }
}

/// Load the supplied brand artwork, rasterize every utility icon, and register
/// them as Bevy image assets.
pub fn build_icons(images: &mut Assets<Image>) -> IconAssets {
    let mut map = HashMap::new();
    for icon in ALL {
        let raster = icon_raster(icon);
        let image = Image::new(
            Extent3d {
                width: raster.width(),
                height: raster.height(),
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            raster.into_raw(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        map.insert(icon, images.add(image));
    }
    let mut systems = HashMap::new();
    for system in SYSTEMS {
        let raster = downscale(render_system_badge(system.id));
        let image = Image::new(
            Extent3d {
                width: raster.width(),
                height: raster.height(),
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            raster.into_raw(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        systems.insert(system.id, images.add(image));
    }
    IconAssets {
        map,
        _systems: systems,
    }
}

fn icon_raster(icon: Icon) -> RgbaImage {
    if icon == Icon::Brand {
        return image::load_from_memory(include_bytes!("../assets/tigerwolf.png"))
            .expect("embedded tigerwolf.png must be valid")
            .into_rgba8();
    }
    downscale(render(icon))
}

/// Original compact hardware silhouettes for the console rail. OpenEmu uses
/// a depiction of each system's physical hardware in this location; we follow
/// that semantic mapping without bundling or tracing its artwork.
fn render_system_badge(system_id: &str) -> RgbaImage {
    let mut img = RgbaImage::new(R, R);
    match system_id {
        "nes" => console_box(&mut img, 0),
        "fds" => {
            console_box(&mut img, 1);
            rect(&mut img, 0.33, 0.18, 0.67, 0.34);
            erase_rect(&mut img, 0.39, 0.22, 0.61, 0.26);
        }
        "snes" => console_box(&mut img, 2),
        "n64" => {
            tri(&mut img, (0.10, 0.70), (0.24, 0.35), (0.90, 0.70));
            rect(&mut img, 0.24, 0.35, 0.78, 0.70);
            erase_rect(&mut img, 0.39, 0.38, 0.63, 0.45);
            erase_disc(&mut img, 0.25, 0.60, 0.045);
            erase_disc(&mut img, 0.77, 0.60, 0.045);
        }
        "gb" => handheld_portrait(&mut img, 0),
        "gbc" => handheld_portrait(&mut img, 1),
        "gba" => handheld_landscape(&mut img, 0),
        "nds" => clamshell(&mut img),
        "genesis" => {
            disc(&mut img, 0.50, 0.53, 0.38);
            erase_disc(&mut img, 0.50, 0.53, 0.22);
            rect(&mut img, 0.16, 0.50, 0.84, 0.74);
            erase_rect(&mut img, 0.39, 0.24, 0.61, 0.39);
        }
        "sms" => {
            console_box(&mut img, 3);
            rect_rotated(&mut img, 0.22, 0.41, 0.72, 0.48, -0.18);
        }
        "gg" => handheld_landscape(&mut img, 1),
        "sg1000" => {
            console_box(&mut img, 4);
            rect(&mut img, 0.22, 0.31, 0.78, 0.36);
        }
        "sega32x" => {
            rect(&mut img, 0.22, 0.38, 0.78, 0.75);
            disc(&mut img, 0.50, 0.40, 0.25);
            erase_rect(&mut img, 0.41, 0.18, 0.59, 0.42);
            erase_rect(&mut img, 0.35, 0.56, 0.65, 0.63);
        }
        "saturn" => disc_console(&mut img, 0),
        "segacd" => disc_console(&mut img, 1),
        "psx" => disc_console(&mut img, 2),
        "pce" => {
            console_box(&mut img, 5);
            erase_rect(&mut img, 0.44, 0.25, 0.56, 0.50);
        }
        "sgx" => {
            console_box(&mut img, 6);
            erase_rect(&mut img, 0.32, 0.46, 0.68, 0.53);
        }
        "pcfx" => tower(&mut img, 0),
        "ngp" => handheld_portrait(&mut img, 2),
        "ws" => handheld_landscape(&mut img, 2),
        "lynx" => handheld_landscape(&mut img, 3),
        "a2600" => {
            tri(&mut img, (0.10, 0.70), (0.20, 0.36), (0.90, 0.70));
            rect(&mut img, 0.20, 0.36, 0.88, 0.70);
            for x in [0.30, 0.42, 0.54, 0.66] {
                erase_rect(&mut img, x, 0.40, x + 0.04, 0.54);
            }
        }
        "a7800" => {
            console_box(&mut img, 7);
            rect(&mut img, 0.16, 0.61, 0.84, 0.70);
        }
        "vb" => virtual_boy(&mut img),
        "colecovision" => {
            console_box(&mut img, 8);
            erase_rect(&mut img, 0.20, 0.40, 0.34, 0.62);
            erase_rect(&mut img, 0.66, 0.40, 0.80, 0.62);
        }
        "intellivision" => {
            console_box(&mut img, 9);
            erase_rect(&mut img, 0.24, 0.40, 0.38, 0.61);
            erase_rect(&mut img, 0.62, 0.40, 0.76, 0.61);
        }
        "vectrex" => upright_screen(&mut img),
        "msx" => computer_keyboard(&mut img),
        _ => console_box(&mut img, 0),
    }
    img
}

fn console_box(img: &mut RgbaImage, variant: u8) {
    rect(img, 0.10, 0.34, 0.90, 0.72);
    let slot_y = 0.40 + f32::from(variant % 3) * 0.055;
    erase_rect(img, 0.28, slot_y, 0.72, slot_y + 0.055);
    if variant & 1 == 0 {
        erase_disc(img, 0.78, 0.62, 0.045);
        erase_disc(img, 0.66, 0.62, 0.030);
    } else {
        erase_rect(img, 0.69, 0.59, 0.82, 0.64);
    }
}

fn handheld_portrait(img: &mut RgbaImage, variant: u8) {
    rect(img, 0.27, 0.07, 0.73, 0.93);
    erase_rect(img, 0.34, 0.16, 0.66, 0.48);
    dpad_cutout(img, 0.39, 0.67, 0.06);
    erase_disc(img, 0.60, 0.65, 0.045);
    erase_disc(img, 0.66, 0.59 + f32::from(variant) * 0.025, 0.040);
    if variant == 1 {
        erase_rect(img, 0.43, 0.83, 0.58, 0.86);
    } else if variant == 2 {
        erase_disc(img, 0.50, 0.87, 0.025);
    }
}

fn handheld_landscape(img: &mut RgbaImage, variant: u8) {
    rect(img, 0.06, 0.27, 0.94, 0.73);
    erase_rect(img, 0.31, 0.34, 0.69, 0.66);
    dpad_cutout(img, 0.19, 0.50, 0.055);
    erase_disc(img, 0.80, 0.48, 0.040);
    erase_disc(img, 0.87, 0.55, 0.040);
    for index in 0..variant.min(3) {
        let y = 0.35 + f32::from(index) * 0.06;
        erase_rect(img, 0.09, y, 0.13, y + 0.025);
    }
}

fn clamshell(img: &mut RgbaImage) {
    rect(img, 0.17, 0.07, 0.83, 0.47);
    rect(img, 0.17, 0.53, 0.83, 0.93);
    erase_rect(img, 0.26, 0.13, 0.74, 0.40);
    erase_rect(img, 0.28, 0.59, 0.72, 0.81);
    dpad_cutout(img, 0.25, 0.86, 0.035);
    erase_disc(img, 0.75, 0.86, 0.030);
}

fn disc_console(img: &mut RgbaImage, variant: u8) {
    rect(img, 0.14, 0.25, 0.86, 0.76);
    let (cx, cy, radius) = match variant {
        0 => (0.50, 0.47, 0.20),
        1 => (0.43, 0.49, 0.18),
        _ => (0.50, 0.50, 0.22),
    };
    erase_disc(img, cx, cy, radius);
    band(img, cx, cy, radius - 0.035, radius);
    erase_rect(img, 0.65, 0.67, 0.79, 0.71);
    if variant == 1 {
        erase_rect(img, 0.68, 0.35, 0.77, 0.55);
    }
}

fn tower(img: &mut RgbaImage, variant: u8) {
    rect(img, 0.28, 0.08, 0.72, 0.92);
    erase_rect(img, 0.35, 0.18, 0.65, 0.25);
    erase_rect(img, 0.35, 0.34, 0.65, 0.66);
    if variant == 0 {
        erase_disc(img, 0.50, 0.79, 0.045);
    }
}

fn computer_keyboard(img: &mut RgbaImage) {
    rect(img, 0.13, 0.31, 0.87, 0.76);
    tri(img, (0.13, 0.76), (0.22, 0.55), (0.87, 0.76));
    for row in 0..3 {
        for column in 0..7 {
            let x = 0.23 + column as f32 * 0.075;
            let y = 0.45 + row as f32 * 0.075;
            erase_rect(img, x, y, x + 0.040, y + 0.035);
        }
    }
}

fn upright_screen(img: &mut RgbaImage) {
    tri(img, (0.18, 0.91), (0.27, 0.07), (0.73, 0.07));
    tri(img, (0.18, 0.91), (0.73, 0.07), (0.82, 0.91));
    erase_rect(img, 0.33, 0.18, 0.67, 0.55);
    erase_rect(img, 0.42, 0.68, 0.58, 0.73);
}

fn virtual_boy(img: &mut RgbaImage) {
    disc(img, 0.34, 0.39, 0.22);
    disc(img, 0.66, 0.39, 0.22);
    erase_disc(img, 0.35, 0.39, 0.10);
    erase_disc(img, 0.65, 0.39, 0.10);
    rect(img, 0.32, 0.35, 0.68, 0.55);
    rect(img, 0.47, 0.55, 0.53, 0.83);
    tri(img, (0.50, 0.72), (0.29, 0.92), (0.71, 0.92));
    erase_rect(img, 0.36, 0.84, 0.64, 0.94);
}

fn dpad_cutout(img: &mut RgbaImage, cx: f32, cy: f32, arm: f32) {
    erase_rect(img, cx - arm * 0.35, cy - arm, cx + arm * 0.35, cy + arm);
    erase_rect(img, cx - arm, cy - arm * 0.35, cx + arm, cy + arm * 0.35);
}

fn downscale(img: RgbaImage) -> RgbaImage {
    imageops::resize(
        &img,
        ICON_PX as u32,
        ICON_PX as u32,
        imageops::FilterType::Triangle,
    )
}

fn render(icon: Icon) -> RgbaImage {
    use std::f32::consts::TAU;
    let mut img = RgbaImage::new(R, R);
    match icon {
        Icon::Brand => unreachable!("brand artwork is loaded from the supplied PNG"),
        Icon::Record => disc(&mut img, 0.5, 0.5, 0.30),
        Icon::Stop => rect(&mut img, 0.26, 0.26, 0.74, 0.74),
        Icon::Play => tri(&mut img, (0.34, 0.24), (0.34, 0.76), (0.80, 0.5)),
        Icon::Pause => {
            rect(&mut img, 0.30, 0.24, 0.44, 0.76);
            rect(&mut img, 0.56, 0.24, 0.70, 0.76);
        }
        Icon::Reset => {
            // ~290° ring with a gap near the top, plus an arrowhead at the gap.
            arc(&mut img, 0.5, 0.5, 0.30, 0.10, -0.9, 4.2);
            tri(&mut img, (0.52, 0.08), (0.52, 0.34), (0.78, 0.21));
        }
        Icon::Save => {
            // down arrow into a tray
            rect(&mut img, 0.45, 0.18, 0.55, 0.48);
            tri(&mut img, (0.34, 0.44), (0.66, 0.44), (0.50, 0.66));
            rect(&mut img, 0.26, 0.74, 0.74, 0.82);
        }
        Icon::Load => {
            // up arrow out of a tray
            rect(&mut img, 0.45, 0.34, 0.55, 0.64);
            tri(&mut img, (0.34, 0.40), (0.66, 0.40), (0.50, 0.18));
            rect(&mut img, 0.26, 0.74, 0.74, 0.82);
        }
        Icon::VolumeDown => {
            speaker(&mut img);
            rect(&mut img, 0.58, 0.47, 0.84, 0.53);
        }
        Icon::VolumeUp => {
            speaker(&mut img);
            rect(&mut img, 0.58, 0.47, 0.84, 0.53);
            rect(&mut img, 0.68, 0.37, 0.74, 0.63);
        }
        Icon::Fullscreen => corners(&mut img),
        Icon::Menu => {
            rect(&mut img, 0.20, 0.26, 0.80, 0.33);
            rect(&mut img, 0.20, 0.465, 0.80, 0.535);
            rect(&mut img, 0.20, 0.67, 0.80, 0.74);
        }
        Icon::Quit => {
            // power symbol: ~300° ring with a gap at the top + a bar through it
            arc(&mut img, 0.5, 0.5, 0.30, 0.10, -1.05, 4.19);
            rect(&mut img, 0.46, 0.14, 0.54, 0.52);
        }
        Icon::Library => {
            rect(&mut img, 0.20, 0.20, 0.46, 0.46);
            rect(&mut img, 0.54, 0.20, 0.80, 0.46);
            rect(&mut img, 0.20, 0.54, 0.46, 0.80);
            rect(&mut img, 0.54, 0.54, 0.80, 0.80);
        }
        Icon::Settings => {
            // gear: eight teeth + body with a center hole
            for k in 0..8 {
                let a = k as f32 / 8.0 * TAU;
                disc(&mut img, 0.5 + 0.34 * a.cos(), 0.5 + 0.34 * a.sin(), 0.09);
            }
            disc(&mut img, 0.5, 0.5, 0.27);
            erase_disc(&mut img, 0.5, 0.5, 0.12);
        }
        Icon::Bios => {
            // chip: square body with pins on all four sides
            rect(&mut img, 0.34, 0.34, 0.66, 0.66);
            for c in [0.40_f32, 0.50, 0.60] {
                rect(&mut img, c - 0.025, 0.24, c + 0.025, 0.34);
                rect(&mut img, c - 0.025, 0.66, c + 0.025, 0.76);
                rect(&mut img, 0.24, c - 0.025, 0.34, c + 0.025);
                rect(&mut img, 0.66, c - 0.025, 0.76, c + 0.025);
            }
            erase_disc(&mut img, 0.5, 0.5, 0.07);
        }
        Icon::Grid => {
            for y in [0.20_f32, 0.48] {
                for x in [0.20_f32, 0.48] {
                    rect(&mut img, x, y, x + 0.23, y + 0.23);
                }
            }
        }
        Icon::List => {
            for y in [0.24_f32, 0.46, 0.68] {
                disc(&mut img, 0.24, y + 0.02, 0.035);
                rect(&mut img, 0.34, y, 0.80, y + 0.05);
            }
        }
        Icon::Search => {
            arc(&mut img, 0.43, 0.42, 0.19, 0.07, 0.0, TAU);
            rect_rotated(&mut img, 0.58, 0.58, 0.82, 0.68, 0.78);
        }
        Icon::Plus => {
            rect(&mut img, 0.46, 0.22, 0.54, 0.78);
            rect(&mut img, 0.22, 0.46, 0.78, 0.54);
        }
        Icon::Star => {
            star(&mut img, 0.5, 0.5, 0.34, 0.15);
        }
        Icon::Controller => {
            rect(&mut img, 0.24, 0.38, 0.76, 0.68);
            disc(&mut img, 0.28, 0.54, 0.17);
            disc(&mut img, 0.72, 0.54, 0.17);
            rect(&mut img, 0.24, 0.50, 0.42, 0.57);
            rect(&mut img, 0.295, 0.43, 0.365, 0.64);
            disc(&mut img, 0.64, 0.49, 0.045);
            disc(&mut img, 0.73, 0.58, 0.045);
            erase_disc(&mut img, 0.50, 0.56, 0.04);
        }
        Icon::Chip => {
            rect(&mut img, 0.30, 0.30, 0.70, 0.70);
            rect(&mut img, 0.39, 0.39, 0.61, 0.61);
            for c in [0.24_f32, 0.76] {
                rect(&mut img, c - 0.02, 0.36, c + 0.02, 0.43);
                rect(&mut img, c - 0.02, 0.48, c + 0.02, 0.55);
                rect(&mut img, c - 0.02, 0.60, c + 0.02, 0.67);
                rect(&mut img, 0.36, c - 0.02, 0.43, c + 0.02);
                rect(&mut img, 0.48, c - 0.02, 0.55, c + 0.02);
                rect(&mut img, 0.60, c - 0.02, 0.67, c + 0.02);
            }
        }
        Icon::Console => {
            rect(&mut img, 0.20, 0.33, 0.80, 0.66);
            rect(&mut img, 0.28, 0.42, 0.58, 0.48);
            disc(&mut img, 0.68, 0.49, 0.045);
            disc(&mut img, 0.74, 0.49, 0.045);
            rect(&mut img, 0.30, 0.68, 0.42, 0.74);
            rect(&mut img, 0.58, 0.68, 0.70, 0.74);
        }
        Icon::Clock => {
            arc(&mut img, 0.5, 0.5, 0.30, 0.07, 0.0, TAU);
            rect(&mut img, 0.48, 0.28, 0.54, 0.52);
            rect_rotated(&mut img, 0.50, 0.50, 0.70, 0.58, 0.34);
        }
        Icon::Refresh => {
            arc(&mut img, 0.5, 0.5, 0.28, 0.08, -0.3, 4.8);
            tri(&mut img, (0.76, 0.24), (0.82, 0.50), (0.56, 0.43));
        }
        Icon::Close => {
            rect_rotated(&mut img, 0.24, 0.46, 0.76, 0.54, 0.75);
            rect_rotated(&mut img, 0.24, 0.46, 0.76, 0.54, -0.75);
        }
    }
    img
}

// ---- drawing primitives; all coordinates are normalized 0.0..1.0 ----

fn set(img: &mut RgbaImage, x: i32, y: i32, on: bool) {
    if x < 0 || y < 0 || x as u32 >= R || y as u32 >= R {
        return;
    }
    let alpha = if on { 255 } else { 0 };
    img.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, alpha]));
}

fn rect(img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32) {
    let r = R as f32;
    for y in (y0 * r) as i32..(y1 * r) as i32 {
        for x in (x0 * r) as i32..(x1 * r) as i32 {
            set(img, x, y, true);
        }
    }
}

fn erase_rect(img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32) {
    let r = R as f32;
    for y in (y0 * r) as i32..(y1 * r) as i32 {
        for x in (x0 * r) as i32..(x1 * r) as i32 {
            set(img, x, y, false);
        }
    }
}

fn rect_rotated(img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32, angle: f32) {
    let r = R as f32;
    let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    let (hw, hh) = ((x1 - x0).abs() * 0.5, (y1 - y0).abs() * 0.5);
    let radius = (hw * hw + hh * hh).sqrt();
    let (sin, cos) = angle.sin_cos();
    for y in ((cy - radius) * r) as i32..=((cy + radius) * r) as i32 {
        for x in ((cx - radius) * r) as i32..=((cx + radius) * r) as i32 {
            let px = x as f32 / r - cx;
            let py = y as f32 / r - cy;
            let local_x = px * cos + py * sin;
            let local_y = -px * sin + py * cos;
            if local_x.abs() <= hw && local_y.abs() <= hh {
                set(img, x, y, true);
            }
        }
    }
}

fn disc(img: &mut RgbaImage, cx: f32, cy: f32, radius: f32) {
    band(img, cx, cy, 0.0, radius);
}

fn erase_disc(img: &mut RgbaImage, cx: f32, cy: f32, radius: f32) {
    let r = R as f32;
    let (cxp, cyp, rp) = (cx * r, cy * r, radius * r);
    let r2 = rp * rp;
    for y in (cyp - rp) as i32..=(cyp + rp) as i32 {
        for x in (cxp - rp) as i32..=(cxp + rp) as i32 {
            let (dx, dy) = (x as f32 - cxp, y as f32 - cyp);
            if dx * dx + dy * dy <= r2 {
                set(img, x, y, false);
            }
        }
    }
}

fn band(img: &mut RgbaImage, cx: f32, cy: f32, r_in: f32, r_out: f32) {
    let r = R as f32;
    let (cxp, cyp) = (cx * r, cy * r);
    let (ri, ro) = (r_in * r, r_out * r);
    let (ri2, ro2) = (ri * ri, ro * ro);
    for y in (cyp - ro) as i32..=(cyp + ro) as i32 {
        for x in (cxp - ro) as i32..=(cxp + ro) as i32 {
            let (dx, dy) = (x as f32 - cxp, y as f32 - cyp);
            let d2 = dx * dx + dy * dy;
            if d2 >= ri2 && d2 <= ro2 {
                set(img, x, y, true);
            }
        }
    }
}

/// Draw an annulus band restricted to the angle range `[a0, a1]` (radians,
/// `a0 < a1`, screen-space where +y points down).
fn arc(img: &mut RgbaImage, cx: f32, cy: f32, radius: f32, thick: f32, a0: f32, a1: f32) {
    use std::f32::consts::TAU;
    let r = R as f32;
    let (cxp, cyp) = (cx * r, cy * r);
    let ro = (radius + thick / 2.0) * r;
    let ri = (radius - thick / 2.0) * r;
    let (ri2, ro2) = (ri * ri, ro * ro);
    for y in (cyp - ro) as i32..=(cyp + ro) as i32 {
        for x in (cxp - ro) as i32..=(cxp + ro) as i32 {
            let (dx, dy) = (x as f32 - cxp, y as f32 - cyp);
            let d2 = dx * dx + dy * dy;
            if d2 < ri2 || d2 > ro2 {
                continue;
            }
            let mut ang = dy.atan2(dx);
            while ang < a0 {
                ang += TAU;
            }
            if ang <= a1 {
                set(img, x, y, true);
            }
        }
    }
}

fn tri(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), c: (f32, f32)) {
    let r = R as f32;
    let p = [(a.0 * r, a.1 * r), (b.0 * r, b.1 * r), (c.0 * r, c.1 * r)];
    let minx = p.iter().map(|q| q.0).fold(f32::MAX, f32::min) as i32;
    let maxx = p.iter().map(|q| q.0).fold(f32::MIN, f32::max) as i32;
    let miny = p.iter().map(|q| q.1).fold(f32::MAX, f32::min) as i32;
    let maxy = p.iter().map(|q| q.1).fold(f32::MIN, f32::max) as i32;
    let edge = |ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32| {
        (px - bx) * (ay - by) - (ax - bx) * (py - by)
    };
    for y in miny..=maxy {
        for x in minx..=maxx {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d1 = edge(p[0].0, p[0].1, p[1].0, p[1].1, fx, fy);
            let d2 = edge(p[1].0, p[1].1, p[2].0, p[2].1, fx, fy);
            let d3 = edge(p[2].0, p[2].1, p[0].0, p[0].1, fx, fy);
            let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
            let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
            if !(has_neg && has_pos) {
                set(img, x, y, true);
            }
        }
    }
}

fn star(img: &mut RgbaImage, cx: f32, cy: f32, r_outer: f32, r_inner: f32) {
    use std::f32::consts::TAU;
    let mut points = Vec::with_capacity(10);
    for i in 0..10 {
        let radius = if i % 2 == 0 { r_outer } else { r_inner };
        let angle = -TAU / 4.0 + i as f32 * TAU / 10.0;
        points.push((cx + radius * angle.cos(), cy + radius * angle.sin()));
    }
    for i in 0..10 {
        tri(img, (cx, cy), points[i], points[(i + 1) % 10]);
    }
}

fn speaker(img: &mut RgbaImage) {
    rect(img, 0.16, 0.42, 0.28, 0.58);
    tri(img, (0.28, 0.42), (0.28, 0.58), (0.46, 0.72));
    tri(img, (0.28, 0.42), (0.46, 0.72), (0.46, 0.28));
}

fn corners(img: &mut RgbaImage) {
    let (t, len, a, b) = (0.06, 0.22, 0.16, 0.84);
    rect(img, a, a, a + len, a + t);
    rect(img, a, a, a + t, a + len);
    rect(img, b - len, a, b, a + t);
    rect(img, b - t, a, b, a + len);
    rect(img, a, b - t, a + len, b);
    rect(img, a, b - len, a + t, b);
    rect(img, b - len, b - t, b, b);
    rect(img, b - t, b - len, b, b);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_catalog_system_has_a_nonempty_distinct_hardware_icon() {
        let mut seen = HashSet::new();
        for system in SYSTEMS {
            let raster = downscale(render_system_badge(system.id));
            assert!(
                raster.pixels().any(|pixel| pixel.0[3] != 0),
                "empty system icon for {}",
                system.id
            );
            assert!(
                seen.insert(raster.into_raw()),
                "duplicate system icon for {}",
                system.id
            );
        }
    }

    #[test]
    fn tigerwolf_brand_uses_the_supplied_png() {
        let raster = icon_raster(Icon::Brand);
        assert_eq!(raster.dimensions(), (305, 305));
        assert!(raster.pixels().any(|pixel| pixel.0 == [244, 129, 32, 255]));
        assert!(raster.pixels().any(|pixel| pixel.0 == [146, 148, 151, 255]));
        assert!(raster.pixels().any(|pixel| pixel.0[3] == 0));
    }
}
