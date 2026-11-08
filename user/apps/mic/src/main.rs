#![no_std]
#![no_main]

//! The microphones' level (doc/70): `run mic MS` reads `audio:in` for MS
//! milliseconds (0: 1000) and logs, every 100 ms, the peak and the RMS of
//! the left channel (full scale 32767) and the frames lost so far.
//!
//! Built with `--permit audio:in`, a strong permission: signed with the
//! system key. Exit 0, or the step that failed.

use t5_rt::{Error, Handle, audio, event, rights, wait};

t5_rt::entry!(main);

fn main(ms: usize, _: usize) -> usize {
    match listen(if ms == 0 { 1000 } else { ms as u32 }) {
        Ok(()) => 0,
        Err((step, e)) => {
            event!(Error, "mic.failed", step = step, error = e as u32);
            step as usize
        }
    }
}

/// Integer square root.
fn isqrt(v: u64) -> u32 {
    let (mut lo, mut hi) = (0u64, 1u64 << 32);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if mid * mid <= v { lo = mid } else { hi = mid }
    }
    lo as u32
}

fn listen(ms: u32) -> Result<(), (u32, Error)> {
    let mic = Handle::open(audio::IN, rights::READ | rights::CTL | rights::WAIT).map_err(|e| (1, e))?;
    let rate = mic.ctl(audio::RATE, 0).map_err(|e| (2, e))? as u32;
    event!(Info, "mic.start", ms = ms, rate = rate);
    let per_block = rate / 10;
    let total = (rate as u64 * ms as u64 / 1000) as u32;
    let mut buf = [0u8; 1024];
    let (mut got, mut in_block, mut n) = (0u32, 0u32, 0u32);
    let (mut peak, mut sum) = (0u32, 0u64);
    while got < total {
        wait(&[&mic], t5_rt::FOREVER).map_err(|e| (3, e))?;
        loop {
            let k = mic.read(&mut buf).map_err(|e| (4, e))?;
            if k == 0 {
                break;
            }
            for f in buf[..k].chunks_exact(audio::FRAME) {
                let s = i16::from_le_bytes([f[0], f[1]]) as i32;
                peak = peak.max(s.unsigned_abs());
                sum += (s * s) as u64;
                in_block += 1;
                got += 1;
                if in_block == per_block {
                    let lost = mic.ctl(audio::LOST, 0).unwrap_or(0) as u32;
                    event!(Info, "mic.level", n = n, peak = peak, rms = isqrt(sum / in_block as u64), lost = lost);
                    (n, in_block, peak, sum) = (n + 1, 0, 0, 0);
                }
            }
        }
    }
    event!(Info, "mic.done", frames = got, lost = mic.ctl(audio::LOST, 0).map_err(|e| (5, e))? as u32);
    Ok(())
}
