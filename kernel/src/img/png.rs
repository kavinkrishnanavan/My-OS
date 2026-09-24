//! A minimal PNG decoder: chunk walking, zlib inflate (via
//! `miniz_oxide`), scanline unfiltering, and the handful of color types
//! real web images use. No interlacing (`Adam7`) and no bit depths below
//! 8 — both rare enough on the web that bailing out (returning `None`,
//! same as any other unsupported image) is a reasonable trade for not
//! doubling this file's size.

use super::Bitmap;
use crate::gfx::{self, Color};
use alloc::vec;
use alloc::vec::Vec;

pub const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Refuse to decode anything bigger than this many pixels — a corrupt or
/// hostile `IHDR` can claim a huge canvas before we've even looked at the
/// (possibly tiny) compressed data behind it, and we'd rather show a
/// missing image than eat the whole heap.
const MAX_PIXELS: u64 = 1_500_000;

struct Ihdr {
    width: u32,
    height: u32,
    color_type: u8,
}

pub fn decode(bytes: &[u8]) -> Option<Bitmap> {
    let mut pos = SIGNATURE.len();
    let mut ihdr: Option<Ihdr> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    loop {
        if pos + 8 > bytes.len() {
            break;
        }
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = data_start.checked_add(len)?;
        if data_end + 4 > bytes.len() {
            break;
        }
        let data = &bytes[data_start..data_end];

        match kind {
            b"IHDR" => {
                if data.len() < 13 {
                    return None;
                }
                let width = u32::from_be_bytes(data[0..4].try_into().ok()?);
                let height = u32::from_be_bytes(data[4..8].try_into().ok()?);
                let bit_depth = data[8];
                let color_type = data[9];
                let interlace = data[12];
                if interlace != 0 || bit_depth != 8 {
                    return None; // Adam7 / sub-byte-depth PNGs: not supported
                }
                if (width as u64) * (height as u64) > MAX_PIXELS {
                    return None;
                }
                ihdr = Some(Ihdr { width, height, color_type });
            }
            b"PLTE" => {
                palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => {
                trns = data.to_vec();
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = data_end + 4; // skip the trailing CRC
    }

    let ihdr = ihdr?;
    let channels: usize = match ihdr.color_type {
        0 => 1, // grayscale
        2 => 3, // RGB
        3 => 1, // palette index
        4 => 2, // grayscale + alpha
        6 => 4, // RGBA
        _ => return None,
    };

    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).ok()?;
    let width = ihdr.width as usize;
    let height = ihdr.height as usize;
    let stride = width * channels;
    let unfiltered = unfilter(&raw, width, height, channels)?;

    let bg = gfx::BLACK;
    let mut pixels = Vec::with_capacity(width * height);
    for row in 0..height {
        let line = &unfiltered[row * stride..row * stride + stride];
        for col in 0..width {
            let px = &line[col * channels..col * channels + channels];
            let (rgb, alpha) = match ihdr.color_type {
                0 => ([px[0], px[0], px[0]], 255u8),
                2 => ([px[0], px[1], px[2]], 255),
                3 => {
                    let idx = px[0] as usize;
                    let rgb = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                    let a = trns.get(idx).copied().unwrap_or(255);
                    (rgb, a)
                }
                4 => ([px[0], px[0], px[0]], px[1]),
                6 => ([px[0], px[1], px[2]], px[3]),
                _ => unreachable!(),
            };
            pixels.push(blend(rgb, alpha, bg));
        }
    }

    Some(Bitmap { width, height, pixels })
}

fn blend(rgb: [u8; 3], alpha: u8, bg: Color) -> Color {
    if alpha == 255 {
        return Color(rgb[0], rgb[1], rgb[2]);
    }
    let a = alpha as u32;
    let mix = |fg: u8, bg: u8| -> u8 { ((fg as u32 * a + bg as u32 * (255 - a)) / 255) as u8 };
    Color(mix(rgb[0], bg.0), mix(rgb[1], bg.1), mix(rgb[2], bg.2))
}

/// Reverses PNG's per-scanline filtering (each row prefixed by a filter
/// type byte, referencing the byte to the left / row above / both).
fn unfilter(raw: &[u8], width: usize, height: usize, channels: usize) -> Option<Vec<u8>> {
    let stride = width * channels;
    let mut out = vec![0u8; stride * height];
    let mut pos = 0;

    for row in 0..height {
        if pos >= raw.len() {
            return None;
        }
        let filter = raw[pos];
        pos += 1;
        if pos + stride > raw.len() {
            return None;
        }
        let src = &raw[pos..pos + stride];
        pos += stride;

        let (out_before, out_row) = out.split_at_mut(row * stride);
        let out_row = &mut out_row[..stride];
        let prev_row: &[u8] = if row == 0 { &[] } else { &out_before[(row - 1) * stride..row * stride] };

        for i in 0..stride {
            let a = if i >= channels { out_row[i - channels] } else { 0 }; // left
            let b = if row > 0 { prev_row[i] } else { 0 }; // up
            let c = if row > 0 && i >= channels { prev_row[i - channels] } else { 0 }; // up-left
            let x = src[i];
            out_row[i] = match filter {
                0 => x,
                1 => x.wrapping_add(a),
                2 => x.wrapping_add(b),
                3 => x.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => x.wrapping_add(paeth(a, b, c)),
                _ => return None,
            };
        }
    }

    Some(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (a as i32, b as i32, c as i32);
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}
