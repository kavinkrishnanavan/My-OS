//! A baseline (SOF0) JPEG decoder: marker walking, Huffman-coded DC/AC
//! coefficient decoding, dequantization, a separable float IDCT, and
//! YCbCr->RGB with nearest-neighbor chroma upsampling (4:4:4/4:2:2/4:2:0,
//! the subsampling modes essentially every real photo on the web uses).
//!
//! Deliberately unsupported (all return `None`, same as any other
//! unsupported image — see `img::decode`): progressive JPEG (SOF2),
//! arithmetic coding, lossless/extended-sequential SOF variants, 12-bit
//! precision, and CMYK/Adobe 4-component color transforms. APPn/COM
//! metadata segments are skipped, not parsed. Restart markers (DRI/RSTn)
//! are handled (common in camera/thumbnail output) but only the simple
//! case: on a decode error we just stop and composite whatever MCUs
//! decoded so far rather than trying to resynchronize mid-stream.

use super::Bitmap;
use crate::gfx::Color;
use alloc::vec;
use alloc::vec::Vec;

pub const SIGNATURE: [u8; 2] = [0xFF, 0xD8];

/// Same reasoning as `png.rs`'s `MAX_PIXELS`: refuse to trust a claimed
/// width/height before we've allocated anything for it.
const MAX_PIXELS: u64 = 1_500_000;

#[rustfmt::skip]
const ZIGZAG: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

struct CompInfo {
    id: u8,
    h: u8,
    v: u8,
    tq: u8,
}

struct SofInfo {
    width: usize,
    height: usize,
    components: Vec<CompInfo>,
}

struct ScanComp {
    comp_idx: usize,
    dc: usize,
    ac: usize,
}

pub fn decode(bytes: &[u8]) -> Option<Bitmap> {
    if !bytes.starts_with(&SIGNATURE) {
        return None;
    }
    let mut pos = 2usize;
    let mut qtables: [[u16; 64]; 4] = [[0; 64]; 4];
    let mut qset = [false; 4];
    let mut huff_dc: [Option<HuffTable>; 4] = [None, None, None, None];
    let mut huff_ac: [Option<HuffTable>; 4] = [None, None, None, None];
    let mut sof: Option<SofInfo> = None;
    let mut restart_interval: usize = 0;

    loop {
        let m0 = *bytes.get(pos)?;
        if m0 != 0xFF {
            return None; // lost sync between segments
        }
        let mut mpos = pos;
        while bytes.get(mpos) == Some(&0xFF) {
            mpos += 1;
        }
        let marker = *bytes.get(mpos)?;
        pos = mpos + 1;

        match marker {
            0xD8 => continue,                    // SOI
            0xD9 => break,                        // EOI with no SOS: nothing to decode
            0x01 => continue,                     // TEM, no payload
            0xD0..=0xD7 => continue,              // stray RSTn outside a scan
            0xC0 => {
                let (info, np) = parse_sof(bytes, pos)?;
                sof = Some(info);
                pos = np;
            }
            0xC4 => pos = parse_dht(bytes, pos, &mut huff_dc, &mut huff_ac)?,
            0xCC => return None,                  // DAC: arithmetic coding, unsupported
            0xC1..=0xC3 | 0xC5..=0xCB | 0xCD..=0xCF => return None, // other SOF variants
            0xDB => pos = parse_dqt(bytes, pos, &mut qtables, &mut qset)?,
            0xDD => {
                let (ri, np) = parse_dri(bytes, pos)?;
                restart_interval = ri;
                pos = np;
            }
            0xDA => {
                let sof_info = sof.as_ref()?;
                let (scan_start, comps) = parse_sos(bytes, pos, sof_info)?;
                return decode_scan(
                    bytes,
                    scan_start,
                    sof_info,
                    &comps,
                    &qtables,
                    &qset,
                    &huff_dc,
                    &huff_ac,
                    restart_interval,
                );
            }
            _ => pos = skip_segment(bytes, pos)?,
        }
    }
    None
}

fn read_u16(bytes: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.get(pos)?, *bytes.get(pos + 1)?]))
}

fn skip_segment(bytes: &[u8], pos: usize) -> Option<usize> {
    let len = read_u16(bytes, pos)? as usize;
    if len < 2 {
        return None;
    }
    let end = pos.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    Some(end)
}

fn parse_sof(bytes: &[u8], pos: usize) -> Option<(SofInfo, usize)> {
    let len = read_u16(bytes, pos)? as usize;
    if len < 8 {
        return None;
    }
    let end = pos.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    let precision = *bytes.get(pos + 2)?;
    if precision != 8 {
        return None;
    }
    let height = read_u16(bytes, pos + 3)? as usize;
    let width = read_u16(bytes, pos + 5)? as usize;
    if width == 0 || height == 0 {
        return None;
    }
    if (width as u64) * (height as u64) > MAX_PIXELS {
        return None;
    }
    let nc = *bytes.get(pos + 7)? as usize;
    if nc != 1 && nc != 3 {
        return None; // grayscale or YCbCr only; CMYK etc. out of scope
    }
    let mut components = Vec::with_capacity(nc);
    let mut p = pos + 8;
    for _ in 0..nc {
        let id = *bytes.get(p)?;
        let hv = *bytes.get(p + 1)?;
        let tq = *bytes.get(p + 2)?;
        let h = hv >> 4;
        let v = hv & 0xF;
        if h == 0 || h > 4 || v == 0 || v > 4 || tq >= 4 {
            return None;
        }
        components.push(CompInfo { id, h, v, tq });
        p += 3;
    }
    Some((SofInfo { width, height, components }, end))
}

fn parse_dqt(bytes: &[u8], pos: usize, qtables: &mut [[u16; 64]; 4], qset: &mut [bool; 4]) -> Option<usize> {
    let len = read_u16(bytes, pos)? as usize;
    if len < 2 {
        return None;
    }
    let end = pos.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    let mut p = pos + 2;
    while p < end {
        let pq_tq = *bytes.get(p)?;
        let pq = pq_tq >> 4;
        let tq = (pq_tq & 0xF) as usize;
        if tq >= 4 {
            return None;
        }
        p += 1;
        let mut table = [0u16; 64];
        for slot in table.iter_mut() {
            if pq == 0 {
                *slot = *bytes.get(p)? as u16;
                p += 1;
            } else {
                *slot = read_u16(bytes, p)?;
                p += 2;
            }
        }
        qtables[tq] = table;
        qset[tq] = true;
    }
    Some(end)
}

fn parse_dht(
    bytes: &[u8],
    pos: usize,
    huff_dc: &mut [Option<HuffTable>; 4],
    huff_ac: &mut [Option<HuffTable>; 4],
) -> Option<usize> {
    let len = read_u16(bytes, pos)? as usize;
    if len < 2 {
        return None;
    }
    let end = pos.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    let mut p = pos + 2;
    while p < end {
        let tc_th = *bytes.get(p)?;
        let tc = tc_th >> 4;
        let th = (tc_th & 0xF) as usize;
        if th >= 4 {
            return None;
        }
        p += 1;
        let mut counts = [0u8; 16];
        let mut total = 0usize;
        for (i, c) in counts.iter_mut().enumerate() {
            let byte = *bytes.get(p + i)?;
            *c = byte;
            total += byte as usize;
        }
        p += 16;
        if total > 256 || p.checked_add(total)? > end {
            return None;
        }
        let values = bytes.get(p..p + total)?.to_vec();
        p += total;
        let table = HuffTable::build(&counts, &values);
        if tc == 0 {
            huff_dc[th] = Some(table);
        } else {
            huff_ac[th] = Some(table);
        }
    }
    Some(end)
}

fn parse_dri(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    let len = read_u16(bytes, pos)? as usize;
    if len != 4 {
        return None;
    }
    let end = pos.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    let ri = read_u16(bytes, pos + 2)? as usize;
    Some((ri, end))
}

fn parse_sos(bytes: &[u8], pos: usize, sof: &SofInfo) -> Option<(usize, Vec<ScanComp>)> {
    let len = read_u16(bytes, pos)? as usize;
    if len < 6 {
        return None;
    }
    if pos.checked_add(len)? > bytes.len() {
        return None;
    }
    let ns = *bytes.get(pos + 2)? as usize;
    if ns == 0 || ns > 4 {
        return None;
    }
    let mut comps = Vec::with_capacity(ns);
    let mut p = pos + 3;
    for _ in 0..ns {
        let cs = *bytes.get(p)?;
        let td_ta = *bytes.get(p + 1)?;
        let td = (td_ta >> 4) as usize;
        let ta = (td_ta & 0xF) as usize;
        if td >= 4 || ta >= 4 {
            return None;
        }
        let comp_idx = sof.components.iter().position(|c| c.id == cs)?;
        comps.push(ScanComp { comp_idx, dc: td, ac: ta });
        p += 2;
    }
    if p.checked_add(3)? > bytes.len() {
        return None;
    }
    p += 3; // Ss, Se, Ah/Al: baseline single-scan always covers the full block
    Some((p, comps))
}

/// JPEG's canonical Huffman table, decoded with the mincode/maxcode/valptr
/// scheme from the spec's own reference decoder (Annex F) rather than a
/// bit-by-bit tree walk — same result, simpler to keep panic-free.
struct HuffTable {
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [i32; 17],
    values: Vec<u8>,
}

impl HuffTable {
    fn build(counts: &[u8; 16], values: &[u8]) -> Self {
        let mut sizes: Vec<u8> = Vec::new();
        for (len_idx, &count) in counts.iter().enumerate() {
            for _ in 0..count {
                sizes.push((len_idx + 1) as u8);
            }
        }
        let mut codes = vec![0u16; sizes.len()];
        let mut code = 0u16;
        let mut k = 0usize;
        let mut si = sizes.first().copied().unwrap_or(0);
        while k < sizes.len() {
            while k < sizes.len() && sizes[k] == si {
                codes[k] = code;
                code = code.wrapping_add(1);
                k += 1;
            }
            code <<= 1;
            si += 1;
        }

        let mut mincode = [0i32; 17];
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0i32; 17];
        let mut p = 0usize;
        for l in 1..=16usize {
            let cnt = counts[l - 1] as usize;
            if cnt == 0 {
                continue;
            }
            valptr[l] = p as i32;
            mincode[l] = codes[p] as i32;
            p += cnt;
            maxcode[l] = codes[p - 1] as i32;
        }

        HuffTable { mincode, maxcode, valptr, values: values.to_vec() }
    }

    fn decode_symbol(&self, br: &mut BitReader) -> Option<u8> {
        let mut code = br.get_bit() as i32;
        let mut l = 1usize;
        loop {
            if l > 16 {
                return None;
            }
            if self.maxcode[l] != -1 && code <= self.maxcode[l] {
                let idx = self.valptr[l] + (code - self.mincode[l]);
                if idx < 0 {
                    return None;
                }
                return self.values.get(idx as usize).copied();
            }
            code = (code << 1) | br.get_bit() as i32;
            l += 1;
        }
    }
}

/// Bit reader over the entropy-coded scan segment. Handles byte stuffing
/// (`0xFF 0x00` -> literal `0xFF`) and stops supplying real bits the
/// moment it sees an actual marker (`0xFF` followed by anything else),
/// padding with zero bits after that so callers never read out of bounds
/// — a truncated scan just produces a partially-gray image, not a panic.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bit_count: u32,
    marker_hit: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bit_buf: 0, bit_count: 0, marker_hit: false }
    }

    fn fill_byte(&mut self) -> u8 {
        if self.marker_hit {
            return 0;
        }
        let b = match self.data.get(self.pos) {
            Some(&b) => b,
            None => {
                self.marker_hit = true;
                return 0;
            }
        };
        if b == 0xFF {
            match self.data.get(self.pos + 1) {
                Some(&0x00) => {
                    self.pos += 2;
                    0xFF
                }
                Some(_) => {
                    // Real marker: leave `pos` pointing at the 0xFF so a
                    // restart-interval resync can find it.
                    self.marker_hit = true;
                    0
                }
                None => {
                    self.marker_hit = true;
                    0
                }
            }
        } else {
            self.pos += 1;
            b
        }
    }

    fn get_bit(&mut self) -> u32 {
        if self.bit_count == 0 {
            self.bit_buf = self.fill_byte() as u32;
            self.bit_count = 8;
        }
        self.bit_count -= 1;
        (self.bit_buf >> self.bit_count) & 1
    }

    fn get_bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.get_bit();
        }
        v
    }

    /// Discard the current partial byte and, if a restart marker sits at
    /// the current position, skip past it. If it doesn't (corrupt/odd
    /// stream), we simply carry on from wherever we are rather than
    /// trying to search for one — worst case the rest of the image comes
    /// out garbled, not a crash.
    fn restart(&mut self) {
        self.bit_count = 0;
        self.marker_hit = false;
        if let Some(&0xFF) = self.data.get(self.pos) {
            if let Some(&m) = self.data.get(self.pos + 1) {
                if (0xD0..=0xD7).contains(&m) {
                    self.pos += 2;
                }
            }
        }
    }
}

fn extend(bits: u32, s: u8) -> i32 {
    let vt = 1i32 << (s - 1);
    let bits = bits as i32;
    if bits < vt {
        bits - (1i32 << s) + 1
    } else {
        bits
    }
}

fn decode_block(br: &mut BitReader, dc: &HuffTable, ac: &HuffTable, pred: &mut i32, coef: &mut [i32; 64]) -> Option<()> {
    let s = dc.decode_symbol(br)?;
    if s > 16 {
        return None;
    }
    let diff = if s == 0 { 0 } else { extend(br.get_bits(s as u32), s) };
    *pred = pred.checked_add(diff)?;
    coef[0] = *pred;

    let mut k = 1usize;
    while k < 64 {
        let rs = ac.decode_symbol(br)?;
        let r = (rs >> 4) as usize;
        let s = rs & 0xF;
        if s == 0 {
            if r == 15 {
                k += 16;
                continue;
            }
            break; // EOB
        }
        k += r;
        if k >= 64 || s > 16 {
            break;
        }
        coef[k] = extend(br.get_bits(s as u32), s);
        k += 1;
    }
    Some(())
}

/// cos((2x+1)*u*pi/16) for the 8-point IDCT, built from a 17-entry table
/// of pi/16 multiples (no runtime `cos()` — `core` has no transcendental
/// functions in `no_std` without pulling in `libm`, so these are the
/// well-known exact values, reduced by the standard reflection
/// `cos(k*pi/16) == cos((32-k)*pi/16)` for k > 16).
fn build_cos_table() -> [[f32; 8]; 8] {
    const COS0: [f32; 17] = [
        1.0, 0.980_785_3, 0.923_879_5, 0.831_469_6, 0.707_106_8, 0.555_570_2, 0.382_683_43, 0.195_090_32, 0.0,
        -0.195_090_32, -0.382_683_43, -0.555_570_2, -0.707_106_8, -0.831_469_6, -0.923_879_5, -0.980_785_3, -1.0,
    ];
    let mut t = [[0f32; 8]; 8];
    for (x, row) in t.iter_mut().enumerate() {
        for (u, cell) in row.iter_mut().enumerate() {
            let k = ((2 * x + 1) * u) % 32;
            let r = if k <= 16 { k } else { 32 - k };
            *cell = COS0[r];
        }
    }
    t
}

fn idct8x8(coef: &[f32; 64], cos: &[[f32; 8]; 8]) -> [f32; 64] {
    const FRAC_1_SQRT2: f32 = core::f32::consts::FRAC_1_SQRT_2;
    let mut tmp = [0f32; 64];
    for v in 0..8 {
        for x in 0..8 {
            let mut sum = 0f32;
            for u in 0..8 {
                let cu = if u == 0 { FRAC_1_SQRT2 } else { 1.0 };
                sum += cu * coef[v * 8 + u] * cos[x][u];
            }
            tmp[v * 8 + x] = 0.5 * sum;
        }
    }
    let mut out = [0f32; 64];
    for x in 0..8 {
        for y in 0..8 {
            let mut sum = 0f32;
            for v in 0..8 {
                let cv = if v == 0 { FRAC_1_SQRT2 } else { 1.0 };
                sum += cv * tmp[v * 8 + x] * cos[y][v];
            }
            out[y * 8 + x] = 0.5 * sum;
        }
    }
    out
}

fn clamp8(v: f32) -> u8 {
    if v < 0.0 {
        0
    } else if v > 255.0 {
        255
    } else {
        v as u8
    }
}

struct Comp {
    h: usize,
    v: usize,
    tq: usize,
    dc_table: usize,
    ac_table: usize,
    width: usize,
    height: usize,
    samples: Vec<u8>,
    pred: i32,
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    bytes: &[u8],
    start: usize,
    sof: &SofInfo,
    scan_comps: &[ScanComp],
    qtables: &[[u16; 64]; 4],
    qset: &[bool; 4],
    huff_dc: &[Option<HuffTable>; 4],
    huff_ac: &[Option<HuffTable>; 4],
    restart_interval: usize,
) -> Option<Bitmap> {
    let nc = sof.components.len();
    if nc != 1 && nc != 3 {
        return None;
    }
    for c in &sof.components {
        if !qset[c.tq as usize] {
            return None;
        }
    }
    let h_max = sof.components.iter().map(|c| c.h).max()? as usize;
    let v_max = sof.components.iter().map(|c| c.v).max()? as usize;
    if h_max == 0 || v_max == 0 {
        return None;
    }

    let mcu_w = 8 * h_max;
    let mcu_h = 8 * v_max;
    let mcu_cols = (sof.width + mcu_w - 1) / mcu_w;
    let mcu_rows = (sof.height + mcu_h - 1) / mcu_h;
    if mcu_cols == 0 || mcu_rows == 0 {
        return None;
    }
    let total_mcus = mcu_cols.checked_mul(mcu_rows)?;

    let mut comps: Vec<Comp> = Vec::with_capacity(nc);
    for (i, c) in sof.components.iter().enumerate() {
        let sc = scan_comps.iter().find(|s| s.comp_idx == i)?;
        if huff_dc[sc.dc].is_none() || huff_ac[sc.ac].is_none() {
            return None;
        }
        let cw = mcu_cols.checked_mul(8)?.checked_mul(c.h as usize)?;
        let ch = mcu_rows.checked_mul(8)?.checked_mul(c.v as usize)?;
        let size = cw.checked_mul(ch)?;
        comps.push(Comp {
            h: c.h as usize,
            v: c.v as usize,
            tq: c.tq as usize,
            dc_table: sc.dc,
            ac_table: sc.ac,
            width: cw,
            height: ch,
            samples: vec![0u8; size],
            pred: 0,
        });
    }

    let cos = build_cos_table();
    let mut br = BitReader::new(bytes.get(start..)?);
    let mut mcus_since_restart = 0usize;

    'outer: for mcu_idx in 0..total_mcus {
        let mcu_col = mcu_idx % mcu_cols;
        let mcu_row = mcu_idx / mcu_cols;
        for ci in 0..comps.len() {
            let h = comps[ci].h;
            let v = comps[ci].v;
            let tq = comps[ci].tq;
            let dct = comps[ci].dc_table;
            let act = comps[ci].ac_table;
            let dc_table = huff_dc[dct].as_ref()?;
            let ac_table = huff_ac[act].as_ref()?;
            for by in 0..v {
                for bx in 0..h {
                    let mut coef = [0i32; 64];
                    if decode_block(&mut br, dc_table, ac_table, &mut comps[ci].pred, &mut coef).is_none() {
                        break 'outer;
                    }
                    let quant = &qtables[tq];
                    let mut natural = [0f32; 64];
                    for k in 0..64 {
                        let val = (coef[k] as i64) * (quant[k] as i64);
                        natural[ZIGZAG[k]] = val as f32;
                    }
                    let block = idct8x8(&natural, &cos);
                    let ox = (mcu_col * h + bx) * 8;
                    let oy = (mcu_row * v + by) * 8;
                    let cw = comps[ci].width;
                    for yy in 0..8 {
                        let row_off = match (oy + yy).checked_mul(cw).and_then(|o| o.checked_add(ox)) {
                            Some(o) => o,
                            None => continue,
                        };
                        if row_off + 8 > comps[ci].samples.len() {
                            continue;
                        }
                        for xx in 0..8 {
                            let px = block[yy * 8 + xx] + 128.0;
                            comps[ci].samples[row_off + xx] = clamp8(px);
                        }
                    }
                }
            }
        }
        mcus_since_restart += 1;
        if restart_interval > 0 && mcus_since_restart == restart_interval && mcu_idx + 1 < total_mcus {
            br.restart();
            for c in comps.iter_mut() {
                c.pred = 0;
            }
            mcus_since_restart = 0;
        }
    }

    let width = sof.width;
    let height = sof.height;
    let mut pixels = Vec::with_capacity(width.checked_mul(height)?);
    for py in 0..height {
        for px in 0..width {
            if nc == 1 {
                let c = &comps[0];
                let sx = (px * c.h / h_max).min(c.width.saturating_sub(1));
                let sy = (py * c.v / v_max).min(c.height.saturating_sub(1));
                let y = c.samples[sy * c.width + sx];
                pixels.push(Color(y, y, y));
            } else {
                let sample_of = |c: &Comp| -> f32 {
                    let sx = (px * c.h / h_max).min(c.width.saturating_sub(1));
                    let sy = (py * c.v / v_max).min(c.height.saturating_sub(1));
                    c.samples[sy * c.width + sx] as f32
                };
                let yv = sample_of(&comps[0]);
                let cb = sample_of(&comps[1]) - 128.0;
                let cr = sample_of(&comps[2]) - 128.0;
                let r = yv + 1.402 * cr;
                let g = yv - 0.344_136 * cb - 0.714_136 * cr;
                let b = yv + 1.772 * cb;
                pixels.push(Color(clamp8(r), clamp8(g), clamp8(b)));
            }
        }
    }

    Some(Bitmap { width, height, pixels })
}
