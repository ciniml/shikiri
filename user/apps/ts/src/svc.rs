//! `svc:ts` (doc/71 §13): TCP and UDP on the tailnet for other apps, in
//! `svc:net`'s protocol (`t5_abi::net`), so `Net::connect_to("ts")` serves
//! them unchanged. Of its requests: TCP_CONNECT, TCP_LISTEN (on the
//! tailnet address; the address and link are ignored), TCP_STATE, SEND,
//! RECV, CLOSE, UDP_BIND (the same; port 0 takes a free one), UDP_SEND,
//! UDP_RECV, UDP_WAIT (doc/71 §14.6), and RESOLVE / RESOLVE_RESULT for the
//! netmap's names (a peer's short name or its whole name, IPv4), IF_INFO
//! (one interface, `ts0`: the tailnet address, packets in and out),
//! PEER_INFO (this node and the peers: name, address, path; doc/71 §14.8). The
//! rest answer BAD_REQUEST.
//!
//! Grants (`svc:ts/GRANT`) as `svc:net`'s: `tcp-connect` or
//! `tcp-connect:PORT`, `tcp-listen:PORT` or `tcp-listen:any`, `udp:PORT`
//! or `udp:any`, `resolve`, `info`.
//!
//! The sockets are smoltcp's in the node; a new session with the control
//! server and DERP (after a failure) starts with none, and the clients'
//! old socket numbers then answer NOT_CONNECTED.

use alloc::vec;
use alloc::vec::Vec;
use t5_rt::net::{self as proto, op, state, status};
use t5_rt::{Channel, Grants, Handle, Listener, Shm, event};
use tsclient::node::Node;
use tsclient::smoltcp::iface::SocketHandle;
use tsclient::smoltcp::socket::{tcp, udp};
use tsclient::smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

/// Clients at a time, each with `proto::SLICE` of shared memory.
pub const CLIENTS: usize = 2;
/// Sockets of all clients (each holds `BUF` twice).
const SOCKETS: usize = 8;
const BUF: usize = 4096;
/// Datagrams a UDP socket holds each way.
const DATAGRAMS: usize = 4;

/// A client's socket, or the result of a RESOLVE.
enum Sock {
    Tcp(SocketHandle),
    Udp(SocketHandle),
    Resolved(Option<[u8; 4]>),
}

struct Client {
    chan: Channel,
    grants: Grants,
    /// By socket number - 1.
    socks: Vec<Option<Sock>>,
    /// A UDP_WAIT not answered yet.
    udp_waiting: bool,
}

pub struct Service {
    listener: Listener,
    pool_shm: Shm,
    pool: &'static mut [u8],
    clients: [Option<Client>; CLIENTS],
    /// Closed by their clients, finishing in smoltcp: removed once done.
    closing: Vec<SocketHandle>,
    /// TCP sockets in the node, closing ones too.
    used: usize,
    next_port: u16,
}

fn reply(st: u32, v0: u32, v1: u32) -> [u8; proto::REPLY_SIZE] {
    let mut r = [0u8; proto::REPLY_SIZE];
    r[0..4].copy_from_slice(&st.to_le_bytes());
    r[4..8].copy_from_slice(&v0.to_le_bytes());
    r[8..12].copy_from_slice(&v1.to_le_bytes());
    r
}

impl Service {
    pub fn open() -> Option<Service> {
        let pool_shm = Shm::alloc(CLIENTS * proto::SLICE).ok()?;
        let pool = pool_shm.map().ok()?;
        let listener = Listener::open("ts").ok()?;
        Some(Service { listener, pool_shm, pool, clients: [const { None }; CLIENTS], closing: Vec::new(), used: 0, next_port: 49152 })
    }

    /// The handles to `wait` on: the listener, then the clients.
    pub fn handles(&self) -> Vec<&Handle> {
        let mut h = vec![&self.listener.0];
        h.extend(self.clients.iter().flatten().map(|c| &c.chan.0));
        h
    }

    /// `wait` on `handles()` said the handle at `index` closed: its client
    /// went away (a read only says there is nothing).
    pub fn closed(&mut self, index: usize, node: &mut Node) {
        let Some(i) = (0..CLIENTS).filter(|&i| self.clients[i].is_some()).nth(index.wrapping_sub(1)) else { return };
        self.drop_client(i, node);
    }

    /// A new node (a new session): the clients' sockets are gone.
    pub fn reset(&mut self) {
        for c in self.clients.iter_mut().flatten() {
            c.socks.clear();
        }
        self.closing.clear();
        self.used = 0;
    }

    /// Takes new clients and answers every request waiting.
    pub fn serve(&mut self, node: &mut Node) -> bool {
        let mut busy = false;
        while let Ok(c) = self.listener.accept() {
            busy = true;
            let Some(i) = self.clients.iter().position(Option::is_none) else { continue };
            if self.pool_shm.grant(&c, i * proto::SLICE, proto::SLICE).is_ok() {
                self.clients[i] = Some(Client { grants: c.peer_grants(), chan: c, socks: Vec::new(), udp_waiting: false });
            }
        }
        let mut msg = [0u8; proto::REQUEST_SIZE_V6];
        for i in 0..CLIENTS {
            loop {
                let Some(c) = self.clients[i].as_ref() else { break };
                let len = match c.chan.0.read(&mut msg) {
                    Ok(n) => n,
                    Err(t5_rt::Error::Busy) => break,
                    Err(_) => {
                        // The client is gone.
                        self.drop_client(i, node);
                        break;
                    }
                };
                busy = true;
                // A UDP_WAIT still out is answered (0) before this
                // request's reply, keeping the replies in order.
                let c = self.clients[i].as_mut().unwrap();
                if core::mem::take(&mut c.udp_waiting) {
                    let _ = c.chan.send(&reply(status::OK, 0, 0));
                }
                if let Some(r) = self.request(i, &msg[..len], node) {
                    let _ = self.clients[i].as_ref().unwrap().chan.send(&r);
                }
            }
            // A datagram for a client waiting for one.
            if let Some(c) = self.clients[i].as_mut().filter(|c| c.udp_waiting && udp_ready(c, node)) {
                c.udp_waiting = false;
                let _ = c.chan.send(&reply(status::OK, 1, 0));
            }
        }
        // Sockets closed by their clients that have finished.
        let sockets = &mut node.tun.sockets;
        let before = self.closing.len();
        self.closing.retain(|&h| {
            let done = matches!(sockets.get::<tcp::Socket>(h).state(), tcp::State::Closed | tcp::State::TimeWait);
            if done {
                sockets.remove(h);
            }
            !done
        });
        self.used -= before - self.closing.len();
        busy
    }

    fn drop_client(&mut self, i: usize, node: &mut Node) {
        if let Some(c) = self.clients[i].take() {
            for s in c.socks.into_iter().flatten() {
                match s {
                    Sock::Tcp(h) => {
                        node.tun.sockets.get_mut::<tcp::Socket>(h).abort();
                        self.closing.push(h);
                    }
                    Sock::Udp(h) => {
                        node.tun.sockets.remove(h);
                        self.used -= 1;
                    }
                    Sock::Resolved(_) => {}
                }
            }
        }
    }

    /// The reply to request `m` of client `i`; None for a UDP_WAIT
    /// answered later.
    fn request(&mut self, i: usize, m: &[u8], node: &mut Node) -> Option<[u8; proto::REPLY_SIZE]> {
        if m.len() < proto::REQUEST_SIZE {
            return Some(reply(status::BAD_REQUEST, 0, 0));
        }
        if m[0] == op::UDP_WAIT {
            let c = self.clients[i].as_mut().unwrap();
            if udp_ready(c, node) {
                return Some(reply(status::OK, 1, 0));
            }
            c.udp_waiting = true;
            return None;
        }
        Some(self.answer(i, m, node))
    }

    fn answer(&mut self, i: usize, m: &[u8], node: &mut Node) -> [u8; proto::REPLY_SIZE] {
        let u = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
        let (code, sock, a, b) = (m[0], u16::from_le_bytes([m[2], m[3]]), u(4), u(8));
        if m[1] == proto::FAMILY_V6 {
            return reply(status::BAD_REQUEST, 0, 0);
        }
        if let Some(what) = self.refused(i, code, b as u16) {
            let mut name = [0u8; t5_rt::t5a::MAX_NAME];
            let c = self.clients[i].as_ref().unwrap();
            let n = c.chan.peer_name(&mut name);
            event!(Warn, "ts.denied", app = core::str::from_utf8(&name[..n]).unwrap_or("?"), op = what);
            return reply(status::DENIED, 0, 0);
        }
        let mem = &mut self.pool[i * proto::SLICE..(i + 1) * proto::SLICE];
        let c = self.clients[i].as_mut().unwrap();
        let tcp_of = |c: &Client| match c.socks.get((sock as usize).wrapping_sub(1)) {
            Some(Some(Sock::Tcp(h))) => Some(*h),
            _ => None,
        };
        let udp_of = |c: &Client| match c.socks.get((sock as usize).wrapping_sub(1)) {
            Some(Some(Sock::Udp(h))) => Some(*h),
            _ => None,
        };
        match code {
            op::TCP_CONNECT | op::TCP_LISTEN => {
                let Some(me) = node.tun.ip else { return reply(status::NO_ADDRESS, 0, 0) };
                if self.used == SOCKETS {
                    return reply(status::NO_SOCKETS, 0, 0);
                }
                let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; BUF]), tcp::SocketBuffer::new(vec![0; BUF]));
                let ok = if code == op::TCP_CONNECT {
                    let ip = a.to_be_bytes();
                    let port = self.next_port;
                    self.next_port = if port == 65535 { 49152 } else { port + 1 };
                    let local = (IpAddress::Ipv4(Ipv4Address::new(me[0], me[1], me[2], me[3])), port);
                    s.connect(node.tun.iface.context(), (IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3])), b as u16), local).is_ok()
                } else {
                    s.listen(b as u16).is_ok()
                };
                if !ok {
                    return reply(status::BAD_REQUEST, 0, 0);
                }
                let h = node.tun.sockets.add(s);
                self.used += 1;
                reply(status::OK, add(c, Sock::Tcp(h)) as u32, 0)
            }
            op::TCP_STATE => {
                let Some(h) = tcp_of(c) else { return reply(status::NOT_CONNECTED, 0, 0) };
                let s = node.tun.sockets.get::<tcp::Socket>(h);
                let st = match s.state() {
                    tcp::State::Closed | tcp::State::TimeWait => state::CLOSED,
                    tcp::State::Listen => state::LISTEN,
                    tcp::State::SynSent | tcp::State::SynReceived => state::CONNECTING,
                    tcp::State::Established => state::ESTABLISHED,
                    _ => state::CLOSING,
                };
                reply(status::OK, st, s.recv_queue() as u32)
            }
            op::SEND => {
                let Some(h) = tcp_of(c) else { return reply(status::NOT_CONNECTED, 0, 0) };
                let s = node.tun.sockets.get_mut::<tcp::Socket>(h);
                let n = (b as usize).min(mem.len());
                if !s.may_send() {
                    return reply(status::NOT_CONNECTED, 0, 0);
                }
                match s.send_slice(&mem[..n]) {
                    Ok(k) => reply(status::OK, k as u32, 0),
                    Err(_) => reply(status::NOT_CONNECTED, 0, 0),
                }
            }
            op::RECV => {
                let Some(h) = tcp_of(c) else { return reply(status::NOT_CONNECTED, 0, 0) };
                let s = node.tun.sockets.get_mut::<tcp::Socket>(h);
                let n = (b as usize).min(mem.len());
                if s.can_recv() {
                    return match s.recv_slice(&mut mem[..n]) {
                        Ok(k) => reply(status::OK, k as u32, 0),
                        Err(_) => reply(status::NOT_CONNECTED, 0, 0),
                    };
                }
                let ended = !s.may_recv() && !matches!(s.state(), tcp::State::SynSent | tcp::State::SynReceived | tcp::State::Listen);
                reply(status::OK, 0, ended as u32)
            }
            op::UDP_BIND => {
                if node.tun.ip.is_none() {
                    return reply(status::NO_ADDRESS, 0, 0);
                }
                if self.used == SOCKETS {
                    return reply(status::NO_SOCKETS, 0, 0);
                }
                let taken = |p: u16| self.clients.iter().flatten().flat_map(|c| c.socks.iter().flatten()).any(|s| matches!(s, Sock::Udp(h) if node.tun.sockets.get::<udp::Socket>(*h).endpoint().port == p));
                let port = match b as u16 {
                    0 => loop {
                        let p = self.next_port;
                        self.next_port = if p == 65535 { 49152 } else { p + 1 };
                        if !taken(p) {
                            break p;
                        }
                    },
                    p if taken(p) => return reply(status::ADDR_IN_USE, 0, 0),
                    p => p,
                };
                let buf = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; DATAGRAMS], vec![0; BUF]);
                let mut s = udp::Socket::new(buf(), buf());
                if s.bind(port).is_err() {
                    return reply(status::BAD_REQUEST, 0, 0);
                }
                let h = node.tun.sockets.add(s);
                self.used += 1;
                let c = self.clients[i].as_mut().unwrap();
                reply(status::OK, add(c, Sock::Udp(h)) as u32, port as u32)
            }
            op::UDP_SEND => {
                let Some(h) = udp_of(c) else { return reply(status::NOT_CONNECTED, 0, 0) };
                let s = node.tun.sockets.get_mut::<udp::Socket>(h);
                let (ip, port, n) = (a.to_be_bytes(), (b >> 16) as u16, ((b & 0xFFFF) as usize).min(mem.len()));
                match s.send_slice(&mem[..n], IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3])), port)) {
                    Ok(()) => reply(status::OK, n as u32, 0),
                    Err(udp::SendError::BufferFull) => reply(status::OK, 0, 0),
                    Err(_) => reply(status::UNREACHABLE, 0, 0),
                }
            }
            op::UDP_RECV => {
                let Some(h) = udp_of(c) else { return reply(status::NOT_CONNECTED, 0, 0) };
                let s = node.tun.sockets.get_mut::<udp::Socket>(h);
                match s.recv_slice(&mut mem[proto::UDP_HEADER..]) {
                    Ok((n, meta)) => {
                        let IpAddress::Ipv4(from) = meta.endpoint.addr;
                        mem[..2].copy_from_slice(&meta.endpoint.port.to_le_bytes());
                        reply(status::OK, n as u32, u32::from_be_bytes(from.octets()))
                    }
                    Err(_) => reply(status::OK, 0, 0),
                }
            }
            op::CLOSE => match c.socks.get_mut((sock as usize).wrapping_sub(1)).and_then(Option::take) {
                Some(Sock::Tcp(h)) => {
                    node.tun.sockets.get_mut::<tcp::Socket>(h).close();
                    self.closing.push(h);
                    reply(status::OK, 0, 0)
                }
                Some(Sock::Udp(h)) => {
                    node.tun.sockets.remove(h);
                    self.used -= 1;
                    reply(status::OK, 0, 0)
                }
                Some(Sock::Resolved(_)) => reply(status::OK, 0, 0),
                None => reply(status::BAD_REQUEST, 0, 0),
            },
            op::RESOLVE => {
                let n = (b as usize).min(proto::NAME_MAX);
                let name = core::str::from_utf8(&mem[..n]).unwrap_or("");
                let ip = resolve(node, name);
                reply(status::OK, add(c, Sock::Resolved(ip)) as u32, 0)
            }
            op::RESOLVE_RESULT => match c.socks.get((sock as usize).wrapping_sub(1)) {
                Some(Some(Sock::Resolved(Some(ip)))) => reply(status::OK, 1, u32::from_be_bytes(*ip)),
                Some(Some(Sock::Resolved(None))) => reply(status::UNREACHABLE, 0, 0),
                _ => reply(status::BAD_REQUEST, 0, 0),
            },
            op::IF_INFO if a > 0 => reply(status::NO_ADDRESS, 0, 0),
            op::IF_INFO => {
                let Some(ip) = node.tun.ip else { return reply(status::OK, 0, 0) };
                let r = &mut mem[..proto::iface::HEADER];
                r.fill(0);
                r[..3].copy_from_slice(b"ts0");
                r[14] = 32;
                r[16..20].copy_from_slice(&u32::from_be_bytes(ip).to_le_bytes());
                r[28..32].copy_from_slice(&node.stats.packets_in.to_le_bytes());
                r[32..36].copy_from_slice(&node.stats.packets_out.to_le_bytes());
                reply(status::OK, 1, proto::iface::HEADER as u32)
            }
            op::PEER_INFO => match peer_info(node, a as usize, &mut mem[..proto::peer::RECORD]) {
                true => reply(status::OK, 1, proto::peer::RECORD as u32),
                false => reply(status::NO_ADDRESS, 0, 0),
            },
            _ => reply(status::BAD_REQUEST, 0, 0),
        }
    }

    /// What the client's grants do not allow; None if they do.
    fn refused(&self, i: usize, code: u8, port: u16) -> Option<&'static str> {
        let g = &self.clients[i].as_ref()?.grants;
        let port_ok = |kind: &[u8]| g.values(kind).any(|v| v == b"any" || core::str::from_utf8(v).ok().and_then(|v| v.parse::<u16>().ok()) == Some(port));
        let (ok, what) = match code {
            op::UDP_BIND => (port_ok(b"udp"), "udp"),
            op::TCP_LISTEN => (port_ok(b"tcp-listen"), "tcp-listen"),
            op::TCP_CONNECT => (g.has(b"tcp-connect") || port_ok(b"tcp-connect"), "tcp-connect"),
            op::RESOLVE | op::RESOLVE_RESULT => (g.has(b"resolve"), "resolve"),
            op::IF_INFO | op::PEER_INFO => (g.has(b"info"), "info"),
            _ => (true, ""),
        };
        (!ok).then_some(what)
    }
}

/// A datagram waits on one of the client's UDP sockets.
fn udp_ready(c: &Client, node: &Node) -> bool {
    c.socks.iter().flatten().any(|s| matches!(s, Sock::Udp(h) if node.tun.sockets.get::<udp::Socket>(*h).can_recv()))
}

/// A free socket number (from 1) for `s`.
fn add(c: &mut Client, s: Sock) -> usize {
    match c.socks.iter().position(Option::is_none) {
        Some(k) => {
            c.socks[k] = Some(s);
            k + 1
        }
        None => {
            c.socks.push(Some(s));
            c.socks.len()
        }
    }
}

/// PEER_INFO: the record (`net::peer`) of this node (`k` = 0) or peer
/// `k` - 1 in `r`; false past the last.
fn peer_info(node: &Node, k: usize, r: &mut [u8]) -> bool {
    use proto::peer;
    let Some(p) = (if k == 0 { node.netmap.node.as_ref() } else { node.netmap.peers.get(k - 1) }) else { return false };
    let now = crate::now_ms();
    r.fill(0);
    let name = p.name.split('.').next().unwrap_or("").as_bytes();
    let n = name.len().min(peer::NAME);
    r[..n].copy_from_slice(&name[..n]);
    r[48..52].copy_from_slice(&u32::from_be_bytes(p.ipv4().unwrap_or([0; 4])).to_le_bytes());
    let mut flags = match p.online {
        Some(true) => peer::ONLINE,
        Some(false) => peer::OFFLINE,
        None => 0,
    };
    if k == 0 {
        flags |= peer::SELF;
    }
    let key = p.node_key();
    if let Some((ip, port)) = key.and_then(|key| node.direct_path(&key, now)) {
        flags |= peer::DIRECT;
        r[54..56].copy_from_slice(&port.to_le_bytes());
        r[56..60].copy_from_slice(&u32::from_be_bytes(ip).to_le_bytes());
    }
    r[52] = flags;
    let region = if k == 0 { node.home_region } else { p.home_region() };
    if let Some((reg, _)) = node.netmap.derp_region(region) {
        let code = reg.region_code.as_bytes();
        let n = code.len().min(8);
        r[60..60 + n].copy_from_slice(&code[..n]);
    }
    let idle = key.and_then(|key| node.last_active(&key)).map_or(u32::MAX, |t| (now.saturating_sub(t) / 1000) as u32);
    r[68..72].copy_from_slice(&idle.to_le_bytes());
    true
}

/// A netmap name's IPv4 address: this node or a peer, by its short name
/// (`laptop`) or whole name (`laptop.example.ts.net`, a final dot or not).
fn resolve(node: &Node, name: &str) -> Option<[u8; 4]> {
    let name = name.trim_end_matches('.');
    let matches = |n: &str| {
        let n = n.trim_end_matches('.');
        !name.is_empty() && (n.eq_ignore_ascii_case(name) || n.split('.').next().is_some_and(|s| s.eq_ignore_ascii_case(name)))
    };
    node.netmap.node.iter().chain(node.netmap.peers.iter()).find(|p| matches(&p.name)).and_then(|p| p.ipv4())
}
