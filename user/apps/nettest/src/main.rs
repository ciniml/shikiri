#![no_std]
#![no_main]

//! N1 test client (doc/research/net-stack.md §4): over 127.0.0.1 through
//! the `net` service, 100 UDP round trips between two sockets, then 1 MiB
//! over one TCP connection (this app drives both ends), compared by a
//! 64-bit FNV-1a hash of what was sent and what arrived.
//!
//! With `mode` 1 it is instead a TCP echo server on port 7777 (for a PC on
//! the host link, doc/21), serving one connection after another until
//! killed; with `arg` 1 it only reads (discard) and with 2 only writes
//! until the peer closes (one direction at a time, doc/58 R5). With `mode` 2 it fetches `http://ARG/` (ARG an IPv4 address as
//! a number, e.g. 0x01010101) and logs the status line and the size: a
//! test of the way out over Wi-Fi (doc/22). With `mode` 3 it pings ARG four
//! times; with `mode` 4 it resolves a few names by DNS and fetches the
//! first one's `/` (doc/22 §8).
//!
//! IPv6 (doc/26 §5): `mode` 5 resolves one.one.one.one's AAAA, pings it and
//! fetches its `/` over IPv6; `mode` 6 sends UDP round trips and 64 KiB of
//! TCP over ::1; `mode` 7 pings the test LAN's peer (fd50::2).
//!
//! Built with `--permit svc:net`. Exit 0 if everything held, otherwise the
//! number of the failed check.

use core::net::{IpAddr, Ipv6Addr};

use t5_rt::net::ipv4;
use t5_rt::{Net, event, sleep_ms, time_ns};

t5_rt::entry!(main);

const LOCALHOST: u32 = ipv4(127, 0, 0, 1);
const TCP_BYTES: usize = 1 << 20;

fn main(mode: usize, arg: usize) -> usize {
    let r = match mode {
        1 => echo_server(arg),
        2 => http_get(arg as u32),
        3 => ping(arg as u32),
        4 => resolve(),
        5 => web6(),
        6 => loop6(),
        7 => ping6(IpAddr::V6(Ipv6Addr::new(0xfd50, 0, 0, 0, 0, 0, 0, 2))),
        _ => run(),
    };
    match r {
        Ok(()) => 0,
        Err(check) => {
            event!(Error, "nettest.failed", check = check);
            check as usize
        }
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn byte(i: usize) -> u8 {
    (i ^ (i >> 8) ^ (i >> 16)) as u8
}

/// Connects to the service, which may still be starting (up to 3 s).
fn connect() -> Result<Net, u32> {
    let mut tries = 0;
    loop {
        match Net::connect() {
            Ok(n) => return Ok(n),
            Err(_) if tries < 30 => {
                tries += 1;
                sleep_ms(100);
            }
            Err(_) => return Err(1),
        }
    }
}

fn ping(ip: u32) -> Result<(), u32> {
    let mut net = connect()?;
    let mut answered = 0u32;
    for seq in 0..4u32 {
        match net.ping(ip, 2000) {
            Ok(us) => {
                answered += 1;
                event!(Info, "nettest.ping", ip = ip, seq_num = seq, rtt_us = us);
            }
            Err(_) => event!(Warn, "nettest.ping_lost", ip = ip, seq_num = seq),
        }
        sleep_ms(200);
    }
    if answered == 0 { Err(50) } else { Ok(()) }
}

/// Names that should resolve on any network with a working resolver.
const NAMES: [&str; 3] = ["example.com", "one.one.one.one", "www.espressif.com"];

fn resolve() -> Result<(), u32> {
    let mut net = connect()?;
    let mut first = 0;
    for name in NAMES {
        let t = time_ns();
        match net.resolve(name, 5000) {
            Ok(ip) => {
                event!(Info, "nettest.resolved", name = name, ip = ip, ms = ((time_ns() - t) / 1_000_000) as u32);
                if first == 0 {
                    first = ip;
                }
            }
            Err(_) => event!(Warn, "nettest.resolve_failed", name = name),
        }
    }
    if first == 0 {
        return Err(60);
    }
    drop(net);
    http_get(first)
}

fn echo_server(how: usize) -> Result<(), u32> {
    let mut net = connect()?;
    let mut buf = [0u8; 4096];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = i as u8;
    }
    let mut served = 0u32;
    loop {
        let s = net.tcp_listen(7777).map_err(|_| 30u32)?;
        event!(Info, "nettest.listening", port = 7777u32, served = served);
        // A listening socket becomes the connection (smoltcp).
        loop {
            match net.tcp_state(s).map_err(|_| 31u32)?.0 {
                t5_rt::net::state::ESTABLISHED => break,
                _ => sleep_ms(5),
            }
        }
        let mut bytes = 0u32;
        // Source: until the peer goes away.
        while how == 2 && net.send_all(s, &buf).is_ok() {
            bytes += buf.len() as u32;
        }
        while how != 2 {
            let n = net.recv(s, &mut buf).map_err(|_| 32u32)?;
            if n == 0 {
                break;
            }
            if how == 0 {
                net.send_all(s, &buf[..n]).map_err(|_| 33u32)?;
            }
            bytes += n as u32;
        }
        // The source's peer may have closed already.
        if net.close(s).is_err() && how != 2 {
            return Err(34);
        }
        served += 1;
        event!(Info, "nettest.echoed", bytes = bytes);
    }
}

/// Pings `ip` (either family) four times.
fn ping6(ip: IpAddr) -> Result<(), u32> {
    let mut net = connect()?;
    let mut answered = 0u32;
    let bytes = match ip {
        IpAddr::V6(a) => a.octets(),
        IpAddr::V4(a) => a.to_ipv6_mapped().octets(),
    };
    for seq in 0..4u32 {
        match net.ping_ip(ip, 2000) {
            Ok(us) => {
                answered += 1;
                event!(Info, "nettest.ping6", addr = &bytes[..], seq_num = seq, rtt_us = us);
            }
            Err(_) => event!(Warn, "nettest.ping6_lost", addr = &bytes[..], seq_num = seq),
        }
        sleep_ms(200);
    }
    if answered == 0 { Err(70) } else { Ok(()) }
}

fn web6() -> Result<(), u32> {
    let mut net = connect()?;
    let t = time_ns();
    let ip = net.resolve6("one.one.one.one", 5000).map_err(|_| 71u32)?;
    event!(Info, "nettest.resolved6", name = "one.one.one.one", addr = &ip.octets()[..], ms = ((time_ns() - t) / 1_000_000) as u32);
    drop(net);
    ping6(IpAddr::V6(ip))?;
    http_get_ip(IpAddr::V6(ip))
}

/// UDP round trips and TCP over ::1.
fn loop6() -> Result<(), u32> {
    let mut net = connect()?;
    let lo = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let a = net.udp_bind(5100).map_err(|_| 80u32)?;
    let b = net.udp_bind(5101).map_err(|_| 81u32)?;
    let mut buf = [0u8; 512];
    let t = time_ns();
    for i in 0..20u32 {
        let msg = i.to_le_bytes();
        net.udp_send_ip(a, lo, 5101, &msg).map_err(|_| 82u32)?;
        let (n, from, port) = net.udp_recv_ip(b, &mut buf, 1000).map_err(|_| 83u32)?;
        if n != 4 || buf[..4] != msg || from != lo || port != 5100 {
            return Err(84);
        }
        net.udp_send_ip(b, from, port, &buf[..n]).map_err(|_| 85u32)?;
        net.udp_recv_ip(a, &mut buf, 1000).map_err(|_| 86u32)?;
    }
    let udp_ms = ((time_ns() - t) / 1_000_000) as u32;
    let l = net.tcp_listen(7778).map_err(|_| 87u32)?;
    let c = net.tcp_connect_ip(lo, 7778).map_err(|_| 88u32)?;
    net.wait_established(c, 5000).map_err(|_| 89u32)?;
    let t = time_ns();
    let (mut sent, mut got) = (0usize, 0usize);
    const N: usize = 64 << 10;
    let mut sum = (Fnv::new(), Fnv::new());
    let mut chunk = [0u8; 2048];
    while got < N {
        if sent < N {
            for (j, c) in chunk.iter_mut().enumerate() {
                *c = byte(sent + j);
            }
            let k = net.send(c, &chunk[..(N - sent).min(chunk.len())]).map_err(|_| 90u32)?;
            sum.0.update(&chunk[..k]);
            sent += k;
        }
        let r = net.recv_now(l, &mut buf).map_err(|_| 91u32)?;
        sum.1.update(&buf[..r.len]);
        got += r.len;
        if r.len == 0 {
            sleep_ms(1);
        }
    }
    let _ = net.close(c);
    let _ = net.close(l);
    event!(Info, "nettest.loop6", udp_round_trips = 20u32, udp_ms = udp_ms, tcp_bytes = N as u32, tcp_ms = ((time_ns() - t) / 1_000_000) as u32, ok = sum.0.0 == sum.1.0);
    if sum.0.0 == sum.1.0 { Ok(()) } else { Err(92) }
}

fn http_get(ip: u32) -> Result<(), u32> {
    http_get_ip(IpAddr::V4(core::net::Ipv4Addr::from_bits(ip)))
}

fn http_get_ip(ip: IpAddr) -> Result<(), u32> {
    let mut net = connect()?;
    let t = time_ns();
    let s = net.tcp_connect_ip(ip, 80).map_err(|_| 40u32)?;
    net.wait_established(s, 10_000).map_err(|_| 41u32)?;
    let connect_ms = ((time_ns() - t) / 1_000_000) as u32;
    net.send_all(s, b"GET / HTTP/1.0\r\nConnection: close\r\n\r\n").map_err(|_| 42u32)?;
    let mut buf = [0u8; 2048];
    let mut first = [0u8; 64];
    let (mut total, mut first_len) = (0usize, 0usize);
    loop {
        let n = net.recv(s, &mut buf).map_err(|_| 43u32)?;
        if n == 0 {
            break;
        }
        if total == 0 {
            let line = buf[..n].iter().position(|&b| b == b'\r').unwrap_or(n).min(first.len());
            first[..line].copy_from_slice(&buf[..line]);
            first_len = line;
        }
        total += n;
    }
    let _ = net.close(s);
    event!(
        Info,
        "nettest.http",
        v6 = ip.is_ipv6(),
        connect_ms = connect_ms,
        bytes = total as u32,
        total_ms = ((time_ns() - t) / 1_000_000) as u32,
        status = core::str::from_utf8(&first[..first_len]).unwrap_or("?")
    );
    if total == 0 { Err(44) } else { Ok(()) }
}

fn run() -> Result<(), u32> {
    let mut net = connect()?;

    // UDP: a -> b, b echoes back to a.
    let a = net.udp_bind(5000).map_err(|_| 2u32)?;
    let b = net.udp_bind(5001).map_err(|_| 3u32)?;
    let mut msg = [0u8; 512];
    let mut got = [0u8; 512];
    let t = time_ns();
    for k in 0..100usize {
        let len = 1 + (k * 37) % 500;
        for (i, x) in msg[..len].iter_mut().enumerate() {
            *x = (k + i) as u8;
        }
        net.udp_send(a, LOCALHOST, 5001, &msg[..len]).map_err(|_| 4u32)?;
        let (n, ip, port) = net.udp_recv(b, &mut got, 1000).map_err(|_| 5u32)?;
        if n != len || ip != LOCALHOST || port != 5000 || got[..n] != msg[..len] {
            return Err(6);
        }
        net.udp_send(b, LOCALHOST, 5000, &got[..n]).map_err(|_| 7u32)?;
        let (n2, _, port2) = net.udp_recv(a, &mut got, 1000).map_err(|_| 8u32)?;
        if n2 != len || port2 != 5001 || got[..n2] != msg[..len] {
            return Err(9);
        }
    }
    let udp_us = (time_ns() - t) / 1000;
    net.close(a).map_err(|_| 10u32)?;
    net.close(b).map_err(|_| 11u32)?;

    // TCP: connect to our own listener and push 1 MiB through.
    let server = net.tcp_listen(7000).map_err(|_| 12u32)?;
    // A second listener on the port is refused.
    if !matches!(net.tcp_listen(7000), Err(t5_rt::NetError::Status(t5_rt::net::status::ADDR_IN_USE))) {
        return Err(28);
    }
    let client = net.tcp_connect(LOCALHOST, 7000).map_err(|_| 13u32)?;
    net.wait_established(client, 2000).map_err(|_| 14u32)?;
    net.wait_established(server, 2000).map_err(|_| 15u32)?;
    let (mut sent_hash, mut recv_hash) = (Fnv::new(), Fnv::new());
    let (mut sent, mut received) = (0usize, 0usize);
    let mut chunk = [0u8; 8192];
    let mut inbox = [0u8; 8192];
    let t = time_ns();
    while received < TCP_BYTES {
        if sent < TCP_BYTES {
            let k = (TCP_BYTES - sent).min(chunk.len());
            for (i, x) in chunk[..k].iter_mut().enumerate() {
                *x = byte(sent + i);
            }
            let n = net.send(client, &chunk[..k]).map_err(|_| 16u32)?;
            sent_hash.update(&chunk[..n]);
            sent += n;
        }
        let r = net.recv_now(server, &mut inbox).map_err(|_| 17u32)?;
        recv_hash.update(&inbox[..r.len]);
        received += r.len;
        if r.ended {
            return Err(18);
        }
        if time_ns() - t > 20_000_000_000 {
            event!(Error, "nettest.stuck", sent = sent as u32, received = received as u32);
            return Err(19);
        }
    }
    let tcp_us = (time_ns() - t) / 1000;
    net.close(client).map_err(|_| 20u32)?;
    // The server side sees the end.
    let mut tail = [0u8; 64];
    if net.recv(server, &mut tail).map_err(|_| 21u32)? != 0 {
        return Err(22);
    }
    net.close(server).map_err(|_| 23u32)?;
    if sent_hash.0 != recv_hash.0 {
        return Err(24);
    }
    event!(
        Info,
        "nettest.done",
        udp_round_trips = 100u32,
        udp_avg_us = (udp_us / 100) as u32,
        tcp_bytes = TCP_BYTES as u32,
        tcp_ms = (tcp_us / 1000) as u32,
        tcp_kib_s = (TCP_BYTES as u64 * 1_000_000 / 1024 / tcp_us.max(1)) as u32,
        hash = sent_hash.0
    );
    Ok(())
}
