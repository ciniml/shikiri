//! The Ethernet MAC (EMAC) in RMII mode with an IP101GRI PHY, as the
//! ESP32-P4-Function-EV-Board wires it (doc/58 R2). Register recipe from
//! ESP-IDF: esp_hal_emac (emac_ll.h, emac_hal.c, emac_periph.c), esp_eth
//! (esp_eth_mac_esp*.c, esp_eth_phy_802_3.c, ip101 of v5.4) and the hw_ver1
//! headers.
//!
//! - Pins (ESP-IDF's defaults for the P4, the EVB's): TX_EN 49, TXD0/1
//!   34/35, CRS_DV 28, RXD0/1 29/30 and REF_CLK 50 (input: the PHY makes
//!   the 50 MHz) on IO_MUX function 3; MDC 31 and MDIO 52 through the GPIO
//!   matrix; the PHY's reset on GPIO51. No PLL is used.
//! - DMA: chained descriptors (32 bytes, padded to 64 for the cache) and
//!   1536-byte buffers in internal SRAM, one frame per buffer. The CPU
//!   reaches that SRAM through L1: descriptors and buffers are written back
//!   before the DMA gets them and invalidated before they are read.
//! - Received frames raise interrupt line 7 (source 92), which only wakes
//!   the core; the scheduler loop reads the rings.

use t5_abi::event;

use super::i2c::{func_in_sel, func_out_sel, io_mux_reg};
use super::psram::delay_us;
use super::{cache_invalidate, cache_writeback, rd, wr};

// ---- registers (hw_ver1 emac_reg.h; esp32p4.peripherals.ld) ----------------
const MAC: usize = 0x5009_8000;
const MAC_CONFIG: usize = MAC;
const MAC_FRAME_FILTER: usize = MAC + 0x04;
const MAC_GMII_ADDR: usize = MAC + 0x10;
const MAC_GMII_DATA: usize = MAC + 0x14;
const MAC_FLOW_CTRL: usize = MAC + 0x18;
const MAC_INT_MASK: usize = MAC + 0x3C;
const MAC_ADDR0_HIGH: usize = MAC + 0x40;
const MAC_ADDR0_LOW: usize = MAC + 0x44;
const DMA: usize = 0x5009_9000;
const DMA_BUS_MODE: usize = DMA;
const DMA_TX_POLL: usize = DMA + 0x04;
const DMA_RX_POLL: usize = DMA + 0x08;
const DMA_RX_LIST: usize = DMA + 0x0C;
const DMA_TX_LIST: usize = DMA + 0x10;
const DMA_STATUS: usize = DMA + 0x14;
const DMA_OP_MODE: usize = DMA + 0x18;
const DMA_INT_EN: usize = DMA + 0x1C;
/// Frames the DMA dropped: no free receive descriptor [15:0], the receive
/// FIFO overflowed [27:17]; cleared by reading.
const DMA_MISSED: usize = DMA + 0x20;
/// The receive descriptor the DMA is at.
const DMA_CUR_RX_DESC: usize = DMA + 0x4C;

// MAC configuration bits.
const CFG_RE: u32 = 1 << 2;
const CFG_TE: u32 = 1 << 3;
const CFG_IPC: u32 = 1 << 10;
const CFG_DM: u32 = 1 << 11;
const CFG_FES: u32 = 1 << 14;
const CFG_PS: u32 = 1 << 15;
// DMA bits.
const BUS_SWR: u32 = 1;
/// MB | AAL | PBL 32 | ATDS (alternate descriptors), as emac_hal.c.
const BUS_MODE: u32 = (1 << 26) | (1 << 25) | (32 << 8) | (1 << 7);
const OP_SR: u32 = 1 << 1;
const OP_OSF: u32 = 1 << 2;
const OP_ST: u32 = 1 << 13;
const OP_FTF: u32 = 1 << 20;
const INT_RIE: u32 = 1 << 6;
const INT_NIE: u32 = 1 << 16;

// Clocks and reset (hp_sys_clkrst_reg.h, lp_clkrst_reg.h, hp_system_reg.h).
const SOC_CLK_CTRL1: usize = 0x500E_6018;
const EMAC_SYS_CLK_EN: u32 = 1 << 13;
const LP_HP_SDMMC_EMAC_RST: usize = 0x5011_104C;
const RST_EN_EMAC: u32 = 1 << 30;
const HP_SYSTEM_GMAC_CTRL0: usize = 0x500E_514C;
const PHY_INTF_RMII: u32 = 4 << 2;
const PERI_CLK_CTRL00: usize = 0x500E_6030;
const PERI_CLK_CTRL01: usize = 0x500E_6034;
const LP_HP_CLK_CTRL: usize = 0x5011_1040;

// Pins.
const IO_MUX_FUNC3_OUT: u32 = 3 << 12;
const IO_MUX_FUNC3_IN: u32 = (3 << 12) | (1 << 9);
const IO_MUX_GPIO_OUT: u32 = 1 << 12;
const IO_MUX_GPIO_INOUT: u32 = (1 << 12) | (1 << 9);
const DRV2: u32 = 2 << 10;
const SIG_MDC: u32 = 108;
const SIG_MDO: u32 = 109;
const SIG_MDI: u32 = 107;
const SIG_GPIO_OUT: u32 = 256;
const GPIO_SIG_IN_SEL: u32 = 1 << 7;
const PHY_RESET_PIN: usize = 51;
const GPIO_OUT1_W1TS: usize = 0x500E_0014;
const GPIO_OUT1_W1TC: usize = 0x500E_0018;
const GPIO_ENABLE1_W1TS: usize = 0x500E_0030;

// eFuse: the factory MAC; Ethernet's is the base + 3 (ESP_MAC_ETH).
const EFUSE_RD_MAC_SYS_0: usize = 0x5012_D044;
const EFUSE_RD_MAC_SYS_1: usize = 0x5012_D048;

// Interrupt: EMAC (ETS_ETH_MAC_INTR_SOURCE, the GMAC's SBD line:
// interrupt_core0_reg.h SBD_INT_MAP at +0x170) on CPU line 7.
const EMAC_SOURCE: usize = 92;
pub const IRQ: usize = 7 + 16;
const INTMTX_CORE0_BASE: usize = 0x500D_6000;
const CLIC_CTRL_BASE: usize = 0x2080_1000;
const CLIC_CTRL: u32 = (0x3F << 24) | (1 << 8);

// ---- descriptors and buffers --------------------------------------------
/// Internal SRAM the apps' pool left free once PSRAM runs them, after the
/// Wi-Fi frame rings (0x4FF8_0000 + 49408, doc/35 §5) and below the pool's
/// end. 32 receive buffers: a TCP window from the PC (about 11 segments at
/// 100 Mbit/s) overran 12 while `net` read them, and smoltcp keeps no
/// segment after a gap (doc/58 R5).
const AREA: usize = 0x4FF8_D000;
const RX_N: usize = 12;
const TX_N: usize = 4;
const DESC: usize = 64;
const BUF: usize = 1536;
const RX_DESC: usize = AREA;
const TX_DESC: usize = RX_DESC + RX_N * DESC;
const RX_BUF: usize = TX_DESC + TX_N * DESC;
const TX_BUF: usize = RX_BUF + RX_N * BUF;
const AREA_END: usize = TX_BUF + TX_N * BUF;
const _: () = assert!(AREA >= 0x4FF8_0000 + t5_abi::netdev::ring::SIZE && RINGS + t5_abi::netdev::ring::SIZE <= 0x4FFA_0000);
/// `netdev:eth0`'s frame rings (doc/58 R5), after the DMA's buffers, in the
/// same internal SRAM.
pub const RINGS: usize = AREA_END.div_ceil(128) * 128;

const OWN: u32 = 1 << 31;
// TDES0.
const TX_IC: u32 = 1 << 30;
const TX_LS: u32 = 1 << 29;
const TX_FS: u32 = 1 << 28;
const TX_TCH: u32 = 1 << 20;
// RDES0 / RDES1.
const RX_ES: u32 = 1 << 15;
const RX_FS: u32 = 1 << 9;
const RX_LS: u32 = 1 << 8;
const RX_RCH: u32 = 1 << 14;

// PHY (IEEE 802.3 and IP101GRI).
const BMCR: u32 = 0;
const BMCR_RESET: u32 = 1 << 15;
const BMCR_AN_EN: u32 = 1 << 12;
const BMCR_PDOWN: u32 = 1 << 11;
const BMCR_AN_RESTART: u32 = 1 << 9;
const PHY_ID1: u32 = 2;
const PHY_ID2: u32 = 3;
/// IP101: page 16's specific status (link bit 8, mode in [2:0]).
const IP101_CSSR: u32 = 0x1E;
/// MDC = SYS clock (180 MHz) / 102 (clock range 4, emac_hal.c).
const MDC_CR: u32 = 4 << 2;

struct State {
    up: bool,
    phy: u32,
    rx_next: usize,
    tx_next: usize,
    link: usize,
}

static mut S: State = State { up: false, phy: 0, rx_next: 0, tx_next: 0, link: 0 };

fn s() -> &'static mut State {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(S) }
}

fn word(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn set_word(addr: usize, v: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

fn ms(n: u64) {
    delay_us(n * 1000);
}

fn mdio_wait() -> Result<(), &'static str> {
    for _ in 0..1000 {
        if unsafe { rd(MAC_GMII_ADDR) } & 1 == 0 {
            return Ok(());
        }
        delay_us(1);
    }
    Err("MDIO busy")
}

fn mdio_read(phy: u32, reg: u32) -> Result<u32, &'static str> {
    mdio_wait()?;
    unsafe { wr(MAC_GMII_ADDR, (phy << 11) | (reg << 6) | MDC_CR | 1) };
    mdio_wait()?;
    Ok(unsafe { rd(MAC_GMII_DATA) } & 0xFFFF)
}

fn mdio_write(phy: u32, reg: u32, v: u32) -> Result<(), &'static str> {
    mdio_wait()?;
    unsafe {
        wr(MAC_GMII_DATA, v & 0xFFFF);
        wr(MAC_GMII_ADDR, (phy << 11) | (reg << 6) | MDC_CR | 2 | 1);
    }
    mdio_wait()
}

/// The Ethernet MAC address: the factory one + 3 (as `ESP_MAC_ETH`), as
/// bytes in order in a u64 (first byte lowest).
fn mac_address() -> u64 {
    let (w0, w1) = unsafe { (rd(EFUSE_RD_MAC_SYS_0), rd(EFUSE_RD_MAC_SYS_1)) };
    // Big-endian in the eFuse: the first byte is MAC_1's high byte.
    let base = ((w1 as u64 & 0xFFFF) << 32 | w0 as u64) + 3;
    let b = base.to_be_bytes();
    u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0])
}

fn pins() {
    unsafe {
        for pin in [49, 34, 35] {
            wr(io_mux_reg(pin), IO_MUX_FUNC3_OUT | DRV2);
        }
        for pin in [28, 29, 30, 50] {
            wr(io_mux_reg(pin), IO_MUX_FUNC3_IN | DRV2);
        }
        // IO_MUX inputs: the matrix's selection must not override them
        // (CRS_DV 178, RXD0/1 179/180).
        for sig in [178, 179, 180] {
            wr(func_in_sel(sig), rd(func_in_sel(sig)) & !GPIO_SIG_IN_SEL);
        }
        wr(io_mux_reg(31), IO_MUX_GPIO_OUT | DRV2);
        wr(func_out_sel(31), SIG_MDC);
        wr(io_mux_reg(52), IO_MUX_GPIO_INOUT | DRV2);
        // The EMAC drives MDIO's output enable.
        wr(func_out_sel(52), SIG_MDO);
        wr(func_in_sel(SIG_MDI), 52 | GPIO_SIG_IN_SEL);
    }
}

fn phy_reset_pin(high: bool) {
    let bit = 1 << (PHY_RESET_PIN - 32);
    unsafe {
        wr(io_mux_reg(PHY_RESET_PIN), IO_MUX_GPIO_OUT | DRV2);
        wr(func_out_sel(PHY_RESET_PIN), SIG_GPIO_OUT);
        wr(if high { GPIO_OUT1_W1TS } else { GPIO_OUT1_W1TC }, bit);
        wr(GPIO_ENABLE1_W1TS, bit);
    }
}

fn clocks() {
    unsafe {
        wr(SOC_CLK_CTRL1, rd(SOC_CLK_CTRL1) | EMAC_SYS_CLK_EN);
        // The SDMMC resets share the register (bits 28 and 29).
        wr(LP_HP_SDMMC_EMAC_RST, rd(LP_HP_SDMMC_EMAC_RST) | RST_EN_EMAC);
        wr(LP_HP_SDMMC_EMAC_RST, rd(LP_HP_SDMMC_EMAC_RST) & !RST_EN_EMAC);
        // RMII, REF_CLK in on the pad (emac_ll_clock_enable_rmii_input).
        wr(HP_SYSTEM_GMAC_CTRL0, (rd(HP_SYSTEM_GMAC_CTRL0) & !(0b111 << 2)) | PHY_INTF_RMII);
        // PERI_CLK_CTRL00 also holds the PSRAM's divider: change bits 24-29
        // only. Pad REF_CLK out off, RMII clock from the txrx pad, on; RX
        // clock from the txrx pad, on.
        let c0 = rd(PERI_CLK_CTRL00) & !(0x3F << 24);
        wr(PERI_CLK_CTRL00, c0 | (1 << 27) | (1 << 29));
        set_divider(1);
        // LP: the txrx pad clock on, the separate tx and rx pads off; bit 28
        // (MPLL) is the PSRAM's.
        let lp = rd(LP_HP_CLK_CTRL) & !((1 << 13) | (1 << 14));
        wr(LP_HP_CLK_CTRL, lp | (1 << 15));
    }
}

/// RX and TX clock dividers of the 50 MHz: 1 for 100 Mbit/s, 19 for 10.
fn set_divider(div: u32) {
    unsafe {
        // [7:0] RX div, bit 8 TX source (txrx pad), bit 9 TX on, [17:10]
        // TX div.
        let c1 = rd(PERI_CLK_CTRL01) & !0x3FFFF;
        wr(PERI_CLK_CTRL01, c1 | div | (1 << 9) | (div << 10));
    }
}

fn find_phy() -> Result<u32, &'static str> {
    for a in 0..32 {
        let id1 = mdio_read(a, PHY_ID1)?;
        if id1 != 0xFFFF && id1 != 0 {
            let id2 = mdio_read(a, PHY_ID2)?;
            event!(Info, "eth.phy", addr = a, id1 = id1, id2 = id2);
            return Ok(a);
        }
    }
    Err("no PHY on MDIO")
}

fn rings() {
    for i in 0..RX_N {
        let d = RX_DESC + i * DESC;
        set_word(d, OWN);
        set_word(d + 4, RX_RCH | BUF as u32);
        set_word(d + 8, (RX_BUF + i * BUF) as u32);
        set_word(d + 12, (RX_DESC + ((i + 1) % RX_N) * DESC) as u32);
    }
    for i in 0..TX_N {
        let d = TX_DESC + i * DESC;
        set_word(d, TX_TCH);
        set_word(d + 4, 0);
        set_word(d + 8, (TX_BUF + i * BUF) as u32);
        set_word(d + 12, (TX_DESC + ((i + 1) % TX_N) * DESC) as u32);
    }
    cache_writeback(AREA, AREA_END - AREA, false);
    cache_invalidate(AREA, AREA_END - AREA, false);
}

/// Starts the MAC, the PHY (autonegotiation runs on) and the DMA; the MAC
/// address.
pub fn up(psram: bool) -> Result<u64, &'static str> {
    if !psram {
        // The area is the apps' pool without PSRAM.
        return Err("no free internal SRAM (no PSRAM)");
    }
    let st = s();
    if st.up {
        return Ok(mac_address());
    }
    // The PHY's reset first: the MAC's reset ends only with REF_CLK running.
    phy_reset_pin(false);
    ms(10);
    phy_reset_pin(true);
    ms(10);
    clocks();
    pins();
    unsafe {
        wr(DMA_BUS_MODE, rd(DMA_BUS_MODE) | BUS_SWR);
        let mut n = 0;
        while rd(DMA_BUS_MODE) & BUS_SWR != 0 {
            if n == 100 {
                return Err("EMAC reset did not finish (no REF_CLK?)");
            }
            ms(1);
            n += 1;
        }
    }
    let phy = find_phy()?;
    st.phy = phy;
    let bmcr = mdio_read(phy, BMCR)?;
    mdio_write(phy, BMCR, (bmcr & !BMCR_PDOWN) | BMCR_RESET)?;
    let mut n = 0;
    while mdio_read(phy, BMCR)? & BMCR_RESET != 0 {
        if n == 100 {
            return Err("PHY reset did not finish");
        }
        ms(1);
        n += 1;
    }
    let bmcr = mdio_read(phy, BMCR)?;
    mdio_write(phy, BMCR, bmcr | BMCR_AN_EN | BMCR_AN_RESTART)?;

    let mac = mac_address();
    let b = mac.to_le_bytes();
    unsafe {
        wr(MAC_CONFIG, CFG_PS | CFG_FES | CFG_DM | CFG_IPC);
        wr(MAC_FRAME_FILTER, 0);
        wr(MAC_FLOW_CTRL, 0);
        wr(MAC_INT_MASK, 0xFFFF_FFFF);
        wr(MAC_ADDR0_HIGH, (b[5] as u32) << 8 | b[4] as u32);
        wr(MAC_ADDR0_LOW, u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        wr(DMA_OP_MODE, rd(DMA_OP_MODE) | OP_FTF);
        let mut n = 0;
        while rd(DMA_OP_MODE) & OP_FTF != 0 && n < 1000 {
            delay_us(10);
            n += 1;
        }
        // Operate on second frame; no store-and-forward (the RX FIFO is
        // 256 bytes on the P4, emac_hal.c).
        wr(DMA_OP_MODE, OP_OSF);
        wr(DMA_BUS_MODE, BUS_MODE);
    }
    rings();
    st.rx_next = 0;
    st.tx_next = 0;
    st.link = 0;
    unsafe {
        wr(DMA_RX_LIST, RX_DESC as u32);
        wr(DMA_TX_LIST, TX_DESC as u32);
        wr(DMA_STATUS, 0xFFFF_FFFF);
        wr(DMA_INT_EN, INT_RIE | INT_NIE);
        wr(INTMTX_CORE0_BASE + 4 * EMAC_SOURCE, IRQ as u32);
        wr(CLIC_CTRL_BASE + 4 * IRQ, CLIC_CTRL);
        // The transmitter before the DMA's (emac_hal_start).
        wr(MAC_CONFIG, rd(MAC_CONFIG) | CFG_TE);
        wr(DMA_OP_MODE, rd(DMA_OP_MODE) | OP_ST);
        wr(DMA_OP_MODE, rd(DMA_OP_MODE) | OP_SR);
        wr(MAC_CONFIG, rd(MAC_CONFIG) | CFG_RE);
    }
    st.up = true;
    Ok(mac)
}

static IRQS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The interrupt only wakes the core: clears what the DMA reported.
pub fn on_irq() {
    IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    unsafe {
        let st = rd(DMA_STATUS);
        wr(DMA_STATUS, st & 0x0001_FFFF);
    }
}

static MISSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static OVERFLOWED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// (interrupts taken, the DMA status now, frames dropped for want of a
/// descriptor, frames dropped by the receive FIFO) for `eth.stats`.
pub fn stats() -> (u32, u32, u32, u32) {
    use core::sync::atomic::Ordering::Relaxed;
    let m = unsafe { rd(DMA_MISSED) };
    MISSED.fetch_add(m & 0xFFFF, Relaxed);
    OVERFLOWED.fetch_add((m >> 17) & 0x7FF, Relaxed);
    (IRQS.load(Relaxed), unsafe { rd(DMA_STATUS) }, MISSED.load(Relaxed), OVERFLOWED.load(Relaxed) | BEHIND.load(Relaxed) << 16)
}

fn rx_desc() -> usize {
    RX_DESC + s().rx_next * DESC
}

static BEHIND: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Received frames dropped for their status (doc/58 R5): all, and those
/// with a CRC error (RDES0 bit 1), a receive error from the PHY (bit 3),
/// split over descriptors (no FS or LS); the last bad status.
static RX_BAD: [core::sync::atomic::AtomicU32; 4] = [const { core::sync::atomic::AtomicU32::new(0) }; 4];
static RX_BAD_LAST: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// (frames dropped, CRC errors, receive errors, split, last bad RDES0).
pub fn rx_errors() -> [u32; 5] {
    use core::sync::atomic::Ordering::Relaxed;
    [RX_BAD[0].load(Relaxed), RX_BAD[1].load(Relaxed), RX_BAD[2].load(Relaxed), RX_BAD[3].load(Relaxed), RX_BAD_LAST.load(Relaxed)]
}

pub fn pending() -> bool {
    if !s().up {
        return false;
    }
    let d = rx_desc();
    cache_invalidate(d, DESC, false);
    let ready = word(d) & OWN == 0;
    // Diagnosis (doc/58 R5): the DMA past this descriptor while it reads
    // as the DMA's here means a stale copy or a ring out of step.
    if !ready && unsafe { rd(DMA_CUR_RX_DESC) } as usize != d {
        BEHIND.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    ready
}

/// The next good frame into `out`; frames with errors (or split over
/// buffers, which 1536-byte buffers do not do) are dropped.
pub fn recv(out: &mut [u8]) -> Option<usize> {
    let st = s();
    if !st.up {
        return None;
    }
    for _ in 0..RX_N {
        let d = rx_desc();
        cache_invalidate(d, DESC, false);
        let status = word(d);
        if status & OWN != 0 {
            return None;
        }
        let len = ((status >> 16) & 0x3FFF) as usize;
        let good = status & RX_ES == 0 && status & (RX_FS | RX_LS) == RX_FS | RX_LS && len > 4;
        if !good {
            use core::sync::atomic::Ordering::Relaxed;
            RX_BAD[0].fetch_add(1, Relaxed);
            RX_BAD[1].fetch_add((status >> 1) & 1, Relaxed);
            RX_BAD[2].fetch_add((status >> 3) & 1, Relaxed);
            RX_BAD[3].fetch_add((status & (RX_FS | RX_LS) != RX_FS | RX_LS) as u32, Relaxed);
            RX_BAD_LAST.store(status, Relaxed);
        }
        let n = len.saturating_sub(4).min(out.len());
        if good {
            let buf = RX_BUF + st.rx_next * BUF;
            cache_invalidate(buf, BUF, false);
            out[..n].copy_from_slice(unsafe { core::slice::from_raw_parts(buf as *const u8, n) });
        }
        // Back to the DMA.
        set_word(d, OWN);
        cache_writeback(d, DESC, false);
        unsafe { wr(DMA_RX_POLL, 0) };
        st.rx_next = (st.rx_next + 1) % RX_N;
        if good {
            return Some(n);
        }
    }
    None
}

pub fn send(frame: &[u8]) -> Result<(), &'static str> {
    let st = s();
    if !st.up {
        return Err("Ethernet not up");
    }
    if frame.len() > BUF {
        return Err("frame too long");
    }
    let d = TX_DESC + st.tx_next * DESC;
    cache_invalidate(d, DESC, false);
    if word(d) & OWN != 0 {
        return Err("busy");
    }
    let buf = TX_BUF + st.tx_next * BUF;
    unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), buf as *mut u8, frame.len()) };
    cache_writeback(buf, frame.len(), false);
    set_word(d + 4, frame.len() as u32);
    set_word(d, OWN | TX_IC | TX_LS | TX_FS | TX_TCH);
    cache_writeback(d, DESC, false);
    unsafe { wr(DMA_TX_POLL, 0) };
    st.tx_next = (st.tx_next + 1) % TX_N;
    Ok(())
}

/// The PHY's link (`netdev::LINK`); the MAC follows its speed and duplex.
pub fn link() -> usize {
    let st = s();
    if !st.up {
        return 0;
    }
    let Ok(cssr) = mdio_read(st.phy, IP101_CSSR) else { return st.link };
    let link = if cssr & (1 << 8) == 0 {
        0
    } else {
        match cssr & 7 {
            1 => 10,
            2 => 100,
            5 => 10 | t5_abi::netdev::LINK_FULL,
            6 => 100 | t5_abi::netdev::LINK_FULL,
            _ => 0,
        }
    };
    if link != st.link && link != 0 {
        let fast = link & 0xFFFF == 100;
        let full = link & t5_abi::netdev::LINK_FULL != 0;
        set_divider(if fast { 1 } else { 19 });
        unsafe {
            let c = rd(MAC_CONFIG) & !(CFG_FES | CFG_DM);
            wr(MAC_CONFIG, c | if fast { CFG_FES } else { 0 } | if full { CFG_DM } else { 0 });
        }
    }
    st.link = link;
    link
}
