//! Tab5 board (M6a, doc/17 §5): IO expander #1 releases the LCD and touch
//! resets, the touch controller on I2C tells which of the three panel lots
//! this is, and touch points are read by polling
//! (doc/research/m6-panel-touch.md §2, §3, §6). Before that, the audio
//! codecs are set up and #1's bit1 (SPK_EN) turns the speaker amp on; the
//! display's writes keep it (doc/70 §9). IO expander #2 (0x44) is
//! read, and written only to set bit0 (the Wi-Fi chip's power,
//! `wlan_power_on`): its bit4 powers the Tab5 off (doc/06 §1.3).

use t5_abi::event;

use super::i2c::{self, func_out_sel, io_mux_reg};
use super::psram::delay_us;
use super::wr;
use crate::platform::{Current, Panel, Platform, TouchPoint};

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
/// #1's bit1: the speaker amp (doc/70 §9).
const SPK_EN: u8 = 1 << 1;

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

/// The kernel's I2C as embedded-hal's, for kumiki's codec drivers.
struct Bus;

#[derive(Debug)]
struct BusError(i2c::Error);

impl embedded_hal::i2c::Error for BusError {
    fn kind(&self) -> embedded_hal::i2c::ErrorKind {
        use embedded_hal::i2c::{ErrorKind, NoAcknowledgeSource};
        match self.0 {
            i2c::Error::Nack => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown),
            i2c::Error::Bus => ErrorKind::Bus,
            i2c::Error::TooLong => ErrorKind::Other,
        }
    }
}

impl embedded_hal::i2c::ErrorType for Bus {
    type Error = BusError;
}

impl embedded_hal::i2c::I2c for Bus {
    fn transaction(&mut self, addr: u8, ops: &mut [embedded_hal::i2c::Operation<'_>]) -> Result<(), BusError> {
        use embedded_hal::i2c::Operation::{Read, Write};
        match ops {
            [Write(w)] => i2c::transfer(addr, w, &mut []),
            [Write(w), Read(r)] => i2c::transfer(addr, w, r),
            _ => Err(i2c::Error::TooLong),
        }
        .map_err(BusError)
    }
}

fn codec_result<E>(r: &Result<(), kumiki_es8388::Error<E>>) -> &'static str {
    match r {
        Ok(()) => "ok",
        Err(kumiki_es8388::Error::I2c(_)) => "i2c",
        Err(kumiki_es8388::Error::Range) => "range",
    }
}

/// Powers the ES8388 down and sets it up (kumiki's `Es8388::init`), the
/// amp off. After a SoC reset with the DAC playing it stays silent,
/// registers and all, until powered down: the soft reset in `init` is not
/// enough; `power_down` (DAC and chip power) for 1 ms was, but only once
/// the stream had run a while, not at boot (doc/70 §9.2).
fn dac() -> Result<(), kumiki_es8388::Error<BusError>> {
    if let Ok(out) = exp_read(EXP1, EXP_OUT) {
        let _ = exp_write(EXP1, EXP_OUT, out & !SPK_EN);
    }
    let mut codec = kumiki_es8388::Es8388::new(Bus, kumiki_es8388::ADDR);
    let _ = codec.power_down();
    ms(2);
    codec.init()
}

/// Sets up the ES8388 (DAC) and the ES7210 (microphones) while I2S already
/// drives MCLK and before the display's power-up, which kumiki found the
/// ES8388 needs (doc/70 §1), with the amp off. SPK_EN if the DAC is set.
fn codecs() -> u8 {
    if Current::audio_rate() == 0 {
        return 0;
    }
    let dac = dac();
    let adc = match kumiki_es7210::Es7210::new(Bus, kumiki_es7210::ADDR).init() {
        Ok(()) => "ok",
        Err(kumiki_es7210::Error::I2c(_)) => "i2c",
        Err(kumiki_es7210::Error::Range) => "range",
    };
    event!(Info, "board.codecs", es8388 = codec_result(&dac), es7210 = adc);
    if dac.is_ok() { SPK_EN } else { 0 }
}

/// Sets the ES8388 up again (the first `audio:out` since boot, doc/70
/// §9.2), if it was at boot (SPK_EN on), and turns the amp back on.
pub fn dac_again() {
    let Ok(out) = exp_read(EXP1, EXP_OUT) else { return };
    if out & SPK_EN == 0 {
        return;
    }
    let r = dac();
    if r.is_ok() {
        let _ = exp_write(EXP1, EXP_OUT, out);
    }
    event!(Info, "audio.dac_again", es8388 = codec_result(&r));
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
    let spk = codecs();
    // Both resets low first, so the result is the same whatever state the
    // expander kept across a SoC reset (m6-panel-touch.md §2.3, §7).
    let steps = [
        (EXP_OUT, 0x44 | spk), // EXT5V_EN, CAM_RST high; LCD_RST, TP_RST low
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
    let released = exp_write(EXP1, EXP_DIR, 0x6F).and_then(|_| exp_write(EXP1, EXP_OUT, 0x64 | spk));
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

pub const NAME: &str = "tab5";
/// The internal I2C bus (GPIO31/32) and the keyboard port (ExtPort1, INT
/// GPIO50).
pub const I2C: bool = true;
pub const KBD: bool = true;
/// The internal I²C bus (I2C1) is brought up at boot (doc/69).
pub const INTERNAL_I2C: bool = true;
/// No Ethernet (doc/58).
pub const ETH: bool = false;
/// The ESP32-C6 (doc/research/net-stack.md §1.1).
pub const SDIO: super::sdio::Pins = super::sdio::Pins { clk: 12, cmd: 13, data: [11, 10, 9, 8], chip_en: 15 };

/// The card's power is the SoC's 3.3 V rail (`sd.rs`).
pub fn sd_power_on() {}

/// IO expander #2's bit0 (WLAN_PWR_EN) powers the ESP32-C6. Turns it on
/// with one read-modify-write per register that changes bit0 only; bit4
/// powers the Tab5 off and is never changed (doc/22 §2, approved by the
/// user on 2026-10-31). Reads back and checks that nothing else changed.
/// Returns whether the C6 was already powered.
pub fn wlan_power_on() -> Result<bool, &'static str> {
    let read3 = || -> Result<[u8; 3], i2c::Error> { Ok([exp_read(EXP2, EXP_OUT)?, exp_read(EXP2, EXP_DIR)?, exp_read(EXP2, EXP_OUT_H_IM)?]) };
    let [out, dir, him] = read3().map_err(|e| e.as_str())?;
    event!(Info, "wlan.expander", out = out, dir = dir, out_h_im = him);
    let on = |[o, d, h]: [u8; 3]| o & 1 != 0 && d & 1 != 0 && h & 1 == 0;
    if on([out, dir, him]) {
        return Ok(true);
    }
    // Level first, then the direction, so the pin goes straight to high.
    let steps = [(EXP_OUT, out, out | 1), (EXP_DIR, dir, dir | 1), (EXP_OUT_H_IM, him, him & !1)];
    for (reg, old, new) in steps {
        if new != old {
            exp_write(EXP2, reg, new).map_err(|e| e.as_str())?;
        }
    }
    let after = read3().map_err(|e| e.as_str())?;
    event!(Info, "wlan.power_on", out = after[0], dir = after[1], out_h_im = after[2]);
    let others_same = (after[0] ^ out) & !1 == 0 && (after[1] ^ dir) & !1 == 0 && (after[2] ^ him) & !1 == 0;
    if !others_same {
        event!(Error, "wlan.expander_changed", out = after[0], dir = after[1], out_h_im = after[2]);
        return Err("expander #2 changed beyond bit0");
    }
    if !on(after) {
        return Err("WLAN_PWR_EN did not take");
    }
    Ok(false)
}
