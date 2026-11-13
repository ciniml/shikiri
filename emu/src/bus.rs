// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Address map of the ESP32-P4 rev v1.x as seen by an HP core (doc/02 §1).
//! Only what the kernel touches is backed; everything else is a bus error,
//! which the CPU reports as an access fault. Memory accepts misaligned
//! accesses; peripheral registers do not (judgement).

use crate::cache::{self, Cache};
use crate::periph::{Peripherals, SYSTIMER_HZ};
use crate::psram::Psram;

/// The flash window. The first `FLASH_SIZE` bytes hold the kernel as the
/// bootloader maps it (loaded from the ELF); pages the kernel maps through
/// the MMU registers read from `phys_flash`.
pub const FLASH_BASE: u32 = 0x4000_0000;
pub const FLASH_WINDOW_SIZE: u32 = 0x0400_0000;
/// The physical flash chip (16 MiB on the Tab5).
pub const PHYS_FLASH_SIZE: usize = 16 << 20;
/// Flash MMU (spi_mem_c_reg.h:2630,2642; ext_mem_defs.h:57): 1024 entries
/// of 64 KiB; content = (paddr >> 16) | VALID.
pub const MMU_ITEM_CONTENT: u32 = 0x5008_C37C;
pub const MMU_ITEM_INDEX: u32 = 0x5008_C380;
const MMU_VALID: u32 = 1 << 12;
/// The ota_0 partition is 2 MiB (boot/partitions.csv).
pub const FLASH_SIZE: u32 = 0x0020_0000;
pub const L2MEM_BASE: u32 = 0x4FF0_0000;
/// 768 KiB of L2MEM, of which the top 128 KiB is the L2 cache on the Tab5's
/// configuration (doc/02 §1.1); modelled as plain memory.
pub const L2MEM_SIZE: u32 = 0x000C_0000;
/// L2MEM through the non-cacheable alias (SOC_NON_CACHEABLE_OFFSET,
/// soc/esp32p4/include/soc/ext_mem_defs.h:68).
pub const L2MEM_UNCACHED_BASE: u32 = L2MEM_BASE + 0x4000_0000;
pub const TCM_BASE: u32 = 0x3010_0000;
pub const TCM_SIZE: u32 = 0x2000;
/// LP SRAM (soc/esp32p4/include/soc/soc.h:176-177): uncached, in the
/// peripheral address space; the kernel keeps its log ring here (doc/12).
pub const LP_SRAM_BASE: u32 = 0x5010_8000;
pub const LP_SRAM_SIZE: u32 = 0x8000;
pub const ROM_BASE: u32 = 0x4FC0_0000;
pub const ROM_SIZE: u32 = 0x0002_0000;
const PERIPH: std::ops::Range<u32> = 0x5000_0000..0x5020_0000;
/// The cache controller's registers (cache_reg.h, DR_REG_CACHE_BASE): kept
/// as plain registers; autoload is not modelled (doc/43 §4).
const CACHE_CTRL: std::ops::Range<u32> = 0x3FF1_0000..0x3FF1_1000;
const RNG_DATA: u32 = 0x5011_01A4;
const CLIC: std::ops::Range<u32> = 0x2080_0000..0x2080_2000;

/// Flash program page and erase sector.
pub const FLASH_PAGE: u32 = 256;
pub const FLASH_SECTOR: u32 = 4096;
/// Where the ROM keeps its pointer to the flash chip parameters on rev v1.x
/// (esp32p4.rom.ld:169), and where the emulator puts the parameters the
/// bootloader would have set (esp_rom_spiflash_config_param): JEDEC id and
/// size. The id is a made-up placeholder.
pub const ROM_SPIFLASH_LEGACY_DATA: u32 = 0x4FF3_FFE8;
const EMU_FLASH_CHIP: u32 = 0x4FF3_FFC0;
pub const EMU_FLASH_DEVICE_ID: u32 = 0x00E5_E018;

pub struct Bus {
    pub cycles: u64,
    pub cpu_hz: u64,
    pub flash: Vec<u8>,
    pub phys_flash: Vec<u8>,
    mmu: Vec<u32>,
    mmu_index: u32,
    /// Set by the ROM's Cache_Suspend_L2_Cache. While set, the flash window
    /// cannot be read or fetched from: the real core would hang.
    pub l2_suspended: bool,
    /// L2 line size in bytes, as `Cache_Set_L2_Cache_Mode` set it.
    pub l2_line: u32,
    /// The RNG register's generator (LP_SYSTEM_REG_RNG_DATA_REG).
    rng: u64,
    /// `--rng-stuck`: the register returns the same word (a failed
    /// generator, for the kernel's health tests; doc/47 §6).
    pub rng_stuck: bool,
    /// First flash-window access made while L2 was suspended.
    pub flash_violation: Option<u32>,
    /// 256-byte pages of `phys_flash` changed by the ROM erase/write
    /// functions and not yet invalidated in the L1 D-cache / the L2 cache.
    /// Reading one through the window would return stale data on the chip.
    stale_l1d: Vec<bool>,
    stale_l2: Vec<bool>,
    /// First window read of a stale page.
    pub stale_read: Option<u32>,
    pub l2mem: Vec<u8>,
    tcm: Vec<u8>,
    pub lp_sram: Vec<u8>,
    pub cache: Cache,
    pub psram: Psram,
    pub periph: Peripherals,
    /// The SDMMC host and the microSD card (doc/64).
    pub sd: crate::sdmmc::Sdmmc,
    /// The AHB-GDMA and I2S0 (doc/70).
    pub audio: crate::audio::Audio,
}

/// HP_SYS_CLKRST_ANA_PLL_CTRL0: MSPI_CAL_END (bit 8) reads as done, so the
/// kernel's MPLL calibration finishes (hp_sys_clkrst_reg.h:3050).
const ANA_PLL_CTRL0: u32 = 0x500E_60BC;
const MSPI_CAL_END: u32 = 1 << 8;

/// The flash MMU as the bootloader leaves it: entries 0-31 map the kernel
/// image from ota_0 (flash 0x1_0000; doc/16 §3.1). The emulator itself
/// serves the kernel from the ELF (`flash`), so this only answers the
/// kernel when it asks which slot it runs from.
fn kernel_mmu() -> Vec<u32> {
    let mut mmu = vec![0; 1024];
    for (i, e) in mmu.iter_mut().take(32).enumerate() {
        *e = (1 + i as u32) | MMU_VALID;
    }
    mmu
}

fn rd(mem: &[u8], off: u32, size: u32) -> u32 {
    let o = off as usize;
    match size {
        1 => mem[o] as u32,
        2 => u16::from_le_bytes([mem[o], mem[o + 1]]) as u32,
        _ => u32::from_le_bytes(mem[o..o + 4].try_into().unwrap()),
    }
}

fn wr(mem: &mut [u8], off: u32, size: u32, v: u32) {
    let o = off as usize;
    mem[o..o + size as usize].copy_from_slice(&v.to_le_bytes()[..size as usize]);
}

fn within(addr: u32, size: u32, base: u32, len: u32) -> Option<u32> {
    let off = addr.checked_sub(base)?;
    (off.checked_add(size)? <= len).then_some(off)
}

impl Bus {
    pub fn new(cpu_hz: u64, cache_mode: cache::Mode, l2mem_fill_seed: u32) -> Bus {
        // RAM holds garbage at power-on; fill it with a deterministic
        // pseudo-random pattern so reliance on zeroed memory shows up.
        let mut l2mem = vec![0u8; L2MEM_SIZE as usize];
        let mut lp_sram = vec![0u8; LP_SRAM_SIZE as usize];
        let mut x = l2mem_fill_seed | 1;
        for b in l2mem.iter_mut().chain(lp_sram.iter_mut()) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        Bus {
            cycles: 0,
            cpu_hz,
            flash: vec![0xFF; FLASH_SIZE as usize],
            phys_flash: vec![0xFF; PHYS_FLASH_SIZE],
            mmu: kernel_mmu(),
            mmu_index: 0,
            l2_suspended: false,
            l2_line: 64,
            rng: 0x2545_F491_4F6C_DD1D,
            rng_stuck: false,
            flash_violation: None,
            stale_l1d: vec![false; PHYS_FLASH_SIZE / FLASH_PAGE as usize],
            stale_l2: vec![false; PHYS_FLASH_SIZE / FLASH_PAGE as usize],
            stale_read: None,
            l2mem,
            tcm: vec![0; TCM_SIZE as usize],
            lp_sram,
            cache: Cache::new(cache_mode, L2MEM_BASE, L2MEM_SIZE),
            psram: Psram::new(cache_mode),
            periph: Peripherals::default(),
            sd: Default::default(),
            audio: Default::default(),
        }
        .with_flash_chip()
    }

    fn with_flash_chip(mut self) -> Bus {
        for (addr, v) in [
            (ROM_SPIFLASH_LEGACY_DATA, EMU_FLASH_CHIP),
            (EMU_FLASH_CHIP, EMU_FLASH_DEVICE_ID),
            (EMU_FLASH_CHIP + 4, PHYS_FLASH_SIZE as u32),
            (EMU_FLASH_CHIP + 8, 0x1_0000),
            (EMU_FLASH_CHIP + 12, FLASH_SECTOR),
            (EMU_FLASH_CHIP + 16, FLASH_PAGE),
        ] {
            wr(&mut self.l2mem, addr - L2MEM_BASE, 4, v);
        }
        self
    }

    /// `esp_rom_spiflash_erase_sector`: the 4 KiB sector becomes 0xFF.
    pub fn flash_erase_sector(&mut self, sector: u32) -> u32 {
        let o = sector as usize * FLASH_SECTOR as usize;
        if o >= PHYS_FLASH_SIZE {
            return 1;
        }
        self.phys_flash[o..o + FLASH_SECTOR as usize].fill(0xFF);
        self.mark_stale(o as u32, FLASH_SECTOR);
        0
    }

    /// `esp_rom_spiflash_write`: NOR programming only clears bits, so data
    /// written over unerased flash comes out ANDed with what was there.
    /// `src` is read through the bus, so a source in the flash window while
    /// L2 is suspended is a violation like any other access.
    pub fn flash_write(&mut self, dest: u32, src: u32, len: u32) -> u32 {
        if dest % 4 != 0 || len % 4 != 0 || within(dest, len, 0, PHYS_FLASH_SIZE as u32).is_none() {
            return 1;
        }
        for i in 0..len {
            let Some(b) = self.read(src + i, 1) else { return 1 };
            self.phys_flash[(dest + i) as usize] &= b as u8;
        }
        self.mark_stale(dest, len);
        0
    }

    fn mark_stale(&mut self, paddr: u32, len: u32) {
        for p in paddr / FLASH_PAGE..(paddr + len).div_ceil(FLASH_PAGE) {
            self.stale_l1d[p as usize] = true;
            self.stale_l2[p as usize] = true;
        }
    }

    /// Cache_Invalidate_Addr over the flash window: the pages of
    /// `[addr, addr + len)` that are mapped are fresh again in the caches
    /// named by `map` (bit 4 = L1 D, bit 5 = L2).
    pub fn invalidate_window(&mut self, map: u32, addr: u32, len: u32) {
        let end = addr.saturating_add(len).min(FLASH_BASE + FLASH_WINDOW_SIZE);
        let mut a = addr.max(FLASH_BASE) & !(FLASH_PAGE - 1);
        while a < end {
            let entry = self.mmu[((a - FLASH_BASE) >> 16) as usize];
            if entry & MMU_VALID != 0 {
                let p = ((((entry & 0x7FF) << 16) | (a & 0xFFFF)) / FLASH_PAGE) as usize;
                if map & (1 << 4) != 0 {
                    self.stale_l1d[p] = false;
                }
                if map & (1 << 5) != 0 {
                    self.stale_l2[p] = false;
                }
            }
            a += FLASH_PAGE;
        }
    }

    /// A system reset as the RWDT does it: peripherals and the cache return
    /// to their reset state and time restarts. L2MEM keeps its contents (the
    /// bootloader reloads the image into it); LP SRAM keeps them only if the
    /// RTC domain is not reset. The console output carries on.
    pub fn reset_system(&mut self, keep_lp_sram: bool, cause: u32, seed: u32) {
        let old = std::mem::take(&mut self.periph);
        self.periph.usj = old.usj;
        self.periph.warn_unmodelled = old.warn_unmodelled;
        self.periph.reset_cause = cause;
        self.cycles = 0;
        self.cache = Cache::new(self.cache.mode, L2MEM_BASE, L2MEM_SIZE);
        // The bootloader clears the MMU and maps only the kernel again; the
        // PSRAM keeps its contents but its MMU is cleared too.
        self.mmu = kernel_mmu();
        self.psram.mmu.fill(0);
        self.psram.cache = Cache::new(self.cache.mode, crate::psram::WINDOW, crate::psram::WINDOW_SIZE);
        self.l2_suspended = false;
        self.l2_line = 64;
        if !keep_lp_sram {
            let mut x = seed | 1;
            for b in self.lp_sram.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
        }
    }

    /// SYSTIMER ticks since reset.
    pub fn ticks(&self) -> u64 {
        // In 64 bits while the product fits (some hours of emulated time):
        // the 128-bit division cost much, being done every instruction.
        match self.cycles.checked_mul(SYSTIMER_HZ) {
            Some(p) => p / self.cpu_hz,
            None => (self.cycles as u128 * SYSTIMER_HZ as u128 / self.cpu_hz as u128) as u64,
        }
    }

    /// CPU cycles at which the SYSTIMER reaches `ticks`.
    pub fn cycles_for_ticks(&self, ticks: u64) -> u64 {
        (ticks as u128 * self.cpu_hz as u128).div_ceil(SYSTIMER_HZ as u128) as u64
    }

    /// Loads an ELF segment as the bootloader would (no cache effects).
    pub fn load(&mut self, addr: u32, data: &[u8]) -> Result<(), String> {
        let len = data.len() as u32;
        let (mem, off) = if let Some(o) = within(addr, len, FLASH_BASE, FLASH_SIZE) {
            (&mut self.flash, o)
        } else if let Some(o) = within(addr, len, L2MEM_BASE, L2MEM_SIZE) {
            (&mut self.l2mem, o)
        } else if let Some(o) = within(addr, len, TCM_BASE, TCM_SIZE) {
            (&mut self.tcm, o)
        } else {
            return Err(format!("segment at {addr:#010x} (+{len:#x}) is outside flash, L2MEM and TCM"));
        };
        mem[off as usize..(off + len) as usize].copy_from_slice(data);
        Ok(())
    }

    /// Reads `size` bytes of the flash window at `addr`: the kernel image,
    /// or a page the kernel mapped. Records a violation if L2 is suspended.
    fn window_read(&mut self, addr: u32, size: u32) -> Option<u32> {
        within(addr, size, FLASH_BASE, FLASH_WINDOW_SIZE)?;
        if self.l2_suspended {
            self.flash_violation.get_or_insert(addr);
        }
        if let Some(o) = within(addr, size, FLASH_BASE, FLASH_SIZE) {
            return Some(rd(&self.flash, o, size));
        }
        let entry = self.mmu[((addr - FLASH_BASE) >> 16) as usize];
        if entry & MMU_VALID == 0 || (addr & 0xFFFF) + size > 0x1_0000 {
            return None;
        }
        let paddr = ((entry & 0x7FF) << 16) | (addr & 0xFFFF);
        let o = within(paddr, size, 0, PHYS_FLASH_SIZE as u32)?;
        let p = (paddr / FLASH_PAGE) as usize;
        if self.stale_l1d[p] || self.stale_l2[p] {
            self.stale_read.get_or_insert(addr);
        }
        Some(rd(&self.phys_flash, o, size))
    }

    /// Writes an image into the physical flash (`--flash FILE@OFFSET`).
    pub fn load_phys_flash(&mut self, offset: u32, data: &[u8]) -> Result<(), String> {
        let o = within(offset, data.len() as u32, 0, PHYS_FLASH_SIZE as u32)
            .ok_or(format!("{} bytes at flash offset {offset:#x} do not fit in 16 MiB", data.len()))?;
        self.phys_flash[o as usize..o as usize + data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Data-side read. `None` is a bus error.
    pub fn read(&mut self, addr: u32, size: u32) -> Option<u32> {
        if let Some(o) = within(addr, size, L2MEM_BASE, L2MEM_SIZE) {
            return Some(rd(&self.l2mem, o, size));
        }
        if let Some(o) = within(addr, size, L2MEM_UNCACHED_BASE, L2MEM_SIZE) {
            return Some(rd(&self.l2mem, o, size));
        }
        if within(addr, size, FLASH_BASE, FLASH_WINDOW_SIZE).is_some() {
            return self.window_read(addr, size);
        }
        if Psram::in_window(addr) || Psram::in_uncached(addr) {
            if self.l2_suspended && Psram::in_window(addr) {
                self.flash_violation.get_or_insert(addr);
            }
            return self.psram.translate(addr, size).map(|o| rd(&self.psram.mem, o as u32, size));
        }
        if let Some(o) = within(addr, size, TCM_BASE, TCM_SIZE) {
            return Some(rd(&self.tcm, o, size));
        }
        if let Some(o) = within(addr, size, LP_SRAM_BASE, LP_SRAM_SIZE) {
            return Some(rd(&self.lp_sram, o, size));
        }
        match (addr, size) {
            (MMU_ITEM_INDEX, 4) => return Some(self.mmu_index),
            (MMU_ITEM_CONTENT, 4) => return Some(self.mmu[(self.mmu_index & 0x3FF) as usize]),
            (crate::psram::MMU_ITEM_INDEX, 4) => return Some(self.psram.mmu_index),
            (crate::psram::MMU_ITEM_CONTENT, 4) => return Some(self.psram.mmu[(self.psram.mmu_index & 0x3FF) as usize]),
            (ANA_PLL_CTRL0, 4) => return Some(self.periph.read32(addr, self.ticks()) | MSPI_CAL_END),
            _ => {}
        }
        if crate::sdmmc::contains(addr) && size == 4 && addr % 4 == 0 {
            return Some(crate::sdmmc::read(self, addr));
        }
        if crate::audio::contains(addr) && size == 4 && addr % 4 == 0 {
            return Some(crate::audio::read(self, addr));
        }
        if (PERIPH.contains(&addr) || CLIC.contains(&addr) || CACHE_CTRL.contains(&addr)) && addr % size == 0 {
            let ticks = self.ticks();
            let word = self.periph.read32(addr & !3, ticks);
            let word = crate::dma2d::read_state(addr & !3, word).unwrap_or(word);
            // The RNG register (soc/wdev_reg.h): a fresh word each read.
            let word = if addr & !3 == RNG_DATA && self.rng_stuck {
                0x5A5A_5A5A
            } else if addr & !3 == RNG_DATA {
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                self.rng as u32
            } else {
                word
            };
            let shift = (addr & 3) * 8;
            let mask = if size == 4 { u32::MAX } else { (1 << (size * 8)) - 1 };
            return Some((word >> shift) & mask);
        }
        None
    }

    /// Data-side write. `false` is a bus error. Flash and ROM are read-only.
    pub fn write(&mut self, addr: u32, size: u32, v: u32) -> bool {
        if let Some(o) = within(addr, size, L2MEM_BASE, L2MEM_SIZE) {
            wr(&mut self.l2mem, o, size, v);
            self.cache.on_store(addr, size);
            return true;
        }
        if let Some(o) = within(addr, size, L2MEM_UNCACHED_BASE, L2MEM_SIZE) {
            // Bypasses the D-cache, but the I-cache may still hold the line.
            wr(&mut self.l2mem, o, size, v);
            self.cache.on_store(L2MEM_BASE + o, size);
            self.cache.writeback_d(L2MEM_BASE + o, size);
            return true;
        }
        if Psram::in_window(addr) || Psram::in_uncached(addr) {
            if self.l2_suspended && Psram::in_window(addr) {
                self.flash_violation.get_or_insert(addr);
            }
            let Some(o) = self.psram.translate(addr, size) else { return false };
            wr(&mut self.psram.mem, o as u32, size, v);
            // As for L2MEM: the I-cache may hold the line either way.
            let cached = crate::psram::WINDOW + (addr & (crate::psram::WINDOW_SIZE - 1));
            self.psram.cache.on_store(cached, size);
            if Psram::in_uncached(addr) {
                self.psram.cache.writeback_d(cached, size);
            }
            return true;
        }
        if let Some(o) = within(addr, size, TCM_BASE, TCM_SIZE) {
            wr(&mut self.tcm, o, size, v);
            return true;
        }
        if let Some(o) = within(addr, size, LP_SRAM_BASE, LP_SRAM_SIZE) {
            wr(&mut self.lp_sram, o, size, v);
            return true;
        }
        match (addr, size) {
            (crate::psram::MMU_ITEM_INDEX, 4) => {
                self.psram.mmu_index = v & 0x3FF;
                return true;
            }
            (crate::psram::MMU_ITEM_CONTENT, 4) => {
                self.psram.mmu[self.psram.mmu_index as usize] = v;
                return true;
            }
            (MMU_ITEM_INDEX, 4) => {
                self.mmu_index = v & 0x3FF;
                return true;
            }
            (MMU_ITEM_CONTENT, 4) => {
                self.mmu[self.mmu_index as usize] = v;
                return true;
            }
            _ => {}
        }
        if crate::sdmmc::contains(addr) && size == 4 && addr % 4 == 0 {
            crate::sdmmc::write(self, addr, v);
            return true;
        }
        if crate::audio::contains(addr) && size == 4 && addr % 4 == 0 {
            crate::audio::write(self, addr, v);
            return true;
        }
        if (PERIPH.contains(&addr) || CLIC.contains(&addr) || CACHE_CTRL.contains(&addr)) && addr % size == 0 {
            let ticks = self.ticks();
            let a = addr & !3;
            let word = if size == 4 {
                v
            } else {
                // Narrow writes merge into the register (read-modify-write).
                let shift = (addr & 3) * 8;
                let mask = ((1u32 << (size * 8)) - 1) << shift;
                (self.periph.read32(a, ticks) & !mask) | ((v << shift) & mask)
            };
            self.periph.write32(a, word, ticks);
            if crate::dma2d::contains(a) {
                crate::dma2d::on_write(self, a, word);
            }
            return true;
        }
        false
    }

    /// A DMA read of memory (L2MEM or PSRAM, not through the caches).
    pub fn dma_read(&mut self, addr: u32, size: u32) -> u32 {
        if let Some(o) = within(addr, size, L2MEM_BASE, L2MEM_SIZE) {
            return rd(&self.l2mem, o, size);
        }
        match self.psram.translate(addr, size) {
            Some(o) if Psram::in_window(addr) => rd(&self.psram.mem, o as u32, size),
            _ => 0,
        }
    }

    /// A DMA write of memory (L2MEM or PSRAM, not through the caches).
    pub fn dma_write(&mut self, addr: u32, size: u32, v: u32) {
        if let Some(o) = within(addr, size, L2MEM_BASE, L2MEM_SIZE) {
            wr(&mut self.l2mem, o, size, v);
        } else if let (Some(o), true) = (self.psram.translate(addr, size), Psram::in_window(addr)) {
            wr(&mut self.psram.mem, o as u32, size, v);
        }
    }

    /// Instruction fetch of a halfword. `None` is a bus error. ROM is handled
    /// by the CPU (only the cache functions are modelled).
    pub fn fetch16(&mut self, addr: u32) -> Option<u16> {
        if within(addr, 2, FLASH_BASE, FLASH_WINDOW_SIZE).is_some() {
            return self.window_read(addr, 2).map(|v| v as u16);
        }
        if let Some(o) = within(addr, 2, L2MEM_BASE, L2MEM_SIZE) {
            return Some(rd(&self.l2mem, o, 2) as u16);
        }
        if let Some(o) = within(addr, 2, TCM_BASE, TCM_SIZE) {
            return Some(rd(&self.tcm, o, 2) as u16);
        }
        if Psram::in_window(addr) {
            if self.l2_suspended {
                self.flash_violation.get_or_insert(addr);
            }
            return self.psram.translate(addr, 2).map(|o| rd(&self.psram.mem, o as u32, 2) as u16);
        }
        None
    }

    pub fn is_rom(addr: u32) -> bool {
        within(addr, 2, ROM_BASE, ROM_SIZE).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Maps flash 0x42_0000 (one 64 KiB page) at 0x4142_0000, as the kernel
    /// does, and returns that address.
    fn mapped(bus: &mut Bus) -> u32 {
        let vaddr = 0x4142_0000;
        assert!(bus.write(MMU_ITEM_INDEX, 4, (vaddr - FLASH_BASE) >> 16));
        assert!(bus.write(MMU_ITEM_CONTENT, 4, 0x42 | MMU_VALID));
        vaddr
    }

    #[test]
    fn erase_and_write_need_invalidation() {
        let mut bus = Bus::new(40_000_000, cache::Mode::Off, 1);
        let v = mapped(&mut bus);
        bus.load_phys_flash(0x42_0000, &[0x12; 8]).unwrap();
        assert_eq!(bus.read(v, 4), Some(0x1212_1212));

        assert_eq!(bus.flash_erase_sector(0x420), 0);
        bus.read(v, 4);
        assert_eq!(bus.stale_read.take(), Some(v));
        // L1 D alone is not enough: L2 still has the old line.
        bus.invalidate_window(1 << 4, v, FLASH_SECTOR);
        bus.read(v, 4);
        assert_eq!(bus.stale_read.take(), Some(v));
        bus.invalidate_window(1 << 5, v, FLASH_SECTOR);
        assert_eq!(bus.read(v, 4), Some(0xFFFF_FFFF));
        assert_eq!(bus.stale_read, None);

        // Programming only clears bits: a second write ANDs.
        let src = L2MEM_BASE + 0x1000;
        bus.write(src, 4, 0x0F0F_F0F0);
        assert_eq!(bus.flash_write(0x42_0000, src, 4), 0);
        bus.write(src, 4, 0x00FF_FFFF);
        assert_eq!(bus.flash_write(0x42_0000, src, 4), 0);
        bus.invalidate_window((1 << 4) | (1 << 5), v, 4);
        assert_eq!(bus.read(v, 4), Some(0x000F_F0F0));
        // Unaligned writes fail like the ROM's.
        assert_eq!(bus.flash_write(0x42_0002, src, 4), 1);
    }

    #[test]
    fn rom_flash_chip_parameters() {
        let mut bus = Bus::new(40_000_000, cache::Mode::Off, 1);
        let p = bus.read(ROM_SPIFLASH_LEGACY_DATA, 4).unwrap();
        assert_eq!(bus.read(p, 4), Some(EMU_FLASH_DEVICE_ID));
        assert_eq!(bus.read(p + 4, 4), Some(PHYS_FLASH_SIZE as u32));
    }
}
