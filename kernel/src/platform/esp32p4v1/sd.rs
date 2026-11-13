// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The Tab5's microSD card (M7a, doc/18): the SDMMC host (Synopsys
//! DesignWare MMC) on slot 0's fixed IO_MUX pins, polled, one data line at
//! 400 kHz. Data moves by the host's internal DMA (IDMAC): CPU reads of the
//! FIFO window return the same word and do not pop the FIFO on this chip
//! (seen on the Tab5; ESP-IDF never uses the FIFO by CPU either).
//!
//! The card's power is wired to the SoC's 3.3 V rail and cannot be switched
//! (doc/research/m7-sdcard.md §1.2): nothing to enable, nothing to touch.
//! CMD11 (switch to 1.8 V) is never sent: only a power cycle would undo it.
//! Register addresses, values and the order are from
//! doc/research/m7-sdcard.md §2-§4 and §8 (ESP-IDF 6.0 esp_driver_sdmmc).

use core::sync::atomic::{AtomicU32, Ordering};

use t5_abi::event;

use super::i2c::{func_out_sel, io_mux_reg};
use super::psram::{clear_bits, delay_us, set_bits, set_field, wait};
use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;

// Clocks and reset (m7-sdcard.md §2.1).
const SOC_CLK_CTRL1: usize = 0x500E_6018;
const SDMMC_SYS_CLK_EN: u32 = 1 << 14;
const LP_SDMMC_EMAC_RST_CTRL: usize = 0x5011_104C;
const RST_EN_SDMMC: u32 = 1 << 28;
const REF_CLK_CTRL0: usize = 0x500E_6024;
const REF_CLK_CTRL2: usize = 0x500E_602C;
const REF_160M_CLK_EN: u32 = 1 << 0;
const PERI_CLK_CTRL01: usize = 0x500E_6034;
const SDIO_HS_MODE: u32 = 1 << 22;
const SDIO_LS_CLK_SRC_PLL_200M: u32 = 1 << 23;
const SDIO_LS_CLK_EN: u32 = 1 << 24;
const PERI_CLK_CTRL02: usize = 0x500E_6038;
const SDIO_CFG_UPDATE: u32 = 1 << 8;
/// SLF, DRV and SAM clock enables.
const SDIO_PHASE_CLK_EN: u32 = 0b111 << 27;

// Host registers (sdmmc_reg.h; m7-sdcard.md §2.2).
const HOST: usize = 0x5008_3000;
const CTRL: usize = HOST;
const CLKDIV: usize = HOST + 0x08;
const CLKSRC: usize = HOST + 0x0C;
const CLKENA: usize = HOST + 0x10;
const TMOUT: usize = HOST + 0x14;
const CTYPE: usize = HOST + 0x18;
const BLKSIZ: usize = HOST + 0x1C;
const BYTCNT: usize = HOST + 0x20;
const INTMASK: usize = HOST + 0x24;
const CMDARG: usize = HOST + 0x28;
const CMD: usize = HOST + 0x2C;
const RESP0: usize = HOST + 0x30;
const RINTSTS: usize = HOST + 0x44;
const STATUS: usize = HOST + 0x48;
const CDETECT: usize = HOST + 0x50;
const WRTPRT: usize = HOST + 0x54;
const VERID: usize = HOST + 0x6C;
const HCON: usize = HOST + 0x70;
const BMOD: usize = HOST + 0x80;
const PLDMND: usize = HOST + 0x84;
const DBADDR: usize = HOST + 0x88;
const IDSTS: usize = HOST + 0x8C;
const IDINTEN: usize = HOST + 0x90;
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_USE_IDMAC: u32 = 1 << 25;
const BMOD_SWR: u32 = 1 << 0;
const BMOD_FB: u32 = 1 << 1;
const BMOD_DE: u32 = 1 << 7;
const IDSTS_TI: u32 = 1 << 0;
const IDSTS_RI: u32 = 1 << 1;
/// FBE, DU, CES: bus error, descriptor unavailable, card error.
const IDSTS_ERRORS: u32 = (1 << 2) | (1 << 4) | (1 << 5);
// IDMAC descriptor DES0 bits.
const DES0_LD: u32 = 1 << 2;
const DES0_FS: u32 = 1 << 3;
const DES0_OWN: u32 = 1 << 31;
/// The non-cacheable alias of L2MEM, for the descriptor (as M6's DMA list).
const L2MEM_NON_CACHE: usize = 0x4000_0000;
const CLK_EDGE_SEL: usize = HOST + 0x800;

const CTRL_RESETS: u32 = 0b111; // controller, FIFO, DMA
const CTRL_INT_ENABLE: u32 = 1 << 4;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CMD_START: u32 = 1 << 31;

// RINTSTS bits.
const RE: u32 = 1 << 1;
const CMD_DONE: u32 = 1 << 2;
const DTO: u32 = 1 << 3;
const RCRC: u32 = 1 << 6;
const DCRC: u32 = 1 << 7;
const RTO: u32 = 1 << 8;
const DRTO: u32 = 1 << 9;
const HTO: u32 = 1 << 10;
const HLE: u32 = 1 << 12;
const SBE: u32 = 1 << 13;
const EBE: u32 = 1 << 15;
/// The transfer's bits (0-15); 16 + n is slot n's SDIO card interrupt,
/// which commands leave alone.
const TRANSFER_INTS: u32 = 0xFFFF;
const CMD_ERRORS: u32 = RTO | RCRC | RE | HLE;
const DATA_ERRORS: u32 = DRTO | DCRC | HTO | SBE | EBE;

/// RINTSTS and INTMASK bit of slot `slot`'s SDIO card interrupt: set on a
/// falling edge of its DAT1, the only interrupt the host raises (ESP-IDF
/// sdmmc_host.c:1043-1048).
const fn card_int(slot: u32) -> u32 {
    1 << (16 + slot)
}

/// Whether slot `slot`'s card interrupt came; clears it.
pub(super) fn card_int_take(slot: u32) -> bool {
    unsafe {
        let came = rd(RINTSTS) & card_int(slot) != 0;
        if came {
            wr(RINTSTS, card_int(slot));
        }
        came
    }
}

/// Whether slot `slot`'s card interrupt came (not cleared).
pub(super) fn card_int_came(slot: u32) -> bool {
    unsafe { rd(RINTSTS) & card_int(slot) != 0 }
}

/// Lets slot `slot`'s card interrupt reach the CPU (`on`) or not.
pub(super) fn card_int_enable(slot: u32, on: bool) {
    unsafe {
        let mask = rd(INTMASK) & !card_int(slot);
        wr(INTMASK, if on { mask | card_int(slot) } else { mask });
        if on {
            wr(CTRL, rd(CTRL) | CTRL_INT_ENABLE);
        }
    }
}

// Pins: CLK 43, CMD 44, D0-D3 39-42, IO_MUX function 0 (m7-sdcard.md §2.3).
const DATA_PINS: [usize; 3] = [39, 40, 41];
const D3: usize = 42;
const CLK_CMD_PINS: [usize; 2] = [43, 44];
/// MCU_SEL 0 (SD), drive 3, input enabled, no pulls (external 5.1 kOhm).
const IO_MUX_SD: u32 = 0x0E00;
const SIG_GPIO_OUT: u32 = 256;
const GPIO_ENABLE1_W1TC: usize = 0x500E_0034;
// Slot 0's card detect, write protect and interrupt inputs: constant
// "present", "writable", "no interrupt" (signals 126, 130, 128).
const IN_SEL_CD: usize = 0x500E_0350;
const IN_SEL_WP: usize = 0x500E_0360;
const IN_SEL_INT: usize = 0x500E_0358;
const IN_CONST_LOW: u32 = 0xBE;
const IN_CONST_HIGH: u32 = 0xBF;

// Command register values (slot 0, use_hold_reg; m7-sdcard.md §3.1).
const UPDATE_CLOCK: u32 = 0xA020_2000;
const CMD0: u32 = 0xA000_8000;
const CMD8: u32 = 0xA000_2148;
const CMD55: u32 = 0xA000_2177;
const ACMD41: u32 = 0xA000_2069;
const CMD2: u32 = 0xA000_21C2;
const CMD3: u32 = 0xA000_2143;
const CMD9: u32 = 0xA000_21C9;
const CMD7: u32 = 0xA000_2147;
const CMD13: u32 = 0xA000_214D;
const CMD16: u32 = 0xA000_2150;
const CMD17: u32 = 0xA000_2351;
const CMD18: u32 = 0xA000_3352;
const CMD24: u32 = 0xA000_2758;
const CMD25: u32 = 0xA000_3759;
const ACMD6: u32 = 0xA000_2146;
const ACMD51: u32 = 0xA000_2373;
const CMD6: u32 = 0xA000_2346;
/// STATUS.data_busy: the card holds D0 low while it programs.
const STATUS_DATA_BUSY: u32 = 1 << 9;
/// CMD13: READY_FOR_DATA and CURRENT_STATE = tran (4).
const R1_READY: u32 = 1 << 8;
const R1_STATE_TRAN: u32 = 4 << 9;

/// HCS and the 2.7-3.6 V window; never S18R (no 1.8 V).
const ACMD41_ARG: u32 = 0x40FF_8000;
const OCR_BUSY_DONE: u32 = 1 << 31;
const OCR_CCS: u32 = 1 << 30;

pub const SECTOR: usize = 512;

/// One IDMAC descriptor, cache-line aligned (IDF pads them to 64 bytes).
#[repr(C, align(64))]
struct Desc([u32; 16]);
static mut DESC: Desc = Desc([0; 16]);
/// The DMA's buffer in internal SRAM, whole cache lines.
#[repr(C, align(64))]
struct Bounce([u8; BOUNCE_SIZE]);
/// The longest transfer: 8 sectors.
pub const BOUNCE_SIZE: usize = 4096;
static mut BOUNCE: Bounce = Bounce([0; BOUNCE_SIZE]);

#[derive(Clone, Copy)]
pub struct Card {
    rca: u32,
    /// SDHC/SDXC: addressed in sectors (else in bytes).
    block_addressed: bool,
    pub sectors: u32,
}

static mut CARD: Option<Card> = None;

pub fn card() -> Option<Card> {
    unsafe { CARD }
}

// ---- host ----

/// The first divider of the 160 MHz source (sdmmc_ll_set_clock_div), then
/// the phase setup (sdmmc_ll_init_phase_delay).
fn set_source_div(div: u32) {
    set_field(PERI_CLK_CTRL02, 13, 4, div / 2 - 1); // EDGE_H
    set_field(PERI_CLK_CTRL02, 9, 4, div - 1); // EDGE_L
    set_field(PERI_CLK_CTRL02, 17, 4, div - 1); // EDGE_N
    set_bits(PERI_CLK_CTRL02, SDIO_CFG_UPDATE);
    clear_bits(PERI_CLK_CTRL02, SDIO_CFG_UPDATE);
}

/// The host is shared by the SD card (slot 0) and the Wi-Fi chip's SDIO
/// (slot 1, `sdio.rs`): it is set up once. The first clock stage is fixed
/// at 40 MHz; each slot divides it with its own divider (CLKDIV field,
/// CLKSRC), so one slot's clock changes never touch the other's (doc/22).
static mut HOST_UP: bool = false;
/// The first stage's divider of the 160 MHz source: 40 MHz.
const SOURCE_DIV: u32 = 4;

pub(super) fn host_up() -> Result<(), &'static str> {
    if unsafe { HOST_UP } {
        return Ok(());
    }
    let ref160_div = (unsafe { rd(REF_CLK_CTRL0) } >> 24) & 0xFF;
    let ref160_en = unsafe { rd(REF_CLK_CTRL2) } & REF_160M_CLK_EN != 0;
    if ref160_div != 2 || !ref160_en {
        event!(Error, "sd.no_clock", ref160_div = ref160_div, ref160_en = ref160_en);
        return Err("PLL_F160M not as expected");
    }
    set_bits(SOC_CLK_CTRL1, SDMMC_SYS_CLK_EN);
    set_bits(LP_SDMMC_EMAC_RST_CTRL, RST_EN_SDMMC);
    delay_us(10);
    clear_bits(LP_SDMMC_EMAC_RST_CTRL, RST_EN_SDMMC);
    set_source_div(SOURCE_DIV);
    clear_bits(PERI_CLK_CTRL01, SDIO_HS_MODE | SDIO_LS_CLK_SRC_PLL_200M);
    set_bits(PERI_CLK_CTRL01, SDIO_LS_CLK_EN);
    set_bits(PERI_CLK_CTRL02, SDIO_PHASE_CLK_EN);
    set_field(PERI_CLK_CTRL02, 23, 2, 1); // DRV edge
    set_field(PERI_CLK_CTRL02, 25, 2, 0); // SAM edge
    set_field(PERI_CLK_CTRL02, 21, 2, 0); // SLF edge
    set_bits(PERI_CLK_CTRL02, SDIO_CFG_UPDATE);
    clear_bits(PERI_CLK_CTRL02, SDIO_CFG_UPDATE);
    delay_us(10);

    unsafe { wr(CTRL, CTRL_RESETS) };
    if !wait(100_000, || unsafe { rd(CTRL) } & CTRL_RESETS == 0) {
        return Err("SDMMC host reset did not finish");
    }
    unsafe {
        wr(INTMASK, 0);
        wr(RINTSTS, TRANSFER_INTS);
        wr(IDSTS, 0x3FF);
        wr(CTYPE, 0); // one data line on every slot
        wr(CLKENA, 0);
        // IDMAC: reset, then on (sdmmc_ll_init_dma); polled through IDSTS.
        // IDSTS stays 0 unless IDINTEN enables the bits (seen: with 0 every
        // transfer timed out), so they also raise the host interrupt once
        // it is on; a deferred write is finished there (`finish_write`).
        wr(CTRL, rd(CTRL) | CTRL_DMA_ENABLE | CTRL_USE_IDMAC);
        wr(BMOD, BMOD_SWR);
        wr(IDINTEN, IDSTS_TI | IDSTS_RI | (1 << 8));
    }
    event!(
        Info,
        "sd.host",
        verid = unsafe { rd(VERID) },
        hcon = unsafe { rd(HCON) },
        clk_edge_sel = unsafe { rd(CLK_EDGE_SEL) },
        cdetect = unsafe { rd(CDETECT) },
        wrtprt = unsafe { rd(WRTPRT) }
    );
    unsafe { HOST_UP = true };
    Ok(())
}

/// Slot 0's pins and its 400 kHz clock for identification.
fn slot0_up() -> Result<(), &'static str> {
    host_up()?;
    unsafe {
        for p in DATA_PINS.iter().chain(&CLK_CMD_PINS) {
            wr(io_mux_reg(*p), IO_MUX_SD);
            wr(func_out_sel(*p), SIG_GPIO_OUT);
        }
        // D3 stays an input (pulled up) until 4-bit mode: a high D3 at CMD0
        // selects SD mode.
        wr(io_mux_reg(D3), (1 << 12) | (1 << 9));
        wr(GPIO_ENABLE1_W1TC, 1 << (D3 - 32));
        wr(IN_SEL_CD, IN_CONST_LOW);
        wr(IN_SEL_WP, IN_CONST_LOW);
        wr(IN_SEL_INT, IN_CONST_HIGH);
        wr(CTYPE, rd(CTYPE) & !1);
    }
    set_card_clock(0, 50, 400)
}

/// `slot`'s data bus: 4 lines or 1 (CTYPE bit `slot`).
pub(super) fn set_wide(slot: u32, four: bool) {
    finish_write();
    let claimed = claim_host();
    unsafe { wr(CTYPE, if four { rd(CTYPE) | 1 << slot } else { rd(CTYPE) & !(1 << slot) }) };
    release_host(claimed);
}

/// Sends the "update clock registers" command for `slot`.
fn update_clock(slot: u32) -> Result<(), &'static str> {
    finish_write();
    unsafe {
        wr(CMDARG, 0);
        wr(CMD, UPDATE_CLOCK | slot << 16);
    }
    if !wait(100_000, || unsafe { rd(CMD) } & CMD_START == 0) {
        return Err("SDMMC clock update not taken");
    }
    Ok(())
}

/// `slot`'s card clock: its own divider (`div2`: 40 MHz / (2 x div2),
/// 0 = 40 MHz) on the first stage; `khz` (what that gives) sets the data
/// timeout (m7-sdcard.md §3.2). The other slot's clock keeps running.
pub(super) fn set_card_clock(slot: u32, div2: u32, khz: u32) -> Result<(), &'static str> {
    let claimed = claim_host();
    let r = set_card_clock_owned(slot, div2, khz);
    release_host(claimed);
    r
}

fn set_card_clock_owned(slot: u32, div2: u32, khz: u32) -> Result<(), &'static str> {
    let bit = 1 << slot;
    unsafe { wr(CLKENA, rd(CLKENA) & !bit) };
    update_clock(slot)?;
    unsafe {
        let shift = 8 * slot;
        wr(CLKDIV, (rd(CLKDIV) & !(0xFF << shift)) | div2 << shift);
        // Slot n uses divider n.
        wr(CLKSRC, (rd(CLKSRC) & !(0b11 << (2 * slot))) | slot << (2 * slot));
    }
    update_clock(slot)?;
    unsafe { wr(CLKENA, rd(CLKENA) | bit) }; // running, not stopped when idle
    update_clock(slot)?;
    unsafe { wr(TMOUT, ((100 * khz).min(0xFF_FFFF) << 8) | 255) };
    event!(Info, "sd.clock", slot = slot, khz = khz);
    Ok(())
}

// ---- commands ----

/// The data phase of a command, through the IDMAC and the bounce buffer
/// (at most its size).
pub(super) enum Data<'a> {
    None,
    Read(&'a mut [u8]),
    /// A read of this many bytes, handed to the closure where the DMA put
    /// them (no copy; doc/35 §2).
    ReadWith(usize, &'a mut dyn FnMut(&[u8])),
    Write(&'a [u8]),
    /// A write of these pieces one after another, gathered straight into
    /// the bounce buffer.
    WriteParts(&'a [&'a [u8]]),
}

impl Data<'_> {
    pub(super) fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::Read(b) => b.len(),
            Data::ReadWith(n, _) => *n,
            Data::Write(b) => b.len(),
            Data::WriteParts(parts) => parts.iter().map(|p| p.len()).sum(),
        }
    }

    pub(super) fn is_write(&self) -> bool {
        matches!(self, Data::Write(_) | Data::WriteParts(_))
    }
}

/// The core using the host (1 + hart), or 0. A command keeps it while its
/// data phase is waited for without the kernel lock (`wait_dma`); the
/// other core, wanting the host meanwhile, lets the lock go until it is
/// free (doc/34 §9). Changed only with the kernel lock held.
static HOST_OWNER: AtomicU32 = AtomicU32::new(0);

/// Takes the host for this core; true if it was not already this core's
/// (the caller then gives it back with `release_host`).
fn claim_host() -> bool {
    let me = (crate::smp::hartid() & 1) as u32 + 1;
    loop {
        match HOST_OWNER.load(Ordering::Acquire) {
            o if o == me => return false,
            0 => {
                HOST_OWNER.store(me, Ordering::Release);
                return true;
            }
            _ => crate::smp::pause(),
        }
    }
}

fn release_host(claimed: bool) {
    if claimed {
        HOST_OWNER.store(0, Ordering::Release);
    }
}

/// Data phases at least this long are waited for without the kernel lock.
const UNLOCKED_WAIT_MIN: usize = 512;
/// How long a data phase may take: 1 s at 360 MHz (longer at a lower clock).
const DMA_TIMEOUT_CYCLES: u32 = 360_000_000;

/// Waits for the DMA of a data phase: `done_bit` in IDSTS and the end of
/// the data (DTO), or an error; returns (ended, IDSTS, RINTSTS). With
/// `unlocked` the kernel lock is let go meanwhile: a 1.5 KB frame takes
/// about 80 µs, which the other core spent waiting for the lock (doc/34
/// §9). So it runs from RAM and reads only the host's registers.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
fn wait_dma(done_bit: u32, unlocked: bool) -> (bool, u32, u32) {
    if unlocked {
        crate::smp::unlock_for_wait();
    }
    let start = crate::arch::riscv32::cycles();
    let (mut ids, mut ints);
    let done = loop {
        ids = unsafe { core::ptr::read_volatile(IDSTS as *const u32) };
        ints = unsafe { core::ptr::read_volatile(RINTSTS as *const u32) };
        if ids & (done_bit | IDSTS_ERRORS) != 0 && ints & (DTO | DATA_ERRORS) != 0 || ints & DATA_ERRORS != 0 {
            break true;
        }
        if crate::arch::riscv32::cycles().wrapping_sub(start) > DMA_TIMEOUT_CYCLES {
            break false;
        }
    };
    if unlocked {
        crate::smp::relock_after_wait();
    }
    (done, ids, ints)
}

/// `command_on` for the SD card's slot.
fn command(cmd: u32, arg: u32, data: Data) -> Result<[u32; 4], &'static str> {
    command_on(0, SECTOR, cmd, arg, data)
}

/// Issues `cmd` (its register value for slot 0) to `slot` with `arg` and
/// its data phase (blocks of at most `block` bytes). Returns the four
/// response words (RESP0 = bits 31:0).
/// Per slot: time (ticks) spent in commands and how many (doc/31 §4).
static mut BUSY: [(u64, u32); 2] = [(0, 0); 2];

/// (ticks in commands, commands) of slot `slot` since boot.
pub(super) fn busy(slot: u32) -> (u64, u32) {
    unsafe { (*core::ptr::addr_of!(BUSY))[slot as usize & 1] }
}

pub(super) fn command_on(slot: u32, block: usize, cmd: u32, arg: u32, data: Data) -> Result<[u32; 4], &'static str> {
    let claimed = claim_host();
    let start = Esp32p4v1::now();
    let r = command_timed(slot, block, cmd, arg, data);
    release_host(claimed);
    unsafe {
        let b = &mut (*core::ptr::addr_of_mut!(BUSY))[slot as usize & 1];
        b.0 += Esp32p4v1::now() - start;
        b.1 += 1;
    }
    r
}

/// The slot whose writes end without waiting for their data phase: the
/// C6 (`sdio.rs`). A Wi-Fi frame then goes out while the caller goes on
/// (doc/34). The SD card's writes stay whole, so a write that returns is
/// on the card.
const DEFER_SLOT: u32 = 1;

/// A write of `DEFER_SLOT` whose data phase may still run: the card took
/// the command and the IDMAC moves the data from the bounce buffer.
static mut WRITE_IN_FLIGHT: bool = false;

/// Ends a deferred write before the host is used again, or when its DMA
/// raised the host interrupt (TI): waits for its DMA and for the card to
/// finish taking the data, and clears TI. A failure is logged; the frame
/// it carried is lost.
pub(super) fn finish_write() {
    if !unsafe { WRITE_IN_FLIGHT } {
        return;
    }
    let claimed = claim_host();
    // Another core may have finished it while this one waited for the host.
    if !core::mem::replace(unsafe { &mut *core::ptr::addr_of_mut!(WRITE_IN_FLIGHT) }, false) {
        release_host(claimed);
        return;
    }
    let r = dma_finish(IDSTS_TI, UNLOCKED_WAIT_MIN).and_then(|_| {
        if wait(5_000_000, || unsafe { rd(STATUS) } & STATUS_DATA_BUSY == 0) { Ok(()) } else { Err("card stayed busy") }
    });
    unsafe { wr(RINTSTS, TRANSFER_INTS) };
    release_host(claimed);
    if let Err(e) = r {
        event!(Warn, "sd.write_failed", slot = DEFER_SLOT, error = e);
    }
}

/// Whether a deferred write's DMA still runs (nothing waits). Its end
/// raises the host interrupt.
pub(super) fn write_busy() -> bool {
    if !unsafe { WRITE_IN_FLIGHT } {
        return false;
    }
    let (ids, ints) = unsafe { (rd(IDSTS), rd(RINTSTS)) };
    !(ids & (IDSTS_TI | IDSTS_ERRORS) != 0 && ints & (DTO | DATA_ERRORS) != 0 || ints & DATA_ERRORS != 0)
}

fn command_timed(slot: u32, block: usize, cmd: u32, arg: u32, data: Data) -> Result<[u32; 4], &'static str> {
    // The bounce buffer and the host are free again.
    finish_write();
    if !wait(1_000_000, || unsafe { rd(CMD) } & CMD_START == 0) {
        return Err("SDMMC busy");
    }
    let len = data.len();
    if len > BOUNCE_SIZE {
        return Err("transfer too long");
    }
    unsafe {
        wr(RINTSTS, TRANSFER_INTS);
        if len > 0 {
            wr(CTRL, rd(CTRL) | CTRL_FIFO_RESET);
            let _ = wait(10_000, || rd(CTRL) & CTRL_FIFO_RESET == 0);
            wr(BLKSIZ, len.min(block) as u32);
            wr(BYTCNT, len as u32);
            let mut dst = core::ptr::addr_of_mut!(BOUNCE).cast::<u8>();
            let one;
            let parts: &[&[u8]] = match &data {
                Data::Write(b) => {
                    one = [*b];
                    &one
                }
                Data::WriteParts(parts) => parts,
                _ => &[],
            };
            for p in parts {
                core::ptr::copy_nonoverlapping(p.as_ptr(), dst, p.len());
                dst = dst.add(p.len());
            }
            dma_start(len);
        }
        wr(CMDARG, arg);
        wr(CMD, cmd | slot << 16);
    }
    if !wait(1_000_000, || unsafe { rd(CMD) } & CMD_START == 0) {
        return Err("SDMMC command not taken");
    }
    let mut ints = 0;
    if !wait(1_000_000, || {
        ints = unsafe { rd(RINTSTS) };
        ints & (CMD_DONE | CMD_ERRORS) != 0
    }) {
        return Err("SDMMC command timed out");
    }
    if ints & RTO != 0 {
        return Err("no response");
    }
    if ints & CMD_ERRORS != 0 {
        event!(Warn, "sd.cmd_error", slot = slot, cmd = cmd & 0x3F, rintsts = ints, status = unsafe { rd(STATUS) });
        return Err("response error");
    }
    let resp = unsafe { [rd(RESP0), rd(RESP0 + 4), rd(RESP0 + 8), rd(RESP0 + 12)] };
    match data {
        Data::None => {}
        Data::Read(buf) => {
            dma_finish(IDSTS_RI, len)?;
            buf.copy_from_slice(bounce_read(len));
        }
        Data::ReadWith(_, f) => {
            dma_finish(IDSTS_RI, len)?;
            f(bounce_read(len));
        }
        Data::Write(_) | Data::WriteParts(_) if slot == DEFER_SLOT => {
            unsafe { WRITE_IN_FLIGHT = true };
            return Ok(resp);
        }
        Data::Write(_) | Data::WriteParts(_) => {
            dma_finish(IDSTS_TI, len)?;
            // The card programs with D0 low (up to 250 ms per block by the
            // spec; 5 s as ESP-IDF allows).
            if !wait(5_000_000, || unsafe { rd(STATUS) } & STATUS_DATA_BUSY == 0) {
                return Err("card stayed busy");
            }
        }
    }
    unsafe { wr(RINTSTS, TRANSFER_INTS) };
    Ok(resp)
}

/// Sets the IDMAC up to move `len` bytes to or from the bounce buffer.
fn dma_start(len: usize) {
    let buf = core::ptr::addr_of!(BOUNCE) as usize;
    let desc = core::ptr::addr_of!(DESC) as usize;
    // Nothing of the buffer may be dirty in the cache, or a later write-back
    // would overwrite what the DMA put there.
    super::cache_writeback(buf, len.next_multiple_of(64), false);
    let d = (desc + L2MEM_NON_CACHE) as *mut u32;
    unsafe {
        core::ptr::write_volatile(d, DES0_OWN | DES0_FS | DES0_LD);
        core::ptr::write_volatile(d.add(1), len as u32);
        core::ptr::write_volatile(d.add(2), buf as u32);
        core::ptr::write_volatile(d.add(3), 0);
        wr(IDSTS, 0x3FF);
        wr(DBADDR, desc as u32);
        wr(CTRL, rd(CTRL) | CTRL_DMA_ENABLE | CTRL_USE_IDMAC);
        wr(BMOD, BMOD_DE | BMOD_FB);
        wr(PLDMND, 1);
    }
}

/// Waits for the DMA (`done_bit`: RI for a read, TI for a write) and the
/// end of the data phase.
fn dma_finish(done_bit: u32, len: usize) -> Result<(), &'static str> {
    let unlocked = len >= UNLOCKED_WAIT_MIN && crate::smp::two_cores();
    let (done, ids, ints) = wait_dma(done_bit, unlocked);
    if !done || ids & IDSTS_ERRORS != 0 || ints & DATA_ERRORS != 0 {
        event!(Warn, "sd.dma_error", idsts = ids, rintsts = ints, status = unsafe { rd(STATUS) }, done = done);
        unsafe {
            wr(CTRL, rd(CTRL) | (1 << 2)); // dma_reset
            let _ = wait(10_000, || rd(CTRL) & (1 << 2) == 0);
            wr(BMOD, 0);
            wr(IDSTS, 0x3FF);
        }
        return Err("data error");
    }
    unsafe { wr(IDSTS, 0x3FF) };
    Ok(())
}

/// The first `len` bytes a read's DMA put in the bounce buffer (valid
/// until the host's next command).
fn bounce_read(len: usize) -> &'static [u8] {
    let buf = core::ptr::addr_of!(BOUNCE) as usize;
    super::cache_invalidate(buf, len.next_multiple_of(64), false);
    unsafe { core::slice::from_raw_parts(buf as *const u8, len) }
}

fn app_command(rca: u32, cmd: u32, arg: u32, data: Data) -> Result<[u32; 4], &'static str> {
    command(CMD55, rca << 16, Data::None)?;
    command(cmd, arg, data)
}

/// Waits until the card is back in the transfer state and ready for data
/// (after a write).
fn wait_ready(rca: u32) -> Result<(), &'static str> {
    let end = Esp32p4v1::now() + 16 * 5_000_000;
    loop {
        let r = command(CMD13, rca << 16, Data::None)?[0];
        if r & R1_READY != 0 && r & (0xF << 9) == R1_STATE_TRAN {
            return Ok(());
        }
        if Esp32p4v1::now() > end {
            event!(Warn, "sd.not_ready", status = r);
            return Err("card not ready");
        }
    }
}

/// Capacity in sectors from the CSD (versions 1 and 2).
fn csd_sectors(csd: [u32; 4]) -> u32 {
    let bits = |hi: u32, lo: u32| -> u32 {
        // CSD bits [hi:lo] (at most 32 wide); RESP3 holds bits 127:96.
        let mut v = 0u64;
        for b in (lo..=hi).rev() {
            let word = csd[(b / 32) as usize];
            v = (v << 1) | ((word >> (b % 32)) & 1) as u64;
        }
        v as u32
    };
    match bits(127, 126) {
        1 => (bits(69, 48) + 1) * 1024,
        0 => {
            let c_size = bits(73, 62);
            let mult = bits(49, 47);
            let read_bl_len = bits(83, 80);
            ((c_size + 1) << (mult + 2)) << read_bl_len >> 9
        }
        _ => 0,
    }
}

/// Initialises the host and the card (m7-sdcard.md §8 steps 2-16).
pub fn init() -> Result<Card, &'static str> {
    if let Some(c) = card() {
        return Ok(c);
    }
    super::board::sd_power_on();
    slot0_up()?;
    delay_us(1000);
    command(CMD0, 0, Data::None)?;
    delay_us(20_000);
    // CMD8: version 2 cards answer with the check pattern.
    let v2 = match command(CMD8, 0x1AA, Data::None) {
        Ok(r) if r[0] & 0xFFF == 0x1AA => true,
        Ok(r) => {
            event!(Warn, "sd.if_cond", resp = r[0]);
            return Err("bad CMD8 answer");
        }
        Err("no response") => false,
        Err(e) => return Err(e),
    };
    let arg = if v2 { ACMD41_ARG } else { ACMD41_ARG & !OCR_CCS };
    let mut ocr = 0;
    let mut tries = 0;
    while tries < 300 {
        tries += 1;
        ocr = app_command(0, ACMD41, arg, Data::None)?[0];
        if ocr & OCR_BUSY_DONE != 0 {
            break;
        }
        delay_us(10_000);
    }
    let block_addressed = ocr & OCR_CCS != 0;
    event!(Info, "sd.ocr", ocr = ocr, tries = tries, v2 = v2, ccs = block_addressed);
    if ocr & OCR_BUSY_DONE == 0 {
        return Err("card did not power up");
    }
    let cid = command(CMD2, 0, Data::None)?;
    let rca = command(CMD3, 0, Data::None)?[0] >> 16;
    let csd = command(CMD9, rca << 16, Data::None)?;
    let sectors = csd_sectors(csd);
    // CID: manufacturer [127:120], OEM [119:104], name [103:64].
    let name = [(cid[3] & 0xFF) as u8, (cid[2] >> 24) as u8, (cid[2] >> 16) as u8, (cid[2] >> 8) as u8, cid[2] as u8];
    event!(
        Info,
        "sd.card",
        mid = cid[3] >> 24,
        oid = (cid[3] >> 8) & 0xFFFF,
        name = core::str::from_utf8(&name).unwrap_or("?"),
        rca = rca,
        csd_ver = csd[3] >> 30,
        sectors = sectors,
        mib = sectors / 2048
    );
    command(CMD7, rca << 16, Data::None)?;
    if !block_addressed {
        command(CMD16, SECTOR as u32, Data::None)?;
    }
    let c = Card { rca, block_addressed, sectors };
    let mut before = [0u8; SECTOR];
    command(CMD17, 0, Data::Read(&mut before))?;
    speed_up(rca)?;
    // The same sector at the new width and clock (m7-sdcard.md §8 step 22).
    let mut after = [0u8; SECTOR];
    command(CMD17, 0, Data::Read(&mut after))?;
    if before != after {
        event!(Error, "sd.verify_failed");
        return Err("sector 0 differs after the speed change");
    }
    unsafe { CARD = Some(c) };
    Ok(c)
}

/// SCR, then 4 data lines and 20 MHz, then High Speed (40 MHz) if the card
/// switches (m7-sdcard.md §4 steps 9-13). 3.3 V only.
fn speed_up(rca: u32) -> Result<(), &'static str> {
    let mut scr = [0u8; 8];
    app_command(rca, ACMD51, 0, Data::Read(&mut scr))?;
    let spec = scr[0] & 0xF;
    let four_bit = scr[1] & 0x04 != 0;
    if four_bit {
        app_command(rca, ACMD6, 2, Data::None)?;
        unsafe {
            wr(CTYPE, rd(CTYPE) | 1);
            wr(io_mux_reg(D3), IO_MUX_SD);
            wr(func_out_sel(D3), SIG_GPIO_OUT);
        }
    }
    set_card_clock(0, 1, 20_000)?;
    // CMD6 mode 1, group 1 function 1 (High Speed); the answer's bits
    // 379:376 say which function is now selected.
    let mut high_speed = false;
    if spec >= 1 {
        let mut st = [0u8; 64];
        if command(CMD6, 0x80FF_FFF1, Data::Read(&mut st)).is_ok() && st[16] & 0xF == 1 {
            set_card_clock(0, 0, 40_000)?;
            high_speed = true;
        }
    }
    event!(Info, "sd.bus", scr_spec = spec, four_bit = four_bit, high_speed = high_speed);
    Ok(())
}

fn address(c: &Card, lba: u32) -> u32 {
    if c.block_addressed { lba } else { lba * SECTOR as u32 }
}

/// Reads `buf.len() / 512` sectors from `lba` (at most `BOUNCE_SIZE`).
pub fn read_blocks(lba: u32, buf: &mut [u8]) -> Result<(), &'static str> {
    let c = init()?;
    let n = buf.len() / SECTOR;
    if buf.len() % SECTOR != 0 || n == 0 || lba as u64 + n as u64 > c.sectors as u64 {
        return Err("bad sector range");
    }
    let cmd = if n == 1 { CMD17 } else { CMD18 };
    command(cmd, address(&c, lba), Data::Read(buf))?;
    Ok(())
}

/// Writes `data.len() / 512` sectors at `lba` (at most `BOUNCE_SIZE`) and
/// waits until the card has programmed them.
pub fn write_blocks(lba: u32, data: &[u8]) -> Result<(), &'static str> {
    let c = init()?;
    let n = data.len() / SECTOR;
    if data.len() % SECTOR != 0 || n == 0 || lba as u64 + n as u64 > c.sectors as u64 {
        return Err("bad sector range");
    }
    let cmd = if n == 1 { CMD24 } else { CMD25 };
    command(cmd, address(&c, lba), Data::Write(data))?;
    wait_ready(c.rca)
}

/// Reads sector `lba`.
pub fn read(lba: u32, buf: &mut [u8; SECTOR]) -> Result<(), &'static str> {
    read_blocks(lba, &mut buf[..])
}

/// The sectors `sdtest` may overwrite: in the gap between the MBR and the
/// first partition of the card at hand (doc/18 §9). Saved and restored.
const TEST_LBA: u32 = 8192;

/// `sdtest`: write and read back 8 sectors at `TEST_LBA` (restoring what
/// was there), then time 256 KiB of reads and of writes.
pub fn test() -> Result<u32, &'static str> {
    init()?;
    let mut saved = [0u8; BOUNCE_SIZE];
    read_blocks(TEST_LBA, &mut saved)?;
    let mut pattern = [0u8; BOUNCE_SIZE];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i * 7 + (i >> 9)) as u8;
    }
    write_blocks(TEST_LBA, &pattern)?;
    let mut back = [0u8; BOUNCE_SIZE];
    read_blocks(TEST_LBA, &mut back)?;
    let ok = back == pattern;
    let rounds = 64u32; // 64 x 4 KiB
    let t = Esp32p4v1::now();
    for i in 0..rounds {
        read_blocks(16384 + i * 8, &mut back)?;
    }
    let read_us = ((Esp32p4v1::now() - t) / 16) as u32;
    let t = Esp32p4v1::now();
    for _ in 0..rounds {
        write_blocks(TEST_LBA, &pattern)?;
    }
    let write_us = ((Esp32p4v1::now() - t) / 16) as u32;
    write_blocks(TEST_LBA, &saved)?;
    read_blocks(TEST_LBA, &mut back)?;
    let restored = back == saved;
    let kib = rounds * 4;
    event!(
        Info,
        "sd.test",
        verified = ok,
        restored = restored,
        read_kib_s = (kib as u64 * 1_000_000 / read_us.max(1) as u64) as u32,
        write_kib_s = (kib as u64 * 1_000_000 / write_us.max(1) as u64) as u32
    );
    if ok && restored { Ok(kib) } else { Err("read back differs") }
}

/// `sdinfo`: initialises the card if needed and logs sector 0's partition
/// table (MBR) or, without one, whether it starts with a FAT boot sector.
pub fn info() -> Result<u32, &'static str> {
    let c = init()?;
    let mut s = [0u8; SECTOR];
    let t = Esp32p4v1::now();
    read(0, &mut s)?;
    let us = ((Esp32p4v1::now() - t) / 16) as u32;
    let sig = u16::from_le_bytes([s[510], s[511]]);
    let fat_boot = (s[0] == 0xEB || s[0] == 0xE9) && (&s[82..87] == b"FAT32" || &s[54..59] == b"FAT16" || &s[54..59] == b"FAT12");
    event!(Info, "sd.sector0", rca = c.rca, sig = sig, fat_boot_sector = fat_boot, read_us = us);
    for i in 0..4 {
        let e = &s[446 + 16 * i..446 + 16 * (i + 1)];
        let kind = e[4];
        if kind != 0 {
            let start = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
            let len = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
            event!(Info, "sd.partition", index = i as u32, kind = kind, start = start, sectors = len, mib = len / 2048);
        }
    }
    Ok(c.sectors)
}

/// `sdread LBA`: logs one sector in 32-byte pieces.
pub fn dump(lba: u32) -> Result<u32, &'static str> {
    let mut s = [0u8; SECTOR];
    read(lba, &mut s)?;
    for (i, chunk) in s.chunks(32).enumerate() {
        event!(Info, "sd.bytes", lba = lba, offset = (i * 32) as u32, data = chunk);
    }
    Ok(lba)
}
