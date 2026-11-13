#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! A tone on the speaker (doc/70): `run beep HZ MS` plays a sine of HZ
//! (0: 1000) for MS milliseconds (0: 1000) through `audio:out`, at the
//! stream's rate, a quarter of full scale, then waits for the queue to
//! drain. Several at once are mixed by the kernel.
//!
//! Built with `--permit audio:out`. Exit 0, or the step that failed.

use t5_rt::{Error, Handle, audio, event, rights, sleep_ms, wait};

t5_rt::entry!(main);

const AMPLITUDE: f32 = 8192.0;
/// Frames written at a time.
const BLOCK: usize = 256;

fn main(hz: usize, ms: usize) -> usize {
    let (hz, ms) = (if hz == 0 { 1000 } else { hz }, if ms == 0 { 1000 } else { ms });
    match play(hz as u32, ms as u32) {
        Ok(frames) => {
            event!(Info, "beep.done", frames = frames);
            0
        }
        Err((step, e)) => {
            event!(Error, "beep.failed", step = step, error = e as u32);
            step as usize
        }
    }
}

/// cos(x) for 0 <= x <= pi (Taylor series; no libm here).
fn cos(x: f32) -> f32 {
    let x2 = x * x;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..12 {
        term *= -x2 / ((2 * k - 1) * (2 * k)) as f32;
        sum += term;
    }
    sum
}

fn play(hz: u32, ms: u32) -> Result<u32, (u32, Error)> {
    let out = Handle::open(audio::OUT, rights::WRITE | rights::CTL | rights::WAIT).map_err(|e| (1, e))?;
    let rate = out.ctl(audio::RATE, 0).map_err(|e| (2, e))? as u32;
    let empty = out.ctl(audio::QUEUED, 0).map_err(|e| (2, e))?;
    event!(Info, "beep.start", hz = hz, ms = ms, rate = rate);
    // y[n] = 2 cos(w) y[n-1] - y[n-2]: a sine without sin().
    let w = 2.0 * core::f32::consts::PI * hz as f32 / rate as f32;
    let c = 2.0 * cos(w);
    let (mut y1, mut y2) = (0.0f32, -AMPLITUDE * (w - w * w * w / 6.0));
    let total = (rate as u64 * ms as u64 / 1000) as u32;
    let mut done = 0u32;
    let mut block = [0u8; BLOCK * audio::FRAME];
    let mut pending = 0..0;
    while done < total || !pending.is_empty() {
        if pending.is_empty() {
            let n = (total - done).min(BLOCK as u32) as usize;
            for f in block[..n * audio::FRAME].chunks_exact_mut(audio::FRAME) {
                let y = c * y1 - y2;
                y2 = y1;
                y1 = y;
                let s = (y as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                f[..2].copy_from_slice(&s.to_le_bytes());
                f[2..].copy_from_slice(&s.to_le_bytes());
            }
            done += n as u32;
            pending = 0..n * audio::FRAME;
        }
        let k = out.write(&block[pending.clone()]).map_err(|e| (3, e))?;
        pending.start += k;
        if k == 0 {
            wait(&[&out], t5_rt::FOREVER).map_err(|e| (4, e))?;
        }
    }
    // Closing drops what is still queued.
    while out.ctl(audio::QUEUED, 0).map_err(|e| (5, e))? < empty {
        sleep_ms(5);
    }
    Ok(done)
}
