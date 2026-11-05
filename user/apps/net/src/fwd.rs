//! Forwarding between the links (N3, doc/23 §3): packets the devices found
//! not to be for the Tab5 wait in `inbox`; `Stack::forward` routes each one
//! (longest prefix), lowers its TTL, and sends it out of the link the route
//! names, finding the next hop's MAC address by ARP on Ethernet links. A
//! TTL that runs out and a destination without a route are answered with
//! ICMP Time Exceeded and Destination Unreachable (at most `ICMP_PER_S`
//! a second).

use t5_rt::event;

use crate::Stack;
use crate::filter::{self, Action};
use crate::nat;
use crate::link::{ETH_FRAME, ETH_HDR, HOST_ADDR, HOST_PREFIX, MTU, Ring};

/// Interfaces (and links): 0 is loopback with the host link, then the
/// Ethernet links.
pub const IFACE0: usize = 0;
pub const WLAN: usize = 1;
pub const TEST: usize = 2;
/// The wired Ethernet (doc/58 R2), where the board has one.
pub const ETH: usize = 3;
pub const IFACES: usize = 4;
pub const LINK_NAMES: [&str; IFACES] = ["host", "wlan0", "test0", "eth0"];

const INBOX: usize = 8;
const REJECTS: usize = 4;
const ARP_ENTRIES: usize = 32;
const ARP_TTL_NS: u64 = 300_000_000_000;
/// Packets waiting for their next hop's MAC address, and for how long.
const PENDING: usize = 4;
const PENDING_NS: u64 = 1_000_000_000;
const ICMP_PER_S: u32 = 10;
const STATS_NS: u64 = 30_000_000_000;

const PROTO_ICMP: u8 = 1;
const ICMP_DEST_UNREACHABLE: u8 = 3;
const ICMP_TIME_EXCEEDED: u8 = 11;
/// Destination Unreachable's code for a filter's reject.
const ADMIN_PROHIBITED: u8 = 13;

pub fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// The Internet checksum (RFC 1071) of `b`.
pub fn checksum(b: &[u8]) -> u16 {
    let mut sum = 0u32;
    for c in b.chunks(2) {
        sum += (c[0] as u32) << 8 | c.get(1).copied().unwrap_or(0) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

static mut INBOX_RING: Ring<INBOX> = Ring::new();

/// Packets to forward, tagged with the link they came from (Ethernet links
/// keep their header).
pub fn inbox() -> &'static mut Ring<INBOX> {
    unsafe { &mut *core::ptr::addr_of_mut!(INBOX_RING) }
}

static mut OWN: [Option<u32>; IFACES] = [None; IFACES];

/// Records the links' own addresses (`Stack::sync_addrs`).
pub fn set_own(own: [Option<u32>; IFACES]) {
    unsafe { *core::ptr::addr_of_mut!(OWN) = own };
}

/// Whether `a` is one of the Tab5's addresses, on any link.
pub fn is_own(a: u32) -> bool {
    unsafe { (*core::ptr::addr_of!(OWN)).contains(&Some(a)) }
}

static mut REJECT_RING: Ring<REJECTS> = Ring::new();

/// Packets the filter rejected (IN): answered with ICMP Destination
/// Unreachable, administratively prohibited.
pub fn rejects() -> &'static mut Ring<REJECTS> {
    unsafe { &mut *core::ptr::addr_of_mut!(REJECT_RING) }
}

#[derive(Clone, Copy)]
struct ArpEntry {
    link: u8,
    ip: u32,
    mac: [u8; 6],
    at: u64,
}

/// What ARP packets on the Ethernet links said: IPv4 → MAC address.
pub struct Arp {
    entries: [Option<ArpEntry>; ARP_ENTRIES],
    next: usize,
}

static mut ARP: Arp = Arp { entries: [None; ARP_ENTRIES], next: 0 };

pub fn arp() -> &'static mut Arp {
    unsafe { &mut *core::ptr::addr_of_mut!(ARP) }
}

impl Arp {
    pub fn learn(&mut self, link: usize, ip: u32, mac: [u8; 6]) {
        let e = ArpEntry { link: link as u8, ip, mac, at: crate::clock_ns() };
        if let Some(slot) = self.entries.iter_mut().find(|s| s.is_some_and(|s| s.link == link as u8 && s.ip == ip)) {
            *slot = Some(e);
            return;
        }
        // Otherwise the next slot in turn (the oldest, roughly).
        self.entries[self.next] = Some(e);
        self.next = (self.next + 1) % ARP_ENTRIES;
    }

    pub fn lookup(&self, link: usize, ip: u32) -> Option<[u8; 6]> {
        let now = crate::clock_ns();
        self.entries.iter().flatten().find(|e| e.link == link as u8 && e.ip == ip && now - e.at < ARP_TTL_NS).map(|e| e.mac)
    }
}

struct Pending {
    link: usize,
    next_hop: u32,
    since: u64,
    len: usize,
    packet: [u8; MTU],
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct Stats {
    forwarded: u32,
    no_route: u32,
    ttl_expired: u32,
    no_arp: u32,
    dropped: u32,
    icmp_sent: u32,
    icmp_limited: u32,
}

pub struct Forwarder {
    pending: [Option<Pending>; PENDING],
    stats: Stats,
    logged: Stats,
    stats_at: u64,
    /// ICMP errors left this second, and when the second ends.
    icmp_left: u32,
    icmp_until: u64,
}

impl Forwarder {
    pub const fn new() -> Forwarder {
        Forwarder {
            pending: [const { None }; PENDING],
            stats: Stats { forwarded: 0, no_route: 0, ttl_expired: 0, no_arp: 0, dropped: 0, icmp_sent: 0, icmp_limited: 0 },
            logged: Stats { forwarded: 0, no_route: 0, ttl_expired: 0, no_arp: 0, dropped: 0, icmp_sent: 0, icmp_limited: 0 },
            stats_at: 0,
            icmp_left: 0,
            icmp_until: 0,
        }
    }
}

/// A route: where packets for `dst` go and through which next hop.
#[derive(Clone, Copy)]
pub struct Route {
    pub link: usize,
    pub next_hop: u32,
}

impl Stack {
    /// The link's own address and prefix.
    pub fn link_addr(&self, link: usize) -> Option<(u32, u8)> {
        match link {
            IFACE0 => self.dev.host.is_some().then_some((HOST_ADDR, HOST_PREFIX)),
            _ => self.eth[link - 1].as_ref().and_then(|e| e.dev.own),
        }
    }

    /// Longest prefix over the links' networks and the default route that
    /// DHCP gave on Wi-Fi. 127/8 has none (it never leaves interface 0).
    pub fn route(&self, dst: u32) -> Option<Route> {
        if dst >> 24 == 127 {
            return None;
        }
        let mut best: Option<(u8, Route)> = None;
        let mut consider = |prefix: u8, net: u32, route: Route| {
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
            if dst & mask == net & mask && best.is_none_or(|(p, _)| prefix > p) {
                best = Some((prefix, route));
            }
        };
        for link in 0..IFACES {
            if let Some((addr, prefix)) = self.link_addr(link) {
                consider(prefix, addr, Route { link, next_hop: dst });
            }
        }
        if let Some(gw) = self.eth[WLAN - 1].as_ref().and_then(|e| e.router) {
            consider(0, 0, Route { link: WLAN, next_hop: gw });
        }
        best.map(|(_, r)| r)
    }

    /// Forwards what the devices put in the inbox, then what waited for ARP.
    pub fn forward(&mut self) {
        // Runs every poll pass: a static buffer, not one cleared each time.
        static mut BUF: [u8; ETH_FRAME] = [0; ETH_FRAME];
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
        while let Some((link, len)) = inbox().pop(buf) {
            let link = link as usize;
            let off = if link == IFACE0 { 0 } else { ETH_HDR };
            if len > off {
                if buf[off] >> 4 == 6 {
                    self.forward6(link, &mut buf[off..len]);
                } else {
                    self.forward_one(link, &mut buf[off..len]);
                }
            }
        }
        while let Some((link, len)) = rejects().pop(buf) {
            let link = link as usize;
            let off = if link == IFACE0 { 0 } else { ETH_HDR };
            if len >= off + 40 && buf[off] >> 4 == 6 {
                self.reject6(link, &buf[off..len]);
            } else if len >= off + 20 {
                self.icmp_error(link, ICMP_DEST_UNREACHABLE, ADMIN_PROHIBITED, &buf[off..len]);
            }
        }
        self.flush_pending();
        self.log_stats();
    }

    fn forward_one(&mut self, from: usize, p: &mut [u8]) {
        let ihl = (p[0] & 0xF) as usize * 4;
        let total = u16::from_be_bytes([p[2], p[3]]) as usize;
        if p.len() < 20 || ihl < 20 || total < ihl || total > p.len() {
            self.fwd.stats.dropped += 1;
            return;
        }
        let p = &mut p[..total];
        let route = self.route(be32(&p[16..20]));
        if let Some(r) = route {
            match filter::check(filter::FORWARD, from as u8, r.link as u8, p) {
                Action::Pass => {}
                Action::Drop => return,
                Action::Reject => return self.icmp_error(from, ICMP_DEST_UNREACHABLE, ADMIN_PROHIBITED, p),
            }
        }
        if p[8] <= 1 {
            self.fwd.stats.ttl_expired += 1;
            self.icmp_error(from, ICMP_TIME_EXCEEDED, 0, p);
            return;
        }
        let Some(route) = route else {
            self.fwd.stats.no_route += 1;
            // Code 0: network unreachable.
            self.icmp_error(from, ICMP_DEST_UNREACHABLE, 0, p);
            return;
        };
        p[8] -= 1;
        if let Some((addr, _)) = self.link_addr(route.link) {
            nat::outbound(p, route.link as u8, addr);
        }
        p[10] = 0;
        p[11] = 0;
        let c = checksum(&p[..ihl]);
        p[10..12].copy_from_slice(&c.to_be_bytes());
        self.fwd.stats.forwarded += 1;
        self.send_ip(route, p);
    }

    /// Sends an IPv4 packet out of `route.link`.
    fn send_ip(&mut self, route: Route, p: &[u8]) {
        if route.link == IFACE0 {
            if let Some(h) = &self.dev.host
                && h.write(p).is_ok()
            {
                self.dev.to_host += 1;
            }
            return;
        }
        let Some(e) = self.eth[route.link - 1].as_mut() else { return };
        if let Some(mac) = arp().lookup(route.link, route.next_hop) {
            let f = crate::link::tx_frame();
            f[..6].copy_from_slice(&mac);
            f[6..12].copy_from_slice(&e.dev.mac);
            f[12..14].copy_from_slice(&[0x08, 0x00]);
            f[ETH_HDR..ETH_HDR + p.len()].copy_from_slice(p);
            if !e.dev.port.write(&f[..ETH_HDR + p.len()]) {
                self.fwd.stats.dropped += 1;
            }
            return;
        }
        // Ask, and keep the packet a while (one per free place).
        let Some((own, _)) = e.dev.own else { return };
        let mut req = [0u8; ETH_HDR + 28];
        req[..6].fill(0xFF);
        req[6..12].copy_from_slice(&e.dev.mac);
        req[12..14].copy_from_slice(&[0x08, 0x06]);
        req[14..22].copy_from_slice(&[0, 1, 0x08, 0x00, 6, 4, 0, 1]);
        req[22..28].copy_from_slice(&e.dev.mac);
        req[28..32].copy_from_slice(&own.to_be_bytes());
        req[38..42].copy_from_slice(&route.next_hop.to_be_bytes());
        e.dev.port.write(&req);
        match self.fwd.pending.iter_mut().find(|s| s.is_none()) {
            Some(slot) if p.len() <= MTU => {
                let mut w = Pending { link: route.link, next_hop: route.next_hop, since: crate::clock_ns(), len: p.len(), packet: [0; MTU] };
                w.packet[..p.len()].copy_from_slice(p);
                *slot = Some(w);
            }
            _ => self.fwd.stats.no_arp += 1,
        }
    }

    fn flush_pending(&mut self) {
        let now = crate::clock_ns();
        for i in 0..PENDING {
            let Some(w) = &self.fwd.pending[i] else { continue };
            if arp().lookup(w.link, w.next_hop).is_some() {
                let w = self.fwd.pending[i].take().unwrap();
                self.send_ip(Route { link: w.link, next_hop: w.next_hop }, &w.packet[..w.len]);
            } else if now - w.since > PENDING_NS {
                self.fwd.pending[i] = None;
                self.fwd.stats.no_arp += 1;
            }
        }
    }

    /// Answers packet `p` (which came in on `from`) with an ICMP error from
    /// the address of `from`: its header and the first 8 bytes of its
    /// payload (RFC 792). Never for ICMP errors or broadcast sources.
    fn icmp_error(&mut self, from: usize, kind: u8, code: u8, p: &[u8]) {
        let ihl = (p[0] & 0xF) as usize * 4;
        let src = be32(&p[12..16]);
        if p[9] == PROTO_ICMP && p.get(ihl).is_some_and(|&t| t != 0 && t != 8) {
            return;
        }
        if src == 0 || src == u32::MAX || src >> 28 == 0xE {
            return;
        }
        let Some((own, _)) = self.link_addr(from) else { return };
        let now = crate::clock_ns();
        if now >= self.fwd.icmp_until {
            self.fwd.icmp_until = now + 1_000_000_000;
            self.fwd.icmp_left = ICMP_PER_S;
        }
        if self.fwd.icmp_left == 0 {
            self.fwd.stats.icmp_limited += 1;
            return;
        }
        self.fwd.icmp_left -= 1;
        let Some(route) = self.route(src) else { return };
        let quoted = (ihl + 8).min(p.len());
        let len = 20 + 8 + quoted;
        let mut out = [0u8; 20 + 8 + 60 + 8];
        out[0] = 0x45;
        out[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        out[8] = 64;
        out[9] = PROTO_ICMP;
        out[12..16].copy_from_slice(&own.to_be_bytes());
        out[16..20].copy_from_slice(&src.to_be_bytes());
        let c = checksum(&out[..20]);
        out[10..12].copy_from_slice(&c.to_be_bytes());
        out[20] = kind;
        out[21] = code;
        out[28..28 + quoted].copy_from_slice(&p[..quoted]);
        let c = checksum(&out[20..len]);
        out[22..24].copy_from_slice(&c.to_be_bytes());
        self.fwd.stats.icmp_sent += 1;
        self.send_ip(route, &out[..len]);
    }

    fn log_stats(&mut self) {
        let now = crate::clock_ns();
        if now < self.fwd.stats_at {
            return;
        }
        self.fwd.stats_at = now + STATS_NS;
        let s = self.fwd.stats;
        if s == self.fwd.logged {
            return;
        }
        self.fwd.logged = s;
        event!(
            Info,
            "net.fwd",
            forwarded = s.forwarded,
            no_route = s.no_route,
            ttl_expired = s.ttl_expired,
            no_arp = s.no_arp,
            dropped = s.dropped + inbox().dropped,
            icmp_sent = s.icmp_sent,
            icmp_limited = s.icmp_limited
        );
    }
}
