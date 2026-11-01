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

use core::cell::Cell;

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use t5_rt::netdev::ring::Rings;
use t5_rt::{Error, Handle, event, netdev, sleep_ms, yield_now};

use crate::filter::{self, ANY_LINK, Action, IN, OUT};
use crate::fwd::{self, IFACE0};
use crate::fwd6;
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

/// Received frames wait here, one buffer per kind of device (`RX_*`):
/// smoltcp consumes a token before its device receives again. Static and
/// not cleared: zeroing a 1.5 KB frame on every `receive` (most find
/// nothing) and moving it in the token showed in the profile (doc/34).
static mut RX_FRAMES: [[u8; ETH_FRAME]; 3] = [[0; ETH_FRAME]; 3];
pub const RX_LINKS: usize = 0;
pub const RX_ETH: usize = 1;
pub const RX_PEER: usize = 2;

/// The receive buffer of device kind `slot`.
pub fn rx_frame(slot: usize) -> &'static mut [u8; ETH_FRAME] {
    unsafe { &mut *core::ptr::addr_of_mut!(RX_FRAMES[slot]) }
}

/// The frame a transmit token fills (one token is consumed at a time);
/// smoltcp writes all of it.
static mut TX_FRAME: [u8; ETH_FRAME] = [0; ETH_FRAME];

pub fn tx_frame() -> &'static mut [u8; ETH_FRAME] {
    unsafe { &mut *core::ptr::addr_of_mut!(TX_FRAME) }
}

/// A received frame: `len` bytes of `rx_frame(slot)`, or with `ring` the
/// oldest frame of that receive ring, given back once consumed.
pub struct Rx {
    pub slot: usize,
    pub len: usize,
    pub ring: Option<Rings>,
}

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        match self.ring {
            Some(g) => {
                let r = f(g.rx_peek().map_or(&[][..], |b| &b[..self.len.min(b.len())]));
                g.rx_pop();
                r
            }
            None => f(&rx_frame(self.slot)[..self.len]),
        }
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
    /// The kernel may have frames from the host queued (see
    /// `EthNic::queued`).
    pub host_queued: bool,
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
        let buf = tx_frame();
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
        let frame = rx_frame(RX_LINKS);
        if let Some((_, n)) = lo().pop(frame) {
            return Some((Rx { slot: RX_LINKS, len: n, ring: None }, LinkTx(self)));
        }
        let h = self.host.as_ref()?;
        loop {
            if !self.host_queued {
                return None;
            }
            let Ok((len, more)) = h.read_queued(&mut frame[..MTU]) else {
                self.host_queued = false;
                return None;
            };
            self.host_queued = more > 0;
            self.from_host += 1;
            if nat::inbound(&mut frame[..len], IFACE0 as u8) {
                fwd::inbox().push(IFACE0 as u8, &frame[..len]);
                continue;
            }
            let ip = &frame[..len];
            if transit(ip, Some((HOST_ADDR, HOST_PREFIX))) || fwd6::transit(ip) {
                fwd::inbox().push(IFACE0 as u8, ip);
                continue;
            }
            match filter::check(IN, IFACE0 as u8, ANY_LINK, ip) {
                Action::Pass => return Some((Rx { slot: RX_LINKS, len, ring: None }, LinkTx(self))),
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

/// Frames queued on the transmit ring before the kernel is told.
const KICK_EVERY: u32 = 4;

/// Where an Ethernet link's frames go.
pub enum Port {
    /// A kernel `netdev` object, through `read`/`write`.
    Dev(Handle),
    /// A kernel `netdev` object, through its frame rings (doc/35 §5).
    /// `unsent`: frames queued since the last `KICK`.
    Ring { h: Handle, rings: Rings, unsent: Cell<u32> },
    /// The test LAN.
    Test,
}

impl Port {
    /// The device's frame rings if it has them, else `read`/`write`.
    pub fn open(h: Handle) -> Port {
        match h.ctl(netdev::MAP_RING, 0) {
            Ok(base) => {
                event!(Info, "net.rings", base = t5_rt::log::Addr(base as u32));
                Port::Ring { h, rings: Rings(base), unsent: Cell::new(0) }
            }
            Err(_) => Port::Dev(h),
        }
    }

    /// A frame, and whether more may be queued.
    pub fn read(&self, out: &mut [u8]) -> Option<(usize, bool)> {
        match self {
            Port::Dev(h) => h.read_queued(out).ok().map(|(n, more)| (n, more > 0)),
            // Filled by the peer within the same pass, not signalled by
            // `wait`: always looked at.
            Port::Test => from_peer().pop(out).map(|(_, n)| (n, true)),
            Port::Ring { .. } => None,
        }
    }

    fn signalled(&self) -> bool {
        matches!(self, Port::Dev(_))
    }

    /// A free transmit slot of the rings, waiting while they are full.
    /// On Core1 a `KICK` only asks Core0 to send, which frees a slot about
    /// every 80 µs: so first a few `yield`s, then 1 ms sleeps (as `write`
    /// does for `Busy`).
    fn tx_slot(h: &Handle, rings: &Rings) -> Option<&'static mut [u8; netdev::ring::SLOT]> {
        if let Some(s) = rings.tx_slot() {
            return Some(s);
        }
        let _ = h.ctl(netdev::KICK, 0);
        for i in 0..64 {
            if let Some(s) = rings.tx_slot() {
                return Some(s);
            }
            if i < 44 { yield_now() } else { sleep_ms(1) }
        }
        None
    }

    /// Counts a frame queued on the rings; every `KICK_EVERY` the kernel
    /// is told at once, so the ring does not fill up within one smoltcp
    /// pass (a TCP window is about 22 frames).
    fn queued(h: &Handle, unsent: &Cell<u32>) {
        let n = unsent.get() + 1;
        if n >= KICK_EVERY {
            let _ = h.ctl(netdev::KICK, 0);
            unsent.set(0);
        } else {
            unsent.set(n);
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
            Port::Ring { h, rings, unsent } => {
                let Some(slot) = Self::tx_slot(h, rings).filter(|_| f.len() <= netdev::ring::SLOT) else { return false };
                slot[..f.len()].copy_from_slice(f);
                rings.tx_push(f.len());
                Self::queued(h, unsent);
                true
            }
            Port::Test => to_peer().push(0, f),
        }
    }

    /// Has the kernel send what was queued on the rings (once per pass,
    /// before `wait`).
    pub fn kick(&self) {
        if let Port::Ring { h, unsent, .. } = self
            && unsent.replace(0) > 0
        {
            let _ = h.ctl(netdev::KICK, 0);
        }
    }

    pub fn handle(&self) -> Option<&Handle> {
        match self {
            Port::Dev(h) | Port::Ring { h, .. } => Some(h),
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
    /// The kernel may have frames queued: set after each `wait`, cleared
    /// by a `read` that leaves none. A `receive` without it makes no
    /// system call; most were empty reads, one per smoltcp poll (doc/34).
    pub queued: bool,
}

pub struct EthTx<'a>(&'a mut EthNic);

impl TxToken for EthTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let nic = self.0;
        // With rings, smoltcp builds the frame in the transmit slot.
        let slot = match &nic.port {
            Port::Ring { h, rings, .. } if len <= netdev::ring::SLOT => Port::tx_slot(h, rings),
            _ => None,
        };
        let in_slot = slot.is_some();
        let buf: &mut [u8] = match slot {
            Some(s) => s,
            None => tx_frame(),
        };
        let r = f(&mut buf[..len]);
        let ip = buf[12..14] == ETHERTYPE_IPV4 || buf[12..14] == ETHERTYPE_IPV6;
        if len > ETH_HDR && ip && filter::check(OUT, ANY_LINK, nic.link as u8, &buf[ETH_HDR..len]) != Action::Pass {
            return r;
        }
        if let Port::Ring { h, rings, unsent } = &nic.port {
            if in_slot {
                rings.tx_push(len);
                Port::queued(h, unsent);
                nic.tx += 1;
            } else {
                nic.tx_dropped += 1;
            }
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
        loop {
            // With rings, the frame is looked at (and changed by NAT) in
            // its slot, which goes back once used (doc/35 §5).
            let (frame, n, ring) = if let Port::Ring { rings, .. } = &self.port {
                let f = rings.rx_peek()?;
                let n = f.len();
                (f, n, Some(*rings))
            } else {
                let frame = rx_frame(RX_ETH);
                if !self.queued {
                    return None;
                }
                let Some((n, more)) = self.port.read(frame) else {
                    self.queued = !self.port.signalled();
                    return None;
                };
                self.queued = more || !self.port.signalled();
                (&mut frame[..], n, None)
            };
            if !self.keep(&mut frame[..n]) {
                if let Some(g) = ring {
                    g.rx_pop();
                }
                continue;
            }
            return Some((Rx { slot: RX_ETH, len: n, ring }, EthTx(self)));
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

impl EthNic {
    /// Counts a received frame and decides where it goes: false if it was
    /// forwarded, rejected or dropped here, true if it is for smoltcp.
    fn keep(&mut self, frame: &mut [u8]) -> bool {
        let n = frame.len();
        self.rx += 1;
        if n > ETH_HDR && frame[..6] == self.mac && frame[12..14] == ETHERTYPE_IPV4 && nat::inbound(&mut frame[ETH_HDR..n], self.link as u8) {
            self.rx_unicast += 1;
            fwd::inbox().push(self.link as u8, &frame[..n]);
            return false;
        }
        let f = &frame[..n];
        if f.len() < ETH_HDR {
            return false;
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
        if ethertype == ETHERTYPE_IPV6 {
            fwd6::snoop(self.link, f);
            if to_me && fwd6::transit(&f[ETH_HDR..]) {
                self.rx_unicast += 1;
                fwd::inbox().push(self.link as u8, f);
                return false;
            }
        }
        if to_me {
            self.rx_unicast += 1;
        }
        if (ethertype == ETHERTYPE_IPV4 || ethertype == ETHERTYPE_IPV6) && (to_me || f[0] & 1 != 0) {
            if to_me && ethertype == ETHERTYPE_IPV4 && transit(&f[ETH_HDR..], self.own) {
                fwd::inbox().push(self.link as u8, f);
                return false;
            }
            match filter::check(IN, self.link as u8, ANY_LINK, &f[ETH_HDR..]) {
                Action::Pass => {}
                Action::Drop => return false,
                Action::Reject => {
                    fwd::rejects().push(self.link as u8, f);
                    return false;
                }
            }
        }
        true
    }
}

/// Logs the link's counters (while it waits for an address).
pub fn log_counters(name: &str, d: &EthNic) {
    event!(Info, "net.link_waiting", link = name, rx = d.rx, rx_unicast = d.rx_unicast, tx = d.tx, tx_dropped = d.tx_dropped);
}
