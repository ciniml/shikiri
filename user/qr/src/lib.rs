#![cfg_attr(not(test), no_std)]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! QR codes (ISO/IEC 18004) for short texts such as a login URL (doc/71
//! §14.15): byte mode only, versions 1 to 10 (up to 271 bytes at level
//! L), the mask with the lowest penalty. No alloc: the symbol is a bitmap
//! of `u64` rows. The layout follows the standard's steps in the order
//! Nayuki's qrcodegen lays them out (function patterns, then the
//! codewords in the zigzag, then the mask).

/// The largest version made, and its size in modules.
pub const MAX_VERSION: usize = 10;
pub const MAX_SIZE: usize = 17 + 4 * MAX_VERSION;
const MAX_CODEWORDS: usize = 346;
const MAX_BLOCKS: usize = 8;
const MAX_ECC: usize = 30;

/// Error correction level: about 7, 15, 25 and 30 % of the codewords.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ecl {
    L,
    M,
    Q,
    H,
}

impl Ecl {
    fn index(self) -> usize {
        self as usize
    }
    /// The two bits in the format information.
    fn bits(self) -> u32 {
        [1, 0, 3, 2][self.index()]
    }
}

/// ECC codewords per block and the number of blocks, by level and version
/// (index 0 unused).
const ECC_PER_BLOCK: [[u8; MAX_VERSION + 1]; 4] = [
    [0, 7, 10, 15, 20, 26, 18, 20, 24, 30, 18],
    [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26],
    [0, 13, 22, 18, 26, 18, 24, 18, 22, 20, 24],
    [0, 17, 28, 22, 16, 22, 28, 26, 26, 24, 28],
];
const BLOCKS: [[u8; MAX_VERSION + 1]; 4] = [
    [0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 4],
    [0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5],
    [0, 1, 1, 2, 2, 4, 4, 6, 6, 8, 8],
    [0, 1, 1, 2, 4, 4, 4, 5, 6, 8, 8],
];

/// A QR symbol: `size` x `size` modules, true for dark.
#[derive(Clone)]
pub struct Qr {
    pub version: usize,
    pub ecl: Ecl,
    pub mask: u8,
    size: usize,
    dark: [u64; MAX_SIZE],
    function: [u64; MAX_SIZE],
}

impl Qr {
    /// `data` in the smallest version that holds it at level `min`, then
    /// the highest level that still fits that version.
    pub fn encode(data: &[u8], min: Ecl) -> Option<Qr> {
        let version = (1..=MAX_VERSION).find(|&v| data.len() <= capacity(v, min))?;
        let ecl = [Ecl::H, Ecl::Q, Ecl::M].into_iter().find(|&e| e.index() > min.index() && data.len() <= capacity(version, e)).unwrap_or(min);
        Qr::with(data, version, ecl, None)
    }

    /// `data` in `version` at level `ecl`, with `mask` (0 to 7) or the one
    /// with the lowest penalty. None if it does not fit.
    pub fn with(data: &[u8], version: usize, ecl: Ecl, mask: Option<u8>) -> Option<Qr> {
        if !(1..=MAX_VERSION).contains(&version) || data.len() > capacity(version, ecl) {
            return None;
        }
        let size = 17 + 4 * version;
        let mut q = Qr { version, ecl, mask: 0, size, dark: [0; MAX_SIZE], function: [0; MAX_SIZE] };
        q.draw_function_patterns();
        let mut codewords = [0u8; MAX_CODEWORDS];
        let n = codewords_of(data, version, ecl, &mut codewords);
        q.draw_codewords(&codewords[..n]);
        let mask = mask.unwrap_or_else(|| {
            (0..8u8)
                .min_by_key(|&m| {
                    let mut t = q.clone();
                    t.apply_mask(m);
                    t.draw_format(m);
                    t.penalty()
                })
                .unwrap()
        });
        q.apply_mask(mask);
        q.draw_format(mask);
        q.mask = mask;
        Some(q)
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Whether the module at column `x`, row `y` is dark (false outside,
    /// the quiet zone).
    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.dark[y] >> x & 1 != 0
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        if dark {
            self.dark[y] |= 1 << x;
        } else {
            self.dark[y] &= !(1 << x);
        }
    }

    fn set_function(&mut self, x: usize, y: usize, dark: bool) {
        self.set(x, y, dark);
        self.function[y] |= 1 << x;
    }

    fn is_function(&self, x: usize, y: usize) -> bool {
        self.function[y] >> x & 1 != 0
    }

    fn draw_function_patterns(&mut self) {
        let size = self.size;
        // Timing patterns, then the finders over their ends.
        for i in 0..size {
            self.set_function(6, i, i % 2 == 0);
            self.set_function(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (size - 4, 3), (3, size - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if (0..size as i32).contains(&x) && (0..size as i32).contains(&y) {
                        let d = dx.abs().max(dy.abs());
                        self.set_function(x as usize, y as usize, d != 2 && d != 4);
                    }
                }
            }
        }
        let mut at = [0usize; 7];
        let align = alignment_positions(self.version, &mut at);
        let last = align.len().wrapping_sub(1);
        for (i, &cx) in align.iter().enumerate() {
            for (j, &cy) in align.iter().enumerate() {
                // Not over the finders.
                if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        self.set_function((cx as i32 + dx) as usize, (cy as i32 + dy) as usize, dx.abs().max(dy.abs()) != 1);
                    }
                }
            }
        }
        // Reserved for the format information (drawn after the mask).
        self.draw_format(0);
        if self.version >= 7 {
            let mut rem = self.version as u32;
            for _ in 0..12 {
                rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
            }
            let bits = (self.version as u32) << 12 | rem;
            for i in 0..18 {
                let dark = bits >> i & 1 != 0;
                let (a, b) = (size - 11 + i % 3, i / 3);
                self.set_function(a, b, dark);
                self.set_function(b, a, dark);
            }
        }
    }

    fn draw_format(&mut self, mask: u8) {
        let data = self.ecl.bits() << 3 | mask as u32;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = (data << 10 | rem) ^ 0x5412;
        let bit = |i: usize| bits >> i & 1 != 0;
        let size = self.size;
        for i in 0..6 {
            self.set_function(8, i, bit(i));
        }
        self.set_function(8, 7, bit(6));
        self.set_function(8, 8, bit(7));
        self.set_function(7, 8, bit(8));
        for i in 9..15 {
            self.set_function(14 - i, 8, bit(i));
        }
        for i in 0..8 {
            self.set_function(size - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.set_function(8, size - 15 + i, bit(i));
        }
        // The dark module.
        self.set_function(8, size - 8, true);
    }

    /// The codewords' bits up and down the column pairs from the right.
    fn draw_codewords(&mut self, codewords: &[u8]) {
        let size = self.size;
        let mut i = 0;
        let mut right = size as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            let upward = (right + 1) & 2 == 0;
            for vert in 0..size {
                let y = if upward { size - 1 - vert } else { vert };
                for j in 0..2 {
                    let x = right as usize - j;
                    if !self.is_function(x, y) && i < codewords.len() * 8 {
                        self.set(x, y, codewords[i >> 3] >> (7 - (i & 7)) & 1 != 0);
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let flip = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if flip && !self.is_function(x, y) {
                    self.dark[y] ^= 1 << x;
                }
            }
        }
    }

    /// The standard's penalty (rules N1 to N4); outside the symbol counts
    /// as light for the finder-like rule.
    fn penalty(&self) -> u32 {
        let n = self.size;
        let mut score = 0;
        for along_rows in [true, false] {
            let at = |i: usize, j: usize| if along_rows { self.get(j, i) } else { self.get(i, j) };
            for i in 0..n {
                // N1: runs of 5 or more.
                let mut run = 1;
                for j in 1..=n {
                    if j < n && at(i, j) == at(i, j - 1) {
                        run += 1;
                    } else {
                        if run >= 5 {
                            score += 3 + (run - 5);
                        }
                        run = 1;
                    }
                }
                // N3: 1:1:3:1:1 with 4 light on one side.
                const PAT: [bool; 7] = [true, false, true, true, true, false, true];
                for j in 0..=n - 7 {
                    if (0..7).all(|k| at(i, j + k) == PAT[k]) {
                        let light = |from: i32| (from..from + 4).all(|k| k < 0 || k >= n as i32 || !at(i, k as usize));
                        if light(j as i32 - 4) {
                            score += 40;
                        }
                        if light(j as i32 + 7) {
                            score += 40;
                        }
                    }
                }
            }
        }
        // N2: 2 x 2 blocks of one colour.
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = self.get(x, y);
                if c == self.get(x + 1, y) && c == self.get(x, y + 1) && c == self.get(x + 1, y + 1) {
                    score += 3;
                }
            }
        }
        // N4: the share of dark modules away from half.
        let dark: u32 = self.dark[..n].iter().map(|r| r.count_ones()).sum();
        let total = (n * n) as u32;
        let k = (dark * 20).abs_diff(total * 10).div_ceil(total) - 1;
        score + k * 10
    }
}

/// Modules for data and ECC (all but the function patterns and the
/// format and version information).
fn raw_modules(version: usize) -> usize {
    let mut r = (16 * version + 128) * version + 64;
    if version >= 2 {
        let align = version / 7 + 2;
        r -= (25 * align - 10) * align - 55;
        if version >= 7 {
            r -= 36;
        }
    }
    r
}

fn data_codewords(version: usize, ecl: Ecl) -> usize {
    raw_modules(version) / 8 - ECC_PER_BLOCK[ecl.index()][version] as usize * BLOCKS[ecl.index()][version] as usize
}

/// Bytes that fit `version` at `ecl` in byte mode (mode and count before
/// them: 8-bit count up to version 9, 16 from 10).
pub fn capacity(version: usize, ecl: Ecl) -> usize {
    let count_bits = if version < 10 { 8 } else { 16 };
    (data_codewords(version, ecl) * 8 - 4 - count_bits) / 8
}

fn alignment_positions(version: usize, out: &mut [usize; 7]) -> &[usize] {
    if version == 1 {
        return &[];
    }
    let n = version / 7 + 2;
    let step = (version * 8 + n * 3 + 5) / (n * 4 - 4) * 2;
    out[0] = 6;
    let mut pos = 4 * version + 10;
    for i in (1..n).rev() {
        out[i] = pos;
        pos -= step;
    }
    &out[..n]
}

/// The data codewords (mode, count, bytes, terminator, padding), split
/// into blocks with their ECC and interleaved; the number written.
fn codewords_of(data: &[u8], version: usize, ecl: Ecl, out: &mut [u8; MAX_CODEWORDS]) -> usize {
    let n_data = data_codewords(version, ecl);
    let mut d = [0u8; MAX_CODEWORDS];
    let mut bits = 0usize;
    let put = |v: u32, n: usize, d: &mut [u8; MAX_CODEWORDS], bits: &mut usize| {
        for i in (0..n).rev() {
            if v >> i & 1 != 0 {
                d[*bits >> 3] |= 0x80 >> (*bits & 7);
            }
            *bits += 1;
        }
    };
    put(0b0100, 4, &mut d, &mut bits);
    put(data.len() as u32, if version < 10 { 8 } else { 16 }, &mut d, &mut bits);
    for &b in data {
        put(b as u32, 8, &mut d, &mut bits);
    }
    // Terminator, then whole bytes, then the pad bytes.
    let capacity = n_data * 8;
    put(0, 4.min(capacity - bits), &mut d, &mut bits);
    let mut len = bits.div_ceil(8);
    for pad in [0xEC, 0x11].into_iter().cycle() {
        if len >= n_data {
            break;
        }
        d[len] = pad;
        len += 1;
    }

    let (eccn, blocks) = (ECC_PER_BLOCK[ecl.index()][version] as usize, BLOCKS[ecl.index()][version] as usize);
    let raw = raw_modules(version) / 8;
    let short = blocks - raw % blocks;
    let short_data = raw / blocks - eccn;
    let mut divisor = [0u8; MAX_ECC];
    rs_divisor(&mut divisor[..eccn]);
    let mut ecc = [[0u8; MAX_ECC]; MAX_BLOCKS];
    let mut starts = [0usize; MAX_BLOCKS + 1];
    for b in 0..blocks {
        let n = short_data + usize::from(b >= short);
        starts[b + 1] = starts[b] + n;
        rs_remainder(&d[starts[b]..starts[b + 1]], &divisor[..eccn], &mut ecc[b][..eccn]);
    }
    let mut k = 0;
    for i in 0..=short_data {
        for b in 0..blocks {
            if starts[b] + i < starts[b + 1] {
                out[k] = d[starts[b] + i];
                k += 1;
            }
        }
    }
    for i in 0..eccn {
        for e in &ecc[..blocks] {
            out[k] = e[i];
            k += 1;
        }
    }
    k
}

/// Product in GF(2^8) modulo x^8 + x^4 + x^3 + x^2 + 1.
fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y >> i) & 1) as u32 * x as u32;
    }
    z as u8
}

/// The Reed-Solomon generator of degree `out.len()`, its leading 1 left
/// out.
fn rs_divisor(out: &mut [u8]) {
    let n = out.len();
    out.fill(0);
    out[n - 1] = 1;
    let mut root = 1u8;
    for _ in 0..n {
        for j in 0..n {
            out[j] = gf_mul(out[j], root);
            if j + 1 < n {
                out[j] ^= out[j + 1];
            }
        }
        root = gf_mul(root, 2);
    }
}

fn rs_remainder(data: &[u8], divisor: &[u8], out: &mut [u8]) {
    let n = out.len();
    out.fill(0);
    for &b in data {
        let factor = b ^ out[0];
        out.copy_within(1.., 0);
        out[n - 1] = 0;
        for (o, &d) in out.iter_mut().zip(divisor) {
            *o ^= gf_mul(d, factor);
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vectors;
