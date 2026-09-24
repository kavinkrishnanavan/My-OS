//! Decodes downloaded image bytes into a plain RGB bitmap the framebuffer
//! can blit. PNG only for now (see `png.rs`) — enough to show most icons
//! and simple web graphics; JPEG/WebP/GIF are out of scope (each is its
//! own decoder the size of `png.rs` or bigger) and are silently skipped,
//! same as any other unsupported format.

mod png;

use crate::gfx::Color;
use alloc::vec::Vec;

pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    /// Row-major, `width * height` entries, alpha already blended over
    /// the page background (see `png::decode`) since the framebuffer
    /// blit itself is opaque.
    pub pixels: Vec<Color>,
}

/// Sniffs `bytes` for a known image signature and decodes it. Returns
/// `None` for anything unrecognized, truncated, or using a feature this
/// decoder doesn't handle (interlacing, >8-bit channels, absurd
/// dimensions) — callers treat that exactly like a failed fetch.
pub fn decode(bytes: &[u8]) -> Option<Bitmap> {
    if bytes.starts_with(&png::SIGNATURE) {
        return png::decode(bytes);
    }
    None
}
