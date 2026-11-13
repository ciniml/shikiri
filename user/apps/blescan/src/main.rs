#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! Scans through the BLE service (doc/48 M2): `run blescan [SECONDS
//! [WINDOW_MS]]` asks `svc:ble` for an active scan of SECONDS (10 if 0)
//! with a 100 ms interval and a WINDOW_MS window (30 if 0), and logs the
//! first advertisers and the totals. Built with `--permit svc:ble`. Exit
//! 0, or the step that failed.

use t5_rt::{Channel, ble, event};

t5_rt::entry!(main);

const INTERVAL_MS: u16 = 100;
const LOG_ADVERTISERS: usize = 16;

fn main(seconds: usize, window: usize) -> usize {
    let Ok(c) = Channel::connect(ble::SVC) else {
        event!(Error, "blescan.no_service");
        return 1;
    };
    let seconds = if seconds == 0 { 10 } else { seconds.min(255) } as u8;
    let window = if window == 0 { 30 } else { window.min(INTERVAL_MS as usize) } as u16;
    let mut req = [ble::SCAN, seconds, 1, 0, 0, 0, 0];
    req[3..5].copy_from_slice(&INTERVAL_MS.to_le_bytes());
    req[5..7].copy_from_slice(&window.to_le_bytes());
    if c.send(&req).is_err() {
        return 2;
    }
    let mut seen = [[0u8; 6]; LOG_ADVERTISERS];
    let (mut advertisers, mut reports) = (0usize, 0u32);
    let mut buf = [0u8; ble::MSG_MAX];
    loop {
        let Ok(n) = c.recv(&mut buf) else {
            event!(Error, "blescan.service_gone");
            return 3;
        };
        let m = &buf[..n];
        match m.first() {
            Some(&ble::ADV) if n >= 11 => {
                reports += 1;
                let mut addr = [0u8; 6];
                addr.copy_from_slice(&m[2..8]);
                if advertisers < LOG_ADVERTISERS && !seen[..advertisers].contains(&addr) {
                    seen[advertisers] = addr;
                    advertisers += 1;
                    event!(Info, "blescan.adv", addr = &addr[..], addr_type = m[1], kind = m[8], rssi = m[9] as i8 as i32, data = &m[11..n]);
                }
            }
            Some(&ble::DONE) if n >= 9 => {
                let word = |i: usize| u32::from_le_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
                event!(Info, "blescan.done", advertisers = advertisers as u32, reports = reports, sent = word(1), dropped = word(5));
                return 0;
            }
            Some(&ble::FAILED) => {
                event!(Error, "blescan.failed", request = m.get(1).copied().unwrap_or(0), code = m.get(2).copied().unwrap_or(0));
                return 4;
            }
            _ => {}
        }
    }
}
