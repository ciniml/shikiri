//! The links' devices for smoltcp (doc/23 §2). Each one decides, frame by
//! frame, whether a packet is for the Tab5 (it goes to smoltcp) or passes
//! through (it goes to the forwarding inbox, `fwd`).
//!
//! - `Links`: interface 0 (`Medium::Ip`): loopback (127.0.0.0/8) and the
//!   host link to the PC (`netdev:host`, 10.55.0.2/24).
//! - `EthNic`: an Ethernet link: the Wi-Fi station (`netdev:wlan0`) or the
//!   test LAN (`test0`, a pair of rings to a peer inside `net`).
//!
//! Packets for the Tab5 pass the filter's IN hook here, and packets from
//! its stack the OUT hook (doc/24).

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use t5_rt::{Error, Handle, event, netdev, sleep_ms};

use crate::filter::{self, ANY_LINK, Action, IN, OUT};
use crate::fwd::{self, IFACE0};
use crate::nat;

pub const MTU: usize = 1500;
pub const ETH_FRAME: usize = netdev::ETH_FRAME;
pub const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
const ETHERTYPE_IPV6: [u8; 2] = [0x86, 0xDD];
const ETHERTYPE_ARP: [u8; 2] = [0x08, 0x06];

/// Whether an IP packet stays on the Tab5 (127/8, ::1).
fn loopback(ip: &[u8]) -> bool {
    match ip.first().map(|v| v >> 4) {
        Some(4) => ip.get(16) == Some(&127),
        Some(6) => ip.get(24..40).is_some_and(|d| d[..15].iter().all(|&b| b == 0) && d[15] == 1),
        _ => false,
    }
}

/// The host link's address and network (doc/21).
pub const HOST_ADDR: u32 = 0x0A37_0002; // 10.55.0.2
pub const HOST_PREFIX: u8 = 24;

/// A fixed ring of frames, each with a tag (the link it came from).
pub struct Ring<const N: usize> {
    frames: [[u8; ETH_FRAME]; N],
    lens: [usize; N],
    tags: [u8; N],
    head: usize,
    count: usize,
    pub dropped: u32,
}

impl<const N: usize> Ring<N> {
    pub const fn new() -> Self {
        Ring { frames: [[0; ETH_FRAME]; N], lens: [0; N], tags: [0; N], head: 0, count: 0, dropped: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn is_full(&self) -> bool {
        self.count == N
    }

    /// False (and counted) when full: the frame is lost, as on a real link.
    pub fn push(&mut self, tag: u8, f: &[u8]) -> bool {
        if self.count == N || f.len() > ETH_FRAME {
            self.dropped += 1;
            return false;
        }
        let i = (self.head + self.count) % N;
        self.frames[i][..f.len()].copy_from_slice(f);
        self.lens[i] = f.len();
        self.tags[i] = tag;
        self.count += 1;
        true
    }

    /// The oldest frame into `out`: (tag, length).
    pub fn pop(&mut self, out: &mut [u8]) -> Option<(u8, usize)> {
        if self.count == 0 {
            return None;
        }
        let i = self.head;
        let n = self.lens[i].min(out.len());
        out[..n].copy_from_slice(&self.frames[i][..n]);
        self.head = (self.head + 1) % N;
        self.count -= 1;
        Some((self.tags[i], n))
    }
}

pub struct Rx {
    pub frame: [u8; ETH_FRAME],
    pub len: usize,
}

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame[..self.len])
    }
}

/// Whether an IPv4 packet that reached a link with address `own` (address,
/// prefix) is for another host: then it is forwarded. Broadcast, multicast,
/// loopback and the Tab5's addresses on every link stay (each interface
/// also has the other links' addresses, `Stack::sync_addrs`); so does
/// everything while the link has no address (DHCP's offer is sent to the
/// address offered).
pub fn transit(ip: &[u8], own: Option<(u32, u8)>) -> bool {
    let Some((addr, prefix)) = own else { return false };
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return false;
    }
    let dst = fwd::be32(&ip[16..20]);
    let host_bits = if prefix >= 32 { 0 } else { u32::MAX >> prefix };
    let subnet_broadcast = dst & !host_bits == addr & !host_bits && dst & host_bits == host_bits;
    !(dst == addr || fwd::is_own(dst) || dst == u32::MAX || subnet_broadcast || dst >> 28 == 0xE || dst >> 24 == 127)
}

// ---- interface 0: loopback and the host link ----

const LOOP_RING: usize = 16;
static mut LOOPBACK: Ring<LOOP_RING> = Ring::new();

fn lo() -> &'static mut Ring<LOOP_RING> {
    unsafe { &mut *core::ptr::addr_of_mut!(LOOPBACK) }
}

pub struct Links {
    /// `netdev:host`, if the kernel has it.
    pub host: Option<Handle>,
    pub to_host: u32,
    pub from_host: u32,
}

impl Links {
    pub fn loopback_idle() -> bool {
        lo().is_empty()
    }
}

/// A packet from the stack: sent by its destination (127/8 stays).
pub struct LinkTx<'a>(&'a mut Links);

impl TxToken for LinkTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; MTU];
        let r = f(&mut buf[..len]);
        let links = self.0;
        if len >= 20 && !loopback(&buf[..len])
            && let Some(h) = &links.host
        {
            if filter::check(OUT, ANY_LINK, IFACE0 as u8, &buf[..len]) != Action::Pass {
                return r;
            }
            if h.write(&buf[..len]).is_ok() {
                links.to_host += 1;
            }
            return r;
        }
        lo().push(0, &buf[..len]);
        r
    }
}

impl Device for Links {
    type RxToken<'a> = Rx;
    type TxToken<'a> = LinkTx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, LinkTx<'_>)> {
        let mut out = Rx { frame: [0; ETH_FRAME], len: 0 };
        if let Some((_, n)) = lo().pop(&mut out.frame) {
            out.len = n;
            return Some((out, LinkTx(self)));
        }
        let h = self.host.as_ref()?;
        loop {
            out.len = h.read(&mut out.frame[..MTU]).ok()?;
            self.from_host += 1;
            if nat::inbound(&mut out.frame[..out.len], IFACE0 as u8) {
                fwd::inbox().push(IFACE0 as u8, &out.frame[..out.len]);
                continue;
            }
            let ip = &out.frame[..out.len];
            if transit(ip, Some((HOST_ADDR, HOST_PREFIX))) {
                fwd::inbox().push(IFACE0 as u8, ip);
                continue;
            }
            match filter::check(IN, IFACE0 as u8, ANY_LINK, ip) {
                Action::Pass => return Some((out, LinkTx(self))),
                Action::Drop => {}
                Action::Reject => {
                    fwd::rejects().push(IFACE0 as u8, ip);
                }
            }
        }
    }

    fn transmit(&mut self, _: Instant) -> Option<LinkTx<'_>> {
        (!lo().is_full()).then_some(LinkTx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

// ---- Ethernet links ----

/// The test LAN's two directions (`test0`, doc/23 §4).
pub const TEST_RING: usize = 8;
static mut TO_PEER: Ring<TEST_RING> = Ring::new();
static mut FROM_PEER: Ring<TEST_RING> = Ring::new();

pub fn to_peer() -> &'static mut Ring<TEST_RING> {
    unsafe { &mut *core::ptr::addr_of_mut!(TO_PEER) }
}

pub fn from_peer() -> &'static mut Ring<TEST_RING> {
    unsafe { &mut *core::ptr::addr_of_mut!(FROM_PEER) }
}

/// Where an Ethernet link's frames go.
pub enum Port {
    /// A kernel `netdev` object.
    Dev(Handle),
    /// The test LAN.
    Test,
}

impl Port {
    pub fn read(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Port::Dev(h) => h.read(out).ok(),
            Port::Test => from_peer().pop(out).map(|(_, n)| n),
        }
    }

    /// Sends one frame; false if it was dropped.
    pub fn write(&self, f: &[u8]) -> bool {
        match self {
            Port::Dev(h) => {
                for _ in 0..20 {
                    match h.write(f) {
                        Ok(_) => return true,
                        Err(Error::Busy) => sleep_ms(1),
                        Err(_) => break,
                    }
                }
                false
            }
            Port::Test => to_peer().push(0, f),
        }
    }

    pub fn handle(&self) -> Option<&Handle> {
        match self {
            Port::Dev(h) => Some(h),
            Port::Test => None,
        }
    }
}

pub struct EthNic {
    pub link: usize,
    pub port: Port,
    pub mac: [u8; 6],
    /// The link's address and prefix, once it has one.
    pub own: Option<(u32, u8)>,
    pub rx: u32,
    pub rx_unicast: u32,
    pub tx: u32,
    pub tx_dropped: u32,
}

pub struct EthTx<'a>(&'a mut EthNic);

impl TxToken for EthTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; ETH_FRAME];
        let r = f(&mut buf[..len]);
        let nic = self.0;
        let ip = buf[12..14] == ETHERTYPE_IPV4 || buf[12..14] == ETHERTYPE_IPV6;
        if len > ETH_HDR && ip && filter::check(OUT, ANY_LINK, nic.link as u8, &buf[ETH_HDR..len]) != Action::Pass {
            return r;
        }
        if nic.port.write(&buf[..len]) {
            nic.tx += 1;
        } else {
            nic.tx_dropped += 1;
        }
        r
    }
}

impl Device for EthNic {
    type RxToken<'a> = Rx;
    type TxToken<'a> = EthTx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, EthTx<'_>)> {
        let mut out = Rx { frame: [0; ETH_FRAME], len: 0 };
        loop {
            out.len = self.port.read(&mut out.frame)?;
            self.rx += 1;
            let n = out.len;
            if n > ETH_HDR && out.frame[..6] == self.mac && out.frame[12..14] == ETHERTYPE_IPV4 && nat::inbound(&mut out.frame[ETH_HDR..n], self.link as u8) {
                self.rx_unicast += 1;
                fwd::inbox().push(self.link as u8, &out.frame[..n]);
                continue;
            }
            let f = &out.frame[..out.len];
            if f.len() < ETH_HDR {
                continue;
            }
            let ethertype = [f[12], f[13]];
            if ethertype == ETHERTYPE_ARP && f.len() >= ETH_HDR + 28 {
                // Sender's address pair (RFC 826), for forwarding.
                let ip = fwd::be32(&f[28..32]);
                if ip != 0 {
                    fwd::arp().learn(self.link, ip, f[22..28].try_into().unwrap());
                }
            }
            let to_me = f[..6] == self.mac;
            if to_me {
                self.rx_unicast += 1;
            }
            if (ethertype == ETHERTYPE_IPV4 || ethertype == ETHERTYPE_IPV6) && (to_me || f[0] & 1 != 0) {
                if to_me && ethertype == ETHERTYPE_IPV4 && transit(&f[ETH_HDR..], self.own) {
                    fwd::inbox().push(self.link as u8, f);
                    continue;
                }
                match filter::check(IN, self.link as u8, ANY_LINK, &f[ETH_HDR..]) {
                    Action::Pass => {}
                    Action::Drop => continue,
                    Action::Reject => {
                        fwd::rejects().push(self.link as u8, f);
                        continue;
                    }
                }
            }
            return Some((out, EthTx(self)));
        }
    }

    fn transmit(&mut self, _: Instant) -> Option<EthTx<'_>> {
        Some(EthTx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = ETH_FRAME;
        c
    }
}

/// Logs the link's counters (while it waits for an address).
pub fn log_counters(name: &str, d: &EthNic) {
    event!(Info, "net.link_waiting", link = name, rx = d.rx, rx_unicast = d.rx_unicast, tx = d.tx, tx_dropped = d.tx_dropped);
}
