//! ESP32-P4 rev v1.x (M5Stack Tab5). Register addresses come from ESP-IDF 6.0
//! `components/soc/esp32p4/register/hw_ver1` (doc/06 §2, §3.1).
//!
//! Verified on a Tab5 (rev v1.0) in M1 (doc/09) and M2 (doc/10).

use super::Platform;

mod clock;
mod display;
// The EMAC (doc/58 R2); only boards with Ethernet use it.
#[cfg_attr(not(feature = "board-p4evb"), allow(dead_code))]
mod emac;
mod entropy;
mod i2c;
mod kbd;
mod membench;
mod ppa;
mod psram;
mod sd;
mod sdio;
// The board (doc/58 §3.1): what is wired to the chip.
#[cfg(feature = "board-p4evb")]
mod p4evb;
#[cfg(feature = "board-p4evb")]
use p4evb as board;
#[cfg(not(feature = "board-p4evb"))]
mod tab5;
#[cfg(not(feature = "board-p4evb"))]
use tab5 as board;

// USB Serial/JTAG (doc/06 §3.1): usb_serial_jtag_reg.h
/// The RNG (`LP_SYSTEM_REG_RNG_DATA_REG`); random with the entropy source
/// running (doc/47).
pub(super) const RNG_DATA: usize = 0x5011_01A4;
const RNG_WORD_US: u64 = 56;
const USJ_BASE: usize = 0x500D_2000;
const USJ_EP1: usize = USJ_BASE;
const USJ_EP1_CONF: usize = USJ_BASE + 0x4;
const USJ_WR_DONE: u32 = 1 << 0;
const USJ_SERIAL_IN_EP_DATA_FREE: u32 = 1 << 1;
/// EP1_CONF.SERIAL_OUT_EP_DATA_AVAIL: the host sent bytes (usb_serial_jtag_reg.h:50).
const USJ_SERIAL_OUT_EP_DATA_AVAIL: u32 = 1 << 2;
const USJ_INT_ENA: usize = USJ_BASE + 0x10;
const USJ_INT_CLR: usize = USJ_BASE + 0x14;
/// SERIAL_OUT_RECV_PKT: the host sent a USB packet (usb_serial_jtag_reg.h:319,439).
const USJ_OUT_RECV_PKT_INT: u32 = 1 << 2;
/// ETS_USB_SERIAL_JTAG_INTR_SOURCE (interrupts.h), routed to CPU line 3.
const USJ_SOURCE: usize = 22;
const USJ_IRQ: usize = 3 + CLIC_EXT_INTR_NUM_OFFSET;
/// ETS_SDIO_HOST_INTR_SOURCE: the SDMMC host, whose only enabled interrupt
/// is the C6's SDIO card interrupt (doc/22 §8), routed to CPU line 4.
const SDIO_SOURCE: usize = 23;
// Core1's clock and reset (hp_sys_clkrst_reg.h:147,179,3064,3124) and the
// ROM call that tells it where to start (esp32p4.rom.ld:57; doc/32 §1).
const HP_SYS_CLKRST_SOC_CLK_CTRL0: usize = 0x500E_6014;
const CORE1_CPU_CLK_EN: u32 = 1 << 4;
const HP_SYS_CLKRST_HP_RST_EN0: usize = 0x500E_60C0;
const RST_EN_CORE1_GLOBAL: u32 = 1 << 8;
/// PMU's software stall of each core (pmu_reg.h: CPU_SW_STALL, core 1's
/// code in [23:16]; 0x86 stalls, 0xFF releases), and whether core 1 is
/// stalled (hp_system_reg.h: CPU_CORESTALLED_ST bit 1).
const PMU_CPU_SW_STALL: usize = 0x5011_5200;
const CORE1_STALL_S: u32 = 16;
const HP_SYSTEM_CPU_CORESTALLED_ST: usize = 0x500E_5064;
const ROM_SET_APPCPU_BOOT_ADDR: usize = 0x4FC0_00A8;
const SDIO_IRQ: usize = 4 + CLIC_EXT_INTR_NUM_OFFSET;
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
// Core 1's alarm: comparator 1 (systimer_reg.h:42,216,228,290,402,461),
// routed through core 1's interrupt matrix to the same CLIC line (doc/33).
const SYSTIMER_TARGET1_WORK_EN: u32 = 1 << 23;
const SYSTIMER_TARGET1_HI: usize = SYSTIMER_BASE + 0x24;
const SYSTIMER_TARGET1_LO: usize = SYSTIMER_BASE + 0x28;
const SYSTIMER_TARGET1_CONF: usize = SYSTIMER_BASE + 0x38;
const SYSTIMER_COMP1_LOAD: usize = SYSTIMER_BASE + 0x54;
const SYSTIMER_TARGET1_INT: u32 = 1 << 1;
const SYSTIMER_TARGET1_SOURCE: usize = 54;
const INTMTX_CORE1_BASE: usize = 0x500D_6800;
/// CPU interrupt line used for the scheduler tick.
const TIMER_CPU_LINE: usize = 1;
const CLIC_EXT_INTR_NUM_OFFSET: usize = 16;
const TIMER_CLIC_INDEX: usize = TIMER_CPU_LINE + CLIC_EXT_INTR_NUM_OFFSET;
// Interrupts between the cores (doc/32 §1, doc/33 §10): writing 1 to
// HP_SYSTEM CPU_INT_FROM_CPU_n raises source FROM_CPU_INTRn until 0 is
// written (hp_system_reg.h:37-60, crosscore_int_ll.h). As in ESP-IDF, n is
// the core it is for: source 79 goes to core 0, 80 to core 1
// (interrupts.h:100-101), both to CPU line 5.
const HP_SYSTEM_CPU_INT_FROM_CPU_0: usize = 0x500E_5010;
const FROM_CPU_INTR0_SOURCE: usize = 79;
const IPI_CPU_LINE: usize = 5;
const IPI_CLIC_INDEX: usize = IPI_CPU_LINE + CLIC_EXT_INTR_NUM_OFFSET;

// CLIC, rev v1.x layout (soc/esp32p4/include/soc/clic_reg.h, doc/01 §4.1).
// One word per interrupt: [31:24] ctl (priority in the top NLBITS=3 bits),
// [23:16] attr (mode RO, trig, shv), [8] ie, [0] ip.
const CLIC_INT_THRESH: usize = 0x2080_0008;
const CLIC_CTRL_BASE: usize = 0x2080_1000;
/// Priority 1 (ctl = 1 << 5 | 0x1F), level-triggered, non-vectored, enabled.
const CLIC_TIMER_CTRL: u32 = (0x3F << 24) | (1 << 8);
const CLIC_IP: u32 = 1 << 0;

/// Threshold 0: every enabled level gets through. v3 keeps the threshold in
/// the standard `mintthresh` CSR (0x347, riscv/csr_clic.h) instead.
unsafe fn clic_open() {
    unsafe {
        wr(CLIC_INT_THRESH, 0);
        #[cfg(feature = "chip-v3")]
        core::arch::asm!("csrw 0x347, zero");
    }
}

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
/// XIP text of a process slot at XIP_TEXT + the slot's offset in the PSRAM
/// window (flash MMU entries 512-1023, 32 MiB); its data at the same offset
/// in the PSRAM window's second alias, 0x4A00_0000 + offset, which is the
/// slot itself (doc/15 §2, doc/30 §3).
const XIP_TEXT: usize = 0x4200_0000;
const XIP_SPAN: usize = 32 << 20;
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

// esp32p4.rom.ld:180-182, 194; rom/cache.h:97-110.
const ROM_CACHE_GET_L2_LINE_SIZE: usize = 0x4FC0_03CC;
const ROM_CACHE_SET_L2_MODE: usize = 0x4FC0_03D4;
const ROM_CACHE_INVALIDATE_ALL: usize = 0x4FC0_0404;
const CACHE_SIZE_128K: u32 = 9;
const CACHE_LINE_SIZE_128B: u32 = 4;
/// L2 line size set at boot (doc/43 §3). The bootloader leaves 64 bytes
/// (boot/sdkconfig: CONFIG_CACHE_L2_CACHE_LINE_64B).
pub const L2_LINE: u32 = 128;

/// Sets the L2 cache to 128 KiB, 8 ways, `L2_LINE`-byte lines and empties
/// it, as ESP-IDF's app startup does (cpu_start.c:467-487, cache_hal.c:58-
/// 77). Only at boot, before the PSRAM is up: L2 then holds clean flash
/// lines only. Runs from RAM: the flash window goes through L2.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
unsafe fn set_l2_line() {
    unsafe {
        let set_mode: unsafe extern "C" fn(u32, u32, u32) = core::mem::transmute(ROM_CACHE_SET_L2_MODE);
        let invalidate_all: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_CACHE_INVALIDATE_ALL);
        let writeback_all: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_CACHE_WRITEBACK_ALL);
        let mhcr: usize;
        core::arch::asm!("csrrc {0}, 0x7c1, {1}", out(reg) mhcr, in(reg) MHCR_BRANCH_PREDICTION);
        writeback_all(CACHE_MAP_L1_DCACHE);
        set_mode(CACHE_SIZE_128K, 8, CACHE_LINE_SIZE_128B);
        invalidate_all(CACHE_MAP_L2_CACHE);
        core::arch::asm!("csrw 0x7c1, {0}", in(reg) mhcr);
    }
}

/// The L2 line size in bytes.
pub fn l2_line() -> u32 {
    let get: unsafe extern "C" fn() -> u32 = unsafe { core::mem::transmute(ROM_CACHE_GET_L2_LINE_SIZE) };
    unsafe { get() }
}

// ROM SPI flash functions (esp32p4.rom.ld:123-125 for rev v1.x; the eco5 file
// is for v3), and the pointer to the chip parameters the bootloader set with
// esp_rom_spiflash_config_param (esp32p4.rom.ld:169, esp_rom_spiflash.h:361).
// They drive SPI1 and wait until the chip is done. There is no auto-suspend
// on the P4, so SPI0 (the cache) must not touch flash meanwhile (doc/14 §5.2).
const ROM_SPIFLASH_ERASE_SECTOR: usize = 0x4FC0_014C;
const ROM_SPIFLASH_WRITE: usize = 0x4FC0_0154;
#[cfg(not(feature = "chip-v3"))]
const ROM_SPIFLASH_LEGACY_DATA: usize = 0x4FF3_FFE8;
#[cfg(feature = "chip-v3")]
const ROM_SPIFLASH_LEGACY_DATA: usize = 0x4FFB_FFE8;
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
pub(super) fn cache_invalidate(addr: usize, len: usize, l2: bool) {
    unsafe {
        let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
        invalidate(CACHE_MAP_L1_ICACHE_MASK, addr as u32, len as u32);
        invalidate(CACHE_MAP_L1_DCACHE, addr as u32, len as u32);
        if l2 {
            invalidate(CACHE_MAP_L2_CACHE, addr as u32, len as u32);
        }
    }
}

/// Invalidates `[addr, addr + len)` in L1 D only: memory a DMA wrote that
/// is never run as code (the EMAC's descriptors and buffers, doc/58 R5).
/// Half the ROM calls of `cache_invalidate`.
pub(super) fn dcache_invalidate(addr: usize, len: usize) {
    unsafe {
        let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
        invalidate(CACHE_MAP_L1_DCACHE, addr as u32, len as u32);
    }
}

/// Writes `[addr, addr + len)` back from L1 D, then from L2 if `l2`
/// (cache_ll.h:644-693: the levels are written back in order).
pub(super) fn cache_writeback(addr: usize, len: usize, l2: bool) {
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

/// Each core's alarm time (written by that core only).
static mut ALARM_AT: [u64; 2] = [u64::MAX; 2];

/// This core's alarm comparator: (CONF work-enable bit, interrupt bit,
/// target hi, target lo, load register).
fn alarm_regs() -> (u32, u32, usize, usize, usize) {
    if crate::smp::hartid() == 0 {
        (SYSTIMER_TARGET0_WORK_EN, SYSTIMER_TARGET0_INT, SYSTIMER_TARGET0_HI, SYSTIMER_TARGET0_LO, SYSTIMER_COMP0_LOAD)
    } else {
        (SYSTIMER_TARGET1_WORK_EN, SYSTIMER_TARGET1_INT, SYSTIMER_TARGET1_HI, SYSTIMER_TARGET1_LO, SYSTIMER_COMP1_LOAD)
    }
}

unsafe fn rd(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

unsafe fn wr(addr: usize, v: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

impl Platform for Esp32p4v1 {
    #[cfg(not(feature = "chip-v3"))]
    const NAME: &'static str = "esp32p4-v1.x";
    #[cfg(feature = "chip-v3")]
    const NAME: &'static str = "esp32p4-v3.x";
    const MTVEC_MODE: usize = 3;
    // Upper L2MEM below the 128 KiB L2 cache (doc/02 §1.1, doc/07 §3), but
    // the kernel's .bss below it (RAM_HI, doc/30 §2). Apps only use it when
    // there is no PSRAM.
    const APP_POOL: core::ops::Range<usize> = 0x4FF8_0000..0x4FFA_0000;
    const PROTECTED_MMIO: usize = USJ_BASE;
    const SERVE: bool = true;
    const TIMER_HZ: u64 = 16_000_000;
    const TIMER_IRQ: usize = TIMER_CLIC_INDEX;
    const IPI_IRQ: usize = IPI_CLIC_INDEX;

    fn start_secondary(entry: usize) -> bool {
        unsafe extern "C" {
            static __stack1_bottom: u8;
            static __stack1_top: u8;
        }
        // Core 1's stack is where the bootloader ran (link-esp32p4v1.ld).
        // The ROM's flash parameters, which the flash writes use, must not
        // be there.
        let stack1 = core::ptr::addr_of!(__stack1_bottom) as usize..core::ptr::addr_of!(__stack1_top) as usize;
        let chip = unsafe { rd(ROM_SPIFLASH_LEGACY_DATA) } as usize;
        if stack1.contains(&chip) {
            t5_abi::event!(Error, "smp.stack1_in_use", rom_flash_chip = t5_abi::log::Addr(chip as u32));
            return false;
        }
        unsafe {
            let set_boot: unsafe extern "C" fn(u32) = core::mem::transmute(ROM_SET_APPCPU_BOOT_ADDR);
            // Core 1 may still run from before a reset that did not stop it
            // (a panic, or a kernel older than `stop_secondary`), and the
            // ROM may still hold the old entry. Clear the entry and restart
            // core 1 through the ROM, which then waits for the new one
            // (doc/33 §8).
            set_boot(0);
            // Released from a software stall first, as ESP-IDF does
            // (esp_cpu_unstall(1) in start_other_core): on the EVB's chip
            // v1.3 core 1 did not come up without it (doc/58).
            let stall = rd(PMU_CPU_SW_STALL);
            let stalled = rd(HP_SYSTEM_CPU_CORESTALLED_ST);
            t5_abi::event!(Info, "smp.core1_stall", code = (stall >> CORE1_STALL_S) & 0xFF, stalled = stalled & 2 != 0);
            wr(PMU_CPU_SW_STALL, (stall & !(0xFF << CORE1_STALL_S)) | (0xFF << CORE1_STALL_S));
            wr(HP_SYS_CLKRST_HP_RST_EN0, rd(HP_SYS_CLKRST_HP_RST_EN0) | RST_EN_CORE1_GLOBAL);
            wr(HP_SYS_CLKRST_SOC_CLK_CTRL0, rd(HP_SYS_CLKRST_SOC_CLK_CTRL0) | CORE1_CPU_CLK_EN);
            wr(HP_SYS_CLKRST_HP_RST_EN0, rd(HP_SYS_CLKRST_HP_RST_EN0) & !RST_EN_CORE1_GLOBAL);
            set_boot(entry as u32);
        }
        true
    }

    fn init_secondary() {
        unsafe {
            // Core1's alarm (comparator 1, one-shot on unit 0) to its CLIC,
            // on the line core 0's uses; let every enabled level through.
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) & !SYSTIMER_TARGET1_WORK_EN);
            wr(SYSTIMER_INT_CLR, SYSTIMER_TARGET1_INT);
            wr(SYSTIMER_TARGET1_CONF, 0);
            wr(SYSTIMER_INT_ENA, rd(SYSTIMER_INT_ENA) | SYSTIMER_TARGET1_INT);
            wr(INTMTX_CORE1_BASE + 4 * SYSTIMER_TARGET1_SOURCE, TIMER_CLIC_INDEX as u32);
            wr(CLIC_CTRL_BASE + 4 * TIMER_CLIC_INDEX, CLIC_TIMER_CTRL);
            Self::ipi_clear();
            wr(INTMTX_CORE1_BASE + 4 * (FROM_CPU_INTR0_SOURCE + 1), IPI_CLIC_INDEX as u32);
            wr(CLIC_CTRL_BASE + 4 * IPI_CLIC_INDEX, CLIC_TIMER_CTRL);
            clic_open();
            if L2_LINE == 128 {
                set_l2_line();
            }
        }
    }

    fn ipi_send(hart: usize) {
        unsafe { wr(HP_SYSTEM_CPU_INT_FROM_CPU_0 + 4 * hart, 1) };
    }

    fn ipi_clear() {
        unsafe {
            wr(HP_SYSTEM_CPU_INT_FROM_CPU_0 + 4 * (crate::smp::hartid() & 1), 0);
            // As for the alarm, the clear reaches the CLIC a little later.
            let mut n = 0;
            while rd(CLIC_CTRL_BASE + 4 * IPI_CLIC_INDEX) & CLIC_IP != 0 && n < 256 {
                n += 1;
            }
        }
    }

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
            // Console input: routed now, enabled at the source by `console_wake`.
            wr(USJ_INT_ENA, rd(USJ_INT_ENA) & !USJ_OUT_RECV_PKT_INT);
            wr(USJ_INT_CLR, USJ_OUT_RECV_PKT_INT);
            wr(INTMTX_CORE0_BASE + 4 * USJ_SOURCE, USJ_IRQ as u32);
            wr(CLIC_CTRL_BASE + 4 * USJ_IRQ, CLIC_TIMER_CTRL);
            // The SDMMC host: routed now, unmasked in the host by `sdio`.
            wr(INTMTX_CORE0_BASE + 4 * SDIO_SOURCE, SDIO_IRQ as u32);
            wr(CLIC_CTRL_BASE + 4 * SDIO_IRQ, CLIC_TIMER_CTRL);
            // From core 1: routed, cleared.
            Self::ipi_clear();
            wr(INTMTX_CORE0_BASE + 4 * FROM_CPU_INTR0_SOURCE, IPI_CLIC_INDEX as u32);
            wr(CLIC_CTRL_BASE + 4 * IPI_CLIC_INDEX, CLIC_TIMER_CTRL);
            clic_open();
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
        // Each core has its comparator (doc/33). SYSTIMER_CONF is shared:
        // both cores change it only with the kernel lock held.
        let (en, int, hi, lo, load) = alarm_regs();
        unsafe {
            ALARM_AT[crate::smp::hartid() & 1] = at;
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) & !en);
            wr(SYSTIMER_INT_CLR, int);
            wr(hi, (at >> 32) as u32);
            wr(lo, at as u32);
            wr(load, 1);
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) | en);
        }
    }

    fn clear_alarm() {
        let (en, int, ..) = alarm_regs();
        unsafe {
            ALARM_AT[crate::smp::hartid() & 1] = u64::MAX;
            wr(SYSTIMER_CONF, rd(SYSTIMER_CONF) & !en);
            wr(SYSTIMER_INT_CLR, int);
            // The clear reaches this core's CLIC a little later; going back
            // to U-mode before then traps again at once (doc/33 §8).
            let mut n = 0;
            while rd(CLIC_CTRL_BASE + 4 * TIMER_CLIC_INDEX) & CLIC_IP != 0 && n < 256 {
                n += 1;
            }
        }
    }

    fn alarm_due() -> bool {
        Self::now() >= unsafe { ALARM_AT[crate::smp::hartid() & 1] }
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

    fn console_read(buf: &mut [u8]) -> usize {
        // Clear first: a packet arriving after the reads below raises the
        // interrupt again, so the idle wait cannot miss it.
        unsafe { wr(USJ_INT_CLR, USJ_OUT_RECV_PKT_INT) };
        let mut n = 0;
        while n < buf.len() && unsafe { rd(USJ_EP1_CONF) } & USJ_SERIAL_OUT_EP_DATA_AVAIL != 0 {
            buf[n] = unsafe { rd(USJ_EP1) } as u8;
            n += 1;
        }
        n
    }

    fn console_pending() -> bool {
        unsafe { rd(USJ_EP1_CONF) & USJ_SERIAL_OUT_EP_DATA_AVAIL != 0 }
    }

    fn console_wake(on: bool) {
        unsafe {
            wr(USJ_INT_CLR, USJ_OUT_RECV_PKT_INT);
            let ena = rd(USJ_INT_ENA) & !USJ_OUT_RECV_PKT_INT;
            wr(USJ_INT_ENA, if on { ena | USJ_OUT_RECV_PKT_INT } else { ena });
        }
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
        crate::smp::quiesce();
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
        // As esp_restart_noos does (system_internal.c:113-146): hold core 1
        // in reset and clear its entry, so that after the reset it waits in
        // the ROM for the next kernel instead of jumping to this one's.
        unsafe {
            wr(HP_SYS_CLKRST_HP_RST_EN0, rd(HP_SYS_CLKRST_HP_RST_EN0) | RST_EN_CORE1_GLOBAL);
            let set_boot: unsafe extern "C" fn(u32) = core::mem::transmute(ROM_SET_APPCPU_BOOT_ADDR);
            set_boot(0);
        }
        // esp_rom_software_reset_system (esp32p4.rom.ld:52).
        unsafe {
            let reset: unsafe extern "C" fn() -> ! = core::mem::transmute(ROM_SOFTWARE_RESET);
            reset()
        }
    }

    fn flash_offset(addr: usize) -> Option<u32> {
        (FLASH_WINDOW..FLASH_WINDOW + FLASH_SIZE).contains(&addr).then(|| (addr - FLASH_WINDOW) as u32)
    }

    fn map_xip(base: usize, capacity: usize, flash_offset: u32, pages: usize) -> Option<usize> {
        let psram = unsafe { PSRAM_SIZE };
        let offset = base.checked_sub(psram::WINDOW)?;
        let per_slot = capacity / MMU_PAGE;
        if flash_offset as usize % MMU_PAGE != 0
            || offset % MMU_PAGE != 0
            || pages > per_slot
            || offset + capacity > psram.min(XIP_SPAN)
        {
            return None;
        }
        let vbase = XIP_TEXT + offset;
        let first = (vbase - FLASH_WINDOW_BASE) / MMU_PAGE;
        crate::smp::quiesce();
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
        crate::smp::quiesce();
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
        crate::smp::quiesce();
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

    fn membench(buf: usize, pause_display: bool) -> Result<u32, &'static str> {
        membench::bench(buf, pause_display)
    }

    fn ppa_bench(buf: usize) -> Result<u32, &'static str> {
        membench::ppa_bench(buf)
    }

    fn gfx_start(req: super::Gfx) -> Result<(), &'static str> {
        let fb = display::buffer().ok_or("no display")?;
        let (w, h) = (display::WIDTH as u32, display::HEIGHT as u32);
        let at = |x: u16, y: u16| ppa::Block { base: fb, width: w, height: h, x: x as u32, y: y as u32 };
        // Whole rows: the rows the hardware reads are written back; the
        // ones it writes are written back and dropped, so that nothing the
        // cache holds is written over its result later.
        let rows = |y: u16, n: u16| (fb + y as usize * w as usize * 2, n as usize * w as usize * 2);
        if ppa::busy() {
            return Err("busy");
        }
        match req {
            super::Gfx::Fill { x, y, w: bw, h: bh, rgb565 } => {
                let (a, n) = rows(y, bh);
                cache_writeback(a, n, true);
                cache_invalidate(a, n, true);
                ppa::start_fill(&at(x, y), bw as u32, bh as u32, rgb565)
            }
            super::Gfx::Copy { sx, sy, dx, dy, w: bw, h: bh } => {
                let (a, n) = rows(sy, bh);
                cache_writeback(a, n, true);
                let (a, n) = rows(dy, bh);
                cache_writeback(a, n, true);
                cache_invalidate(a, n, true);
                ppa::start_copy(&at(dx, dy), &at(sx, sy), bw as u32, bh as u32)
            }
        }
    }

    fn rng_failures() -> u32 {
        entropy::failures()
    }

    fn set_entropy(on: bool) -> Result<u32, &'static str> {
        if on {
            let distinct = entropy::enable()?;
            t5_abi::event!(Info, "rng.entropy", source = "sar_adc", distinct = distinct);
            Ok(distinct)
        } else {
            entropy::disable()?;
            t5_abi::event!(Warn, "rng.entropy_off");
            Ok(0)
        }
    }

    fn rng_dump(buf: usize, len: usize, raw: Option<u32>) -> Result<u32, &'static str> {
        let start = Self::now();
        for i in 0..len / 4 {
            let v = match raw {
                Some(us) => {
                    let t = Self::now();
                    while Self::now() - t < us as u64 * 16 {}
                    unsafe { rd(RNG_DATA) }
                }
                None => Self::random_u32(),
            };
            unsafe { wr(buf + 4 * i, v) };
            if i % 4096 == 0 {
                Self::feed_watchdog();
                display::service();
            }
            // At 56 µs a word this takes half a minute; show it is alive.
            if i % (64 * 1024) == 0 {
                t5_abi::event!(Debug, "rng.dump_progress", words = i as u32);
            }
        }
        // The probe reads memory past the caches.
        cache_writeback(buf, len, true);
        let ms = (Self::now() - start) / 16_000;
        t5_abi::event!(Info, "rng.dump", addr = t5_abi::log::Addr(buf as u32), bytes = len as u32, raw = raw.is_some(), us = raw.unwrap_or(0), ms = ms as u32);
        Ok(buf as u32)
    }

    fn random_u32() -> u32 {
        // As esp_random (hw_random.c:49-80): XOR the register over the time
        // ESP-IDF waits between words on the P4, 14 * 360 CPU cycles times
        // the CPU/APB ratio (4), 56 µs (its tested rate, about 75 kB/s),
        // and the timer. Random once `clock_init` has started the SAR ADC
        // (doc/47).
        let start = Self::now();
        let mut r = start as u32;
        while Self::now() - start < RNG_WORD_US * 16 {
            let v = unsafe { rd(RNG_DATA) };
            entropy::check(v);
            r ^= v;
        }
        r ^ (Self::now() as u32).rotate_left(16)
    }

    fn gfx_finished() -> (u32, u32) {
        (ppa::finished(), ppa::failed())
    }

    fn gfx_busy() -> bool {
        ppa::busy()
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
        // The RNG's entropy source (doc/47), whatever the clock does.
        match entropy::enable() {
            Ok(distinct) => t5_abi::event!(Info, "rng.entropy", source = "sar_adc", distinct = distinct),
            Err(why) => t5_abi::event!(Warn, "rng.no_entropy", error = why),
        }
        // PVT first, then the clock, as ESP-IDF's pmu_init and esp_clk_init
        // do (doc/research/m4-psram-and-clock.md §10).
        // On v3 ESP-IDF uses PVT only with eFuse block versions 2..99 and
        // otherwise runs 400 MHz on the PMU's default dbias (24,
        // pmu_param.c:332-350), which the bootloader leaves.
        if !clock::pvt_on() && !cfg!(feature = "chip-v3") {
            t5_abi::event!(Warn, "clock.no_pvt");
            return;
        }
        let mhz = clock::cpu_full();
        t5_abi::event!(Info, "clock.cpu", mhz = mhz.unwrap_or(0), measured_khz = cpu_khz());
        clock::log_voltages("cpu_full");
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
        t5_abi::event!(Info, "board.name", name = board::NAME);
        board::init()
    }

    fn kbd_poll(sink: &mut dyn FnMut(u8, u8)) -> Result<(), &'static str> {
        // Not on the EVB: its GPIO50 (the keyboard's INT on the Tab5) is
        // the Ethernet's REF_CLK.
        if !board::KBD {
            return Err("no keyboard port on this board");
        }
        kbd::poll(sink)
    }

    fn touch_read(out: &mut [super::TouchPoint; 5]) -> Result<usize, &'static str> {
        board::touch_read(out)
    }

    fn display_init(panel: super::Panel, fb: usize) -> Result<usize, &'static str> {
        // Drawing at 90° and 270° writes the framebuffer in strides, which
        // autoload makes slower (doc/43 §4): keep it to the memory below.
        psram::autoload(fb - psram::WINDOW);
        display::init(panel, fb).map(|_| display::FB_BYTES)
    }

    fn display_service() {
        display::service();
        // A missed interrupt, or the timeout.
        if ppa::busy() {
            let _ = ppa::poll();
        }
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
        if code == ppa::IRQ {
            let _ = ppa::poll();
            return true;
        }
        if code == USJ_IRQ {
            // Woken for console input; the scheduler loop reads it.
            unsafe { wr(USJ_INT_CLR, USJ_OUT_RECV_PKT_INT) };
            return true;
        }
        if code == SDIO_IRQ {
            // Woken by the C6; the scheduler loop reads from it.
            sdio::on_irq();
            return true;
        }
        if code == emac::IRQ {
            // A frame came; the scheduler loop reads the ring.
            emac::on_irq();
            return true;
        }
        false
    }

    fn eth_up() -> Result<u64, &'static str> {
        if !board::ETH {
            return Err("no Ethernet on this board");
        }
        emac::up(unsafe { PSRAM_SIZE } != 0)
    }

    fn eth_recv(out: &mut [u8]) -> Option<usize> {
        if board::ETH { emac::recv(out) } else { None }
    }

    fn eth_pending() -> bool {
        board::ETH && emac::pending()
    }

    fn eth_send(frame: &[u8]) -> Result<(), &'static str> {
        if board::ETH { emac::send(frame) } else { Err("no Ethernet on this board") }
    }

    fn eth_link() -> usize {
        if board::ETH { emac::link() } else { 0 }
    }

    fn eth_stats() -> (u32, u32, u32, u32) {
        if board::ETH { emac::stats() } else { (0, 0, 0, 0) }
    }

    fn eth_ring_area() -> Option<usize> {
        (board::ETH && unsafe { PSRAM_SIZE } != 0).then_some(emac::RINGS)
    }

    fn eth_rx_errors() -> [u32; 5] {
        if board::ETH { emac::rx_errors() } else { [0; 5] }
    }

    fn wlan_probe() -> Result<u32, &'static str> {
        sdio::probe()
    }

    fn wlan_up() -> Result<(), &'static str> {
        sdio::up().map(|_| ())
    }

    fn wlan_poll(sink: &mut dyn FnMut(u8, &[u8])) -> Result<(), &'static str> {
        sdio::poll(sink)
    }

    fn wlan_send(if_type: u8, pkt_type: u8, payload: &[u8]) -> Result<(), &'static str> {
        sdio::send(if_type, pkt_type, payload)
    }

    fn wlan_pending() -> bool {
        sdio::pending()
    }

    fn wlan_tx_idle() -> bool {
        !sd::write_busy()
    }

    fn wlan_irq_stats() -> (u32, u32, u32, u32) {
        sdio::irq_stats()
    }

    fn sd_info() -> Result<u32, &'static str> {
        sd::info()
    }

    fn sd_sectors() -> Result<u32, &'static str> {
        sd::init().map(|c| c.sectors)
    }

    fn sd_read(lba: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        sd::read_blocks(lba, buf)
    }

    fn sd_write(lba: u32, data: &[u8]) -> Result<(), &'static str> {
        sd::write_blocks(lba, data)
    }

    fn sd_test() -> Result<u32, &'static str> {
        sd::test()
    }

    fn sd_dump(lba: u32) -> Result<u32, &'static str> {
        sd::dump(lba)
    }

    fn i2c_scan() -> Result<u32, &'static str> {
        if !board::I2C {
            return Err("no I2C bus on this board");
        }
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
