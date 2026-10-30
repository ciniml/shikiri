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

use t5_abi::log as wire;
use t5_abi::{Error, event};

use crate::platform::{Current, Platform};

/// First byte of a packet frame (after COBS decoding).
pub const PACKET: u8 = 0xFF;
pub const MTU: usize = 1500;
const QUEUE: usize = 8;
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
};

fn link() -> &'static mut Link {
    unsafe { &mut *core::ptr::addr_of_mut!(LINK) }
}

/// Reads what the line has (from the scheduler loop). Only while the
/// service has the link open: otherwise the bytes are left to the hardware.
pub fn poll() {
    let l = link();
    if !l.open {
        return;
    }
    let mut buf = [0u8; 64];
    // A bounded amount per call, so a flood cannot stall the loop.
    for _ in 0..32 {
        let n = Current::console_read(&mut buf);
        if n == 0 {
            break;
        }
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
    let mut decoded = [0u8; MTU + 1];
    let Some(n) = wire::cobs_decode(&l.raw[..l.raw_len], &mut decoded) else {
        l.dropped += 1;
        return;
    };
    if n < 2 || decoded[0] != PACKET {
        return; // not ours (nothing else is sent to the Tab5 on this line yet)
    }
    if l.count == QUEUE {
        l.dropped += 1;
        return;
    }
    let i = (l.head + l.count) % QUEUE;
    l.packets[i][..n - 1].copy_from_slice(&decoded[1..n]);
    l.lens[i] = n - 1;
    l.count += 1;
    l.received += 1;
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
        let mut plain = [0u8; MTU + 1];
        plain[0] = PACKET;
        plain[1..=packet.len()].copy_from_slice(packet);
        let mut frame = [0u8; FRAME_MAX];
        let n = wire::cobs_encode(&plain[..=packet.len()], &mut frame).ok_or(Error::Size)?;
        if Current::console_write(&frame[..n]) { Ok(packet.len()) } else { Err(Error::Busy) }
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
        event!(Info, "hostlink.closed", received = l.received, dropped = l.dropped);
    }
}
