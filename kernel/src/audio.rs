//! The `audio:out` and `audio:in` objects (doc/70 §3, stage A2): PCM frames
//! (16-bit stereo) through `write` / `read`, mixed and split here.
//!
//! Each open handle has a queue in the L2MEM `init` was given (after the
//! stream's DMA rings, main.rs): byte counters `r` and `w` that only grow,
//! the bytes at `counter % size`. `poll` (from the scheduler loop, and after
//! the stream's interrupt) moves them on:
//! - play: as much as the stream takes and some queue has, in chunks: each
//!   queue's samples scaled by its volume and added (saturating; a queue with
//!   less than the others adds silence for the rest), then pushed.
//! - record: what the stream received is copied to every `audio:in` queue;
//!   what does not fit is dropped and counted.
//!
//! The stream's interrupt is on while any handle is open (`users`), and for
//! `QUIET_MS` after the last push: kumiki clears each played descriptor to
//! silence when it polls, and without polls the TX ring would go on
//! repeating its last turn (`tools/wavcheck.py` heard it in p4emu-run-44).
//! `audio:volume` sets the speaker's level in the codec (the platform's
//! `audio_volume`), for every stream; the per-stream `VOLUME` scales here.
//!
//! Handles go through `write`/`read` rather than mapped rings: a system call
//! per few ms costs little at 192 KB/s, needs no PMP region, and keeps the
//! mixing here.

use t5_abi::{Error, audio};

use crate::platform::{Current, Platform};

const OUTS: usize = 4;
const INS: usize = 2;
/// Most bytes a queue holds (43 ms at 48 kHz).
const QUEUE_MAX: usize = 8192;
/// Bytes mixed or copied at a time.
const CHUNK: usize = 512;
/// `READABLE` from this many bytes (or half the queue).
const READ_MIN: usize = 1024;
/// The interrupt stays on this long after the last push: two turns of the
/// TX ring (85 ms) and some.
const QUIET_MS: u64 = 200;

#[derive(Clone, Copy)]
struct Queue {
    used: bool,
    r: usize,
    w: usize,
}

impl Queue {
    const FREE: Queue = Queue { used: false, r: 0, w: 0 };

    fn len(&self) -> usize {
        self.w - self.r
    }
}

struct State {
    /// Where queue `i` (outs first, then ins) starts: `base + i * size`.
    base: usize,
    size: usize,
    outs: [Queue; OUTS],
    gain: [u16; OUTS],
    ins: [Queue; INS],
    lost: [u32; INS],
    users: usize,
    /// The stream's interrupt is on.
    irq: bool,
    /// When the TX ring is silent again after the last push (ticks).
    quiet_at: u64,
    /// The speaker's level (`audio:volume`).
    master: u8,
}

static mut S: State =
    State { base: 0, size: 0, outs: [Queue::FREE; OUTS], gain: [256; OUTS], ins: [Queue::FREE; INS], lost: [0; INS], users: 0, irq: false, quiet_at: 0, master: 100 };

/// Single core (core 0's scheduler loop and interrupt, with the lock held).
fn st() -> &'static mut State {
    unsafe { &mut *core::ptr::addr_of_mut!(S) }
}

/// The queues go at `base..base + len` (L2MEM nothing else uses).
pub fn init(base: usize, len: usize) {
    let size = (len / (OUTS + INS)).min(QUEUE_MAX) & !63;
    if size < CHUNK * 2 {
        return;
    }
    let s = st();
    s.base = base;
    s.size = size;
    t5_abi::event!(Info, "audio.queues", base = t5_abi::log::Addr(base as u32), size = size as u32, outs = OUTS as u32, ins = INS as u32);
}

/// Copies `data` into queue `i`'s memory at counter `at`.
fn put(s: &State, i: usize, at: usize, data: &[u8]) {
    let pos = at % s.size;
    let first = data.len().min(s.size - pos);
    let m = (s.base + i * s.size) as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), m.add(pos), first);
        core::ptr::copy_nonoverlapping(data.as_ptr().add(first), m, data.len() - first);
    }
}

/// Copies from queue `i`'s memory at counter `at` into `out`.
fn get(s: &State, i: usize, at: usize, out: &mut [u8]) {
    let pos = at % s.size;
    let first = out.len().min(s.size - pos);
    let m = (s.base + i * s.size) as *const u8;
    unsafe {
        core::ptr::copy_nonoverlapping(m.add(pos), out.as_mut_ptr(), first);
        core::ptr::copy_nonoverlapping(m, out.as_mut_ptr().add(first), out.len() - first);
    }
}

fn opened() -> Result<(), Error> {
    if Current::audio_rate() == 0 || st().size == 0 {
        return Err(Error::NoDevice);
    }
    Ok(())
}

/// `audio:volume` opens on boards with a stream.
pub fn master_check() -> Result<(), Error> {
    opened()
}

pub fn master_ctl(op: usize, arg: u64) -> Result<usize, Error> {
    match op {
        audio::LEVEL => Ok(st().master as usize),
        audio::VOLUME if arg <= 100 => {
            let r = Current::audio_volume(arg as u8);
            t5_abi::event!(Info, "audio.volume", volume = arg as u32, ok = r.is_ok());
            r.map_err(|_| Error::Fail)?;
            st().master = arg as u8;
            Ok(0)
        }
        audio::VOLUME => Err(Error::Invalid),
        _ => Err(Error::NoSupport),
    }
}

fn add_user() {
    let s = st();
    s.users += 1;
    if !s.irq {
        s.irq = true;
        Current::audio_interrupts(true);
    }
}

/// The interrupt goes off in `poll` once the ring is silent.
fn drop_user() {
    st().users -= 1;
}

/// `audio:out`.
pub struct OutHandle {
    slot: usize,
}

impl OutHandle {
    pub fn open() -> Result<OutHandle, Error> {
        opened()?;
        let s = st();
        let slot = s.outs.iter().position(|q| !q.used).ok_or(Error::Busy)?;
        s.outs[slot] = Queue { used: true, r: 0, w: 0 };
        s.gain[slot] = 256;
        add_user();
        Ok(OutHandle { slot })
    }

    fn free(&self) -> usize {
        st().size - st().outs[self.slot].len()
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, Error> {
        let n = (data.len() / audio::FRAME * audio::FRAME).min(self.free());
        let s = st();
        put(s, self.slot, s.outs[self.slot].w, &data[..n]);
        s.outs[self.slot].w += n;
        poll();
        Ok(n)
    }

    pub fn ctl(&mut self, op: usize, arg: u64) -> Result<usize, Error> {
        match op {
            audio::RATE => Ok(Current::audio_rate() as usize),
            audio::QUEUED => Ok(self.free()),
            audio::VOLUME if arg <= 100 => {
                st().gain[self.slot] = (arg * 256 / 100) as u16;
                Ok(0)
            }
            audio::VOLUME => Err(Error::Invalid),
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let free = self.free();
        (free >= st().size / 2).then_some((audio::WRITABLE, free))
    }
}

impl Drop for OutHandle {
    fn drop(&mut self) {
        st().outs[self.slot] = Queue::FREE;
        drop_user();
    }
}

/// `audio:in`.
pub struct InHandle {
    slot: usize,
}

impl InHandle {
    pub fn open() -> Result<InHandle, Error> {
        opened()?;
        let s = st();
        let slot = s.ins.iter().position(|q| !q.used).ok_or(Error::Busy)?;
        // Nobody read what the stream recorded before: from now on.
        if s.ins.iter().all(|q| !q.used) {
            let mut buf = [0u8; CHUNK];
            while Current::audio_pop(&mut buf) > 0 {}
        }
        s.ins[slot] = Queue { used: true, r: 0, w: 0 };
        s.lost[slot] = 0;
        add_user();
        Ok(InHandle { slot })
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        poll();
        let s = st();
        let q = s.ins[self.slot];
        let n = (out.len() / audio::FRAME * audio::FRAME).min(q.len());
        get(s, OUTS + self.slot, q.r, &mut out[..n]);
        s.ins[self.slot].r += n;
        Ok(n)
    }

    pub fn ctl(&mut self, op: usize, _arg: u64) -> Result<usize, Error> {
        match op {
            audio::RATE => Ok(Current::audio_rate() as usize),
            audio::QUEUED => Ok(st().ins[self.slot].len()),
            audio::LOST => Ok(st().lost[self.slot] as usize),
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = st().ins[self.slot].len();
        (n >= READ_MIN.min(st().size / 2)).then_some((audio::READABLE, n))
    }
}

impl Drop for InHandle {
    fn drop(&mut self) {
        st().ins[self.slot] = Queue::FREE;
        drop_user();
    }
}

/// Mixes the `audio:out` queues into the stream and copies what it
/// recorded to the `audio:in` queues.
pub fn poll() {
    let s = st();
    // The loop idles with interrupts masked: the stream is serviced here
    // too, or a pending interrupt would wake every `wfi` and, with no
    // process running, kumiki would stop clearing what was played.
    if s.irq {
        Current::audio_service();
    }
    if s.users == 0 {
        if s.irq && Current::now() >= s.quiet_at {
            s.irq = false;
            Current::audio_interrupts(false);
        }
        return;
    }
    loop {
        let most = s.outs.iter().filter(|q| q.used).map(Queue::len).max().unwrap_or(0);
        let n = Current::audio_space().min(most).min(CHUNK) / audio::FRAME * audio::FRAME;
        if n == 0 {
            break;
        }
        let mut mix = [0i32; CHUNK / 2];
        let mut buf = [0u8; CHUNK];
        for i in 0..OUTS {
            let q = s.outs[i];
            let k = q.len().min(n);
            if !q.used || k == 0 {
                continue;
            }
            get(s, i, q.r, &mut buf[..k]);
            s.outs[i].r += k;
            let g = s.gain[i] as i32;
            for (m, b) in mix.iter_mut().zip(buf[..k].chunks_exact(2)) {
                *m += i16::from_le_bytes([b[0], b[1]]) as i32 * g >> 8;
            }
        }
        for (b, m) in buf[..n].chunks_exact_mut(2).zip(mix.iter()) {
            b.copy_from_slice(&((*m).clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes());
        }
        Current::audio_push(&buf[..n]);
        s.quiet_at = Current::now() + crate::time::ms_to_ticks(QUIET_MS);
    }
    if s.ins.iter().any(|q| q.used) {
        let mut buf = [0u8; CHUNK];
        loop {
            let n = Current::audio_pop(&mut buf);
            if n == 0 {
                break;
            }
            for i in 0..INS {
                let q = s.ins[i];
                if !q.used {
                    continue;
                }
                let k = (s.size - q.len()).min(n);
                put(s, OUTS + i, q.w, &buf[..k]);
                s.ins[i].w += k;
                s.lost[i] = s.lost[i].saturating_add(((n - k) / audio::FRAME) as u32);
            }
        }
    }
}
