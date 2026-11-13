#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! M2 test app: sleeps `period_ms` five times and checks that each sleep
//! lasted at least that long. Exit 1 if a sleep ended early.

use t5_rt::{event, sleep_ms, time_ns, yield_now};

t5_rt::entry!(main);

const TICKS: u32 = 5;

fn main(id: usize, period_ms: usize) -> usize {
    yield_now();
    for i in 0..TICKS {
        let before = time_ns();
        sleep_ms(period_ms as u64);
        let slept_us = (time_ns() - before) / 1000;
        event!(Info, "ticker.tick", id = id, tick = i, slept_us = slept_us);
        if slept_us < period_ms as u64 * 1000 {
            return 1;
        }
    }
    0
}
