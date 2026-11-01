//! A sampling profiler for one process (doc/34).
//!
//! `prof start PID US`: while process PID runs, the alarm also comes every
//! US µs (besides the end of the slice); each such tick records the
//! process's PC. The kernel runs with interrupts off, so a tick that comes
//! while the kernel works for the process (a system call) is taken right
//! after the return to U-mode, before any instruction: such samples are
//! marked (bit 0 set) and count as kernel time. System calls are counted
//! too, with the cycles (`mcycle`) the kernel spent on them, per number.
//!
//! `prof stop` logs the summary and the system calls; `prof dump K` logs
//! samples `K * DUMP` onwards (in parts, so the log keeps up).

use t5_abi::event;
use t5_abi::log::Addr;

use crate::platform::{Current, Platform};
use crate::time;

/// Samples kept (32 KiB on the P4, in RAM_HI's .bss; QEMU's RAM is
/// smaller).
#[cfg(not(feature = "platform-virt"))]
const SAMPLES: usize = 8192;
#[cfg(feature = "platform-virt")]
const SAMPLES: usize = 512;
/// Samples per `prof dump`, and per event.
pub const DUMP: usize = 512;
const PER_EVENT: usize = 16;
/// System call numbers counted apart; the rest share the last entry.
const SYSCALLS: usize = 16;

struct Prof {
    pid: u32,
    /// Ticks between samples; 0 = off.
    interval: u64,
    /// When the next sample is due. Kept here rather than counted from
    /// each return to U-mode: a process making system calls every few µs
    /// would never be sampled.
    next: u64,
    /// The process's text start, for the host to subtract.
    text: usize,
    n: usize,
    dropped: u32,
    samples: [u32; SAMPLES],
    sys: [(u16, u32, u64); SYSCALLS],
    started: u64,
    stopped: u64,
}

static mut PROF: Prof = Prof {
    pid: 0,
    interval: 0,
    next: 0,
    text: 0,
    n: 0,
    dropped: 0,
    samples: [0; SAMPLES],
    sys: [(0, 0, 0); SYSCALLS],
    started: 0,
    stopped: 0,
};

/// Only touched with the kernel lock held.
fn prof() -> &'static mut Prof {
    unsafe { &mut *core::ptr::addr_of_mut!(PROF) }
}

pub fn start(pid: u32, interval_us: u32, text: usize) {
    let p = prof();
    p.pid = pid;
    p.interval = time::ns_to_ticks(interval_us.max(20) as u64 * 1000);
    p.text = text;
    p.n = 0;
    p.dropped = 0;
    p.sys = [(0, 0, 0); SYSCALLS];
    p.started = Current::now();
    p.next = p.started + p.interval;
    event!(Info, "prof.started", pid = pid, interval_us = interval_us, text = Addr(text as u32));
}

pub fn stop() {
    let p = prof();
    if p.interval != 0 {
        p.interval = 0;
        p.stopped = Current::now();
    }
    let ms = (time::ticks_to_ns(p.stopped.saturating_sub(p.started)) / 1_000_000) as u32;
    event!(
        Info,
        "prof.summary",
        pid = p.pid,
        ms = ms,
        samples = p.n as u32,
        dropped = p.dropped,
        text = Addr(p.text as u32)
    );
    for &(nr, count, cycles) in p.sys.iter().filter(|e| e.1 != 0) {
        event!(Info, "prof.syscall", nr = nr as u32, count = count, kcycles = (cycles / 1000) as u32);
    }
}

pub fn dump(part: u32) {
    let p = prof();
    let from = (part as usize * DUMP).min(p.n);
    let to = (from + DUMP).min(p.n);
    for (k, chunk) in p.samples[from..to].chunks(PER_EVENT).enumerate() {
        let mut bytes = [0u8; PER_EVENT * 4];
        for (j, s) in chunk.iter().enumerate() {
            bytes[4 * j..4 * j + 4].copy_from_slice(&s.to_le_bytes());
        }
        event!(Info, "prof.pcs", index = (from + k * PER_EVENT) as u32, pcs = &bytes[..chunk.len() * 4]);
    }
}

/// When the next sample of process `pid` is due (ticks), if it is
/// profiled.
pub fn next(pid: u32) -> Option<u64> {
    let p = prof();
    (p.interval != 0 && p.pid == pid).then_some(p.next)
}

/// Process `pid` starts a slice: a sample due already moves on.
pub fn resumed(pid: u32) {
    let p = prof();
    if p.interval != 0 && p.pid == pid {
        let now = Current::now();
        if p.next <= now {
            p.next = now + p.interval;
        }
    }
}

/// A sample: `pc` where the tick found the process; `entered` whether no
/// instruction ran since the kernel returned to it.
pub fn sample(pc: usize, entered: bool) {
    let p = prof();
    p.next = Current::now() + p.interval;
    if p.n < SAMPLES {
        p.samples[p.n] = pc as u32 | entered as u32;
        p.n += 1;
    } else {
        p.dropped += 1;
    }
}

/// A system call of the profiled process and the cycles it took.
pub fn syscall(nr: usize, cycles: u32) {
    let p = prof();
    let nr = nr.min(u16::MAX as usize) as u16;
    let i = p.sys.iter().position(|e| e.1 == 0 || e.0 == nr).unwrap_or(SYSCALLS - 1);
    let e = &mut p.sys[i];
    e.0 = nr;
    e.1 += 1;
    e.2 += cycles as u64;
}
