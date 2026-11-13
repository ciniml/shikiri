#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! The Tab5's IMU (BMI270) from an app (doc/69): kumiki's driver
//! `kumiki-bmi270`, unchanged, on `kumiki_t5os::I2cDevice` (the kernel's
//! `i2c:int/0x68`) and `kumiki_t5os::Delay`. `run imu N` initialises it
//! (uploads its configuration) and logs N samples (0: 5), 100 ms apart:
//! acceleration in milli-g and the raw angular rate.
//!
//! Built with `--permit i2c:int/0x68 --permit i2c:int/0x69` (allowed for
//! app-key images). Exit 0, or the step that failed.

use kumiki_bmi270::Bmi270;
use kumiki_t5os::{Delay, I2cDevice};
use t5_rt::{event, sleep_ms};

t5_rt::entry!(main);

fn main(samples: usize, _: usize) -> usize {
    match run(if samples == 0 { 5 } else { samples }) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "imu.failed", step = step);
            step as usize
        }
    }
}

fn run(samples: usize) -> Result<(), u32> {
    // 0x68 on the Tab5; some lots answer at 0x69 (doc/06).
    let (mut imu, addr) = [0x68u8, 0x69]
        .into_iter()
        .find_map(|a| {
            let mut imu = Bmi270::new(I2cDevice::open(a).ok()?, a);
            imu.chip_id().ok().map(|_| (imu, a))
        })
        .ok_or(1u32)?;
    let id = imu.chip_id().map_err(|_| 2u32)?;
    event!(Info, "imu.found", addr = addr, chip_id = id);
    imu.init(&mut Delay).map_err(|_| 3u32)?;
    event!(Info, "imu.ready");
    for i in 0..samples {
        let a = imu.accel_raw().map_err(|_| 4u32)?;
        let g = imu.gyro_raw().map_err(|_| 5u32)?;
        let mg = |v: i16| v as i32 * 1000 / 4096;
        event!(Info, "imu.sample", n = i as u32, ax_mg = mg(a.x), ay_mg = mg(a.y), az_mg = mg(a.z), gx = g.x as i32, gy = g.y as i32, gz = g.z as i32);
        sleep_ms(100);
    }
    Ok(())
}
