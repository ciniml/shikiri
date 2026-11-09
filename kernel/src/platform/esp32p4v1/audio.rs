//! I2S0 for audio on the Tab5 (doc/70), with kumiki-p4v1-hal's driver:
//! 48 kHz 16-bit stereo, full duplex, the TX and RX rings on the AHB-GDMA.
//!
//! The kernel sets the AHB-GDMA up itself (bus clock, block reset, the
//! memory window over L2MEM) and tells kumiki so (`assume_initialized`); the
//! rings go where `main` puts them (the L2MEM the apps left for PSRAM). The
//! stream starts at boot and plays silence while nothing is pushed (the TX
//! ring starts silent; kumiki clears each played descriptor when polled).
//! The DMA's interrupt (every descriptor sent or closed, CPU line 8; `on_irq`
//! does kumiki's bookkeeping) is routed but stays off until something uses
//! the stream (stage A2): about 1,100 a second through the kernel's trap
//! path for nothing slowed the 40 MHz emulator enough to make Matter's
//! start lose its race (p4emu-run-26).
//!
//! kumiki's bookkeeping needs a poll at least once per turn of the TX ring
//! (85 ms): it counts the descriptors done from the DMA's EOF address, so
//! a longer gap loses whole turns, and it then clears and fills the wrong
//! descriptors (the last turn repeats). The kernel can be held that long:
//! loading a large app checks its signature with the lock held (140 ms on
//! the Tab5 for a 470 KB one). `fresh` restarts the stream instead (rings
//! rebuilt silent) when a gap passed 3/4 of a turn: a short dropout.
//!
//! A reset leaves the stream's DMA running (a SoC reset does not stop the
//! AHB-GDMA): `stop` before a software reset, and at boot for a reset that
//! did not go through it (a debug probe's), as the display does (doc/70
//! §9.3).
//!
//! Stage A1 without the codecs: the ES8388 and ES7210 (kumiki's crate, to
//! come) are set up here before the display, then SPK_EN.

use kumiki_p4v1_hal::{ahb_dma, gpio::AnyPin, i2s};

use super::psram::{clear_bits, set_bits};
use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;
use t5_abi::event;

/// CPU line 8: 1 timer, 2 display, 3 console, 4 SDIO, 5 the other core, 6
/// the PPA, 7 the EMAC.
pub const IRQ: usize = 8 + 16;
const INTMTX_CORE0_BASE: usize = 0x500D_6000;
const CLIC_CTRL_BASE: usize = 0x2080_1000;
const CLIC_CTRL: u32 = (0x3F << 24) | (1 << 8);

// The AHB-GDMA block (hp_sys_clkrst_reg.h, ahb_dma_ll.h; kumiki ahb_dma.rs).
const SOC_CLK_CTRL1: usize = 0x500E_6018;
const AHB_PDMA_SYS_CLK_EN: u32 = 1 << 3;
const HP_RST_EN1: usize = 0x500E_60C4;
const RST_EN_AHB_PDMA: u32 = 1 << 1;
const AHB_DMA: usize = 0x5008_5000;
const MISC_CONF: usize = AHB_DMA + 0x64;
const MISC_AHBM_RST_INTER: u32 = 1 << 0;
const MISC_CLK_EN: u32 = 1 << 3;
/// Channel 0's OUT_PERI_SEL: 0x3F (none) after reset, I2S0 while a stream
/// uses it (kumiki ahb_dma.rs).
const OUT0_PERI_SEL: usize = AHB_DMA + 0xD0 + 0x30;
const PERI_NONE: u32 = 0x3F;
const INTR_MEM_START: usize = AHB_DMA + 0x3C4;
const INTR_MEM_END: usize = AHB_DMA + 0x3C8;
/// The window the DMA may reach: L2MEM (as ESP-IDF sets it).
const WINDOW: core::ops::Range<usize> = 0x4000_0000..0x4FFC_0000;

/// The Tab5's I2S pins (MCLK, BCK, WS, DOUT to the ES8388, DIN from the
/// ES7210; kumiki's tab5.dts).
const PINS: [u8; 5] = [30, 27, 29, 26, 28];
const SAMPLE_RATE: u32 = 48_000;
/// kumiki's default channel; nothing else in the kernel uses this block.
const DMA_CHANNEL: usize = 0;

/// Bytes `init` needs at its area.
pub const RING_BYTES: usize = core::mem::size_of::<i2s::DmaMem>();

static mut I2S: Option<i2s::I2s> = None;
static mut IRQS: u32 = 0;
/// When kumiki last polled (ticks); whether sound was pushed since the
/// stream (re)started; the restarts after gaps with sound in the ring
/// (after an idle gap the ring is silent and a restart only resyncs).
static mut LAST_POLL: u64 = 0;
static mut PUSHED: bool = false;
/// The DMA's interrupt is wanted on (`start` turns it off).
static mut IRQ_ON: bool = false;
static mut RESTARTS: u32 = 0;
/// 3/4 of a turn of the TX ring at 48 kHz (64 ms), in 16 MHz ticks.
const GAP_MAX: u64 = 64_000 * 16;

/// Sets up the AHB-GDMA and I2S0 with the rings at `area` (L2MEM, 64-byte
/// aligned, `RING_BYTES` long) and starts the stream. Once, at boot.
pub fn init(area: usize) -> Result<(), &'static str> {
    if area % 64 != 0 || !WINDOW.contains(&area) || !WINDOW.contains(&(area + RING_BYTES - 1)) {
        return Err("audio rings outside L2MEM");
    }
    set_bits(SOC_CLK_CTRL1, AHB_PDMA_SYS_CLK_EN);
    set_bits(HP_RST_EN1, RST_EN_AHB_PDMA);
    clear_bits(HP_RST_EN1, RST_EN_AHB_PDMA);
    set_bits(MISC_CONF, MISC_CLK_EN);
    set_bits(MISC_CONF, MISC_AHBM_RST_INTER);
    clear_bits(MISC_CONF, MISC_AHBM_RST_INTER);
    unsafe {
        wr(INTR_MEM_START, WINDOW.start as u32);
        wr(INTR_MEM_END, WINDOW.end as u32);
    }
    ahb_dma::assume_initialized();

    let p = kumiki_p4v1_hal::take().ok_or("i2s taken")?;
    let pin = |n: u8| unsafe { AnyPin::steal(n) }.ok_or("i2s pin");
    let pins = i2s::Pins { mclk: pin(PINS[0])?, bclk: pin(PINS[1])?, ws: pin(PINS[2])?, dout: pin(PINS[3])?, din: pin(PINS[4])? };
    let config = i2s::Config { sample_rate: SAMPLE_RATE, dma_channel: DMA_CHANNEL };
    // SAFETY: `area` is L2MEM nothing else uses (main.rs), checked above.
    let mem = unsafe { &mut *(area as *mut i2s::DmaMem) };
    let mut s = i2s::I2s::new_in(p.i2s0, pins, config, mem).map_err(|_| "i2s setup")?;
    let src = s.interrupt_sources();
    unsafe {
        wr(INTMTX_CORE0_BASE + 4 * src.tx as usize, IRQ as u32);
        wr(INTMTX_CORE0_BASE + 4 * src.rx as usize, IRQ as u32);
        wr(CLIC_CTRL_BASE + 4 * IRQ, CLIC_CTRL);
    }
    s.start();
    unsafe { LAST_POLL = Esp32p4v1::now() };
    event!(
        Info,
        "audio.started",
        rate_millihz = s.actual_sample_rate_mhz() as u32,
        ring_us = s.tx_ring_us() as u32,
        area = t5_abi::log::Addr(area as u32),
        int_tx = src.tx,
        int_rx = src.rx
    );
    unsafe { I2S = Some(s) };
    Ok(())
}

/// Stops the stream (I2S0 and both DMA channels) and resets the AHB-GDMA:
/// before a software reset, and at boot before L2MEM and PSRAM are used,
/// for a reset that left it writing the RX ring and reading the TX ring.
/// Returns whether channel 0 served a peripheral (at boot: a previous
/// boot's stream).
pub fn stop() -> bool {
    if let Some(s) = stream() {
        s.stop();
    }
    if unsafe { rd(SOC_CLK_CTRL1) } & AHB_PDMA_SYS_CLK_EN == 0 {
        return false;
    }
    let used = unsafe { rd(OUT0_PERI_SEL) } & PERI_NONE != PERI_NONE;
    set_bits(HP_RST_EN1, RST_EN_AHB_PDMA);
    clear_bits(HP_RST_EN1, RST_EN_AHB_PDMA);
    used
}

fn stream() -> Option<&'static mut i2s::I2s> {
    unsafe { (*core::ptr::addr_of_mut!(I2S)).as_mut() }
}

/// The stream, restarted if kumiki was not polled for too long (see the
/// top); every caller polls it next.
fn fresh() -> Option<&'static mut i2s::I2s> {
    let s = stream()?;
    let now = Esp32p4v1::now();
    let gap = now.saturating_sub(unsafe { LAST_POLL });
    unsafe { LAST_POLL = now };
    if gap > GAP_MAX && s.is_running() {
        s.start();
        if unsafe { IRQ_ON } {
            s.set_interrupts(true);
        }
        let pushed = unsafe { PUSHED };
        unsafe { PUSHED = false };
        if pushed {
            unsafe { RESTARTS += 1 };
            event!(Debug, "audio.restart", gap_us = (gap / 16).min(u32::MAX as u64) as u32);
        }
    }
    Some(s)
}

pub fn rate() -> u32 {
    stream().map_or(0, |s| s.config().sample_rate)
}

pub fn space() -> usize {
    fresh().map_or(0, |s| s.tx_space())
}

pub fn push(data: &[u8]) -> usize {
    let n = fresh().map_or(0, |s| s.push(data));
    if n > 0 {
        unsafe { PUSHED = true };
    }
    n
}

pub fn pop(out: &mut [u8]) -> usize {
    fresh().map_or(0, |s| s.pop(out))
}

/// The DMA's interrupt on while someone uses the stream (see the top).
/// Off once the kernel's queues are empty and the ring is silent
/// (kernel/src/audio.rs): nothing pushed is left to lose.
pub fn interrupts(on: bool) {
    if let Some(s) = stream() {
        s.set_interrupts(on);
        unsafe { IRQ_ON = on };
        if !on {
            unsafe { PUSHED = false };
        }
    }
}

/// What the interrupt does, from the scheduler loop.
pub fn service() {
    if let Some(s) = fresh() {
        s.on_interrupt();
    }
}

/// The DMA's interrupt: kumiki clears it and moves the rings on.
pub fn on_irq() {
    if let Some(s) = fresh() {
        s.on_interrupt();
        unsafe { IRQS = IRQS.wrapping_add(1) };
    }
}

/// Logs the stream's counters (after a poll: with the interrupt off,
/// `tx_descs` misses whole turns of the ring between polls).
pub fn log_stats() -> Result<u32, &'static str> {
    let s = fresh().ok_or("no audio")?;
    s.poll();
    let st = s.stats();
    let (irqs, restarts) = unsafe { (IRQS, RESTARTS) };
    event!(
        Info,
        "audio.stats",
        irqs = irqs,
        tx_descs = st.tx_descs_done as u32,
        tx_underruns = st.tx_underruns,
        rx_overruns = st.rx_overruns,
        restarts = restarts
    );
    Ok(irqs)
}
