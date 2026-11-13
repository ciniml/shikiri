#![no_std]
#![no_main]

//! `run tsget IP PORT`: one HTTP/1.0 `GET /` to a tailnet peer through
//! `svc:ts` (doc/71 §13), to try the service: the status line and the
//! bytes, as `tsget.done`. IP as a number (100.64.0.2 is 1681915906);
//! PORT 0 is 80. PORT + 0x10000: UDP instead (doc/71 §14.6), `ROUNDS`
//! datagrams to an echo server there, each waited for with UDP_WAIT, as
//! `tsget.udp` (a reply after its round counts as `late`). `run tsget 0
//! 0`: PEER_INFO of every node as `tsget.peer` (what the shell's `ts`
//! shows, for the log).

use t5_rt::{Net, event, time_ns, wait};

t5_rt::entry!(main);

fn main(ip: usize, port: usize) -> usize {
    if ip == 0 && port == 0 {
        return peers();
    }
    if port > 0xFFFF {
        return match echo(ip as u32, port as u16) {
            Ok(()) => 0,
            Err(step) => {
                event!(Warn, "tsget.failed", ip = ip as u32, port = port as u16 as u32, step = step);
                1
            }
        };
    }
    let (ip, port) = (ip as u32, if port == 0 { 80 } else { port as u16 });
    match get(ip, port) {
        Ok((status, bytes, ms)) => {
            event!(Info, "tsget.done", ip = ip, port = port as u32, status = status, bytes = bytes, ms = ms);
            0
        }
        Err(step) => {
            event!(Warn, "tsget.failed", ip = ip, port = port as u32, step = step);
            1
        }
    }
}

fn peers() -> usize {
    let Ok(ts) = Net::connect_to("ts") else { return 1 };
    for k in 0.. {
        let Ok(r) = ts.peer_info(k) else { break };
        if r.len() < t5_rt::net::peer::RECORD {
            return 2;
        }
        let name = &r[..t5_rt::net::peer::NAME];
        let name = core::str::from_utf8(&name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())]).unwrap_or("?");
        let u = |at: usize| u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
        event!(Info, "tsget.peer", k = k, name = name, flags = r[52] as u32, port = u16::from_le_bytes([r[54], r[55]]) as u32, ip4 = u(56), ip6 = &r[72..88], idle_s = u(68));
    }
    0
}

/// The status code, the bytes received and the time taken (ms).
fn get(ip: u32, port: u16) -> Result<(u32, u32, u32), &'static str> {
    let start = t5_rt::time_ns();
    let mut net = Net::connect_to("ts").map_err(|_| "connect to svc:ts")?;
    let s = net.tcp_connect(ip, port).map_err(|_| "tcp connect")?;
    net.wait_established(s, 15_000).map_err(|_| "establish")?;
    let st0 = net.tcp_state(s).map_or(99, |v| v.0);
    net.send_all(s, b"GET / HTTP/1.0\r\nUser-Agent: tab5-os-tsget\r\n\r\n").map_err(|_| "send")?;
    let st1 = net.tcp_state(s).map_or(99, |v| v.0);
    event!(Info, "tsget.sent", before = st0, after = st1, ms = ((t5_rt::time_ns() - start) / 1_000_000) as u32);
    let mut buf = [0u8; 1024];
    let mut head = [0u8; 16];
    let (mut total, mut kept) = (0usize, 0usize);
    loop {
        let n = net.recv(s, &mut buf).map_err(|_| "recv")?;
        if n == 0 {
            event!(Info, "tsget.ended", state = net.tcp_state(s).map_or(99, |v| v.0), ms = ((t5_rt::time_ns() - start) / 1_000_000) as u32);
            break;
        }
        let k = n.min(head.len() - kept);
        head[kept..kept + k].copy_from_slice(&buf[..k]);
        kept += k;
        total += n;
    }
    let _ = net.close(s);
    // "HTTP/1.x NNN".
    if kept < 12 {
        event!(Warn, "tsget.short", bytes = total as u32);
        return Err("short answer");
    }
    let status = core::str::from_utf8(&head[9..12]).ok().and_then(|t| t.parse().ok()).ok_or("bad status")?;
    Ok((status, total as u32, ((t5_rt::time_ns() - start) / 1_000_000) as u32))
}

const ROUNDS: u32 = 5;
const ECHO_MS: u64 = 2000;

/// Datagrams to an echo server: how many came back, how many of those
/// after their round's `ECHO_MS` (taken in a later round or in a last
/// `ECHO_MS`), and the round trips.
fn echo(ip: u32, port: u16) -> Result<(), &'static str> {
    let mut net = Net::connect_to("ts").map_err(|_| "connect to svc:ts")?;
    let s = net.udp_bind(0).map_err(|_| "udp bind")?;
    let (mut min_us, mut max_us) = (u32::MAX, 0u32);
    let mut sent_at = [0u64; ROUNDS as usize];
    let mut back = [false; ROUNDS as usize];
    let mut late = 0u32;
    let mut buf = [0u8; 64];
    // Rounds, then the last wait (round ROUNDS).
    for i in 0..=ROUNDS as usize {
        let start = time_ns();
        if i < ROUNDS as usize {
            let msg = [b't', b's', b'g', b'e', b't', b'0' + i as u8];
            sent_at[i] = start;
            net.udp_send(s, ip, port, &msg).map_err(|_| "udp send")?;
        }
        let end = start + ECHO_MS * 1_000_000;
        loop {
            let now = time_ns();
            if now >= end || back.iter().all(|&b| b) {
                break;
            }
            net.udp_arm().map_err(|_| "udp arm")?;
            let _ = wait(&[net.handle()], end - now);
            let Ok((n, from, _)) = net.udp_recv(s, &mut buf, 0) else { continue };
            // Which round's: `tsgetN`.
            let k = match &buf[..n] {
                [b't', b's', b'g', b'e', b't', d] if from == ip && d.wrapping_sub(b'0') < ROUNDS as u8 => (d - b'0') as usize,
                _ => continue,
            };
            if back[k] {
                continue;
            }
            back[k] = true;
            let us = ((time_ns() - sent_at[k]) / 1000) as u32;
            (min_us, max_us) = (min_us.min(us), max_us.max(us));
            if k != i {
                late += 1;
            } else {
                break;
            }
        }
    }
    let _ = net.close(s);
    let back = back.iter().filter(|&&b| b).count() as u32;
    event!(Info, "tsget.udp", ip = ip, port = port as u32, sent = ROUNDS, back = back, late = late, min_us = if back > 0 { min_us } else { 0 }, max_us = max_us);
    if back == 0 { Err("no echo") } else { Ok(()) }
}
