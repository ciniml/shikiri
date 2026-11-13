// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The tailnet's IP inside the node (doc/71 form A): smoltcp on a device
//! of two queues. WireGuard's decrypted packets go into `rx`; what smoltcp
//! sends comes out of `tx` for the node to encrypt to the peer that has
//! the destination address. No link layer (smoltcp's `Medium::Ip`).

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address};

/// Tailscale's MTU on the tailnet.
pub const MTU: usize = 1280;
/// Packets smoltcp may queue before the node takes them.
const TX_MAX: usize = 64;
/// The tailnet's addresses (CGNAT, 100.64.0.0/10): every peer is on the
/// link, so smoltcp needs no route.
const PREFIX: u8 = 10;

#[derive(Default)]
pub struct Queues {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: VecDeque<Vec<u8>>,
}

pub struct Rx(Vec<u8>);
pub struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0; len];
        let r = f(&mut b);
        self.0.push_back(b);
        r
    }
}

impl Device for Queues {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        let p = self.rx.pop_front()?;
        Some((Rx(p), Tx(&mut self.tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (self.tx.len() < TX_MAX).then(|| Tx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

pub struct Tun {
    pub dev: Queues,
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    /// The address set, if any.
    pub ip: Option<[u8; 4]>,
}

fn at(ms: u64) -> Instant {
    Instant::from_millis(ms as i64)
}

impl Tun {
    /// `seed`: randomness for smoltcp (TCP's initial sequence numbers,
    /// ports).
    pub fn new(seed: u64, now_ms: u64) -> Tun {
        let mut dev = Queues::default();
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = seed;
        let iface = Interface::new(config, &mut dev, at(now_ms));
        Tun { dev, iface, sockets: SocketSet::new(Vec::new()), ip: None }
    }

    /// This node's tailnet address.
    pub fn set_address(&mut self, ip: [u8; 4]) {
        if self.ip == Some(ip) {
            return;
        }
        self.ip = Some(ip);
        let a = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            let _ = addrs.push(IpCidr::new(a, PREFIX));
        });
    }

    /// Lets smoltcp take the received packets and act on its timers;
    /// what it sends lands in `dev.tx`.
    pub fn poll(&mut self, now_ms: u64) {
        self.iface.poll(at(now_ms), &mut self.dev, &mut self.sockets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::socket::tcp;

    fn sum(data: &[u8], mut s: u32) -> u16 {
        for c in data.chunks(2) {
            s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
        }
        while s > 0xFFFF {
            s = (s & 0xFFFF) + (s >> 16);
        }
        !(s as u16)
    }

    /// An IPv4 packet from 100.64.0.2 to 100.64.0.1 with `body`.
    fn ipv4(proto: u8, mut body: Vec<u8>, csum_at: usize) -> Vec<u8> {
        let (src, dst) = ([100, 64, 0, 2], [100, 64, 0, 1]);
        let mut h = vec![0x45, 0, 0, 0, 0, 1, 0x40, 0, 64, proto, 0, 0];
        h[2..4].copy_from_slice(&((20 + body.len()) as u16).to_be_bytes());
        h.extend_from_slice(&src);
        h.extend_from_slice(&dst);
        let c = sum(&h, 0);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        let pseudo = if proto == 6 { [&src[..], &dst, &[0, 6], &(body.len() as u16).to_be_bytes()].concat() } else { Vec::new() };
        let c = sum(&[pseudo, body.clone()].concat(), 0);
        body[csum_at..csum_at + 2].copy_from_slice(&c.to_be_bytes());
        h.extend_from_slice(&body);
        h
    }

    #[test]
    fn ping_and_syn() {
        let mut t = Tun::new(1, 0);
        t.set_address([100, 64, 0, 1]);
        let s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 1024]), tcp::SocketBuffer::new(vec![0; 1024]));
        let h = t.sockets.add(s);
        t.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();

        t.dev.rx.push_back(ipv4(1, vec![8, 0, 0, 0, 0, 1, 0, 1, b'h', b'i'], 2));
        t.poll(1);
        let r = t.dev.tx.pop_front().unwrap();
        assert_eq!((r[9], r[20]), (1, 0));
        assert_eq!(&r[16..20], &[100, 64, 0, 2]);

        // A SYN to port 80 from port 40000.
        let mut syn = vec![0x9C, 0x40, 0, 80, 0, 0, 0, 1, 0, 0, 0, 0, 0x50, 0x02, 0xFF, 0xFF, 0, 0, 0, 0];
        syn[16..18].fill(0);
        t.dev.rx.push_back(ipv4(6, syn, 16));
        t.poll(2);
        let r = t.dev.tx.pop_front().unwrap();
        assert_eq!(r[9], 6);
        // SYN and ACK, to port 40000.
        assert_eq!(&r[22..24], &[0x9C, 0x40]);
        assert_eq!(r[33] & 0x12, 0x12);
        assert_eq!(t.sockets.get::<tcp::Socket>(h).state(), tcp::State::SynReceived);
    }
}
