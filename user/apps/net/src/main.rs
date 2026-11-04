#![no_std]
#![no_main]

//! The network service (N1-N3, doc/20, doc/21, doc/22, doc/23): smoltcp in
//! U-mode, one interface per kind of link:
//!
//! - interface 0 (`Medium::Ip`): loopback (127.0.0.0/8) and, if the kernel
//!   offers it, the host link to the PC (`netdev:host`, 10.55.0.2/24);
//! - the Wi-Fi station (`netdev:wlan0`, Ethernet, address by DHCP);
//! - with `run net 1`, the test LAN (`test0`, 192.168.50.1/24, whose peer
//!   192.168.50.2 lives in this service too, `testlan`).
//!
//! Packets for other hosts are forwarded between the links (`fwd`); every
//! packet passes the filter (`filter`), whose rules come over `svc:netctl`
//! (`rules`). Clients
//! on `svc:net` get a 64 KiB slice of shared memory for data and use TCP and
//! UDP sockets, ping and DNS through `t5_abi::net` requests. A socket that
//! listens on every link is one smoltcp socket per interface. No allocator:
//! socket buffers are a static pool.
//!
//! Built with `--permit shm --permit listen:net --permit listen:netctl
//! --permit netdev:host --permit netdev:wlan0`.

use core::mem::MaybeUninit;

mod filter;
mod fwd;
mod fwd6;
mod link;
mod nat;
mod rules;
mod dhcpd;
mod dnsrelay;
mod testlan;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage};
use smoltcp::socket::{dhcpv4, dns, icmp, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, Icmpv6Packet, Icmpv6Repr, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address, Ipv6Cidr};
use t5_rt::net::{self as proto, iface, op, state, status};
use t5_rt::{Channel, Error, Handle, Listener, Shm, chan, event, netdev, rights, sleep_ms, time_ns, wait_raw};

use fwd::{Forwarder, IFACE0, IFACES, LINK_NAMES, TEST, WLAN};
use link::{EthNic, HOST_ADDR, HOST_PREFIX, Links, Port};

t5_rt::entry!(main);

const CLIENTS: usize = 4;
/// Client sockets.
const SOCKETS: usize = 8;
/// smoltcp sockets with buffers: a client socket that listens on every
/// link takes one per interface.
const POOL: usize = 16;
const TCP_BUF: usize = 16 << 10;
const UDP_PACKETS: usize = 16;
const UDP_BUF: usize = 8 << 10;
const ICMP_PACKETS: usize = 2;
const ICMP_BUF: usize = 256;
/// Echo data at most (PING's b).
const PING_DATA: usize = 64;
/// Name lookups in flight at once.
const DNS_QUERIES: usize = 4;
/// Longest wait without a request, so timers (retransmission) run.
const IDLE_NS: u64 = 50_000_000;
/// The pool index of the shared DNS socket (not in the pool).
const SHARED: usize = usize::MAX;

/// IPv6 (doc/26 §2): the host link's ULA network (the PC is ::1 in it, the
/// Tab5 ::2) and the test LAN's (`net` ::1, the peer ::2).
pub const HOST_NET6: Ipv6Address = Ipv6Address::new(0xfd55, 0, 0, 0, 0, 0, 0, 0);
pub const TEST_NET6: Ipv6Address = Ipv6Address::new(0xfd50, 0, 0, 0, 0, 0, 0, 0);

/// Address `host` in the /64 `net`.
pub fn in64(net: Ipv6Address, host: u16) -> Ipv6Address {
    let s = net.segments();
    Ipv6Address::new(s[0], s[1], s[2], s[3], 0, 0, 0, host)
}

fn same64(a: &Ipv6Address, net: &Ipv6Address) -> bool {
    a.segments()[..4] == net.segments()[..4]
}

/// The link-local address made from a MAC address (EUI-64, RFC 4291 §2.5.1).
pub fn link_local(mac: [u8; 6]) -> IpCidr {
    let w = |a: u8, b: u8| u16::from_be_bytes([a, b]);
    IpCidr::new(Ipv6Address::new(0xfe80, 0, 0, 0, w(mac[0] ^ 2, mac[1]), w(mac[2], 0xff), w(0xfe, mac[3]), w(mac[4], mac[5])).into(), 64)
}


/// The time of the current pass (`tick`), in ns. Reading the time is a
/// system call; once per pass instead of once per smoltcp poll and per
/// packet (filter, NAT, neighbour tables) took 300,000 calls per 16 MiB
/// echo away (doc/34).
static mut CLOCK_NS: u64 = 0;

/// Reads the time for the pass that begins.
pub fn tick() -> u64 {
    let t = time_ns();
    unsafe { CLOCK_NS = t };
    t
}

pub fn clock_ns() -> u64 {
    unsafe { CLOCK_NS }
}

pub fn now() -> Instant {
    Instant::from_micros((clock_ns() / 1000) as i64)
}

// ---- socket storage (static: there is no allocator) ----

static mut TCP_RX: [[u8; TCP_BUF]; POOL] = [[0; TCP_BUF]; POOL];
static mut TCP_TX: [[u8; TCP_BUF]; POOL] = [[0; TCP_BUF]; POOL];
static mut UDP_RX: [[u8; UDP_BUF]; POOL] = [[0; UDP_BUF]; POOL];
static mut UDP_TX: [[u8; UDP_BUF]; POOL] = [[0; UDP_BUF]; POOL];
// The tables whose empty values are not zero bytes are uninitialised (in
// .bss, not in the image) and filled by `init_statics` at start.
static mut UDP_RX_META: MaybeUninit<[[udp::PacketMetadata; UDP_PACKETS]; POOL]> = MaybeUninit::uninit();
static mut UDP_TX_META: MaybeUninit<[[udp::PacketMetadata; UDP_PACKETS]; POOL]> = MaybeUninit::uninit();
static mut ICMP_RX: [[u8; ICMP_BUF]; POOL] = [[0; ICMP_BUF]; POOL];
static mut ICMP_TX: [[u8; ICMP_BUF]; POOL] = [[0; ICMP_BUF]; POOL];
static mut ICMP_RX_META: [[icmp::PacketMetadata; ICMP_PACKETS]; POOL] = [[icmp::PacketMetadata::EMPTY; ICMP_PACKETS]; POOL];
static mut ICMP_TX_META: [[icmp::PacketMetadata; ICMP_PACKETS]; POOL] = [[icmp::PacketMetadata::EMPTY; ICMP_PACKETS]; POOL];
static mut STORAGE0: MaybeUninit<[SocketStorage<'static>; POOL]> = MaybeUninit::uninit();
/// The Ethernet interfaces' sockets: any of the pool, plus the DHCP
/// client, DNS, the DHCP server and the DNS relay.
static mut ETH_STORAGE: MaybeUninit<[[SocketStorage<'static>; POOL + 4]; IFACES - 1]> = MaybeUninit::uninit();

/// Writes `n` values made by `f` from `p` on.
unsafe fn fill<T>(p: *mut T, n: usize, f: impl Fn() -> T) {
    for i in 0..n {
        unsafe { p.add(i).write(f()) };
    }
}

/// Fills the uninitialised tables (once, before anything uses them).
fn init_statics() {
    unsafe {
        fill(core::ptr::addr_of_mut!(UDP_RX_META).cast::<udp::PacketMetadata>(), UDP_PACKETS * POOL, || udp::PacketMetadata::EMPTY);
        fill(core::ptr::addr_of_mut!(UDP_TX_META).cast::<udp::PacketMetadata>(), UDP_PACKETS * POOL, || udp::PacketMetadata::EMPTY);
        fill(core::ptr::addr_of_mut!(STORAGE0).cast::<SocketStorage<'static>>(), POOL, || SocketStorage::EMPTY);
        fill(core::ptr::addr_of_mut!(ETH_STORAGE).cast::<SocketStorage<'static>>(), (POOL + 4) * (IFACES - 1), || SocketStorage::EMPTY);
    }
    filter::init();
    nat::init();
}
static mut DNS_SLOTS: [Option<dns::DnsQuery>; DNS_QUERIES] = [const { None }; DNS_QUERIES];

/// Row `i` of a static table, for pool entry `i` only (each entry's
/// buffers are handed to one socket at a time).
unsafe fn row<T, const N: usize, const M: usize>(table: *mut [[T; N]; M], i: usize) -> &'static mut [T] {
    assert!(i < M);
    unsafe { core::slice::from_raw_parts_mut(table.cast::<T>().add(i * N), N) }
}

fn tcp_socket(i: usize) -> tcp::Socket<'static> {
    unsafe {
        let rx = row(core::ptr::addr_of_mut!(TCP_RX), i);
        let tx = row(core::ptr::addr_of_mut!(TCP_TX), i);
        tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx))
    }
}

fn udp_socket(i: usize) -> udp::Socket<'static> {
    unsafe {
        let rx = udp::PacketBuffer::new(row((*core::ptr::addr_of_mut!(UDP_RX_META)).as_mut_ptr(), i), row(core::ptr::addr_of_mut!(UDP_RX), i));
        let tx = udp::PacketBuffer::new(row((*core::ptr::addr_of_mut!(UDP_TX_META)).as_mut_ptr(), i), row(core::ptr::addr_of_mut!(UDP_TX), i));
        udp::Socket::new(rx, tx)
    }
}

fn icmp_socket(i: usize) -> icmp::Socket<'static> {
    unsafe {
        let rx = icmp::PacketBuffer::new(row(core::ptr::addr_of_mut!(ICMP_RX_META), i), row(core::ptr::addr_of_mut!(ICMP_RX), i));
        let tx = icmp::PacketBuffer::new(row(core::ptr::addr_of_mut!(ICMP_TX_META), i), row(core::ptr::addr_of_mut!(ICMP_TX), i));
        icmp::Socket::new(rx, tx)
    }
}

fn eth_storage(link: usize) -> &'static mut [SocketStorage<'static>] {
    unsafe { row((*core::ptr::addr_of_mut!(ETH_STORAGE)).as_mut_ptr(), link - 1) }
}

// ---- Ethernet links ----

pub struct EthLink {
    pub iface: Interface,
    pub dev: EthNic,
    pub sockets: SocketSet<'static>,
    dhcp: Option<SocketHandle>,
    /// Made when DHCP names DNS servers.
    dns: Option<SocketHandle>,
    /// The default router and the first DNS server DHCP gave.
    pub router: Option<u32>,
    dns_server: Option<u32>,
    /// The DNS server the upstream RA named (RDNSS, doc/46), used when
    /// DHCP gave none (smoltcp keeps one server).
    dns6: Option<u128>,
    /// While it has no address: when to log the counters next.
    report_at: u64,
    /// Its address changed: the interfaces' addresses are to be redone.
    changed: bool,
    /// The global IPv6 address last logged (SLAAC).
    v6_logged: Option<Ipv6Address>,
    /// SLAAC is on: temporary addresses too (RFC 8981, doc/46 §4).
    slaac: bool,
    temp: Option<Temp>,
    /// Earlier temporary addresses: kept (avoided as sources) until then.
    old_temps: [Option<(Ipv6Address, u64)>; OLD_TEMPS],
}

/// A temporary address: the /64 it is in, when to make the next one,
/// when it goes.
#[derive(Clone, Copy)]
struct Temp {
    addr: Ipv6Address,
    prefix: u128,
    renew_at: u64,
    remove_at: u64,
}

const OLD_TEMPS: usize = 2;
const DAY_NS: u64 = 86_400_000_000_000;
/// RFC 8981 §3.8: TEMP_PREFERRED_LIFETIME 1 day, TEMP_VALID_LIFETIME
/// 2 days, MAX_DESYNC_FACTOR 0.4.
const TEMP_PREFERRED_NS: u64 = DAY_NS;
const TEMP_VALID_NS: u64 = 2 * DAY_NS;

/// Random bits from the kernel, else from the clock (doc/46 §4).
fn random_u64() -> u64 {
    t5_rt::random_u64().unwrap_or_else(|_| time_ns().wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

impl EthLink {
    /// `slaac`: take IPv6 addresses and the default route from router
    /// advertisements.
    fn new(link: usize, port: Port, mac: [u8; 6], slaac: bool) -> EthLink {
        let mut dev = EthNic { link, port, mac, own: None, rx: 0, rx_unicast: 0, tx: 0, tx_dropped: 0, queued: true };
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.slaac = slaac;
        config.random_seed = random_u64() ^ u64::from_le_bytes([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0, 0]);
        let mut iface = Interface::new(config, &mut dev, now());
        iface.update_ip_addrs(|a| {
            let _ = a.push(link_local(mac));
        });
        EthLink {
            iface,
            dev,
            sockets: SocketSet::new(eth_storage(link)),
            dhcp: None,
            dns: None,
            router: None,
            dns_server: None,
            dns6: None,
            report_at: clock_ns() + 5_000_000_000,
            changed: true,
            v6_logged: None,
            slaac,
            temp: None,
            old_temps: [None; OLD_TEMPS],
        }
    }

    /// Temporary addresses (RFC 8981): one in the /64 SLAAC gave, made
    /// again every day or so; the earlier ones stay for the connections
    /// that use them but are not chosen as sources (smoltcp's source
    /// selection, rules 3 and 7, doc/46 §4).
    fn privacy(&mut self) {
        if !self.slaac {
            return;
        }
        let now = clock_ns();
        let stable_iid = match link_local(self.dev.mac).address() {
            IpAddress::Ipv6(a) => a.to_bits() & u64::MAX as u128,
            _ => return,
        };
        let stable = self.iface.ip_addrs().iter().find_map(|c| match c {
            IpCidr::Ipv6(c) if !c.address().is_unicast_link_local() && c.address().to_bits() & u64::MAX as u128 == stable_iid => Some(c.address()),
            _ => None,
        });
        let prefix = stable.map(|a| a.to_bits() & !(u64::MAX as u128));
        let made = |l: &EthLink| -> [Option<Ipv6Address>; 1 + OLD_TEMPS] { [l.temp.map(|t| t.addr), l.old_temps[0].map(|o| o.0), l.old_temps[1].map(|o| o.0)] };
        let before = made(self);
        let mut changed = false;
        if let Some(t) = self.temp
            && (prefix != Some(t.prefix) || now >= t.renew_at)
        {
            self.temp = None;
            // Kept only while its prefix is.
            let until = if prefix == Some(t.prefix) { t.remove_at } else { now };
            match self.old_temps.iter_mut().find(|o| o.is_none()) {
                Some(o) => *o = Some((t.addr, until)),
                None => {
                    // The oldest goes now.
                    let oldest = self.old_temps.iter_mut().min_by_key(|o| o.map_or(0, |(_, at)| at)).unwrap();
                    *oldest = Some((t.addr, until));
                }
            }
            changed = true;
        }
        if let Some(p) = prefix
            && self.temp.is_none()
        {
            // A random interface identifier: not zero, not the stable one,
            // not a reserved one (RFC 5453: fdff:ffff:ffff:ff80-ff).
            let mut iid = random_u64() as u128;
            while iid == 0 || iid == stable_iid || iid >> 7 == 0x1FF_FFFF_FFFF_FFFF {
                iid = random_u64() as u128;
            }
            let addr = Ipv6Address::from_bits(p | iid);
            let desync = random_u64() % (TEMP_PREFERRED_NS * 2 / 5);
            self.temp = Some(Temp { addr, prefix: p, renew_at: now + TEMP_PREFERRED_NS - desync, remove_at: now + TEMP_VALID_NS });
            event!(Info, "net.temp_addr", link = LINK_NAMES[self.dev.link], addr = &addr.octets()[..], hours = ((TEMP_PREFERRED_NS - desync) / 3_600_000_000_000) as u32);
            changed = true;
        }
        for o in self.old_temps.iter_mut() {
            if o.is_some_and(|(_, at)| now >= at) {
                *o = None;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let keep = made(self);
        self.iface.update_ip_addrs(|addrs| {
            // Temporary addresses no longer kept go; the new one comes.
            addrs.retain(|c| match c {
                IpCidr::Ipv6(c) => !before.contains(&Some(c.address())) || keep.contains(&Some(c.address())),
                _ => true,
            });
            for a in keep.iter().flatten() {
                let cidr = IpCidr::Ipv6(Ipv6Cidr::new(*a, 64));
                if !addrs.contains(&cidr) {
                    let _ = addrs.push(cidr);
                }
            }
        });
        let avoid: [Ipv6Address; OLD_TEMPS] = core::array::from_fn(|k| self.old_temps[k].map_or(Ipv6Address::UNSPECIFIED, |o| o.0));
        self.iface.set_ipv6_source_preference(self.temp.map(|t| t.addr), &avoid);
        self.changed = true;
    }

    /// Logs the global IPv6 address when SLAAC gives (or changes) it.
    fn log_ipv6(&mut self) {
        self.privacy();
        let global = self.iface.ip_addrs().iter().find_map(|c| match c {
            IpCidr::Ipv6(c) if !c.address().is_unicast_link_local() => Some(c.address()),
            _ => None,
        });
        if global != self.v6_logged {
            self.v6_logged = global;
            if let Some(a) = global {
                event!(Info, "net.ipv6", link = LINK_NAMES[self.dev.link], addr = &a.octets()[..]);
            }
        }
    }

    fn set_addr(&mut self, addr: u32, prefix: u8, router: Option<u32>) {
        self.iface.update_ip_addrs(|a| {
            // IPv6 addresses (SLAAC's among them) stay.
            a.retain(|c| matches!(c, IpCidr::Ipv6(_)));
            let _ = a.push(IpCidr::new(Ipv4Address::from_bits(addr).into(), prefix));
        });
        match router {
            Some(gw) => {
                let _ = self.iface.routes_mut().add_default_ipv4_route(Ipv4Address::from_bits(gw));
            }
            None => {
                self.iface.routes_mut().remove_default_ipv4_route();
            }
        }
        self.dev.own = Some((addr, prefix));
        self.router = router;
        self.changed = true;
    }

    /// The DNS server to ask: DHCP's (IPv4), else the RA's (IPv6).
    pub fn dns_pick(&self) -> Option<IpAddress> {
        self.dns_server.map(|d| IpAddress::Ipv4(Ipv4Address::from_bits(d))).or(self.dns6.map(|d| IpAddress::Ipv6(Ipv6Address::from_bits(d))))
    }

    /// Points the resolver at `dns_pick` (made the first time there is one).
    fn update_dns(&mut self) {
        let Some(server) = self.dns_pick() else { return };
        match self.dns {
            Some(d) => self.sockets.get_mut::<dns::Socket>(d).update_servers(&[server]),
            None => {
                let q = unsafe { &mut *core::ptr::addr_of_mut!(DNS_SLOTS) };
                self.dns = Some(self.sockets.add(dns::Socket::new(&[server], &mut q[..])));
            }
        }
    }

    /// Applies what DHCP says (address, default route, DNS servers).
    fn dhcp(&mut self) {
        let name = LINK_NAMES[self.dev.link];
        self.log_ipv6();
        if self.dev.own.is_none() && clock_ns() >= self.report_at {
            self.report_at = clock_ns() + 5_000_000_000;
            link::log_counters(name, &self.dev);
        }
        let Some(h) = self.dhcp else { return };
        match self.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
            Some(dhcpv4::Event::Configured(c)) => {
                let (addr, prefix) = (c.address.address().to_bits(), c.address.prefix_len());
                let router = c.router.map(|r| r.to_bits());
                event!(Info, "net.dhcp", link = name, addr = addr, prefix = prefix as u32, router = router.unwrap_or(0), dns = c.dns_servers.first().map_or(0, |d| d.to_bits()));
                let dns_server = c.dns_servers.first().map(|d| d.to_bits());
                self.set_addr(addr, prefix, router);
                self.dns_server = dns_server;
                self.update_dns();
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.iface.update_ip_addrs(|a| a.retain(|c| matches!(c, IpCidr::Ipv6(_))));
                self.iface.routes_mut().remove_default_ipv4_route();
                self.dev.own = None;
                self.router = None;
                self.changed = true;
                event!(Info, "net.dhcp_lost", link = name);
            }
            None => {}
        }
    }
}

/// Opens `netdev:wlan0` once the Wi-Fi service has told the kernel the MAC
/// address (it has joined or is joining), waiting up to 2 s; `None` if the
/// chip is not up yet (the caller tries again, doc/30 §5).
fn open_wifi() -> Option<EthLink> {
    let h = Handle::open(netdev::WLAN0, rights::READ | rights::WRITE | rights::WAIT | rights::CTL).ok()?;
    let mut mac = None;
    for _ in 0..20 {
        if let (Ok(lo), Ok(hi)) = (h.ctl(netdev::MAC_LO, 0), h.ctl(netdev::MAC_HI, 0)) {
            let b = (lo as u64 | (hi as u64) << 32).to_le_bytes();
            mac = Some([b[0], b[1], b[2], b[3], b[4], b[5]]);
            break;
        }
        sleep_ms(100);
    }
    let Some(mac) = mac else {
        event!(Warn, "net.no_wifi_mac");
        return None;
    };
    let mut l = EthLink::new(WLAN, Port::open(h), mac, true);
    l.dhcp = Some(l.sockets.add(dhcpv4::Socket::new()));
    event!(Info, "net.link", link = "wlan0", addr = "dhcp", mtu = netdev::MTU as u32);
    Some(l)
}

// ---- client sockets ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tcp,
    Udp,
    /// One echo request and its reply.
    Ping,
    /// One name lookup on the Wi-Fi link's DNS socket (shared).
    Dns,
}

/// One smoltcp socket and its pool entry.
#[derive(Clone, Copy)]
struct Sock {
    handle: SocketHandle,
    pool: usize,
}

#[derive(Clone, Copy)]
struct Slot {
    kind: Kind,
    /// The client that owns it; None once closed, until the stack is done.
    owner: Option<usize>,
    /// Its smoltcp socket on each interface (several only while listening
    /// or bound on every link).
    socks: [Option<Sock>; IFACES],
    /// The interface that sends and receives.
    primary: usize,
    /// Ping: when the request went out (ns).
    sent_ns: u64,
    /// Dns: the lookup, until its result is taken.
    query: Option<dns::QueryHandle>,
    /// Dns: an AAAA lookup.
    v6: bool,
}

/// The echo identifier of client socket `i`.
fn ping_ident(i: usize) -> u16 {
    0x7450 + i as u16
}

pub struct Stack {
    iface: Interface,
    pub dev: Links,
    sockets: SocketSet<'static>,
    /// The Ethernet links (`WLAN`, `TEST`), at index link - 1.
    pub eth: [Option<EthLink>; IFACES - 1],
    peer: Option<testlan::Peer>,
    slots: [Option<Slot>; SOCKETS],
    pool_used: [bool; POOL],
    next_port: u16,
    pub fwd: Forwarder,
    pub v6: fwd6::Fwd6,
    /// The DHCP server, and the rule set generation it was made from.
    dhcpd: Option<dhcpd::Server>,
    /// The DNS relay, with the DHCP server (doc/29).
    relay: Option<dnsrelay::Relay>,
    rules_gen: u32,
    /// An IPv6 address for the reply being made (doc/26 §4).
    reply_addr: Option<[u8; 16]>,
    /// Clients whose UDP_WAIT is unanswered (doc/49 §18).
    udp_waiting: [bool; CLIENTS],
}

impl Stack {
    /// Runs the interfaces, the test LAN's peer and forwarding until
    /// nothing moves (a forwarded packet may need another round: ARP, the
    /// peer's answer).
    fn poll(&mut self) {
        // The time is read after each `wait` and before the next one's
        // timeout (`tick`), not here: requests make this run several times
        // per wake-up (doc/34 §10).
        for _ in 0..8 {
            self.sync_own6();
            let mut moved = false;
            for _ in 0..256 {
                let r = self.iface.poll(now(), &mut self.dev, &mut self.sockets);
                if r == PollResult::None && Links::loopback_idle() {
                    break;
                }
                moved = true;
            }
            for e in self.eth.iter_mut().flatten() {
                for _ in 0..64 {
                    if e.iface.poll(now(), &mut e.dev, &mut e.sockets) == PollResult::None {
                        break;
                    }
                    moved = true;
                }
                e.dhcp();
            }
            if self.eth.iter().flatten().any(|e| e.changed) {
                self.sync_addrs();
            }
            self.serve_dhcp();
            if let Some(p) = &mut self.peer {
                p.poll();
            }
            let busy = !fwd::inbox().is_empty() || !link::to_peer().is_empty() || !link::from_peer().is_empty();
            self.forward();
            self.after_forward6();
            if !moved && !busy {
                break;
            }
        }
        self.sweep();
    }

    /// Runs the DHCP server the rule set asks for (made again when the set
    /// changes).
    fn serve_dhcp(&mut self) {
        let (rules, generation) = filter::active();
        if generation != self.rules_gen {
            self.rules_gen = generation;
            if let Some(old) = self.dhcpd.take()
                && let Some(e) = self.eth[old.cfg.link as usize - 1].as_mut()
            {
                old.remove(&mut e.sockets);
            }
            if let Some(old) = self.relay.take()
                && let Some((down, up)) = self.down_up(old.link as usize)
            {
                old.remove(&mut down.sockets, &mut up.sockets);
            }
            if let Some(cfg) = rules.dhcp
                && cfg.link as usize != IFACE0
                && let Some(e) = self.eth[cfg.link as usize - 1].as_mut()
            {
                self.dhcpd = Some(dhcpd::Server::new(cfg, &mut e.sockets));
                if let Some((down, up)) = self.down_up(cfg.link as usize) {
                    self.relay = Some(dnsrelay::Relay::new(cfg.link, &mut down.sockets, &mut up.sockets));
                }
            }
        }
        let d6 = self.dns6();
        if let Some(w) = self.eth[WLAN - 1].as_mut()
            && w.dns6 != d6
        {
            w.dns6 = d6;
            w.update_dns();
        }
        let dns = self.eth[WLAN - 1].as_ref().and_then(|w| w.dns_pick());
        // DHCP hands out IPv4 servers only.
        let dns4 = self.eth[WLAN - 1].as_ref().and_then(|w| w.dns_server);
        if let Some(r) = &mut self.relay
            && let (Some(w), Some(d)) = {
                let (a, b) = self.eth.split_at_mut(1);
                (a[0].as_mut(), b[0].as_mut())
            }
        {
            r.poll(&mut d.sockets, &mut w.sockets, dns);
        }
        let relaying = self.relay.is_some();
        let Some(srv) = &mut self.dhcpd else { return };
        let Some(e) = self.eth[srv.cfg.link as usize - 1].as_mut() else { return };
        let Some(own) = e.dev.own else { return };
        // The relay answers for names on the link itself.
        let given = if relaying { own.0 } else { dns4.unwrap_or(own.0) };
        srv.poll(&mut e.sockets, own, given);
    }

    /// The downstream link `down` and Wi-Fi (the DNS relay's two sides),
    /// if both are there and differ.
    fn down_up(&mut self, down: usize) -> Option<(&mut EthLink, &mut EthLink)> {
        if down != TEST {
            return None;
        }
        let (a, b) = self.eth.split_at_mut(1);
        Some((b[0].as_mut()?, a[0].as_mut()?))
    }

    /// Gives each interface, besides its own addresses, the Tab5's
    /// addresses on the other links (/32): a packet to any of them is
    /// taken by the link it arrives on and answered there (doc/24 §5).
    fn sync_addrs(&mut self) {
        let own: [Option<(u32, u8)>; IFACES] = core::array::from_fn(|k| self.link_addr(k));
        fwd::set_own(core::array::from_fn(|k| own[k].map(|(a, _)| a)));
        let cidr = |a: u32, len: u8| IpCidr::new(Ipv4Address::from_bits(a).into(), len);
        let others = move |k: usize| (0..IFACES).filter(move |&j| j != k).filter_map(move |j| own[j].map(|(a, _)| a));
        let has_host = self.dev.host.is_some();
        self.iface.update_ip_addrs(|a| {
            // IPv6 addresses (SLAAC's among them) stay.
            a.retain(|c| matches!(c, IpCidr::Ipv6(_)));
            let _ = a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
            if has_host {
                let _ = a.push(cidr(HOST_ADDR, HOST_PREFIX));
            }
            for x in others(IFACE0) {
                let _ = a.push(cidr(x, 32));
            }
        });
        for e in self.eth.iter_mut().flatten() {
            let k = e.dev.link;
            e.changed = false;
            e.iface.update_ip_addrs(|a| {
                // IPv6 addresses (SLAAC's among them) stay.
            a.retain(|c| matches!(c, IpCidr::Ipv6(_)));
                if let Some((p, len)) = own[k] {
                    let _ = a.push(cidr(p, len));
                    for x in others(k) {
                        let _ = a.push(cidr(x, 32));
                    }
                }
            });
        }
    }

    /// The earliest time an interface wants to be polled again.
    fn poll_delay(&mut self) -> Option<smoltcp::time::Duration> {
        let d0 = self.iface.poll_delay(now(), &self.sockets);
        let mut delay = d0;
        let mut each = [u32::MAX; 1 + IFACES];
        let us = |d: Option<smoltcp::time::Duration>| d.map_or(u32::MAX, |d| d.total_micros().min(u32::MAX as u64 - 1) as u32);
        each[0] = us(d0);
        let mut min = |d: Option<smoltcp::time::Duration>| {
            if let Some(d) = d {
                delay = Some(delay.map_or(d, |e| e.min(d)));
            }
        };
        for (k, e) in self.eth.iter_mut().enumerate() {
            if let Some(e) = e {
                let d = e.iface.poll_delay(now(), &e.sockets);
                each[1 + k] = us(d);
                min(d);
            }
        }
        if let Some(p) = &mut self.peer {
            min(p.poll_delay());
        }
        // Diagnosis (doc/34): which interface keeps the loop from waiting.
        static mut ZERO_STREAK: u32 = 0;
        let streak = unsafe { &mut *core::ptr::addr_of_mut!(ZERO_STREAK) };
        if delay.is_some_and(|d| d.total_micros() == 0) {
            *streak += 1;
            if *streak == 1000 {
                event!(Warn, "net.busy_poll", iface0_us = each[0], wlan_us = each[1], test_us = each[2]);
            }
        } else {
            *streak = 0;
        }
        delay
    }

    /// Closed sockets the stack has finished with go back to the pool.
    fn sweep(&mut self) {
        for i in 0..SOCKETS {
            let Some(slot) = self.slots[i] else { continue };
            if slot.owner.is_some() {
                continue;
            }
            for k in 0..IFACES {
                let Some(s) = slot.socks[k] else { continue };
                let done = match slot.kind {
                    Kind::Tcp => self.set(k).is_none_or(|set| matches!(set.get::<tcp::Socket>(s.handle).state(), tcp::State::Closed | tcp::State::TimeWait)),
                    _ => true,
                };
                if done {
                    self.drop_sock(k, s);
                    self.slots[i].as_mut().unwrap().socks[k] = None;
                }
            }
            if self.slots[i].unwrap().socks.iter().all(Option::is_none) {
                self.slots[i] = None;
            }
        }
    }

    fn set(&mut self, iface: usize) -> Option<&mut SocketSet<'static>> {
        match iface {
            IFACE0 => Some(&mut self.sockets),
            _ => self.eth[iface - 1].as_mut().map(|e| &mut e.sockets),
        }
    }

    fn iface_set(&mut self, iface: usize) -> Option<(&mut Interface, &mut SocketSet<'static>)> {
        match iface {
            IFACE0 => Some((&mut self.iface, &mut self.sockets)),
            _ => self.eth[iface - 1].as_mut().map(|e| (&mut e.iface, &mut e.sockets)),
        }
    }

    /// A new smoltcp socket of `kind` on interface `iface`.
    fn new_sock(&mut self, iface: usize, kind: Kind) -> Result<Sock, u32> {
        let pool = self.pool_used.iter().position(|u| !u).ok_or(status::NO_SOCKETS)?;
        let set = self.set(iface).ok_or(status::NO_ADDRESS)?;
        let handle = match kind {
            Kind::Tcp => set.add(tcp_socket(pool)),
            Kind::Udp => set.add(udp_socket(pool)),
            Kind::Ping => set.add(icmp_socket(pool)),
            Kind::Dns => return Err(status::BAD_REQUEST),
        };
        self.pool_used[pool] = true;
        Ok(Sock { handle, pool })
    }

    fn drop_sock(&mut self, iface: usize, s: Sock) {
        if s.pool == SHARED {
            return;
        }
        if let Some(set) = self.set(iface) {
            set.remove(s.handle);
        }
        self.pool_used[s.pool] = false;
    }

    /// Drops every socket of client socket `i` and frees it (a request
    /// failed half way).
    fn unalloc(&mut self, i: usize) {
        if let Some(slot) = self.slots[i].take() {
            for k in 0..IFACES {
                if let Some(s) = slot.socks[k] {
                    self.drop_sock(k, s);
                }
            }
        }
    }

    fn new_slot(&mut self, client: usize, kind: Kind) -> Result<usize, u32> {
        let i = self.slots.iter().position(Option::is_none).ok_or(status::NO_SOCKETS)?;
        self.slots[i] = Some(Slot { kind, owner: Some(client), socks: [None; IFACES], primary: IFACE0, sent_ns: 0, query: None, v6: false });
        Ok(i)
    }

    /// Client socket `i` gets `s` on `iface` as its primary socket.
    fn attach(&mut self, i: usize, iface: usize, s: Sock) {
        let slot = self.slots[i].as_mut().unwrap();
        slot.socks[iface] = Some(s);
        slot.primary = iface;
    }

    /// A new client socket of `kind` with one smoltcp socket on `iface`.
    fn open_one(&mut self, client: usize, kind: Kind, iface: usize) -> Result<(usize, Sock), u32> {
        let i = self.new_slot(client, kind)?;
        match self.new_sock(iface, kind) {
            Ok(s) => {
                self.attach(i, iface, s);
                Ok((i, s))
            }
            Err(e) => {
                self.unalloc(i);
                Err(e)
            }
        }
    }

    /// The interface a packet to `dst` leaves by.
    fn out_iface(&self, dst: IpAddress) -> Option<usize> {
        match dst {
            IpAddress::Ipv4(a) if a.octets()[0] == 127 => Some(IFACE0),
            IpAddress::Ipv4(a) => self.route(a.to_bits()).map(|r| r.link),
            IpAddress::Ipv6(a) => self.route6(&a),
        }
    }

    /// IPv6 (doc/26 §2): ::1 and the host link's network stay on interface
    /// 0, the test LAN's network goes there, the rest to Wi-Fi (its routes
    /// come from router advertisements). No IPv6 forwarding yet.
    fn route6(&self, a: &Ipv6Address) -> Option<usize> {
        let present = |k: usize| self.eth[k - 1].is_some();
        if a.is_loopback() || same64(a, &HOST_NET6) && self.dev.host.is_some() {
            Some(IFACE0)
        } else if same64(a, &TEST_NET6) && present(TEST) {
            Some(TEST)
        } else if present(WLAN) {
            Some(WLAN)
        } else if present(TEST) && a.is_unicast_link_local() {
            Some(TEST)
        } else {
            None
        }
    }

    /// Whether interface `k` has address `a`.
    fn has_addr(&self, k: usize, a: IpAddress) -> bool {
        match k {
            IFACE0 => self.iface.has_ip_addr(a),
            _ => self.eth[k - 1].as_ref().is_some_and(|e| e.iface.has_ip_addr(a)),
        }
    }

    /// Where TCP_LISTEN or UDP_BIND puts its sockets: for each interface,
    /// None (not there) or the local address to accept (None = any).
    fn bind_targets(&self, addr: Option<IpAddress>, v6: bool, link: u8) -> Result<[Option<Option<IpAddress>>; IFACES], u32> {
        let mut t = [None; IFACES];
        let present = |k: usize| k == IFACE0 || self.eth[k - 1].is_some();
        if let Some(addr) = addr {
            let k = match addr {
                IpAddress::Ipv4(a) if a.octets()[0] == 127 => IFACE0,
                IpAddress::Ipv6(a) if a.is_loopback() => IFACE0,
                _ => (0..IFACES).find(|&k| self.has_addr(k, addr)).ok_or(status::NO_ADDRESS)?,
            };
            t[k] = Some(Some(addr));
            return Ok(t);
        }
        let v4 = |a: u32| Some(Some(IpAddress::Ipv4(Ipv4Address::from_bits(a))));
        let v6a = |a: Ipv6Address| Some(Some(IpAddress::Ipv6(a)));
        match link {
            proto::link::ANY => {
                for (k, slot) in t.iter_mut().enumerate() {
                    if present(k) {
                        *slot = Some(None);
                    }
                }
            }
            proto::link::LO => t[IFACE0] = if v6 { v6a(Ipv6Address::LOCALHOST) } else { v4(0x7F00_0001) },
            proto::link::HOST if self.dev.host.is_some() => t[IFACE0] = if v6 { v6a(in64(HOST_NET6, 2)) } else { v4(HOST_ADDR) },
            proto::link::WLAN0 if present(WLAN) => t[WLAN] = Some(None),
            proto::link::TEST0 if present(TEST) => t[TEST] = Some(None),
            _ => return Err(status::NO_ADDRESS),
        }
        Ok(t)
    }

    /// The primary socket of a client's socket, after settling a listener
    /// on several links: the one a connection came to stays, the others go.
    fn primary(&mut self, client: usize, sock: u16, kind: Kind) -> Result<(usize, SocketHandle), u32> {
        let i = sock as usize;
        let slot = match self.slots.get(i) {
            Some(Some(s)) if s.owner == Some(client) && s.kind == kind => *s,
            _ => return Err(status::BAD_REQUEST),
        };
        if kind == Kind::Tcp && slot.socks.iter().flatten().count() > 1 {
            let taken = (0..IFACES).find(|&k| {
                slot.socks[k].is_some_and(|s| self.set(k).is_some_and(|set| !matches!(set.get::<tcp::Socket>(s.handle).state(), tcp::State::Listen | tcp::State::Closed)))
            });
            if let Some(k) = taken {
                for j in (0..IFACES).filter(|&j| j != k) {
                    if let Some(s) = slot.socks[j] {
                        self.drop_sock(j, s);
                        self.slots[i].as_mut().unwrap().socks[j] = None;
                    }
                }
                self.slots[i].as_mut().unwrap().primary = k;
                return Ok((k, slot.socks[k].unwrap().handle));
            }
        }
        let p = slot.primary;
        slot.socks[p].map(|s| (p, s.handle)).ok_or(status::BAD_REQUEST)
    }

    /// Whether a client's socket on `iface` already listens on (TCP) or is
    /// bound to (UDP) `port` at an address overlapping `local`. smoltcp
    /// lets two listeners share a port, and the first one takes every
    /// connection.
    fn in_use(&mut self, iface: usize, kind: Kind, local: Option<IpAddress>, port: u16) -> bool {
        let slots = self.slots;
        let Some(set) = self.set(iface) else { return false };
        port != 0
            && slots.iter().flatten().filter(|s| s.owner.is_some() && s.kind == kind).filter_map(|s| s.socks[iface]).any(|s| {
                let ep = match kind {
                    Kind::Tcp => Some(set.get::<tcp::Socket>(s.handle)).filter(|t| t.is_listening()).map(|t| t.listen_endpoint()),
                    Kind::Udp => Some(set.get::<udp::Socket>(s.handle)).filter(|u| u.is_open()).map(|u| u.endpoint()),
                    _ => None,
                };
                ep.is_some_and(|ep| ep.port == port && (ep.addr.is_none() || local.is_none() || ep.addr == local))
            })
    }

    fn ephemeral(&mut self) -> u16 {
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        self.next_port
    }

    /// TCP_LISTEN and UDP_BIND.
    fn bind(&mut self, client: usize, kind: Kind, addr: Option<IpAddress>, v6: bool, b: u32) -> Result<(u32, u32), u32> {
        let (port, link) = (b as u16, (b >> 16) as u8);
        let targets = self.bind_targets(addr, v6, link)?;
        if targets.iter().enumerate().any(|(k, t)| t.is_some_and(|local| self.in_use(k, kind, local, port))) {
            return Err(status::ADDR_IN_USE);
        }
        let i = self.new_slot(client, kind)?;
        let mut first = None;
        for (k, t) in targets.iter().enumerate() {
            let Some(local) = *t else { continue };
            let s = match self.new_sock(k, kind) {
                Ok(s) => s,
                Err(e) => {
                    self.unalloc(i);
                    return Err(e);
                }
            };
            self.slots[i].as_mut().unwrap().socks[k] = Some(s);
            let ep = IpListenEndpoint { addr: local, port };
            let set = self.set(k).unwrap();
            let ok = match kind {
                Kind::Tcp => set.get_mut::<tcp::Socket>(s.handle).listen(ep).is_ok(),
                _ => set.get_mut::<udp::Socket>(s.handle).bind(ep).is_ok(),
            };
            if !ok {
                self.unalloc(i);
                return Err(status::ADDR_IN_USE);
            }
            first.get_or_insert(k);
        }
        self.slots[i].as_mut().unwrap().primary = first.unwrap_or(IFACE0);
        Ok((i as u32, 0))
    }

    fn handle(&mut self, client: usize, mem: &mut [u8], m: &[u8]) -> Result<(u32, u32), u32> {
        if m.len() < proto::REQUEST_SIZE {
            return Err(status::BAD_REQUEST);
        }
        let code = m[0];
        let sock = u16::from_le_bytes([m[2], m[3]]);
        let b = u32::from_le_bytes([m[8], m[9], m[10], m[11]]);
        // The address: IPv4 in `a`, or IPv6 after the request.
        let v6 = m[1] == proto::FAMILY_V6;
        let a = if v6 {
            let o: [u8; 16] = m.get(proto::REQUEST_SIZE..proto::REQUEST_SIZE_V6).ok_or(status::BAD_REQUEST)?.try_into().unwrap();
            IpAddress::Ipv6(Ipv6Address::from(o))
        } else {
            IpAddress::Ipv4(Ipv4Address::from_bits(u32::from_le_bytes([m[4], m[5], m[6], m[7]])))
        };
        let local = (!a.is_unspecified()).then_some(a);
        match code {
            op::TCP_LISTEN => self.bind(client, Kind::Tcp, local, v6, b),
            op::UDP_BIND => self.bind(client, Kind::Udp, local, v6, b),
            op::TCP_CONNECT => {
                let k = self.out_iface(a).ok_or(status::UNREACHABLE)?;
                let (i, s) = self.open_one(client, Kind::Tcp, k)?;
                let port = self.ephemeral();
                let (iface, set) = self.iface_set(k).unwrap();
                if set.get_mut::<tcp::Socket>(s.handle).connect(iface.context(), (a, b as u16), port).is_err() {
                    self.unalloc(i);
                    return Err(status::UNREACHABLE);
                }
                Ok((i as u32, 0))
            }
            op::TCP_STATE => {
                let (k, h) = self.primary(client, sock, Kind::Tcp)?;
                let s = self.set(k).unwrap().get::<tcp::Socket>(h);
                let st = match s.state() {
                    tcp::State::Closed | tcp::State::TimeWait => state::CLOSED,
                    tcp::State::Listen => state::LISTEN,
                    tcp::State::SynSent | tcp::State::SynReceived => state::CONNECTING,
                    tcp::State::Established => state::ESTABLISHED,
                    _ => state::CLOSING,
                };
                Ok((st, s.recv_queue() as u32))
            }
            op::SEND => {
                let (k, h) = self.primary(client, sock, Kind::Tcp)?;
                let s = self.set(k).unwrap().get_mut::<tcp::Socket>(h);
                let n = (b as usize).min(mem.len());
                if !s.may_send() {
                    return Err(status::NOT_CONNECTED);
                }
                s.send_slice(&mem[..n]).map(|k| (k as u32, 0)).map_err(|_| status::NOT_CONNECTED)
            }
            op::RECV => {
                let (k, h) = self.primary(client, sock, Kind::Tcp)?;
                let s = self.set(k).unwrap().get_mut::<tcp::Socket>(h);
                let n = (b as usize).min(mem.len());
                if s.can_recv() {
                    return s.recv_slice(&mut mem[..n]).map(|k| (k as u32, 0)).map_err(|_| status::NOT_CONNECTED);
                }
                // Nothing now; tell whether more can ever come.
                let ended = !s.may_recv() && !matches!(s.state(), tcp::State::SynSent | tcp::State::SynReceived | tcp::State::Listen);
                Ok((0, ended as u32))
            }
            op::CLOSE => {
                let i = sock as usize;
                let slot = match self.slots.get(i) {
                    Some(Some(s)) if s.owner == Some(client) => *s,
                    _ => return Err(status::BAD_REQUEST),
                };
                self.release(i, slot, false);
                Ok((0, 0))
            }
            op::UDP_SEND => {
                let (k0, _) = self.primary(client, sock, Kind::Udp)?;
                let slot = self.slots[sock as usize].unwrap();
                // The bound socket on the link the destination is on.
                let k = self.out_iface(a).filter(|&k| slot.socks[k].is_some()).unwrap_or(k0);
                let h = slot.socks[k].unwrap().handle;
                let s = self.set(k).unwrap().get_mut::<udp::Socket>(h);
                let (port, n) = ((b >> 16) as u16, (b & 0xFFFF) as usize);
                let n = n.min(mem.len());
                match s.send_slice(&mem[..n], IpEndpoint::new(a, port)) {
                    Ok(()) => Ok((n as u32, 0)),
                    Err(udp::SendError::BufferFull) => Ok((0, 0)),
                    Err(_) => Err(status::UNREACHABLE),
                }
            }
            op::LINK_RENEW => {
                // Only Wi-Fi changes networks under the stack.
                if b != proto::link::WLAN0 as u32 {
                    return Err(status::BAD_REQUEST);
                }
                let w = self.eth[WLAN - 1].as_mut().ok_or(status::NO_ADDRESS)?;
                let h = w.dhcp.ok_or(status::NO_ADDRESS)?;
                w.sockets.get_mut::<dhcpv4::Socket>(h).reset();
                event!(Info, "net.link_renew", link = "wlan0");
                Ok((0, 0))
            }
            op::UDP_WAIT => {
                if self.udp_ready(client) {
                    return Ok((1, 0));
                }
                self.udp_waiting[client] = true;
                Err(DEFERRED)
            }
            op::UDP_RECV => {
                self.primary(client, sock, Kind::Udp)?;
                let slot = self.slots[sock as usize].unwrap();
                for k in 0..IFACES {
                    let Some(s) = slot.socks[k] else { continue };
                    let Some(set) = self.set(k) else { continue };
                    if let Ok((n, meta)) = set.get_mut::<udp::Socket>(s.handle).recv_slice(&mut mem[proto::UDP_HEADER..]) {
                        mem[..2].copy_from_slice(&meta.endpoint.port.to_le_bytes());
                        return Ok((n as u32, self.reply_with(meta.endpoint.addr)));
                    }
                }
                Ok((0, 0))
            }
            op::PING => {
                let n = (b as usize).min(PING_DATA).min(mem.len());
                let k = self.out_iface(a).ok_or(status::UNREACHABLE)?;
                let (i, s) = self.open_one(client, Kind::Ping, k)?;
                let ident = ping_ident(i);
                let sk = self.set(k).unwrap().get_mut::<icmp::Socket>(s.handle);
                let data = &mem[..n];
                // smoltcp fills in the ICMPv6 checksum (it knows the source).
                let sent = sk.bind(icmp::Endpoint::Ident(ident)).is_ok()
                    && match a {
                        IpAddress::Ipv4(_) => {
                            let repr = Icmpv4Repr::EchoRequest { ident, seq_no: 0, data };
                            sk.send(repr.buffer_len(), a).map(|buf| repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &Default::default())).is_ok()
                        }
                        IpAddress::Ipv6(dst) => {
                            let repr = Icmpv6Repr::EchoRequest { ident, seq_no: 0, data };
                            sk.send(repr.buffer_len(), a)
                                .map(|buf| repr.emit(&Ipv6Address::UNSPECIFIED, &dst, &mut Icmpv6Packet::new_unchecked(buf), &Default::default()))
                                .is_ok()
                        }
                    };
                if !sent {
                    self.unalloc(i);
                    return Err(status::UNREACHABLE);
                }
                self.slots[i].as_mut().unwrap().sent_ns = clock_ns();
                Ok((i as u32, 0))
            }
            op::PING_RESULT => {
                let (k, h) = self.primary(client, sock, Kind::Ping)?;
                let sent_ns = self.slots[sock as usize].unwrap().sent_ns;
                let s = self.set(k).unwrap().get_mut::<icmp::Socket>(h);
                while let Ok((buf, from)) = s.recv() {
                    // The type (0: echo reply, 129: ICMPv6 echo reply) and
                    // the identifier.
                    let reply = match from {
                        IpAddress::Ipv4(_) => 0,
                        IpAddress::Ipv6(_) => 129,
                    };
                    if buf.len() >= 8 && buf[0] == reply && u16::from_be_bytes([buf[4], buf[5]]) == ping_ident(sock as usize) {
                        return Ok((1, ((clock_ns() - sent_ns) / 1000) as u32));
                    }
                }
                Ok((0, 0))
            }
            op::RESOLVE => {
                let n = (b as usize).min(proto::NAME_MAX).min(mem.len());
                let name = core::str::from_utf8(&mem[..n]).map_err(|_| status::BAD_REQUEST)?;
                let w = self.eth[WLAN - 1].as_mut().ok_or(status::UNREACHABLE)?;
                let dns = w.dns.ok_or(status::UNREACHABLE)?;
                let q = w
                    .sockets
                    .get_mut::<dns::Socket>(dns)
                    .start_query(w.iface.context(), name, if v6 { DnsQueryType::Aaaa } else { DnsQueryType::A })
                    .map_err(|e| if e == dns::StartQueryError::NoFreeSlot { status::NO_SOCKETS } else { status::BAD_REQUEST })?;
                let i = match self.new_slot(client, Kind::Dns) {
                    Ok(i) => i,
                    Err(e) => {
                        self.eth[WLAN - 1].as_mut().unwrap().sockets.get_mut::<dns::Socket>(dns).cancel_query(q);
                        return Err(e);
                    }
                };
                self.attach(i, WLAN, Sock { handle: dns, pool: SHARED });
                let slot = self.slots[i].as_mut().unwrap();
                slot.query = Some(q);
                slot.v6 = v6;
                Ok((i as u32, 0))
            }
            op::RESOLVE_RESULT => {
                let (_, h) = self.primary(client, sock, Kind::Dns)?;
                let w = self.eth[WLAN - 1].as_mut().ok_or(status::UNREACHABLE)?;
                let slot = self.slots[sock as usize].as_mut().unwrap();
                let Some(q) = slot.query else { return Err(status::BAD_REQUEST) };
                match w.sockets.get_mut::<dns::Socket>(h).get_query_result(q) {
                    Ok(addrs) => {
                        slot.query = None;
                        let want6 = slot.v6;
                        let ip = addrs.iter().copied().find(|a| matches!(a, IpAddress::Ipv6(_)) == want6).ok_or(status::UNREACHABLE)?;
                        Ok((1, self.reply_with(ip)))
                    }
                    Err(dns::GetQueryResultError::Pending) => Ok((0, 0)),
                    Err(dns::GetQueryResultError::Failed) => {
                        slot.query = None;
                        Err(status::UNREACHABLE)
                    }
                }
            }
            op::IF_INFO => self.if_info(u32::from_le_bytes([m[4], m[5], m[6], m[7]]), mem),
            // The interfaces the socket is bound on join the group (MLD or
            // IGMP reports go out on the next poll); groups stay joined
            // (doc/49: mDNS for Matter).
            op::UDP_JOIN => {
                self.primary(client, sock, Kind::Udp)?;
                if !a.is_multicast() {
                    return Err(status::BAD_REQUEST);
                }
                let slot = self.slots[sock as usize].unwrap();
                for k in 0..IFACES {
                    if slot.socks[k].is_none() {
                        continue;
                    }
                    let Some((iface, _)) = self.iface_set(k) else { continue };
                    if iface.join_multicast_group(a).is_err() {
                        return Err(status::NO_SOCKETS);
                    }
                }
                let mut g = [0u8; 16];
                let n = match a {
                    IpAddress::Ipv4(v4) => {
                        g[..4].copy_from_slice(&v4.octets());
                        4
                    }
                    IpAddress::Ipv6(v6) => {
                        g.copy_from_slice(&v6.octets());
                        16
                    }
                };
                event!(Info, "net.joined", group = &g[..n], sock = sock as u32);
                Ok((0, 0))
            }
            _ => Err(status::BAD_REQUEST),
        }
    }

    /// IF_INFO (doc/41): interface `k`'s record in `mem`.
    fn if_info(&mut self, k: u32, mem: &mut [u8]) -> Result<(u32, u32), u32> {
        let k = k as usize;
        if k >= IFACES {
            return Err(status::NO_ADDRESS);
        }
        let (iface, mac, router, dns, rx, tx) = match k {
            IFACE0 => {
                let Some(_) = self.dev.host else { return Ok((0, 0)) };
                (&self.iface, [0; 6], None, None, self.dev.from_host, self.dev.to_host)
            }
            _ => {
                let Some(e) = self.eth[k - 1].as_ref() else { return Ok((0, 0)) };
                (&e.iface, e.dev.mac, e.router, e.dns_server, e.dev.rx, e.dev.tx)
            }
        };
        let r = mem.get_mut(..iface::RECORD_MAX).ok_or(status::BAD_REQUEST)?;
        r.fill(0);
        let name = LINK_NAMES[k].as_bytes();
        r[..name.len()].copy_from_slice(name);
        r[8..14].copy_from_slice(&mac);
        let mut n6 = 0;
        for c in iface.ip_addrs() {
            match c {
                IpCidr::Ipv4(c) if r[14] == 0 => {
                    r[14] = c.prefix_len();
                    r[16..20].copy_from_slice(&c.address().to_bits().to_le_bytes());
                }
                IpCidr::Ipv6(c) if n6 < iface::V6_MAX => {
                    let o = iface::HEADER + n6 * iface::V6_ENTRY;
                    r[o..o + 16].copy_from_slice(&c.address().octets());
                    r[o + 16] = c.prefix_len();
                    n6 += 1;
                }
                _ => {}
            }
        }
        r[15] = n6 as u8;
        r[20..24].copy_from_slice(&router.unwrap_or(0).to_le_bytes());
        r[24..28].copy_from_slice(&dns.unwrap_or(0).to_le_bytes());
        r[28..32].copy_from_slice(&rx.to_le_bytes());
        r[32..36].copy_from_slice(&tx.to_le_bytes());
        Ok((1, (iface::HEADER + n6 * iface::V6_ENTRY) as u32))
    }

    /// An address for the reply: IPv4 as the value, IPv6 after the reply.
    fn reply_with(&mut self, a: IpAddress) -> u32 {
        match a {
            IpAddress::Ipv4(v) => v.to_bits(),
            IpAddress::Ipv6(v) => {
                self.reply_addr = Some(v.octets());
                0
            }
        }
    }

    /// CLOSE (`abort` false) or the client went away (true): listeners and
    /// everything but TCP connections go now; connections close (or abort)
    /// and go once the stack is done with them (`sweep`).
    fn release(&mut self, i: usize, slot: Slot, abort: bool) {
        for k in 0..IFACES {
            let Some(s) = slot.socks[k] else { continue };
            let keep = match (slot.kind, self.set(k)) {
                (Kind::Tcp, Some(set)) => {
                    let t = set.get_mut::<tcp::Socket>(s.handle);
                    if matches!(t.state(), tcp::State::Listen | tcp::State::Closed) {
                        false
                    } else {
                        if abort {
                            t.abort()
                        } else {
                            t.close()
                        }
                        true
                    }
                }
                (Kind::Dns, Some(set)) => {
                    if let Some(q) = slot.query {
                        set.get_mut::<dns::Socket>(s.handle).cancel_query(q);
                    }
                    false
                }
                _ => false,
            };
            if !keep {
                self.drop_sock(k, s);
                self.slots[i].as_mut().unwrap().socks[k] = None;
            }
        }
        let s = self.slots[i].as_mut().unwrap();
        s.owner = None;
        s.query = None;
        if s.socks.iter().all(Option::is_none) {
            self.slots[i] = None;
        }
    }

    /// A client left: abort its sockets.
    /// Whether a datagram waits on one of `client`'s UDP sockets.
    fn udp_ready(&mut self, client: usize) -> bool {
        for i in 0..SOCKETS {
            let Some(slot) = self.slots[i] else { continue };
            if slot.owner != Some(client) || slot.kind != Kind::Udp {
                continue;
            }
            for k in 0..IFACES {
                if let Some(sk) = slot.socks[k]
                    && let Some(set) = self.set(k)
                    && set.get::<udp::Socket>(sk.handle).can_recv()
                {
                    return true;
                }
            }
        }
        false
    }

    fn drop_client(&mut self, client: usize) {
        self.udp_waiting[client] = false;
        for i in 0..SOCKETS {
            if let Some(s) = self.slots[i]
                && s.owner == Some(client)
            {
                self.release(i, s, true);
            }
        }
    }
}

/// One `svc:netctl` request.
fn control(staging: &mut rules::Staging, m: &[u8]) -> [u8; proto::REPLY_SIZE] {
    match m.first().copied() {
        Some(proto::ctl::BEGIN) => {
            *staging = rules::Staging::new();
            reply(status::OK, 0, 0)
        }
        Some(proto::ctl::LINE) => match core::str::from_utf8(&m[1..]) {
            Ok(text) => {
                staging.line(text);
                reply(status::OK, 0, 0)
            }
            Err(_) => reply(status::BAD_REQUEST, staging.lines + 1, 0),
        },
        Some(proto::ctl::COMMIT) => match staging.error {
            Some(line) => {
                event!(Warn, "net.rules_rejected", line = line);
                reply(status::BAD_REQUEST, line, 0)
            }
            None => {
                filter::replace(&staging.set);
                reply(status::OK, staging.set.len as u32, 0)
            }
        },
        Some(proto::ctl::CLEAR) => {
            filter::replace(&filter::Ruleset::OPEN);
            reply(status::OK, 0, 0)
        }
        Some(proto::ctl::STATS) => {
            filter::log_stats();
            reply(status::OK, 0, 0)
        }
        _ => reply(status::BAD_REQUEST, 0, 0),
    }
}

/// `handle`'s answer for a request answered later (UDP_WAIT).
const DEFERRED: u32 = u32::MAX;

fn reply(status: u32, v0: u32, v1: u32) -> [u8; proto::REPLY_SIZE] {
    let mut r = [0u8; proto::REPLY_SIZE];
    r[0..4].copy_from_slice(&status.to_le_bytes());
    r[4..8].copy_from_slice(&v0.to_le_bytes());
    r[8..12].copy_from_slice(&v1.to_le_bytes());
    r
}

/// A reply with an IPv6 address after it.
fn reply6(status: u32, v0: u32, v1: u32, addr: [u8; 16]) -> [u8; proto::REPLY_SIZE_V6] {
    let mut r = [0u8; proto::REPLY_SIZE_V6];
    r[..proto::REPLY_SIZE].copy_from_slice(&reply(status, v0, v1));
    r[proto::REPLY_SIZE..].copy_from_slice(&addr);
    r
}

/// `run net [FLAGS]`: FLAGS bit 0 adds the test LAN (doc/23 §4); bit 1
/// has its peer use DHCP (doc/25 §4).
/// `run net ADDR [ROUTER]` (ADDR an IPv4 address as a number, so ≥ 256):
/// the Wi-Fi link uses ADDR/24 and ROUTER instead of DHCP (doc/22 §7).
fn main(a0: usize, a1: usize) -> usize {
    tick();
    let (flags, static_addr, static_router) = if a0 < 0x100 { (a0 as u32, 0, 0) } else { (0, a0 as u32, a1 as u32) };
    match run(flags, static_addr, static_router) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "net.failed", step = step);
            step as usize
        }
    }
}

const FLAG_TEST_LAN: u32 = 1;
/// The test LAN's peer takes its address by DHCP (doc/25 §4).
const FLAG_PEER_DHCP: u32 = 2;

fn run(flags: u32, static_addr: u32, static_router: u32) -> Result<(), u32> {
    init_statics();
    let pool_shm = Shm::alloc(CLIENTS * proto::SLICE).map_err(|_| 1u32)?;
    let pool = pool_shm.map().map_err(|_| 2u32)?;
    let l = Listener::open(proto::SERVICE).map_err(|_| 3u32)?;
    let ctl_listener = Listener::open(proto::ctl::SERVICE).ok();
    let mut ctl: Option<Channel> = None;
    let mut staging = rules::Staging::new();

    let host = Handle::open(netdev::HOST, rights::READ | rights::WRITE | rights::WAIT).ok();
    let mut dev = Links { host, host_queued: true, to_host: 0, from_host: 0 };
    let iface = Interface::new(Config::new(HardwareAddress::Ip), &mut dev, now());
    let mut stack = Stack {
        iface,
        dev,
        sockets: SocketSet::new(unsafe { core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(STORAGE0).cast::<SocketStorage<'static>>(), POOL) }),
        eth: [None, None],
        peer: None,
        slots: [None; SOCKETS],
        pool_used: [false; POOL],
        next_port: 49152,
        fwd: Forwarder::new(),
        v6: fwd6::Fwd6::new(),
        dhcpd: None,
        relay: None,
        rules_gen: 0,
        reply_addr: None,
        udp_waiting: [false; CLIENTS],
    };
    let has_host = stack.dev.host.is_some();
    stack.iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(Ipv6Address::LOCALHOST.into(), 128));
        let _ = a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        if has_host {
            let _ = a.push(IpCidr::new(in64(HOST_NET6, 2).into(), 64));
            let _ = a.push(IpCidr::new(Ipv4Address::from_bits(HOST_ADDR).into(), HOST_PREFIX));
        }
    });
    event!(Info, "net.ready", link = "lo", addr = "127.0.0.1/8", sockets = SOCKETS as u32);
    if has_host {
        event!(Info, "net.link", link = "host", addr = "10.55.0.2/24", mtu = netdev::MTU as u32);
    }
    stack.eth[WLAN - 1] = open_wifi();
    if static_addr != 0
        && let Some(w) = &mut stack.eth[WLAN - 1]
    {
        if let Some(h) = w.dhcp.take() {
            w.sockets.remove(h);
        }
        w.set_addr(static_addr, 24, (static_router != 0).then_some(static_router));
        event!(Info, "net.static", link = "wlan0", addr = static_addr, router = static_router);
    }
    if flags & FLAG_TEST_LAN != 0 {
        let mut t = EthLink::new(TEST, Port::Test, testlan::NET_MAC, false);
        t.iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(in64(TEST_NET6, 1).into(), 64));
        });
        t.set_addr(testlan::NET_ADDR, testlan::PREFIX, None);
        stack.eth[TEST - 1] = Some(t);
        stack.peer = Some(testlan::Peer::new(flags & FLAG_PEER_DHCP != 0));
        event!(Info, "net.link", link = "test0", addr = "192.168.50.1/24", mtu = netdev::MTU as u32);
    }

    let mut clients: [Option<Channel>; CLIENTS] = [const { None }; CLIENTS];
    let mut msg = [0u8; chan::MSG_MAX];
    // Without Wi-Fi (started before the Wi-Fi service had the chip up, at
    // boot), try again every second.
    let mut wifi_retry_at = clock_ns() + 1_000_000_000;
    loop {
        if stack.eth[WLAN - 1].is_none() && static_addr == 0 && clock_ns() >= wifi_retry_at {
            wifi_retry_at = clock_ns() + 1_000_000_000;
            stack.eth[WLAN - 1] = open_wifi();
        }
        stack.poll();
        // UDP_WAITs whose datagram has come.
        for (i, c) in clients.iter().enumerate() {
            if let Some(c) = c
                && stack.udp_waiting[i]
                && stack.udp_ready(i)
            {
                stack.udp_waiting[i] = false;
                let _ = c.send(&reply(status::OK, 1, 0));
            }
        }
        // The listeners, the links (frames arriving), the control client,
        // the clients.
        let mut hs = [0u32; 6 + CLIENTS];
        let mut who = [usize::MAX; 6 + CLIENTS];
        hs[0] = l.0.raw();
        let mut n = 1;
        let mut ctl_at = (usize::MAX, usize::MAX);
        if let Some(cl) = &ctl_listener {
            hs[n] = cl.0.raw();
            ctl_at.0 = n;
            n += 1;
        }
        if let Some(c) = &ctl {
            hs[n] = c.0.raw();
            ctl_at.1 = n;
            n += 1;
        }
        let first_link = n;
        let (mut host_at, mut wifi_at) = (usize::MAX, usize::MAX);
        if let Some(h) = &stack.dev.host {
            (hs[n], host_at) = (h.raw(), n);
            n += 1;
        }
        if let Some(h) = stack.eth[WLAN - 1].as_ref().and_then(|w| w.dev.port.handle()) {
            (hs[n], wifi_at) = (h.raw(), n);
            n += 1;
        }
        let links = n;
        for (i, c) in clients.iter().enumerate() {
            if let Some(c) = c {
                hs[n] = c.0.raw();
                who[n] = i;
                n += 1;
            }
        }
        for e in stack.eth.iter().flatten() {
            e.dev.port.kick();
        }
        tick();
        let delay = stack.poll_delay();
        let timeout = delay.map_or(IDLE_NS, |d| (d.total_micros() * 1000).min(IDLE_NS)).max(1);
        let ev = wait_raw(&hs[..n], timeout);
        // Requests below are handled at the time they came.
        tick();
        let ev = match ev {
            Ok(ev) => ev,
            Err(Error::TimedOut) => continue,
            Err(_) => return Err(4),
        };
        // Only the link that woke us is read: `wait` is level-triggered, so
        // frames on another one end the next `wait` at once. Reading every
        // link after each `wait` was most of the `READ`s, nearly all empty
        // (doc/35 §3).
        if ev.index == host_at {
            stack.dev.host_queued = true;
        }
        if ev.index == wifi_at
            && let Some(w) = stack.eth[WLAN - 1].as_mut()
        {
            w.dev.queued = true;
        }
        if ev.index == ctl_at.0 {
            if let Some(c) = ctl_listener.as_ref().and_then(|cl| cl.accept().ok())
                && ctl.is_none()
            {
                ctl = Some(c);
            }
            continue;
        }
        if ev.index == ctl_at.1 {
            if ev.kind == chan::PEER_CLOSED {
                ctl = None;
                continue;
            }
            let c = ctl.as_ref().unwrap();
            while let Ok(len) = c.0.read(&mut msg) {
                let r = control(&mut staging, &msg[..len]);
                let _ = c.send(&r);
            }
            continue;
        }
        if ev.index >= first_link && ev.index < links {
            continue; // the next poll takes the frames
        }
        if ev.index == 0 {
            let Ok(c) = l.accept() else { continue };
            let Some(i) = clients.iter().position(Option::is_none) else { continue };
            let offset = i * proto::SLICE;
            if pool_shm.grant(&c, offset, proto::SLICE).is_ok() {
                clients[i] = Some(c);
            }
            continue;
        }
        let i = who[ev.index];
        if ev.kind == chan::PEER_CLOSED {
            stack.drop_client(i);
            clients[i] = None;
            continue;
        }
        let mem = &mut pool[i * proto::SLICE..(i + 1) * proto::SLICE];
        let c = clients[i].as_ref().unwrap();
        while let Ok(len) = c.0.read(&mut msg) {
            // An unanswered UDP_WAIT is answered first (0), keeping the
            // replies in order.
            if stack.udp_waiting[i] {
                stack.udp_waiting[i] = false;
                let _ = c.send(&reply(status::OK, 0, 0));
            }
            stack.reply_addr = None;
            let (st, v0, v1) = match stack.handle(i, mem, &msg[..len]) {
                Ok((v0, v1)) => (status::OK, v0, v1),
                Err(DEFERRED) => continue,
                Err(s) => (s, 0, 0),
            };
            // Let the stack act on the request before answering (a send
            // reaches the peer, a connect starts).
            stack.poll();
            let _ = match stack.reply_addr.take() {
                Some(a) => c.send(&reply6(st, v0, v1, a)),
                None => c.send(&reply(st, v0, v1)),
            };
        }
    }
}
