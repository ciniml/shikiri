//! Address map of the ESP32-P4 rev v1.x as seen by an HP core (doc/02 §1).
//! Only what the kernel touches is backed; everything else is a bus error,
//! which the CPU reports as an access fault. Memory accepts misaligned
//! accesses; peripheral registers do not (judgement).

use crate::cache::{self, Cache};
use crate::periph::{Peripherals, SYSTIMER_HZ};

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
const CLIC: std::ops::Range<u32> = 0x2080_0000..0x2080_2000;

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
    /// First flash-window access made while L2 was suspended.
    pub flash_violation: Option<u32>,
    pub l2mem: Vec<u8>,
    tcm: Vec<u8>,
    pub lp_sram: Vec<u8>,
    pub cache: Cache,
    pub periph: Peripherals,
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
            mmu: vec![0; 1024],
            mmu_index: 0,
            l2_suspended: false,
            flash_violation: None,
            l2mem,
            tcm: vec![0; TCM_SIZE as usize],
            lp_sram,
            cache: Cache::new(cache_mode, L2MEM_BASE, L2MEM_SIZE),
            periph: Peripherals::default(),
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
        // The bootloader clears the MMU and maps only the kernel again.
        self.mmu.fill(0);
        self.l2_suspended = false;
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
        (self.cycles as u128 * SYSTIMER_HZ as u128 / self.cpu_hz as u128) as u64
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
        within(paddr, size, 0, PHYS_FLASH_SIZE as u32).map(|o| rd(&self.phys_flash, o, size))
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
        if let Some(o) = within(addr, size, TCM_BASE, TCM_SIZE) {
            return Some(rd(&self.tcm, o, size));
        }
        if let Some(o) = within(addr, size, LP_SRAM_BASE, LP_SRAM_SIZE) {
            return Some(rd(&self.lp_sram, o, size));
        }
        match (addr, size) {
            (MMU_ITEM_INDEX, 4) => return Some(self.mmu_index),
            (MMU_ITEM_CONTENT, 4) => return Some(self.mmu[(self.mmu_index & 0x3FF) as usize]),
            _ => {}
        }
        if (PERIPH.contains(&addr) || CLIC.contains(&addr)) && addr % size == 0 {
            let ticks = self.ticks();
            let word = self.periph.read32(addr & !3, ticks);
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
        if let Some(o) = within(addr, size, TCM_BASE, TCM_SIZE) {
            wr(&mut self.tcm, o, size, v);
            return true;
        }
        if let Some(o) = within(addr, size, LP_SRAM_BASE, LP_SRAM_SIZE) {
            wr(&mut self.lp_sram, o, size, v);
            return true;
        }
        match (addr, size) {
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
        if (PERIPH.contains(&addr) || CLIC.contains(&addr)) && addr % size == 0 {
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
            return true;
        }
        false
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
        None
    }

    pub fn is_rom(addr: u32) -> bool {
        within(addr, 2, ROM_BASE, ROM_SIZE).is_some()
    }
}
