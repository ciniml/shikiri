//! tsclient against a control server from the host (doc/71): the server
//! key, the Noise channel, register, the first map response.
//!
//! tsprobe KEYFILE [HOSTNAME [SERVER]]: KEYFILE keeps the machine, node and
//! disco keys (made if missing). The auth key, if any, from TS_AUTHKEY.
//! Without one, the login URL goes to KEYFILE.url (not printed) and the
//! probe registers again every 10 s until a person approves (30 minutes).

use std::io::Write as _;
use std::net::TcpStream;

use embedded_io_adapters::std::FromStd;
use embedded_tls::blocking::TlsConnection;
use embedded_tls::{TlsConfig, TlsContext};
use rand_core::{OsRng, RngCore};
use tsclient::{control, keys::Keys, netmap::NetMap, tls};

fn tls_open(host: &str) -> TlsConnection<'static, FromStd<TcpStream>, tls::Suite> {
    let tcp = TcpStream::connect((host, 443)).expect("tcp connect");
    let rbuf = Box::leak(vec![0u8; tls::READ_BUF].into_boxed_slice());
    let wbuf = Box::leak(vec![0u8; tls::WRITE_BUF].into_boxed_slice());
    let mut t = TlsConnection::new(FromStd::new(tcp), rbuf, wbuf);
    let config = TlsConfig::new().with_server_name(host);
    t.open(TlsContext::new(&config, tls::Provider::new(OsRng))).expect("tls handshake");
    t
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let keyfile = args.get(1).expect("usage: tsprobe KEYFILE [HOSTNAME [SERVER]]");
    let hostname = args.get(2).map_or("tab5-os-probe", |s| s.as_str());
    let server: &'static str = Box::leak(args.get(3).map_or("controlplane.tailscale.com", |s| s.as_str()).to_string().into_boxed_str());
    let auth = std::env::var("TS_AUTHKEY").unwrap_or_default();

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

    let mut t = tls_open(server);
    let srv = control::server_key(&mut t, server).expect("server key");
    drop(t);
    println!("server key {}", tsclient::keys::to_hex("mkey:", &srv));

    let mut eph = [0u8; 32];
    OsRng.fill_bytes(&mut eph);
    let (mut h, early) = control::connect(tls_open(server), server, &keys, &srv, eph).expect("noise");
    println!("noise and h2 up; early payload {} bytes", early.as_ref().map_or(0, |e| e.len()));

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
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
    let mut m = control::map(&mut h, &keys, hostname, 0, &[]).expect("map");
    let mut nm = NetMap::default();
    for i in 0..3 {
        let json = m.next(&mut h).expect("map response");
        let len = json.len();
        let r = nm.apply_json(json).expect("map json");
        println!(
            "map {i}: {len} bytes, keepalive={}, self={:?}, peers={}, derp regions={}, home={:?}",
            r.keep_alive,
            nm.self_ipv4(),
            nm.peers.len(),
            nm.derp.regions.len(),
            nm.home_derp().map(|(r, n)| (r.region_id, n.host_name.clone()))
        );
        if i == 0 {
            for p in &nm.peers {
                println!("  peer {:?} region {} endpoints {}", p.ipv4(), p.home_region(), p.endpoints.len());
            }
        }
    }
}
