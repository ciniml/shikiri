// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The host link (N2a, doc/21): IP packets between the network service and
//! a PC, on the console line (USB Serial/JTAG on the Tab5) next to the log.
//! Both directions use the log's framing (COBS, 0x00 between frames); a
//! packet frame starts with `PACKET`, a byte no log record starts with (its
//! level is at most 4). On the PC, `t5log hostlink` passes packets to a TUN
//! device and prints the log frames.
//!
//! The service opens `netdev:host`: `read` takes one received packet,
//! `write` sends one. Received packets wait in a small fixed queue; the
//! line is read from the scheduler loop (`poll`).
//!
//! Other frames are control commands (doc/54), as on the RTT down-channel
//! (doc/14 §3): the PC can control the kernel with the USB cable alone.
//! The line is read for them whether or not the link is open; while
//! commands wait, it is not read, so the PC's writes wait in USB.

use t5_abi::log as wire;
use t5_abi::{Error, event};

use crate::platform::{Current, Platform};

/// First byte of a packet frame (after COBS decoding).
pub const PACKET: u8 = 0xFF;
pub const MTU: usize = 1500;
const QUEUE: usize = 8;
/// Command frames waiting for the service loop.
const COMMANDS: usize = 4;
/// A whole encoded frame: marker, packet, COBS overhead, terminator.
const FRAME_MAX: usize = MTU + 1 + MTU / 254 + 2;

struct Link {
    /// Bytes of the frame being received (still encoded).
    raw: [u8; FRAME_MAX],
    raw_len: usize,
    /// The current frame is too long: skip to its end.
    skipping: bool,
    packets: [[u8; MTU]; QUEUE],
    lens: [usize; QUEUE],
    head: usize,
    count: usize,
    open: bool,
    received: u32,
    dropped: u32,
    /// Bytes read from the line, and frames that were not packets.
    bytes: u32,
    other: u32,
    commands: [[u8; t5_abi::ctl::MAX_COMMAND]; COMMANDS],
    command_lens: [usize; COMMANDS],
    command_head: usize,
    command_count: usize,
    /// Packets that came while the link was closed.
    closed: u32,
    /// Scratch for decoding and encoding (static: the kernel stack is small).
    decoded: [u8; MTU + 1],
    plain: [u8; MTU + 1],
    out: [u8; FRAME_MAX],
}

static mut LINK: Link = Link {
    raw: [0; FRAME_MAX],
    raw_len: 0,
    skipping: false,
    packets: [[0; MTU]; QUEUE],
    lens: [0; QUEUE],
    head: 0,
    count: 0,
    open: false,
    received: 0,
    dropped: 0,
    bytes: 0,
    other: 0,
    commands: [[0; t5_abi::ctl::MAX_COMMAND]; COMMANDS],
    command_lens: [0; COMMANDS],
    command_head: 0,
    command_count: 0,
    closed: 0,
    decoded: [0; MTU + 1],
    plain: [0; MTU + 1],
    out: [0; FRAME_MAX],
};

fn link() -> &'static mut Link {
    unsafe { &mut *core::ptr::addr_of_mut!(LINK) }
}

/// Whether bytes wait on the line for `poll` (the scheduler does not sleep
/// then: `poll` reads a bounded amount per call).
pub fn pending() -> bool {
    link().open && Current::console_pending()
}

/// Reads what the line has (from the scheduler loop).
pub fn poll() {
    let l = link();
    let mut buf = [0u8; 64];
    // A bounded amount per call, so a flood cannot stall the loop; nothing
    // while commands wait (one USB packet holds at most a few).
    for _ in 0..32 {
        if l.command_count > 0 {
            break;
        }
        let n = Current::console_read(&mut buf);
        if n == 0 {
            break;
        }
        if l.bytes == 0 && l.open {
            event!(Info, "hostlink.first_bytes", n = n as u32, b0 = buf[0]);
        }
        l.bytes = l.bytes.wrapping_add(n as u32);
        for &b in &buf[..n] {
            if b != 0 {
                if l.raw_len < FRAME_MAX {
                    l.raw[l.raw_len] = b;
                    l.raw_len += 1;
                } else {
                    l.skipping = true;
                }
                continue;
            }
            // End of a frame.
            if !l.skipping && l.raw_len > 0 {
                frame_in(l);
            }
            l.raw_len = 0;
            l.skipping = false;
        }
    }
}

fn frame_in(l: &mut Link) {
    let Some(n) = wire::cobs_decode(&l.raw[..l.raw_len], &mut l.decoded) else {
        l.dropped += 1;
        return;
    };
    if n == 0 {
        return;
    }
    if l.decoded[0] != PACKET {
        if l.command_count == COMMANDS || n > t5_abi::ctl::MAX_COMMAND {
            l.other += 1;
            event!(Warn, "ctl.console_dropped", len = n as u32);
            return;
        }
        let i = (l.command_head + l.command_count) % COMMANDS;
        l.commands[i][..n].copy_from_slice(&l.decoded[..n]);
        l.command_lens[i] = n;
        l.command_count += 1;
        return;
    }
    if n < 2 {
        l.other += 1;
        return;
    }
    if !l.open {
        l.closed += 1;
        return;
    }
    let decoded = &l.decoded;
    if l.count == QUEUE {
        l.dropped += 1;
        return;
    }
    let i = (l.head + l.count) % QUEUE;
    l.packets[i][..n - 1].copy_from_slice(&decoded[1..n]);
    l.lens[i] = n - 1;
    l.count += 1;
    l.received += 1;
    if l.received == 1 {
        event!(Info, "hostlink.first_packet", len = (n - 1) as u32);
    }
}

/// The next command frame from the line, decoded from COBS into `out`.
pub fn read_command(out: &mut [u8; t5_abi::ctl::MAX_COMMAND]) -> Option<usize> {
    let l = link();
    if l.command_count == 0 {
        return None;
    }
    let n = l.command_lens[l.command_head];
    out[..n].copy_from_slice(&l.commands[l.command_head][..n]);
    l.command_head = (l.command_head + 1) % COMMANDS;
    l.command_count -= 1;
    Some(n)
}

/// The handle on `netdev:host`; one at a time.
pub struct NetDev;

impl NetDev {
    pub fn open() -> Result<NetDev, Error> {
        let l = link();
        if l.open {
            return Err(Error::Busy);
        }
        l.open = true;
        l.raw_len = 0;
        l.count = 0;
        Current::console_wake(true);
        event!(Info, "hostlink.open", mtu = MTU as u32);
        Ok(NetDev)
    }

    /// One received packet into `out` (`Busy` if none, `Size` if too small:
    /// the packet stays).
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let l = link();
        if l.count == 0 {
            return Err(Error::Busy);
        }
        let n = l.lens[l.head];
        if n > out.len() {
            return Err(Error::Size);
        }
        out[..n].copy_from_slice(&l.packets[l.head][..n]);
        l.head = (l.head + 1) % QUEUE;
        l.count -= 1;
        Ok(n)
    }

    /// Sends one packet to the PC.
    pub fn write(&mut self, packet: &[u8]) -> Result<usize, Error> {
        if packet.is_empty() || packet.len() > MTU {
            return Err(Error::Size);
        }
        let l = link();
        l.plain[0] = PACKET;
        l.plain[1..=packet.len()].copy_from_slice(packet);
        let n = wire::cobs_encode(&l.plain[..=packet.len()], &mut l.out).ok_or(Error::Size)?;
        if Current::console_write(&l.out[..n]) { Ok(packet.len()) } else { Err(Error::Busy) }
    }

    /// The `wait` event: READABLE with the packets waiting.
    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = link().count;
        (n > 0).then_some((t5_abi::netdev::READABLE, n))
    }
}

impl Drop for NetDev {
    fn drop(&mut self) {
        let l = link();
        l.open = false;
        Current::console_wake(false);
        event!(Info, "hostlink.closed", received = l.received, dropped = l.dropped, bytes = l.bytes, other = l.other);
    }
}
