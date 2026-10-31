//! The ESP32-C6 Wi-Fi chip on SDMMC slot 1 (N2b, doc/22): power, reset and
//! the SDIO card's identification. Pins through the GPIO matrix: CLK 12,
//! CMD 13, D0-D3 11/10/9/8; CHIP_EN = GPIO15 (doc/research/net-stack.md
//! §1.1). The host is shared with the SD card (`sd.rs`); everything here
//! uses slot 1's divider and card number only.
//!
//! The C6's firmware (esp_hosted, as M5Stack or TCC left it) is not
//! changed (user, 2026-10-31).

use t5_abi::event;

use super::i2c::{func_in_sel, func_out_sel, io_mux_reg};
use super::psram::delay_us;
use super::sd::{self, Data};
use super::{Esp32p4v1, Platform, tab5, wr};

const SLOT: u32 = 1;
const CLK_PIN: usize = 12;
const CMD_PIN: usize = 13;
const DATA_PINS: [usize; 4] = [11, 10, 9, 8];
const CHIP_EN: usize = 15;

// GPIO matrix signals of SDMMC slot 1 (gpio_sig_map.h: SD_CARD_*_2_PAD).
const SIG_CCLK: u32 = 0;
const SIG_CCMD: u32 = 1;
const SIG_CDATA0: u32 = 2;
const SIG_CD: u32 = 127;
const SIG_INT: u32 = 129;
const SIG_WP: u32 = 131;
const GPIO_SIG_IN_SEL: u32 = 1 << 7;
const IN_CONST_LOW: u32 = 0xBE;
const IN_CONST_HIGH: u32 = 0xBF;
const SIG_GPIO_OUT: u32 = 256;
/// MCU_SEL 1 (GPIO matrix), drive 3, input enabled, pull-up.
const IO_MUX_MATRIX: u32 = (1 << 12) | (3 << 10) | (1 << 9) | (1 << 8);
const GPIO_OUT_W1TS: usize = 0x500E_0008;
const GPIO_OUT_W1TC: usize = 0x500E_000C;
const GPIO_ENABLE_W1TS: usize = 0x500E_0024;

// Command register values for slot 0 (the slot is added by `command_on`):
// start, use_hold_reg, wait_prvdata_complete, response expected, CRC check.
const CMD0: u32 = 0xA000_8000;
/// IO_SEND_OP_COND: R4, no CRC.
const CMD5: u32 = 0xA000_2045;
const CMD3: u32 = 0xA000_2143;
const CMD7: u32 = 0xA000_2147;
/// IO_RW_DIRECT: R5.
const CMD52: u32 = 0xA000_2174;

/// R4: card ready, number of I/O functions, memory present, OCR.
const R4_READY: u32 = 1 << 31;
/// 3.2-3.4 V (bits 20, 21): the C6 runs at 3.3 V.
const OCR_33V: u32 = 0x0030_0000;
/// R5 flags [15:8]: COM_CRC_ERROR, ILLEGAL_COMMAND, ERROR, FUNCTION_NUMBER,
/// OUT_OF_RANGE.
const R5_ERRORS: u32 = 0xCB00;

// CCCR registers (SDIO spec 6.9).
const CCCR_IO_ENABLE: u32 = 0x02;
const CCCR_IO_READY: u32 = 0x03;
const CCCR_IO_ABORT: u32 = 0x06;
const CCCR_LEN: usize = 0x14;
const FUNC1: u8 = 1 << 1;

fn ms(n: u64) {
    // In 100 ms steps, feeding the watchdog (power-up waits are 1.5 s).
    let mut left = n;
    while left > 0 {
        let step = left.min(100);
        delay_us(step * 1000);
        Esp32p4v1::feed_watchdog();
        left -= step;
    }
}

fn cmd(c: u32, arg: u32) -> Result<u32, &'static str> {
    sd::command_on(SLOT, 512, c, arg, Data::None).map(|r| r[0])
}

/// CMD52: one byte of function `func`'s register `addr`; `write` stores
/// it and returns what the card reads back.
fn rw_direct(func: u32, addr: u32, write: Option<u8>) -> Result<u8, &'static str> {
    let arg = match write {
        Some(v) => 1 << 31 | func << 28 | 1 << 27 | addr << 9 | v as u32,
        None => func << 28 | addr << 9,
    };
    let r = cmd(CMD52, arg)?;
    if r & R5_ERRORS != 0 {
        event!(Warn, "wlan.cmd52_error", func = func, addr = addr, r5 = r);
        return Err("CMD52 error");
    }
    Ok(r as u8)
}

fn pins() {
    unsafe {
        wr(io_mux_reg(CLK_PIN), IO_MUX_MATRIX);
        wr(func_out_sel(CLK_PIN), SIG_CCLK);
        let io = core::iter::once((CMD_PIN, SIG_CCMD)).chain(DATA_PINS.iter().enumerate().map(|(i, &p)| (p, SIG_CDATA0 + i as u32)));
        for (pin, sig) in io {
            wr(io_mux_reg(pin), IO_MUX_MATRIX);
            // The host drives the output enable (OE_SEL 0).
            wr(func_out_sel(pin), sig);
            wr(func_in_sel(sig), pin as u32 | GPIO_SIG_IN_SEL);
        }
        wr(func_in_sel(SIG_CD), IN_CONST_LOW);
        wr(func_in_sel(SIG_WP), IN_CONST_LOW);
        wr(func_in_sel(SIG_INT), IN_CONST_HIGH);
    }
}

fn chip_en(high: bool) {
    unsafe {
        wr(io_mux_reg(CHIP_EN), (1 << 12) | (1 << 9));
        wr(func_out_sel(CHIP_EN), SIG_GPIO_OUT);
        wr(if high { GPIO_OUT_W1TS } else { GPIO_OUT_W1TC }, 1 << CHIP_EN);
        wr(GPIO_ENABLE_W1TS, 1 << CHIP_EN);
    }
}

/// `wlanprobe`: powers the C6 (if needed), resets it, identifies the SDIO
/// card at 400 kHz, logs the CCCR and enables function 1 (N2b-1).
pub fn probe() -> Result<u32, &'static str> {
    let was_on = tab5::wlan_power_on()?;
    // A reset pulse on CHIP_EN, as the esp_hosted host does; then the time
    // TCC gives the C6 to boot (wifi_setup.cpp:117-121).
    chip_en(false);
    ms(if was_on { 50 } else { 300 });
    chip_en(true);
    ms(1500);

    sd::host_up()?;
    pins();
    sd::set_card_clock(SLOT, 50, 400)?;
    delay_us(1000);
    // Ignored by a card that has just started; resets one that was in use.
    let _ = rw_direct(0, CCCR_IO_ABORT, Some(0x08));
    let _ = cmd(CMD0, 0);
    delay_us(20_000);
    let r4 = cmd(CMD5, 0)?;
    let functions = (r4 >> 28) & 7;
    event!(Info, "wlan.ocr", r4 = r4, functions = functions, memory = r4 & (1 << 27) != 0);
    let mut tries = 0;
    let mut r = r4;
    while r & R4_READY == 0 {
        tries += 1;
        if tries > 100 {
            return Err("SDIO card did not become ready");
        }
        r = cmd(CMD5, r4 & OCR_33V)?;
        delay_us(10_000);
    }
    let rca = cmd(CMD3, 0)? >> 16;
    cmd(CMD7, rca << 16)?;
    let mut cccr = [0u8; CCCR_LEN];
    for (i, b) in cccr.iter_mut().enumerate() {
        *b = rw_direct(0, i as u32, None)?;
    }
    event!(Info, "wlan.card", rca = rca, ready_tries = tries, was_on = was_on, cccr = &cccr[..]);

    let en = rw_direct(0, CCCR_IO_ENABLE, None)?;
    rw_direct(0, CCCR_IO_ENABLE, Some(en | FUNC1))?;
    let mut ready = 0;
    for _ in 0..100 {
        ready = rw_direct(0, CCCR_IO_READY, None)?;
        if ready & FUNC1 != 0 {
            break;
        }
        delay_us(10_000);
    }
    event!(Info, "wlan.func1", io_ready = ready, ready = ready & FUNC1 != 0);
    if ready & FUNC1 == 0 {
        return Err("function 1 not ready");
    }
    Ok(rca)
}
