#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! Talks to the Bluetooth controller on the ESP32-C6 through `bt:hci`
//! (doc/48 M0/M1): HCI_Reset, Read_Local_Version_Information and
//! Read_BD_ADDR, then, if `run hcitest SECONDS`, an active LE scan for
//! SECONDS that logs the first advertisers it hears. Needs the controller
//! started (`wifi 1`). Built with `--permit bt:hci`. Exit 0, or the step
//! that failed.

use t5_rt::{Error, Handle, bt, event, rights, sleep_ms, time_ns, wait};

t5_rt::entry!(main);

const TIMEOUT_NS: u64 = 2_000_000_000;
/// Events: Command Complete, Command Status, LE Meta.
const EVT_COMMAND_COMPLETE: u8 = 0x0E;
const EVT_COMMAND_STATUS: u8 = 0x0F;
const EVT_LE_META: u8 = 0x3E;
const LE_ADVERTISING_REPORT: u8 = 0x02;
/// The advertisers logged, at most.
const LOG_ADVERTISERS: usize = 12;

fn main(seconds: usize, _: usize) -> usize {
    let Ok(h) = Handle::open(bt::HCI, rights::READ | rights::WRITE | rights::WAIT) else {
        event!(Error, "hcitest.no_hci");
        return 1;
    };
    let mut buf = [0u8; bt::PKT_MAX];
    let Some(_) = command(&h, &mut buf, 0x0C03, &[]) else { return 2 };
    event!(Info, "hcitest.reset");
    let Some(p) = command(&h, &mut buf, 0x1001, &[]) else { return 3 };
    // Status, HCI version, revision, LMP version, manufacturer, subversion.
    if p.len() >= 9 {
        let u16_at = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]) as u32;
        event!(Info, "hcitest.version", hci = p[1], hci_rev = u16_at(2), lmp = p[4], manufacturer = u16_at(5), lmp_subver = u16_at(7));
    }
    let Some(p) = command(&h, &mut buf, 0x1009, &[]) else { return 4 };
    if p.len() >= 7 {
        let mut addr = [0u8; 6];
        for (i, b) in p[1..7].iter().rev().enumerate() {
            addr[i] = *b;
        }
        event!(Info, "hcitest.bd_addr", addr = &addr[..]);
    }
    if seconds == 0 {
        event!(Info, "hcitest.done", advertisers = 0u32, reports = 0u32);
        return 0;
    }
    scan(&h, &mut buf, seconds as u64)
}

/// Active LE scan for `seconds`; logs the first advertisers.
fn scan(h: &Handle, buf: &mut [u8; bt::PKT_MAX], seconds: u64) -> usize {
    // The default event mask leaves LE Meta (bit 61) off.
    let mask = (0x0000_1FFF_FFFF_FFFFu64 | 1 << 61).to_le_bytes();
    if command(h, buf, 0x0C01, &mask).is_none() {
        return 5;
    }
    // Active, 30 ms interval and window, public address, no filter.
    if command(h, buf, 0x200B, &[1, 0x30, 0, 0x30, 0, 0, 0]).is_none() {
        return 6;
    }
    // Enable, duplicates filtered.
    if command(h, buf, 0x200C, &[1, 1]).is_none() {
        return 7;
    }
    let end = time_ns() + seconds * 1_000_000_000;
    let mut seen = [[0u8; 6]; LOG_ADVERTISERS];
    let (mut advertisers, mut reports) = (0usize, 0u32);
    while time_ns() < end {
        let Ok(n) = next(h, buf, end - time_ns()) else { continue };
        let p = &buf[..n];
        // [EVT, LE Meta, len, Advertising Report, count, type, addr type, addr[6], data len, data, rssi]
        if n < 14 || p[0] != bt::EVT || p[1] != EVT_LE_META || p[3] != LE_ADVERTISING_REPORT {
            continue;
        }
        reports += 1;
        let mut addr = [0u8; 6];
        for (i, b) in p[7..13].iter().rev().enumerate() {
            addr[i] = *b;
        }
        if advertisers < LOG_ADVERTISERS && !seen[..advertisers].contains(&addr) {
            seen[advertisers] = addr;
            advertisers += 1;
            let data_len = p[13] as usize;
            let rssi = p.get(14 + data_len).map_or(0, |&r| r as i8 as i32);
            event!(Info, "hcitest.adv", addr = &addr[..], addr_type = p[6], kind = p[5], rssi = rssi, data_len = data_len as u32);
        }
    }
    let _ = command(h, buf, 0x200C, &[0, 0]);
    event!(Info, "hcitest.done", advertisers = advertisers as u32, reports = reports);
    0
}

/// Sends a command and waits for its Command Complete (or a failed
/// Command Status); returns the return parameters (status first), left in
/// `buf`.
fn command<'a>(h: &Handle, buf: &'a mut [u8; bt::PKT_MAX], opcode: u16, params: &[u8]) -> Option<&'a [u8]> {
    let mut pkt = [0u8; 4 + 255];
    pkt[0] = bt::CMD;
    pkt[1..3].copy_from_slice(&opcode.to_le_bytes());
    pkt[3] = params.len() as u8;
    pkt[4..4 + params.len()].copy_from_slice(params);
    loop {
        match h.write(&pkt[..4 + params.len()]) {
            Err(Error::Busy) => sleep_ms(1),
            Err(e) => {
                event!(Error, "hcitest.write_failed", opcode = opcode as u32, error = e as u32);
                return None;
            }
            Ok(_) => break,
        }
    }
    let end = time_ns() + TIMEOUT_NS;
    while time_ns() < end {
        let Ok(n) = next(h, buf, end - time_ns()) else { continue };
        let p = &buf[..n];
        if n >= 6 && p[0] == bt::EVT && p[1] == EVT_COMMAND_COMPLETE && u16::from_le_bytes([p[4], p[5]]) == opcode {
            let status = p.get(6).copied().unwrap_or(0xFF);
            if status != 0 {
                event!(Error, "hcitest.command_failed", opcode = opcode as u32, status = status);
                return None;
            }
            return Some(&buf[6..n]);
        }
        if n >= 7 && p[0] == bt::EVT && p[1] == EVT_COMMAND_STATUS && u16::from_le_bytes([p[5], p[6]]) == opcode && p[3] != 0 {
            event!(Error, "hcitest.command_failed", opcode = opcode as u32, status = p[3]);
            return None;
        }
        event!(Debug, "hcitest.other", head = &p[..n.min(16)]);
    }
    event!(Error, "hcitest.no_answer", opcode = opcode as u32);
    None
}

/// The next packet from the controller, waiting up to `timeout_ns`.
fn next(h: &Handle, buf: &mut [u8], timeout_ns: u64) -> Result<usize, Error> {
    match h.read(buf) {
        Err(Error::Busy) => {
            wait(&[h], timeout_ns)?;
            h.read(buf)
        }
        r => r,
    }
}
