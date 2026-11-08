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
//! Stage A1 without the codecs: the ES8388 and ES7210 (kumiki's crate, to
//! come) are set up here before the display, then SPK_EN.

use kumiki_p4v1_hal::{ahb_dma, gpio::AnyPin, i2s};

use super::psram::{clear_bits, set_bits};
use super::wr;
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

/// The DMA's interrupt: kumiki clears it and moves the rings on.
pub fn on_irq() {
    let s = unsafe { &mut *core::ptr::addr_of_mut!(I2S) };
    if let Some(s) = s.as_mut() {
        s.on_interrupt();
        unsafe { IRQS = IRQS.wrapping_add(1) };
    }
}

/// Logs the stream's counters (after a poll: with the interrupt off,
/// `tx_descs` misses whole turns of the ring between polls).
pub fn log_stats() -> Result<u32, &'static str> {
    let s = unsafe { &mut *core::ptr::addr_of_mut!(I2S) };
    let s = s.as_mut().ok_or("no audio")?;
    s.poll();
    let st = s.stats();
    let irqs = unsafe { IRQS };
    event!(
        Info,
        "audio.stats",
        irqs = irqs,
        tx_descs = st.tx_descs_done as u32,
        tx_underruns = st.tx_underruns,
        rx_overruns = st.rx_overruns
    );
    Ok(irqs)
}
