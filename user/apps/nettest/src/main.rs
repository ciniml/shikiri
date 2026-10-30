#![no_std]
#![no_main]

//! N1 test client (doc/research/net-stack.md §4): over 127.0.0.1 through
//! the `net` service, 100 UDP round trips between two sockets, then 1 MiB
//! over one TCP connection (this app drives both ends), compared by a
//! 64-bit FNV-1a hash of what was sent and what arrived.
//!
//! With `mode` 1 it is instead a TCP echo server on port 7777 (for a PC on
//! the host link, doc/21), serving one connection after another until
//! killed.
//!
//! Built with `--permit svc:net`. Exit 0 if everything held, otherwise the
//! number of the failed check.

use t5_rt::net::ipv4;
use t5_rt::{Net, event, sleep_ms, time_ns};

t5_rt::entry!(main);

const LOCALHOST: u32 = ipv4(127, 0, 0, 1);
const TCP_BYTES: usize = 1 << 20;

fn main(mode: usize, _: usize) -> usize {
    let r = if mode == 1 { echo_server() } else { run() };
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

fn echo_server() -> Result<(), u32> {
    let mut net = connect()?;
    let mut buf = [0u8; 4096];
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
        loop {
            let n = net.recv(s, &mut buf).map_err(|_| 32u32)?;
            if n == 0 {
                break;
            }
            net.send_all(s, &buf[..n]).map_err(|_| 33u32)?;
            bytes += n as u32;
        }
        net.close(s).map_err(|_| 34u32)?;
        served += 1;
        event!(Info, "nettest.echoed", bytes = bytes);
    }
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
