#![no_std]
#![no_main]

//! Whether the speaker sounds, heard by the microphones (doc/70 §9.3):
//! `run spkcheck HZ MS` records `audio:in` for 300 ms of quiet, then plays a
//! sine of HZ (0: 1000) for MS milliseconds (0: 1500) on `audio:out` while
//! recording, and logs the level at HZ (Goertzel over 100 ms blocks of the
//! left channel, as the amplitude of a sine, full scale 32767) in the quiet
//! and during the tone (the first 200 ms of it skipped: the latency).
//! `heard` when the tone's level is 8 times the quiet's and at least 64.
//!
//! Built with `--permit audio:out --permit audio:in` (strong: the system
//! key). Exit 0 heard, 1 not heard, or 10 + the step that failed.

use t5_rt::{Error, Handle, audio, event, rights, wait};

t5_rt::entry!(main);

const AMPLITUDE: f32 = 8192.0;
const QUIET_MS: u32 = 300;
const SKIP_MS: u32 = 200;
const BLOCK: usize = 256;

fn main(hz: usize, ms: usize) -> usize {
    let (hz, ms) = (if hz == 0 { 1000 } else { hz as u32 }, if ms == 0 { 1500 } else { ms as u32 });
    match check(hz, ms) {
        Ok(heard) => (!heard) as usize,
        Err((step, e)) => {
            event!(Error, "spkcheck.failed", step = step, error = e as u32);
            10 + step as usize
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

fn sqrt(v: f32) -> f32 {
    if v <= 0.0 {
        return 0.0;
    }
    let mut x = if v > 1.0 { v } else { 1.0 };
    for _ in 0..40 {
        x = 0.5 * (x + v / x);
    }
    x
}

/// The level at one frequency, block by block.
struct Goertzel {
    c: f32,
    n: usize,
    len: usize,
    s1: f32,
    s2: f32,
    blocks: u32,
    sum: f32,
    min: f32,
    max: f32,
}

impl Goertzel {
    fn new(c: f32, len: usize) -> Self {
        Goertzel { c, n: 0, len, s1: 0.0, s2: 0.0, blocks: 0, sum: 0.0, min: f32::MAX, max: 0.0 }
    }

    fn add(&mut self, x: f32) {
        let s = x + self.c * self.s1 - self.s2;
        self.s2 = self.s1;
        self.s1 = s;
        self.n += 1;
        if self.n == self.len {
            let p = self.s1 * self.s1 + self.s2 * self.s2 - self.c * self.s1 * self.s2;
            let a = 2.0 * sqrt(p) / self.len as f32;
            self.sum += a;
            self.min = self.min.min(a);
            self.max = self.max.max(a);
            self.blocks += 1;
            (self.n, self.s1, self.s2) = (0, 0.0, 0.0);
        }
    }

    fn mean(&self) -> u32 {
        if self.blocks == 0 { 0 } else { (self.sum / self.blocks as f32) as u32 }
    }
}

fn check(hz: u32, ms: u32) -> Result<bool, (u32, Error)> {
    let mic = Handle::open(audio::IN, rights::READ | rights::CTL | rights::WAIT).map_err(|e| (1, e))?;
    let rate = mic.ctl(audio::RATE, 0).map_err(|e| (2, e))? as u32;
    let w = 2.0 * core::f32::consts::PI * hz as f32 / rate as f32;
    let c = 2.0 * cos(w);
    let len = (rate / 10) as usize;
    let mut buf = [0u8; 1024];

    // The quiet.
    let mut quiet = Goertzel::new(c, len);
    let mut got = 0u32;
    while got < rate * QUIET_MS / 1000 {
        wait(&[&mic], t5_rt::FOREVER).map_err(|e| (3, e))?;
        let k = mic.read(&mut buf).map_err(|e| (4, e))?;
        for f in buf[..k].chunks_exact(audio::FRAME) {
            quiet.add(i16::from_le_bytes([f[0], f[1]]) as f32);
            got += 1;
        }
    }

    // The tone, recorded as it plays.
    let out = Handle::open(audio::OUT, rights::WRITE | rights::CTL | rights::WAIT).map_err(|e| (5, e))?;
    let (mut y1, mut y2) = (0.0f32, -AMPLITUDE * (w - w * w * w / 6.0));
    let total = rate * ms / 1000;
    let skip = rate * SKIP_MS / 1000;
    let mut tone = Goertzel::new(c, len);
    let (mut done, mut heard_n) = (0u32, 0u32);
    let mut block = [0u8; BLOCK * audio::FRAME];
    let mut pending = 0..0;
    while heard_n < total {
        if pending.is_empty() && done < total {
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
        let mut moved = 0;
        if !pending.is_empty() {
            let k = out.write(&block[pending.clone()]).map_err(|e| (6, e))?;
            pending.start += k;
            moved += k;
        }
        let k = mic.read(&mut buf).map_err(|e| (7, e))?;
        moved += k;
        for f in buf[..k].chunks_exact(audio::FRAME) {
            if heard_n >= skip {
                tone.add(i16::from_le_bytes([f[0], f[1]]) as f32);
            }
            heard_n += 1;
        }
        if moved == 0 {
            wait(&[&mic], t5_rt::FOREVER).map_err(|e| (8, e))?;
        }
    }
    let lost = mic.ctl(audio::LOST, 0).map_err(|e| (9, e))? as u32;
    let (q, t) = (quiet.mean(), tone.mean());
    let heard = t >= 64 && t >= 8 * q.max(1);
    event!(
        Info,
        "spkcheck.result",
        hz = hz,
        quiet = q,
        tone = t,
        tone_min = tone.min as u32,
        tone_max = tone.max as u32,
        blocks = tone.blocks,
        lost = lost,
        heard = heard
    );
    Ok(heard)
}
