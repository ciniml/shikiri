// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The PPA's blending engine and the 2D-DMA, as the kernel uses them
//! (doc/44, doc/research/ppa-dma2d-registers.md): an operation runs at
//! once, when it starts, on memory itself (the DMA bypasses the caches).
//! Fill and blend start when the PPA's mode is taken (`BLEND_TRANS_MODE`
//! update), a copy (M2M) when its RX channel starts. Only RX channel 1
//! (the kernel's output channel) ends operations and interrupts.

use crate::bus::Bus;

pub const DMA2D: u32 = 0x5008_8000;
pub const PPA: u32 = 0x5008_7000;
const fn tx(n: u32) -> u32 {
    DMA2D + 0x100 * n
}
const RX1: u32 = DMA2D + 0x600;
pub const RX1_INT_RAW: u32 = RX1 + 0x04;
pub const RX1_INT_ENA: u32 = RX1 + 0x08;
const INT_CLR: u32 = 0x10;
const LINK_CONF: u32 = 0x1C;
const LINK_ADDR: u32 = 0x20;
const STATE: u32 = 0x24;
const OUT_PERI_SEL: u32 = 0x38;
const IN_PERI_SEL: u32 = 0x3C;
const IN_LINK_START: u32 = 1 << 22;
const BLEND_TRANS_MODE: u32 = PPA + 0x34;
const BLEND_RGB: u32 = PPA + 0x48;
const BLEND_FIX_PIXEL: u32 = PPA + 0x4C;
const TRANS_EN: u32 = 1 << 0;
const TRANS_FILL: u32 = 1 << 2;
const TRANS_UPDATE: u32 = 1 << 3;
/// in_done, in_suc_eof.
const DONE: u32 = 0b11;
const PERI_PPA_BG: u32 = 2;
const PERI_PPA_FG: u32 = 3;
const PERI_M2M_RX: u32 = 3;
/// soc/interrupts.h: ETS_DMA2D_IN_CH1_INTR_SOURCE.
pub const SRC_DMA2D_IN_CH1: usize = 105;

pub fn contains(addr: u32) -> bool {
    (PPA..PPA + 0x1000).contains(&addr) || (DMA2D..DMA2D + 0xA30).contains(&addr)
}

/// A channel's STATE: reset_avail set (TX bit 24, RX bit 23), the rest as
/// written. The FSMs are always idle.
pub fn read_state(addr: u32, stored: u32) -> Option<u32> {
    let off = addr.checked_sub(DMA2D)?;
    (off % 0x100 == STATE && off < 0xA00).then(|| stored | if off < 0x500 { 1 << 24 } else { 1 << 23 })
}

/// After the CPU wrote `v` to `addr` (already stored).
pub fn on_write(bus: &mut Bus, addr: u32, v: u32) {
    let peek = |bus: &Bus, a: u32| bus.periph.peek(a);
    if addr == RX1 + INT_CLR {
        let raw = peek(bus, RX1_INT_RAW);
        bus.periph.poke(RX1_INT_RAW, raw & !v);
    } else if addr == RX1 + LINK_CONF && v & IN_LINK_START != 0 && peek(bus, RX1 + IN_PERI_SEL) & 7 == PERI_M2M_RX {
        // The two lists in step, block by block (the kernel cuts copies
        // over themselves into pieces, doc/44).
        let (mut s, mut d) = (peek(bus, tx(1) + LINK_ADDR), peek(bus, RX1 + LINK_ADDR));
        while s != 0 && d != 0 {
            let (src, dst) = (desc(bus, s), desc(bus, d));
            for j in 0..dst.vb {
                for i in 0..dst.hb {
                    let px = bus.dma_read(src.at(i, j), 2);
                    bus.dma_write(dst.at(i, j), 2, px);
                }
            }
            (s, d) = (src.next, dst.next);
        }
        end(bus);
    } else if addr == BLEND_TRANS_MODE && v & TRANS_UPDATE != 0 && v & TRANS_EN != 0 {
        let out = peek(bus, RX1 + LINK_ADDR);
        let out = desc(bus, out);
        if v & TRANS_FILL != 0 {
            let argb = peek(bus, BLEND_FIX_PIXEL);
            let px = (argb >> 8 & 0xF800) | (argb >> 5 & 0x07E0) | (argb >> 3 & 0x1F);
            for j in 0..out.vb {
                for i in 0..out.hb {
                    bus.dma_write(out.at(i, j), 2, px);
                }
            }
        } else {
            let by_peri = |bus: &Bus, peri: u32| (0..3).find(|&n| peek(bus, tx(n) + OUT_PERI_SEL) & 7 == peri).map(|n| peek(bus, tx(n) + LINK_ADDR));
            let (Some(bg), Some(fg)) = (by_peri(bus, PERI_PPA_BG), by_peri(bus, PERI_PPA_FG)) else { return };
            let rgb = peek(bus, BLEND_RGB);
            let (bg, fg) = (desc(bus, bg), desc(bus, fg));
            let mix = |b: u32, f: u32, a: u32| (b * (255 - a) + f * a + 127) / 255;
            for j in 0..out.vb {
                for i in 0..out.hb {
                    let b = bus.dma_read(bg.at(i, j), 2);
                    let a = bus.dma_read(fg.at(i, j), 1);
                    let r = mix((b >> 11) << 3, rgb >> 16 & 0xFF, a) >> 3;
                    let g = mix((b >> 5 & 0x3F) << 2, rgb >> 8 & 0xFF, a) >> 2;
                    let bl = mix((b & 0x1F) << 3, rgb & 0xFF, a) >> 3;
                    bus.dma_write(out.at(i, j), 2, r << 11 | g << 5 | bl);
                }
            }
        }
        end(bus);
    }
}

fn end(bus: &mut Bus) {
    let raw = bus.periph.peek(RX1_INT_RAW);
    bus.periph.poke(RX1_INT_RAW, raw | DONE);
}

/// A 2D descriptor: the block's size, the image's width, its place, the
/// image's address and the bytes per pixel.
struct Desc {
    hb: u32,
    vb: u32,
    ha: u32,
    x: u32,
    y: u32,
    buffer: u32,
    bpp: u32,
    next: u32,
}

impl Desc {
    fn at(&self, i: u32, j: u32) -> u32 {
        self.buffer + ((self.y + j) * self.ha + self.x + i) * self.bpp
    }
}

fn desc(bus: &mut Bus, addr: u32) -> Desc {
    let w = |bus: &mut Bus, k: u32| bus.read(addr + 4 * k, 4).unwrap_or(0);
    let (w0, w1, w2, buffer, next) = (w(bus, 0), w(bus, 1), w(bus, 2), w(bus, 3), w(bus, 4));
    let bpp = match w1 >> 28 {
        1 => 1,
        3 => 2,
        5 => 4,
        _ => 2,
    };
    Desc { vb: w0 & 0x3FFF, hb: w0 >> 14 & 0x3FFF, ha: w1 >> 14 & 0x3FFF, y: w2 & 0x3FFF, x: w2 >> 14 & 0x3FFF, buffer, bpp, next }
}
