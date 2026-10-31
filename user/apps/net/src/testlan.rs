//! The test LAN (`test0`, doc/23 §4): a second Ethernet link whose far end
//! is a host simulated inside `net`, so forwarding can be tried where there
//! is no second real network (t5emu, or the Tab5 with only the host link).
//! `net` is 192.168.50.1/24 on it; the peer is 192.168.50.2/24 with its
//! default route through `net`, answers pings, and echoes TCP on port 7777.
//! It also starts traffic of its own, for the filter (doc/24 §6): every
//! `PROBE_NS` a ping, in turn to the Tab5 (10.55.0.2) and to the PC
//! (10.55.0.1), counting the replies (`net.peer_probes`).

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, TxToken};
use smoltcp::socket::{icmp, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address};
use t5_rt::{event, time_ns};

use crate::link::{ETH_FRAME, Rx, from_peer, to_peer};
use crate::now;

pub const NET_ADDR: u32 = 0xC0A8_3201; // 192.168.50.1
pub const PEER_ADDR: u32 = 0xC0A8_3202; // 192.168.50.2
pub const PREFIX: u8 = 24;
/// Locally administered addresses.
pub const NET_MAC: [u8; 6] = [0x02, 0x74, 0x35, 0, 0, 1];
const PEER_MAC: [u8; 6] = [0x02, 0x74, 0x35, 0, 0, 2];
pub const ECHO_PORT: u16 = 7777;
const ECHO_BUF: usize = 16 << 10;
const PROBE_NS: u64 = 2_000_000_000;
const PROBE_IDENT: u16 = 0x5050;
const PROBE_TARGETS: [IpAddress; 2] = [IpAddress::v4(10, 55, 0, 2), IpAddress::v4(10, 55, 0, 1)];

/// The peer's side of the rings.
struct PeerDev;

struct PeerTx;

impl TxToken for PeerTx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; ETH_FRAME];
        let r = f(&mut buf[..len]);
        from_peer().push(0, &buf[..len]);
        r
    }
}

impl Device for PeerDev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = PeerTx;

    fn receive(&mut self, _: Instant) -> Option<(Rx, PeerTx)> {
        let mut out = Rx { frame: [0; ETH_FRAME], len: 0 };
        out.len = to_peer().pop(&mut out.frame)?.1;
        Some((out, PeerTx))
    }

    fn transmit(&mut self, _: Instant) -> Option<PeerTx> {
        (!from_peer().is_full()).then_some(PeerTx)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = ETH_FRAME;
        c
    }
}

static mut STORAGE: [SocketStorage<'static>; 2] = [SocketStorage::EMPTY; 2];
static mut PROBE_RX: [u8; 512] = [0; 512];
static mut PROBE_TX: [u8; 512] = [0; 512];
static mut PROBE_RX_META: [icmp::PacketMetadata; 4] = [icmp::PacketMetadata::EMPTY; 4];
static mut PROBE_TX_META: [icmp::PacketMetadata; 4] = [icmp::PacketMetadata::EMPTY; 4];
static mut ECHO_RX: [u8; ECHO_BUF] = [0; ECHO_BUF];
static mut ECHO_TX: [u8; ECHO_BUF] = [0; ECHO_BUF];

pub struct Peer {
    iface: Interface,
    dev: PeerDev,
    sockets: SocketSet<'static>,
    echo: SocketHandle,
    probe: SocketHandle,
    probe_at: u64,
    /// Pings sent and answered, per target.
    sent: [u32; 2],
    answered: [u32; 2],
}

impl Peer {
    pub fn new() -> Peer {
        let mut dev = PeerDev;
        let mut iface = Interface::new(Config::new(HardwareAddress::Ethernet(EthernetAddress(PEER_MAC))), &mut dev, now());
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(Ipv4Address::from_bits(PEER_ADDR).into(), PREFIX));
        });
        let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::from_bits(NET_ADDR));
        let mut sockets = SocketSet::new(unsafe { &mut (&mut *core::ptr::addr_of_mut!(STORAGE))[..] });
        let (rx, tx) = unsafe { (&mut (&mut *core::ptr::addr_of_mut!(ECHO_RX))[..], &mut (&mut *core::ptr::addr_of_mut!(ECHO_TX))[..]) };
        let echo = sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx)));
        let probe = unsafe {
            let rx = icmp::PacketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(PROBE_RX_META))[..], &mut (&mut *core::ptr::addr_of_mut!(PROBE_RX))[..]);
            let tx = icmp::PacketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(PROBE_TX_META))[..], &mut (&mut *core::ptr::addr_of_mut!(PROBE_TX))[..]);
            sockets.add(icmp::Socket::new(rx, tx))
        };
        let _ = sockets.get_mut::<icmp::Socket>(probe).bind(icmp::Endpoint::Ident(PROBE_IDENT));
        Peer { iface, dev, sockets, echo, probe, probe_at: time_ns() + PROBE_NS, sent: [0; 2], answered: [0; 2] }
    }

    /// Pings the next target when it is time; counts the replies.
    fn probe(&mut self) {
        let s = self.sockets.get_mut::<icmp::Socket>(self.probe);
        while let Ok((buf, from)) = s.recv() {
            let Ok(p) = Icmpv4Packet::new_checked(buf) else { continue };
            if let Ok(Icmpv4Repr::EchoReply { ident: PROBE_IDENT, .. }) = Icmpv4Repr::parse(&p, &Default::default())
                && let Some(t) = PROBE_TARGETS.iter().position(|a| *a == from)
            {
                self.answered[t] += 1;
            }
        }
        let now = time_ns();
        if now < self.probe_at {
            return;
        }
        self.probe_at = now + PROBE_NS;
        let n = self.sent[0] + self.sent[1];
        let t = (n % 2) as usize;
        let repr = Icmpv4Repr::EchoRequest { ident: PROBE_IDENT, seq_no: n as u16, data: b"test0 peer" };
        if let Ok(buf) = s.send(repr.buffer_len(), PROBE_TARGETS[t]) {
            repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &Default::default());
            self.sent[t] += 1;
        }
        if n % 10 == 9 {
            event!(Info, "net.peer_probes", sent_tab5 = self.sent[0], answered_tab5 = self.answered[0], sent_pc = self.sent[1], answered_pc = self.answered[1]);
        }
    }

    /// Runs the peer: its stack, its pings, and the echo server (one
    /// connection at a time, listening again when it ends).
    pub fn poll(&mut self) {
        self.probe();
        for _ in 0..16 {
            let moved = self.iface.poll(now(), &mut self.dev, &mut self.sockets);
            let s = self.sockets.get_mut::<tcp::Socket>(self.echo);
            if !s.is_open() {
                let _ = s.listen(ECHO_PORT);
            }
            let mut buf = [0u8; 1024];
            let mut echoed = false;
            while s.can_recv() && s.send_capacity() - s.send_queue() >= buf.len() {
                match s.recv_slice(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = s.send_slice(&buf[..n]);
                        echoed = true;
                    }
                }
            }
            if !s.may_recv() && s.may_send() && s.send_queue() == 0 {
                s.close();
            }
            if moved == smoltcp::iface::PollResult::None && !echoed && to_peer().is_empty() {
                break;
            }
        }
    }

    pub fn poll_delay(&mut self) -> Option<smoltcp::time::Duration> {
        self.iface.poll_delay(now(), &self.sockets)
    }
}
