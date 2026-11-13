// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The Tab5's 32 MiB AP Memory HEX PSRAM behind its own MSPI controllers
//! (doc/research/m4-psram-and-clock.md). Modelled at the level the kernel
//! uses it:
//! - direct commands through the ROM's `esp_rom_spi_cmd_config` /
//!   `esp_rom_spi_cmd_start` on MSPI3: mode register read/write and sync
//!   data read/write (timing is always right, so every tuning point passes);
//! - the PSRAM MMU (index/content registers, 64 KiB pages) and the cached
//!   window 0x4800_0000 with its uncached alias 0x8800_0000;
//! - the L1 cache incoherence for code loaded there (same model as L2MEM).

use crate::cache::{self, Cache};

pub const WINDOW: u32 = 0x4800_0000;
pub const UNCACHED: u32 = 0x8800_0000;
pub const WINDOW_SIZE: u32 = 0x0400_0000;
pub const SIZE: usize = 32 << 20;
/// PSRAM MMU registers (spi_mem_s_reg.h; doc/02 §3).
pub const MMU_ITEM_CONTENT: u32 = 0x5008_E37C;
pub const MMU_ITEM_INDEX: u32 = 0x5008_E380;
const MMU_VALID: u32 = 1 << 11;

// AP HEX commands (esp_psram_impl_ap_hex.c:21-26).
const SYNC_READ: u16 = 0x0000;
const SYNC_WRITE: u16 = 0x8080;
const REG_READ: u16 = 0x4040;
const REG_WRITE: u16 = 0xC0C0;

/// A command configured by `esp_rom_spi_cmd_config`, run by `_cmd_start`.
#[derive(Clone, Debug, Default)]
pub struct SpiCmd {
    pub cmd: u16,
    pub addr: u32,
    pub tx: Vec<u8>,
    pub rx_bits: u32,
}

pub struct Psram {
    pub mem: Vec<u8>,
    pub mmu: Vec<u32>,
    pub mmu_index: u32,
    pub cache: Cache,
    /// Mode registers MR0-MR15. MR1 vendor 0x0D (AP), MR2 density 7
    /// (256 Mbit) and KGD 6, as read on the Tab5.
    pub mr: [u8; 16],
    pub cmd: Option<SpiCmd>,
}

impl Psram {
    pub fn new(cache_mode: cache::Mode) -> Psram {
        let mut mr = [0u8; 16];
        mr[1] = 0x0D;
        mr[2] = 7 | (6 << 5);
        Psram {
            mem: vec![0; SIZE],
            mmu: vec![0; 1024],
            mmu_index: 0,
            cache: Cache::new(cache_mode, WINDOW, WINDOW_SIZE),
            mr,
            cmd: None,
        }
    }

    /// Physical offset of a window or alias address, if its page is mapped.
    pub fn translate(&self, addr: u32, size: u32) -> Option<usize> {
        let base = if (WINDOW..WINDOW + WINDOW_SIZE).contains(&addr) { WINDOW } else { UNCACHED };
        let off = addr.checked_sub(base)?;
        if off >= WINDOW_SIZE || (off & 0xFFFF) + size > 0x1_0000 {
            return None;
        }
        let entry = self.mmu[(off >> 16) as usize];
        if entry & MMU_VALID == 0 {
            return None;
        }
        let p = (((entry & 0x3FF) << 16) | (off & 0xFFFF)) as usize;
        (p + size as usize <= self.mem.len()).then_some(p)
    }

    pub fn in_window(addr: u32) -> bool {
        (WINDOW..WINDOW + WINDOW_SIZE).contains(&addr)
    }

    pub fn in_uncached(addr: u32) -> bool {
        (UNCACHED..UNCACHED + WINDOW_SIZE).contains(&addr)
    }

    /// Runs the configured command; returns the bytes read (`rx_len`).
    pub fn start(&mut self, rx_len: usize) -> Vec<u8> {
        let Some(c) = self.cmd.clone() else { return vec![0; rx_len] };
        let a = c.addr as usize;
        match c.cmd {
            REG_READ => (0..rx_len).map(|i| self.mr.get(a + i).copied().unwrap_or(0)).collect(),
            REG_WRITE => {
                for (i, &b) in c.tx.iter().enumerate() {
                    if let Some(r) = self.mr.get_mut(a + i) {
                        *r = b;
                    }
                }
                vec![0; rx_len]
            }
            SYNC_WRITE => {
                for (i, &b) in c.tx.iter().enumerate() {
                    if let Some(m) = self.mem.get_mut(a + i) {
                        *m = b;
                    }
                }
                vec![0; rx_len]
            }
            SYNC_READ => (0..rx_len).map(|i| self.mem.get(a + i).copied().unwrap_or(0)).collect(),
            _ => vec![0; rx_len],
        }
    }
}
