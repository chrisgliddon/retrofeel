//! Pixel-format conversion: libretro pixel formats → RGBA8.
//!
//! libretro reports frames in one of three formats (selected via
//! `RETRO_ENVIRONMENT_SET_PIXEL_FORMAT`):
//! * `0RGB1555` — 16-bit, 1 bit unused + 5R/5G/5B (legacy default)
//! * `RGB565`   — 16-bit, 5R/6G/5B
//! * `XRGB8888` — 32-bit, 8R/8G/8B + 8 unused (native endianness; on little-endian
//!   hosts the bytes are B,G,R,X — we handle both paths below)
//!
//! Cores may also pass a `NULL` data pointer with a non-zero pitch, which means
//! "reuse the previous frame" (some cores do this between changes). We surface
//! that as `Frame::Repeat`.

use crate::abi;

/// A decoded video frame in RGBA8 (row-major, top-to-bottom).
///
/// Re-exported from `retrofeel_types::VideoFrame` so the recording/capture
/// path has no libretro coupling. Non-libretro sources (e.g. Steam
/// ScreenCaptureKit) construct the same type directly from
/// `retrofeel_types::VideoFrame`.
pub use retrofeel_types::VideoFrame as Frame;

/// Core signalled "same frame as last time".
#[derive(Debug, Clone, Copy)]
pub struct FrameRepeat;

/// Convert a libretro video-refresh callback payload to RGBA8.
///
/// `data` is the raw pointer the core passed; `pitch` is the row stride in
/// **bytes** (not pixels). Returns `Err(FrameRepeat)` when `data` is null.
pub fn convert(
    data: *const std::ffi::c_void,
    width: u32,
    height: u32,
    pitch: usize,
    pixel_format: u32,
) -> Result<Frame, FrameRepeat> {
    if data.is_null() {
        return Err(FrameRepeat);
    }
    let n = (width as usize) * (height as usize);
    let mut rgba = Vec::with_capacity(n * 4);
    match pixel_format {
        abi::PIXEL_FORMAT_XRGB8888 => convert_xrgb8888(data, width, height, pitch, &mut rgba),
        abi::PIXEL_FORMAT_RGB565 => convert_rgb565(data, width, height, pitch, &mut rgba),
        abi::PIXEL_FORMAT_0RGB1555 => convert_0rgb1555(data, width, height, pitch, &mut rgba),
        other => panic!("unsupported libretro pixel format {other}"),
    }
    Ok(Frame {
        width,
        height,
        rgba,
    })
}

fn convert_xrgb8888(
    data: *const std::ffi::c_void,
    width: u32,
    height: u32,
    pitch: usize,
    out: &mut Vec<u8>,
) {
    let bytes_per_pixel = 4usize;
    let w = width as usize;
    let h = height as usize;
    let src = data as *const u8;
    for y in 0..h {
        let row = unsafe { src.add(y * pitch) };
        for x in 0..w {
            // Little-endian XRGB8888 in memory = bytes B, G, R, X.
            let b = unsafe { *row.add(x * bytes_per_pixel) };
            let g = unsafe { *row.add(x * bytes_per_pixel + 1) };
            let r = unsafe { *row.add(x * bytes_per_pixel + 2) };
            out.push(r);
            out.push(g);
            out.push(b);
            out.push(0xFF);
        }
    }
}

fn convert_rgb565(
    data: *const std::ffi::c_void,
    width: u32,
    height: u32,
    pitch: usize,
    out: &mut Vec<u8>,
) {
    let w = width as usize;
    let h = height as usize;
    let src = data as *const u16;
    let stride_u16 = pitch / 2;
    for y in 0..h {
        for x in 0..w {
            // Little-endian 16-bit: bits [15:11]=R, [10:5]=G, [4:0]=B.
            let px = unsafe { *src.add(y * stride_u16 + x) };
            let r5 = ((px >> 11) & 0x1F) as u8;
            let g6 = ((px >> 5) & 0x3F) as u8;
            let b5 = (px & 0x1F) as u8;
            // Expand to 8-bit by replicating the high bits.
            out.push((r5 << 3) | (r5 >> 2));
            out.push((g6 << 2) | (g6 >> 4));
            out.push((b5 << 3) | (b5 >> 2));
            out.push(0xFF);
        }
    }
}

fn convert_0rgb1555(
    data: *const std::ffi::c_void,
    width: u32,
    height: u32,
    pitch: usize,
    out: &mut Vec<u8>,
) {
    let w = width as usize;
    let h = height as usize;
    let src = data as *const u16;
    let stride_u16 = pitch / 2;
    for y in 0..h {
        for x in 0..w {
            // Little-endian 16-bit: bit 15 unused, [14:10]=R, [9:5]=G, [4:0]=B.
            let px = unsafe { *src.add(y * stride_u16 + x) };
            let r5 = ((px >> 10) & 0x1F) as u8;
            let g5 = ((px >> 5) & 0x1F) as u8;
            let b5 = (px & 0x1F) as u8;
            out.push((r5 << 3) | (r5 >> 2));
            out.push((g5 << 3) | (g5 >> 2));
            out.push((b5 << 3) | (b5 >> 2));
            out.push(0xFF);
        }
    }
}

/// Pack RGBA8 bytes back into XRGB8888 u32s (little-endian), used when handing
/// frames to code that wants the libretro-native 32-bit form.
pub fn rgba8_to_xrgb8888(rgba: &[u8]) -> Vec<u32> {
    rgba.chunks_exact(4)
        .map(|c| {
            let r = c[0] as u32;
            let g = c[1] as u32;
            let b = c[2] as u32;
            (0xFF << 24) | (r << 16) | (g << 8) | b
        })
        .collect()
}
