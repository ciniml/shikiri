// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Screen capture on the console line (doc/54). The kernel sends the
//! framebuffer as COBS frames next to the log records, each starting with
//! `FRAME` (no log record starts with it: its level is at most 4; the host
//! link's packets start with 0xFF):
//!
//! - `FRAME | START | u32 id | u16 width | u16 height | u8 orientation |
//!   u8 format`: the framebuffer as the panel has it (720 x 1280 on the
//!   Tab5) and the quarter turns to the user's view (as `touch`, doc/17
//!   §11).
//! - `FRAME | DATA | u32 id | u32 first pixel | runs`: pixels in order.
//! - `FRAME | END | u32 id | u32 pixels | u32 CRC-32 of the raw pixels`.
//!
//! Runs: a byte `n`; with bit 7 set, the next pixel `(n & 0x7F) + 1` times,
//! else `n + 1` pixels as they are. Pixels are RGB565, little-endian.

pub const FRAME: u8 = 0xFE;
pub const START: u8 = 1;
pub const DATA: u8 = 2;
pub const END: u8 = 3;
/// `format`: RGB565, little-endian.
pub const RGB565: u8 = 1;
/// Longest run of either kind.
pub const RUN_MAX: usize = 128;
/// Pixels per DATA frame.
pub const CHUNK: usize = 512;
/// The largest DATA frame (before COBS): header, every pixel literal.
pub const DATA_MAX: usize = 1 + 1 + 4 + 4 + CHUNK * 2 + CHUNK.div_ceil(RUN_MAX);

/// Encodes `px` as runs into `out`; returns the length (`out` must hold
/// `px.len() * 2 + px.len().div_ceil(RUN_MAX)` bytes).
pub fn encode_runs(px: &[u16], out: &mut [u8]) -> usize {
    let mut o = 0;
    let mut i = 0;
    while i < px.len() {
        let mut same = 1;
        while i + same < px.len() && same < RUN_MAX && px[i + same] == px[i] {
            same += 1;
        }
        if same >= 2 {
            out[o] = 0x80 | (same - 1) as u8;
            out[o + 1..o + 3].copy_from_slice(&px[i].to_le_bytes());
            o += 3;
            i += same;
            continue;
        }
        // Literal up to the next pair of equal pixels.
        let mut n = 1;
        while i + n < px.len() && n < RUN_MAX && !(i + n + 1 < px.len() && px[i + n] == px[i + n + 1]) {
            n += 1;
        }
        out[o] = (n - 1) as u8;
        o += 1;
        for p in &px[i..i + n] {
            out[o..o + 2].copy_from_slice(&p.to_le_bytes());
            o += 2;
        }
        i += n;
    }
    o
}

/// Decodes runs, calling `put` for each pixel; `None` if they are cut off.
pub fn decode_runs(mut b: &[u8], put: &mut dyn FnMut(u16)) -> Option<usize> {
    let mut n = 0;
    while let Some((&h, rest)) = b.split_first() {
        let k = (h & 0x7F) as usize + 1;
        if h & 0x80 != 0 {
            let p = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?);
            (0..k).for_each(|_| put(p));
            b = &rest[2..];
        } else {
            for c in rest.get(..2 * k)?.chunks_exact(2) {
                put(u16::from_le_bytes([c[0], c[1]]));
            }
            b = &rest[2 * k..];
        }
        n += k;
    }
    Some(n)
}

/// CRC-32 (IEEE, as zlib), continued from `crc` (0 to start).
pub fn crc32(crc: u32, bytes: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in bytes {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

static CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn runs_round_trip() {
        let cases: [&[u16]; 5] = [
            &[],
            &[7],
            &[1, 2, 3, 3, 3, 4, 5, 5],
            &[9; 300],
            &[1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 3, 3],
        ];
        for px in cases {
            let mut enc = vec![0u8; px.len() * 2 + px.len().div_ceil(RUN_MAX) + 1];
            let n = encode_runs(px, &mut enc);
            let mut dec = Vec::new();
            assert_eq!(decode_runs(&enc[..n], &mut |p| dec.push(p)), Some(px.len()));
            assert_eq!(dec, px);
        }
        let long: Vec<u16> = (0..CHUNK as u16).collect();
        let mut enc = vec![0u8; DATA_MAX];
        assert!(encode_runs(&long, &mut enc) <= DATA_MAX - 10);
    }

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xCBF4_3926);
    }
}
