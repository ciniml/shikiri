// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Screen captures from the console line (doc/54, `t5_abi::capture`): the
//! frames put back together, turned to the user's view and written as PNG.

use t5_abi::capture as wire;

/// One screen as the kernel sent it.
pub struct Shot {
    pub id: u32,
    /// The framebuffer as the panel has it.
    pub width: usize,
    pub height: usize,
    /// Quarter turns clockwise from the panel's top to the user's top.
    pub orientation: u8,
    pub pixels: Vec<u16>,
}

impl Shot {
    /// The pixels as the user sees the screen: `(width, height, pixels)`.
    /// The inverse of the kernel's touch mapping (kernel/src/screen.rs).
    pub fn user_view(&self) -> (usize, usize, Vec<u16>) {
        let (w, h) = (self.width, self.height);
        let (uw, uh) = if self.orientation % 2 == 1 { (h, w) } else { (w, h) };
        let mut out = vec![0u16; w * h];
        for py in 0..h {
            for px in 0..w {
                let (ux, uy) = match self.orientation {
                    1 => (py, w - 1 - px),
                    2 => (w - 1 - px, h - 1 - py),
                    3 => (h - 1 - py, px),
                    _ => (px, py),
                };
                out[uy * uw + ux] = self.pixels[py * w + px];
            }
        }
        (uw, uh, out)
    }

    /// The user's view as PNG.
    pub fn png(&self) -> Vec<u8> {
        let (w, h, px) = self.user_view();
        let mut rgb = Vec::with_capacity(w * h * 3);
        for p in px {
            let (r, g, b) = rgb565(p);
            rgb.extend_from_slice(&[r, g, b]);
        }
        png_rgb(w, h, &rgb)
    }
}

/// RGB565 to 8 bits per channel (the top bits repeated below).
pub fn rgb565(p: u16) -> (u8, u8, u8) {
    let (r, g, b) = ((p >> 11) as u8 & 0x1F, (p >> 5) as u8 & 0x3F, p as u8 & 0x1F);
    (r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)
}

/// Puts the frames of a capture together.
#[derive(Default)]
pub struct Assembler {
    shot: Option<Shot>,
    next: usize,
    crc: u32,
}

impl Assembler {
    pub fn new() -> Assembler {
        Assembler::default()
    }

    /// One decoded capture frame (starting with `wire::FRAME`). A finished
    /// screen at its END frame; an error if frames went missing.
    pub fn push(&mut self, f: &[u8]) -> Option<Result<Shot, String>> {
        let u16_at = |o: usize| f.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let u32_at = |o: usize| f.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let id = u32_at(2)?;
        match *f.get(1)? {
            wire::START => {
                let (w, h) = (u16_at(6)? as usize, u16_at(8)? as usize);
                if f.get(11) != Some(&wire::RGB565) {
                    return Some(Err(format!("capture {id}: unknown format")));
                }
                self.shot = Some(Shot { id, width: w, height: h, orientation: f[10], pixels: Vec::with_capacity(w * h) });
                self.next = 0;
                self.crc = 0;
                None
            }
            wire::DATA => {
                let s = self.shot.as_mut().filter(|s| s.id == id)?;
                let first = u32_at(6)? as usize;
                if first != self.next {
                    self.shot = None;
                    return Some(Err(format!("capture {id}: pixels from {} missing (got {first})", self.next)));
                }
                let before = s.pixels.len();
                let px = &mut s.pixels;
                if t5_abi::capture::decode_runs(&f[10..], &mut |p| px.push(p)).is_none() {
                    self.shot = None;
                    return Some(Err(format!("capture {id}: bad runs at pixel {first}")));
                }
                let raw: Vec<u8> = s.pixels[before..].iter().flat_map(|p| p.to_le_bytes()).collect();
                self.crc = wire::crc32(self.crc, &raw);
                self.next = s.pixels.len();
                None
            }
            wire::END => {
                let s = self.shot.take().filter(|s| s.id == id)?;
                let (n, crc) = (u32_at(6)? as usize, u32_at(10)?);
                Some(if s.pixels.len() != n || n != s.width * s.height {
                    Err(format!("capture {id}: {} of {n} pixels", s.pixels.len()))
                } else if crc != self.crc {
                    Err(format!("capture {id}: CRC {:08x}, expected {crc:08x}", self.crc))
                } else {
                    Ok(s)
                })
            }
            _ => None,
        }
    }
}

/// An 8-bit RGB PNG with stored (uncompressed) deflate blocks: no
/// compression library needed.
pub fn png_rgb(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(h * (1 + 3 * w));
    for row in rgb.chunks(3 * w) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    while let Some(b) = blocks.next() {
        z.push(blocks.peek().is_none() as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr[..]), (b"IDAT", &z[..]), (b"IEND", &[][..])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = wire::crc32(0, &out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    out
}

fn adler32(b: &[u8]) -> u32 {
    let (mut a, mut s) = (1u32, 0u32);
    for chunk in b.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            s += a;
        }
        a %= 65521;
        s %= 65521;
    }
    s << 16 | a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(id: u32, w: u16, h: u16, orientation: u8, px: &[u16]) -> Vec<Vec<u8>> {
        let mut v = vec![[&[wire::FRAME, wire::START][..], &id.to_le_bytes(), &w.to_le_bytes(), &h.to_le_bytes(), &[orientation, wire::RGB565]].concat()];
        let mut crc = 0;
        for (i, c) in px.chunks(wire::CHUNK).enumerate() {
            let mut enc = vec![0u8; wire::DATA_MAX];
            let n = wire::encode_runs(c, &mut enc);
            v.push([&[wire::FRAME, wire::DATA][..], &id.to_le_bytes(), &((i * wire::CHUNK) as u32).to_le_bytes(), &enc[..n]].concat());
            crc = wire::crc32(crc, &c.iter().flat_map(|p| p.to_le_bytes()).collect::<Vec<_>>());
        }
        v.push([&[wire::FRAME, wire::END][..], &id.to_le_bytes(), &(px.len() as u32).to_le_bytes(), &crc.to_le_bytes()].concat());
        v
    }

    #[test]
    fn assembles_and_turns() {
        // 3 x 2 panel, orientation 3 (the Tab5's): user view 2 x 3.
        let px = [1, 2, 3, 4, 5, 6];
        let mut a = Assembler::new();
        let mut shot = None;
        for f in frames(7, 3, 2, 3, &px) {
            if let Some(r) = a.push(&f) {
                shot = Some(r.unwrap());
            }
        }
        let s = shot.unwrap();
        // User (x, y) = (h - 1 - py, px): panel row 1 is the user's column 0.
        assert_eq!(s.user_view(), (2, 3, vec![4, 1, 5, 2, 6, 3]));
        assert!(s.png().starts_with(b"\x89PNG"));
    }

    #[test]
    fn missing_frame_is_an_error() {
        let px: Vec<u16> = (0..2000).collect();
        let mut f = frames(1, 50, 40, 0, &px);
        f.remove(2);
        let mut a = Assembler::new();
        let errs: Vec<_> = f.iter().filter_map(|f| a.push(f)).collect();
        assert!(errs[0].is_err());
    }

    #[test]
    fn adler_known() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
