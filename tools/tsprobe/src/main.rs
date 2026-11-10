//! tsclient against a control server from the host (doc/71): the server
//! key, the Noise channel, register, the map stream, DERP and WireGuard:
//! answers pings to its tailnet address for TS_SECONDS (default 120).
//!
//! tsprobe KEYFILE [HOSTNAME [SERVER]]: KEYFILE keeps the machine, node and
//! disco keys (made if missing). The auth key, if any, from TS_AUTHKEY.
//! Without one, the login URL goes to KEYFILE.url (not printed) and the
//! probe registers again every 10 s until a person approves (30 minutes).

use std::io::Write as _;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use embedded_io_adapters::std::FromStd;
use embedded_tls::blocking::TlsConnection;
use embedded_tls::{TlsConfig, TlsContext};
use rand_core::{OsRng, RngCore};
use tsclient::derp::{Derp, Frame};
use tsclient::node::Node;
use tsclient::stream::Buffered;
use tsclient::{control, keys::Keys, tls};

type Tls = Buffered<TlsConnection<'static, FromStd<TcpStream>, tls::Suite>>;

/// TLS to `host:port`, and a second handle on the TCP socket to see
/// whether bytes wait.
fn tls_open(host: &str, port: u16) -> (Tls, TcpStream) {
    let tcp = TcpStream::connect((host, port)).expect("tcp connect");
    let peek = tcp.try_clone().unwrap();
    let rbuf = Box::leak(vec![0u8; tls::READ_BUF].into_boxed_slice());
    let wbuf = Box::leak(vec![0u8; tls::WRITE_BUF].into_boxed_slice());
    let mut t = TlsConnection::new(FromStd::new(tcp), rbuf, wbuf);
    let config = TlsConfig::new().with_server_name(host);
    t.open(TlsContext::new(&config, tls::Provider::new(OsRng))).expect("tls handshake");
    (Buffered::new(t), peek)
}

/// Bytes (or the end) wait on the socket.
fn waiting(t: &TcpStream) -> bool {
    t.set_nonblocking(true).unwrap();
    let r = t.peek(&mut [0u8]);
    t.set_nonblocking(false).unwrap();
    !matches!(r, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

/// Three rounds of STUN to every region, 700 ms each.
fn netcheck(map: &tsclient::netmap::DerpMap) -> tsclient::netcheck::Netcheck {
    let mut nc = tsclient::netcheck::Netcheck::new(map);
    let u = std::net::UdpSocket::bind("0.0.0.0:0").expect("udp");
    u.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let mut buf = [0u8; 1500];
    for _ in 0..3 {
        for (ip, port, q) in nc.round(&mut OsRng, now()) {
            let _ = u.send_to(&q, (std::net::Ipv4Addr::from(ip), port));
        }
        let end = now() + 700;
        while now() < end {
            if let Ok((n, _)) = u.recv_from(&mut buf) {
                nc.answer(&buf[..n], now());
            }
        }
    }
    nc
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // `tsprobe --tls HOST`: only the TLS handshake, its error shown.
    if args.get(1).map(|s| s.as_str()) == Some("--tls") {
        let host = &args[2];
        let tcp = TcpStream::connect((host.as_str(), 443)).expect("tcp connect");
        let mut rbuf = vec![0u8; tls::READ_BUF];
        let mut wbuf = vec![0u8; tls::WRITE_BUF];
        let mut t = TlsConnection::new(FromStd::new(tcp), &mut rbuf, &mut wbuf);
        let config = TlsConfig::new().with_server_name(host);
        println!("{host}: {:?}", t.open(TlsContext::new(&config, tls::Provider::new(OsRng))));
        return;
    }
    // `tsprobe --netcheck DERPMAP.json`: the regions' STUN round trips (a
    // DERP map as https://login.tailscale.com/derpmap/default gives it).
    if args.get(1).map(|s| s.as_str()) == Some("--netcheck") {
        let mut nm = tsclient::netmap::NetMap::default();
        let json = std::fs::read(&args[2]).expect("derp map");
        let wrapped = [&b"{\"DERPMap\":"[..], &json, b"}"].concat();
        nm.apply_json(&wrapped).expect("derp map json");
        let nc = netcheck(&nm.derp);
        let mut l: Vec<_> = nc.latencies().collect();
        l.sort_by_key(|&(_, ms)| ms);
        for (r, ms) in l {
            let code = nm.derp_region(r).map_or("", |(r, _)| r.region_code.as_str());
            println!("{r:3} {code:6} {ms} ms");
        }
        println!("best {:?}, endpoint {}", nc.best(), nc.endpoint.is_some());
        return;
    }
    let keyfile = args.get(1).expect("usage: tsprobe KEYFILE [HOSTNAME [SERVER]]");
    let hostname = args.get(2).map_or("tab5-os-probe", |s| s.as_str());
    let server: &'static str = Box::leak(args.get(3).map_or("controlplane.tailscale.com", |s| s.as_str()).to_string().into_boxed_str());
    let auth = std::env::var("TS_AUTHKEY").unwrap_or_default();
    let seconds: u64 = std::env::var("TS_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;

    let keys = match std::fs::read(keyfile) {
        Ok(b) if b.len() == 96 => Keys::from_bytes(b[..].try_into().unwrap()),
        _ => {
            let k = Keys::generate(&mut OsRng);
            let mut f = std::fs::File::create(keyfile).expect("keyfile");
            f.write_all(&k.to_bytes()).unwrap();
            println!("new keys in {keyfile}");
            k
        }
    };
    println!("node {}", tsclient::keys::to_hex("nodekey:", &keys.node.public));

    let (mut t, _) = tls_open(server, 443);
    let srv = control::server_key(&mut t, server).expect("server key");
    drop(t);

    let mut eph = [0u8; 32];
    OsRng.fill_bytes(&mut eph);
    let (t, ctl_tcp) = tls_open(server, 443);
    let (mut h, _) = control::connect(t, server, &keys, &srv, eph).expect("noise");

    let mut tries = 0;
    loop {
        let r = control::register(&mut h, &keys, hostname, &auth).expect("register");
        println!("registered: authorized={} login url={} error={:?}", r.authorized, if r.auth_url.is_empty() { "none" } else { "(in .url)" }, r.error);
        if r.authorized {
            break;
        }
        if !r.auth_url.is_empty() {
            std::fs::write(format!("{keyfile}.url"), &r.auth_url).unwrap();
        }
        tries += 1;
        if tries > 180 || std::env::var("TS_NOWAIT").is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_secs(10));
    }

    let mut node = Node::new(keys.node.clone(), keys.disco.clone(), &mut OsRng, now_ms());
    let mut m = control::map(&mut h, &keys, hostname, 0, &[]).expect("map");
    let json = m.next(&mut h).expect("first map response");
    let len = json.len();
    node.apply_map(json, now_ms()).expect("map json");
    let nc = netcheck(&node.netmap.derp);
    let latency: Vec<_> = nc.latencies().collect();
    let (region, host, port) = {
        let (r, n) = nc.best().and_then(|(r, _)| node.netmap.derp_region(r)).or_else(|| node.netmap.home_derp()).expect("no derp");
        (r.region_id, n.host_name.clone(), if n.derp_port == 0 { 443 } else { n.derp_port })
    };
    println!("map: {len} bytes, self {:?}, peers {}, home derp {region} {host}", node.netmap.self_ipv4(), node.netmap.peers.len());

    let (t, derp_tcp) = tls_open(&host, port);
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let mut derp = Derp::connect(t, &host, &keys.node, nonce).expect("derp");
    println!("derp: connected to {host}");
    control::update(&mut h, &keys, hostname, region, &latency, &[]).expect("update");

    let mut shown = node.stats;
    while now_ms() < seconds * 1000 {
        let mut busy = false;
        if m.ready() || h.buffered() || waiting(&ctl_tcp) {
            busy = true;
            if m.poll(&mut h).expect("map stream") {
                let json = m.take();
                let r = node.apply_map(json, now_ms()).expect("map json");
                if !r.keep_alive {
                    println!("map update: peers {}", node.netmap.peers.len());
                }
            }
        }
        if derp.buffered() || waiting(&derp_tcp) {
            busy = true;
            let packet = match derp.read().expect("derp read") {
                Frame::Packet { src, data } => Some((src, data.to_vec())),
                _ => None,
            };
            if let Some((src, data)) = packet {
                node.from_derp(&src, &data, &mut OsRng, now_ms());
            }
        }
        for (dest, m) in node.poll(&mut OsRng, now_ms()) {
            busy = true;
            // Through DERP only here.
            if let tsclient::node::Dest::Derp(peer) = dest {
                derp.send(&peer, &m).expect("derp send");
            }
        }
        let s = node.stats;
        if (s.handshakes, s.echo_replies, s.dropped, s.disco) != (shown.handshakes, shown.echo_replies, shown.dropped, shown.disco) {
            println!("stats {s:?} last drop {:?}", node.last_drop);
            shown = s;
        }
        if !busy {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
