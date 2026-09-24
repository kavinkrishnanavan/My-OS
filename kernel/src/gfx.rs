//! Draws onto the bootloader's linear framebuffer.
//!
//! This is what turns "we fetched some bytes over TCP" into an actual
//! on-screen page: a pixel-level `Framebuffer` (this module) plus an
//! HTML-to-text extractor (`html.rs`) that a simple flowed-text renderer
//! walks to draw glyphs.

use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use noto_sans_mono_bitmap::{get_raster, FontWeight, RasterHeight};
use spin::Mutex;

pub static SCREEN: Mutex<Option<Framebuffer>> = Mutex::new(None);

pub struct Framebuffer {
    buffer: &'static mut [u8],
    info: FrameBufferInfo,
}

#[derive(Clone, Copy)]
pub struct Color(pub u8, pub u8, pub u8);

pub const BLACK: Color = Color(0x18, 0x18, 0x1c);
pub const WHITE: Color = Color(0xea, 0xea, 0xea);
pub const BLUE: Color = Color(0x5a, 0x9c, 0xf5);
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

    pub fn clear(&mut self, color: Color) {
        for y in 0..self.info.height {
            for x in 0..self.info.width {
                self.put_pixel(x, y, color);
            }
        }
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

    fn draw_char(&mut self, x: usize, y: usize, c: char, color: Color, bg: Color, size: RasterHeight) {
        let Some(raster) = get_raster(c, FontWeight::Regular, size) else {
            return;
        };
        for (row, line) in raster.raster().iter().enumerate() {
            for (col, &intensity) in line.iter().enumerate() {
                self.blend_pixel(x + col, y + row, color, intensity, bg);
            }
        }
    }

    /// Draws `bitmap` with its top-left corner at `(x0, y0)`, downscaled
    /// (nearest-neighbor — no filtering hardware or FPU budget for
    /// anything fancier here) to fit within `max_w` if it's wider than
    /// that. Returns the y-coordinate just below the drawn image.
    pub fn draw_bitmap(&mut self, bitmap: &crate::img::Bitmap, x0: usize, y0: usize, max_w: usize) -> usize {
        if bitmap.width == 0 || bitmap.height == 0 {
            return y0;
        }
        let scale = if bitmap.width > max_w {
            max_w as f32 / bitmap.width as f32
        } else {
            1.0
        };
        let draw_w = ((bitmap.width as f32) * scale) as usize;
        let draw_h = ((bitmap.height as f32) * scale) as usize;
        let draw_w = draw_w.max(1);
        let draw_h = draw_h.max(1);

        for y in 0..draw_h {
            let src_y = (y * bitmap.height) / draw_h;
            for x in 0..draw_w {
                let src_x = (x * bitmap.width) / draw_w;
                let color = bitmap.pixels[src_y * bitmap.width + src_x];
                self.put_pixel(x0 + x, y0 + y, color);
            }
        }
        y0 + draw_h
    }

    /// Draws `text` starting at `(x0, y0)`, word-wrapping at `max_x`, and
    /// returns the y-coordinate just below the last line drawn (so
    /// callers can stack blocks of text one after another).
    pub fn draw_wrapped(
        &mut self,
        text: &str,
        x0: usize,
        mut y: usize,
        max_x: usize,
        color: Color,
        bg: Color,
        size: RasterHeight,
    ) -> usize {
        let char_w = noto_sans_mono_bitmap::get_raster_width(FontWeight::Regular, size);
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
                self.draw_char(x, y, c, color, bg, size);
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
}
