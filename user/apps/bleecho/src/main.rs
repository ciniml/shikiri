#![no_std]
#![no_main]

//! The BLE service's peripheral role (doc/48 M4): `run bleecho [SECONDS]`
//! advertises "T5-echo" with one service (UUID c5d7a0a1-5b3e-4c2d-9f1e-
//! 1a2b3c4d5e60): C1 (…5e61, write) and C2 (…5e62, read and notify).
//! What a central writes to C1 becomes C2's value and is notified back;
//! while it has subscribed to C2, "tick N" is also notified every second.
//! When the central leaves, it advertises again (doc/48 §9: counting
//! connections). `run bleecho [SECONDS [INTERVAL_MS]]` (100 ms if 0).
//! Until SECONDS (60 if 0) pass. Built with
//! `--permit svc:ble`. Exit 0, or the step that failed.

use t5_rt::{Channel, ble, event, time_ns, wait};

t5_rt::entry!(main);

const NAME: &[u8] = b"T5-echo";
/// The UUIDs, little-endian; the last byte (first in the string form)
/// differs per characteristic.
const fn uuid(last: u8) -> [u8; 16] {
    [last, 0x5e, 0x4d, 0x3c, 0x2b, 0x1a, 0x1e, 0x9f, 0x2d, 0x4c, 0x3e, 0x5b, 0xa1, 0xa0, 0xd7, 0xc5]
}
const PROP_READ: u8 = 0x02;
const PROP_WRITE_NO_RESPONSE: u8 = 0x04;
const PROP_WRITE: u8 = 0x08;
const PROP_NOTIFY: u8 = 0x10;

fn main(seconds: usize, interval: usize) -> usize {
    let Ok(c) = Channel::connect(ble::SVC) else {
        event!(Error, "bleecho.no_service");
        return 1;
    };
    let seconds = if seconds == 0 { 60 } else { seconds } as u64;
    let end = time_ns() + seconds * 1_000_000_000;

    // ADVERTISE: 100 ms, the name, the service, C1 and C2, then the data:
    // flags and the service's UUID; the name in the scan response.
    let mut req = [0u8; ble::MSG_MAX];
    let mut n = 0;
    let mut put = |b: &[u8]| {
        req[n..n + b.len()].copy_from_slice(b);
        n += b.len();
    };
    put(&[ble::ADVERTISE]);
    put(&(if interval == 0 { 100 } else { interval.min(10_240) } as u16).to_le_bytes());
    put(&[NAME.len() as u8]);
    put(NAME);
    put(&[16]);
    put(&uuid(0x60));
    put(&[2, 16]);
    put(&uuid(0x61));
    put(&[PROP_WRITE | PROP_WRITE_NO_RESPONSE, 16]);
    put(&uuid(0x62));
    put(&[PROP_READ | PROP_NOTIFY]);
    put(&[3 + 18, 2, 0x01, 0x06, 17, 0x07]);
    put(&uuid(0x60));
    put(&[2 + NAME.len() as u8, 1 + NAME.len() as u8, 0x09]);
    put(NAME);
    let adv = &req[..n];
    if c.send(adv).is_err() {
        return 2;
    }
    let mut buf = [0u8; ble::MSG_MAX];
    let Ok(m) = c.recv(&mut buf) else { return 2 };
    if buf[0] != ble::ADVERTISING || m < 10 {
        event!(Error, "bleecho.failed", got = buf[0], request = buf[1], code = buf[2]);
        return 2;
    }
    let c1 = u16::from_le_bytes([buf[2], buf[3]]);
    let c2 = u16::from_le_bytes([buf[6], buf[7]]);
    let c2_cccd = u16::from_le_bytes([buf[8], buf[9]]);
    event!(Info, "bleecho.advertising", c1 = c1 as u32, c2 = c2 as u32, c2_cccd = c2_cccd as u32);

    let (mut writes, mut sent, mut peers) = (0u32, 0u32, 0u32);
    let mut waiting = 0u32;
    let (mut subscribed, mut ticks, mut next_tick) = (false, 0u32, 0u64);
    loop {
        let now = time_ns();
        if now >= end {
            let _ = c.send(&[ble::STOP]);
            break;
        }
        if subscribed && now >= next_tick {
            next_tick = now + 1_000_000_000;
            ticks += 1;
            let mut out = [0u8; 16];
            out[0] = ble::SEND;
            out[1..3].copy_from_slice(&c2.to_le_bytes());
            let text = b"tick ";
            out[4..9].copy_from_slice(text);
            out[9] = b'0' + (ticks % 10) as u8;
            if c.send(&out[..10]).is_err() {
                return 3;
            }
            waiting += 1;
        }
        let n = match c.0.read(&mut buf) {
            Ok(n) => n,
            Err(_) => {
                let until = if subscribed { next_tick.min(end) } else { end };
                let _ = wait(&[&c.0], until.saturating_sub(now));
                continue;
            }
        };
        match buf[0] {
            ble::PEER => {
                peers += 1;
                event!(Info, "bleecho.peer", addr = &buf[2..8], addr_type = buf[1], count = peers);
            }
            ble::PEER_WRITE if n >= 3 => {
                let handle = u16::from_le_bytes([buf[1], buf[2]]);
                event!(Info, "bleecho.write", handle = handle as u32, data = &buf[3..n.min(3 + 32)]);
                if handle == c2_cccd && n >= 4 {
                    subscribed = buf[3] & 1 != 0;
                    next_tick = time_ns() + 1_000_000_000;
                }
                if handle != c1 {
                    continue;
                }
                writes += 1;
                let mut out = [0u8; ble::MSG_MAX];
                out[0] = ble::SEND;
                out[1..3].copy_from_slice(&c2.to_le_bytes());
                out[3] = 0;
                out[4..4 + n - 3].copy_from_slice(&buf[3..n]);
                if c.send(&out[..4 + n - 3]).is_err() {
                    return 3;
                }
                waiting += 1;
            }
            ble::SENT => {
                sent += 1;
                waiting = waiting.saturating_sub(1);
            }
            ble::FAILED => {
                event!(Warn, "bleecho.failed", got = buf[0], request = buf[1], code = buf[2]);
                waiting = waiting.saturating_sub(1);
            }
            ble::DISCONNECTED => {
                // Again, for the next central (the handles stay the same).
                subscribed = false;
                if c.send(adv).is_err() {
                    return 4;
                }
            }
            _ => {}
        }
    }
    event!(Info, "bleecho.done", peers = peers, writes = writes, ticks = ticks, sent = sent);
    0
}
