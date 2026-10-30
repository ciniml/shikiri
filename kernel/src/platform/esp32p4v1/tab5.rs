//! Tab5 board (M6a, doc/17 §5): IO expander #1 releases the LCD and touch
//! resets, the touch controller on I2C tells which of the three panel lots
//! this is, and touch points are read by polling
//! (doc/research/m6-panel-touch.md §2, §3, §6). IO expander #2 (0x44) is
//! only read: one of its bits powers the Tab5 off (doc/06 §1.3).

use t5_abi::event;

use super::i2c::{self, func_out_sel, io_mux_reg};
use super::psram::delay_us;
use super::wr;
use crate::platform::{Panel, TouchPoint};

// PI4IOE5V6408 registers (datasheet table 2; m6-panel-touch.md §2.1).
const EXP1: u8 = 0x43;
const EXP2: u8 = 0x44;
const EXP_ID: u8 = 0x01;
const EXP_DIR: u8 = 0x03;
const EXP_OUT: u8 = 0x05;
const EXP_OUT_H_IM: u8 = 0x07;
const EXP_PULL_EN: u8 = 0x0B;
const EXP_PULL_SEL: u8 = 0x0D;
const EXP_IN: u8 = 0x0F;

// GPIO23 = touch INT (GT911 address select).
const TP_INT: usize = 23;
const GPIO_OUT_W1TS: usize = 0x500E_0008;
const GPIO_OUT_W1TC: usize = 0x500E_000C;
const GPIO_ENABLE_W1TS: usize = 0x500E_0024;
const GPIO_ENABLE_W1TC: usize = 0x500E_0028;
const SIG_GPIO_OUT: u32 = 256;

// Touch controllers.
const ST712X: u8 = 0x55;
const GT911: [u8; 2] = [0x14, 0x5D];
const ST_FW_VERSION: u16 = 0x0000;
const ST_MAX_TOUCHES: u16 = 0x0009;
const ST_STATUS: u16 = 0x0010;
const ST_WITH_COORD: u8 = 1 << 3;
const ST_POINTS: u16 = 0x0014;
const ST_POINT_SIZE: usize = 7;
const GT_STATUS: u16 = 0x814E;
const GT_POINTS: u16 = 0x814F;
const GT_POINT_SIZE: usize = 8;
const GT_READY: u8 = 1 << 7;

const MAX_POINTS: usize = 5;

fn reg16_read(addr: u8, reg: u16, out: &mut [u8]) -> Result<(), i2c::Error> {
    i2c::transfer(addr, &reg.to_be_bytes(), out)
}

fn exp_write(addr: u8, reg: u8, v: u8) -> Result<(), i2c::Error> {
    i2c::transfer(addr, &[reg, v], &mut [])
}

fn exp_read(addr: u8, reg: u8) -> Result<u8, i2c::Error> {
    let mut b = [0u8];
    i2c::transfer(addr, &[reg], &mut b).map(|_| b[0])
}

fn tp_int_output(high: bool) {
    unsafe {
        wr(io_mux_reg(TP_INT), 1 << 12 | 1 << 9);
        wr(func_out_sel(TP_INT), SIG_GPIO_OUT);
        wr(if high { GPIO_OUT_W1TS } else { GPIO_OUT_W1TC }, 1 << TP_INT);
        wr(GPIO_ENABLE_W1TS, 1 << TP_INT);
    }
}

fn tp_int_input() {
    unsafe { wr(GPIO_ENABLE_W1TC, 1 << TP_INT) };
}

fn ms(n: u64) {
    delay_us(n * 1000);
}

/// What `init` found, kept for touch reads.
static mut FOUND: Panel = Panel::None;

pub fn panel() -> Panel {
    unsafe { FOUND }
}

/// Releases the LCD and touch resets and identifies the panel.
pub fn init() -> Panel {
    i2c::init();
    let id1 = exp_read(EXP1, EXP_ID);
    let id2 = exp_read(EXP2, EXP_ID);
    // #2's inputs: bit6 CHG_STAT (charging), read only (doc/17 §8).
    let in2 = exp_read(EXP2, EXP_IN);
    event!(
        Info,
        "board.expanders",
        exp1_id = id1.map_or(-1, |v| v as i32),
        exp2_id = id2.map_or(-1, |v| v as i32),
        exp2_in = in2.map_or(-1, |v| v as i32)
    );
    if let Err(e) = id1 {
        event!(Error, "board.no_expander", error = e.as_str());
        return Panel::None;
    }
    // Both resets low first, so the result is the same whatever state the
    // expander kept across a SoC reset (m6-panel-touch.md §2.3, §7).
    let steps = [
        (EXP_OUT, 0x44), // EXT5V_EN, CAM_RST high; LCD_RST, TP_RST, SPK_EN low
        (EXP_DIR, 0x7F), // bits 0-6 outputs, bit7 HP_DET input
        (EXP_OUT_H_IM, 0x00),
        (EXP_PULL_SEL, 0x7F),
        (EXP_PULL_EN, 0x7F),
    ];
    for (reg, v) in steps {
        if let Err(e) = exp_write(EXP1, reg, v) {
            event!(Error, "board.expander_failed", reg = reg, error = e.as_str());
            return Panel::None;
        }
    }
    tp_int_output(true);
    ms(10);
    // LCD_RST released as an input pulled up, TP_RST driven high (as
    // upstream M5GFX and esp-bsp do).
    let released = exp_write(EXP1, EXP_DIR, 0x6F).and_then(|_| exp_write(EXP1, EXP_OUT, 0x64));
    tp_int_input();
    if let Err(e) = released {
        event!(Error, "board.expander_failed", reg = EXP_DIR, error = e.as_str());
        return Panel::None;
    }
    ms(100);

    let panel = identify();
    if let Panel::Ili9881c { .. } = panel {
        gt911_wakeup();
    }
    unsafe { FOUND = panel };
    panel
}

/// m6-panel-touch.md §3.2: the ST712x touch answers at 0x55 with its FW
/// version (1 = ST7121, 3 = ST7123); otherwise a GT911 ACKs at 0x14 (or
/// 0x5D). Retried every 10 ms for up to 600 ms: the ST712x needs tens of
/// ms after a reset.
fn identify() -> Panel {
    for attempt in 0..60u32 {
        let mut fw = [0u8];
        match reg16_read(ST712X, ST_FW_VERSION, &mut fw) {
            Ok(()) => {
                let mut max = [0u8];
                let _ = reg16_read(ST712X, ST_MAX_TOUCHES, &mut max);
                event!(Info, "display.panel", kind = st_kind(fw[0]), fw = fw[0], touch_addr = ST712X, attempts = attempt + 1, max_touches = max[0]);
                return match fw[0] {
                    1 => Panel::St7121,
                    3 => Panel::St7123,
                    _ => Panel::Unknown,
                };
            }
            Err(_) => {
                if let Some(&a) = GT911.iter().find(|&&a| i2c::probe(a)) {
                    event!(Info, "display.panel", kind = "ili9881c", fw = 0u8, touch_addr = a, attempts = attempt + 1, max_touches = MAX_POINTS as u8);
                    return Panel::Ili9881c { touch_addr: a };
                }
            }
        }
        ms(10);
    }
    event!(Warn, "display.unknown");
    Panel::Unknown
}

fn st_kind(fw: u8) -> &'static str {
    match fw {
        1 => "st7121",
        3 => "st7123",
        _ => "st712x?",
    }
}

/// GT911's INT synchronisation after reset, as M5GFX's `wakeup()`: INT low
/// for 58 ms, high for 2 ms, then input (m6-panel-touch.md §6.1).
fn gt911_wakeup() {
    tp_int_output(false);
    ms(58);
    tp_int_output(true);
    ms(2);
    tp_int_input();
}

/// Reads the current touch points into `out`; returns how many (0 = none).
pub fn touch_read(out: &mut [TouchPoint; MAX_POINTS]) -> Result<usize, &'static str> {
    match panel() {
        Panel::St7121 | Panel::St7123 => st_read(out),
        Panel::Ili9881c { touch_addr } => gt_read(touch_addr, out),
        _ => Err("no touch"),
    }
}

fn st_read(out: &mut [TouchPoint; MAX_POINTS]) -> Result<usize, &'static str> {
    let e = |e: i2c::Error| e.as_str();
    let mut status = [0u8];
    reg16_read(ST712X, ST_STATUS, &mut status).map_err(e)?;
    if status[0] & ST_WITH_COORD == 0 {
        return Ok(0);
    }
    // Up to 4 points (28 bytes) per transfer; slots are fixed, valid ones
    // have bit7 of their first byte set.
    let mut n = 0;
    for chunk in 0..3u16 {
        let mut b = [0u8; 4 * ST_POINT_SIZE];
        reg16_read(ST712X, ST_POINTS + chunk * b.len() as u16, &mut b).map_err(e)?;
        for (i, p) in b.chunks(ST_POINT_SIZE).enumerate() {
            if p[0] & 0x80 != 0 && n < MAX_POINTS {
                out[n] = TouchPoint {
                    x: ((p[0] as u16 & 0x3F) << 8) | p[1] as u16,
                    y: ((p[2] as u16) << 8) | p[3] as u16,
                    id: chunk * 4 + i as u16,
                    size: p[4] as u16,
                };
                n += 1;
            }
        }
    }
    Ok(n)
}

fn gt_read(addr: u8, out: &mut [TouchPoint; MAX_POINTS]) -> Result<usize, &'static str> {
    let e = |e: i2c::Error| e.as_str();
    let mut status = [0u8];
    reg16_read(addr, GT_STATUS, &mut status).map_err(e)?;
    if status[0] & GT_READY == 0 {
        return Ok(0);
    }
    let count = (status[0] & 0x0F) as usize;
    let mut n = 0;
    if (1..=MAX_POINTS).contains(&count) {
        // Three points (24 bytes) per transfer.
        for first in (0..count).step_by(3) {
            let k = (count - first).min(3);
            let mut b = [0u8; 3 * GT_POINT_SIZE];
            let reg = GT_POINTS + (first * GT_POINT_SIZE) as u16;
            reg16_read(addr, reg, &mut b[..k * GT_POINT_SIZE]).map_err(e)?;
            for p in b[..k * GT_POINT_SIZE].chunks(GT_POINT_SIZE) {
                out[n] = TouchPoint {
                    id: p[0] as u16,
                    x: u16::from_le_bytes([p[1], p[2]]),
                    y: u16::from_le_bytes([p[3], p[4]]),
                    size: u16::from_le_bytes([p[5], p[6]]),
                };
                n += 1;
            }
        }
    }
    // The GT911 repeats the same report until the status is cleared.
    let [hi, lo] = GT_STATUS.to_be_bytes();
    i2c::transfer(addr, &[hi, lo, 0], &mut []).map_err(e)?;
    Ok(n)
}
