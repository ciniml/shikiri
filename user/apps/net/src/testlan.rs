//! The test LAN (`test0`, doc/23 §4): a second Ethernet link whose far end
//! is a host simulated inside `net`, so forwarding can be tried where there
//! is no second real network (t5emu, or the Tab5 with only the host link).
//! `net` is 192.168.50.1/24 on it; the peer is 192.168.50.2/24 with its
//! default route through `net`, answers pings, and echoes TCP on port 7777.

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address};

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

static mut STORAGE: [SocketStorage<'static>; 1] = [SocketStorage::EMPTY; 1];
static mut ECHO_RX: [u8; ECHO_BUF] = [0; ECHO_BUF];
static mut ECHO_TX: [u8; ECHO_BUF] = [0; ECHO_BUF];

pub struct Peer {
    iface: Interface,
    dev: PeerDev,
    sockets: SocketSet<'static>,
    echo: SocketHandle,
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
        Peer { iface, dev, sockets, echo }
    }

    /// Runs the peer: its stack, and the echo server (one connection at a
    /// time, listening again when it ends).
    pub fn poll(&mut self) {
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
