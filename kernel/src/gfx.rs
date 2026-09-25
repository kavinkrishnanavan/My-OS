//! Draws onto the bootloader's linear framebuffer.
//!
//! This is what turns "we fetched some bytes over TCP" into an actual
//! on-screen page: a pixel-level `Framebuffer` (this module) plus an
//! HTML-to-text extractor (`html.rs`) that a simple flowed-text renderer
//! walks to draw glyphs.

use bootloader_api::info::{FrameBufferInfo, PixelFormat};
pub use noto_sans_mono_bitmap::FontWeight;
pub use noto_sans_mono_bitmap::RasterHeight as FontSize;
use noto_sans_mono_bitmap::{get_raster, RasterHeight};
use spin::Mutex;

pub static SCREEN: Mutex<Option<Framebuffer>> = Mutex::new(None);

pub struct Framebuffer {
    buffer: &'static mut [u8],
    info: FrameBufferInfo,
}

#[derive(Clone, Copy)]
pub struct Color(pub u8, pub u8, pub u8);

pub const BLACK: Color = Color(0x18, 0x18, 0x1c);
pub const GRAY: Color = Color(0x90, 0x90, 0x98);

impl Framebuffer {
    pub fn new(buffer: &'static mut [u8], info: FrameBufferInfo) -> Self {
        Framebuffer { buffer, info }
    }

    pub fn width(&self) -> usize {
        self.info.width
    }

    pub fn height(&self) -> usize {
        self.info.height
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
        if byte_offset + bpp <= self.buffer.len() {
            self.buffer[byte_offset..byte_offset + bpp].copy_from_slice(&bytes[..bpp]);
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
    /// growing-triangle silhouette with a light outline on its trailing
    /// edge, so it stays visible against light AND dark page
    /// backgrounds alike (a page's own content color can't be known
    /// generically here). Always drawn last, on top of everything else
    /// (`net/http.rs` calls this immediately after `draw_at_scroll`).
    pub fn draw_cursor(&mut self, x: usize, y: usize) {
        const HEIGHT: usize = 14;
        const FILL: Color = Color(0x10, 0x10, 0x10);
        const OUTLINE: Color = Color(0xf5, 0xf5, 0xf5);
        for row in 0..HEIGHT {
            let width = (row + 1).min(9);
            for col in 0..width {
                let on_edge = col == width - 1 || row == HEIGHT - 1;
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
