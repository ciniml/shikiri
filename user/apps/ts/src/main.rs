#![no_std]
#![no_main]

//! A Tailscale client (doc/71), stages T1 to T4a: registers this node with
//! the control server and follows its map, keeps a connection to the home
//! DERP server (the nearest region by STUN), and talks IP with the peers
//! over WireGuard through DERP (smoltcp: pings, and a status page on TCP
//! port 80; other apps' TCP through `svc:ts`, `svc.rs`). The protocols
//! are in `tsclient`; here are the
//! TCP (`svc:net`), TLS (embedded-tls), the settings, the keys and the
//! loop that serves both connections (each read only when bytes wait).
//!
//! Settings (`svc:cfg`): `ts.authkey` (secret: a pre-auth key; without
//! one the control server gives a login URL, logged as `ts.login`),
//! `ts.hostname` (default `tab5`), `ts.server` (default
//! `controlplane.tailscale.com`). The machine, node and disco keys are
//! made on the first start and kept as the secret `ts.keys`.
//!
//! `run ts`. Exits only when it cannot start (the step); otherwise retries
//! after failures.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use embedded_tls::blocking::TlsConnection;
use embedded_tls::{TlsConfig, TlsContext};
use linked_list_allocator::LockedHeap;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use t5_rt::{Config, Net, event, random_fill, sleep_ms, time_ns, wait};
use tsclient::derp::{Derp, Frame};
use tsclient::netcheck::Netcheck;
use tsclient::smoltcp::iface::SocketHandle;
use tsclient::smoltcp::socket::tcp;
use tsclient::node::{Dest, Node};
use tsclient::stream::Buffered;
use tsclient::{Error, control, keys::Keys, tls};

mod svc;

t5_rt::entry!(main);

/// The map responses' JSON and the TLS buffers. A process has 2 MiB in
/// all (text, data, the 256 KiB stack too: embedded-tls's handshake and
/// the RSA checks overran 64 KiB into the heap); the first map response of
/// an 11-peer tailnet was 33 KB (doc/71 §9.2).
const HEAP_BYTES: usize = 1 << 20;
static mut ARENA: [u8; HEAP_BYTES] = [0; HEAP_BYTES];

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

const DEFAULT_SERVER: &str = "controlplane.tailscale.com";
const DEFAULT_HOSTNAME: &str = "tab5";
const RETRY_MS: u64 = 30_000;
/// Re-registering while a person has not approved the node yet.
const LOGIN_POLL_MS: u64 = 20_000;
const RESOLVE_MS: u64 = 5000;
const CONNECT_MS: u64 = 10_000;
/// The loop's nap when neither connection has bytes waiting: doubles
/// while idle up to `IDLE_MAX_MS` (each look costs `net` two calls; at
/// 2 ms `ts` took 5 % of a core and `net` 8 % more), back to 1 on traffic.
const IDLE_MAX_MS: u64 = 20;
/// The UDP port (Tailscale's usual): STUN now, WireGuard directly later.
const UDP_PORT: u16 = 41641;
const NETCHECK_ROUNDS: usize = 3;
const NETCHECK_ROUND_MS: u64 = 700;

fn main(_: usize, _: usize) -> usize {
    unsafe { HEAP.lock().init(core::ptr::addr_of_mut!(ARENA) as *mut u8, HEAP_BYTES) };
    let mut seed = [0u8; 32];
    if random_fill(&mut seed).is_err() {
        event!(Error, "ts.failed", step = "random");
        return 1;
    }
    let mut rng = ChaCha20Rng::from_seed(seed);
    let Ok(cfg) = Config::connect_within(5000) else {
        event!(Error, "ts.failed", step = "cfg");
        return 2;
    };
    let mut buf = [0u8; t5_rt::cfg::VALUE_MAX];
    let mut text = |key: &str, default: &str| -> String {
        match cfg.get(key, &mut buf) {
            Ok(Some(v)) if v.len > 0 => String::from_utf8_lossy(&buf[..v.len]).into_owned(),
            _ => default.into(),
        }
    };
    let server: &'static str = Box::leak(text("ts.server", DEFAULT_SERVER).into_boxed_str());
    let hostname = text("ts.hostname", DEFAULT_HOSTNAME);
    let auth = text("ts.authkey", "");
    let keys = match cfg.get("ts.keys", &mut buf) {
        Ok(Some(v)) if v.len == 96 => Keys::from_bytes(buf[..96].try_into().unwrap()),
        _ => {
            let k = Keys::generate(&mut rng);
            if cfg.set("ts.keys", &k.to_bytes(), true).is_err() {
                event!(Error, "ts.failed", step = "save keys");
                return 3;
            }
            event!(Info, "ts.new_keys");
            k
        }
    };
    drop(cfg);
    // The node key (public) is what `tailscale lock sign` takes.
    let node = tsclient::keys::to_hex("nodekey:", &keys.node.public);
    event!(Info, "ts.start", server = server, hostname = hostname.as_str(), auth_key = !auth.is_empty(), node = node.as_str());
    // Other apps' TCP on the tailnet (doc/71 §13).
    let mut svc = svc::Service::open();
    if svc.is_none() {
        event!(Warn, "ts.failed", step = "svc", error = "io");
    }
    loop {
        let e = run(&mut rng, server, &hostname, &auth, &keys, svc.as_mut());
        let (step, error) = e;
        event!(Warn, "ts.failed", step = step, error = describe(error));
        sleep_ms(RETRY_MS);
    }
}

fn describe(e: Error) -> &'static str {
    match e {
        Error::Io => "io",
        Error::Http(_) => "http",
        Error::Proto(p) => p,
        Error::Noise => "noise",
        Error::Json => "json",
        Error::Size => "size",
    }
}

/// A TCP connection through `svc:net` (shared by the connections), as
/// embedded-io for embedded-tls.
struct Sock {
    net: Rc<RefCell<Net>>,
    s: u16,
}

/// Bytes wait on socket `s`, or it is no longer up (a read then fails
/// rather than waits).
fn waiting(net: &Rc<RefCell<Net>>, s: u16) -> bool {
    match net.borrow().tcp_state(s) {
        Ok((state, n)) => n > 0 || state != t5_rt::net::state::ESTABLISHED,
        Err(_) => true,
    }
}

#[derive(Debug)]
struct SockError;

impl core::fmt::Display for SockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("socket")
    }
}

impl core::error::Error for SockError {}

impl embedded_io::Error for SockError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl embedded_io::ErrorType for Sock {
    type Error = SockError;
}

impl embedded_io::Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SockError> {
        self.net.borrow_mut().recv(self.s, buf).map_err(|_| SockError)
    }
}

impl embedded_io::Write for Sock {
    fn write(&mut self, buf: &[u8]) -> Result<usize, SockError> {
        self.net.borrow_mut().send(self.s, buf).map_err(|_| SockError)
    }

    fn flush(&mut self) -> Result<(), SockError> {
        Ok(())
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = self.net.borrow().close(self.s);
    }
}

type Tls<'a> = Buffered<TlsConnection<'a, Sock, tls::Suite>>;

/// A TLS connection's record buffers (read, write).
struct Bufs(Vec<u8>, Vec<u8>);

impl Bufs {
    fn new() -> Bufs {
        Bufs(vec![0; tls::READ_BUF], vec![0; tls::WRITE_BUF])
    }
}

fn now_ms() -> u64 {
    time_ns() / 1_000_000
}

/// TLS to `host` (at `ip`, `port`), its certificate checked, with the
/// record buffers `bufs`; and the socket's number (for `waiting`).
fn tls_open<'a>(net: &Rc<RefCell<Net>>, bufs: &'a mut Bufs, ip: u32, port: u16, host: &str, rng: &mut ChaCha20Rng) -> Result<(Tls<'a>, u16), (&'static str, Error)> {
    let s = net.borrow().tcp_connect(ip, port).map_err(|_| ("tcp", Error::Io))?;
    let sock = Sock { net: net.clone(), s };
    if net.borrow().wait_established(s, CONNECT_MS).is_err() {
        return Err(("tcp", Error::Io));
    }
    let mut t = TlsConnection::new(sock, &mut bufs.0, &mut bufs.1);
    let config = TlsConfig::new().with_server_name(host);
    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);
    t.open(TlsContext::new(&config, tls::Provider::new(ChaCha20Rng::from_seed(seed)))).map_err(|_| ("tls", Error::Io))?;
    Ok((Buffered::new(t), s))
}

/// One session with the control server and DERP, until it fails.
fn run(rng: &mut ChaCha20Rng, server: &'static str, hostname: &str, auth: &str, keys: &Keys, mut svc: Option<&mut svc::Service>) -> (&'static str, Error) {
    let Ok(net) = Net::connect() else { return ("net", Error::Io) };
    let net = Rc::new(RefCell::new(net));
    let ip = match net.borrow_mut().resolve(server, RESOLVE_MS) {
        Ok(ip) => ip,
        Err(_) => return ("resolve", Error::Io),
    };
    let mut ctl_bufs = Bufs::new();
    let srv = {
        let (mut t, _) = match tls_open(&net, &mut ctl_bufs, ip, 443, server, rng) {
            Ok(t) => t,
            Err(e) => return e,
        };
        match control::server_key(&mut t, server) {
            Ok(k) => k,
            Err(e) => return ("server key", e),
        }
    };
    event!(Info, "ts.server_key", ip = ip);
    let (t, ctl_s) = match tls_open(&net, &mut ctl_bufs, ip, 443, server, rng) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let mut eph = [0u8; 32];
    rng.fill_bytes(&mut eph);
    let (mut h, early) = match control::connect(t, server, keys, &srv, eph) {
        Ok(c) => c,
        Err(e) => return ("noise", e),
    };
    event!(Info, "ts.noise", early = early.map_or(0, |e| e.len()));
    loop {
        let r = match control::register(&mut h, keys, hostname, auth) {
            Ok(r) => r,
            Err(e) => return ("register", e),
        };
        event!(Info, "ts.registered", authorized = r.authorized, login = !r.auth_url.is_empty(), error = r.error.as_str());
        if r.authorized {
            break;
        }
        if !r.auth_url.is_empty() {
            event!(Warn, "ts.login", url = r.auth_url.as_str());
        }
        sleep_ms(LOGIN_POLL_MS);
    }

    // The first map response: the DERP map to pick home from.
    let mut node = Node::new(keys.node.clone(), keys.disco.clone(), rng, now_ms());
    if let Some(svc) = svc.as_deref_mut() {
        svc.reset();
    }
    if TRACE_TCP {
        node.tap = Some(trace);
    }
    let mut m = match control::map(&mut h, keys, hostname, 0, &[]) {
        Ok(m) => m,
        Err(e) => return ("map", e),
    };
    match m.next(&mut h) {
        Ok(json) => {
            let bytes = json.len();
            if let Err(e) = node.apply_map(json, now_ms()) {
                return ("map json", e);
            }
            log_map(&node, bytes);
        }
        Err(e) => return ("map stream", e),
    }
    // STUN, then disco and WireGuard straight from peers (doc/71 §14).
    let udp = match net.borrow().udp_bind(UDP_PORT) {
        Ok(s) => Some(s),
        Err(_) => {
            event!(Warn, "ts.failed", step = "udp bind", error = "io");
            None
        }
    };
    let nc = netcheck(&net, udp, &node.netmap.derp, rng);
    let latency: Vec<(i32, u32)> = nc.latencies().collect();
    let best = nc.best();
    let ext = nc.endpoint.map_or(0, |(ip, _)| u32::from_be_bytes(ip));
    event!(Info, "ts.netcheck", regions = nc.len(), answered = latency.len(), best = best.map_or(0, |b| b.0 as u32), ms = best.map_or(0, |b| b.1), ext_ip = ext);
    let home = best.and_then(|(r, _)| node.netmap.derp_region(r)).or_else(|| node.netmap.home_derp());
    let Some((region, host, port)) = home.map(|(r, n)| (r.region_id, n.host_name.clone(), if n.derp_port == 0 { 443 } else { n.derp_port })) else {
        return ("derp map", Error::Proto("no derp"));
    };
    let derp_ip = match net.borrow_mut().resolve(&host, RESOLVE_MS) {
        Ok(ip) => ip,
        Err(_) => return ("derp resolve", Error::Io),
    };
    let mut derp_bufs = Bufs::new();
    let (t, derp_s) = match tls_open(&net, &mut derp_bufs, derp_ip, port, &host, rng) {
        Ok(t) => t,
        Err((step, e)) => return (if step == "tls" { "derp tls" } else { "derp tcp" }, e),
    };
    let mut nonce = [0u8; 24];
    rng.fill_bytes(&mut nonce);
    let mut derp = match Derp::connect(t, &host, &keys.node, nonce) {
        Ok(d) => d,
        Err(e) => return ("derp", e),
    };
    event!(Info, "ts.derp", region = region as u32, host = host.as_str(), ip = derp_ip);
    // Peers send to us through this region from now on.
    node.home_region = region;
    let eps = if udp.is_some() { endpoints(&net, UDP_PORT, nc.endpoint) } else { Vec::new() };
    event!(Info, "ts.endpoints", count = eps.len(), first = eps.first().map_or("", |e| e.as_str()));
    if let Err(e) = control::update(&mut h, keys, hostname, region, &latency, &eps) {
        return ("update", e);
    }

    let mut shown = node.stats;
    let mut idle_ms = 1;
    let mut web = Web::new(&mut node);
    let mut udp_buf = vec![0u8; 1500];
    let mut last_map = now_ms();
    let mut direct = 0;
    let mut shown_web = 0;
    let started = now_ms();
    loop {
        let mut busy = false;
        if m.ready() || h.buffered() || waiting(&net, ctl_s) {
            busy = true;
            match m.poll(&mut h) {
                Ok(true) => {
                    last_map = now_ms();
                    let json = m.take();
                    let bytes = json.len();
                    match node.apply_map(json, now_ms()) {
                        Ok(r) if !r.keep_alive || r.node.is_some() => log_map(&node, bytes),
                        Ok(_) => {}
                        Err(e) => return ("map json", e),
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    // Why the control connection went (doc/71 §14).
                    let (state, queued) = net.borrow().tcp_state(ctl_s).unwrap_or((99, 0));
                    event!(Warn, "ts.ctl_lost", state = state, queued = queued, since_map_ms = (now_ms() - last_map) as u32, up_s = ((now_ms() - started) / 1000) as u32);
                    return ("map stream", e);
                }
            }
        }
        if derp.buffered() || waiting(&net, derp_s) {
            busy = true;
            let packet = match derp.read() {
                Ok(Frame::Packet { src, data }) => Some((src, data.to_vec())),
                Ok(_) => None,
                Err(e) => return ("derp read", e),
            };
            if let Some((src, data)) = packet {
                node.from_derp(&src, &data, rng, now_ms());
            }
        }
        web.step(&mut node, |n| status(n, hostname, region, &host, started));
        if let Some(svc) = svc.as_deref_mut() {
            busy |= svc.serve(&mut node);
        }
        if let Some(s) = udp {
            let r = net.borrow_mut().udp_recv(s, &mut udp_buf, 0);
            if let Ok((n, ip, port)) = r {
                busy = true;
                node.from_udp(ip.to_be_bytes(), port, &udp_buf[..n], rng, now_ms());
            }
        }
        for (dest, m) in node.poll(rng, now_ms()) {
            busy = true;
            match dest {
                Dest::Derp(peer) => {
                    if let Err(e) = derp.send(&peer, &m) {
                        return ("derp send", e);
                    }
                }
                Dest::Udp(ip, port) => {
                    if let Some(s) = udp {
                        let _ = net.borrow_mut().udp_send(s, u32::from_be_bytes(ip), port, &m);
                    }
                }
            }
        }
        let s = node.stats;
        let d = node.direct_peers(now_ms());
        if d != direct {
            event!(Info, "ts.direct", peers = d, udp_in = s.udp_in, udp_out = s.udp_out, pongs = s.pongs);
            direct = d;
        }
        if (s.handshakes, s.initiations, s.echo_replies, s.dropped, web.served) != (shown.handshakes, shown.initiations, shown.echo_replies, shown.dropped, shown_web) {
            if node.last_drop == "unknown peer" {
                let k = tsclient::keys::to_hex("", &node.wg.unknown);
                event!(Warn, "ts.unknown_peer", key = &k[..16], wg_peers = node.wg.peers.len(), map_peers = node.netmap.peers.len());
            }
            event!(
                Info,
                "ts.stats",
                handshakes = s.handshakes,
                initiations = s.initiations,
                packets = s.packets_in,
                echo_replies = s.echo_replies,
                packets_out = s.packets_out,
                web = web.served,
                disco = s.disco,
                pongs = s.pongs,
                udp_in = s.udp_in,
                dropped = s.dropped,
                last_drop = node.last_drop
            );
            shown = s;
            shown_web = web.served;
        }
        if busy {
            idle_ms = 1;
        } else {
            // A client's request ends the nap.
            match svc.as_deref_mut() {
                Some(svc) => {
                    let ev = wait(&svc.handles(), idle_ms * 1_000_000);
                    if let Ok(ev) = ev
                        && ev.kind == t5_rt::chan::PEER_CLOSED
                    {
                        svc.closed(ev.index, &mut node);
                    }
                }
                None => sleep_ms(idle_ms),
            }
            idle_ms = (idle_ms * 2).min(IDLE_MAX_MS);
        }
    }
}

/// STUN to every DERP region from the UDP socket `s`: `NETCHECK_ROUNDS`
/// rounds of `NETCHECK_ROUND_MS` each, the fastest answer per region.
fn netcheck(net: &Rc<RefCell<Net>>, s: Option<u16>, map: &tsclient::netmap::DerpMap, rng: &mut ChaCha20Rng) -> Netcheck {
    let mut nc = Netcheck::new(map);
    let Some(s) = s else { return nc };
    let mut buf = [0u8; 576];
    for _ in 0..NETCHECK_ROUNDS {
        for (ip, port, q) in nc.round(rng, now_ms()) {
            let _ = net.borrow_mut().udp_send(s, u32::from_be_bytes(ip), port, &q);
        }
        let end = now_ms() + NETCHECK_ROUND_MS;
        while now_ms() < end {
            match net.borrow_mut().udp_recv(s, &mut buf, 0) {
                Ok((n, ..)) => nc.answer(&buf[..n], now_ms()),
                Err(_) => sleep_ms(1),
            }
        }
    }
    nc
}

/// This node's UDP endpoints for peers to try: each interface's IPv4
/// address but loopback, and the address STUN saw, with their ports.
fn endpoints(net: &Rc<RefCell<Net>>, port: u16, seen: Option<([u8; 4], u16)>) -> Vec<String> {
    let mut e = Vec::new();
    let n = net.borrow();
    for k in 0..8 {
        match n.if_info(k) {
            Ok(Some(len)) if len >= 20 && n.slice[14] > 0 => {
                let ip = u32::from_le_bytes(n.slice[16..20].try_into().unwrap()).to_be_bytes();
                if ip[0] != 127 {
                    e.push(alloc::format!("{}.{}.{}.{}:{port}", ip[0], ip[1], ip[2], ip[3]));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    if let Some((ip, p)) = seen {
        let s = alloc::format!("{}.{}.{}.{}:{p}", ip[0], ip[1], ip[2], ip[3]);
        if !e.contains(&s) {
            e.push(s);
        }
    }
    e
}

/// The status page on the tailnet: `http://<tailnet address>/`, one
/// connection at a time (doc/71 §12).
struct Web {
    socket: SocketHandle,
    request: Vec<u8>,
    served: u32,
}

const WEB_PORT: u16 = 80;
/// More of a request than this is not read.
const REQUEST_MAX: usize = 2048;

impl Web {
    fn new(node: &mut Node) -> Web {
        let s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 1024]), tcp::SocketBuffer::new(vec![0; 4096]));
        let socket = node.tun.sockets.add(s);
        Web { socket, request: Vec::new(), served: 0 }
    }

    /// Listens again once the last connection is gone; answers a whole
    /// request head with `body` and closes.
    fn step(&mut self, node: &mut Node, body: impl FnOnce(&Node) -> String) {
        let s = node.tun.sockets.get_mut::<tcp::Socket>(self.socket);
        if !s.is_open() {
            self.request.clear();
            let _ = s.listen(WEB_PORT);
            return;
        }
        if s.can_recv() {
            let room = REQUEST_MAX - self.request.len();
            let _ = s.recv(|b| {
                let n = b.len().min(room);
                self.request.extend_from_slice(&b[..n]);
                (b.len(), ())
            });
        }
        let whole = self.request.windows(4).any(|w| w == b"\r\n\r\n") || self.request.len() == REQUEST_MAX;
        if !whole || !s.can_send() {
            return;
        }
        let body = body(node);
        let s = node.tun.sockets.get_mut::<tcp::Socket>(self.socket);
        let head = alloc::format!("HTTP/1.0 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = s.send_slice(head.as_bytes());
        let _ = s.send_slice(body.as_bytes());
        s.close();
        self.request.clear();
        self.served += 1;
    }
}

/// What the status page says.
fn status(node: &Node, hostname: &str, region: i32, derp: &str, started: u64) -> String {
    let ip = node.netmap.self_ipv4().unwrap_or_default();
    let s = node.stats;
    let mut t = alloc::format!(
        "tab5-os ts\nhostname {hostname}\naddress {}.{}.{}.{}\nderp {region} {derp}\nup {} s\npeers {}\nhandshakes {}\npackets in {} out {}\necho replies {}\ndropped {} ({})\n\n",
        ip[0], ip[1], ip[2], ip[3],
        (now_ms() - started) / 1000,
        node.netmap.peers.len(),
        s.handshakes,
        s.packets_in,
        s.packets_out,
        s.echo_replies,
        s.dropped,
        node.last_drop
    );
    for p in &node.netmap.peers {
        let name = p.name.split('.').next().unwrap_or("");
        let online = match p.online {
            Some(true) => "online",
            Some(false) => "offline",
            None => "",
        };
        t += &alloc::format!("{name} derp {} {online}\n", p.home_region());
    }
    t
}

/// Logs every TCP segment's header (`ts.tcp`): for debugging.
const TRACE_TCP: bool = false;

/// TCP segments' headers (`TRACE_TCP`).
fn trace(out: bool, p: &[u8]) {
    let ihl = (p[0] & 0x0F) as usize * 4;
    if p.len() < ihl + 20 || p[9] != 6 {
        return;
    }
    let t = &p[ihl..];
    let doff = (t[12] >> 4) as usize * 4;
    event!(
        Debug,
        "ts.tcp",
        out = out,
        sport = u16::from_be_bytes([t[0], t[1]]) as u32,
        dport = u16::from_be_bytes([t[2], t[3]]) as u32,
        tcp_seq = u32::from_be_bytes([t[4], t[5], t[6], t[7]]),
        ack = u32::from_be_bytes([t[8], t[9], t[10], t[11]]),
        flags = t[13] as u32,
        win = u16::from_be_bytes([t[14], t[15]]) as u32,
        len = (p.len() - ihl - doff) as u32
    );
}

fn log_map(node: &Node, bytes: usize) {
    // As `svc:net` writes addresses (the first octet in the high byte).
    let ip = node.netmap.self_ipv4().map_or(0, u32::from_be_bytes);
    let (region, derp) = node.netmap.home_derp().map_or((0, ""), |(r, n)| (r.region_id, n.host_name.as_str()));
    event!(Info, "ts.map", bytes = bytes, ip = ip, peers = node.netmap.peers.len(), regions = node.netmap.derp.regions.len(), home = region as u32, derp = derp, clock = node.clock.is_some());
}
