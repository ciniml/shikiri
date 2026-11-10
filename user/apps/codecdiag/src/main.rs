#![no_std]
#![no_main]

//! The Tab5's speaker path, read from an app (doc/70 §9): `run codecdiag 0`
//! logs IO expander #1's DIR / OUT / OUT_H_IM (read only: #1 also switches
//! power) and the ES8388's registers 0..=52; `run codecdiag 1` first sets
//! the ES8388 up again (kumiki's `Es8388::init`), to tell whether the
//! kernel's set-up at boot was lost. After a SoC reset the speaker can stay
//! silent until the power is cut (doc/70 §9.2): `run codecdiag 2` holds #1's
//! bit1 (SPK_EN) low for a second, `run codecdiag 3 MS` also powers the ES8388
//! down for MS ms (0: a second) and sets it up again; `run codecdiag 4 MS`
//! does it as the kernel's `dac_again` does, spinning for the MS ms
//! instead of sleeping. Those left the speaker silent and sounding by
//! turns (doc/70 §9.5); to find which writes do: `run codecdiag 5 MS`
//! powers down for MS ms and writes the power registers back (4 = 0x3C,
//! 2 = 0x0A) without `init` (it sounded after), `run codecdiag 6` runs
//! `init` alone (silent after).
//!
//! Built with `--permit i2c:int/0x10 --permit i2c:int/0x43` (strong: the
//! system key). Exit 0, or the step that failed.

use embedded_hal::i2c::I2c;
use kumiki_t5os::I2cDevice;
use t5_rt::{event, sleep_ms, time_ns};

t5_rt::entry!(main);

fn main(mode: usize, ms: usize) -> usize {
    match run(mode, ms) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "codecdiag.failed", step = step);
            step as usize
        }
    }
}

fn reg(dev: &mut I2cDevice, addr: u8, r: u8) -> Result<u8, u32> {
    let mut b = [0u8];
    dev.write_read(addr, &[r], &mut b).map_err(|_| 2u32)?;
    Ok(b[0])
}

fn run(mode: usize, ms: usize) -> Result<(), u32> {
    let mut exp = I2cDevice::open(0x43).map_err(|_| 1u32)?;
    let (dir, out, him) = (reg(&mut exp, 0x43, 0x03)?, reg(&mut exp, 0x43, 0x05)?, reg(&mut exp, 0x43, 0x07)?);
    event!(Info, "codecdiag.expander", dir = dir, out = out, out_h_im = him);
    let mut dac = I2cDevice::open(kumiki_es8388::ADDR).map_err(|_| 3u32)?;
    if mode == 5 || mode == 6 {
        exp.write(0x43, &[0x05, out & !0x02]).map_err(|_| 5u32)?;
        if mode == 5 {
            dac.write(kumiki_es8388::ADDR, &[0x04, 0xC0]).map_err(|_| 6u32)?;
            dac.write(kumiki_es8388::ADDR, &[0x02, 0xFF]).map_err(|_| 6u32)?;
            sleep_ms(if ms > 0 { ms as u64 } else { 2 });
            dac.write(kumiki_es8388::ADDR, &[0x02, 0x0A]).map_err(|_| 6u32)?;
            dac.write(kumiki_es8388::ADDR, &[0x04, 0x3C]).map_err(|_| 6u32)?;
        } else {
            let mut codec = kumiki_es8388::Es8388::new(dac, kumiki_es8388::ADDR);
            codec.init().map_err(|_| 4u32)?;
            dac = codec.release();
        }
        let now = reg(&mut exp, 0x43, 0x05)?;
        exp.write(0x43, &[0x05, now | (out & 0x02)]).map_err(|_| 5u32)?;
        event!(Info, "codecdiag.cycled", mode = mode as u32, ms = ms as u32, out = now | (out & 0x02));
    } else if mode >= 2 {
        // Bit1 only, read-modify-write (#1's other bits switch power).
        exp.write(0x43, &[0x05, out & !0x02]).map_err(|_| 5u32)?;
        if mode >= 3 {
            // DAC and outputs, then the chip (ES8388 registers 4 and 2).
            dac.write(kumiki_es8388::ADDR, &[0x04, 0xC0]).map_err(|_| 6u32)?;
            dac.write(kumiki_es8388::ADDR, &[0x02, 0xFF]).map_err(|_| 6u32)?;
        }
        let hold = if mode >= 3 && ms > 0 { ms as u64 } else { 1000 };
        if mode == 4 {
            let end = time_ns() + hold * 1_000_000;
            while time_ns() < end {}
        } else {
            sleep_ms(hold);
        }
        if mode >= 3 {
            let mut codec = kumiki_es8388::Es8388::new(dac, kumiki_es8388::ADDR);
            codec.init().map_err(|_| 4u32)?;
            dac = codec.release();
        }
        let now = reg(&mut exp, 0x43, 0x05)?;
        exp.write(0x43, &[0x05, now | (out & 0x02)]).map_err(|_| 5u32)?;
        event!(Info, "codecdiag.cycled", mode = mode as u32, ms = ms as u32, out = now | (out & 0x02));
    }
    if mode == 1 {
        let mut codec = kumiki_es8388::Es8388::new(dac, kumiki_es8388::ADDR);
        codec.init().map_err(|_| 4u32)?;
        event!(Info, "codecdiag.reinit");
        dac = codec.release();
    }
    // 53 registers, four to a line.
    for base in (0..53u8).step_by(4) {
        let mut v = [0u8; 4];
        for (i, b) in v.iter_mut().enumerate() {
            let r = base + i as u8;
            if r < 53 {
                *b = reg(&mut dac, kumiki_es8388::ADDR, r)?;
            }
        }
        event!(Info, "codecdiag.es8388", reg = base, v = u32::from_be_bytes(v));
    }
    Ok(())
}
