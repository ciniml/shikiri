//! IPv6 forwarding and the NDP proxy (V4, doc/27). The devices put IPv6
//! packets that are not for the Tab5 in the forwarding inbox (as for IPv4)
//! and copy neighbor discovery messages here (`snoop`); `Stack::forward6`
//! routes, lowers the hop limit and sends, finding the next hop's MAC
//! address in the neighbor table (learnt from every IPv6 frame's source) or
//! asking with a Neighbor Solicitation.
//!
//! With `ndp-proxy on UP to DOWN` (method 2 of doc/26 §6), the downstream
//! link gets router advertisements with the upstream link's /64 (taken by
//! SLAAC on UP): autonomous, not on-link, so downstream hosts make their
//! addresses in it and send everything to the Tab5. Upstream, the Tab5
//! answers Neighbor Solicitations for the downstream hosts' addresses with
//! its own MAC address (proxy Neighbor Advertisement, RFC 4389 §4.1.3),
//! so packets for them come to it. Downstream it answers for the rest of
//! the /64 in the same way: hosts (smoltcp among them) take a SLAAC /64 as
//! on-link even when the RA says it is not.

use smoltcp::wire::IpCidr;
use t5_rt::event;

use crate::Stack;
use crate::filter::{self, Action};
use crate::fwd::{IFACE0, TEST, WLAN};
use crate::link::{ETH_FRAME, ETH_HDR, MTU, Ring};
use crate::{HOST_NET6, TEST_NET6};

const NEIGHBORS: usize = 32;
const NEIGHBOR_TTL_NS: u64 = 300_000_000_000;
const PENDING: usize = 4;
const PENDING_NS: u64 = 1_000_000_000;
const RA_EVERY_NS: u64 = 10_000_000_000;
/// While Wi-Fi has no global address: how often to ask for an RA.
const RS_EVERY_NS: u64 = 10_000_000_000;
const ALL_ROUTERS: u128 = 0xff02_0000_0000_0000_0000_0000_0000_0002;
/// Advertised lifetimes (s): the router, and the prefix (valid, preferred).
const ROUTER_LIFETIME: u16 = 1800;
const PREFIX_VALID: u32 = 3600;
const PREFIX_PREFERRED: u32 = 1800;
const OWN_MAX: usize = 16;

const ETHERTYPE_IPV6: [u8; 2] = [0x86, 0xDD];
const NEXT_ICMPV6: u8 = 58;
const RS: u8 = 133;
const RA: u8 = 134;
const NS: u8 = 135;
const NA: u8 = 136;

fn be128(b: &[u8]) -> u128 {
    u128::from_be_bytes(b[..16].try_into().unwrap())
}

fn same64(a: u128, b: u128) -> bool {
    a >> 64 == b >> 64
}

/// Global or unique-local unicast (what may be forwarded).
fn routable(a: u128) -> bool {
    let top = (a >> 112) as u16;
    a > 1 && top >> 8 != 0xFF && top & 0xFFC0 != 0xFE80
}

// ---- the Tab5's own addresses ----

static mut OWN: [u128; OWN_MAX] = [0; OWN_MAX];

/// Records every IPv6 address of every interface (each poll).
pub fn set_own(addrs: impl Iterator<Item = u128>) {
    let own = unsafe { &mut *core::ptr::addr_of_mut!(OWN) };
    *own = [0; OWN_MAX];
    for (slot, a) in own.iter_mut().zip(addrs) {
        *slot = a;
    }
}

fn is_own(a: u128) -> bool {
    unsafe { (*core::ptr::addr_of!(OWN)).contains(&a) }
}

/// Whether an IPv6 packet is for another host (and so forwarded).
pub fn transit(ip: &[u8]) -> bool {
    ip.len() >= 40 && ip[0] >> 4 == 6 && {
        let dst = be128(&ip[24..40]);
        routable(dst) && !is_own(dst)
    }
}

// ---- neighbors ----

#[derive(Clone, Copy)]
struct Neighbor {
    link: u8,
    ip: u128,
    mac: [u8; 6],
    at: u64,
}

pub struct Neighbors {
    entries: [Option<Neighbor>; NEIGHBORS],
    next: usize,
}

static mut NEIGHBOR_TABLE: Neighbors = Neighbors { entries: [None; NEIGHBORS], next: 0 };

pub fn neighbors() -> &'static mut Neighbors {
    unsafe { &mut *core::ptr::addr_of_mut!(NEIGHBOR_TABLE) }
}

impl Neighbors {
    pub fn learn(&mut self, link: usize, ip: u128, mac: [u8; 6]) {
        if ip == 0 || mac[0] & 1 != 0 {
            return;
        }
        let n = Neighbor { link: link as u8, ip, mac, at: crate::clock_ns() };
        if let Some(slot) = self.entries.iter_mut().find(|s| s.is_some_and(|s| s.link == link as u8 && s.ip == ip)) {
            *slot = Some(n);
            return;
        }
        self.entries[self.next] = Some(n);
        self.next = (self.next + 1) % NEIGHBORS;
    }

    pub fn lookup(&self, link: usize, ip: u128) -> Option<[u8; 6]> {
        let now = crate::clock_ns();
        self.entries.iter().flatten().find(|n| n.link == link as u8 && n.ip == ip && now - n.at < NEIGHBOR_TTL_NS).map(|n| n.mac)
    }
}

/// Neighbor discovery messages (RS, RA, NS) for `Stack::ndp`, tagged with
/// their link.
static mut NDP_RING: Ring<8> = Ring::new();

fn ndp_in() -> &'static mut Ring<8> {
    unsafe { &mut *core::ptr::addr_of_mut!(NDP_RING) }
}

/// A device saw IPv6 frame `f` on `link`: learns its source, and keeps a
/// copy of neighbor discovery messages. A Duplicate Address Detection
/// solicitation (from ::) tells the address its sender is about to use.
pub fn snoop(link: usize, f: &[u8]) {
    if f.len() < ETH_HDR + 40 {
        return;
    }
    let ip = &f[ETH_HDR..];
    let mac: [u8; 6] = f[6..12].try_into().unwrap();
    let src = be128(&ip[8..24]);
    neighbors().learn(link, src, mac);
    // A Neighbor Advertisement names its target (and may come from another
    // of the sender's addresses).
    if ip[6] == NEXT_ICMPV6 && ip.len() >= 40 + 24 && ip[40] == NA {
        neighbors().learn(link, be128(&ip[48..64]), mac);
    }
    if ip[6] == NEXT_ICMPV6 && ip.len() >= 40 + 24 && matches!(ip[40], RS | RA | NS) {
        if ip[40] == NS && src == 0 {
            neighbors().learn(link, be128(&ip[48..64]), mac);
        }
        ndp_in().push(link as u8, f);
    }
}

// ---- building messages ----

/// ICMPv6's checksum over its pseudo-header (RFC 8200 §8.1).
fn icmp6_checksum(src: &[u8; 16], dst: &[u8; 16], msg: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut add = |b: &[u8]| {
        for c in b.chunks(2) {
            sum += (c[0] as u32) << 8 | c.get(1).copied().unwrap_or(0) as u32;
        }
    };
    add(src);
    add(dst);
    add(&(msg.len() as u32).to_be_bytes());
    add(&[0, 0, 0, NEXT_ICMPV6]);
    add(msg);
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// An Ethernet frame with an ICMPv6 message (hop limit 255, as neighbor
/// discovery needs); its length.
fn icmp6_frame(out: &mut [u8], eth_dst: [u8; 6], eth_src: [u8; 6], src: u128, dst: u128, msg: &mut [u8]) -> usize {
    let (s, d) = (src.to_be_bytes(), dst.to_be_bytes());
    msg[2] = 0;
    msg[3] = 0;
    let c = icmp6_checksum(&s, &d, msg);
    msg[2..4].copy_from_slice(&c.to_be_bytes());
    out[..6].copy_from_slice(&eth_dst);
    out[6..12].copy_from_slice(&eth_src);
    out[12..14].copy_from_slice(&ETHERTYPE_IPV6);
    let ip = &mut out[ETH_HDR..];
    ip[..8].copy_from_slice(&[0x60, 0, 0, 0, 0, 0, NEXT_ICMPV6, 255]);
    ip[4..6].copy_from_slice(&(msg.len() as u16).to_be_bytes());
    ip[8..24].copy_from_slice(&s);
    ip[24..40].copy_from_slice(&d);
    ip[40..40 + msg.len()].copy_from_slice(msg);
    ETH_HDR + 40 + msg.len()
}

/// The multicast MAC address of IPv6 address `a` (RFC 2464 §7).
fn multicast_mac(a: u128) -> [u8; 6] {
    let b = a.to_be_bytes();
    [0x33, 0x33, b[12], b[13], b[14], b[15]]
}

fn solicited_node(a: u128) -> u128 {
    0xff02_0000_0000_0000_0000_0001_ff00_0000 | a & 0xFF_FFFF
}

fn link_local(mac: [u8; 6]) -> u128 {
    match crate::link_local(mac) {
        IpCidr::Ipv6(c) => c.address().to_bits(),
        _ => 0,
    }
}

const ALL_NODES: u128 = 0xff02_0000_0000_0000_0000_0000_0000_0001;

// ICMPv6 errors (RFC 4443 §3): types and codes.
const DEST_UNREACHABLE: u8 = 1;
const PACKET_TOO_BIG: u8 = 2;
const TIME_EXCEEDED: u8 = 3;
const NO_ROUTE: u8 = 0;
const ADMIN_PROHIBITED: u8 = 1;
const ADDRESS_UNREACHABLE: u8 = 3;
/// Errors sent per second at most (RFC 4443 §2.4 (f)), as for IPv4.
const ICMP6_PER_S: u32 = 10;
/// An error and its quote fit the IPv6 minimum MTU (RFC 4443 §2.4 (c)).
const MIN_MTU: usize = 1280;
/// RDNSS servers kept, and the RA option's type (RFC 8106 §5.1).
const RDNSS_MAX: usize = 2;
const OPT_RDNSS: u8 = 25;
/// `route6_fwd`'s `from` for packets of our own (any link will do).
const FROM_US: usize = usize::MAX;

struct Pending {
    /// Where the packet came in (for an error if no neighbor answers).
    from: usize,
    link: usize,
    next_hop: u128,
    since: u64,
    len: usize,
    packet: [u8; MTU],
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct Stats {
    forwarded: u32,
    no_route: u32,
    hop_limit: u32,
    no_neighbor: u32,
    proxied: u32,
    ra_sent: u32,
    icmp_sent: u32,
    icmp_limited: u32,
}

pub struct Fwd6 {
    pending: [Option<Pending>; PENDING],
    /// The upstream router (from its RA): link-local address and MAC.
    router: Option<(u128, [u8; 6])>,
    ra_at: u64,
    ra_now: bool,
    rs_at: u64,
    rs_sent: u32,
    /// The downstream prefix last advertised (logged when it changes).
    advertised: Option<u128>,
    /// The one before, advertised deprecated (preferred lifetime 0) until
    /// then, so hosts move to the new one (doc/46 §3).
    retired: Option<(u128, u64)>,
    stats: Stats,
    logged: Stats,
    stats_at: u64,
    icmp_until: u64,
    icmp_left: u32,
    /// DNS servers the upstream router's RA named (RDNSS, RFC 8106), and
    /// until when (doc/46).
    rdnss: [u128; RDNSS_MAX],
    rdnss_n: usize,
    rdnss_until: u64,
}

impl Fwd6 {
    pub const fn new() -> Fwd6 {
        let z = Stats { forwarded: 0, no_route: 0, hop_limit: 0, no_neighbor: 0, proxied: 0, ra_sent: 0, icmp_sent: 0, icmp_limited: 0 };
        Fwd6 {
            pending: [const { None }; PENDING],
            router: None,
            ra_at: 0,
            ra_now: false,
            rs_at: 0,
            rs_sent: 0,
            advertised: None,
            retired: None,
            stats: z,
            logged: z,
            stats_at: 0,
            icmp_until: 0,
            icmp_left: 0,
            rdnss: [0; RDNSS_MAX],
            rdnss_n: 0,
            rdnss_until: 0,
        }
    }
}

impl Stack {
    /// `ndp-proxy on UP to DOWN`, if set.
    fn proxy(&self) -> Option<(usize, usize)> {
        filter::active().0.ndp.map(|(up, down)| (up as usize, down as usize)).filter(|&(up, down)| self.eth_present(up) && self.eth_present(down))
    }

    fn eth_present(&self, link: usize) -> bool {
        link != IFACE0 && self.eth[link - 1].is_some()
    }

    /// The /64 SLAAC gave the upstream link (its first global address).
    fn upstream_prefix(&self, up: usize) -> Option<u128> {
        let e = self.eth[up - 1].as_ref()?;
        e.iface.ip_addrs().iter().find_map(|c| match c {
            IpCidr::Ipv6(c) if c.prefix_len() == 64 && !c.address().is_unicast_link_local() && !same64(c.address().to_bits(), TEST_NET6.to_bits()) => {
                Some(c.address().to_bits() & !((1u128 << 64) - 1))
            }
            _ => None,
        })
    }

    /// Records every interface's IPv6 addresses (for `transit`).
    pub fn sync_own6(&self) {
        let iface0 = self.iface.ip_addrs().iter();
        let eth = self.eth.iter().flatten().flat_map(|e| e.iface.ip_addrs().iter());
        set_own(iface0.chain(eth).filter_map(|c| match c {
            IpCidr::Ipv6(c) => Some(c.address().to_bits()),
            _ => None,
        }));
    }

    /// Where a forwarded packet to `dst` (which came in on `from`) goes:
    /// (link, next hop).
    fn route6_fwd(&self, from: usize, dst: u128) -> Option<(usize, u128)> {
        if let Some((up, down)) = self.proxy()
            && let Some(p) = self.upstream_prefix(up)
            && same64(dst, p)
        {
            // A downstream host, else a host on the upstream link (never
            // back where it came from).
            return if neighbors().lookup(down, dst).is_some() {
                Some((down, dst))
            } else if from != up {
                Some((up, dst))
            } else {
                None
            };
        }
        if same64(dst, TEST_NET6.to_bits()) && self.eth_present(TEST) {
            return Some((TEST, dst));
        }
        if same64(dst, HOST_NET6.to_bits()) && self.dev.host.is_some() {
            return Some((IFACE0, dst));
        }
        match self.v6.router {
            Some((r, _)) if self.eth_present(WLAN) => Some((WLAN, r)),
            _ => None,
        }
    }

    /// An IPv6 packet from the inbox.
    pub fn forward6(&mut self, from: usize, p: &mut [u8]) {
        if p.len() < 40 {
            return;
        }
        let total = 40 + u16::from_be_bytes([p[4], p[5]]) as usize;
        if total > p.len() {
            return;
        }
        let p = &mut p[..total];
        let dst = be128(&p[24..40]);
        let Some((link, next_hop)) = self.route6_fwd(from, dst) else {
            self.v6.stats.no_route += 1;
            self.icmp6_error(from, DEST_UNREACHABLE, NO_ROUTE, 0, p);
            return;
        };
        match filter::check(filter::FORWARD, from as u8, link as u8, p) {
            Action::Pass => {}
            Action::Drop => return,
            Action::Reject => return self.icmp6_error(from, DEST_UNREACHABLE, ADMIN_PROHIBITED, 0, p),
        }
        if p[7] <= 1 {
            self.v6.stats.hop_limit += 1;
            self.icmp6_error(from, TIME_EXCEEDED, 0, 0, p);
            return;
        }
        if link != IFACE0 && p.len() > MTU {
            self.icmp6_error(from, PACKET_TOO_BIG, 0, MTU as u32, p);
            return;
        }
        p[7] -= 1;
        self.v6.stats.forwarded += 1;
        self.send6(from, link, next_hop, p);
    }

    /// An address of ours on `link` to send an error to `to` from: one in
    /// `to`'s /64, else a global one (the error may have to cross routers),
    /// else the link-local one; never loopback.
    fn own6_on(&self, link: usize, to: u128) -> Option<u128> {
        let addrs = if link == IFACE0 { self.iface.ip_addrs() } else { self.eth[link - 1].as_ref()?.iface.ip_addrs() };
        let v6 = || {
            addrs.iter().filter_map(|c| match c {
                IpCidr::Ipv6(c) if !c.address().is_loopback() => Some(c.address()),
                _ => None,
            })
        };
        v6().find(|a| same64(a.to_bits(), to))
            .or_else(|| v6().find(|a| !a.is_unicast_link_local()))
            .or_else(|| v6().next())
            .map(|a| a.to_bits())
    }

    /// Answers packet `p` (which came in on `from`) with an ICMPv6 error
    /// (RFC 4443): not for errors, nor to multicast or unspecified sources
    /// or for multicast destinations (§2.4 (e)), at most `ICMP6_PER_S` a
    /// second. `param` is the 4 bytes after the checksum (the MTU of a
    /// Packet Too Big).
    pub fn icmp6_error(&mut self, from: usize, kind: u8, code: u8, param: u32, p: &[u8]) {
        if p.len() < 40 {
            return;
        }
        let (src, dst) = (be128(&p[8..24]), be128(&p[24..40]));
        if src == 0 || src >> 120 == 0xFF || (dst >> 120 == 0xFF && kind != PACKET_TOO_BIG) {
            return;
        }
        // An ICMPv6 error message (types below 128) is never answered.
        if p[6] == NEXT_ICMPV6 && p.get(40).is_some_and(|&t| t < 128) {
            return;
        }
        let Some(own) = self.own6_on(from, src) else { return };
        let now = crate::clock_ns();
        if now >= self.v6.icmp_until {
            self.v6.icmp_until = now + 1_000_000_000;
            self.v6.icmp_left = ICMP6_PER_S;
        }
        if self.v6.icmp_left == 0 {
            self.v6.stats.icmp_limited += 1;
            return;
        }
        self.v6.icmp_left -= 1;
        let Some((link, next_hop)) = self.route6_fwd(FROM_US, src) else { return };
        let quoted = p.len().min(MIN_MTU - 40 - 8);
        let len = 40 + 8 + quoted;
        let mut out = [0u8; MIN_MTU];
        out[0] = 0x60;
        out[4..6].copy_from_slice(&((8 + quoted) as u16).to_be_bytes());
        out[6] = NEXT_ICMPV6;
        out[7] = 64;
        out[8..24].copy_from_slice(&own.to_be_bytes());
        out[24..40].copy_from_slice(&src.to_be_bytes());
        out[40] = kind;
        out[41] = code;
        out[44..48].copy_from_slice(&param.to_be_bytes());
        out[48..len].copy_from_slice(&p[..quoted]);
        let c = icmp6_checksum(&own.to_be_bytes(), &src.to_be_bytes(), &out[40..len]);
        out[42..44].copy_from_slice(&c.to_be_bytes());
        self.v6.stats.icmp_sent += 1;
        event!(Debug, "net.icmp6_error", kind = kind, code = code, link = link as u32);
        self.send6(link, link, next_hop, &out[..len]);
    }

    /// A packet to us that the filter rejected (IN).
    pub fn reject6(&mut self, from: usize, p: &[u8]) {
        self.icmp6_error(from, DEST_UNREACHABLE, ADMIN_PROHIBITED, 0, p);
    }

    fn send6(&mut self, from: usize, link: usize, next_hop: u128, p: &[u8]) {
        if link == IFACE0 {
            if let Some(h) = &self.dev.host {
                let _ = h.write(p);
            }
            return;
        }
        let Some(e) = self.eth[link - 1].as_mut() else { return };
        let mac = e.dev.mac;
        if let Some(to) = neighbors().lookup(link, next_hop) {
            let mut f = [0u8; ETH_FRAME];
            f[..6].copy_from_slice(&to);
            f[6..12].copy_from_slice(&mac);
            f[12..14].copy_from_slice(&ETHERTYPE_IPV6);
            f[ETH_HDR..ETH_HDR + p.len()].copy_from_slice(p);
            e.dev.port.write(&f[..ETH_HDR + p.len()]);
            return;
        }
        // Neighbor Solicitation (RFC 4861 §4.3), with our MAC address.
        let mut msg = [0u8; 32];
        msg[0] = NS;
        msg[8..24].copy_from_slice(&next_hop.to_be_bytes());
        msg[24..32].copy_from_slice(&[1, 1, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
        let mut f = [0u8; ETH_HDR + 40 + 32];
        let to = solicited_node(next_hop);
        let n = icmp6_frame(&mut f, multicast_mac(to), mac, link_local(mac), to, &mut msg);
        e.dev.port.write(&f[..n]);
        match self.v6.pending.iter_mut().find(|s| s.is_none()) {
            Some(slot) if p.len() <= MTU => {
                let mut w = Pending { from, link, next_hop, since: crate::clock_ns(), len: p.len(), packet: [0; MTU] };
                w.packet[..p.len()].copy_from_slice(p);
                *slot = Some(w);
            }
            _ => self.v6.stats.no_neighbor += 1,
        }
    }

    /// Packets that waited for a neighbor, the neighbor discovery messages
    /// that came, router advertisements downstream, the counters.
    pub fn after_forward6(&mut self) {
        let now = crate::clock_ns();
        for i in 0..PENDING {
            let Some(w) = &self.v6.pending[i] else { continue };
            if neighbors().lookup(w.link, w.next_hop).is_some() {
                let w = self.v6.pending[i].take().unwrap();
                self.send6(w.from, w.link, w.next_hop, &w.packet[..w.len]);
            } else if now - w.since > PENDING_NS {
                let w = self.v6.pending[i].take().unwrap();
                self.v6.stats.no_neighbor += 1;
                // Not for our own errors that waited (`from` is their link).
                if w.from != w.link {
                    self.icmp6_error(w.from, DEST_UNREACHABLE, ADDRESS_UNREACHABLE, 0, &w.packet[..w.len]);
                }
            }
        }
        self.ndp();
        self.solicit();
        self.advertise();
        if now >= self.v6.stats_at {
            self.v6.stats_at = now + 30_000_000_000;
            let s = self.v6.stats;
            if s != self.v6.logged {
                self.v6.logged = s;
                event!(
                    Info,
                    "net.fwd6",
                    forwarded = s.forwarded,
                    no_route = s.no_route,
                    hop_limit = s.hop_limit,
                    no_neighbor = s.no_neighbor,
                    proxied = s.proxied,
                    ra_sent = s.ra_sent,
                    icmp_sent = s.icmp_sent,
                    icmp_limited = s.icmp_limited
                );
            }
        }
    }

    fn ndp(&mut self) {
        let mut f = [0u8; ETH_FRAME];
        while let Some((link, len)) = ndp_in().pop(&mut f) {
            let link = link as usize;
            let ip = &f[ETH_HDR..len];
            let src = be128(&ip[8..24]);
            let mac: [u8; 6] = f[6..12].try_into().unwrap();
            match ip[40] {
                RA if link == WLAN => {
                    if self.v6.router.is_none_or(|(r, _)| r != src) {
                        // Router lifetime, and whether a prefix option comes.
                        let lifetime = u16::from_be_bytes([ip[46], ip[47]]);
                        // flags: M (0x80, addresses by DHCPv6), O (0x40, other
                        // settings such as DNS by DHCPv6).
                        event!(Info, "net.ra", router = &ip[8..24], lifetime = lifetime, flags = ip[45], len = (len - ETH_HDR) as u32);
                    }
                    if u16::from_be_bytes([ip[46], ip[47]]) > 0 {
                        self.v6.router = Some((src, mac));
                    }
                    self.take_rdnss(ip.get(56..).unwrap_or(&[]));
                }
                RS if self.proxy().is_some_and(|(_, down)| down == link) => self.v6.ra_now = true,
                NS => {
                    let target = be128(&ip[48..64]);
                    let Some((up, down)) = self.proxy() else { continue };
                    if !self.upstream_prefix(up).is_some_and(|p| same64(target, p)) {
                        continue;
                    }
                    let downstream = neighbors().lookup(down, target).is_some();
                    // Upstream: for the downstream hosts. Downstream: for
                    // the rest of the /64 (hosts take it as on-link whatever
                    // the RA says), but never for DAD, which would make the
                    // asking host drop its address.
                    let answer = if link == up { downstream } else { link == down && !downstream && src != 0 && !is_own(target) };
                    if !answer {
                        continue;
                    }
                    let Some(e) = self.eth[link - 1].as_mut() else { continue };
                    let own = e.dev.mac;
                    // Proxy Neighbor Advertisement: router, solicited unless
                    // it answers DAD, not override (RFC 4389 §4.1.3).
                    let mut msg = [0u8; 32];
                    msg[0] = NA;
                    msg[4] = if src == 0 { 0x80 } else { 0xC0 };
                    msg[8..24].copy_from_slice(&target.to_be_bytes());
                    msg[24..32].copy_from_slice(&[2, 1, own[0], own[1], own[2], own[3], own[4], own[5]]);
                    let (to, to_mac) = if src == 0 { (ALL_NODES, multicast_mac(ALL_NODES)) } else { (src, mac) };
                    let mut out = [0u8; ETH_HDR + 40 + 32];
                    // From the target address itself: smoltcp files the
                    // MAC address under the advertisement's source.
                    let n = icmp6_frame(&mut out, to_mac, own, target, to, &mut msg);
                    e.dev.port.write(&out[..n]);
                    self.v6.stats.proxied += 1;
                }
                _ => {}
            }
        }
    }

    /// Router Solicitations on Wi-Fi while it has no global address:
    /// smoltcp's SLAAC asks three times at start only, and a router's
    /// unsolicited RAs may be many minutes apart (seen on the Tab5).
    fn solicit(&mut self) {
        if !self.eth_present(WLAN) || self.upstream_prefix(WLAN).is_some() {
            return;
        }
        let now = crate::clock_ns();
        if now < self.v6.rs_at {
            return;
        }
        self.v6.rs_at = now + RS_EVERY_NS;
        let Some(e) = self.eth[WLAN - 1].as_mut() else { return };
        let mac = e.dev.mac;
        let mut msg = [0u8; 16];
        msg[0] = RS;
        msg[8..16].copy_from_slice(&[1, 1, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
        let mut out = [0u8; ETH_HDR + 40 + 16];
        let n = icmp6_frame(&mut out, multicast_mac(ALL_ROUTERS), mac, link_local(mac), ALL_ROUTERS, &mut msg);
        e.dev.port.write(&out[..n]);
        self.v6.rs_sent += 1;
        if self.v6.rs_sent.is_power_of_two() {
            event!(Info, "net.rs_sent", count = self.v6.rs_sent);
        }
    }

    /// Router advertisements downstream: every `RA_EVERY_NS`, and at once
    /// after a Router Solicitation.
    /// RDNSS options among RA options `opts`: the servers, and how long
    /// they hold (0 drops them).
    fn take_rdnss(&mut self, mut opts: &[u8]) {
        while opts.len() >= 8 {
            let len = opts[1] as usize * 8;
            if len == 0 || len > opts.len() {
                return;
            }
            if opts[0] == OPT_RDNSS && len >= 24 {
                let lifetime = u32::from_be_bytes([opts[4], opts[5], opts[6], opts[7]]);
                let mut servers = [0u128; RDNSS_MAX];
                let n = ((len - 8) / 16).min(RDNSS_MAX);
                for (k, s) in servers[..n].iter_mut().enumerate() {
                    *s = be128(&opts[8 + 16 * k..24 + 16 * k]);
                }
                let changed = servers[..n] != self.v6.rdnss[..self.v6.rdnss_n] || (lifetime == 0) != (self.v6.rdnss_n == 0);
                if lifetime == 0 {
                    self.v6.rdnss_n = 0;
                } else {
                    (self.v6.rdnss, self.v6.rdnss_n) = (servers, n);
                    self.v6.rdnss_until = crate::clock_ns() + lifetime as u64 * 1_000_000_000;
                }
                if changed {
                    event!(Info, "net.rdnss", servers = n as u32, first = &servers[0].to_be_bytes()[..], lifetime = lifetime);
                }
            }
            opts = &opts[len..];
        }
    }

    /// The first DNS server the upstream RA named, while it holds.
    pub fn dns6(&self) -> Option<u128> {
        (self.v6.rdnss_n > 0 && crate::clock_ns() < self.v6.rdnss_until).then_some(self.v6.rdnss[0])
    }

    fn advertise(&mut self) {
        let Some((up, down)) = self.proxy() else { return };
        let Some(prefix) = self.upstream_prefix(up) else { return };
        let now = crate::clock_ns();
        if !self.v6.ra_now && now < self.v6.ra_at {
            return;
        }
        self.v6.ra_now = false;
        self.v6.ra_at = now + RA_EVERY_NS;
        if self.v6.advertised != Some(prefix) {
            if let Some(old) = self.v6.advertised {
                self.v6.retired = Some((old, now + PREFIX_VALID as u64 * 1_000_000_000));
            }
            self.v6.advertised = Some(prefix);
            event!(Info, "net.ndp_proxy", up = crate::fwd::LINK_NAMES[up], down = crate::fwd::LINK_NAMES[down], prefix = &prefix.to_be_bytes()[..8]);
        }
        let Some(e) = self.eth[down - 1].as_mut() else { return };
        let mac = e.dev.mac;
        let mut msg = [0u8; 64 + 32 + 8 + 16 * RDNSS_MAX];
        msg[0] = RA;
        msg[4] = 64; // current hop limit
        msg[6..8].copy_from_slice(&ROUTER_LIFETIME.to_be_bytes());
        // Source link-layer address.
        msg[16..24].copy_from_slice(&[1, 1, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
        // MTU.
        msg[24..32].copy_from_slice(&[5, 1, 0, 0, 0, 0, 0x05, 0xDC]);
        // Prefix information: /64, autonomous (A), not on-link (L = 0).
        msg[32] = 3;
        msg[33] = 4;
        msg[34] = 64;
        msg[35] = 0x40;
        msg[36..40].copy_from_slice(&PREFIX_VALID.to_be_bytes());
        msg[40..44].copy_from_slice(&PREFIX_PREFERRED.to_be_bytes());
        msg[48..64].copy_from_slice(&prefix.to_be_bytes());
        let mut len = 64;
        // The previous prefix, deprecated: preferred 0, valid 0 (hosts keep
        // its addresses for what is left of their lifetime, at most 2 h,
        // RFC 4862 §5.5.3 (e)).
        match self.v6.retired {
            Some((old, until)) if now < until && old != prefix => {
                msg[len..len + 4].copy_from_slice(&[3, 4, 64, 0x40]);
                msg[len + 16..len + 32].copy_from_slice(&old.to_be_bytes());
                len += 32;
            }
            _ => self.v6.retired = None,
        }
        // The upstream's DNS servers (the downstream hosts are on its /64).
        let left_s = self.v6.rdnss_until.saturating_sub(now) / 1_000_000_000;
        if self.v6.rdnss_n > 0 && left_s > 0 {
            let n = self.v6.rdnss_n;
            msg[len] = OPT_RDNSS;
            msg[len + 1] = (1 + 2 * n) as u8;
            msg[len + 4..len + 8].copy_from_slice(&(left_s.min(ROUTER_LIFETIME as u64) as u32).to_be_bytes());
            for (k, s) in self.v6.rdnss[..n].iter().enumerate() {
                msg[len + 8 + 16 * k..len + 24 + 16 * k].copy_from_slice(&s.to_be_bytes());
            }
            len += 8 + 16 * n;
        }
        let mut out = [0u8; ETH_HDR + 40 + 64 + 32 + 8 + 16 * RDNSS_MAX];
        let n = icmp6_frame(&mut out, multicast_mac(ALL_NODES), mac, link_local(mac), ALL_NODES, &mut msg[..len]);
        e.dev.port.write(&out[..n]);
        self.v6.stats.ra_sent += 1;
    }
}
