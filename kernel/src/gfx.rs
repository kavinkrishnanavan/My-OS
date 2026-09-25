//! Draws onto the bootloader's linear framebuffer.
//!
//! This is what turns "we fetched some bytes over TCP" into an actual
//! on-screen page: a pixel-level `Framebuffer` (this module) plus an
//! HTML-to-text extractor (`html.rs`) that a simple flowed-text renderer
//! walks to draw glyphs.

use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::{FrameBufferInfo, PixelFormat};
pub use noto_sans_mono_bitmap::FontWeight;
pub use noto_sans_mono_bitmap::RasterHeight as FontSize;
use noto_sans_mono_bitmap::{get_raster, RasterHeight};
use spin::Mutex;

pub static SCREEN: Mutex<Option<Framebuffer>> = Mutex::new(None);

/// Every draw call writes into `back_buffer`, an off-screen copy the same
/// size as the real video memory — never directly into `buffer` (the
/// actual linear framebuffer the display scans out of). `present()` is
/// the only thing that ever touches `buffer`, and it does so with one
/// single contiguous copy. This is what a real double-buffered renderer
/// does and for the same reason: without it, every draw call (a
/// background fill, then text, then the taskbar, then the cursor — each
/// a separate, comparatively slow, per-pixel loop) was visible on the
/// real display the instant it happened, so a redraw showed up as a
/// flash of blank background before the content painted back in on top
/// of it, then the taskbar, then the cursor — a real, visible flicker
/// on every single mouse move or scroll, not just a one-time glitch.
/// Every caller that used to draw straight to `SCREEN` must now call
/// `present()` once, after every draw call for that frame is done.
pub struct Framebuffer {
    buffer: &'static mut [u8],
    back_buffer: Vec<u8>,
    /// A snapshot of `back_buffer` taken right after real content (page
    /// text/images/taskbar/etc, everything except the cursor) was last
    /// fully redrawn — see `save_content`/`restore_content`. Lets a
    /// caller whose only change since the last frame is "the cursor
    /// moved" skip re-running its whole (potentially expensive, e.g. a
    /// full page's worth of text items) draw routine and instead just
    /// restore this snapshot (one fast contiguous copy) before drawing
    /// the cursor on top of it. Without this, every single mouse Move
    /// event — even just a few pixels, even with no scroll or click
    /// involved — forced a full page re-render, which is genuinely slow
    /// for a real page with many items; that's what made the Browser
    /// specifically feel unresponsive even after event coalescing (which
    /// only reduces how *often* a redraw happens, not how much work
    /// each individual redraw does).
    content_buffer: Vec<u8>,
    info: FrameBufferInfo,
}

#[derive(Clone, Copy)]
pub struct Color(pub u8, pub u8, pub u8);

pub const BLACK: Color = Color(0x18, 0x18, 0x1c);
pub const GRAY: Color = Color(0x90, 0x90, 0x98);

impl Framebuffer {
    pub fn new(buffer: &'static mut [u8], info: FrameBufferInfo) -> Self {
        let back_buffer = vec![0u8; buffer.len()];
        let content_buffer = back_buffer.clone();
        Framebuffer { buffer, back_buffer, content_buffer, info }
    }

    pub fn width(&self) -> usize {
        self.info.width
    }

    pub fn height(&self) -> usize {
        self.info.height
    }

    /// Copies the whole back buffer to the real video memory in one shot
    /// — the only place this struct ever writes to `buffer`. Must be
    /// called once after every draw call for a frame is finished; until
    /// it's called, nothing drawn since the last `present()` is visible
    /// on screen at all.
    pub fn present(&mut self) {
        self.buffer.copy_from_slice(&self.back_buffer);
    }

    /// Snapshots the current back buffer as "the real content, cursor not
    /// yet drawn" — call this right after a full content redraw, before
    /// drawing the cursor on top of it.
    pub fn save_content(&mut self) {
        self.content_buffer.copy_from_slice(&self.back_buffer);
    }

    /// Restores the back buffer to the last `save_content` snapshot — a
    /// cheap way to undo a previous frame's cursor draw before drawing
    /// the cursor again at its new position, without re-running whatever
    /// (potentially expensive) drawing produced the content in the first
    /// place.
    pub fn restore_content(&mut self) {
        self.back_buffer.copy_from_slice(&self.content_buffer);
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: Color) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let offset = y * self.info.stride + x;
        let byte_offset = offset * self.info.bytes_per_pixel;
        let Color(r, g, b) = color;
        let bytes: [u8; 4] = match self.info.pixel_format {
            PixelFormat::Rgb => [r, g, b, 0],
            PixelFormat::Bgr => [b, g, r, 0],
            PixelFormat::U8 => [((r as u16 + g as u16 + b as u16) / 3) as u8, 0, 0, 0],
            _ => [r, g, b, 0],
        };
        let bpp = self.info.bytes_per_pixel;
        if byte_offset + bpp <= self.back_buffer.len() {
            self.back_buffer[byte_offset..byte_offset + bpp].copy_from_slice(&bytes[..bpp]);
        }
    }

    /// Alpha-blends a single glyph pixel (0-255 intensity) over whatever
    /// is already there, rather than stamping a hard-edged box — this is
    /// what makes the anti-aliased font rasters actually look
    /// anti-aliased instead of jagged.
    fn blend_pixel(&mut self, x: usize, y: usize, color: Color, intensity: u8, bg: Color) {
        if intensity == 0 {
            return;
        }
        if intensity == 255 {
            self.put_pixel(x, y, color);
            return;
        }
        let a = intensity as u32;
        let blend = |fg: u8, bg: u8| -> u8 { ((fg as u32 * a + bg as u32 * (255 - a)) / 255) as u8 };
        self.put_pixel(
            x,
            y,
            Color(blend(color.0, bg.0), blend(color.1, bg.1), blend(color.2, bg.2)),
        );
    }

    fn draw_char_weighted(&mut self, x: usize, y: usize, c: char, color: Color, bg: Color, size: RasterHeight, weight: FontWeight) {
        let Some(raster) = get_raster(c, weight, size) else {
            return;
        };
        for (row, line) in raster.raster().iter().enumerate() {
            for (col, &intensity) in line.iter().enumerate() {
                self.blend_pixel(x + col, y + row, color, intensity, bg);
            }
        }
    }

    /// Draws `bitmap` with its top-left corner at `(x0, y0)`, scaled
    /// (nearest-neighbor — no filtering hardware or FPU budget for
    /// anything fancier here) to an EXACT `(target_w, target_h)` — up or
    /// down — rather than only capping an oversized image. `net/http.rs`
    /// computes `target_w`/`target_h` from the page's own declared
    /// on-page size when it has one (a real Wikipedia-style thumbnail is
    /// typically encoded far larger than its intended display size, so
    /// "fit to column if too wide" isn't the right rule at all — the
    /// page's own size should win), falling back to `bitmap_draw_size`'s
    /// fit-to-column sizing when the page doesn't declare one.
    pub fn draw_bitmap_scaled(&mut self, bitmap: &crate::img::Bitmap, x0: usize, y0: usize, target_w: usize, target_h: usize) {
        if bitmap.width == 0 || bitmap.height == 0 || target_w == 0 || target_h == 0 {
            return;
        }
        for y in 0..target_h {
            let src_y = (y * bitmap.height) / target_h;
            for x in 0..target_w {
                let src_x = (x * bitmap.width) / target_w;
                let color = bitmap.pixels[src_y * bitmap.width + src_x];
                self.put_pixel(x0 + x, y0 + y, color);
            }
        }
    }

    /// Draws a simple filled-arrow mouse cursor with its hotspot (the
    /// point that's actually "where the mouse is") at `(x, y)` — a
    /// growing-triangle silhouette (rows 0..8, tip at the hotspot) plus a
    /// rectangular "tail" (rows 8..14), outlined on *every* boundary
    /// edge — left (the long straight side), right/diagonal, and bottom
    /// — not just the right edge and bottom row. A previous version only
    /// outlined those latter two, leaving the whole left edge drawn in
    /// the plain near-black fill color with nothing to contrast it
    /// against a dark page/desktop background — on screen that reads as
    /// the cursor being "cut in half" (only the right/bottom ever
    /// visible), which is exactly the bug this fixes. Always drawn last,
    /// on top of everything else (`net/http.rs`/the desktop apps call
    /// this immediately after their own redraw).
    pub fn draw_cursor(&mut self, x: usize, y: usize) {
        const HEIGHT: usize = 14;
        const FILL: Color = Color(0x10, 0x10, 0x10);
        const OUTLINE: Color = Color(0xf5, 0xf5, 0xf5);
        for row in 0..HEIGHT {
            let width = (row + 1).min(9);
            for col in 0..width {
                let on_edge = col == 0 || col == width - 1 || row == 0 || row == HEIGHT - 1;
                let color = if on_edge { OUTLINE } else { FILL };
                self.put_pixel(x + col, y + row, color);
            }
        }
    }

    /// Draws `text` starting at `(x0, y0)`, word-wrapping at `max_x`, and
    /// returns the y-coordinate just below the last line drawn (so
    /// callers can stack blocks of text one after another).
    pub fn draw_wrapped(
        &mut self,
        text: &str,
        x0: usize,
        y: usize,
        max_x: usize,
        color: Color,
        bg: Color,
        size: RasterHeight,
    ) -> usize {
        self.draw_wrapped_styled(text, x0, y, max_x, color, bg, size, FontWeight::Regular)
    }

    /// Same as `draw_wrapped`, plus a `weight` — what `layout.rs` uses for
    /// everything (headings bold, body regular) instead of `draw_wrapped`,
    /// which stays around only for callers happy with always-regular text.
    pub fn draw_wrapped_styled(
        &mut self,
        text: &str,
        x0: usize,
        mut y: usize,
        max_x: usize,
        color: Color,
        bg: Color,
        size: RasterHeight,
        weight: FontWeight,
    ) -> usize {
        let char_w = noto_sans_mono_bitmap::get_raster_width(weight, size);
        let line_h = size.val() + 2;
        let mut x = x0;

        for word in text.split(' ') {
            if word.is_empty() {
                continue;
            }
            let word_w = word.chars().count() * char_w;
            if x != x0 && x + word_w > max_x {
                x = x0;
                y += line_h;
            }
            for c in word.chars() {
                if x + char_w > max_x {
                    x = x0;
                    y += line_h;
                }
                self.draw_char_weighted(x, y, c, color, bg, size, weight);
                x += char_w;
            }
            // trailing space after the word
            if x + char_w <= max_x {
                x += char_w;
            } else {
                x = x0;
                y += line_h;
            }
        }
        y + line_h
    }

    /// Fills an axis-aligned rectangle — used for a paragraph's own
    /// `background-color` box (see `net/http.rs`'s draw loop), drawn
    /// before the text so the glyphs' alpha-blending has the right `bg`
    /// to blend against.
    pub fn fill_rect(&mut self, x0: usize, y0: usize, w: usize, h: usize, color: Color) {
        for y in y0..y0.saturating_add(h) {
            for x in x0..x0.saturating_add(w) {
                self.put_pixel(x, y, color);
            }
        }
    }

    /// Fills a circle of radius `r` centered at `(cx, cy)` — used for the
    /// taskbar's app icons/logo (`desktop.rs`), which are simple vector
    /// shapes rather than loaded image assets.
    pub fn fill_circle(&mut self, cx: usize, cy: usize, r: usize, color: Color) {
        let r_i = r as isize;
        for dy in -r_i..=r_i {
            for dx in -r_i..=r_i {
                if dx * dx + dy * dy > r_i * r_i {
                    continue;
                }
                let x = cx as isize + dx;
                let y = cy as isize + dy;
                if x >= 0 && y >= 0 {
                    self.put_pixel(x as usize, y as usize, color);
                }
            }
        }
    }
}

/// The `(width, height)` `draw_bitmap` will actually draw at, given the
/// same `max_w` constraint — exposed so a caller (`net/http.rs`'s
/// scrolling redraw) can compute an image's on-screen height without
/// drawing it, the same reason `measure_wrapped_height` exists for text.
pub fn bitmap_draw_size(bitmap: &crate::img::Bitmap, max_w: usize) -> Option<(usize, usize)> {
    if bitmap.width == 0 || bitmap.height == 0 {
        return None;
    }
    let scale = if bitmap.width > max_w {
        max_w as f32 / bitmap.width as f32
    } else {
        1.0
    };
    let draw_w = (((bitmap.width as f32) * scale) as usize).max(1);
    let draw_h = (((bitmap.height as f32) * scale) as usize).max(1);
    Some((draw_w, draw_h))
}

/// Same wrapping arithmetic as `draw_wrapped_styled`, but touches no
/// pixels — just the final height, so a caller (`net/http.rs`'s draw
/// loop) can fill a background rect sized to the text BEFORE drawing the
/// text on top of it. Must stay in exact lockstep with
/// `draw_wrapped_styled`'s own line-breaking logic, or the measured
/// height won't match what actually gets drawn.
pub fn measure_wrapped_height(text: &str, x0: usize, max_x: usize, size: RasterHeight, weight: FontWeight) -> usize {
    let char_w = noto_sans_mono_bitmap::get_raster_width(weight, size);
    let line_h = size.val() + 2;
    let mut x = x0;
    let mut y = 0usize;

    for word in text.split(' ') {
        if word.is_empty() {
            continue;
        }
        let word_w = word.chars().count() * char_w;
        if x != x0 && x + word_w > max_x {
            x = x0;
            y += line_h;
        }
        for _ in word.chars() {
            if x + char_w > max_x {
                x = x0;
                y += line_h;
            }
            x += char_w;
        }
        if x + char_w <= max_x {
            x += char_w;
        } else {
            x = x0;
            y += line_h;
        }
    }
    y + line_h
}
