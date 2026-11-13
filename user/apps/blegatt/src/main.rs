#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! The BLE service's central role (doc/48 M3): `run blegatt [SECONDS]`
//! scans for SECONDS (5 if 0), connects to the strongest connectable
//! advertiser (ADV_IND), lists its services and their characteristics,
//! reads the readable ones (the first 12), subscribes to the first one
//! that notifies or indicates for 3 s (writing only its CCCD), and
//! disconnects. Built with
//! `--permit svc:ble`. Exit 0, or the step that failed.

use t5_rt::{Channel, ble, event};

t5_rt::entry!(main);

const READS_MAX: usize = 12;
/// ADV_IND: connectable, undirected.
const ADV_IND: u8 = 0;
const PROP_READ: u8 = 0x02;
const PROP_NOTIFY: u8 = 0x10;
const PROP_INDICATE: u8 = 0x20;
const LISTEN_NS: u64 = 3_000_000_000;

fn main(seconds: usize, _: usize) -> usize {
    let Ok(c) = Channel::connect(ble::SVC) else {
        event!(Error, "blegatt.no_service");
        return 1;
    };
    let seconds = if seconds == 0 { 5 } else { seconds.min(60) } as u8;
    let mut buf = [0u8; ble::MSG_MAX];

    // The strongest connectable advertiser.
    let mut req = [ble::SCAN, seconds, 0, 0, 0, 0, 0];
    req[3..5].copy_from_slice(&100u16.to_le_bytes());
    req[5..7].copy_from_slice(&20u16.to_le_bytes());
    if c.send(&req).is_err() {
        return 2;
    }
    let mut best: Option<([u8; 7], i8)> = None;
    loop {
        let Ok(n) = c.recv(&mut buf) else { return 2 };
        match buf[0] {
            ble::ADV if n >= 11 && buf[8] == ADV_IND => {
                let rssi = buf[9] as i8;
                if best.is_none_or(|(_, r)| rssi > r) {
                    let mut a = [0u8; 7];
                    a.copy_from_slice(&buf[1..8]);
                    best = Some((a, rssi));
                }
            }
            ble::DONE => break,
            ble::FAILED => return failed(&buf[..n], 2),
            _ => {}
        }
    }
    let Some((target, rssi)) = best else {
        event!(Warn, "blegatt.nothing_connectable");
        return 3;
    };
    event!(Info, "blegatt.target", addr = &target[1..7], addr_type = target[0], rssi = rssi as i32);

    let mut req = [0u8; 9];
    req[0] = ble::CONNECT;
    req[1..8].copy_from_slice(&target);
    req[8] = 10;
    if c.send(&req).is_err() {
        return 4;
    }
    let Ok(n) = c.recv(&mut buf) else { return 4 };
    if buf[0] != ble::CONNECTED {
        return failed(&buf[..n], 4);
    }
    event!(Info, "blegatt.connected", att_mtu = u16::from_le_bytes([buf[1], buf[2]]) as u32);

    // Services, then each one's characteristics.
    let mut services = [(0u16, 0u16); 16];
    let mut count = 0;
    if c.send(&[ble::SERVICES]).is_err() {
        return 5;
    }
    loop {
        let Ok(n) = c.recv(&mut buf) else { return 5 };
        match buf[0] {
            ble::SERVICE if n >= 8 => {
                let (start, end) = (u16::from_le_bytes([buf[2], buf[3]]), u16::from_le_bytes([buf[4], buf[5]]));
                event!(Info, "blegatt.service", index = buf[1], start = start as u32, end = end as u32, uuid = &buf[6..n]);
                if count < services.len() {
                    services[count] = (start, end);
                    count += 1;
                }
            }
            ble::END => break,
            _ => return failed(&buf[..n], 5),
        }
    }
    let mut notifier: Option<(u16, bool)> = None;
    let mut readable = [0u16; READS_MAX];
    let mut reads = 0;
    for i in 0..count {
        if c.send(&[ble::CHARS, i as u8]).is_err() {
            return 6;
        }
        loop {
            let Ok(n) = c.recv(&mut buf) else { return 6 };
            match buf[0] {
                ble::CHAR if n >= 8 => {
                    let handle = u16::from_le_bytes([buf[1], buf[2]]);
                    let cccd = u16::from_le_bytes([buf[3], buf[4]]);
                    event!(Info, "blegatt.char", service = i as u32, handle = handle as u32, cccd = cccd as u32, props = buf[5], uuid = &buf[6..n]);
                    if notifier.is_none() && cccd != 0 && buf[5] & (PROP_NOTIFY | PROP_INDICATE) != 0 {
                        notifier = Some((handle, buf[5] & PROP_NOTIFY == 0));
                    }
                    if buf[5] & PROP_READ != 0 && reads < READS_MAX {
                        readable[reads] = handle;
                        reads += 1;
                    }
                }
                ble::END => break,
                _ => {
                    failed(&buf[..n], 6);
                    break;
                }
            }
        }
    }
    for &handle in &readable[..reads] {
        let mut req = [ble::READ, 0, 0];
        req[1..3].copy_from_slice(&handle.to_le_bytes());
        if c.send(&req).is_err() {
            return 7;
        }
        let Ok(n) = c.recv(&mut buf) else { return 7 };
        if buf[0] == ble::VALUE {
            let text = core::str::from_utf8(&buf[3..n]).ok().filter(|t| t.bytes().all(|b| (0x20..0x7F).contains(&b)));
            event!(Info, "blegatt.value", handle = handle as u32, data = &buf[3..n.min(3 + 32)], text = text.unwrap_or(""));
        } else {
            failed(&buf[..n], 7);
        }
    }
    let mut notes = 0u32;
    if let Some((handle, indicate)) = notifier {
        let mut req = [ble::SUBSCRIBE, 0, 0, indicate as u8];
        req[1..3].copy_from_slice(&handle.to_le_bytes());
        if c.send(&req).is_err() {
            return 8;
        }
        let Ok(n) = c.recv(&mut buf) else { return 8 };
        if buf[0] != ble::SUBSCRIBED {
            return failed(&buf[..n], 8);
        }
        event!(Info, "blegatt.subscribed", handle = handle as u32, indicate = indicate);
        let end = t5_rt::time_ns() + LISTEN_NS;
        while t5_rt::time_ns() < end {
            let mut b = [0u8; ble::MSG_MAX];
            match c.0.read(&mut b) {
                Ok(n) if b[0] == ble::NOTIFY => {
                    notes += 1;
                    if notes <= 4 {
                        event!(Info, "blegatt.notify", handle = u16::from_le_bytes([b[1], b[2]]) as u32, indication = b[3], data = &b[4..n.min(4 + 32)]);
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = t5_rt::wait(&[&c.0], end.saturating_sub(t5_rt::time_ns()));
                }
            }
        }
    }
    let _ = c.send(&[ble::DISCONNECT]);
    // Notifications may still be queued ahead of the answer.
    let n = loop {
        let Ok(n) = c.recv(&mut buf) else { return 9 };
        if buf[0] != ble::NOTIFY {
            break n;
        }
    };
    event!(Info, "blegatt.done", services = count as u32, reads = reads as u32, notifications = notes, disconnected = buf[..n].first() == Some(&ble::DISCONNECTED));
    0
}

fn failed(m: &[u8], step: usize) -> usize {
    event!(Error, "blegatt.failed", step = step as u32, got = m.first().copied().unwrap_or(0), request = m.get(1).copied().unwrap_or(0), code = m.get(2).copied().unwrap_or(0));
    step
}
