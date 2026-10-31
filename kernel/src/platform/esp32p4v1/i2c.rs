//! I2C1 master on the Tab5's internal bus (SDA = GPIO31, SCL = GPIO32),
//! polled, after ESP-IDF 6.0 `esp_hal_i2c` and `esp_driver_i2c`
//! (doc/research/m6-panel-touch.md §1). One transfer is at most one command
//! list: a write of up to 30 bytes, optionally followed by a read of up to
//! 31 bytes; longer reads are split by the caller.

use super::psram::{clear_bits, set_bits, set_field};
use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;

// Clock and reset (hp_sys_clkrst_reg.h).
const SOC_CLK_CTRL2: usize = 0x500E_601C;
const I2C1_APB_CLK_EN: u32 = 1 << 13;
const PERI_CLK_CTRL10: usize = 0x500E_6040;
const I2C1_CLK_SRC_RC_FAST: u32 = 1 << 26;
const I2C1_CLK_EN: u32 = 1 << 27;
const PERI_CLK_CTRL11: usize = 0x500E_6044;
const HP_RST_EN1: usize = 0x500E_60C4;
const RST_EN_I2C1: u32 = 1 << 21;

// Pins (gpio_reg.h, io_mux_reg.h, gpio_sig_map.h).
const GPIO_OUT_W1TS: usize = 0x500E_0008;
const GPIO_OUT1_W1TS: usize = 0x500E_0014;
const GPIO_ENABLE_W1TS: usize = 0x500E_0024;
const GPIO_ENABLE1_W1TS: usize = 0x500E_0030;
const GPIO_PIN_PAD_DRIVER: u32 = 1 << 2;
const IO_MUX_MCU_SEL_GPIO: u32 = 1 << 12;
const IO_MUX_FUN_IE: u32 = 1 << 9;
const SIG_I2C1_SCL: u32 = 70;
const SIG_I2C1_SDA: u32 = 71;
const GPIO_SIG_IN_SEL: u32 = 1 << 7;
const SDA_PIN: usize = 31;
const SCL_PIN: usize = 32;

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

// I2C1 registers (i2c_reg.h; offsets are the same on v1 and v3).
const BASE: usize = 0x500C_5000;
const SCL_LOW_PERIOD: usize = BASE;
const CTR: usize = BASE + 0x04;
const SR: usize = BASE + 0x08;
const TO: usize = BASE + 0x0C;
const FIFO_CONF: usize = BASE + 0x18;
const DATA: usize = BASE + 0x1C;
const INT_RAW: usize = BASE + 0x20;
const INT_CLR: usize = BASE + 0x24;
const INT_ENA: usize = BASE + 0x28;
const SDA_HOLD: usize = BASE + 0x30;
const SDA_SAMPLE: usize = BASE + 0x34;
const SCL_HIGH_PERIOD: usize = BASE + 0x38;
const SCL_START_HOLD: usize = BASE + 0x40;
const SCL_RSTART_SETUP: usize = BASE + 0x44;
const SCL_STOP_HOLD: usize = BASE + 0x48;
const SCL_STOP_SETUP: usize = BASE + 0x4C;
const COMD0: usize = BASE + 0x58;
const SCL_SP_CONF: usize = BASE + 0x80;

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

/// Clocks, reset, pins and 100 kHz timing (m6-panel-touch.md §1.4).
pub fn init() {
    set_bits(SOC_CLK_CTRL2, I2C1_APB_CLK_EN);
    clear_bits(PERI_CLK_CTRL10, I2C1_CLK_SRC_RC_FAST);
    set_bits(PERI_CLK_CTRL10, I2C1_CLK_EN);
    set_field(PERI_CLK_CTRL11, 0, 24, 0);
    set_bits(HP_RST_EN1, RST_EN_I2C1);
    clear_bits(HP_RST_EN1, RST_EN_I2C1);
    unsafe {
        // Pins: latch high, open drain, GPIO function with input, routed
        // through the matrix both ways.
        wr(GPIO_OUT_W1TS, 1 << SDA_PIN);
        wr(GPIO_OUT1_W1TS, 1 << (SCL_PIN - 32));
        for (pin, sig) in [(SDA_PIN, SIG_I2C1_SDA), (SCL_PIN, SIG_I2C1_SCL)] {
            set_bits(gpio_pin_reg(pin), GPIO_PIN_PAD_DRIVER);
            wr(io_mux_reg(pin), IO_MUX_MCU_SEL_GPIO | IO_MUX_FUN_IE);
            wr(func_out_sel(pin), sig);
            wr(func_in_sel(sig), pin as u32 | GPIO_SIG_IN_SEL);
        }
        wr(GPIO_ENABLE_W1TS, 1 << SDA_PIN);
        wr(GPIO_ENABLE1_W1TS, 1 << (SCL_PIN - 32));

        wr(CTR, CTR_MS_MODE);
        wr(FIFO_CONF, 0);
        // 100 kHz from 40 MHz (i2c_ll_master_cal_bus_clk, half = 200).
        wr(SCL_LOW_PERIOD, 199);
        wr(SCL_HIGH_PERIOD, 102 | (98 << 9));
        wr(SDA_HOLD, 49);
        wr(SDA_SAMPLE, 99);
        for r in [SCL_START_HOLD, SCL_RSTART_SETUP, SCL_STOP_HOLD, SCL_STOP_SETUP] {
            wr(r, 199);
        }
        wr(TO, TO_VALUE | TO_EN);
        wr(INT_ENA, 0);
    }
    update();
    fifo_reset();
}

fn update() {
    set_bits(CTR, CTR_CONF_UPGATE);
}

fn fifo_reset() {
    set_bits(FIFO_CONF, FIFO_RX_RST | FIFO_TX_RST);
    clear_bits(FIFO_CONF, FIFO_RX_RST | FIFO_TX_RST);
    unsafe { wr(INT_CLR, 0x3FFFF) };
}

/// After an error: reset the state machine and clock out 9 SCL pulses so a
/// device stuck mid-byte lets SDA go (m6-panel-touch.md §1.6).
fn recover() {
    set_bits(CTR, CTR_FSM_RST);
    unsafe { wr(SCL_SP_CONF, (9 << 1) | SP_RST_SLV_EN) };
    update();
    let end = Esp32p4v1::now() + 16 * 2000;
    while unsafe { rd(SCL_SP_CONF) } & SP_RST_SLV_EN != 0 && Esp32p4v1::now() < end {}
    clear_bits(SCL_SP_CONF, SP_RST_SLV_EN);
    update();
    fifo_reset();
}

/// Writes `tx` to device `addr`, then (if `rx` is not empty) reads `rx`
/// after a repeated start. An empty `tx` and `rx` only checks for an ACK.
pub fn transfer(addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), Error> {
    if tx.len() + 2 > FIFO_SIZE || rx.len() >= FIFO_SIZE {
        return Err(Error::TooLong);
    }
    fifo_reset();
    let mut cmds = [0u32; 8];
    let mut n = 0;
    let mut push = |c: u32| {
        cmds[n] = c;
        n += 1;
    };
    unsafe {
        wr(DATA, (addr as u32) << 1);
        for &b in tx {
            wr(DATA, b as u32);
        }
    }
    push(CMD_RSTART);
    push(CMD_WRITE_ACK | (1 + tx.len() as u32));
    if !rx.is_empty() {
        unsafe { wr(DATA, ((addr as u32) << 1) | 1) };
        push(CMD_RSTART);
        push(CMD_WRITE_ACK | 1);
        if rx.len() > 1 {
            push(CMD_READ | (rx.len() as u32 - 1));
        }
        push(CMD_READ_NACK | 1);
    }
    push(CMD_STOP);
    for (i, &c) in cmds[..n].iter().enumerate() {
        unsafe { wr(COMD0 + 4 * i, c) };
    }
    update();
    set_bits(CTR, CTR_TRANS_START);

    let end = Esp32p4v1::now() + 16 * TRANSFER_US;
    let r = loop {
        let raw = unsafe { rd(INT_RAW) };
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
            let count = (unsafe { rd(SR) } >> 8) as usize & 0x3F;
            if count < rx.len() {
                recover();
                return Err(Error::Bus);
            }
            for b in rx.iter_mut() {
                *b = unsafe { rd(DATA) } as u8;
            }
            Ok(())
        }
        // As ESP-IDF: after a NACK the controller ends the transfer by
        // itself; wait until the bus is free. No reset and no bus clear:
        // the clear's 9 SCL pulses after every NACK (an I2C scan) made the
        // GT911 stop answering until its next reset (doc/17 §9).
        Err(Error::Nack) => {
            let end = Esp32p4v1::now() + 16 * TRANSFER_US;
            while unsafe { rd(SR) } & SR_BUS_BUSY != 0 {
                if Esp32p4v1::now() > end {
                    recover();
                    break;
                }
            }
            fifo_reset();
            Err(Error::Nack)
        }
        Err(e) => {
            recover();
            Err(e)
        }
    }
}

/// Whether anything acknowledges `addr`.
pub fn probe(addr: u8) -> bool {
    transfer(addr, &[], &mut []).is_ok()
}
