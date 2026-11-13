// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The async runtime for this service only (doc/48 §1: async starts in
//! `ble`): one top-level future, polled again at once whenever something
//! woke it; otherwise the process `wait`s on the handles that pending I/O
//! asked for (`want`), up to the next embassy-time deadline. Also the
//! embassy-time driver (the kernel's clock, 1 MHz ticks) and a
//! critical-section that does nothing (one thread, no interrupts in a
//! process).

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use t5_rt::{chan, time_ns, wait_raw};

const WANT_MAX: usize = 8;
/// Rounds woken back to back before a pause, and the pause (it ends at
/// once when a handle has an event, so I/O is not delayed). 64 rounds take
/// about 0.8 ms on the Tab5.
const SPIN_ROUNDS: u32 = 64;
const SPIN_PAUSE_NS: u64 = 10_000_000;

/// One thread: none of this is shared.
struct Round {
    woken: bool,
    want: [u32; WANT_MAX],
    wants: usize,
    /// Earliest embassy-time deadline asked for (µs).
    deadline: u64,
    /// Channels whose other end has closed: `wait` reports that once, as
    /// an event; `read` only says `Busy`.
    closed: [u32; WANT_MAX],
}

static mut ROUND: Round = Round { woken: false, want: [0; WANT_MAX], wants: 0, deadline: u64::MAX, closed: [0; WANT_MAX] };

fn round() -> &'static mut Round {
    unsafe { &mut *core::ptr::addr_of_mut!(ROUND) }
}

/// Asks the next wait to include handle `h` (a read or accept that found
/// nothing).
pub fn want(h: u32) {
    let r = round();
    if !r.want[..r.wants].contains(&h) && r.wants < WANT_MAX {
        r.want[r.wants] = h;
        r.wants += 1;
    }
}

/// Whether `wait` has reported channel `h`'s other end closed.
pub fn closed(h: u32) -> bool {
    h != 0 && round().closed.contains(&h)
}

/// Forgets what was seen of `h` (before its handle is closed and reused).
pub fn forget(h: u32) {
    for c in round().closed.iter_mut().filter(|c| **c == h) {
        *c = 0;
    }
}

fn wake() {
    round().woken = true;
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(core::ptr::null(), &VTABLE),
    |_| wake(),
    |_| wake(),
    |_| {},
);

/// Runs `f` to the end.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut spins = 0u32;
    loop {
        let r = round();
        r.woken = false;
        r.wants = 0;
        r.deadline = u64::MAX;
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        let r = round();
        // A future that wakes itself on every poll (embassy-sync's
        // `OnceLock::get`) would keep this core busy: after a long run of
        // such rounds, wait a little anyway.
        let timeout = if r.woken {
            spins += 1;
            if spins < SPIN_ROUNDS {
                continue;
            }
            spins = 0;
            SPIN_PAUSE_NS
        } else {
            spins = 0;
            let now = time_ns() / 1000;
            if r.deadline <= now {
                continue;
            }
            r.deadline.saturating_sub(now).saturating_mul(1000)
        };
        // Errors (the timeout) only mean: poll again.
        if let Ok(ev) = wait_raw(&r.want[..r.wants], timeout)
            && ev.kind == chan::PEER_CLOSED
        {
            let h = r.want[ev.index];
            if !closed(h)
                && let Some(c) = r.closed.iter_mut().find(|c| **c == 0)
            {
                *c = h;
            }
        }
    }
}

struct Driver;

impl embassy_time_driver::Driver for Driver {
    fn now(&self) -> u64 {
        time_ns() / 1000
    }

    fn schedule_wake(&self, at: u64, _waker: &Waker) {
        let r = round();
        r.deadline = r.deadline.min(at);
    }
}

embassy_time_driver::time_driver_impl!(static DRIVER: Driver = Driver);

struct NoInterrupts;
critical_section::set_impl!(NoInterrupts);

unsafe impl critical_section::Impl for NoInterrupts {
    unsafe fn acquire() -> critical_section::RawRestoreState {}
    unsafe fn release(_: critical_section::RawRestoreState) {}
}
