//! The PPA's blending engine and the 2D-DMA (doc/44): rectangle fill,
//! A8 glyphs in a fixed colour over RGB565, and rectangle copy (M2M), on
//! RGB565 images in PSRAM. Register recipe and sources:
//! doc/research/ppa-dma2d-registers.md (ESP-IDF 6.0, chip rev v1.x).
//!
//! One operation at a time. `start_*` returns once the hardware runs; the
//! output channel's interrupt (or the scheduler loop) calls `poll`. The
//! benchmark's `fill`/`glyph`/`copy` wait for the end.
//! Callers make the caches agree with memory (`cache_writeback` of what
//! the DMA reads, write back and invalidate what it writes).

use super::psram::{clear_bits, set_bits};
use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;

// Clocks and resets (§2).
const SOC_CLK_CTRL1: usize = 0x500E_6018;
const PPA_SYS_CLK_EN: u32 = 1 << 9;
const DMA2D_SYS_CLK_EN: u32 = 1 << 6;
const HP_RST_EN0: usize = 0x500E_60C0;
const RST_EN_DMA2D: u32 = 1 << 31;
const HP_RST_EN1: usize = 0x500E_60C4;
const RST_EN_PPA: u32 = 1 << 0;

// 2D-DMA (§1.2-1.3, §3).
const DMA2D: usize = 0x5008_8000;
const RST_CONF: usize = DMA2D + 0xA04;
const OUT_ARB_CONFIG: usize = DMA2D + 0xA18;
const IN_ARB_CONFIG: usize = DMA2D + 0xA1C;
const fn tx(n: usize) -> usize {
    DMA2D + 0x100 * n
}
const fn rx(n: usize) -> usize {
    DMA2D + 0x500 + 0x100 * n
}
const CONF0: usize = 0x00;
const INT_RAW: usize = 0x04;
const INT_ENA: usize = 0x08;
const INT_CLR: usize = 0x10;
const LINK_CONF: usize = 0x1C;
const LINK_ADDR: usize = 0x20;
const STATE: usize = 0x24;
const OUT_PERI_SEL: usize = 0x38;
const IN_PERI_SEL: usize = 0x3C;
const OUT_COLOR_CONVERT: usize = 0x48;
const CONF0_RST: u32 = 1 << 24;
const CONF0_CMD_DISABLE: u32 = 1 << 25;
/// eof mode, descriptor burst, 128-byte bursts, no macro blocks, page
/// bound (§3.3).
const OUT_CONF0_VALUE: u32 = 0x1706;
const IN_CONF0_VALUE: u32 = 0x1704;
const IN_MEM_TRANS_EN: u32 = 1 << 0;
const OUT_LINK_STOP: u32 = 1 << 20;
const OUT_LINK_START: u32 = 1 << 21;
const IN_LINK_AUTO_RET: u32 = 1 << 20;
const IN_LINK_STOP: u32 = 1 << 21;
const IN_LINK_START: u32 = 1 << 22;
const OUT_RESET_AVAIL: u32 = 1 << 24;
const IN_RESET_AVAIL: u32 = 1 << 23;
/// No colour conversion on a TX channel that has it (ch0-2 on rev v1).
const OUT_CSC_NONE: u32 = 0x3A;
const IN_SUC_EOF: u32 = 1 << 1;
/// err_eof, dscr_err, dscr_empty.
const IN_FAILED: u32 = (1 << 2) | (1 << 3) | (1 << 10);
const PERI_PPA_BG: u32 = 2;
const PERI_PPA_FG: u32 = 3;
const PERI_PPA_OUT: u32 = 2;
const PERI_M2M_TX: u32 = 4;
const PERI_M2M_RX: u32 = 3;
const PERI_NONE: u32 = 7;

// Channels: the blend's background on TX2, its foreground on TX1, the
// output on RX1; a copy on TX1 -> RX1 (M2M pairs equal numbers). RX1 has
// no colour conversion to switch off.
const TX_BG: usize = 2;
const TX_FG: usize = 1;
const RX_OUT: usize = 1;

// PPA blending engine (§1.4, §5).
const PPA: usize = 0x5008_7000;
const BLEND_COLOR_MODE: usize = PPA + 0x24;
const BLEND_BYTE_ORDER: usize = PPA + 0x2C;
const BLEND_TRANS_MODE: usize = PPA + 0x34;
const BLEND_TX_SIZE: usize = PPA + 0x3C;
const BLEND_FIX_ALPHA: usize = PPA + 0x40;
const BLEND_RGB: usize = PPA + 0x48;
const BLEND_FIX_PIXEL: usize = PPA + 0x4C;
const BLEND_RST: u32 = 1 << 4;
/// blend_en, fix pixel fill.
const TRANS_FILL: u32 = 0x05;
/// blend_en.
const TRANS_BLEND: u32 = 0x01;
/// Takes the mode the register holds. Written on its own after the mode:
/// with both in one write, the engine runs in the previous mode (doc/44 §2).
const TRANS_UPDATE: u32 = 1 << 3;
/// bg RGB565, fg A8, out RGB565.
const CM_A8_OVER_RGB565: u32 = 0x262;
/// out RGB565.
const CM_OUT_RGB565: u32 = 0x200;
/// Alpha as the images have it (A8's value); the fixed fields at reset.
const ALPHA_AS_IS: u32 = 0x8080;

const PBYTE_A8: u32 = 1;
const PBYTE_RGB565: u32 = 3;

/// An image in memory and a block of it (pixels).
#[derive(Clone, Copy)]
pub struct Block {
    pub base: usize,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
}

/// Largest image or block side the descriptors hold.
pub const MAX_SIDE: u32 = 0x3FFF;

/// Descriptors: bg, fg, out (24 bytes each, 8-byte aligned), in L2MEM and
/// written through the non-cacheable alias, as the display's list.
#[repr(C, align(64))]
struct Descriptors([[u32; 6]; 3 + 2 * MAX_STRIPS]);
static mut DESC: Descriptors = Descriptors([[0; 6]; 3 + 2 * MAX_STRIPS]);
/// Pieces a copy may be cut into (§ `start_copy`): its TX descriptors from
/// index 3, its RX ones from 3 + MAX_STRIPS.
pub const MAX_STRIPS: usize = 32;
const L2MEM_NON_CACHE: usize = 0x4000_0000;

fn desc_addr(i: usize) -> usize {
    core::ptr::addr_of!(DESC) as usize + 24 * i
}

/// Writes descriptor `i` for block `b` of `w` x `h` pixels, the last of
/// its list.
fn describe(i: usize, b: &Block, w: u32, h: u32, pbyte: u32) {
    describe_linked(i, b, w, h, pbyte, None);
}

/// Writes descriptor `i`; `next` continues the list (suc_eof only on the
/// last).
fn describe_linked(i: usize, b: &Block, w: u32, h: u32, pbyte: u32, next: Option<usize>) {
    // vb hb | en2d suc_eof owner; va ha pbyte; y x mode(single); buffer.
    let words = [
        h | w << 14 | 1 << 29 | (next.is_none() as u32) << 30 | 1 << 31,
        b.height | b.width << 14 | pbyte << 28,
        b.y | b.x << 14,
        b.base as u32,
        next.map_or(0, |n| desc_addr(n) as u32),
        0,
    ];
    let p = (desc_addr(i) + L2MEM_NON_CACHE) as *mut u32;
    for (k, &v) in words.iter().enumerate() {
        unsafe { core::ptr::write_volatile(p.add(k), v) };
    }
}

static mut READY: bool = false;

// Interrupts (soc/interrupts.h; display.rs for the CLIC setting).
const DMA2D_IN_CH1_SOURCE: usize = 105;
/// CPU line 6: 1 is the timer, 2 the display, 3 the console, 4 SDIO, 5
/// the other core (esp32p4v1.rs).
pub const IRQ: usize = 6 + 16;
const INTMTX_CORE0_BASE: usize = 0x500D_6000;
const CLIC_CTRL_BASE: usize = 0x2080_1000;
const CLIC_CTRL: u32 = (0x3F << 24) | (1 << 8);

/// Clocks on, both blocks out of reset (once).
fn init() {
    if unsafe { READY } {
        return;
    }
    // The descriptors are written through the non-cacheable alias; lines
    // of them still in L1 D (from .bss being cleared) must not be written
    // back over them later.
    let (d, n) = (core::ptr::addr_of!(DESC) as usize, core::mem::size_of::<Descriptors>());
    super::cache_writeback(d, n, false);
    super::cache_invalidate(d, n, false);
    set_bits(SOC_CLK_CTRL1, PPA_SYS_CLK_EN);
    set_bits(HP_RST_EN1, RST_EN_PPA);
    clear_bits(HP_RST_EN1, RST_EN_PPA);
    set_bits(SOC_CLK_CTRL1, DMA2D_SYS_CLK_EN);
    set_bits(HP_RST_EN0, RST_EN_DMA2D);
    clear_bits(HP_RST_EN0, RST_EN_DMA2D);
    unsafe {
        // Register clock on; pulse the AXI read and write resets.
        wr(RST_CONF, 0x4);
        wr(RST_CONF, 0x5);
        wr(RST_CONF, 0x4);
        wr(RST_CONF, 0x6);
        wr(RST_CONF, 0x4);
        wr(OUT_ARB_CONFIG, 0);
        wr(IN_ARB_CONFIG, 0);
        // The output channel's end interrupts core 0 (as the display's).
        wr(INTMTX_CORE0_BASE + 4 * DMA2D_IN_CH1_SOURCE, IRQ as u32);
        wr(CLIC_CTRL_BASE + 4 * IRQ, CLIC_CTRL);
        READY = true;
    }
}

fn spin(us: u64, done: impl Fn() -> bool) -> bool {
    let end = Esp32p4v1::now() + us * 16;
    while !done() {
        if Esp32p4v1::now() > end {
            return false;
        }
    }
    true
}

/// Stops and resets TX channel `n`, connects it to `peri`.
fn tx_connect(n: usize, peri: u32) -> Result<(), &'static str> {
    let c = tx(n);
    unsafe {
        wr(c + LINK_CONF, OUT_LINK_STOP);
        set_bits(c + CONF0, CONF0_CMD_DISABLE);
        if !spin(1000, || rd(c + STATE) & OUT_RESET_AVAIL != 0) {
            return Err("dma2d tx reset");
        }
        set_bits(c + CONF0, CONF0_RST);
        clear_bits(c + CONF0, CONF0_RST);
        wr(c + CONF0, OUT_CONF0_VALUE);
        wr(c + OUT_PERI_SEL, peri);
        wr(c + OUT_COLOR_CONVERT, OUT_CSC_NONE);
        wr(c + INT_ENA, 0);
        wr(c + INT_CLR, 0x1FFF);
    }
    Ok(())
}

/// Stops and resets RX channel `n`, connects it to `peri`.
fn rx_connect(n: usize, peri: u32, m2m: bool) -> Result<(), &'static str> {
    let c = rx(n);
    unsafe {
        wr(c + LINK_CONF, IN_LINK_STOP);
        set_bits(c + CONF0, CONF0_CMD_DISABLE);
        if !spin(1000, || rd(c + STATE) & IN_RESET_AVAIL != 0) {
            return Err("dma2d rx reset");
        }
        set_bits(c + CONF0, CONF0_RST);
        clear_bits(c + CONF0, CONF0_RST);
        wr(c + CONF0, IN_CONF0_VALUE | if m2m { IN_MEM_TRANS_EN } else { 0 });
        wr(c + IN_PERI_SEL, peri);
        clear_bits(c + LINK_CONF, IN_LINK_AUTO_RET);
        wr(c + INT_ENA, 0);
        wr(c + INT_CLR, 0x3FFF);
    }
    Ok(())
}

fn tx_start(n: usize, desc: usize) {
    unsafe {
        wr(tx(n) + LINK_ADDR, desc_addr(desc) as u32);
        wr(tx(n) + LINK_CONF, OUT_LINK_START);
    }
}

fn rx_start(n: usize, desc: usize) {
    unsafe {
        wr(rx(n) + LINK_ADDR, desc_addr(desc) as u32);
        wr(rx(n) + LINK_CONF, IN_LINK_START);
    }
}

/// The operation running: its TX channels and when it started.
struct Active {
    txs: [usize; 2],
    n_txs: usize,
    started: u64,
}
static mut ACTIVE: Option<Active> = None;
/// Operations finished, and those of them that failed.
static mut FINISHED: u32 = 0;
static mut FAILED: u32 = 0;

fn begin(txs: &[usize]) {
    let mut a = Active { txs: [0; 2], n_txs: txs.len(), started: Esp32p4v1::now() };
    a.txs[..txs.len()].copy_from_slice(txs);
    unsafe {
        wr(rx(RX_OUT) + INT_ENA, IN_SUC_EOF | IN_FAILED);
        ACTIVE = Some(a);
    }
}

pub fn busy() -> bool {
    unsafe { (*core::ptr::addr_of!(ACTIVE)).is_some() }
}

pub fn finished() -> u32 {
    unsafe { FINISHED }
}

pub fn failed() -> u32 {
    unsafe { FAILED }
}

/// Ends the running operation if its output channel is done (or it took
/// longer than `TIMEOUT_US`): resets what was used after a failure and
/// disconnects the channels. None while it runs or when nothing does.
pub fn poll() -> Option<Result<(), &'static str>> {
    let a = unsafe { (*core::ptr::addr_of!(ACTIVE)).as_ref()? };
    let txs = &a.txs[..a.n_txs];
    let c = rx(RX_OUT);
    let raw = unsafe { rd(c + INT_RAW) };
    let ended = raw & (IN_SUC_EOF | IN_FAILED) != 0;
    if !ended && Esp32p4v1::now() - a.started < TIMEOUT_US * 16 {
        return None;
    }
    let ok = ended && raw & IN_FAILED == 0;
    if !ok {
        let d = (desc_addr(2) + L2MEM_NON_CACHE) as *const u32;
        t5_abi::event!(
            Warn,
            "ppa.failed",
            rx_raw = raw,
            rx_state = unsafe { rd(c + STATE) },
            tx_state = txs.first().map_or(0, |&n| unsafe { rd(tx(n) + STATE) }),
            ppa_raw = unsafe { rd(PPA + 0x10) },
            trans = unsafe { rd(BLEND_TRANS_MODE) },
            axi_err = unsafe { rd(DMA2D + 0xA00) },
            d0 = unsafe { core::ptr::read_volatile(d) },
            d1 = unsafe { core::ptr::read_volatile(d.add(1)) }
        );
    }
    unsafe {
        wr(c + INT_ENA, 0);
        wr(c + INT_CLR, 0x3FFF);
        if !ok {
            wr(c + LINK_CONF, IN_LINK_STOP);
            for &n in txs {
                wr(tx(n) + LINK_CONF, OUT_LINK_STOP);
            }
        }
        wr(c + IN_PERI_SEL, PERI_NONE);
        clear_bits(c + CONF0, IN_MEM_TRANS_EN);
        for &n in txs {
            wr(tx(n) + INT_CLR, 0x1FFF);
            wr(tx(n) + OUT_PERI_SEL, PERI_NONE);
        }
        if !ok {
            set_bits(BLEND_TRANS_MODE, BLEND_RST);
            clear_bits(BLEND_TRANS_MODE, BLEND_RST);
        }
        ACTIVE = None;
        FINISHED = FINISHED.wrapping_add(1);
        FAILED = FAILED.wrapping_add(!ok as u32);
    }
    Some(match (ended, ok) {
        (false, _) => Err("ppa timeout"),
        (true, true) => Ok(()),
        _ => Err("dma2d error"),
    })
}

/// Polls until the running operation has ended.
pub fn wait() -> Result<(), &'static str> {
    loop {
        if let Some(r) = poll() {
            return r;
        }
        if !busy() {
            return Ok(());
        }
    }
}

fn check(b: &Block, w: u32, h: u32) -> Result<(), &'static str> {
    let fits = w > 0 && h > 0 && b.width <= MAX_SIDE && b.height <= MAX_SIDE && b.x + w <= b.width && b.y + h <= b.height;
    if fits { Ok(()) } else { Err("bad block") }
}

/// Long enough for a full-screen operation at the slowest rate measured.
const TIMEOUT_US: u64 = 200_000;

/// Starts filling `w` x `h` pixels of `out` (RGB565) with `rgb565`.
pub fn start_fill(out: &Block, w: u32, h: u32, rgb565: u16) -> Result<(), &'static str> {
    check(out, w, h)?;
    if busy() {
        return Err("busy");
    }
    init();
    describe(2, out, w, h, PBYTE_RGB565);
    rx_connect(RX_OUT, PERI_PPA_OUT, false)?;
    set_bits(BLEND_TRANS_MODE, BLEND_RST);
    clear_bits(BLEND_TRANS_MODE, BLEND_RST);
    rx_start(RX_OUT, 2);
    // The fill colour as ARGB8888; the engine cuts it to RGB565 (§5.1).
    let c = rgb565 as u32;
    let argb = 0xFF00_0000 | (c >> 11 & 0x1F) << 19 | (c >> 5 & 0x3F) << 10 | (c & 0x1F) << 3;
    unsafe {
        wr(BLEND_FIX_PIXEL, argb);
        wr(BLEND_TX_SIZE, w | h << 14);
        wr(BLEND_COLOR_MODE, CM_OUT_RGB565);
        wr(BLEND_TRANS_MODE, TRANS_FILL);
        wr(BLEND_TRANS_MODE, TRANS_FILL | TRANS_UPDATE);
    }
    begin(&[]);
    Ok(())
}

pub fn fill(out: &Block, w: u32, h: u32, rgb565: u16) -> Result<(), &'static str> {
    start_fill(out, w, h, rgb565)?;
    wait()
}

/// Draws `w` x `h` pixels of `alpha` (A8) in the colour `rgb888` over the
/// same block of `out` (RGB565), in place; waits.
pub fn glyph(out: &Block, alpha: &Block, w: u32, h: u32, rgb888: u32) -> Result<(), &'static str> {
    check(out, w, h)?;
    check(alpha, w, h)?;
    if busy() {
        return Err("busy");
    }
    init();
    set_bits(BLEND_TRANS_MODE, BLEND_RST);
    clear_bits(BLEND_TRANS_MODE, BLEND_RST);
    describe(0, out, w, h, PBYTE_RGB565);
    describe(1, alpha, w, h, PBYTE_A8);
    describe(2, out, w, h, PBYTE_RGB565);
    tx_connect(TX_BG, PERI_PPA_BG)?;
    tx_connect(TX_FG, PERI_PPA_FG)?;
    rx_connect(RX_OUT, PERI_PPA_OUT, false)?;
    tx_start(TX_BG, 0);
    tx_start(TX_FG, 1);
    rx_start(RX_OUT, 2);
    unsafe {
        wr(BLEND_COLOR_MODE, CM_A8_OVER_RGB565);
        wr(BLEND_BYTE_ORDER, 0);
        wr(BLEND_FIX_ALPHA, ALPHA_AS_IS);
        wr(BLEND_RGB, rgb888 & 0xFF_FFFF);
        wr(BLEND_TRANS_MODE, TRANS_BLEND);
        wr(BLEND_TRANS_MODE, TRANS_BLEND | TRANS_UPDATE);
    }
    begin(&[TX_BG, TX_FG]);
    wait()
}

/// Starts copying `w` x `h` pixels (RGB565) from `src` to `dst`. The
/// hardware reads ahead of what it writes, so where the two overlap and
/// the destination comes later in memory (rows down, or columns right in
/// the same rows) the copy is cut into pieces no taller (wider) than the
/// shift, taken from the far end, as one list (doc/44 §2-3).
pub fn start_copy(dst: &Block, src: &Block, w: u32, h: u32) -> Result<(), &'static str> {
    check(dst, w, h)?;
    check(src, w, h)?;
    if busy() {
        return Err("busy");
    }
    let overlap = dst.base == src.base && src.x < dst.x + w && dst.x < src.x + w && src.y < dst.y + h && dst.y < src.y + h;
    // (rows per piece, columns per piece)
    let (ph, pw) = match (overlap, dst.y > src.y, dst.y == src.y && dst.x > src.x) {
        (true, true, _) => ((dst.y - src.y).min(h), w),
        (true, false, true) => (h, (dst.x - src.x).min(w)),
        _ => (h, w),
    };
    let n = (h.div_ceil(ph) * w.div_ceil(pw)) as usize;
    if n > MAX_STRIPS {
        return Err("copy needs too many pieces");
    }
    init();
    for k in 0..n {
        // Piece k from the bottom (or the right): rows [y0, y1), columns [x0, x1).
        let (y1, x1) = if ph < h { (h - ph * k as u32, w) } else { (h, w - pw * k as u32) };
        let (y0, x0) = if ph < h { (y1.saturating_sub(ph), 0) } else { (0, x1.saturating_sub(pw)) };
        let at = |b: &Block| Block { x: b.x + x0, y: b.y + y0, ..*b };
        let next = |base: usize| (k + 1 < n).then_some(base + k + 1);
        describe_linked(3 + k, &at(src), x1 - x0, y1 - y0, PBYTE_RGB565, next(3));
        describe_linked(3 + MAX_STRIPS + k, &at(dst), x1 - x0, y1 - y0, PBYTE_RGB565, next(3 + MAX_STRIPS));
    }
    tx_connect(RX_OUT, PERI_M2M_TX)?;
    rx_connect(RX_OUT, PERI_M2M_RX, true)?;
    tx_start(RX_OUT, 3);
    rx_start(RX_OUT, 3 + MAX_STRIPS);
    begin(&[RX_OUT]);
    Ok(())
}

pub fn copy(dst: &Block, src: &Block, w: u32, h: u32) -> Result<(), &'static str> {
    start_copy(dst, src, w, h)?;
    wait()
}
