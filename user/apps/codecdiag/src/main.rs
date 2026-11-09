#![no_std]
#![no_main]

//! The Tab5's speaker path, read from an app (doc/70 §9): `run codecdiag 0`
//! logs IO expander #1's DIR / OUT / OUT_H_IM (read only: #1 also switches
//! power) and the ES8388's registers 0..=52; `run codecdiag 1` first sets
//! the ES8388 up again (kumiki's `Es8388::init`), to tell whether the
//! kernel's set-up at boot was lost.
//!
//! Built with `--permit i2c:int/0x10 --permit i2c:int/0x43` (strong: the
//! system key). Exit 0, or the step that failed.

use embedded_hal::i2c::I2c;
use kumiki_t5os::I2cDevice;
use t5_rt::event;

t5_rt::entry!(main);

fn main(mode: usize, _: usize) -> usize {
    match run(mode) {
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

fn run(mode: usize) -> Result<(), u32> {
    let mut exp = I2cDevice::open(0x43).map_err(|_| 1u32)?;
    let (dir, out, him) = (reg(&mut exp, 0x43, 0x03)?, reg(&mut exp, 0x43, 0x05)?, reg(&mut exp, 0x43, 0x07)?);
    event!(Info, "codecdiag.expander", dir = dir, out = out, out_h_im = him);
    let mut dac = I2cDevice::open(kumiki_es8388::ADDR).map_err(|_| 3u32)?;
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
