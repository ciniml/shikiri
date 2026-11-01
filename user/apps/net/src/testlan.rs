//! The test LAN (`test0`, doc/23 §4): a second Ethernet link whose far end
//! is a host simulated inside `net`, so forwarding can be tried where there
//! is no second real network (t5emu, or the Tab5 with only the host link).
//! `net` is 192.168.50.1/24 on it; the peer is 192.168.50.2/24 with its
//! default route through `net`, answers pings, and echoes TCP on port 7777.
//! It also starts traffic of its own, for the filter (doc/24 §6): every
//! `PROBE_NS` a ping, in turn to the Tab5 (10.55.0.2) and to the PC
//! (10.55.0.1), counting the replies (`net.peer_probes`; the first answer
//! from each is `net.peer_answered`).
//!
//! With `run net 3` the peer gets its address from `net`'s DHCP server
//! instead (N5, doc/25 §4) and, once it has one, fetches `http://1.1.1.1/`
//! (`net.peer_http`): through masquerade on Wi-Fi, a device on the LAN
//! reaches the web.
//!
//! The peer also takes IPv6 addresses from router advertisements (SLAAC):
//! with `ndp-proxy` (doc/27) it gets one in the Wi-Fi link's /64 and then
//! fetches `http://[2606:4700:4700::1111]/` once (`net.peer_http`, v6).

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, TxToken};
use smoltcp::socket::{dhcpv4, dns, icmp, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address};
use t5_rt::event;

use crate::link::{ETH_FRAME, RX_PEER, Rx, from_peer, rx_frame, to_peer, tx_frame};
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
const PROBE_NAMES: [&str; 2] = ["tab5", "pc"];
const HTTP_SERVER: IpAddress = IpAddress::v4(1, 1, 1, 1);
const HTTP_SERVER6: IpAddress = IpAddress::v6(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111);
const HTTP_BUF: usize = 4 << 10;
const HTTP_TIMEOUT_NS: u64 = 10_000_000_000;

/// The peer's side of the rings.
struct PeerDev;

struct PeerTx;

impl TxToken for PeerTx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let buf = tx_frame();
        let r = f(&mut buf[..len]);
        from_peer().push(0, &buf[..len]);
        r
    }
}

impl Device for PeerDev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = PeerTx;

    fn receive(&mut self, _: Instant) -> Option<(Rx, PeerTx)> {
        let len = to_peer().pop(rx_frame(RX_PEER))?.1;
        Some((Rx { slot: RX_PEER, len }, PeerTx))
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

static mut STORAGE: [SocketStorage<'static>; 5] = [SocketStorage::EMPTY; 5];
static mut DNS_SLOTS: [Option<dns::DnsQuery>; 1] = [const { None }; 1];
/// What the peer looks up through the DNS server DHCP gave (doc/29).
const DNS_NAME: &str = "one.one.one.one";
static mut HTTP_RX: [u8; HTTP_BUF] = [0; HTTP_BUF];
static mut HTTP_TX: [u8; HTTP_BUF] = [0; HTTP_BUF];
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
    /// DHCP client (`run net 3`), until it has an address.
    dhcp: Option<SocketHandle>,
    configured: bool,
    http: SocketHandle,
    /// The HTTP fetch: not yet (None), going (start time), done.
    http_since: Option<u64>,
    http_done: bool,
    http_sent: bool,
    http_bytes: u32,
    /// The IPv6 fetch has been started.
    http6: bool,
    /// The fetch going on is over IPv6.
    http_v6: bool,
    /// The lookup through the DNS server DHCP gave: socket, query, start.
    dns: Option<(SocketHandle, dns::QueryHandle, u64)>,
}

impl Peer {
    pub fn new(dhcp: bool) -> Peer {
        let mut dev = PeerDev;
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(PEER_MAC)));
        config.slaac = true;
        let mut iface = Interface::new(config, &mut dev, now());
        // IPv6 (doc/26): fd50::2 and its link-local address, always.
        iface.update_ip_addrs(|a| {
            let _ = a.push(crate::link_local(PEER_MAC));
            let _ = a.push(IpCidr::new(crate::in64(crate::TEST_NET6, 2).into(), 64));
        });
        // Through the Tab5's link-local address, as routers advertise
        // themselves: its solicitations carry that address's MAC, which
        // the peer then knows (smoltcp drops a packet while it asks).
        if let IpCidr::Ipv6(r) = crate::link_local(NET_MAC) {
            let _ = iface.routes_mut().add_default_ipv6_route(r.address());
        }
        if !dhcp {
            iface.update_ip_addrs(|a| {
                let _ = a.push(IpCidr::new(Ipv4Address::from_bits(PEER_ADDR).into(), PREFIX));
            });
            let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::from_bits(NET_ADDR));
        }
        let mut sockets = SocketSet::new(unsafe { &mut (&mut *core::ptr::addr_of_mut!(STORAGE))[..] });
        let (rx, tx) = unsafe { (&mut (&mut *core::ptr::addr_of_mut!(ECHO_RX))[..], &mut (&mut *core::ptr::addr_of_mut!(ECHO_TX))[..]) };
        let echo = sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx)));
        let probe = unsafe {
            let rx = icmp::PacketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(PROBE_RX_META))[..], &mut (&mut *core::ptr::addr_of_mut!(PROBE_RX))[..]);
            let tx = icmp::PacketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(PROBE_TX_META))[..], &mut (&mut *core::ptr::addr_of_mut!(PROBE_TX))[..]);
            sockets.add(icmp::Socket::new(rx, tx))
        };
        let _ = sockets.get_mut::<icmp::Socket>(probe).bind(icmp::Endpoint::Ident(PROBE_IDENT));
        let http = unsafe {
            let rx = tcp::SocketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(HTTP_RX))[..]);
            let tx = tcp::SocketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(HTTP_TX))[..]);
            sockets.add(tcp::Socket::new(rx, tx))
        };
        let dhcp = dhcp.then(|| sockets.add(dhcpv4::Socket::new()));
        Peer {
            iface,
            dev,
            sockets,
            echo,
            probe,
            probe_at: crate::clock_ns() + PROBE_NS,
            sent: [0; 2],
            answered: [0; 2],
            configured: dhcp.is_none(),
            dhcp,
            http,
            http_since: None,
            http_done: false,
            http_sent: false,
            http_bytes: 0,
            http6: false,
            http_v6: false,
            dns: None,
        }
    }

    /// The DHCP client: takes the address and route it is given.
    fn dhcp(&mut self) {
        let Some(h) = self.dhcp else { return };
        if let Some(dhcpv4::Event::Configured(c)) = self.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
            self.iface.update_ip_addrs(|a| {
                a.retain(|c| matches!(c, IpCidr::Ipv6(_)));
                let _ = a.push(IpCidr::Ipv4(c.address));
            });
            if let Some(r) = c.router {
                let _ = self.iface.routes_mut().add_default_ipv4_route(r);
            }
            event!(Info, "net.peer_dhcp", addr = c.address.address().to_bits(), router = c.router.map_or(0, |r| r.to_bits()), dns = c.dns_servers.first().map_or(0, |d| d.to_bits()));
            if let (Some(server), None) = (c.dns_servers.first(), self.dns) {
                let q = unsafe { &mut *core::ptr::addr_of_mut!(DNS_SLOTS) };
                let h = self.sockets.add(dns::Socket::new(&[IpAddress::Ipv4(*server)], &mut q[..]));
                let cx = self.iface.context();
                if let Ok(query) = self.sockets.get_mut::<dns::Socket>(h).start_query(cx, DNS_NAME, DnsQueryType::A) {
                    self.dns = Some((h, query, crate::clock_ns()));
                }
            }
            self.configured = true;
            // Fetch the page once (DHCP mode only).
            if self.http_since.is_none() && !self.http_done {
                let cx = self.iface.context();
                let s = self.sockets.get_mut::<tcp::Socket>(self.http);
                if s.connect(cx, (HTTP_SERVER, 80), 41000).is_ok() {
                    self.http_since = Some(crate::clock_ns());
                }
            }
        }
    }

    /// The HTTP fetch: sends the request once connected, counts the answer,
    /// logs its first line when the server closes (or on timeout).
    fn http(&mut self) {
        let Some(since) = self.http_since else { return };
        let s = self.sockets.get_mut::<tcp::Socket>(self.http);
        if s.may_send() && !self.http_sent {
            self.http_sent = s.send_slice(b"GET / HTTP/1.0\r\nHost: one.one.one.one\r\n\r\n").is_ok();
        }
        let mut status = [0u8; 32];
        let mut n = 0;
        while s.can_recv() {
            let first = self.http_bytes == 0;
            let got = s.recv(|b| {
                if first {
                    n = b.iter().position(|&c| c == b'\r').unwrap_or(b.len()).min(status.len());
                    status[..n].copy_from_slice(&b[..n]);
                }
                (b.len(), b.len())
            });
            match got {
                Ok(k) if k > 0 => self.http_bytes += k as u32,
                _ => break,
            }
        }
        if n > 0 {
            event!(Info, "net.peer_http", v6 = self.http_v6, status = core::str::from_utf8(&status[..n]).unwrap_or("?"), ms = ((crate::clock_ns() - since) / 1_000_000) as u32);
        }
        let ended = !s.may_recv() && self.http_bytes > 0;
        if ended || crate::clock_ns() - since > HTTP_TIMEOUT_NS {
            if !ended {
                event!(Warn, "net.peer_http_failed", bytes = self.http_bytes);
            }
            s.abort();
            self.http_since = None;
            self.http_done = true;
        }
    }

    /// Pings the next target when it is time; counts the replies.
    fn probe(&mut self) {
        if !self.configured {
            return;
        }
        let s = self.sockets.get_mut::<icmp::Socket>(self.probe);
        while let Ok((buf, from)) = s.recv() {
            let Ok(p) = Icmpv4Packet::new_checked(buf) else { continue };
            if let Ok(Icmpv4Repr::EchoReply { ident: PROBE_IDENT, .. }) = Icmpv4Repr::parse(&p, &Default::default())
                && let Some(t) = PROBE_TARGETS.iter().position(|a| *a == from)
            {
                if self.answered[t] == 0 {
                    event!(Info, "net.peer_answered", target = PROBE_NAMES[t]);
                }
                self.answered[t] += 1;
            }
        }
        let now = crate::clock_ns();
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
    /// Starts the IPv6 fetch once SLAAC has given a global address (not
    /// fd50::/64) and no other fetch is going on.
    fn http6_start(&mut self) {
        if self.http6 || self.http_since.is_some() {
            return;
        }
        let global = self.iface.ip_addrs().iter().any(|c| match c {
            IpCidr::Ipv6(c) => !c.address().is_unicast_link_local() && c.address().segments()[0] != 0xfd50,
            _ => false,
        });
        if !global {
            return;
        }
        self.http6 = true;
        let cx = self.iface.context();
        let s = self.sockets.get_mut::<tcp::Socket>(self.http);
        if s.connect(cx, (HTTP_SERVER6, 80), 41001).is_ok() {
            let addr = self.iface.ip_addrs().iter().find_map(|c| match c {
                IpCidr::Ipv6(c) if c.address().segments()[0] != 0xfd50 && !c.address().is_unicast_link_local() => Some(c.address().octets()),
                _ => None,
            });
            event!(Info, "net.peer_slaac", addr = &addr.unwrap_or_default()[..]);
            self.http_since = Some(crate::clock_ns());
            self.http_sent = false;
            self.http_bytes = 0;
            self.http_v6 = true;
        }
    }

    /// The lookup's answer (`net.peer_dns`), once.
    fn dns_result(&mut self) {
        let Some((h, q, since)) = self.dns else { return };
        let r = self.sockets.get_mut::<dns::Socket>(h).get_query_result(q);
        let ms = ((crate::clock_ns() - since) / 1_000_000) as u32;
        match r {
            Err(dns::GetQueryResultError::Pending) => return,
            Ok(addrs) => {
                let ip = addrs.iter().find_map(|a| match a {
                    IpAddress::Ipv4(v) => Some(v.to_bits()),
                    _ => None,
                });
                event!(Info, "net.peer_dns", name = DNS_NAME, ip = ip.unwrap_or(0), ms = ms);
            }
            Err(dns::GetQueryResultError::Failed) => event!(Warn, "net.peer_dns_failed", name = DNS_NAME, ms = ms),
        }
        // Done: the slot stays taken so it is not asked again.
        self.dns = Some((h, q, u64::MAX));
    }

    pub fn poll(&mut self) {
        self.dhcp();
        if self.dns.is_some_and(|d| d.2 != u64::MAX) {
            self.dns_result();
        }
        self.http6_start();
        self.http();
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
