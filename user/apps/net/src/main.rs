#![no_std]
#![no_main]

//! The network service (N1, doc/research/net-stack.md): smoltcp in U-mode,
//! with only the loopback link (127.0.0.0/8) so far. Clients on `svc:net`
//! get a 64 KiB slice of shared memory for data and use TCP and UDP sockets
//! through `t5_abi::net` requests. No allocator: socket buffers are static.
//!
//! Built with `--permit shm --permit listen:net`.

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};
use t5_rt::net::{self as proto, op, state, status};
use t5_rt::{Channel, Error, Listener, Shm, chan, event, time_ns, wait_raw};

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

// ---- socket storage (static: there is no allocator) ----

static mut TCP_RX: [[u8; TCP_BUF]; SOCKETS] = [[0; TCP_BUF]; SOCKETS];
static mut TCP_TX: [[u8; TCP_BUF]; SOCKETS] = [[0; TCP_BUF]; SOCKETS];
static mut UDP_RX: [[u8; UDP_BUF]; SOCKETS] = [[0; UDP_BUF]; SOCKETS];
static mut UDP_TX: [[u8; UDP_BUF]; SOCKETS] = [[0; UDP_BUF]; SOCKETS];
static mut UDP_RX_META: [[udp::PacketMetadata; UDP_PACKETS]; SOCKETS] = [[udp::PacketMetadata::EMPTY; UDP_PACKETS]; SOCKETS];
static mut UDP_TX_META: [[udp::PacketMetadata; UDP_PACKETS]; SOCKETS] = [[udp::PacketMetadata::EMPTY; UDP_PACKETS]; SOCKETS];
static mut STORAGE: [SocketStorage<'static>; SOCKETS] = [SocketStorage::EMPTY; SOCKETS];

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
    /// The client that owns it; None once closed, until the stack is done.
    owner: Option<usize>,
}

struct Stack {
    iface: Interface,
    dev: &'static mut Loopback,
    sockets: SocketSet<'static>,
    slots: [Option<Slot>; SOCKETS],
    next_port: u16,
}

fn now() -> Instant {
    Instant::from_micros((time_ns() / 1000) as i64)
}

impl Stack {
    /// Runs the stack until nothing moves.
    fn poll(&mut self) {
        for _ in 0..256 {
            let r = self.iface.poll(now(), &mut *self.dev, &mut self.sockets);
            if r == PollResult::None && self.dev.is_empty() {
                break;
            }
        }
        // Closed sockets the stack has finished with go back to the pool.
        for s in self.slots.iter_mut() {
            if let Some(slot) = s
                && slot.owner.is_none()
            {
                let done = match slot.kind {
                    Kind::Tcp => matches!(self.sockets.get::<tcp::Socket>(slot.handle).state(), tcp::State::Closed | tcp::State::TimeWait),
                    Kind::Udp => true,
                };
                if done {
                    self.sockets.remove(slot.handle);
                    *s = None;
                }
            }
        }
    }

    fn alloc(&mut self, client: usize, kind: Kind) -> Result<usize, u32> {
        let i = self.slots.iter().position(Option::is_none).ok_or(status::NO_SOCKETS)?;
        let handle = match kind {
            Kind::Tcp => self.sockets.add(tcp_socket(i)),
            Kind::Udp => self.sockets.add(udp_socket(i)),
        };
        self.slots[i] = Some(Slot { handle, kind, owner: Some(client) });
        Ok(i)
    }

    fn slot(&self, client: usize, sock: u16, kind: Kind) -> Result<SocketHandle, u32> {
        match self.slots.get(sock as usize) {
            Some(Some(s)) if s.owner == Some(client) && s.kind == kind => Ok(s.handle),
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
                let i = self.alloc(client, Kind::Tcp)?;
                let h = self.slots[i].unwrap().handle;
                if self.sockets.get_mut::<tcp::Socket>(h).listen(b as u16).is_err() {
                    self.sockets.remove(h);
                    self.slots[i] = None;
                    return Err(status::ADDR_IN_USE);
                }
                Ok((i as u32, 0))
            }
            op::TCP_CONNECT => {
                let i = self.alloc(client, Kind::Tcp)?;
                let h = self.slots[i].unwrap().handle;
                let local = self.ephemeral();
                let r = self.sockets.get_mut::<tcp::Socket>(h).connect(self.iface.context(), (addr(a), b as u16), local);
                if r.is_err() {
                    self.sockets.remove(h);
                    self.slots[i] = None;
                    return Err(status::UNREACHABLE);
                }
                Ok((i as u32, 0))
            }
            op::TCP_STATE => {
                let s = self.sockets.get::<tcp::Socket>(self.slot(client, sock, Kind::Tcp)?);
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
                let s = self.sockets.get_mut::<tcp::Socket>(self.slot(client, sock, Kind::Tcp)?);
                let n = (b as usize).min(mem.len());
                if !s.may_send() {
                    return Err(status::NOT_CONNECTED);
                }
                s.send_slice(&mem[..n]).map(|k| (k as u32, 0)).map_err(|_| status::NOT_CONNECTED)
            }
            op::RECV => {
                let s = self.sockets.get_mut::<tcp::Socket>(self.slot(client, sock, Kind::Tcp)?);
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
                match slot.kind {
                    Kind::Tcp => self.sockets.get_mut::<tcp::Socket>(slot.handle).close(),
                    Kind::Udp => self.sockets.get_mut::<udp::Socket>(slot.handle).close(),
                }
                self.slots[i].as_mut().unwrap().owner = None;
                Ok((0, 0))
            }
            op::UDP_BIND => {
                let i = self.alloc(client, Kind::Udp)?;
                let h = self.slots[i].unwrap().handle;
                if self.sockets.get_mut::<udp::Socket>(h).bind(b as u16).is_err() {
                    self.sockets.remove(h);
                    self.slots[i] = None;
                    return Err(status::ADDR_IN_USE);
                }
                Ok((i as u32, 0))
            }
            op::UDP_SEND => {
                let s = self.sockets.get_mut::<udp::Socket>(self.slot(client, sock, Kind::Udp)?);
                let (port, n) = ((b >> 16) as u16, (b & 0xFFFF) as usize);
                let n = n.min(mem.len());
                match s.send_slice(&mem[..n], IpEndpoint::new(addr(a), port)) {
                    Ok(()) => Ok((n as u32, 0)),
                    Err(udp::SendError::BufferFull) => Ok((0, 0)),
                    Err(_) => Err(status::UNREACHABLE),
                }
            }
            op::UDP_RECV => {
                let s = self.sockets.get_mut::<udp::Socket>(self.slot(client, sock, Kind::Udp)?);
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
        for s in self.slots.iter_mut().flatten() {
            if s.owner == Some(client) {
                match s.kind {
                    Kind::Tcp => self.sockets.get_mut::<tcp::Socket>(s.handle).abort(),
                    Kind::Udp => self.sockets.get_mut::<udp::Socket>(s.handle).close(),
                }
                s.owner = None;
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

fn main(_: usize, _: usize) -> usize {
    match run() {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "net.failed", step = step);
            step as usize
        }
    }
}

fn run() -> Result<(), u32> {
    let pool_shm = Shm::alloc(CLIENTS * proto::SLICE).map_err(|_| 1u32)?;
    let pool = pool_shm.map().map_err(|_| 2u32)?;
    let l = Listener::open(proto::SERVICE).map_err(|_| 3u32)?;

    let dev: &'static mut Loopback = unsafe { &mut *core::ptr::addr_of_mut!(DEV) };
    let iface = Interface::new(Config::new(HardwareAddress::Ip), &mut *dev, now());
    let mut stack = Stack {
        iface,
        dev,
        sockets: SocketSet::new(unsafe { core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(STORAGE).cast::<SocketStorage<'static>>(), SOCKETS) }),
        slots: [None; SOCKETS],
        next_port: 49152,
    };
    stack.iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
    });
    event!(Info, "net.ready", link = "lo", addr = "127.0.0.1/8", sockets = SOCKETS as u32);

    let mut clients: [Option<Channel>; CLIENTS] = [const { None }; CLIENTS];
    let mut msg = [0u8; chan::MSG_MAX];
    loop {
        stack.poll();
        let mut hs = [0u32; 1 + CLIENTS];
        let mut who = [0usize; 1 + CLIENTS];
        hs[0] = l.0.raw();
        let mut n = 1;
        for (i, c) in clients.iter().enumerate() {
            if let Some(c) = c {
                hs[n] = c.0.raw();
                who[n] = i;
                n += 1;
            }
        }
        let timeout = stack
            .iface
            .poll_delay(now(), &stack.sockets)
            .map_or(IDLE_NS, |d| (d.total_micros() * 1000).min(IDLE_NS))
            .max(1);
        let ev = match wait_raw(&hs[..n], timeout) {
            Ok(ev) => ev,
            Err(Error::TimedOut) => continue,
            Err(_) => return Err(4),
        };
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
