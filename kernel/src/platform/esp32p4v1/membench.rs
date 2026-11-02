//! Memory bandwidth of the PSRAM through the caches and without them, and
//! of the L1 cache, for doc/43. Runs on a free process slot (`buf`, at
//! least 2 * `BYTES`) in the kernel, interrupts off; the other core keeps
//! running its process.

use t5_abi::event;

use super::{Esp32p4v1, cache_invalidate, cache_writeback, display, psram};
use crate::platform::Platform;

/// Eight times the L2 cache (128 KiB).
pub const BYTES: usize = 1 << 20;
/// Fits in the L1 D cache (doc/02 §1.1).
const HOT: usize = 16 << 10;

// Cache autoload (psram.rs). The boot setting is saved and put back.
use psram::{L1D_AUTOLOAD, L2_AUTOLOAD};
const CACHE: usize = 0x3FF1_0000;
/// ENA, ascending, section 0; the trigger (0 miss, 1 hit, 2 miss and hit)
/// at bit 3.
const AUTOLOAD_ON: u32 = 1 | (1 << 8);

fn rd(a: usize) -> u32 {
    unsafe { core::ptr::read_volatile(a as *const u32) }
}

fn wr(a: usize, v: u32) {
    unsafe { core::ptr::write_volatile(a as *mut u32, v) }
}

/// Autoload on `[start, start + len)` in L1 D and/or L2.
fn autoload(l1: bool, l2: bool, trigger: u32, start: usize, len: usize) {
    for (on, ctrl) in [(l1, L1D_AUTOLOAD), (l2, L2_AUTOLOAD)] {
        wr(ctrl, 0);
        if on {
            wr(ctrl + 4, start as u32);
            wr(ctrl + 8, len as u32);
            wr(ctrl, AUTOLOAD_ON | trigger << 3);
        }
    }
}

/// Microseconds `f` took.
fn timed(f: impl FnOnce()) -> u64 {
    let t = Esp32p4v1::now();
    f();
    (Esp32p4v1::now() - t) / 16
}

fn write32(a: usize, n: usize) {
    let p = a as *mut u32;
    for i in (0..n / 4).step_by(8) {
        unsafe {
            for k in 0..8 {
                core::ptr::write_volatile(p.add(i + k), i as u32);
            }
        }
    }
}

fn write16(a: usize, n: usize) {
    let p = a as *mut u16;
    for i in (0..n / 2).step_by(8) {
        unsafe {
            for k in 0..8 {
                core::ptr::write_volatile(p.add(i + k), i as u16);
            }
        }
    }
}

/// The terminal's cells at 270° (doc/42): a 16 x 32 cell is 16
/// framebuffer rows (1440 bytes apart) of 64 bytes. All 22 x 80 cells,
/// row by row (`by_row`) or column by column.
fn cells(a: usize, by_row: bool) {
    let span = |r: usize, c: usize| {
        for x in 0..16 {
            let p = (a + (c * 16 + x) * 1440 + r * 64) as *mut u32;
            for k in 0..16 {
                unsafe { core::ptr::write_volatile(p.add(k), k as u32) };
            }
        }
    };
    if by_row {
        (0..22).for_each(|r| (0..80).for_each(|c| span(r, c)));
    } else {
        (0..80).for_each(|c| (0..22).for_each(|r| span(r, c)));
    }
}

fn read32(a: usize, n: usize) -> u32 {
    let p = a as *const u32;
    let mut sum = 0u32;
    for i in (0..n / 4).step_by(8) {
        unsafe {
            for k in 0..8 {
                sum = sum.wrapping_add(core::ptr::read_volatile(p.add(i + k)));
            }
        }
    }
    sum
}

fn run(buf: usize, dma: bool, pf: &str) {
    let report = |case: &str, bytes: usize, us: u64| {
        let mb_s = if us == 0 { 0 } else { bytes as u64 / us };
        event!(Info, "membench.result", case = case, dma = dma, prefetch = pf, bytes = bytes as u32, us = us as u32, mb_s = mb_s as u32);
    };
    let n = BYTES;
    let (a, b) = (buf, buf + n);
    let ua = psram::UNCACHED + (a - psram::WINDOW);
    let both = |f: &dyn Fn()| {
        cache_writeback(a, 2 * n, true);
        cache_invalidate(a, 2 * n, true);
        f();
        Esp32p4v1::feed_watchdog();
        // Interrupts are off: keep the frame ring going.
        display::service();
    };
    both(&|| {
        let w = timed(|| write32(a, n));
        let wb = timed(|| cache_writeback(a, n, true));
        report("write32", n, w);
        report("write32+wb", n, w + wb);
    });
    both(&|| report("write16+wb", n, timed(|| {
        write16(a, n);
        cache_writeback(a, n, true);
    })));
    both(&|| report("read32", n, timed(|| {
        core::hint::black_box(read32(a, n));
    })));
    both(&|| report("memcpy+wb", n, timed(|| {
        unsafe { core::ptr::copy_nonoverlapping(a as *const u8, b as *mut u8, n) };
        cache_writeback(b, n, true);
    })));
    // A scroll: the same buffer, one 32-pixel text row up.
    both(&|| report("memmove+wb", n, timed(|| {
        unsafe { core::ptr::copy((a + 40960) as *const u8, a as *mut u8, n - 40960) };
        cache_writeback(a, n, true);
    })));
    both(&|| report("cells_by_row+wb", 22 * 80 * 16 * 64, timed(|| {
        cells(a, true);
        cache_writeback(a, 1280 * 1440, true);
    })));
    both(&|| report("cells_by_col+wb", 22 * 80 * 16 * 64, timed(|| {
        cells(a, false);
        cache_writeback(a, 1280 * 1440, true);
    })));
    both(&|| report("uc_write32", n / 8, timed(|| write32(ua, n / 8))));
    both(&|| report("uc_read32", n / 8, timed(|| {
        core::hint::black_box(read32(ua, n / 8));
    })));
    // The L1 D cache, warmed first: what the core does without misses.
    core::hint::black_box(read32(a, HOT));
    report("l1_read32", 64 * HOT, timed(|| {
        for _ in 0..64 {
            core::hint::black_box(read32(a, HOT));
        }
    }));
    report("l1_write32", 64 * HOT, timed(|| {
        for _ in 0..64 {
            write32(a, HOT);
        }
    }));
    cache_writeback(a, HOT, true);
    display::service();
}

/// With the display's frame DMA running, without and with autoload; then,
/// if `pause`, with the DMA stopped.
pub fn bench(buf: usize, pause: bool) -> Result<u32, &'static str> {
    if buf < psram::WINDOW {
        return Err("no psram");
    }
    let saved = [L1D_AUTOLOAD, L2_AUTOLOAD].map(|ctrl| [0, 4, 8].map(|o| rd(ctrl + o)));
    event!(Info, "membench.cache", l1d_autoload = saved[0][0], l2_autoload = saved[1][0], l2_blocksize = rd(CACHE + 0x27C), l2_cachesize = rd(CACHE + 0x278), l2_line = super::l2_line());
    let configs = [
        ("none", false, false, 0),
        ("l2", false, true, 0),
        ("l1", true, false, 0),
        ("l1+l2", true, true, 0),
        ("l1+l2/miss+hit", true, true, 2),
    ];
    for &(pf, l1, l2, trigger) in &configs {
        autoload(l1, l2, trigger, buf, 2 * BYTES);
        run(buf, true, pf);
    }
    if pause && display::pause() {
        for &(pf, l1, l2, trigger) in &[configs[0], configs[3]] {
            autoload(l1, l2, trigger, buf, 2 * BYTES);
            run(buf, false, pf);
        }
        display::resume();
    }
    for (ctrl, regs) in [L1D_AUTOLOAD, L2_AUTOLOAD].into_iter().zip(saved) {
        wr(ctrl, 0);
        wr(ctrl + 4, regs[1]);
        wr(ctrl + 8, regs[2]);
        wr(ctrl, regs[0]);
    }
    event!(Info, "membench.done");
    Ok(BYTES as u32)
}
