#![no_std]
#![no_main]

//! The network service (N1/N2a/N2b, doc/20, doc/21, doc/22): smoltcp in
//! U-mode. Links: loopback (127.0.0.0/8) and, if the kernel offers it, the
//! host link to the PC (`netdev:host`, 10.55.0.2/24): one smoltcp interface
//! runs over a `Links` device that sends each packet to the link its
//! destination belongs to (the place for forwarding later, N3). The Wi-Fi
//! station (`netdev:wlan0`, Ethernet, address by DHCP) is a second
//! interface with its own sockets; a TCP connection goes there when its
//! destination is on neither of the first two links. Clients on `svc:net`
//! get a 64 KiB slice of shared memory for data and use TCP and UDP sockets
//! through `t5_abi::net` requests. No allocator: socket buffers are static.
//!
//! Built with `--permit shm --permit listen:net --permit netdev:host
//! --permit netdev:wlan0`.

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};
use t5_rt::net::{self as proto, op, state, status};
use t5_rt::{Channel, Error, Handle, Listener, Shm, chan, event, netdev, rights, sleep_ms, time_ns, wait_raw};

t5_rt::entry!(main);

const CLIENTS: usize = 4;
const SOCKETS: usize = 8;
const TCP_BUF: usize = 16 << 10;
const UDP_PACKETS: usize = 16;
const UDP_BUF: usize = 8 << 10;
const MTU: usize = 1500;
/// Longest wait without a request, so timers (retransmission) run.
const IDLE_NS: u64 = 50_000_000;

// ---- loopback link: a fixed ring of frames ----

const RING: usize = 16;

struct Loopback {
    frames: [[u8; MTU]; RING],
    lens: [usize; RING],
    head: usize,
    count: usize,
}

impl Loopback {
    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

struct Rx {
    frame: [u8; MTU],
    len: usize,
}

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame[..self.len])
    }
}

struct Tx<'a>(&'a mut Loopback);

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let l = self.0;
        if l.count == RING {
            // Full: the frame is lost, as on a real link.
            let mut scratch = [0u8; MTU];
            return f(&mut scratch[..len]);
        }
        let i = (l.head + l.count) % RING;
        let r = f(&mut l.frames[i][..len]);
        l.lens[i] = len;
        l.count += 1;
        r
    }
}

impl Device for Loopback {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        if self.count == 0 {
            return None;
        }
        let i = self.head;
        let mut rx = Rx { frame: [0; MTU], len: self.lens[i] };
        rx.frame[..rx.len].copy_from_slice(&self.frames[i][..rx.len]);
        self.head = (self.head + 1) % RING;
        self.count -= 1;
        Some((rx, Tx(self)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (self.count < RING).then_some(Tx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

// ---- the links: loopback and the host link, behind one device ----

/// The host link's address and network (doc/21).
const HOST_ADDR: [u8; 4] = [10, 55, 0, 2];
const HOST_PREFIX: u8 = 24;

struct Links {
    lo: &'static mut Loopback,
    /// `netdev:host`, if the kernel has it.
    host: Option<Handle>,
    to_host: u32,
    from_host: u32,
}

/// A frame for the stack: copied out of either link.
struct LinkRx {
    frame: [u8; MTU],
    len: usize,
}

impl RxToken for LinkRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame[..self.len])
    }
}

/// A frame from the stack: built here, then sent by its destination.
struct LinkTx<'a>(&'a mut Links);

impl TxToken for LinkTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; MTU];
        let r = f(&mut buf[..len]);
        let links = self.0;
        // IPv4 destination: bytes 16-19. 127/8 stays on the Tab5.
        if len >= 20 && buf[16] != 127
            && let Some(h) = &links.host
        {
            let ok = h.write(&buf[..len]).is_ok();
            if ok {
                links.to_host += 1;
            }
            if links.to_host < 4 || !ok {
                event!(Debug, "net.host_tx", len = len as u32, ok = ok, sent = links.to_host);
            }
            return r;
        }
        Tx(links.lo).consume(len, |out| out.copy_from_slice(&buf[..len]));
        r
    }
}

impl Device for Links {
    type RxToken<'a> = LinkRx;
    type TxToken<'a> = LinkTx<'a>;

    fn receive(&mut self, t: Instant) -> Option<(LinkRx, LinkTx<'_>)> {
        if let Some((rx, _)) = self.lo.receive(t) {
            let mut out = LinkRx { frame: [0; MTU], len: rx.len };
            out.frame[..rx.len].copy_from_slice(&rx.frame[..rx.len]);
            return Some((out, LinkTx(self)));
        }
        let h = self.host.as_ref()?;
        let mut out = LinkRx { frame: [0; MTU], len: 0 };
        out.len = h.read(&mut out.frame).ok()?;
        self.from_host += 1;
        if self.from_host <= 8 {
            let f = &out.frame;
            event!(Debug, "net.host_rx", len = out.len as u32, ver = f[0] >> 4, proto = f[9], dst = u32::from_be_bytes([f[16], f[17], f[18], f[19]]));
        }
        Some((out, LinkTx(self)))
    }

    fn transmit(&mut self, _: Instant) -> Option<LinkTx<'_>> {
        (self.lo.count < RING).then_some(LinkTx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.lo.capabilities()
    }
}

// ---- the Wi-Fi station: Ethernet frames on `netdev:wlan0` ----

struct WlanNic {
    h: Handle,
    mac: [u8; 6],
    rx: u32,
    /// Frames to this station's MAC address, and DHCP replies (UDP to 68).
    rx_unicast: u32,
    rx_dhcp: u32,
    tx: u32,
    tx_dropped: u32,
}

struct EthRx {
    frame: [u8; netdev::ETH_FRAME],
    len: usize,
}

impl RxToken for EthRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame[..self.len])
    }
}

struct EthTx<'a>(&'a mut WlanNic);

impl TxToken for EthTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; netdev::ETH_FRAME];
        let r = f(&mut buf[..len]);
        let nic = self.0;
        for _ in 0..20 {
            match nic.h.write(&buf[..len]) {
                Ok(_) => {
                    nic.tx += 1;
                    return r;
                }
                Err(Error::Busy) => sleep_ms(1),
                Err(_) => break,
            }
        }
        nic.tx_dropped += 1;
        r
    }
}

impl Device for WlanNic {
    type RxToken<'a> = EthRx;
    type TxToken<'a> = EthTx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(EthRx, EthTx<'_>)> {
        let mut out = EthRx { frame: [0; netdev::ETH_FRAME], len: 0 };
        out.len = self.h.read(&mut out.frame).ok()?;
        self.rx += 1;
        let f = &out.frame;
        if f[..6] == self.mac {
            self.rx_unicast += 1;
        }
        // IPv4 (no options assumed), UDP, destination port 68.
        if out.len > 42 && f[12..14] == [8, 0] && f[23] == 17 && f[36..38] == [0, 68] {
            self.rx_dhcp += 1;
            // From the IP header to the client hardware address (BOOTP chaddr).
            event!(Debug, "net.dhcp_rx", len = out.len as u32, head = &f[14..76]);
        }
        Some((out, EthTx(self)))
    }

    fn transmit(&mut self, _: Instant) -> Option<EthTx<'_>> {
        Some(EthTx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = netdev::ETH_FRAME;
        c
    }
}

struct Wifi {
    iface: Interface,
    dev: WlanNic,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    configured: bool,
    /// Static addressing: no DHCP socket.
    dhcp_off: bool,
    /// While not configured: when to log the counters next.
    report_at: u64,
}

/// Opens `netdev:wlan0` once the Wi-Fi service has told the kernel the MAC
/// address (it has joined or is joining), waiting up to 10 s.
fn open_wifi() -> Option<Wifi> {
    let h = Handle::open(netdev::WLAN0, rights::READ | rights::WRITE | rights::WAIT | rights::CTL).ok()?;
    let mut mac = None;
    for _ in 0..100 {
        if let (Ok(lo), Ok(hi)) = (h.ctl(netdev::MAC_LO, 0), h.ctl(netdev::MAC_HI, 0)) {
            let b = (lo as u64 | (hi as u64) << 32).to_le_bytes();
            mac = Some(EthernetAddress([b[0], b[1], b[2], b[3], b[4], b[5]]));
            break;
        }
        sleep_ms(100);
    }
    let Some(mac) = mac else {
        event!(Warn, "net.no_wifi_mac");
        return None;
    };
    let mut dev = WlanNic { h, mac: mac.0, rx: 0, rx_unicast: 0, rx_dhcp: 0, tx: 0, tx_dropped: 0 };
    let iface = Interface::new(Config::new(HardwareAddress::Ethernet(mac)), &mut dev, now());
    let storage = unsafe { core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(WIFI_STORAGE).cast::<SocketStorage<'static>>(), SOCKETS + 1) };
    let mut sockets = SocketSet::new(storage);
    let dhcp = sockets.add(dhcpv4::Socket::new());
    event!(Info, "net.link", link = "wlan0", addr = "dhcp", mtu = netdev::MTU as u32);
    Some(Wifi { iface, dev, sockets, dhcp, configured: false, dhcp_off: false, report_at: time_ns() + 5_000_000_000 })
}

impl Wifi {
    /// Applies what DHCP says (address, default route).
    fn dhcp(&mut self) {
        if (!self.configured || self.dhcp_off) && time_ns() >= self.report_at {
            self.report_at = time_ns() + 5_000_000_000;
            let d = &self.dev;
            event!(Info, "net.wifi_waiting", rx = d.rx, rx_unicast = d.rx_unicast, rx_dhcp = d.rx_dhcp, tx = d.tx, tx_dropped = d.tx_dropped);
        }
        if self.dhcp_off {
            return;
        }
        let ev = self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll();
        match ev {
            Some(dhcpv4::Event::Configured(c)) => {
                self.iface.update_ip_addrs(|a| {
                    a.clear();
                    let _ = a.push(IpCidr::Ipv4(c.address));
                });
                match c.router {
                    Some(gw) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(gw);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                let dns = c.dns_servers.first().map_or(0, |d| d.to_bits());
                event!(
                    Info,
                    "net.dhcp",
                    link = "wlan0",
                    addr = c.address.address().to_bits(),
                    prefix = c.address.prefix_len() as u32,
                    router = c.router.map_or(0, |r| r.to_bits()),
                    dns = dns
                );
                self.configured = true;
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.iface.update_ip_addrs(|a| a.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                event!(Info, "net.dhcp_lost", link = "wlan0");
                self.configured = false;
            }
            None => {}
        }
    }
}

// ---- socket storage (static: there is no allocator) ----

static mut TCP_RX: [[u8; TCP_BUF]; SOCKETS] = [[0; TCP_BUF]; SOCKETS];
static mut TCP_TX: [[u8; TCP_BUF]; SOCKETS] = [[0; TCP_BUF]; SOCKETS];
static mut UDP_RX: [[u8; UDP_BUF]; SOCKETS] = [[0; UDP_BUF]; SOCKETS];
static mut UDP_TX: [[u8; UDP_BUF]; SOCKETS] = [[0; UDP_BUF]; SOCKETS];
static mut UDP_RX_META: [[udp::PacketMetadata; UDP_PACKETS]; SOCKETS] = [[udp::PacketMetadata::EMPTY; UDP_PACKETS]; SOCKETS];
static mut UDP_TX_META: [[udp::PacketMetadata; UDP_PACKETS]; SOCKETS] = [[udp::PacketMetadata::EMPTY; UDP_PACKETS]; SOCKETS];
static mut STORAGE: [SocketStorage<'static>; SOCKETS] = [SocketStorage::EMPTY; SOCKETS];
/// The Wi-Fi interface's sockets: any slot can be there, plus DHCP.
static mut WIFI_STORAGE: [SocketStorage<'static>; SOCKETS + 1] = [SocketStorage::EMPTY; SOCKETS + 1];

/// Row `i` of a static table, for slot `i` only (each slot's buffers are
/// handed to one socket at a time).
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
        let rx = udp::PacketBuffer::new(row(core::ptr::addr_of_mut!(UDP_RX_META), i), row(core::ptr::addr_of_mut!(UDP_RX), i));
        let tx = udp::PacketBuffer::new(row(core::ptr::addr_of_mut!(UDP_TX_META), i), row(core::ptr::addr_of_mut!(UDP_TX), i));
        udp::Socket::new(rx, tx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tcp,
    Udp,
}

#[derive(Clone, Copy)]
struct Slot {
    handle: SocketHandle,
    kind: Kind,
    /// On the Wi-Fi interface (else on the first one).
    wifi: bool,
    /// The client that owns it; None once closed, until the stack is done.
    owner: Option<usize>,
}

struct Stack {
    iface: Interface,
    dev: Links,
    sockets: SocketSet<'static>,
    wifi: Option<Wifi>,
    slots: [Option<Slot>; SOCKETS],
    next_port: u16,
}

/// Whether `a` is reached on the first interface (loopback, host link).
fn local(a: u32) -> bool {
    let [x, y, z, _] = a.to_be_bytes();
    x == 127 || [x, y, z] == [HOST_ADDR[0], HOST_ADDR[1], HOST_ADDR[2]]
}

fn now() -> Instant {
    Instant::from_micros((time_ns() / 1000) as i64)
}

impl Stack {
    /// Runs the stack until nothing moves.
    fn poll(&mut self) {
        for _ in 0..256 {
            let r = self.iface.poll(now(), &mut self.dev, &mut self.sockets);
            if r == PollResult::None && self.dev.lo.is_empty() {
                break;
            }
        }
        if let Some(w) = &mut self.wifi {
            for _ in 0..64 {
                if w.iface.poll(now(), &mut w.dev, &mut w.sockets) == PollResult::None {
                    break;
                }
            }
            w.dhcp();
        }
        // Closed sockets the stack has finished with go back to the pool.
        for s in self.slots.iter_mut() {
            if let Some(slot) = s
                && slot.owner.is_none()
            {
                let set = match (&mut self.wifi, slot.wifi) {
                    (Some(w), true) => &mut w.sockets,
                    _ => &mut self.sockets,
                };
                let done = match slot.kind {
                    Kind::Tcp => matches!(set.get::<tcp::Socket>(slot.handle).state(), tcp::State::Closed | tcp::State::TimeWait),
                    Kind::Udp => true,
                };
                if done {
                    set.remove(slot.handle);
                    *s = None;
                }
            }
        }
    }

    /// The socket set of the first interface, or of Wi-Fi.
    fn set(&mut self, wifi: bool) -> &mut SocketSet<'static> {
        match (&mut self.wifi, wifi) {
            (Some(w), true) => &mut w.sockets,
            _ => &mut self.sockets,
        }
    }

    fn alloc(&mut self, client: usize, kind: Kind, wifi: bool) -> Result<usize, u32> {
        let i = self.slots.iter().position(Option::is_none).ok_or(status::NO_SOCKETS)?;
        let wifi = wifi && self.wifi.is_some();
        let set = self.set(wifi);
        let handle = match kind {
            Kind::Tcp => set.add(tcp_socket(i)),
            Kind::Udp => set.add(udp_socket(i)),
        };
        self.slots[i] = Some(Slot { handle, kind, wifi, owner: Some(client) });
        Ok(i)
    }

    /// Frees slot `i` that was just allocated (a request failed).
    fn unalloc(&mut self, i: usize) {
        if let Some(s) = self.slots[i].take() {
            self.set(s.wifi).remove(s.handle);
        }
    }

    /// The socket set and handle of a client's socket.
    fn slot(&mut self, client: usize, sock: u16, kind: Kind) -> Result<(&mut SocketSet<'static>, SocketHandle), u32> {
        match self.slots.get(sock as usize) {
            Some(Some(s)) if s.owner == Some(client) && s.kind == kind => {
                let (wifi, h) = (s.wifi, s.handle);
                Ok((self.set(wifi), h))
            }
            _ => Err(status::BAD_REQUEST),
        }
    }

    fn ephemeral(&mut self) -> u16 {
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        self.next_port
    }

    fn handle(&mut self, client: usize, mem: &mut [u8], m: &[u8]) -> Result<(u32, u32), u32> {
        if m.len() < proto::REQUEST_SIZE {
            return Err(status::BAD_REQUEST);
        }
        let code = m[0];
        let sock = u16::from_le_bytes([m[2], m[3]]);
        let a = u32::from_le_bytes([m[4], m[5], m[6], m[7]]);
        let b = u32::from_le_bytes([m[8], m[9], m[10], m[11]]);
        let addr = |a: u32| IpAddress::Ipv4(Ipv4Address::from_bits(a));
        match code {
            op::TCP_LISTEN => {
                let i = self.alloc(client, Kind::Tcp, false)?;
                let h = self.slots[i].unwrap().handle;
                if self.sockets.get_mut::<tcp::Socket>(h).listen(b as u16).is_err() {
                    self.unalloc(i);
                    return Err(status::ADDR_IN_USE);
                }
                Ok((i as u32, 0))
            }
            op::TCP_CONNECT => {
                let i = self.alloc(client, Kind::Tcp, !local(a))?;
                let slot = self.slots[i].unwrap();
                let port = self.ephemeral();
                let r = match (&mut self.wifi, slot.wifi) {
                    (Some(w), true) => w.sockets.get_mut::<tcp::Socket>(slot.handle).connect(w.iface.context(), (addr(a), b as u16), port),
                    _ => self.sockets.get_mut::<tcp::Socket>(slot.handle).connect(self.iface.context(), (addr(a), b as u16), port),
                };
                if r.is_err() {
                    self.unalloc(i);
                    return Err(status::UNREACHABLE);
                }
                Ok((i as u32, 0))
            }
            op::TCP_STATE => {
                let (set, h) = self.slot(client, sock, Kind::Tcp)?;
                let s = set.get::<tcp::Socket>(h);
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
                let (set, h) = self.slot(client, sock, Kind::Tcp)?;
                let s = set.get_mut::<tcp::Socket>(h);
                let n = (b as usize).min(mem.len());
                if !s.may_send() {
                    return Err(status::NOT_CONNECTED);
                }
                s.send_slice(&mem[..n]).map(|k| (k as u32, 0)).map_err(|_| status::NOT_CONNECTED)
            }
            op::RECV => {
                let (set, h) = self.slot(client, sock, Kind::Tcp)?;
                let s = set.get_mut::<tcp::Socket>(h);
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
                let set = self.set(slot.wifi);
                match slot.kind {
                    Kind::Tcp => set.get_mut::<tcp::Socket>(slot.handle).close(),
                    Kind::Udp => set.get_mut::<udp::Socket>(slot.handle).close(),
                }
                self.slots[i].as_mut().unwrap().owner = None;
                Ok((0, 0))
            }
            op::UDP_BIND => {
                let i = self.alloc(client, Kind::Udp, false)?;
                let h = self.slots[i].unwrap().handle;
                if self.sockets.get_mut::<udp::Socket>(h).bind(b as u16).is_err() {
                    self.unalloc(i);
                    return Err(status::ADDR_IN_USE);
                }
                Ok((i as u32, 0))
            }
            op::UDP_SEND => {
                let (set, h) = self.slot(client, sock, Kind::Udp)?;
                let s = set.get_mut::<udp::Socket>(h);
                let (port, n) = ((b >> 16) as u16, (b & 0xFFFF) as usize);
                let n = n.min(mem.len());
                match s.send_slice(&mem[..n], IpEndpoint::new(addr(a), port)) {
                    Ok(()) => Ok((n as u32, 0)),
                    Err(udp::SendError::BufferFull) => Ok((0, 0)),
                    Err(_) => Err(status::UNREACHABLE),
                }
            }
            op::UDP_RECV => {
                let (set, h) = self.slot(client, sock, Kind::Udp)?;
                let s = set.get_mut::<udp::Socket>(h);
                match s.recv_slice(&mut mem[proto::UDP_HEADER..]) {
                    Ok((n, meta)) => {
                        mem[..2].copy_from_slice(&meta.endpoint.port.to_le_bytes());
                        let IpAddress::Ipv4(src) = meta.endpoint.addr;
                        Ok((n as u32, src.to_bits()))
                    }
                    Err(_) => Ok((0, 0)),
                }
            }
            _ => Err(status::BAD_REQUEST),
        }
    }

    /// A client left: abort its sockets.
    fn drop_client(&mut self, client: usize) {
        for i in 0..SOCKETS {
            let Some(s) = self.slots[i] else { continue };
            if s.owner == Some(client) {
                let set = self.set(s.wifi);
                match s.kind {
                    Kind::Tcp => set.get_mut::<tcp::Socket>(s.handle).abort(),
                    Kind::Udp => set.get_mut::<udp::Socket>(s.handle).close(),
                }
                self.slots[i].as_mut().unwrap().owner = None;
            }
        }
    }
}

fn reply(status: u32, v0: u32, v1: u32) -> [u8; proto::REPLY_SIZE] {
    let mut r = [0u8; proto::REPLY_SIZE];
    r[0..4].copy_from_slice(&status.to_le_bytes());
    r[4..8].copy_from_slice(&v0.to_le_bytes());
    r[8..12].copy_from_slice(&v1.to_le_bytes());
    r
}

static mut DEV: Loopback = Loopback { frames: [[0; MTU]; RING], lens: [0; RING], head: 0, count: 0 };

/// `run net [ADDR [ROUTER]]`: with ADDR (IPv4 as a number) the Wi-Fi link
/// uses ADDR/24 and ROUTER instead of DHCP (for tests, doc/22 §7).
fn main(addr: usize, router: usize) -> usize {
    match run(addr as u32, router as u32) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "net.failed", step = step);
            step as usize
        }
    }
}

fn run(static_addr: u32, static_router: u32) -> Result<(), u32> {
    let pool_shm = Shm::alloc(CLIENTS * proto::SLICE).map_err(|_| 1u32)?;
    let pool = pool_shm.map().map_err(|_| 2u32)?;
    let l = Listener::open(proto::SERVICE).map_err(|_| 3u32)?;

    let lo: &'static mut Loopback = unsafe { &mut *core::ptr::addr_of_mut!(DEV) };
    let host = Handle::open(netdev::HOST, rights::READ | rights::WRITE | rights::WAIT).ok();
    let mut dev = Links { lo, host, to_host: 0, from_host: 0 };
    let iface = Interface::new(Config::new(HardwareAddress::Ip), &mut dev, now());
    let mut stack = Stack {
        iface,
        dev,
        sockets: SocketSet::new(unsafe { core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(STORAGE).cast::<SocketStorage<'static>>(), SOCKETS) }),
        wifi: None,
        slots: [None; SOCKETS],
        next_port: 49152,
    };
    let has_host = stack.dev.host.is_some();
    stack.iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        if has_host {
            let [x, y, z, w] = HOST_ADDR;
            let _ = a.push(IpCidr::new(IpAddress::v4(x, y, z, w), HOST_PREFIX));
        }
    });
    event!(Info, "net.ready", link = "lo", addr = "127.0.0.1/8", sockets = SOCKETS as u32);
    if has_host {
        event!(Info, "net.link", link = "host", addr = "10.55.0.2/24", mtu = netdev::MTU as u32);
    }
    stack.wifi = open_wifi();
    if static_addr != 0
        && let Some(w) = &mut stack.wifi
    {
        w.sockets.remove(w.dhcp);
        w.dhcp_off = true;
        w.iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::from_bits(static_addr)), 24));
        });
        if static_router != 0 {
            let _ = w.iface.routes_mut().add_default_ipv4_route(Ipv4Address::from_bits(static_router));
        }
        w.configured = true;
        event!(Info, "net.static", link = "wlan0", addr = static_addr, router = static_router);
    }

    let mut clients: [Option<Channel>; CLIENTS] = [const { None }; CLIENTS];
    let mut msg = [0u8; chan::MSG_MAX];
    loop {
        stack.poll();
        // The listener, the links (frames arriving), the clients.
        let mut hs = [0u32; 3 + CLIENTS];
        let mut who = [usize::MAX; 3 + CLIENTS];
        hs[0] = l.0.raw();
        let mut n = 1;
        for h in [stack.dev.host.as_ref(), stack.wifi.as_ref().map(|w| &w.dev.h)].into_iter().flatten() {
            hs[n] = h.raw();
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
        let mut delay = stack.iface.poll_delay(now(), &stack.sockets);
        if let Some(w) = &mut stack.wifi
            && let Some(d) = w.iface.poll_delay(now(), &w.sockets)
        {
            delay = Some(delay.map_or(d, |e| e.min(d)));
        }
        let timeout = delay.map_or(IDLE_NS, |d| (d.total_micros() * 1000).min(IDLE_NS)).max(1);
        let ev = match wait_raw(&hs[..n], timeout) {
            Ok(ev) => ev,
            Err(Error::TimedOut) => continue,
            Err(_) => return Err(4),
        };
        if ev.index > 0 && ev.index < links {
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
            let r = match stack.handle(i, mem, &msg[..len]) {
                Ok((v0, v1)) => reply(status::OK, v0, v1),
                Err(s) => reply(s, 0, 0),
            };
            // Let the stack act on the request before answering (a send
            // reaches the peer, a connect starts).
            stack.poll();
            let _ = c.send(&r);
        }
    }
}
