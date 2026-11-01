//! I2C masters (I2C1 on the Tab5's internal bus, I2C0 on ExtPort1),
//! polled, after ESP-IDF 6.0 `esp_hal_i2c` and `esp_driver_i2c`
//! (doc/research/m6-panel-touch.md §1). One transfer is at most one command
//! list: a write of up to 30 bytes, optionally followed by a read of up to
//! 31 bytes; longer reads are split by the caller.

use super::psram::{clear_bits, set_bits, set_field};
use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;

// Clock and reset (hp_sys_clkrst_reg.h).
const SOC_CLK_CTRL2: usize = 0x500E_601C;
const PERI_CLK_CTRL10: usize = 0x500E_6040;
const PERI_CLK_CTRL11: usize = 0x500E_6044;
const HP_RST_EN1: usize = 0x500E_60C4;

// Pins (gpio_reg.h, io_mux_reg.h, gpio_sig_map.h).
const GPIO_OUT_W1TS: usize = 0x500E_0008;
const GPIO_OUT1_W1TS: usize = 0x500E_0014;
const GPIO_ENABLE_W1TS: usize = 0x500E_0024;
const GPIO_ENABLE1_W1TS: usize = 0x500E_0030;
const GPIO_PIN_PAD_DRIVER: u32 = 1 << 2;
const IO_MUX_MCU_SEL_GPIO: u32 = 1 << 12;
const IO_MUX_FUN_IE: u32 = 1 << 9;
const GPIO_SIG_IN_SEL: u32 = 1 << 7;

pub(super) const fn gpio_pin_reg(n: usize) -> usize {
    0x500E_0074 + 4 * n
}
pub(super) const fn io_mux_reg(n: usize) -> usize {
    0x500E_1004 + 4 * n
}
pub(super) const fn func_out_sel(n: usize) -> usize {
    0x500E_0558 + 4 * n
}
pub(super) const fn func_in_sel(sig: u32) -> usize {
    0x500E_0158 + 4 * sig as usize
}

/// One I2C controller and its pins.
pub struct Bus {
    base: usize,
    sda: usize,
    scl: usize,
    sig_sda: u32,
    sig_scl: u32,
    /// APB clock enable in SOC_CLK_CTRL2, and reset in HP_RST_EN1.
    apb_clk: u32,
    rst: u32,
}

/// I2C1 on the Tab5's internal bus (SDA = GPIO31, SCL = GPIO32): the IO
/// expanders, touch, RTC and the rest (doc/06 §1).
pub const INTERNAL: Bus = Bus { base: 0x500C_5000, sda: 31, scl: 32, sig_sda: 71, sig_scl: 70, apb_clk: 1 << 13, rst: 1 << 21 };
/// I2C0 on ExtPort1 (SDA = GPIO0, SCL = GPIO1): the clip-on keyboard
/// (doc/39).
pub const EXT1: Bus = Bus { base: 0x500C_4000, sda: 0, scl: 1, sig_sda: 69, sig_scl: 68, apb_clk: 1 << 12, rst: 1 << 22 };

// Register offsets (i2c_reg.h; the same on v1 and v3).
const SCL_LOW_PERIOD: usize = 0x00;
const CTR: usize = 0x04;
const SR: usize = 0x08;
const TO: usize = 0x0C;
const FIFO_CONF: usize = 0x18;
const DATA: usize = 0x1C;
const INT_RAW: usize = 0x20;
const INT_CLR: usize = 0x24;
const INT_ENA: usize = 0x28;
const SDA_HOLD: usize = 0x30;
const SDA_SAMPLE: usize = 0x34;
const SCL_HIGH_PERIOD: usize = 0x38;
const SCL_START_HOLD: usize = 0x40;
const SCL_RSTART_SETUP: usize = 0x44;
const SCL_STOP_HOLD: usize = 0x48;
const SCL_STOP_SETUP: usize = 0x4C;
const COMD0: usize = 0x58;
const SCL_SP_CONF: usize = 0x80;

const CTR_MS_MODE: u32 = 1 << 4;
const CTR_TRANS_START: u32 = 1 << 5;
const CTR_FSM_RST: u32 = 1 << 10;
const CTR_CONF_UPGATE: u32 = 1 << 11;
const FIFO_RX_RST: u32 = 1 << 12;
const FIFO_TX_RST: u32 = 1 << 13;
const TO_EN: u32 = 1 << 5;
/// 2^17 cycles of 40 MHz, about 3.3 ms (IDF's value).
const TO_VALUE: u32 = 17;
const SP_RST_SLV_EN: u32 = 1 << 0;
const SR_BUS_BUSY: u32 = 1 << 4;

const INT_TRANS_COMPLETE: u32 = 1 << 7;
const INT_NACK: u32 = 1 << 10;
/// TIME_OUT, ARBITRATION_LOST, SCL_ST_TO, SCL_MAIN_ST_TO.
const INT_ERRORS: u32 = (1 << 8) | (1 << 5) | (1 << 13) | (1 << 14);

// Command words (i2c_ll.h:28-48).
const CMD_RSTART: u32 = 6 << 11;
const CMD_WRITE_ACK: u32 = (1 << 11) | (1 << 8);
const CMD_READ: u32 = 3 << 11;
const CMD_READ_NACK: u32 = CMD_READ | (1 << 10);
const CMD_STOP: u32 = 2 << 11;

const FIFO_SIZE: usize = 32;
/// Software limit on one transfer, well above 31 bytes at 100 kHz (~3 ms).
const TRANSFER_US: u64 = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The address (or a data byte) was not acknowledged: nobody there.
    Nack,
    /// Bus error or time-out; the controller was reset and the bus cleared.
    Bus,
    TooLong,
}

impl Error {
    pub fn as_str(self) -> &'static str {
        match self {
            Error::Nack => "nack",
            Error::Bus => "bus error",
            Error::TooLong => "transfer too long",
        }
    }
}

/// The internal bus.
pub fn init() {
    init_on(&INTERNAL);
}

/// Clocks, reset, pins and 100 kHz timing (m6-panel-touch.md §1.4), from
/// the 40 MHz crystal.
pub fn init_on(b: &Bus) {
    set_bits(SOC_CLK_CTRL2, b.apb_clk);
    if b.base == INTERNAL.base {
        // I2C1: source bit 26 (0 = XTAL), enable bit 27, divider in CTRL11.
        clear_bits(PERI_CLK_CTRL10, 1 << 26);
        set_bits(PERI_CLK_CTRL10, 1 << 27);
        set_field(PERI_CLK_CTRL11, 0, 24, 0);
    } else {
        // I2C0: source bit 0, enable bit 1, divider bits 2-9.
        clear_bits(PERI_CLK_CTRL10, 1 << 0);
        set_bits(PERI_CLK_CTRL10, 1 << 1);
        set_field(PERI_CLK_CTRL10, 0x3FC, 2, 0);
    }
    set_bits(HP_RST_EN1, b.rst);
    clear_bits(HP_RST_EN1, b.rst);
    unsafe {
        // Pins: latch high, open drain, GPIO function with input, routed
        // through the matrix both ways.
        for pin in [b.sda, b.scl] {
            if pin < 32 {
                wr(GPIO_OUT_W1TS, 1 << pin);
            } else {
                wr(GPIO_OUT1_W1TS, 1 << (pin - 32));
            }
        }
        for (pin, sig) in [(b.sda, b.sig_sda), (b.scl, b.sig_scl)] {
            set_bits(gpio_pin_reg(pin), GPIO_PIN_PAD_DRIVER);
            wr(io_mux_reg(pin), IO_MUX_MCU_SEL_GPIO | IO_MUX_FUN_IE);
            wr(func_out_sel(pin), sig);
            wr(func_in_sel(sig), pin as u32 | GPIO_SIG_IN_SEL);
        }
        for pin in [b.sda, b.scl] {
            if pin < 32 {
                wr(GPIO_ENABLE_W1TS, 1 << pin);
            } else {
                wr(GPIO_ENABLE1_W1TS, 1 << (pin - 32));
            }
        }

        wr(b.base + CTR, CTR_MS_MODE);
        wr(b.base + FIFO_CONF, 0);
        // 100 kHz from 40 MHz (i2c_ll_master_cal_bus_clk, half = 200).
        wr(b.base + SCL_LOW_PERIOD, 199);
        wr(b.base + SCL_HIGH_PERIOD, 102 | (98 << 9));
        wr(b.base + SDA_HOLD, 49);
        wr(b.base + SDA_SAMPLE, 99);
        for r in [SCL_START_HOLD, SCL_RSTART_SETUP, SCL_STOP_HOLD, SCL_STOP_SETUP] {
            wr(b.base + r, 199);
        }
        wr(b.base + TO, TO_VALUE | TO_EN);
        wr(b.base + INT_ENA, 0);
    }
    update(b);
    fifo_reset(b);
}

fn update(b: &Bus) {
    set_bits(b.base + CTR, CTR_CONF_UPGATE);
}

fn fifo_reset(b: &Bus) {
    set_bits(b.base + FIFO_CONF, FIFO_RX_RST | FIFO_TX_RST);
    clear_bits(b.base + FIFO_CONF, FIFO_RX_RST | FIFO_TX_RST);
    unsafe { wr(b.base + INT_CLR, 0x3FFFF) };
}

/// After an error: reset the state machine and clock out 9 SCL pulses so a
/// device stuck mid-byte lets SDA go (m6-panel-touch.md §1.6).
fn recover(b: &Bus) {
    set_bits(b.base + CTR, CTR_FSM_RST);
    unsafe { wr(b.base + SCL_SP_CONF, (9 << 1) | SP_RST_SLV_EN) };
    update(b);
    let end = Esp32p4v1::now() + 16 * 2000;
    while unsafe { rd(b.base + SCL_SP_CONF) } & SP_RST_SLV_EN != 0 && Esp32p4v1::now() < end {}
    clear_bits(b.base + SCL_SP_CONF, SP_RST_SLV_EN);
    update(b);
    fifo_reset(b);
}

/// `transfer_on` the internal bus.
pub fn transfer(addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), Error> {
    transfer_on(&INTERNAL, addr, tx, rx)
}

/// Writes `tx` to device `addr` on bus `b`, then (if `rx` is not empty)
/// reads `rx` after a repeated start. An empty `tx` and `rx` only checks
/// for an ACK.
pub fn transfer_on(b: &Bus, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), Error> {
    if tx.len() + 2 > FIFO_SIZE || rx.len() >= FIFO_SIZE {
        return Err(Error::TooLong);
    }
    fifo_reset(b);
    let mut cmds = [0u32; 8];
    let mut n = 0;
    let mut push = |c: u32| {
        cmds[n] = c;
        n += 1;
    };
    unsafe {
        wr(b.base + DATA, (addr as u32) << 1);
        for &byte in tx {
            wr(b.base + DATA, byte as u32);
        }
    }
    push(CMD_RSTART);
    push(CMD_WRITE_ACK | (1 + tx.len() as u32));
    if !rx.is_empty() {
        unsafe { wr(b.base + DATA, ((addr as u32) << 1) | 1) };
        push(CMD_RSTART);
        push(CMD_WRITE_ACK | 1);
        if rx.len() > 1 {
            push(CMD_READ | (rx.len() as u32 - 1));
        }
        push(CMD_READ_NACK | 1);
    }
    push(CMD_STOP);
    for (i, &c) in cmds[..n].iter().enumerate() {
        unsafe { wr(b.base + COMD0 + 4 * i, c) };
    }
    update(b);
    set_bits(b.base + CTR, CTR_TRANS_START);

    let end = Esp32p4v1::now() + 16 * TRANSFER_US;
    let r = loop {
        let raw = unsafe { rd(b.base + INT_RAW) };
        if raw & INT_NACK != 0 {
            break Err(Error::Nack);
        }
        if raw & INT_ERRORS != 0 || Esp32p4v1::now() > end {
            break Err(Error::Bus);
        }
        if raw & INT_TRANS_COMPLETE != 0 {
            break Ok(());
        }
    };
    match r {
        Ok(()) => {
            let count = (unsafe { rd(b.base + SR) } >> 8) as usize & 0x3F;
            if count < rx.len() {
                recover(b);
                return Err(Error::Bus);
            }
            for byte in rx.iter_mut() {
                *byte = unsafe { rd(b.base + DATA) } as u8;
            }
            Ok(())
        }
        // As ESP-IDF: after a NACK the controller ends the transfer by
        // itself; wait until the bus is free. No reset and no bus clear:
        // the clear's 9 SCL pulses after every NACK (an I2C scan) made the
        // GT911 stop answering until its next reset (doc/17 §9).
        Err(Error::Nack) => {
            let end = Esp32p4v1::now() + 16 * TRANSFER_US;
            while unsafe { rd(b.base + SR) } & SR_BUS_BUSY != 0 {
                if Esp32p4v1::now() > end {
                    recover(b);
                    break;
                }
            }
            fifo_reset(b);
            Err(Error::Nack)
        }
        Err(e) => {
            recover(b);
            Err(e)
        }
    }
}

/// Whether anything acknowledges `addr` on the internal bus.
pub fn probe(addr: u8) -> bool {
    transfer(addr, &[], &mut []).is_ok()
}
