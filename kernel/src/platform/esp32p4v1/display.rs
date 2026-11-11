//! The Tab5's MIPI-DSI display (M6b, doc/17): LDO VO3 for the D-PHY, the
//! DesignWare DSI host and its D-PHY, the DSI bridge, and DW-GDMA streaming
//! a 720x1280 RGB565 framebuffer from PSRAM, one frame per DMA block.
//!
//! The blocks come from a ring of `RING` list items, each pointing to the
//! next. The DMA clears an item's VALID bit when it has used it, and stops
//! at an invalid one; after each frame `service` marks the finished items
//! valid again. So the CPU has `RING - 1` frames to react, not the 0.5 ms
//! vertical blanking that ESP-IDF's one-item list (restarted from the
//! interrupt) leaves: with that, every kernel stretch of more than a few
//! ms (a signature check is ~23 ms) cost a frame, and the screen
//! flickered (doc/17 §10).
//!
//! Register addresses, values and the order are from
//! doc/research/m6-dsi.md §7 (ESP-IDF 6.0 esp_lcd/dsi, esp_hal_lcd and
//! dw_gdma), the panel's commands from doc/research/m6-panel-touch.md §4.3
//! (M5GFX's ILI9881C table). Each stage logs an event, so a hang shows
//! where it stopped.

use t5_abi::event;
use t5_abi::log::Addr;

use super::psram::{clear_bits, delay_us, set_bits, set_field, wait};
use super::{Esp32p4v1, rd, wr};
use crate::platform::{Panel, Platform};

pub const WIDTH: usize = 720;
pub const HEIGHT: usize = 1280;
pub const FB_BYTES: usize = WIDTH * HEIGHT * 2;

// ---- LDO VO3 = 2500 mV (m6-dsi.md §1). VO1 (flash) is never touched. ----
const PMU_EXT_LDO_P0_0P2A: usize = 0x5011_51C0;
const PMU_EXT_LDO_P0_0P2A_ANA: usize = 0x5011_51C4;
const LDO_FORCE_TIEH_SEL: u32 = 1 << 7;
const LDO_XPD: u32 = 1 << 8;
const LDO_TIEH_SEL: u32 = 0b111 << 9;
const LDO_TIEH: u32 = 1 << 14;
const LDO_ANA_MUL_S: u32 = 23;
const LDO_ANA_EN_VDET: u32 = 1 << 26;
const LDO_ANA_DREF_S: u32 = 28;
const EFUSE_RD_MAC_SYS_2: usize = 0x5012_D04C;
const EFUSE_RD_MAC_SYS_3: usize = 0x5012_D050;
const VO3_MV: i32 = 2500;

// ---- Clocks and resets (hp_sys_clkrst_reg.h; m6-dsi.md §2) ----
const SOC_CLK_CTRL0: usize = 0x500E_6014;
const GDMA_CPU_CLK_EN: u32 = 1 << 13;
const SOC_CLK_CTRL1: usize = 0x500E_6018;
const GDMA_SYS_CLK_EN: u32 = 1 << 5;
const DSI_SYS_CLK_EN: u32 = 1 << 12;
const REF_CLK_CTRL1: usize = 0x500E_6028;
const REF_240M_CLK_EN: u32 = 1 << 30;
const REF_CLK_CTRL2: usize = 0x500E_602C;
const REF_20M_CLK_EN: u32 = 1 << 8;
const PERI_CLK_CTRL02: usize = 0x500E_6038;
const PERI_CLK_CTRL03: usize = 0x500E_603C;
const DPHY_CFG_CLK_EN: u32 = 1 << 0;
const DPHY_PLL_REFCLK_EN: u32 = 1 << 1;
const DPICLK_EN: u32 = 1 << 7;
const HP_RST_EN0: usize = 0x500E_60C0;
const RST_EN_GDMA: u32 = 1 << 21;
const RST_EN_DSI_BRG: u32 = 1 << 26;
/// `reset_cause` after `reset` (SW_SYS).
const RESET_SOFTWARE: u32 = 3;

// ---- DSI host (mipi_dsi_host_reg.h) ----
const HOST: usize = 0x500A_0000;
const PWR_UP: usize = HOST + 0x04;
const CLKMGR_CFG: usize = HOST + 0x08;
const DPI_VCID: usize = HOST + 0x0C;
const DPI_COLOR_CODING: usize = HOST + 0x10;
const DPI_CFG_POL: usize = HOST + 0x14;
const PCKHDL_CFG: usize = HOST + 0x2C;
const GEN_VCID: usize = HOST + 0x30;
const MODE_CFG: usize = HOST + 0x34;
const VID_MODE_CFG: usize = HOST + 0x38;
const VID_PKT_SIZE: usize = HOST + 0x3C;
const VID_NUM_CHUNKS: usize = HOST + 0x40;
const VID_NULL_SIZE: usize = HOST + 0x44;
const VID_HSA_TIME: usize = HOST + 0x48;
const VID_HBP_TIME: usize = HOST + 0x4C;
const VID_HLINE_TIME: usize = HOST + 0x50;
const VID_VSA_LINES: usize = HOST + 0x54;
const VID_VBP_LINES: usize = HOST + 0x58;
const VID_VFP_LINES: usize = HOST + 0x5C;
const VID_VACTIVE_LINES: usize = HOST + 0x60;
const CMD_MODE_CFG: usize = HOST + 0x68;
const GEN_HDR: usize = HOST + 0x6C;
const GEN_PLD_DATA: usize = HOST + 0x70;
const CMD_PKT_STATUS: usize = HOST + 0x74;
const TO_CNT_CFG: usize = HOST + 0x78;
const LPCLK_CTRL: usize = HOST + 0x94;
const PHY_TMR_LPCLK_CFG: usize = HOST + 0x98;
const PHY_TMR_CFG: usize = HOST + 0x9C;
const PHY_RSTZ: usize = HOST + 0xA0;
const PHY_IF_CFG: usize = HOST + 0xA4;
const PHY_STATUS: usize = HOST + 0xB0;
const PHY_TST_CTRL0: usize = HOST + 0xB4;
const PHY_TST_CTRL1: usize = HOST + 0xB8;
const INT_ST0: usize = HOST + 0xBC;
const INT_ST1: usize = HOST + 0xC0;
const PHY_TMR_RD_CFG: usize = HOST + 0xF4;

const GEN_CMD_EMPTY: u32 = 1 << 0;
const GEN_CMD_FULL: u32 = 1 << 1;
const GEN_PLD_W_EMPTY: u32 = 1 << 2;
const GEN_PLD_W_FULL: u32 = 1 << 3;
const GEN_PLD_R_EMPTY: u32 = 1 << 4;
const GEN_RD_CMD_BUSY: u32 = 1 << 6;
const PHY_LOCK: u32 = 1 << 0;
/// Clock lane and lanes 0 and 1 in the stop state.
const PHY_STOPSTATES: u32 = (1 << 2) | (1 << 4) | (1 << 7);
const BTA_EN: u32 = 1 << 2;

// DSI data types (mipi_dsi_types.h).
const DT_DCS_SHORT_WRITE_0: u32 = 0x05;
const DT_DCS_SHORT_WRITE_1: u32 = 0x15;
const DT_DCS_READ: u32 = 0x06;
const DT_SET_MAX_RETURN: u32 = 0x37;
const DT_DCS_LONG_WRITE: u32 = 0x39;

// ---- DSI bridge (mipi_dsi_bridge_reg.h, v1) ----
const BRG: usize = 0x500A_0800;
const BRG_EN: usize = BRG + 0x04;
const BRG_DMA_REQ_CFG: usize = BRG + 0x08;
const BRG_RAW_NUM_CFG: usize = BRG + 0x0C;
const BRG_FIFO_FLOW_STATUS: usize = BRG + 0x14;
const BRG_PIXEL_TYPE: usize = BRG + 0x18;
const BRG_DPI_V_CFG0: usize = BRG + 0x30;
const BRG_DPI_V_CFG1: usize = BRG + 0x34;
const BRG_DPI_H_CFG0: usize = BRG + 0x38;
const BRG_DPI_H_CFG1: usize = BRG + 0x3C;
const BRG_DPI_MISC_CONFIG: usize = BRG + 0x40;
const BRG_DPI_CONFIG_UPDATE: usize = BRG + 0x44;
const BRG_INT_ENA: usize = BRG + 0x50;
const BRG_INT_CLR: usize = BRG + 0x54;
const BRG_INT_RAW: usize = BRG + 0x58;
const BRG_DMA_FRAME_INTERVAL: usize = BRG + 0x6C;
const BRG_HOST_CTRL: usize = BRG + 0x80;
const BRG_DMA_FLOW_CTRL: usize = BRG + 0x88;
const BRG_RAW_BUF_ALMOST_EMPTY_THRD: usize = BRG + 0x8C;
const BRG_FIFO: u32 = 0x5010_5000;
const BRG_UNDERRUN: u32 = 1 << 0;
const DMA_MULTIBLK_EN: u32 = 1 << 28;

// ---- DW-GDMA, channel 1 (dw_gdma_reg.h; m6-dsi.md §5.4) ----
const GDMA: usize = 0x5008_1000;
const GDMA_CFG0: usize = GDMA + 0x10;
const GDMA_CHEN0: usize = GDMA + 0x18;
const GDMA_CHEN1: usize = GDMA + 0x1C;
const GDMA_RESET0: usize = GDMA + 0x58;
const CH: usize = GDMA + 0x100;
const CH_CFG0: usize = CH + 0x20;
const CH_CFG1: usize = CH + 0x24;
const CH_LLP0: usize = CH + 0x28;
const CH_LLP1: usize = CH + 0x2C;
const CH_STATUS0: usize = CH + 0x30;
const CH_INTSTATUS_ENABLE0: usize = CH + 0x80;
const CH_INTSTATUS0: usize = CH + 0x88;
const CH_INTCLEAR0: usize = CH + 0x98;
const CH1_ENABLE: u32 = 0x101;
const BLOCK_TFR_DONE: u32 = 1 << 0;
const DMA_TFR_DONE: u32 = 1 << 1;
const LLI_INVALID_ERR: u32 = 1 << 13;
/// CTL of the frame's block: from memory (master 1, incrementing, 64-bit,
/// bursts of 512) to the bridge FIFO (master 0, fixed, 64-bit, bursts of
/// 256); AXI bursts of 16.
const LLI_CTL_LO: u32 = 0x001E_1B41;
const LLI_CTL_HI: u32 = 0x0010_8840;
const LLI_VALID: u32 = 1 << 31;
/// Interrupt (block done) at the end of each frame.
const IOC_BLKTFR: u32 = 1 << 26;
/// Word index of CTL[63:32] in a list item.
const LLI_CTL_HI_WORD: usize = 9;
/// The non-cacheable alias of L2MEM: the DMA reads the list item from
/// memory, not through the cache (dw_gdma.c:45; ext_mem_defs.h:68).
const L2MEM_NON_CACHE: usize = 0x4000_0000;

// Interrupt: DW-GDMA is source 24 (interrupts.h), routed to CPU line 2.
const INTMTX_CORE0_BASE: usize = 0x500D_6000;
const GDMA_SOURCE: usize = 24;
pub const IRQ: usize = 2 + 16;
const CLIC_CTRL_BASE: usize = 0x2080_1000;
/// Priority 1, level-triggered, enabled (as the timer).
const CLIC_CTRL: u32 = (0x3F << 24) | (1 << 8);

// Backlight (GPIO22 to the ME2212's EN; m6-panel-touch.md §5).
const BL_PIN: usize = 22;
const GPIO_OUT_W1TS: usize = 0x500E_0008;
const GPIO_OUT_W1TC: usize = 0x500E_000C;
const GPIO_ENABLE_W1TS: usize = 0x500E_0024;
const SIG_GPIO_OUT: u32 = 256;
const OUT_SEL_OEN: u32 = 1 << 10;

// ROM Cache_WriteBack_Addr (esp32p4.rom.ld:190): L1 D, then L2.
const ROM_CACHE_WRITEBACK_ADDR: usize = 0x4FC0_03F4;
const CACHE_MAP_L1_DCACHE: u32 = 0x10;
const CACHE_MAP_L2_CACHE: u32 = 0x20;

/// Panel timing in pixels/lines and the lane rate (m6-panel-touch.md §4.2).
struct Timing {
    hsw: u32,
    hbp: u32,
    hfp: u32,
    vsw: u32,
    vbp: u32,
    vfp: u32,
}

const ILI9881C_TIMING: Timing = Timing { hsw: 40, hbp: 140, hfp: 40, vsw: 4, vbp: 20, vfp: 20 };
const LANE_MBPS: u32 = 1040;
const DPI_MHZ: u32 = 80;
/// 20 MHz reference x 52 / 1 (m6-dsi.md §4.2).
const PLL_N: u32 = 1;
const PLL_M: u32 = 52;
const HSFREQRANGE: u32 = 0x2A;

/// The DMA's list items: 64 bytes each, 64-byte aligned, in internal SRAM.
#[repr(C, align(64))]
struct Lli([u32; 16]);
/// 16 frames, about 250 ms: longer than the longest kernel stretch without
/// the scheduler loop (verifying a 138 KiB image takes ~127 ms).
const RING: usize = 16;
static mut LLI: [Lli; RING] = [const { Lli([0; 16]) }; RING];

struct State {
    /// The display's memory (the first seat's framebuffer).
    fb: usize,
    /// The framebuffer the list items point at (doc/60).
    shown: usize,
    frames: u32,
    underruns: u32,
    /// Times the DMA ran into an invalid item and was restarted.
    restarts: u32,
    started: u64,
}
static mut STATE: Option<State> = None;

fn state() -> Option<&'static mut State> {
    unsafe { (*core::ptr::addr_of_mut!(STATE)).as_mut() }
}

// ---- power ----

/// IDF's search for dref/mul with VO3's eFuse calibration
/// (ldo_ll_voltage_to_dref_mul, ldo_ll.h:70-122). Returns (dref, mul, mV).
fn vo3_setting() -> (u32, u32, i32, [u32; 3]) {
    let (sys2, sys3) = unsafe { (rd(EFUSE_RD_MAC_SYS_2), rd(EFUSE_RD_MAC_SYS_3)) };
    let blk = ((sys2 >> 11) & 3) * 100 + ((sys2 >> 8) & 7);
    let (ek, evos, ec) = if blk >= 1 { ((sys3 >> 6) & 0xFF, (sys3 >> 14) & 0x3F, (sys3 >> 20) & 0x3F) } else { (0, 0, 0) };
    let k = match ek {
        0 => 1000,
        k if k & 0x80 != 0 => 975 - (k & 0x7F) as i32,
        k => 975 + k as i32,
    };
    let vos = match evos {
        0 => 0,
        v if v & 0x20 != 0 => -((v & 0x1F) as i32) - 3,
        v => v as i32 - 3,
    };
    let c = match ec {
        0 => 1000,
        c if c & 0x20 != 0 => 990 - (c & 0x1F) as i32,
        c => 990 + c as i32,
    };
    let (mut best, mut best_diff) = ((0, 0, 0), i64::MAX);
    for dref in 0..16 {
        let vref_20 = if dref < 9 { 10 + dref } else { 20 + (dref - 9) * 2 } as i64;
        for mul in 0..8 {
            let vout = (vref_20 * k as i64 + 20 * vos as i64) * (4000 + mul as i64 * c as i64);
            let diff = (VO3_MV as i64 * 80_000 - vout).abs();
            if diff < best_diff {
                best_diff = diff;
                best = (dref as u32, mul as u32, (vout / 80_000) as i32);
            }
        }
    }
    (best.0, best.1, best.2, [ek, evos, ec])
}

fn vo3_on() -> Result<(), &'static str> {
    let (dref, mul, mv, [ek, evos, ec]) = vo3_setting();
    let before = unsafe { (rd(PMU_EXT_LDO_P0_0P2A), rd(PMU_EXT_LDO_P0_0P2A_ANA)) };
    event!(Info, "display.ldo", dref = dref, mul = mul, mv = mv, efuse_k = ek, efuse_vos = evos, efuse_c = ec, ldo = before.0, ana = before.1);
    // The D-PHY is a 2.5 V supply; refuse anything far from it.
    if !(2400..=2600).contains(&mv) {
        return Err("VO3 setting out of range");
    }
    unsafe {
        clear_bits(PMU_EXT_LDO_P0_0P2A, LDO_TIEH);
        let ana = rd(PMU_EXT_LDO_P0_0P2A_ANA) & !((0xF << LDO_ANA_DREF_S) | (0b111 << LDO_ANA_MUL_S));
        wr(PMU_EXT_LDO_P0_0P2A_ANA, ana | (dref << LDO_ANA_DREF_S) | (mul << LDO_ANA_MUL_S) | LDO_ANA_EN_VDET);
        let ldo = rd(PMU_EXT_LDO_P0_0P2A) & !LDO_TIEH_SEL;
        wr(PMU_EXT_LDO_P0_0P2A, ldo | LDO_FORCE_TIEH_SEL | LDO_XPD);
    }
    delay_us(100);
    Ok(())
}

// ---- DSI host and D-PHY ----

fn phy_write(addr: u32, val: u32) {
    unsafe {
        wr(PHY_TST_CTRL0, 0);
        wr(PHY_TST_CTRL1, 0x1_0000 | addr);
        wr(PHY_TST_CTRL0, 0b10);
        wr(PHY_TST_CTRL0, 0);
        wr(PHY_TST_CTRL1, val);
        wr(PHY_TST_CTRL0, 0b10);
        wr(PHY_TST_CTRL0, 0);
    }
}

fn host_init() -> Result<(), &'static str> {
    if unsafe { rd(REF_CLK_CTRL1) } & REF_240M_CLK_EN == 0 || unsafe { rd(REF_CLK_CTRL2) } & REF_20M_CLK_EN == 0 {
        return Err("PLL_F240M or PLL_F20M is gated");
    }
    set_bits(SOC_CLK_CTRL1, DSI_SYS_CLK_EN);
    set_bits(HP_RST_EN0, RST_EN_DSI_BRG);
    clear_bits(HP_RST_EN0, RST_EN_DSI_BRG);
    let host_ctrl = unsafe { rd(BRG_HOST_CTRL) };
    if host_ctrl & 1 == 0 {
        // The host's cfg and reference clocks (m6-dsi.md §2).
        set_bits(BRG_HOST_CTRL, 1);
    }
    set_field(PERI_CLK_CTRL02, 30, 2, 0); // D-PHY config and PLL reference: PLL_F20M
    set_bits(PERI_CLK_CTRL03, DPHY_CFG_CLK_EN | DPHY_PLL_REFCLK_EN);
    unsafe {
        wr(PHY_IF_CFG, 1); // 2 lanes
        wr(PWR_UP, 1);
        for v in [0x1, 0x1, 0x3, 0x7, 0xF] {
            wr(PHY_RSTZ, v); // shutdownz, rstz 0 -> 1, enableclk, forcepll
        }
    }
    phy_write(0x44, HSFREQRANGE << 1);
    phy_write(0x19, 0x30);
    phy_write(0x17, PLL_N - 1);
    phy_write(0x18, (PLL_M - 1) & 0x1F);
    phy_write(0x18, 0x80 | (((PLL_M - 1) >> 5) & 0x0F));
    let locked = wait(10_000, || unsafe { rd(PHY_STATUS) } & PHY_LOCK != 0);
    let stopped = locked && wait(10_000, || unsafe { rd(PHY_STATUS) } & PHY_STOPSTATES == PHY_STOPSTATES);
    event!(Info, "display.phy", locked = locked, stopped = stopped, status = unsafe { rd(PHY_STATUS) }, host_ctrl = host_ctrl);
    if !stopped {
        return Err("D-PHY did not lock");
    }
    unsafe {
        wr(MODE_CFG, 1); // command mode
        wr(LPCLK_CTRL, 0);
        wr(PHY_TMR_CFG, 0x0032_0068);
        wr(PHY_TMR_LPCLK_CFG, 0x002E_0080);
        // CRC and ECC receive, EoTp transmit, bus turnaround (for reads).
        wr(PCKHDL_CFG, 0x19 | BTA_EN);
        let mbps = LANE_MBPS / 8;
        wr(CLKMGR_CFG, ((mbps / 10) << 8) | (mbps / 18));
        for i in 0..6 {
            wr(TO_CNT_CFG + 4 * i, 0);
        }
        wr(PHY_TMR_RD_CFG, 6000);
        wr(PHY_IF_CFG, (0x3F << 8) | 1);
        wr(CMD_MODE_CFG, 0x010F_7F02); // every command in LP, ACK requested
    }
    Ok(())
}

/// Waits until the command and payload FIFOs have been sent.
fn cmd_idle() -> Result<(), &'static str> {
    let done = || unsafe { rd(CMD_PKT_STATUS) } & (GEN_CMD_EMPTY | GEN_PLD_W_EMPTY) == GEN_CMD_EMPTY | GEN_PLD_W_EMPTY;
    if wait(20_000, done) { Ok(()) } else { Err("DSI command not sent") }
}

/// A DCS write: `cmd` with `params` (m6-dsi.md §3.3).
fn dcs_write(cmd: u8, params: &[u8]) -> Result<(), &'static str> {
    cmd_idle()?;
    let hdr = match params.len() {
        0 => ((cmd as u32) << 8) | DT_DCS_SHORT_WRITE_0,
        1 => ((params[0] as u32) << 16) | ((cmd as u32) << 8) | DT_DCS_SHORT_WRITE_1,
        n => {
            let wc = 1 + n as u32;
            let mut word = cmd as u32;
            let mut shift = 8;
            for &p in params {
                word |= (p as u32) << shift;
                shift += 8;
                if shift == 32 {
                    push_payload(word)?;
                    word = 0;
                    shift = 0;
                }
            }
            if shift != 0 {
                push_payload(word)?;
            }
            ((wc >> 8) << 16) | ((wc & 0xFF) << 8) | DT_DCS_LONG_WRITE
        }
    };
    if !wait(20_000, || unsafe { rd(CMD_PKT_STATUS) } & GEN_CMD_FULL == 0) {
        return Err("DSI command FIFO full");
    }
    unsafe { wr(GEN_HDR, hdr) };
    cmd_idle()
}

fn push_payload(word: u32) -> Result<(), &'static str> {
    if !wait(20_000, || unsafe { rd(CMD_PKT_STATUS) } & GEN_PLD_W_FULL == 0) {
        return Err("DSI payload FIFO full");
    }
    unsafe { wr(GEN_PLD_DATA, word) };
    Ok(())
}

/// A DCS read of `out.len()` (at most 4) bytes, with a time limit.
fn dcs_read(cmd: u8, out: &mut [u8]) -> Result<(), &'static str> {
    cmd_idle()?;
    let n = out.len() as u32;
    unsafe {
        wr(GEN_HDR, ((n >> 8) << 16) | ((n & 0xFF) << 8) | DT_SET_MAX_RETURN);
        cmd_idle()?;
        wr(GEN_VCID, 0);
        wr(GEN_HDR, ((cmd as u32) << 8) | DT_DCS_READ);
    }
    let got = wait(50_000, || unsafe { rd(CMD_PKT_STATUS) } & GEN_RD_CMD_BUSY == 0)
        && wait(50_000, || unsafe { rd(CMD_PKT_STATUS) } & GEN_PLD_R_EMPTY == 0);
    if !got {
        event!(Warn, "display.read_failed", cmd = cmd, int_st0 = unsafe { rd(INT_ST0) }, int_st1 = unsafe { rd(INT_ST1) });
        return Err("DSI read timed out");
    }
    let word = unsafe { rd(GEN_PLD_DATA) };
    for (i, b) in out.iter_mut().enumerate() {
        *b = (word >> (8 * i)) as u8;
    }
    // Drain anything left.
    while unsafe { rd(CMD_PKT_STATUS) } & GEN_PLD_R_EMPTY == 0 {
        unsafe { rd(GEN_PLD_DATA) };
    }
    Ok(())
}

// ---- the panel ----

/// ILI9881C commands as M5GFX sends them (m6-panel-touch.md §4.3):
/// (command, parameters, milliseconds to wait after).
const ILI9881C_INIT: &[(u8, &[u8], u16)] = &[
    (0xFF, &[0x98, 0x81, 0x01], 0),
    (0xB7, &[0x03], 0), // 2 lanes
    (0xFF, &[0x98, 0x81, 0x00], 0),
    (0x11, &[], 120), // SLPOUT
    (0xFF, &[0x98, 0x81, 0x03], 0),
];

/// Page 3 (GIP) as (register, value) pairs.
const ILI9881C_PAGE3: &[(u8, u8)] = &[
    (0x01, 0x00), (0x02, 0x00), (0x03, 0x73), (0x04, 0x00), (0x05, 0x00), (0x06, 0x08), (0x07, 0x00), (0x08, 0x00),
    (0x09, 0x1B), (0x0A, 0x01), (0x0B, 0x01), (0x0C, 0x0D), (0x0D, 0x01), (0x0E, 0x01), (0x0F, 0x26), (0x10, 0x26),
    (0x11, 0x00), (0x12, 0x00), (0x13, 0x02), (0x14, 0x00), (0x15, 0x00), (0x16, 0x00), (0x17, 0x00), (0x18, 0x00),
    (0x19, 0x00), (0x1A, 0x00), (0x1B, 0x00), (0x1C, 0x00), (0x1D, 0x00), (0x1E, 0x40), (0x1F, 0x00), (0x20, 0x06),
    (0x21, 0x01), (0x22, 0x00), (0x23, 0x00), (0x24, 0x00), (0x25, 0x00), (0x26, 0x00), (0x27, 0x00), (0x28, 0x33),
    (0x29, 0x03), (0x2A, 0x00), (0x2B, 0x00), (0x2C, 0x00), (0x2D, 0x00), (0x2E, 0x00), (0x2F, 0x00), (0x30, 0x00),
    (0x31, 0x00), (0x32, 0x00), (0x33, 0x00), (0x34, 0x00), (0x35, 0x00), (0x36, 0x00), (0x37, 0x00), (0x38, 0x00),
    (0x39, 0x00), (0x3A, 0x00), (0x3B, 0x00), (0x3C, 0x00), (0x3D, 0x00), (0x3E, 0x00), (0x3F, 0x00), (0x40, 0x00),
    (0x41, 0x00), (0x42, 0x00), (0x43, 0x00), (0x44, 0x00), (0x50, 0x01), (0x51, 0x23), (0x52, 0x45), (0x53, 0x67),
    (0x54, 0x89), (0x55, 0xAB), (0x56, 0x01), (0x57, 0x23), (0x58, 0x45), (0x59, 0x67), (0x5A, 0x89), (0x5B, 0xAB),
    (0x5C, 0xCD), (0x5D, 0xEF), (0x5E, 0x11), (0x5F, 0x02), (0x60, 0x00), (0x61, 0x07), (0x62, 0x06), (0x63, 0x0E),
    (0x64, 0x0F), (0x65, 0x0C), (0x66, 0x0D), (0x67, 0x02), (0x68, 0x02), (0x69, 0x02), (0x6A, 0x02), (0x6B, 0x02),
    (0x6C, 0x02), (0x6D, 0x02), (0x6E, 0x02), (0x6F, 0x02), (0x70, 0x02), (0x71, 0x02), (0x72, 0x02), (0x73, 0x05),
    (0x74, 0x01), (0x75, 0x02), (0x76, 0x00), (0x77, 0x07), (0x78, 0x06), (0x79, 0x0E), (0x7A, 0x0F), (0x7B, 0x0C),
    (0x7C, 0x0D), (0x7D, 0x02), (0x7E, 0x02), (0x7F, 0x02), (0x80, 0x02), (0x81, 0x02), (0x82, 0x02), (0x83, 0x02),
    (0x84, 0x02), (0x85, 0x02), (0x86, 0x02), (0x87, 0x02), (0x88, 0x02), (0x89, 0x05), (0x8A, 0x01),
];

/// Page 4 (power).
const ILI9881C_PAGE4: &[(u8, u8)] =
    &[(0x38, 0x01), (0x39, 0x00), (0x6C, 0x15), (0x6E, 0x1A), (0x6F, 0x25), (0x3A, 0xA4), (0x8D, 0x20), (0x87, 0xBA), (0x3B, 0x98)];

/// Page 1 (power and gamma).
const ILI9881C_PAGE1: &[(u8, u8)] = &[
    (0x22, 0x0A), (0x31, 0x00), (0x50, 0x6B), (0x51, 0x66), (0x53, 0x73), (0x55, 0x8B), (0x60, 0x1B), (0x61, 0x01),
    (0x62, 0x0C), (0x63, 0x00), (0xA0, 0x00), (0xA1, 0x15), (0xA2, 0x1F), (0xA3, 0x13), (0xA4, 0x11), (0xA5, 0x21),
    (0xA6, 0x17), (0xA7, 0x1B), (0xA8, 0x6B), (0xA9, 0x1E), (0xAA, 0x2B), (0xAB, 0x5D), (0xAC, 0x19), (0xAD, 0x14),
    (0xAE, 0x4B), (0xAF, 0x1D), (0xB0, 0x27), (0xB1, 0x49), (0xB2, 0x5D), (0xB3, 0x39), (0xC0, 0x00), (0xC1, 0x01),
    (0xC2, 0x0C), (0xC3, 0x11), (0xC4, 0x15), (0xC5, 0x28), (0xC6, 0x1B), (0xC7, 0x1C), (0xC8, 0x62), (0xC9, 0x1C),
    (0xCA, 0x29), (0xCB, 0x60), (0xCC, 0x16), (0xCD, 0x17), (0xCE, 0x4A), (0xCF, 0x23), (0xD0, 0x24), (0xD1, 0x4F),
    (0xD2, 0x5F), (0xD3, 0x39),
];

fn ili9881c_init() -> Result<(), &'static str> {
    // The ID on page 1 (98 81), as M5GFX checks it; logged, not required.
    dcs_write(0xFF, &[0x98, 0x81, 0x01])?;
    let mut id = [0u8; 2];
    let r0 = dcs_read(0x00, &mut id[..1]);
    let r1 = dcs_read(0x01, &mut id[1..]);
    event!(Info, "display.id", id0 = id[0], id1 = id[1], ok = r0.is_ok() && r1.is_ok());
    for &(cmd, params, ms) in ILI9881C_INIT {
        dcs_write(cmd, params)?;
        delay_us(ms as u64 * 1000);
    }
    for &(r, v) in ILI9881C_PAGE3 {
        dcs_write(r, &[v])?;
    }
    dcs_write(0xFF, &[0x98, 0x81, 0x04])?;
    for &(r, v) in ILI9881C_PAGE4 {
        dcs_write(r, &[v])?;
    }
    dcs_write(0xFF, &[0x98, 0x81, 0x01])?;
    for &(r, v) in ILI9881C_PAGE1 {
        dcs_write(r, &[v])?;
    }
    dcs_write(0xFF, &[0x98, 0x81, 0x00])?;
    dcs_write(0x35, &[])?; // TEON
    dcs_write(0xFE, &[])?; // vendor command, in every source
    dcs_write(0x29, &[])?; // DISPON
    dcs_write(0x36, &[0x00])?; // MADCTL
    dcs_write(0x3A, &[0x55])?; // COLMOD: RGB565
    Ok(())
}

// ---- DMA and bridge ----

fn gdma_init() -> Result<(), &'static str> {
    set_bits(SOC_CLK_CTRL0, GDMA_CPU_CLK_EN);
    set_bits(SOC_CLK_CTRL1, GDMA_SYS_CLK_EN);
    set_bits(HP_RST_EN0, RST_EN_GDMA);
    clear_bits(HP_RST_EN0, RST_EN_GDMA);
    unsafe { wr(GDMA_RESET0, 1) };
    if !wait(1000, || unsafe { rd(GDMA_RESET0) } & 1 == 0) {
        return Err("DW-GDMA reset did not finish");
    }
    unsafe {
        wr(GDMA_CFG0, 0x3); // DMAC_EN, INT_EN
        wr(CH_CFG0, 0xF); // source and destination: linked list
        wr(CH_CFG1, 0x0A02_0001); // M->P, DMA flow control, DSI, priority 1
        wr(CH_INTSTATUS_ENABLE0, 0xFFFF_FFFF);
        // The channel's interrupt: a frame done, or the DMA stopped.
        wr(CH + 0x90, BLOCK_TFR_DONE | DMA_TFR_DONE | LLI_INVALID_ERR); // CH_INTSIGNAL_ENABLE0
    }
    Ok(())
}

fn bridge_init(t: &Timing) {
    let words = (FB_BYTES / 8) as u32;
    let htotal = t.hsw + t.hbp + WIDTH as u32 + t.hfp;
    let vtotal = t.vsw + t.vbp + HEIGHT as u32 + t.vfp;
    // Host: horizontal times in lane byte clocks (mipi_dsi_hal.c:234-249).
    let r = |px: u32| px * LANE_MBPS / (DPI_MHZ * 8);
    let (hsa, hbp) = (r(t.hsw), r(t.hbp));
    let hline = r(WIDTH as u32) + hsa + hbp + r(t.hfp);
    set_field(PERI_CLK_CTRL03, 5, 2, 1); // DPI clock: PLL_F240M
    set_field(PERI_CLK_CTRL03, 8, 8, 240 / DPI_MHZ - 1);
    set_bits(PERI_CLK_CTRL03, DPICLK_EN);
    unsafe {
        wr(DPI_VCID, 0);
        wr(DPI_COLOR_CODING, 0); // 16-bit RGB565
        wr(DPI_CFG_POL, 0);
        wr(VID_MODE_CFG, 0xFF02); // burst, LP in every period, frame ACK
        wr(VID_PKT_SIZE, WIDTH as u32);
        wr(VID_NUM_CHUNKS, 0);
        wr(VID_NULL_SIZE, 0);
        wr(VID_HSA_TIME, hsa);
        wr(VID_HBP_TIME, hbp);
        wr(VID_HLINE_TIME, hline);
        wr(VID_VSA_LINES, t.vsw);
        wr(VID_VBP_LINES, t.vbp);
        wr(VID_VFP_LINES, t.vfp);
        wr(VID_VACTIVE_LINES, HEIGHT as u32);

        wr(BRG_RAW_NUM_CFG, words | (1 << 31));
        wr(BRG_DPI_MISC_CONFIG, (WIDTH as u32) << 4);
        wr(BRG_PIXEL_TYPE, 0x2); // RGB565
        wr(BRG_DPI_H_CFG0, ((WIDTH as u32) << 16) | htotal);
        wr(BRG_DPI_H_CFG1, (t.hsw << 16) | t.hbp);
        wr(BRG_DPI_V_CFG0, ((HEIGHT as u32) << 16) | vtotal);
        wr(BRG_DPI_V_CFG1, (t.vsw << 16) | t.vbp);
        wr(BRG_DMA_FLOW_CTRL, 0x10); // the DMA controls the flow, one block
        clear_bits(BRG_DMA_FRAME_INTERVAL, DMA_MULTIBLK_EN);
        wr(BRG_DMA_REQ_CFG, 256);
        wr(BRG_RAW_BUF_ALMOST_EMPTY_THRD, 768);
        wr(BRG_EN, 1);
        wr(BRG_DPI_CONFIG_UPDATE, 1);
    }
    event!(Info, "display.timing", hsa = hsa, hbp = hbp, hline = hline, htotal = htotal, vtotal = vtotal, frame_words = words);
}

// ---- framebuffer ----

fn writeback(addr: usize, len: usize) {
    unsafe {
        let f: unsafe extern "C" fn(u32, u32, u32) -> i32 = core::mem::transmute(ROM_CACHE_WRITEBACK_ADDR);
        f(CACHE_MAP_L1_DCACHE, addr as u32, len as u32);
        f(CACHE_MAP_L2_CACHE, addr as u32, len as u32);
    }
}

const fn rgb565(r: u32, g: u32, b: u32) -> u16 {
    (((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)) as u16
}

/// Eight horizontal colour bands and a white frame: shows the colour order,
/// the orientation and whether the edges are cut off.
fn test_pattern(fb: usize) {
    const BANDS: [u16; 8] = [
        rgb565(255, 255, 255),
        rgb565(255, 255, 0),
        rgb565(0, 255, 255),
        rgb565(0, 255, 0),
        rgb565(255, 0, 255),
        rgb565(255, 0, 0),
        rgb565(0, 0, 255),
        rgb565(0, 0, 0),
    ];
    let px = unsafe { core::slice::from_raw_parts_mut(fb as *mut u16, WIDTH * HEIGHT) };
    for y in 0..HEIGHT {
        let row = &mut px[y * WIDTH..(y + 1) * WIDTH];
        let edge_row = y < 8 || y >= HEIGHT - 8;
        let band = BANDS[y * BANDS.len() / HEIGHT];
        for (x, p) in row.iter_mut().enumerate() {
            *p = if edge_row || x < 8 || x >= WIDTH - 8 { 0xFFFF } else { band };
        }
    }
    writeback(fb, FB_BYTES);
}

fn lli_addr(i: usize) -> usize {
    core::ptr::addr_of!(LLI) as usize + i * core::mem::size_of::<Lli>()
}

/// Item `i` through the non-cacheable alias: the DMA reads and writes the
/// items in memory, not through the cache.
fn lli_uncached(i: usize) -> *mut u32 {
    (lli_addr(i) + L2MEM_NON_CACHE) as *mut u32
}

/// Writes the ring (every item a whole frame, pointing to the next) and
/// starts the channel at item 0.
fn start(fb: usize) {
    let words = (FB_BYTES / 8) as u32;
    for i in 0..RING {
        let next = lli_addr((i + 1) % RING) as u32 | 1; // LMS = 1: in memory (master 1)
        let item = [fb as u32, 0, BRG_FIFO, 0, words - 1, 0, next, 0, LLI_CTL_LO, LLI_CTL_HI | IOC_BLKTFR | LLI_VALID];
        for (k, &w) in item.iter().enumerate() {
            unsafe { core::ptr::write_volatile(lli_uncached(i).add(k), w) };
        }
    }
    unsafe {
        wr(CH_LLP0, lli_addr(0) as u32 | 1);
        wr(CH_LLP1, 0);
        wr(GDMA_CHEN0, CH1_ENABLE);
    }
}

/// Marks every used (invalidated) item valid again.
fn revalidate() {
    for i in 0..RING {
        let w = unsafe { lli_uncached(i).add(LLI_CTL_HI_WORD) };
        unsafe {
            let v = core::ptr::read_volatile(w);
            if v & LLI_VALID == 0 {
                core::ptr::write_volatile(w, v | LLI_VALID);
            }
        }
    }
}

fn backlight(on: bool) {
    unsafe {
        wr(super::i2c::io_mux_reg(BL_PIN), 1 << 12);
        wr(super::i2c::func_out_sel(BL_PIN), SIG_GPIO_OUT | OUT_SEL_OEN);
        wr(if on { GPIO_OUT_W1TS } else { GPIO_OUT_W1TC }, 1 << BL_PIN);
        wr(GPIO_ENABLE_W1TS, 1 << BL_PIN);
    }
}

/// Brings the display up with the framebuffer at `fb` (`FB_BYTES`, in
/// PSRAM) showing a test pattern.
pub fn init(panel: Panel, fb: usize) -> Result<(), &'static str> {
    let timing = match panel {
        Panel::Ili9881c { .. } => &ILI9881C_TIMING,
        // ST7123/ST7121 tables are in m6-panel-touch.md §4.4-4.5; not yet
        // needed on the Tab5 at hand.
        _ => return Err("panel not supported yet"),
    };
    vo3_on()?;
    host_init()?;
    delay_us(80_000); // as M5GFX, before talking to the panel
    gdma_init()?;
    bridge_init(timing);
    ili9881c_init()?;
    event!(Info, "display.panel_ready");
    test_pattern(fb);
    unsafe {
        *core::ptr::addr_of_mut!(STATE) = Some(State { fb, shown: fb, frames: 0, underruns: 0, restarts: 0, started: Esp32p4v1::now() });
        wr(INTMTX_CORE0_BASE + 4 * GDMA_SOURCE, IRQ as u32);
        wr(CLIC_CTRL_BASE + 4 * IRQ, CLIC_CTRL);
    }
    start(fb);
    unsafe {
        wr(MODE_CFG, 0); // video mode
        wr(LPCLK_CTRL, 0x3); // clock lane: automatic HS
        set_bits(BRG_DPI_MISC_CONFIG, 1);
        wr(BRG_DPI_CONFIG_UPDATE, 1);
        wr(BRG_INT_ENA, BRG_UNDERRUN);
    }
    // After a debug probe's reset the frame DMA sometimes does not start
    // (`fifo` and `dma_status` stay 0, the screen keeps what it had; once
    // so, every probe reset after it too). Setting up the DMA, the bridge
    // and the 2D DMA's blocks again did not help; a software reset has
    // always cleared it (doc/17 §13).
    if !running() {
        stuck();
        if Esp32p4v1::reset_cause() != RESET_SOFTWARE {
            event!(Warn, "display.reset");
            Esp32p4v1::reset();
        }
    }
    delay_us(20_000); // a frame on screen before the light comes on
    backlight(true);
    let fifo = unsafe { rd(BRG_FIFO_FLOW_STATUS) };
    event!(
        Info,
        "display.started",
        fb = Addr(fb as u32),
        frames = state().map_or(0, |s| s.frames),
        dma_status = unsafe { rd(CH_STATUS0) },
        int_st1 = unsafe { rd(INT_ST1) },
        fifo = fifo
    );
    Ok(())
}

/// Whether the frame DMA has started: within 2 ms the bridge's FIFO fills
/// or the channel has moved data (a frame is ~21 ms).
fn running() -> bool {
    wait(2_000, || unsafe { rd(BRG_FIFO_FLOW_STATUS) != 0 || rd(CH_STATUS0) != 0 })
}

/// Logs the state of a frame DMA that did not start.
fn stuck() {
    event!(
        Warn,
        "display.dma_stuck",
        chen = unsafe { rd(GDMA_CHEN0) },
        cfg = unsafe { rd(GDMA_CFG0) },
        ch_cfg1 = unsafe { rd(CH_CFG1) },
        llp = unsafe { rd(CH_LLP0) },
        ch_int = unsafe { rd(CH_INTSTATUS0) },
        brg_en = unsafe { rd(BRG_EN) },
        brg_int = unsafe { rd(BRG_INT_RAW) },
        host_int0 = unsafe { rd(INT_ST0) },
        host_int1 = unsafe { rd(INT_ST1) },
        clk1 = unsafe { rd(SOC_CLK_CTRL1) },
        rst0 = unsafe { rd(HP_RST_EN0) }
    );
}

/// After frames: marks the used list items valid again; restarts the DMA
/// if it stopped at an invalid item anyway. Counts frames and underruns.
/// Called from the scheduler loop and on the DMA interrupt.
pub fn service() {
    let Some(s) = state() else { return };
    let st = unsafe { rd(CH_INTSTATUS0) };
    if st == 0 {
        return;
    }
    unsafe { wr(CH_INTCLEAR0, st) };
    if st & BLOCK_TFR_DONE != 0 {
        s.frames = s.frames.wrapping_add(1);
    }
    if st & (LLI_INVALID_ERR | DMA_TFR_DONE) != 0 {
        // Too late after all: the channel waits at an invalid item.
        s.restarts = s.restarts.wrapping_add(1);
        stop();
        start(s.shown);
    } else {
        revalidate();
    }
    if unsafe { rd(BRG_INT_RAW) } & BRG_UNDERRUN != 0 {
        unsafe { wr(BRG_INT_CLR, BRG_UNDERRUN) };
        s.underruns = s.underruns.wrapping_add(1);
    }
}

pub fn buffer() -> Option<usize> {
    state().map(|s| s.fb)
}

pub fn frames() -> u32 {
    state().map_or(0, |s| s.frames)
}

/// Writes back what the CPU drew in `[addr, addr + len)` (whole cache
/// lines: the range is widened to 128 bytes).
pub fn flush(addr: usize, len: usize) {
    let start = addr & !127;
    let end = (addr + len + 127) & !127;
    writeback(start, end - start);
}

/// Logs the frame count and rate.
pub fn log_stats() -> Result<u32, &'static str> {
    let s = state().ok_or("no display")?;
    let ms = ((Esp32p4v1::now() - s.started) / 16_000).max(1);
    event!(
        Info,
        "display.stats",
        frames = s.frames,
        underruns = s.underruns,
        restarts = s.restarts,
        ms = ms as u32,
        fps_x10 = (s.frames as u64 * 10_000 / ms) as u32,
        int_st1 = unsafe { rd(INT_ST1) }
    );
    Ok(s.frames)
}

/// Stops the frame DMA for a measurement (doc/43); the panel shows
/// underruns until `resume`. Returns whether it was running.
pub fn pause() -> bool {
    state().is_some() && stop()
}

pub fn resume() {
    if let Some(s) = state() {
        start(s.shown);
    }
}

/// Points every list item at the framebuffer `fb` (doc/60 §4): the DMA
/// sends it from the next item on, the frame in flight ends as it began.
pub fn show(fb: usize) {
    let Some(s) = state() else { return };
    s.shown = fb;
    for i in 0..RING {
        unsafe { core::ptr::write_volatile(lli_uncached(i), fb as u32) };
    }
}

/// Stops the DMA before a software reset: it would keep writing across the
/// reset (system_internal.c:66-70).
/// Returns whether the channel was running.
pub fn stop() -> bool {
    if unsafe { rd(SOC_CLK_CTRL1) } & GDMA_SYS_CLK_EN == 0 {
        return false;
    }
    let running = unsafe { rd(GDMA_CHEN0) } & 1 != 0;
    if running {
        unsafe { wr(GDMA_CHEN1, CH1_ENABLE) }; // abort channel 1
        let _ = wait(1000, || unsafe { rd(GDMA_CHEN0) } & 1 == 0);
    }
    running
}
