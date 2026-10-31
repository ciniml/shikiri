//! The PC end of the host link (doc/21) inside the emulator: a smoltcp
//! interface at 10.55.0.1/24 whose packets travel as frames on the USJ
//! console, as `t5log hostlink` sends them. Once started it pings the Tab5
//! (10.55.0.2) and then sends data to the TCP echo server (`nettest 1`,
//! port 7777) and checks that the same bytes come back.

use std::collections::VecDeque;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{icmp, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr};

use crate::periph::{PACKET, Usj};

const PEER: IpAddress = IpAddress::v4(10, 55, 0, 1);
const TAB5: IpAddress = IpAddress::v4(10, 55, 0, 2);
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
    Passed,
    Failed,
}

pub struct HostPeer {
    dev: Queues,
    iface: Interface,
    sockets: SocketSet<'static>,
    icmp: SocketHandle,
    tcp: SocketHandle,
    step: Step,
    since: i64,
    /// What goes to the echo server, and how much was sent and checked.
    data: Vec<u8>,
    sent: usize,
    checked: usize,
    pub ping_ms: Vec<f64>,
    pub tcp_ms: i64,
    pub why: String,
}

impl HostPeer {
    /// `bytes`: how much to send to the echo server.
    pub fn new(bytes: usize) -> HostPeer {
        let mut dev = Queues::default();
        let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut dev, Instant::ZERO);
        iface.update_ip_addrs(|a| a.push(IpCidr::new(PEER, 24)).unwrap());
        let mut sockets = SocketSet::new(Vec::new());
        let icmp = sockets.add(icmp::Socket::new(
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]),
            icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]),
        ));
        let tcp = sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 65536])));
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
            step: Step::Idle,
            since: 0,
            data,
            sent: 0,
            checked: 0,
            ping_ms: Vec::new(),
            tcp_ms: 0,
            why: String::new(),
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
                    let repr = Icmpv4Repr::EchoRequest { ident: IDENT, seq_no: seq, data: b"t5emu host peer" };
                    let Ok(buf) = s.send(repr.buffer_len(), TAB5) else { return };
                    repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &ChecksumCapabilities::default());
                    self.step = Step::Ping { seq, sent_at: now_ms };
                    return;
                }
                while let Ok((buf, from)) = s.recv() {
                    let Ok(p) = Icmpv4Packet::new_checked(buf) else { continue };
                    let Ok(Icmpv4Repr::EchoReply { seq_no, .. }) = Icmpv4Repr::parse(&p, &ChecksumCapabilities::default()) else {
                        continue;
                    };
                    if from == TAB5 && seq_no == seq {
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
                    s.connect(cx, (TAB5, ECHO_PORT), 49152).unwrap();
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
                    self.step = Step::Passed;
                } else if !s.is_open() {
                    self.fail(format!("TCP closed after {} of {} bytes", self.checked, self.data.len()));
                } else if now_ms - self.since > TIMEOUT_MS * 4 {
                    self.fail(format!("TCP echo: {} of {} bytes after {} ms", self.checked, self.data.len(), now_ms - self.since));
                }
            }
            _ => {}
        }
    }
}
