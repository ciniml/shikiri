#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! M3d test app (doc/14 §4): waits `count` times on a periodic timer of
//! `period_ms`, with a one-shot timer as a deadline for the whole run, and
//! checks the handle rules on the way. Built with `--permit timer`.
//!
//! Exit 0 if everything held, otherwise the number of the failed check.

use t5_rt::{Error, FOREVER, Handle, Timer, close_raw, event, rights, time_ns, wait, wait_raw};

t5_rt::entry!(main);

/// How late a wake-up may be: a slice of a busy process (10 ms) plus slack.
const LATE_US: u64 = 12_000;

fn main(period_ms: usize, count: usize) -> usize {
    match run(period_ms as u64, count as u32) {
        Ok(()) => 0,
        Err(check) => {
            event!(Error, "timers.failed", period_ms = period_ms, check = check);
            check as usize
        }
    }
}

fn check(ok: bool, n: u32) -> Result<(), u32> {
    if ok { Ok(()) } else { Err(n) }
}

fn run(period_ms: u64, count: u32) -> Result<(), u32> {
    let period_us = period_ms * 1000;
    let tick = Timer::open().map_err(|_| 1u32)?;
    let deadline = Timer::open().map_err(|_| 2u32)?;
    // Names outside the Permissions TLV are refused.
    check(Handle::open("display", rights::WAIT).err() == Some(Error::Denied), 3)?;
    // Too short a period, and an unknown operation.
    check(tick.set_periodic_ns(1).err() == Some(Error::Invalid), 4)?;
    check(tick.0.ctl(99, 0).err() == Some(Error::NoSupport), 5)?;
    // Nothing set: polling times out at once.
    check(wait(&[&tick.0], 0).err() == Some(Error::TimedOut), 6)?;

    // Read the clock first: the first expiry is a period after the set.
    let start = time_ns();
    tick.set_periodic_ns(period_ms * 1_000_000).map_err(|_| 7u32)?;
    deadline.set_oneshot_ns((count as u64 + 2) * period_ms * 1_000_000).map_err(|_| 8u32)?;
    let mut last = start;
    let (mut min_gap, mut max_gap, mut coalesced) = (u64::MAX, 0, 0);
    for _ in 0..count {
        let e = wait(&[&deadline.0, &tick.0], FOREVER).map_err(|_| 9u32)?;
        check(e.index == 1 && e.kind == t5_rt::timer::EXPIRED, 10)?;
        coalesced += e.value as u32 - 1;
        let now = time_ns();
        let gap = (now - last) / 1000;
        last = now;
        min_gap = min_gap.min(gap);
        max_gap = max_gap.max(gap);
    }
    let elapsed_us = (last - start) / 1000;
    // Expiries are on a fixed grid, so the total cannot drift even if one
    // wake-up is late.
    check(elapsed_us >= count as u64 * period_us && elapsed_us <= count as u64 * period_us + LATE_US, 11)?;
    check(max_gap <= period_us + LATE_US, 12)?;

    // The deadline fires next; then, stopped, the tick produces nothing.
    tick.stop().map_err(|_| 13u32)?;
    let e = wait(&[&tick.0, &deadline.0], FOREVER).map_err(|_| 14u32)?;
    check(e.index == 1, 15)?;
    check(wait(&[&tick.0], 2_000_000).err() == Some(Error::TimedOut), 16)?;

    // A closed handle is no longer usable.
    let raw = tick.0.raw();
    drop(tick);
    check(wait_raw(&[raw], 0).err() == Some(Error::Invalid), 17)?;
    check(close_raw(raw).err() == Some(Error::Invalid), 18)?;

    event!(
        Info,
        "timers.done",
        period_ms = period_ms,
        count = count,
        elapsed_us = elapsed_us,
        min_gap_us = min_gap,
        max_gap_us = max_gap,
        coalesced = coalesced
    );
    Ok(())
}
