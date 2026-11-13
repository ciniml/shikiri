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
//! `run ts [MODE]`: 0 (as in START.CFG) starts only on a node that has
//! its keys already (exit 0 with `ts.off` otherwise), so a device that
//! never joined a tailnet does not ask the control server for one at each
//! boot; 1 starts in any case, making the keys on the first start. Exits
//! only when it cannot start (the step); otherwise retries after failures.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::net::{IpAddr, Ipv6Addr, SocketAddr};
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
use tsclient::disco::{self, Addr};
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
const RETRY_FIRST_MS: u64 = 2000;
const RETRY_MAX_MS: u64 = 30_000;
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
/// Both servers send keep-alives about every minute (DERP's frame, the
/// map's `KeepAlive`): nothing for this long and the connection is taken
/// as dead (its TCP can die without a word: a NAT that forgot it).
const SILENT_MS: u64 = 180_000;
/// A connection to another region's DERP (a peer's home, doc/71 §14.12)
/// closes after this long without a packet sent (Tailscale's
/// derpInactiveCleanupTime).
const RELAY_IDLE_MS: u64 = 60_000;
/// After failing to reach a region's DERP, not tried again for this long
/// (connecting stops the loop meanwhile).
const RELAY_RETRY_MS: u64 = 30_000;

fn main(mode: usize, _: usize) -> usize {
    unsafe { HEAP.lock().init(core::ptr::addr_of_mut!(ARENA) as *mut u8, HEAP_BYTES) };
    // A health test's failure: no keys from a generator we cannot trust.
    let mut raw = [0u8; tsclient::keys::RAW_SEED];
    if random_fill(&mut raw).is_err() {
        event!(Error, "ts.failed", step = "random");
        return 1;
    }
    let mut rng = ChaCha20Rng::from_seed(tsclient::keys::seed(&raw));
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
        _ if mode == 0 && auth.is_empty() => {
            event!(Info, "ts.off");
            return 0;
        }
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
    let mut retry = RETRY_FIRST_MS;
    loop {
        let start = now_ms();
        let e = run(&mut rng, server, &hostname, &auth, &keys, svc.as_mut());
        let (step, error) = e;
        event!(Warn, "ts.failed", step = step, error = describe(error), retry_ms = retry);
        sleep_ms(retry);
        // Short at first (at boot `net` may not have an address yet),
        // longer while failures follow each other.
        retry = if now_ms() - start > RETRY_MAX_MS * 2 { RETRY_FIRST_MS } else { (retry * 2).min(RETRY_MAX_MS) };
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

/// A connection to another region's DERP, for peers whose home it is: we
/// send there; they answer through ours.
struct Relay {
    region: i32,
    derp: Derp<Tls<'static>>,
    s: u16,
    /// When a packet last went through it.
    used: u64,
    sent: u32,
    // After `derp`, so dropped after it: the TLS record buffers it uses.
    _bufs: Box<Bufs>,
}

impl Relay {
    fn open(net: &Rc<RefCell<Net>>, map: &tsclient::netmap::NetMap, region: i32, keys: &Keys, rng: &mut ChaCha20Rng) -> Result<Relay, &'static str> {
        let Some((_, n)) = map.derp_region(region) else { return Err("no region") };
        let (host, port) = (n.host_name.clone(), if n.derp_port == 0 { 443 } else { n.derp_port });
        let ip = net.borrow_mut().resolve(&host, RESOLVE_MS).map_err(|_| "resolve")?;
        let mut bufs = Box::new(Bufs::new());
        // SAFETY: the buffers are on the heap and outlive `derp` (they
        // move with the Box, and the field order drops `derp` first).
        let b: &'static mut Bufs = unsafe { &mut *(&mut *bufs as *mut Bufs) };
        let (t, s) = tls_open(net, b, ip, port, &host, rng).map_err(|(step, _)| step)?;
        let mut nonce = [0u8; 24];
        rng.fill_bytes(&mut nonce);
        let derp = Derp::connect(t, &host, &keys.node, nonce).map_err(|_| "derp")?;
        event!(Info, "ts.relay", region = region as u32, host = host.as_str(), ip = ip);
        Ok(Relay { region, derp, s, used: now_ms(), sent: 0, _bufs: bufs })
    }
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
    (node.endpoints, node.global_v6) = if udp.is_some() { endpoints(&net, UDP_PORT, nc.endpoint) } else { (Vec::new(), false) };
    let eps: Vec<String> = node.endpoints.iter().map(|a| sock_addr(a).to_string()).collect();
    let v6 = node.endpoints.iter().filter(|a| a.0[..12] != disco::v4([0; 4], 0).0[..12]).count();
    event!(Info, "ts.endpoints", count = eps.len(), first = eps.first().map_or("", |e| e.as_str()), v6 = v6, global_v6 = node.global_v6);
    if let Err(e) = control::update(&mut h, keys, hostname, region, &latency, &eps) {
        return ("update", e);
    }

    let mut shown = node.stats;
    let mut idle_ms = 1;
    let mut web = Web::new(&mut node);
    let mut udp_buf = vec![0u8; 1500];
    let mut last_map = now_ms();
    let mut last_derp = now_ms();
    let mut udp_check = true;
    let mut direct = 0;
    // Datagrams `net` would not take (no route: an IPv6 address off our links).
    let mut udp_unsent = 0u32;
    let mut shown_web = 0;
    let started = now_ms();
    let mut relay: Option<Relay> = None;
    // A region whose DERP could not be reached, and when.
    let mut relay_failed: Option<(i32, u64)> = None;
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
            last_derp = now_ms();
            if let Some((src, data)) = packet {
                node.from_derp(&src, &data, rng, now_ms());
            }
        }
        // The other region's DERP: packets for us (peers mostly use ours),
        // keep-alives; closed when idle or broken.
        if let Some(r) = relay.as_mut() {
            let mut gone = None;
            if r.derp.buffered() || waiting(&net, r.s) {
                busy = true;
                match r.derp.read() {
                    Ok(Frame::Packet { src, data }) => {
                        let (src, data) = (src, data.to_vec());
                        node.from_derp(&src, &data, rng, now_ms());
                    }
                    Ok(_) => {}
                    Err(_) => gone = Some("read"),
                }
            }
            if gone.is_none() && now_ms() - r.used > RELAY_IDLE_MS {
                gone = Some("idle");
            }
            if let Some(why) = gone {
                event!(Info, "ts.relay_closed", region = r.region as u32, why = why, sent = r.sent);
                relay = None;
            }
        }
        let now = now_ms();
        if now - last_map > SILENT_MS || now - last_derp > SILENT_MS {
            event!(Warn, "ts.silent", since_map_ms = (now - last_map) as u32, since_derp_ms = (now - last_derp) as u32, up_s = ((now - started) / 1000) as u32);
            return ("silent", Error::Io);
        }
        web.step(&mut node, |n| status(n, hostname, region, &host, started));
        if let Some(svc) = svc.as_deref_mut() {
            busy |= svc.serve(&mut node);
        }
        // Read only after traffic or when `net` woke the nap for a
        // datagram (UDP_WAIT): not one call per round while idle.
        if let Some(s) = udp
            && (udp_check || busy)
        {
            let r = net.borrow_mut().udp_recv_ip(s, &mut udp_buf, 0);
            udp_check = false;
            if let Ok((n, ip, port)) = r {
                busy = true;
                udp_check = true;
                let from = match ip {
                    IpAddr::V4(a) => disco::v4(a.octets(), port),
                    IpAddr::V6(a) => (a.octets(), port),
                };
                node.from_udp(from, &udp_buf[..n], rng, now_ms());
            }
        }
        for (dest, m) in node.poll(rng, now_ms()) {
            busy = true;
            match dest {
                Dest::Derp(peer) => {
                    let r = node.peer_region(&peer);
                    if r != 0 && r != region && relay.as_ref().is_none_or(|x| x.region != r) && relay_failed.is_none_or(|(f, at)| f != r || now_ms() - at > RELAY_RETRY_MS) {
                        if let Some(old) = relay.take() {
                            event!(Info, "ts.relay_closed", region = old.region as u32, why = "other", sent = old.sent);
                        }
                        match Relay::open(&net, &node.netmap, r, keys, rng) {
                            Ok(x) => relay = Some(x),
                            Err(step) => {
                                event!(Warn, "ts.failed", step = "relay", error = step, region = r as u32);
                                relay_failed = Some((r, now_ms()));
                            }
                        }
                    }
                    match relay.as_mut().filter(|x| x.region == r) {
                        Some(x) => {
                            x.used = now_ms();
                            x.sent += 1;
                            if x.derp.send(&peer, &m).is_err() {
                                event!(Info, "ts.relay_closed", region = r as u32, why = "send", sent = x.sent);
                                relay = None;
                            }
                        }
                        // Ours, as before: the peer gets it only if it is on
                        // our region's servers too.
                        None => {
                            if let Err(e) = derp.send(&peer, &m) {
                                return ("derp send", e);
                            }
                        }
                    }
                }
                Dest::Udp(a) => {
                    if let Some(s) = udp {
                        let to = sock_addr(&a);
                        if net.borrow_mut().udp_send_ip(s, to.ip(), to.port(), &m).is_err() {
                            udp_unsent += 1;
                        }
                    }
                }
            }
        }
        let s = node.stats;
        let d = node.direct_peers(now_ms());
        if d != direct {
            event!(Info, "ts.direct", peers = d, udp_in = s.udp_in, udp_out = s.udp_out, udp6_in = s.udp6_in, unsent = udp_unsent, pongs = s.pongs, heartbeats = s.heartbeats);
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
            // A client's request or a datagram ends the nap.
            let armed = udp.is_some() && net.borrow().udp_arm().is_ok();
            let n = net.borrow();
            let mut hs = svc.as_deref().map_or(Vec::new(), |svc| svc.handles());
            let net_at = hs.len();
            if armed {
                hs.push(n.handle());
            }
            let ev = if hs.is_empty() {
                sleep_ms(idle_ms);
                None
            } else {
                wait(&hs, idle_ms * 1_000_000).ok()
            };
            drop(hs);
            drop(n);
            match ev {
                Some(ev) if armed && ev.index == net_at => udp_check = true,
                Some(ev) if ev.kind == t5_rt::chan::PEER_CLOSED => {
                    if let Some(svc) = svc.as_deref_mut() {
                        svc.closed(ev.index, &mut node);
                    }
                }
                _ => {}
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
/// address but loopback, the address STUN saw, and each IPv6 address but
/// loopback and link-local (doc/71 §14.11), with their ports; and whether
/// one of those is global (a router offers a way out).
fn endpoints(net: &Rc<RefCell<Net>>, port: u16, seen: Option<([u8; 4], u16)>) -> (Vec<Addr>, bool) {
    use t5_rt::net::iface;
    let (mut e, mut v6) = (Vec::new(), Vec::new());
    let n = net.borrow();
    for k in 0..8 {
        match n.if_info(k) {
            Ok(Some(len)) if len >= iface::HEADER => {
                let ip = u32::from_le_bytes(n.slice[16..20].try_into().unwrap()).to_be_bytes();
                if n.slice[14] > 0 && ip[0] != 127 {
                    e.push(disco::v4(ip, port));
                }
                for j in 0..(n.slice[15] as usize).min(iface::V6_MAX) {
                    let at = iface::HEADER + j * iface::V6_ENTRY;
                    let Some(a) = n.slice.get(at..at + 16) else { break };
                    let a = Ipv6Addr::from(<[u8; 16]>::try_from(a).unwrap());
                    if !a.is_loopback() && !a.is_unicast_link_local() && !a.is_unspecified() {
                        v6.push((a.octets(), port));
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    if let Some((ip, p)) = seen
        && !e.contains(&disco::v4(ip, p))
    {
        e.push(disco::v4(ip, p));
    }
    let global = v6.iter().any(|a: &Addr| a.0[0] & 0xE0 == 0x20);
    e.extend(v6);
    (e, global)
}

/// A disco address as a socket address (IPv4 for IPv4-mapped ones).
fn sock_addr(a: &Addr) -> SocketAddr {
    let ip = Ipv6Addr::from(a.0);
    match ip.to_ipv4_mapped() {
        Some(v4) => SocketAddr::new(IpAddr::V4(v4), a.1),
        None => SocketAddr::new(IpAddr::V6(ip), a.1),
    }
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
