#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! Logs the keyboard's events (doc/39). `run keylog SECONDS [COUNT]`: for
//! SECONDS (10 if 0), or until COUNT events came. Built with `--permit
//! kbd`. Exit 0, or 1 if `kbd` could not be opened.

use t5_rt::{Handle, event, kbd, rights, time_ns, wait};

t5_rt::entry!(main);

fn main(seconds: usize, count: usize) -> usize {
    let Ok(h) = Handle::open(kbd::NAME, rights::READ | rights::WAIT) else {
        event!(Error, "keylog.no_keyboard");
        return 1;
    };
    let end = time_ns() + if seconds == 0 { 10 } else { seconds as u64 } * 1_000_000_000;
    let mut seen = 0u32;
    while time_ns() < end && (count == 0 || (seen as usize) < count) {
        if wait(&[&h], end - time_ns()).is_err() {
            continue;
        }
        let mut buf = [0u8; kbd::EVENT_SIZE * 8];
        let Ok(n) = h.read(&mut buf) else { continue };
        for ev in buf[..n].chunks(kbd::EVENT_SIZE) {
            event!(Info, "keylog.key", modifier = ev[0], usage = ev[1]);
            seen += 1;
        }
    }
    event!(Info, "keylog.done", events = seen);
    0
}
