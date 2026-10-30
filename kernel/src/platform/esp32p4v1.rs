//! ESP32-P4 rev v1.x (M5Stack Tab5). Register addresses come from ESP-IDF 6.0
//! `components/soc/esp32p4/register/hw_ver1` (doc/06 §2, §3.1).
//!
//! Verified on a Tab5 (rev v1.0) in M1 (doc/09) and M2 (doc/10).

use super::Platform;

mod clock;
mod display;
mod i2c;
mod psram;
mod tab5;

// USB Serial/JTAG (doc/06 §3.1): usb_serial_jtag_reg.h
const USJ_BASE: usize = 0x500D_2000;
const USJ_EP1: usize = USJ_BASE;
const USJ_EP1_CONF: usize = USJ_BASE + 0x4;
const USJ_WR_DONE: u32 = 1 << 0;
const USJ_SERIAL_IN_EP_DATA_FREE: u32 = 1 << 1;
/// How long to wait for room in the FIFO before deciding that nobody reads
/// the CDC port. Longer than the host's 1 ms USB polling interval: with a
/// poll-count limit (50,000) bursts were dropped while a host was reading
/// (doc/14 §9.3).
const USJ_TIMEOUT_TICKS: u64 = SYSTIMER_TICKS_PER_MS * 50;
/// After a timeout: until the FIFO has room again, bytes are dropped
/// without waiting. Otherwise every byte would wait the full timeout while
/// nobody reads the port, slowing the whole system down several times
/// (seen while logs were read over RTT only, doc/12 §8).
static mut USJ_STALLED: bool = false;

// LP_WDT / RWDT (doc/06 §2): lp_wdt_reg.h, lpwdt_ll.h
const LP_WDT_BASE: usize = 0x5011_6000;
const LP_WDT_CONFIG0: usize = LP_WDT_BASE;
const LP_WDT_WPROTECT: usize = LP_WDT_BASE + 0x18;
const LP_WDT_CONFIG1: usize = LP_WDT_BASE + 0x4;
const LP_WDT_FEED: usize = LP_WDT_BASE + 0x14;
/// CONFIG1.WDT_STG0_HOLD for about 4 s. Measured on the Tab5 (doc/12 §7):
/// the bootloader's 1,224,000 (9 s at 136 kHz) gave 18 s, i.e. the timeout
/// is 2 * hold / 136 kHz on this chip (inferred from one data point).
const LP_WDT_STG0_HOLD: u32 = 272_000;
const LP_WDT_WKEY: u32 = 0x50D8_3AA1;
const LP_WDT_EN: u32 = 1 << 31;
const LP_WDT_FEED_BIT: u32 = 1 << 31;
/// CONFIG0.WDT_STG0, bits [30:28]; 3 = reset the HP system but not the RTC
/// (LP) domain, so LP SRAM and the log ring survive
/// (esp_hal_wdt/esp32p4/include/hal/lpwdt_ll.h:39-43). The bootloader sets 4,
/// which also resets the RTC domain (bootloader_init.c:93).
const LP_WDT_STG0_SHIFT: u32 = 28;
const LP_WDT_STG0_MASK: u32 = 0b111 << LP_WDT_STG0_SHIFT;
const LP_WDT_STG_RESET_SYSTEM: u32 = 3;

// Reset cause (lp_clkrst_reg.h LP_CLKRST_RESET_CAUSE_REG, HPCORE0 bits
// [12:7]); values are RESET_REASON in esp_rom/esp32p4/include/esp32p4/rom/rtc.h.
const LP_CLKRST_RESET_CAUSE: usize = 0x5011_1010;
const ROM_SOFTWARE_RESET: usize = 0x4FC0_0094;
const EFUSE_RD_MAC_SYS_2: usize = 0x5012_D04C;

// Clock, LDO and PLL state, logged at boot for M4 (doc/research/
// m4-psram-and-clock.md §5 step 0): hp_sys_clkrst_reg.h:29,69,102,147,1057,
// 2989; lp_clkrst_reg.h:662; pmu_reg.h:2563,4339,4407.
const HP_SYS_CLKRST: usize = 0x500E_6000;
const CLOCK_REGS: [(&str, usize); 9] = [
    ("root_clk_ctrl0", HP_SYS_CLKRST + 0x04),
    ("root_clk_ctrl1", HP_SYS_CLKRST + 0x08),
    ("root_clk_ctrl2", HP_SYS_CLKRST + 0x0C),
    ("soc_clk_ctrl0", HP_SYS_CLKRST + 0x14),
    ("peri_clk_ctrl00", HP_SYS_CLKRST + 0x30),
    ("ana_pll_ctrl0", HP_SYS_CLKRST + 0xBC),
    ("lp_hp_clk_ctrl", 0x5011_1040),
    ("pmu_rf_pwc", 0x5011_515C),
    ("pmu_ext_ldo_p1_0p1a", 0x5011_51D0),
];
const PMU_EXT_LDO_P1_0P1A_ANA: usize = 0x5011_51D4;

// Timer group MWDTs: timer_group_reg.h
const TIMG_BASES: [usize; 2] = [0x500C_2000, 0x500C_3000];
const TIMG_WDTCONFIG0: usize = 0x48;
const TIMG_WDTWPROTECT: usize = 0x64;
const TIMG_WDT_WKEY: u32 = 0x50D8_3AA1;
const TIMG_WDT_EN: u32 = 1 << 31;

// SYSTIMER (doc/10 §2): systimer_reg.h, hal/esp32p4/include/hal/systimer_ll.h.
// Unit 0 counts at 16 MHz from reset (esp_hw_support/port/esp32p4/systimer.c);
// alarm 0 is compared against it. The bus and function clocks are enabled at
// reset (hp_sys_clkrst_reg.h: SYSTIMER_APB_CLK_EN, SYSTIMER_CLK_EN).
const SYSTIMER_BASE: usize = 0x500E_2000;
const SYSTIMER_TICKS_PER_MS: u64 = 16_000;
const SYSTIMER_CONF: usize = SYSTIMER_BASE;
const SYSTIMER_CLK_EN: u32 = 1 << 31;
const SYSTIMER_TARGET0_WORK_EN: u32 = 1 << 24;
const SYSTIMER_UNIT0_OP: usize = SYSTIMER_BASE + 0x04;
const SYSTIMER_UNIT0_UPDATE: u32 = 1 << 30;
const SYSTIMER_UNIT0_VALUE_VALID: u32 = 1 << 29;
const SYSTIMER_TARGET0_HI: usize = SYSTIMER_BASE + 0x1C;
const SYSTIMER_TARGET0_LO: usize = SYSTIMER_BASE + 0x20;
/// Period mode off, compared against unit 0.
const SYSTIMER_TARGET0_CONF: usize = SYSTIMER_BASE + 0x34;
const SYSTIMER_UNIT0_VALUE_HI: usize = SYSTIMER_BASE + 0x40;
const SYSTIMER_UNIT0_VALUE_LO: usize = SYSTIMER_BASE + 0x44;
const SYSTIMER_COMP0_LOAD: usize = SYSTIMER_BASE + 0x50;
const SYSTIMER_INT_ENA: usize = SYSTIMER_BASE + 0x64;
const SYSTIMER_INT_CLR: usize = SYSTIMER_BASE + 0x6C;
const SYSTIMER_TARGET0_INT: u32 = 1 << 0;

// Interrupt matrix (interrupt_core0_reg.h): one map register per source,
// holding the CLIC interrupt index (CPU line + 16, hal/interrupt_clic_ll.h).
const INTMTX_CORE0_BASE: usize = 0x500D_6000;
const SYSTIMER_TARGET0_SOURCE: usize = 53; // ETS_SYSTIMER_TARGET0_INTR_SOURCE, map reg +0xD4
/// CPU interrupt line used for the scheduler tick.
const TIMER_CPU_LINE: usize = 1;
const CLIC_EXT_INTR_NUM_OFFSET: usize = 16;
const TIMER_CLIC_INDEX: usize = TIMER_CPU_LINE + CLIC_EXT_INTR_NUM_OFFSET;

// CLIC, rev v1.x layout (soc/esp32p4/include/soc/clic_reg.h, doc/01 §4.1).
// One word per interrupt: [31:24] ctl (priority in the top NLBITS=3 bits),
// [23:16] attr (mode RO, trig, shv), [8] ie, [0] ip.
const CLIC_INT_THRESH: usize = 0x2080_0008;
const CLIC_CTRL_BASE: usize = 0x2080_1000;
/// Priority 1 (ctl = 1 << 5 | 0x1F), level-triggered, non-vectored, enabled.
const CLIC_TIMER_CTRL: u32 = (0x3F << 24) | (1 << 8);

// Flash MMU (doc/14 §5.1, doc/research/m3-flash-access.md §1): one entry per
// 64 KiB page of the window 0x4000_0000-0x43FF_FFFF. Write the entry number to
// ITEM_INDEX, then `(paddr >> 16) | VALID` to ITEM_CONTENT (spi_mem_c_reg.h:
// 2630,2642; ext_mem_defs.h:57). The bootloader uses entries 0-31 for the
// kernel and 1023; the kernel maps the whole 16 MiB flash linearly at
// 0x4100_0000 (entries 256-511).
const MMU_ITEM_CONTENT: usize = 0x5008_C37C;
const MMU_ITEM_INDEX: usize = 0x5008_C380;
const MMU_VALID: u32 = 1 << 12;
const FLASH_WINDOW_BASE: usize = 0x4000_0000;
const FLASH_WINDOW: usize = 0x4100_0000;
/// XIP text of process slot k at XIP_TEXT + k * XIP_SLOT (flash MMU entries
/// 512-1023); its data at the same offset in the PSRAM window's second
/// alias, 0x4A00_0000 + k * XIP_SLOT (doc/15 §2).
const XIP_TEXT: usize = 0x4200_0000;
const XIP_SLOT: usize = 8 << 20;
const FLASH_SIZE: usize = 16 << 20;
const MMU_PAGE: usize = 64 << 10;
// esp32p4.rom.ld:259-260
const ROM_CACHE_SUSPEND_L2: usize = 0x4FC0_0508;
/// esp32p4.rom.ld:198. With PSRAM in use, dirty L1 D lines must be written
/// back before L2 is suspended: an automatic writeback into the suspended
/// L2 would hang the core (esp_mm/esp_cache_utils.c:27-37).
const ROM_CACHE_WRITEBACK_ALL: usize = 0x4FC0_0414;
const ROM_CACHE_RESUME_L2: usize = 0x4FC0_050C;
const CACHE_MAP_L2_CACHE: u32 = 1 << 5;
/// MHCR (0x7C1): return stack, predictive jumps, branch target prediction
/// (riscv/include/riscv/rv_utils.h:48-52). Off while the map changes.
const MHCR_BRANCH_PREDICTION: usize = (1 << 4) | (1 << 5) | (1 << 12);

/// Points `count` flash MMU entries from `first_entry` at the flash pages
/// from `first_page` (the first `valid` of them; the rest become invalid),
/// following ESP-IDF's sequence for P4: L1 D written back, branch
/// prediction off, L2 suspended, entries written, the range invalidated in
/// L1 I/D and L2, L2 resumed. Runs from RAM: the flash window (where the
/// rest of the kernel lives) is unusable while L2 is suspended. Interrupts
/// are off in the kernel.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
unsafe fn map_flash_pages(first_entry: usize, count: usize, first_page: usize, valid: usize) {
    unsafe {
        let suspend: unsafe extern "C" fn() -> u32 = core::mem::transmute(ROM_CACHE_SUSPEND_L2);
        let resume: unsafe extern "C" fn(u32) = core::mem::transmute(ROM_CACHE_RESUME_L2);
        let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
        let mhcr: usize;
        core::arch::asm!("csrrc {0}, 0x7c1, {1}", out(reg) mhcr, in(reg) MHCR_BRANCH_PREDICTION);
        let writeback_all: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_CACHE_WRITEBACK_ALL);
        writeback_all(CACHE_MAP_L1_DCACHE);
        let autoload = suspend();
        let mut i = 0;
        while i < count {
            core::ptr::write_volatile(MMU_ITEM_INDEX as *mut u32, (first_entry + i) as u32);
            let v = if i < valid { (first_page + i) as u32 | MMU_VALID } else { 0 };
            core::ptr::write_volatile(MMU_ITEM_CONTENT as *mut u32, v);
            i += 1;
        }
        let (a, n) = ((FLASH_WINDOW_BASE + first_entry * MMU_PAGE) as u32, (count * MMU_PAGE) as u32);
        invalidate(CACHE_MAP_L1_ICACHE_MASK, a, n);
        invalidate(CACHE_MAP_L1_DCACHE, a, n);
        invalidate(CACHE_MAP_L2_CACHE, a, n);
        resume(autoload);
        core::arch::asm!("csrw 0x7c1, {0}", in(reg) mhcr);
    }
}

// ROM SPI flash functions (esp32p4.rom.ld:123-125 for rev v1.x; the eco5 file
// is for v3), and the pointer to the chip parameters the bootloader set with
// esp_rom_spiflash_config_param (esp32p4.rom.ld:169, esp_rom_spiflash.h:361).
// They drive SPI1 and wait until the chip is done. There is no auto-suspend
// on the P4, so SPI0 (the cache) must not touch flash meanwhile (doc/14 §5.2).
const ROM_SPIFLASH_ERASE_SECTOR: usize = 0x4FC0_014C;
const ROM_SPIFLASH_WRITE: usize = 0x4FC0_0154;
const ROM_SPIFLASH_LEGACY_DATA: usize = 0x4FF3_FFE8;
const SECTOR: u32 = 4096;
const PAGE: usize = 256;

/// One erase (`src` null) or one page write, with the flash window out of
/// use: branch prediction off, L2 suspended, the ROM call, the written range
/// invalidated in L1 I/D and L2, L2 resumed. Runs from RAM like
/// `map_flash_linear`; `src` must be in RAM too. Returns the ROM's result
/// (0 = OK, 1 = error, 2 = timeout).
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
unsafe fn flash_op(offset: u32, src: *const u32, len: u32) -> i32 {
    unsafe {
        let suspend: unsafe extern "C" fn() -> u32 = core::mem::transmute(ROM_CACHE_SUSPEND_L2);
        let resume: unsafe extern "C" fn(u32) = core::mem::transmute(ROM_CACHE_RESUME_L2);
        let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
        let erase: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_SPIFLASH_ERASE_SECTOR);
        let write: unsafe extern "C" fn(u32, *const u32, u32) -> i32 = core::mem::transmute(ROM_SPIFLASH_WRITE);
        let mhcr: usize;
        core::arch::asm!("csrrc {0}, 0x7c1, {1}", out(reg) mhcr, in(reg) MHCR_BRANCH_PREDICTION);
        let writeback_all: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_CACHE_WRITEBACK_ALL);
        writeback_all(CACHE_MAP_L1_DCACHE);
        let autoload = suspend();
        let r = if src.is_null() { erase(offset / SECTOR) } else { write(offset, src, len) };
        let a = FLASH_WINDOW as u32 + offset;
        invalidate(CACHE_MAP_L1_ICACHE_MASK, a, len);
        invalidate(CACHE_MAP_L1_DCACHE, a, len);
        invalidate(CACHE_MAP_L2_CACHE, a, len);
        resume(autoload);
        core::arch::asm!("csrw 0x7c1, {0}", in(reg) mhcr);
        r
    }
}

fn rom_result(r: i32) -> Result<(), &'static str> {
    match r {
        0 => Ok(()),
        2 => Err("flash timeout"),
        _ => Err("flash error"),
    }
}

// ROM cache functions (esp_rom/esp32p4/ld/esp32p4.rom.ld,
// esp_rom/esp32p4/include/esp32p4/rom/cache.h). ESP-IDF uses these rather than
// fence.i to make written code fetchable (doc/02 §2.2).
const ROM_CACHE_INVALIDATE_ADDR: usize = 0x4FC0_03E4;
const ROM_CACHE_WRITEBACK_ADDR: usize = 0x4FC0_03F4;
const CACHE_MAP_L1_ICACHE_MASK: u32 = (1 << 0) | (1 << 1);
const CACHE_MAP_L1_DCACHE: u32 = 1 << 4;

type RomCacheFn = unsafe extern "C" fn(map: u32, addr: u32, size: u32) -> i32;

/// Invalidates `[addr, addr + len)` in L1 I and D, and in L2 if `l2`
/// (external memory: flash and PSRAM sit behind L2).
fn cache_invalidate(addr: usize, len: usize, l2: bool) {
    unsafe {
        let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
        invalidate(CACHE_MAP_L1_ICACHE_MASK, addr as u32, len as u32);
        invalidate(CACHE_MAP_L1_DCACHE, addr as u32, len as u32);
        if l2 {
            invalidate(CACHE_MAP_L2_CACHE, addr as u32, len as u32);
        }
    }
}

/// Writes `[addr, addr + len)` back from L1 D, then from L2 if `l2`
/// (cache_ll.h:644-693: the levels are written back in order).
fn cache_writeback(addr: usize, len: usize, l2: bool) {
    unsafe {
        let writeback: RomCacheFn = core::mem::transmute(ROM_CACHE_WRITEBACK_ADDR);
        writeback(CACHE_MAP_L1_DCACHE, addr as u32, len as u32);
        if l2 {
            writeback(CACHE_MAP_L2_CACHE, addr as u32, len as u32);
        }
    }
}

pub struct Esp32p4v1;

/// Set once at boot by `psram_init` (single core, kernel only).
static mut PSRAM_SIZE: usize = 0;

/// ESP-IDF `esp_app_desc_t` (esp_app_format/include/esp_app_desc.h). The
/// 2nd stage bootloader checks the eFuse block revision range; espflash reads
/// the MMU page size.
#[repr(C)]
struct AppDesc {
    magic_word: u32,
    secure_version: u32,
    reserv1: [u32; 2],
    version: [u8; 32],
    project_name: [u8; 32],
    time: [u8; 16],
    date: [u8; 16],
    idf_ver: [u8; 32],
    app_elf_sha256: [u8; 32],
    min_efuse_blk_rev_full: u16,
    max_efuse_blk_rev_full: u16,
    mmu_page_size: u8,
    reserv3: [u8; 3],
    reserv2: [u32; 18],
}

const _: () = assert!(core::mem::size_of::<AppDesc>() == 256);

const fn cstr<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < b.len() && i < N - 1 {
        out[i] = b[i];
        i += 1;
    }
    out
}

/// The kernel's version and secure_version as the bootloader and kernel
/// updates see them (doc/16 §2). The update tests build kernels with other
/// values (`T5_KERNEL_VERSION`, `T5_KERNEL_SECURE_VERSION`).
const VERSION: &str = match option_env!("T5_KERNEL_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
const SECURE_VERSION: u32 = match option_env!("T5_KERNEL_SECURE_VERSION") {
    Some(v) => parse_u32(v),
    None => 0,
};

const fn parse_u32(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut v = 0u32;
    let mut i = 0;
    while i < b.len() {
        assert!(b[i].is_ascii_digit(), "T5_KERNEL_SECURE_VERSION must be a decimal number");
        v = v * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    v
}

#[used]
#[unsafe(link_section = ".flash.appdesc")]
static APP_DESC: AppDesc = AppDesc {
    magic_word: 0xABCD_5432,
    secure_version: SECURE_VERSION,
    reserv1: [0; 2],
    version: cstr(VERSION),
    project_name: cstr("tab5-os"),
    time: [0; 16],
    date: [0; 16],
    idf_ver: cstr("tab5-os"),
    app_elf_sha256: [0; 32],
    // Same range as an ESP-IDF 6.0 build for rev v1.x (the Tab5 reports
    // eFuse block revision v0.3).
    min_efuse_blk_rev_full: 0,
    max_efuse_blk_rev_full: 199,
    mmu_page_size: 16, // 64 KiB
    reserv3: [0; 3],
    reserv2: [0; 18],
};

/// The CPU clock, measured: mcycle against SYSTIMER over 1 ms.
fn cpu_khz() -> u64 {
    let read_mcycle = || -> u32 {
        let c: u32;
        unsafe { core::arch::asm!("csrr {0}, mcycle", out(reg) c) };
        c
    };
    let (t0, c0) = (Esp32p4v1::now(), read_mcycle());
    while Esp32p4v1::now() - t0 < SYSTIMER_TICKS_PER_MS {}
    let (t1, c1) = (Esp32p4v1::now(), read_mcycle());
    c1.wrapping_sub(c0) as u64 * SYSTIMER_TICKS_PER_MS / (t1 - t0)
}

unsafe fn rd(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

unsafe fn wr(addr: usize, v: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

impl Platform for Esp32p4v1 {
    const NAME: &'static str = "esp32p4-v1.x";
    const MTVEC_MODE: usize = 3;
    // Upper L2MEM region with a 128 KiB L2 cache (doc/02 §1.1, doc/07 §3).
    const APP_POOL: core::ops::Range<usize> = 0x4FF4_0000..0x4FFA_0000;
    const PROTECTED_MMIO: usize = USJ_BASE;
    const SERVE: bool = true;
    const TIMER_HZ: u64 = 16_000_000;
    const TIMER_IRQ: usize = TIMER_CLIC_INDEX;

    fn init() {
        unsafe {
            // The 2nd stage bootloader leaves RWDT running. Shorten its
            // timeout to ~4 s and reset only the HP system on expiry, so LP
            // SRAM survives a lock-up; the scheduler feeds it (doc/12 L3).
            Self::arm_watchdog();
            for base in TIMG_BASES {
                wr(base + TIMG_WDTWPROTECT, TIMG_WDT_WKEY);
                wr(base + TIMG_WDTCONFIG0, rd(base + TIMG_WDTCONFIG0) & !TIMG_WDT_EN);
                wr(base + TIMG_WDTWPROTECT, 0);
            }

            // Alarm 0: one-shot on unit 0, routed to CPU line 1 at priority 1.
            // A threshold of 0 lets every enabled level through; U-mode runs
            // at level 0 and the kernel masks with MIE=0.
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) | SYSTIMER_CLK_EN);
            Self::clear_alarm();
            wr(SYSTIMER_TARGET0_CONF, 0);
            wr(SYSTIMER_INT_ENA, rd(SYSTIMER_INT_ENA) | SYSTIMER_TARGET0_INT);
            wr(INTMTX_CORE0_BASE + 4 * SYSTIMER_TARGET0_SOURCE, TIMER_CLIC_INDEX as u32);
            wr(CLIC_CTRL_BASE + 4 * TIMER_CLIC_INDEX, CLIC_TIMER_CTRL);
            wr(CLIC_INT_THRESH, 0);
        }
    }

    fn now() -> u64 {
        // Same sequence as systimer_hal_get_counter_value(): snapshot, then
        // read lo/hi/lo until lo is stable.
        unsafe {
            wr(SYSTIMER_UNIT0_OP, SYSTIMER_UNIT0_UPDATE);
            while rd(SYSTIMER_UNIT0_OP) & SYSTIMER_UNIT0_VALUE_VALID == 0 {}
            let mut lo = rd(SYSTIMER_UNIT0_VALUE_LO);
            loop {
                let hi = rd(SYSTIMER_UNIT0_VALUE_HI);
                let lo2 = rd(SYSTIMER_UNIT0_VALUE_LO);
                if lo2 == lo {
                    return ((hi as u64) << 32) | lo as u64;
                }
                lo = lo2;
            }
        }
    }

    fn set_alarm(at: u64) {
        // The P4 alarm fires immediately for a target already in the past
        // (SOC_SYSTIMER_ALARM_MISS_COMPENSATE), so no retry loop is needed.
        unsafe {
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) & !SYSTIMER_TARGET0_WORK_EN);
            wr(SYSTIMER_INT_CLR, SYSTIMER_TARGET0_INT);
            wr(SYSTIMER_TARGET0_HI, (at >> 32) as u32);
            wr(SYSTIMER_TARGET0_LO, at as u32);
            wr(SYSTIMER_COMP0_LOAD, 1);
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) | SYSTIMER_TARGET0_WORK_EN);
        }
    }

    fn clear_alarm() {
        unsafe {
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) & !SYSTIMER_TARGET0_WORK_EN);
            wr(SYSTIMER_INT_CLR, SYSTIMER_TARGET0_INT);
        }
    }

    fn console_write(bytes: &[u8]) -> bool {
        // Single core, interrupts off in the kernel.
        let stalled = unsafe { &mut *core::ptr::addr_of_mut!(USJ_STALLED) };
        let free = || unsafe { rd(USJ_EP1_CONF) & USJ_SERIAL_IN_EP_DATA_FREE != 0 };
        let mut all = true;
        for &b in bytes {
            if !free() {
                if !*stalled {
                    let deadline = Self::now() + USJ_TIMEOUT_TICKS;
                    while !free() && Self::now() < deadline {}
                }
                if !free() {
                    *stalled = true;
                    all = false;
                    continue;
                }
            }
            *stalled = false;
            unsafe { wr(USJ_EP1, b as u32) };
        }
        unsafe { wr(USJ_EP1_CONF, USJ_WR_DONE) };
        all
    }

    fn sync_code(addr: usize, len: usize) {
        // L2MEM is only behind the L1 caches. PSRAM is behind L2 as well:
        // write both levels back, as ESP-IDF does for code it copies into
        // PSRAM (mmu_psram_flash_v2.c:82-96). The I-cache refills through L2.
        let external = (psram::WINDOW..psram::WINDOW + (64 << 20)).contains(&addr);
        cache_writeback(addr, len, external);
        unsafe {
            let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
            invalidate(CACHE_MAP_L1_ICACHE_MASK, addr as u32, len as u32);
            core::arch::asm!("fence.i");
        }
    }

    fn map_flash() -> Option<usize> {
        let first = (FLASH_WINDOW - FLASH_WINDOW_BASE) / MMU_PAGE;
        let pages = FLASH_SIZE / MMU_PAGE;
        unsafe { map_flash_pages(first, pages, 0, pages) };
        Some(FLASH_WINDOW)
    }

    fn kernel_slot() -> Option<u32> {
        // The bootloader maps the kernel image from flash MMU entry 0
        // (0x4000_0000), as M3b found; reading an entry needs no cache stop.
        let entry = unsafe {
            wr(MMU_ITEM_INDEX, 0);
            rd(MMU_ITEM_CONTENT)
        };
        (entry & MMU_VALID != 0).then(|| (entry & (MMU_VALID - 1)) * MMU_PAGE as u32)
    }

    fn chip_revision() -> u16 {
        // efuse_ll.h:58-67; efuse_reg.h:749-824.
        let v = unsafe { rd(EFUSE_RD_MAC_SYS_2) };
        let major = ((v >> 23) & 1) << 2 | ((v >> 4) & 3);
        (major * 100 + (v & 0xF)) as u16
    }

    fn reset() -> ! {
        display::stop();
        // esp_rom_software_reset_system (esp32p4.rom.ld:52).
        unsafe {
            let reset: unsafe extern "C" fn() -> ! = core::mem::transmute(ROM_SOFTWARE_RESET);
            reset()
        }
    }

    fn flash_offset(addr: usize) -> Option<u32> {
        (FLASH_WINDOW..FLASH_WINDOW + FLASH_SIZE).contains(&addr).then(|| (addr - FLASH_WINDOW) as u32)
    }

    fn map_xip(slot: usize, flash_offset: u32, pages: usize) -> Option<usize> {
        let psram = unsafe { PSRAM_SIZE };
        let per_slot = XIP_SLOT / MMU_PAGE;
        if flash_offset as usize % MMU_PAGE != 0 || pages > per_slot || (slot + 1) * XIP_SLOT > psram {
            return None;
        }
        let vbase = XIP_TEXT + slot * XIP_SLOT;
        let first = (vbase - FLASH_WINDOW_BASE) / MMU_PAGE;
        unsafe { map_flash_pages(first, per_slot, flash_offset as usize / MMU_PAGE, pages) };
        Some(vbase)
    }

    fn retire_alias(addr: usize, len: usize) {
        // The two linear maps of the PSRAM (doc/15 §2).
        let half = psram::ALIAS - psram::WINDOW;
        let other = match addr {
            a if (psram::WINDOW..psram::ALIAS).contains(&a) => a + half,
            a if (psram::ALIAS..psram::ALIAS + half).contains(&a) => a - half,
            _ => return,
        };
        cache_writeback(other, len, true);
        cache_invalidate(other, len, true);
    }

    fn flash_erase_sector(offset: u32) -> Result<(), &'static str> {
        if offset % SECTOR != 0 || offset as usize >= FLASH_SIZE {
            return Err("bad sector");
        }
        rom_result(unsafe { flash_op(offset, core::ptr::null(), SECTOR) })
    }

    fn flash_write(offset: u32, data: &[u8]) -> Result<(), &'static str> {
        let o = offset as usize;
        if o % 4 != 0 || data.is_empty() || o % PAGE + data.len() > PAGE || o + data.len() > FLASH_SIZE {
            return Err("bad write");
        }
        // A copy on the stack (RAM), padded to whole words with the erased
        // value, whatever `data` points at.
        let mut words = [u32::MAX; PAGE / 4];
        for (i, &b) in data.iter().enumerate() {
            words[i / 4] &= !(0xFF << (8 * (i % 4))) | ((b as u32) << (8 * (i % 4)));
        }
        let len = data.len().next_multiple_of(4) as u32;
        rom_result(unsafe { flash_op(offset, words.as_ptr(), len) })
    }

    fn psram_init() -> Result<(usize, usize), &'static str> {
        // A reset that did not go through `reset` (a debug probe's, the
        // watchdog's) leaves the frame DMA reading PSRAM; stop it before the
        // PSRAM is set up again (doc/17 §10).
        if display::stop() {
            t5_abi::event!(Warn, "display.dma_was_running");
        }
        let size = psram::init()?;
        unsafe { PSRAM_SIZE = size };
        Ok((psram::WINDOW, size))
    }

    fn psram_test() -> Result<u32, &'static str> {
        let size = unsafe { PSRAM_SIZE };
        if size == 0 {
            return Err("no psram");
        }
        match psram::memtest(size, 1) {
            0 => Ok(size as u32),
            _ => Err("memory test failed"),
        }
    }

    fn flash_chip() -> Option<(u32, u32)> {
        // esp_rom_spiflash_chip_t starts with device_id, chip_size.
        let p = unsafe { rd(ROM_SPIFLASH_LEGACY_DATA) } as usize;
        (0x4FF0_0000..0x4FFC_0000).contains(&p).then(|| unsafe { (rd(p), rd(p + 4)) })
    }

    fn log_info() {
        // The RWDT timeout is in RTC slow clock cycles (CONFIG1.WDT_STG0_HOLD,
        // shifted by eFuse WDT_DELAY_SEL); logging it relates the measured
        // timeout to the setting (doc/12 §7).
        let (config0, hold) = unsafe { (rd(LP_WDT_CONFIG0), rd(LP_WDT_CONFIG1)) };
        t5_abi::event!(Info, "boot.rwdt", config0 = config0, stg0_hold = hold);
        for (name, addr) in CLOCK_REGS {
            t5_abi::event!(Debug, "boot.clock_reg", name = name, addr = t5_abi::log::Addr(addr as u32), value = unsafe { rd(addr) });
        }
        t5_abi::event!(Debug, "boot.clock_reg", name = "pmu_ext_ldo_p1_0p1a_ana", addr = t5_abi::log::Addr(PMU_EXT_LDO_P1_0P1A_ANA as u32), value = unsafe { rd(PMU_EXT_LDO_P1_0P1A_ANA) });
        t5_abi::event!(Info, "boot.clock", cpu_khz = cpu_khz());
    }

    fn clock_init() {
        // PVT first, then the clock, as ESP-IDF's pmu_init and esp_clk_init
        // do (doc/research/m4-psram-and-clock.md §10).
        if !clock::pvt_on() {
            t5_abi::event!(Warn, "clock.no_pvt");
            return;
        }
        let mhz = clock::cpu_360();
        t5_abi::event!(Info, "clock.cpu", mhz = mhz.unwrap_or(0), measured_khz = cpu_khz());
        clock::log_voltages("cpu360");
    }

    fn set_cpu_mhz(mhz: u32) -> Result<u32, &'static str> {
        let r = clock::set_cpu_mhz(mhz).ok_or("unsupported clock or state")?;
        t5_abi::event!(Info, "clock.cpu", mhz = r, measured_khz = cpu_khz());
        clock::log_voltages("clock_cmd");
        Ok(r)
    }

    fn set_pvt(on: bool) -> Result<u32, &'static str> {
        if on {
            clock::pvt_on().then_some(1).ok_or("no PVT calibration")
        } else {
            clock::pvt_off();
            Ok(0)
        }
    }

    fn log_power(stage: &'static str) {
        clock::log_voltages(stage);
    }

    fn feed_watchdog() {
        unsafe {
            wr(LP_WDT_WPROTECT, LP_WDT_WKEY);
            wr(LP_WDT_FEED, LP_WDT_FEED_BIT);
            wr(LP_WDT_WPROTECT, 0);
        }
    }

    fn arm_watchdog() {
        unsafe {
            wr(LP_WDT_WPROTECT, LP_WDT_WKEY);
            wr(LP_WDT_CONFIG1, LP_WDT_STG0_HOLD);
            let c = rd(LP_WDT_CONFIG0) & !LP_WDT_STG0_MASK;
            wr(LP_WDT_CONFIG0, c | (LP_WDT_STG_RESET_SYSTEM << LP_WDT_STG0_SHIFT) | LP_WDT_EN);
            wr(LP_WDT_FEED, LP_WDT_FEED_BIT);
            wr(LP_WDT_WPROTECT, 0);
        }
    }

    fn board_init() -> super::Panel {
        tab5::init()
    }

    fn touch_read(out: &mut [super::TouchPoint; 5]) -> Result<usize, &'static str> {
        tab5::touch_read(out)
    }

    fn display_init(panel: super::Panel, fb: usize) -> Result<usize, &'static str> {
        display::init(panel, fb).map(|_| display::FB_BYTES)
    }

    fn display_service() {
        display::service();
    }

    fn display_buffer() -> Option<(usize, usize)> {
        display::buffer().map(|fb| (fb, display::FB_BYTES))
    }

    fn display_size() -> Option<(u16, u16)> {
        display::buffer().map(|_| (display::WIDTH as u16, display::HEIGHT as u16))
    }

    fn display_flush(addr: usize, len: usize) {
        display::flush(addr, len);
    }

    fn display_frames() -> u32 {
        display::frames()
    }

    fn display_stats() -> Result<u32, &'static str> {
        display::log_stats()
    }

    fn handle_irq(code: usize) -> bool {
        if code == display::IRQ {
            display::service();
            return true;
        }
        false
    }

    fn i2c_scan() -> Result<u32, &'static str> {
        let mut n = 0;
        for addr in 0x08..0x78u8 {
            if i2c::probe(addr) {
                t5_abi::event!(Info, "i2c.found", addr = addr);
                n += 1;
            }
        }
        Ok(n)
    }

    fn reset_cause() -> u32 {
        unsafe { (rd(LP_CLKRST_RESET_CAUSE) >> 7) & 0x3F }
    }

    fn exit(_code: u32) -> ! {
        // A deliberate stop: the watchdog must not reset the chip.
        unsafe {
            wr(LP_WDT_WPROTECT, LP_WDT_WKEY);
            wr(LP_WDT_CONFIG0, rd(LP_WDT_CONFIG0) & !LP_WDT_EN);
            wr(LP_WDT_WPROTECT, 0);
        }
        loop {
            unsafe { core::arch::asm!("wfi") };
        }
    }
}
