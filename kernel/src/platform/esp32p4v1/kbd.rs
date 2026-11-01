//! The Tab5's clip-on keyboard (doc/39): I2C 0x6D on ExtPort1 (I2C0, SDA =
//! GPIO0, SCL = GPIO1), INT = GPIO50 (active low). Its firmware runs in HID
//! mode and queues key events as (modifier, usage) pairs of the USB HID
//! keyboard page; usage 0 is a release. Register map: M5Stack's
//! `unit_Tab5Keyboard.hpp`, as tab5_claude_client
//! `main/input_tab5_kbd.cpp:26-34` quotes it. Its 5 V (EXT5V_EN, IO
//! expander #1 bit2) is already on from the panel's start (`tab5.rs`).

use super::i2c::{self, EXT1, io_mux_reg};
use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;
use t5_abi::event;

const ADDR: u8 = 0x6D;
const REG_INT_CFG: u8 = 0x00;
const REG_INT_STAT: u8 = 0x01;
const REG_EVENT_NUM: u8 = 0x02;
const REG_MODE: u8 = 0x10;
const REG_HID_EVENT: u8 = 0x30;
const REG_FIRMWARE: u8 = 0xFE;
const MODE_HID: u8 = 1;
const INT_EN_HID: u8 = 1 << 1;
/// HID_EVENT when the queue is empty: both bytes.
const EMPTY: u8 = 0xFF;

const INT_PIN: usize = 50;
const GPIO_IN1: usize = 0x500E_0040;
const IO_MUX_MCU_SEL_GPIO: u32 = 1 << 12;
const IO_MUX_FUN_IE: u32 = 1 << 9;
const IO_MUX_FUN_PU: u32 = 1 << 8;

/// While none answers: look again this often.
const PROBE_TICKS: u64 = 16_000_000;
/// Read the queue at least this often even without INT (a missed edge).
const IDLE_READ_TICKS: u64 = 8_000_000;

struct Kbd {
    up: bool,
    present: bool,
    next_probe: u64,
    next_read: u64,
}

static mut K: Kbd = Kbd { up: false, present: false, next_probe: 0, next_read: 0 };

fn k() -> &'static mut Kbd {
    // Core 0 under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(K) }
}

fn reg_read(reg: u8, out: &mut [u8]) -> Result<(), i2c::Error> {
    i2c::transfer_on(&EXT1, ADDR, &[reg], out)
}

fn reg_write(reg: u8, v: u8) -> Result<(), i2c::Error> {
    i2c::transfer_on(&EXT1, ADDR, &[reg, v], &mut [])
}

/// Whether the keyboard asks to be read (INT low).
fn int_low() -> bool {
    let level = unsafe { rd(GPIO_IN1) };
    level & (1 << (INT_PIN - 32)) == 0
}

/// Finds the keyboard and puts it into HID mode.
fn probe() -> bool {
    let mut fw = [0u8];
    if reg_read(REG_FIRMWARE, &mut fw).is_err() {
        return false;
    }
    let r = reg_write(REG_EVENT_NUM, 0)
        .and_then(|_| reg_write(REG_INT_STAT, 0))
        .and_then(|_| reg_write(REG_MODE, MODE_HID))
        .and_then(|_| reg_write(REG_INT_CFG, INT_EN_HID));
    match r {
        Ok(()) => {
            event!(Info, "kbd.found", firmware = fw[0]);
            true
        }
        Err(e) => {
            event!(Warn, "kbd.setup_failed", error = e.as_str());
            false
        }
    }
}

/// Passes the queued key events to `sink`; looks for the keyboard once a
/// second while there is none.
pub fn poll(sink: &mut dyn FnMut(u8, u8)) -> Result<(), &'static str> {
    let k = k();
    let now = Esp32p4v1::now();
    if !k.up {
        i2c::init_on(&EXT1);
        // INT: input with pull-up (the keyboard pulls it low).
        unsafe { wr(io_mux_reg(INT_PIN), IO_MUX_MCU_SEL_GPIO | IO_MUX_FUN_IE | IO_MUX_FUN_PU) };
        k.up = true;
    }
    if !k.present {
        if now < k.next_probe {
            return Err("no keyboard");
        }
        k.next_probe = now + PROBE_TICKS;
        k.present = probe();
        if !k.present {
            return Err("no keyboard");
        }
    }
    if !int_low() && now < k.next_read {
        return Ok(());
    }
    k.next_read = now + IDLE_READ_TICKS;
    let mut got = false;
    let r = (|| -> Result<(), i2c::Error> {
        // EVENT_NUM counts down as events are read.
        for _ in 0..32 {
            let mut n = [0u8];
            reg_read(REG_EVENT_NUM, &mut n)?;
            if n[0] == 0 {
                break;
            }
            let mut ev = [0u8; 2];
            reg_read(REG_HID_EVENT, &mut ev)?;
            if ev == [EMPTY, EMPTY] {
                break;
            }
            got = true;
            sink(ev[0], ev[1]);
        }
        // Releases INT until the next event.
        if got { reg_write(REG_INT_STAT, 0) } else { Ok(()) }
    })();
    if let Err(e) = r {
        event!(Warn, "kbd.lost", error = e.as_str());
        k.present = false;
        return Err("keyboard lost");
    }
    Ok(())
}
