//! ESP32-P4 rev v1.x (M5Stack Tab5). Register addresses come from ESP-IDF 6.0
//! `components/soc/esp32p4/register/hw_ver1` (doc/06 §2, §3.1).
//!
//! Untested on hardware until M1a.

use super::Platform;

// USB Serial/JTAG (doc/06 §3.1): usb_serial_jtag_reg.h
const USJ_BASE: usize = 0x500D_2000;
const USJ_EP1: usize = USJ_BASE;
const USJ_EP1_CONF: usize = USJ_BASE + 0x4;
const USJ_WR_DONE: u32 = 1 << 0;
const USJ_SERIAL_IN_EP_DATA_FREE: u32 = 1 << 1;
/// Polls before giving up on a byte, so output does not hang when no host has
/// the CDC port open.
const USJ_TIMEOUT_POLLS: u32 = 50_000;

// LP_WDT / RWDT (doc/06 §2): lp_wdt_reg.h, lpwdt_ll.h
const LP_WDT_BASE: usize = 0x5011_6000;
const LP_WDT_CONFIG0: usize = LP_WDT_BASE;
const LP_WDT_WPROTECT: usize = LP_WDT_BASE + 0x18;
const LP_WDT_WKEY: u32 = 0x50D8_3AA1;
const LP_WDT_EN: u32 = 1 << 31;

// Timer group MWDTs: timer_group_reg.h
const TIMG_BASES: [usize; 2] = [0x500C_2000, 0x500C_3000];
const TIMG_WDTCONFIG0: usize = 0x48;
const TIMG_WDTWPROTECT: usize = 0x64;
const TIMG_WDT_WKEY: u32 = 0x50D8_3AA1;
const TIMG_WDT_EN: u32 = 1 << 31;

// ROM cache functions (esp_rom/esp32p4/ld/esp32p4.rom.ld,
// esp_rom/esp32p4/include/esp32p4/rom/cache.h). ESP-IDF uses these rather than
// fence.i to make written code fetchable (doc/02 §2.2).
const ROM_CACHE_INVALIDATE_ADDR: usize = 0x4FC0_03E4;
const ROM_CACHE_WRITEBACK_ADDR: usize = 0x4FC0_03F4;
const CACHE_MAP_L1_ICACHE_MASK: u32 = (1 << 0) | (1 << 1);
const CACHE_MAP_L1_DCACHE: u32 = 1 << 4;

type RomCacheFn = unsafe extern "C" fn(map: u32, addr: u32, size: u32) -> i32;

pub struct Esp32p4v1;

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

#[used]
#[unsafe(link_section = ".flash.appdesc")]
static APP_DESC: AppDesc = AppDesc {
    magic_word: 0xABCD_5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: cstr(env!("CARGO_PKG_VERSION")),
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

    fn init() {
        unsafe {
            // The 2nd stage bootloader leaves RWDT running (9 s by default).
            wr(LP_WDT_WPROTECT, LP_WDT_WKEY);
            wr(LP_WDT_CONFIG0, rd(LP_WDT_CONFIG0) & !LP_WDT_EN);
            wr(LP_WDT_WPROTECT, 0);
            for base in TIMG_BASES {
                wr(base + TIMG_WDTWPROTECT, TIMG_WDT_WKEY);
                wr(base + TIMG_WDTCONFIG0, rd(base + TIMG_WDTCONFIG0) & !TIMG_WDT_EN);
                wr(base + TIMG_WDTWPROTECT, 0);
            }
        }
    }

    fn console_write(bytes: &[u8]) {
        for &b in bytes {
            let mut polls = 0;
            unsafe {
                while rd(USJ_EP1_CONF) & USJ_SERIAL_IN_EP_DATA_FREE == 0 {
                    polls += 1;
                    if polls >= USJ_TIMEOUT_POLLS {
                        return;
                    }
                }
                wr(USJ_EP1, b as u32);
            }
        }
        unsafe { wr(USJ_EP1_CONF, USJ_WR_DONE) };
    }

    fn sync_code(addr: usize, len: usize) {
        // L2MEM is only behind the L1 caches. PSRAM/flash would also need the
        // L2 cache (CACHE_MAP_L2_CACHE) once apps are loaded there (M4).
        unsafe {
            let writeback: RomCacheFn = core::mem::transmute(ROM_CACHE_WRITEBACK_ADDR);
            let invalidate: RomCacheFn = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
            writeback(CACHE_MAP_L1_DCACHE, addr as u32, len as u32);
            invalidate(CACHE_MAP_L1_ICACHE_MASK, addr as u32, len as u32);
            core::arch::asm!("fence.i");
        }
    }

    fn exit(_code: u32) -> ! {
        loop {
            unsafe { core::arch::asm!("wfi") };
        }
    }
}
