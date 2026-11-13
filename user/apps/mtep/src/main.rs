#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! An app's Matter endpoint (doc/53), for tests: `run mtep KIND SECONDS`.
//! KIND: 0 `light`, 1 `plug`, 2 `contact`, 3 `temp`, 4 `humidity` (the
//! kind is the label too). It asks for the endpoint, then for SECONDS (0: until stopped):
//! every 2 s a sensor gives a new value (contact: open and closed in turn;
//! temp: 20.00 to 24.50 ℃; humidity: 40.00 to 49.00 %), and what `matter`
//! tells it (`on 0`, `on 1`) is logged as `mtep.state`. Built with
//! `--permit svc:matter/endpoint`. Exit 0, or the step that failed.

use core::fmt::Write as _;

use t5_rt::log::FmtBuf;
use t5_rt::{Channel, Error, chan, event, matter, time_ns, wait};

t5_rt::entry!(main);

const KINDS: [&str; 5] = ["light", "plug", "contact", "temp", "humidity"];
const PERIOD_MS: u64 = 2000;

fn main(kind: usize, seconds: usize) -> usize {
    match run(kind, seconds as u64) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "mtep.failed", step = step);
            step as usize
        }
    }
}

fn now_ms() -> u64 {
    time_ns() / 1_000_000
}

fn run(kind: usize, seconds: u64) -> Result<(), u32> {
    let name = *KINDS.get(kind).ok_or(1u32)?;
    let c = Channel::connect(matter::SVC).map_err(|_| 2u32)?;
    let mut req = FmtBuf::<32>::new();
    let _ = write!(req, "endpoint {name} {name}");
    c.send(req.as_str().as_bytes()).map_err(|_| 3u32)?;
    let mut m = [0u8; 128];
    loop {
        match c.recv(&mut m).map_err(|_| 4u32)? {
            k if k >= 1 && m[0] == matter::LINE => {
                event!(Info, "mtep.line", text = core::str::from_utf8(&m[1..k]).unwrap_or("?"));
            }
            k if k >= 2 && m[0] == matter::END => {
                if m[1] == 0 {
                    return Err(5);
                }
                break;
            }
            _ => {}
        }
    }
    let end = (seconds != 0).then(|| now_ms() + seconds * 1000);
    let mut next = now_ms();
    let mut i = 0u32;
    loop {
        let now = now_ms();
        if end.is_some_and(|e| now >= e) {
            return Ok(());
        }
        if now >= next && kind >= 2 {
            next = now + PERIOD_MS;
            let v = match kind {
                2 => i % 2,
                3 => 2000 + i % 10 * 50,
                _ => 4000 + i % 10 * 100,
            };
            i += 1;
            let mut t = FmtBuf::<24>::new();
            let _ = write!(t, "set {v}");
            c.send(t.as_str().as_bytes()).map_err(|_| 6u32)?;
            event!(Info, "mtep.set", value = v);
        }
        loop {
            match c.0.read(&mut m) {
                Ok(k) if k >= 1 && m[0] == matter::STATE => {
                    event!(Info, "mtep.state", text = core::str::from_utf8(&m[1..k]).unwrap_or("?"));
                }
                Ok(_) => {}
                Err(Error::Busy) => break,
                Err(_) => return Err(7),
            }
        }
        let wake = end.unwrap_or(u64::MAX).min(if kind >= 2 { next } else { u64::MAX });
        let ms = wake.saturating_sub(now_ms()).clamp(1, 1000);
        if let Ok(ev) = wait(&[&c.0], ms * 1_000_000)
            && ev.kind == chan::PEER_CLOSED
        {
            return Err(8);
        }
    }
}
