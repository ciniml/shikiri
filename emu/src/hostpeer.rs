// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The PC end of the host link (doc/21) inside the emulator: a smoltcp
//! interface at 10.55.0.1/24 whose packets travel as frames on the USJ
//! console, as `t5log hostlink` sends them. Once started it pings the
//! target (the Tab5, 10.55.0.2, by default) and then sends data to the TCP
//! echo server on port 7777 (`nettest 1`) and checks that the same bytes
//! come back.
//!
//! A target elsewhere is reached through the Tab5 (default route via
//! 10.55.0.2): the forwarding test (doc/23 §6) against the test LAN's peer
//! (`run net 1`, 192.168.50.2). It then also checks the Tab5's ICMP errors:
//! Time Exceeded for a ping with TTL 1, Destination Unreachable for a ping
//! to a network without a route.
//!
//! The PC also has fd55::1/64 (doc/26): with an IPv6 target (fd55::2) the
//! pings are ICMPv6 and the TCP echo runs over IPv6.
//!
//! With `serve_http`, it also answers HTTP/1.0 `GET`s on port 8080 with the
//! files of a directory (`--http-dir`, doc/67): the update service's
//! downloads.

use std::collections::VecDeque;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{icmp, raw, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, Icmpv4Packet, Icmpv4Repr, Icmpv6Packet, Icmpv6Repr, IpAddress, IpCidr, IpProtocol, IpVersion, Ipv4Address, Ipv6Address};

use crate::periph::{PACKET, Usj};

const PEER: IpAddress = IpAddress::v4(10, 55, 0, 1);
const TAB5: IpAddress = IpAddress::v4(10, 55, 0, 2);
/// Has no route on the Tab5 (forwarding test).
const NOWHERE: IpAddress = IpAddress::v4(192, 168, 99, 1);
const ICMP_TIME_EXCEEDED: u8 = 11;
const ICMP_DEST_UNREACHABLE: u8 = 3;
/// The Tab5 on the host link over IPv6, an address no route leads to, and
/// the ICMPv6 errors (doc/46).
const TAB5_V6: IpAddress = IpAddress::v6(0xfd55, 0, 0, 0, 0, 0, 0, 2);
const NOWHERE_V6: IpAddress = IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
const ICMPV6_TIME_EXCEEDED: u8 = 3;
const ICMPV6_DEST_UNREACHABLE: u8 = 1;
const ECHO_PORT: u16 = 7777;
const MTU: usize = 1500;
const PINGS: u16 = 3;
const IDENT: u16 = 0x7435;
/// One USB packet of the OUT endpoint (full speed bulk).
const USB_PACKET: usize = 64;
/// No answer for this long ends the test as failed.
const TIMEOUT_MS: i64 = 5_000;

/// Packets between the interface and the console line.
#[derive(Default)]
struct Queues {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct Rx(Vec<u8>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
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
        Some(Tx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Idle,
    Ping { seq: u16, sent_at: i64 },
    Tcp,
    /// An ICMP error of `kind` from the Tab5 is due for the ping sent at
    /// `sent_at` (to the target with TTL 1, or to `NOWHERE`).
    IcmpError { kind: u8, sent_at: i64 },
    Passed,
    Failed,
}

pub struct HostPeer {
    dev: Queues,
    iface: Interface,
    sockets: SocketSet<'static>,
    icmp: SocketHandle,
    tcp: SocketHandle,
    /// Every ICMP and ICMPv6 packet that arrives (for the errors).
    raw: SocketHandle,
    raw6: SocketHandle,
    target: IpAddress,
    step: Step,
    since: i64,
    /// What goes to the echo server, and how much was sent and checked.
    data: Vec<u8>,
    sent: usize,
    checked: usize,
    pub ping_ms: Vec<f64>,
    pub tcp_ms: i64,
    /// ICMP errors seen (forwarding test).
    pub icmp_errors: Vec<u8>,
    /// Sources of the echo requests that came while idle (NAT test).
    pub pinged_by: std::collections::BTreeSet<IpAddress>,
    pub why: String,
    http: Option<Http>,
    /// Files served (path, bytes).
    pub served: Vec<(String, usize)>,
}

const HTTP_PORT: u16 = 8080;

/// The HTTP file server (`serve_http`).
struct Http {
    dir: std::path::PathBuf,
    socket: SocketHandle,
    request: Vec<u8>,
    /// The answer being sent, and how much of it went.
    answer: Vec<u8>,
    sent: usize,
}

impl HostPeer {
    /// `bytes`: how much to send to the echo server at `target`.
    pub fn new(bytes: usize, target: IpAddress) -> HostPeer {
        let mut dev = Queues::default();
        let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut dev, Instant::ZERO);
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(PEER, 24)).unwrap();
            a.push(IpCidr::new(Ipv6Address::new(0xfd55, 0, 0, 0, 0, 0, 0, 1).into(), 64)).unwrap();
        });
        iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 55, 0, 2)).unwrap();
        iface.routes_mut().add_default_ipv6_route(Ipv6Address::new(0xfd55, 0, 0, 0, 0, 0, 0, 2)).unwrap();
        let mut sockets = SocketSet::new(Vec::new());
        let icmp = sockets.add(icmp::Socket::new(
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]),
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]),
        ));
        let tcp = sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 65536])));
        let raw = sockets.add(raw::Socket::new(
            Some(IpVersion::Ipv4),
            Some(IpProtocol::Icmp),
            raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 16], vec![0; 4096]),
            raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 1], vec![0; 64]),
        ));
        let raw6 = sockets.add(raw::Socket::new(
            Some(IpVersion::Ipv6),
            Some(IpProtocol::Icmpv6),
            raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 16], vec![0; 8192]),
            raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 1], vec![0; 64]),
        ));
        // Deterministic contents that are not periodic in the window size.
        let mut x = 0x2545_F491u32;
        let data = (0..bytes)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        HostPeer {
            dev,
            iface,
            sockets,
            icmp,
            tcp,
            raw,
            raw6,
            target,
            step: Step::Idle,
            since: 0,
            data,
            sent: 0,
            checked: 0,
            ping_ms: Vec::new(),
            tcp_ms: 0,
            icmp_errors: Vec::new(),
            pinged_by: Default::default(),
            why: String::new(),
            http: None,
            served: Vec::new(),
        }
    }

    /// Answers HTTP `GET`s on port 8080 with the files under `dir`.
    pub fn serve_http(&mut self, dir: &str) {
        let socket = self.sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 65536])));
        self.http = Some(Http { dir: dir.into(), socket, request: Vec::new(), answer: Vec::new(), sent: 0 });
    }

    /// One connection at a time: reads the request, sends the file (or
    /// 404), closes.
    fn http_poll(&mut self) {
        let Some(h) = &mut self.http else { return };
        let s = self.sockets.get_mut::<tcp::Socket>(h.socket);
        if !s.is_open() {
            h.request.clear();
            h.answer.clear();
            h.sent = 0;
            let _ = s.listen(HTTP_PORT);
            return;
        }
        if h.answer.is_empty() {
            while s.can_recv() {
                let _ = s.recv(|b| {
                    h.request.extend_from_slice(b);
                    (b.len(), ())
                });
            }
            let Some(end) = h.request.windows(4).position(|w| w == b"\r\n\r\n") else { return };
            let head = String::from_utf8_lossy(&h.request[..end]).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
            let file = (!path.contains("..")).then(|| std::fs::read(h.dir.join(&path)).ok()).flatten();
            h.answer = match &file {
                Some(body) => {
                    self.served.push((path.clone(), body.len()));
                    let mut a = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n", body.len()).into_bytes();
                    a.extend_from_slice(body);
                    a
                }
                None => b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            };
        }
        while h.sent < h.answer.len() && s.can_send() {
            match s.send_slice(&h.answer[h.sent..]) {
                Ok(n) => h.sent += n,
                Err(_) => break,
            }
        }
        if h.sent == h.answer.len() && s.send_queue() == 0 {
            s.close();
        }
    }

    pub fn start(&mut self) {
        if self.step == Step::Idle {
            self.step = Step::Ping { seq: 0, sent_at: -1 };
        }
    }

    pub fn finished(&self) -> bool {
        matches!(self.step, Step::Passed | Step::Failed)
    }

    pub fn passed(&self) -> bool {
        self.step == Step::Passed
    }

    fn fail(&mut self, why: String) {
        self.why = why;
        self.step = Step::Failed;
    }

    /// One round: packets from the Tab5 in, the test advanced, packets to
    /// the Tab5 out (as console frames, in USB-packet pieces).
    pub fn poll(&mut self, usj: &mut Usj, now_ms: i64) {
        self.dev.rx.extend(usj.packets_out.drain(..));
        let now = Instant::from_millis(now_ms);
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
        if self.http.is_some() {
            self.http_poll();
            self.iface.poll(now, &mut self.dev, &mut self.sockets);
        }
        if self.step == Step::Idle {
            // Who pings the PC (answered by the stack itself).
            let r = self.sockets.get_mut::<raw::Socket>(self.raw);
            while let Ok(p) = r.recv() {
                let ihl = (p[0] & 0xF) as usize * 4;
                if p.get(ihl) == Some(&8) {
                    self.pinged_by.insert(IpAddress::v4(p[12], p[13], p[14], p[15]));
                }
            }
        }
        if !self.finished() && self.step != Step::Idle {
            self.advance(now_ms);
            self.iface.poll(now, &mut self.dev, &mut self.sockets);
        }
        for p in self.dev.tx.drain(..) {
            let mut plain = Vec::with_capacity(p.len() + 1);
            plain.push(PACKET);
            plain.extend_from_slice(&p);
            let mut frame = vec![0u8; plain.len() + plain.len() / 254 + 2];
            let n = t5_abi::log::cobs_encode(&plain, &mut frame).expect("frame fits");
            for chunk in frame[..n].chunks(USB_PACKET) {
                usj.send(chunk);
            }
        }
    }

    fn advance(&mut self, now_ms: i64) {
        match self.step {
            Step::Ping { seq, sent_at } => {
                let s = self.sockets.get_mut::<icmp::Socket>(self.icmp);
                if !s.is_open() {
                    s.bind(icmp::Endpoint::Ident(IDENT)).unwrap();
                }
                if sent_at < 0 {
                    let data = b"t5emu host peer";
                    let sent = match self.target {
                        IpAddress::Ipv4(_) => {
                            let repr = Icmpv4Repr::EchoRequest { ident: IDENT, seq_no: seq, data };
                            s.send(repr.buffer_len(), self.target).map(|buf| repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &ChecksumCapabilities::default()))
                        }
                        // smoltcp fills in the checksum (it knows the source).
                        IpAddress::Ipv6(dst) => {
                            let repr = Icmpv6Repr::EchoRequest { ident: IDENT, seq_no: seq, data };
                            s.send(repr.buffer_len(), self.target)
                                .map(|buf| repr.emit(&Ipv6Address::UNSPECIFIED, &dst, &mut Icmpv6Packet::new_unchecked(buf), &ChecksumCapabilities::default()))
                        }
                    };
                    if sent.is_err() {
                        return;
                    }
                    self.step = Step::Ping { seq, sent_at: now_ms };
                    return;
                }
                while let Ok((buf, from)) = s.recv() {
                    // Echo reply (0, ICMPv6 129) with our identifier.
                    let reply = if matches!(from, IpAddress::Ipv4(_)) { 0 } else { 129 };
                    if buf.len() < 8 || buf[0] != reply || u16::from_be_bytes([buf[4], buf[5]]) != IDENT {
                        continue;
                    }
                    let seq_no = u16::from_be_bytes([buf[6], buf[7]]);
                    if from == self.target && seq_no == seq {
                        self.ping_ms.push((now_ms - sent_at) as f64);
                        self.step = if seq + 1 == PINGS {
                            self.since = now_ms;
                            Step::Tcp
                        } else {
                            Step::Ping { seq: seq + 1, sent_at: -1 }
                        };
                        return;
                    }
                }
                if now_ms - sent_at > TIMEOUT_MS {
                    self.fail(format!("no echo reply to ping {seq}"));
                }
            }
            Step::Tcp => {
                let cx = self.iface.context();
                let s = self.sockets.get_mut::<tcp::Socket>(self.tcp);
                if !s.is_open() {
                    s.connect(cx, (self.target, ECHO_PORT), 49152).unwrap();
                }
                if s.can_send() && self.sent < self.data.len() {
                    self.sent += s.send_slice(&self.data[self.sent..]).unwrap_or(0);
                }
                while s.can_recv() {
                    let data = &self.data;
                    let checked = &mut self.checked;
                    let r = s.recv(|b| {
                        let n = b.len().min(data.len() - *checked);
                        let ok = b[..n] == data[*checked..*checked + n];
                        *checked += n;
                        (n, ok)
                    });
                    match r {
                        Ok(true) => {}
                        Ok(false) => return self.fail(format!("echoed bytes differ near offset {}", self.checked)),
                        Err(e) => return self.fail(format!("TCP recv: {e}")),
                    }
                    if self.checked == self.data.len() {
                        break;
                    }
                }
                if self.checked == self.data.len() {
                    s.close();
                    self.tcp_ms = now_ms - self.since;
                    // The ICMP errors are checked when forwarding.
                    let forwarded = self.target != TAB5 && self.target != TAB5_V6;
                    let first = if self.v6() { ICMPV6_TIME_EXCEEDED } else { ICMP_TIME_EXCEEDED };
                    self.step = if forwarded { self.send_probe(first, now_ms) } else { Step::Passed };
                } else if !s.is_open() {
                    self.fail(format!("TCP closed after {} of {} bytes", self.checked, self.data.len()));
                } else if now_ms - self.since > TIMEOUT_MS * 4 {
                    self.fail(format!("TCP echo: {} of {} bytes after {} ms", self.checked, self.data.len(), now_ms - self.since));
                }
            }
            Step::IcmpError { kind, sent_at } => {
                // The ICMP type follows the IP header.
                let mut got = false;
                if self.v6() {
                    let r = self.sockets.get_mut::<raw::Socket>(self.raw6);
                    while let Ok(p) = r.recv() {
                        let src = p.get(8..24).map(|a| IpAddress::Ipv6(Ipv6Address::from(<[u8; 16]>::try_from(a).unwrap())));
                        if src == Some(TAB5_V6) && p.get(40) == Some(&kind) {
                            got = true;
                        }
                    }
                } else {
                    let r = self.sockets.get_mut::<raw::Socket>(self.raw);
                    while let Ok(p) = r.recv() {
                        let ihl = (p[0] & 0xF) as usize * 4;
                        let src = IpAddress::v4(p[12], p[13], p[14], p[15]);
                        if src == TAB5 && p.get(ihl) == Some(&kind) {
                            got = true;
                        }
                    }
                }
                if got {
                    self.icmp_errors.push(kind);
                    self.step = match kind {
                        ICMP_TIME_EXCEEDED if !self.v6() => self.send_probe(ICMP_DEST_UNREACHABLE, now_ms),
                        ICMPV6_TIME_EXCEEDED if self.v6() => self.send_probe(ICMPV6_DEST_UNREACHABLE, now_ms),
                        _ => Step::Passed,
                    };
                } else if now_ms - sent_at > TIMEOUT_MS {
                    self.fail(format!("no ICMP type {kind} from the Tab5"));
                }
            }
            _ => {}
        }
    }

    fn v6(&self) -> bool {
        matches!(self.target, IpAddress::Ipv6(_))
    }

    /// Sends the ping that should come back as ICMP error `kind` (ICMPv6
    /// when the target is IPv6).
    fn send_probe(&mut self, kind: u8, now_ms: i64) -> Step {
        // Drop what the raw sockets have collected so far (echo replies).
        for h in [self.raw, self.raw6] {
            let r = self.sockets.get_mut::<raw::Socket>(h);
            while r.recv().is_ok() {}
        }
        let v6 = self.v6();
        let s = self.sockets.get_mut::<icmp::Socket>(self.icmp);
        let exceeded = kind == if v6 { ICMPV6_TIME_EXCEEDED } else { ICMP_TIME_EXCEEDED };
        let nowhere = if v6 { NOWHERE_V6 } else { NOWHERE };
        let (to, ttl) = if exceeded { (self.target, Some(1)) } else { (nowhere, None) };
        s.set_hop_limit(ttl);
        let (seq_no, data) = (100 + kind as u16, b"t5emu probe");
        match to {
            IpAddress::Ipv4(_) => {
                let repr = Icmpv4Repr::EchoRequest { ident: IDENT, seq_no, data };
                if let Ok(buf) = s.send(repr.buffer_len(), to) {
                    repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &ChecksumCapabilities::default());
                }
            }
            IpAddress::Ipv6(dst) => {
                let repr = Icmpv6Repr::EchoRequest { ident: IDENT, seq_no, data };
                if let Ok(buf) = s.send(repr.buffer_len(), to) {
                    repr.emit(&Ipv6Address::UNSPECIFIED, &dst, &mut Icmpv6Packet::new_unchecked(buf), &ChecksumCapabilities::default());
                }
            }
        }
        Step::IcmpError { kind, sent_at: now_ms }
    }
}
