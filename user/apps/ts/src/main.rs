#![no_std]
#![no_main]

//! A Tailscale client (doc/71), stage T1: registers this node with the
//! control server and follows its map (this node's tailnet address, the
//! peers, the DERP regions), logging what changes. The protocols are in
//! `tsclient`; here are the TCP (`svc:net`), TLS (embedded-tls), the
//! settings and the keys.
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
use alloc::string::String;
use alloc::vec;
use embedded_tls::blocking::TlsConnection;
use embedded_tls::{TlsConfig, TlsContext};
use linked_list_allocator::LockedHeap;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use t5_rt::{Config, Net, event, random_fill, sleep_ms};
use tsclient::{Error, control, keys::Keys, netmap::NetMap, tls};

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
    loop {
        let e = run(&mut rng, server, &hostname, &auth, &keys);
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

/// A TCP connection through `svc:net`, as embedded-io for embedded-tls.
struct Sock<'a> {
    net: &'a mut Net,
    s: u16,
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

impl embedded_io::ErrorType for Sock<'_> {
    type Error = SockError;
}

impl embedded_io::Read for Sock<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SockError> {
        self.net.recv(self.s, buf).map_err(|_| SockError)
    }
}

impl embedded_io::Write for Sock<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, SockError> {
        self.net.send(self.s, buf).map_err(|_| SockError)
    }

    fn flush(&mut self) -> Result<(), SockError> {
        Ok(())
    }
}

impl Drop for Sock<'_> {
    fn drop(&mut self) {
        let _ = self.net.close(self.s);
    }
}

type Tls<'a> = TlsConnection<'a, Sock<'a>, tls::Suite>;

/// A TLS connection's record buffers (read, write).
struct Bufs(alloc::vec::Vec<u8>, alloc::vec::Vec<u8>);

/// TLS to `host` (at `ip`, port 443), its certificate checked, with the
/// record buffers `bufs`.
fn tls_open<'a>(net: &'a mut Net, bufs: &'a mut Bufs, ip: u32, host: &str, rng: &mut ChaCha20Rng) -> Result<Tls<'a>, (&'static str, Error)> {
    let s = net.tcp_connect(ip, 443).map_err(|_| ("tcp", Error::Io))?;
    net.wait_established(s, CONNECT_MS).map_err(|_| {
        let _ = net.close(s);
        ("tcp", Error::Io)
    })?;
    let mut t = TlsConnection::new(Sock { net, s }, &mut bufs.0, &mut bufs.1);
    let config = TlsConfig::new().with_server_name(host);
    let child = ChaCha20Rng::from_seed({
        let mut s = [0u8; 32];
        rng.fill_bytes(&mut s);
        s
    });
    t.open(TlsContext::new(&config, tls::Provider::new(child))).map_err(|_| ("tls", Error::Io))?;
    Ok(t)
}

/// One session with the control server, until it fails.
fn run(rng: &mut ChaCha20Rng, server: &'static str, hostname: &str, auth: &str, keys: &Keys) -> (&'static str, Error) {
    let Ok(mut net) = Net::connect() else { return ("net", Error::Io) };
    let mut bufs = Bufs(vec![0; tls::READ_BUF], vec![0; tls::WRITE_BUF]);
    let ip = match net.resolve(server, RESOLVE_MS) {
        Ok(ip) => ip,
        Err(_) => return ("resolve", Error::Io),
    };
    let srv = {
        let mut t = match tls_open(&mut net, &mut bufs, ip, server, rng) {
            Ok(t) => t,
            Err(e) => return e,
        };
        match control::server_key(&mut t, server) {
            Ok(k) => k,
            Err(e) => return ("server key", e),
        }
    };
    event!(Info, "ts.server_key", ip = ip);
    let t = match tls_open(&mut net, &mut bufs, ip, server, rng) {
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
    let mut m = match control::map(&mut h, keys, hostname, 0, &[]) {
        Ok(m) => m,
        Err(e) => return ("map", e),
    };
    let mut nm = NetMap::default();
    loop {
        let json = match m.next(&mut h) {
            Ok(j) => j,
            Err(e) => return ("map stream", e),
        };
        let bytes = json.len();
        let r = match nm.apply_json(json) {
            Ok(r) => r,
            Err(e) => return ("map json", e),
        };
        if r.keep_alive && r.node.is_none() {
            continue;
        }
        // As `svc:net` writes addresses (the first octet in the high byte).
        let ip = nm.self_ipv4().map_or(0, u32::from_be_bytes);
        let (region, derp) = nm.home_derp().map_or((0, ""), |(r, n)| (r.region_id, n.host_name.as_str()));
        event!(Info, "ts.map", bytes = bytes, ip = ip, peers = nm.peers.len(), regions = nm.derp.regions.len(), home = region as u32, derp = derp);
    }
}
