#![no_std]
#![no_main]

//! `run tsget IP PORT`: one HTTP/1.0 `GET /` to a tailnet peer through
//! `svc:ts` (doc/71 §13), to try the service: the status line and the
//! bytes, as `tsget.done`. IP as a number (100.64.0.2 is 1681915906);
//! PORT 0 is 80. PORT + 0x10000: UDP instead (doc/71 §14.6), `ROUNDS`
//! datagrams to an echo server there, each waited for with UDP_WAIT, as
//! `tsget.udp`.

use t5_rt::{Net, event, time_ns, wait};

t5_rt::entry!(main);

fn main(ip: usize, port: usize) -> usize {
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

/// Datagrams to an echo server: how many came back, and the round trips.
fn echo(ip: u32, port: u16) -> Result<(), &'static str> {
    let mut net = Net::connect_to("ts").map_err(|_| "connect to svc:ts")?;
    let s = net.udp_bind(0).map_err(|_| "udp bind")?;
    let (mut back, mut min_us, mut max_us) = (0u32, u32::MAX, 0u32);
    let mut buf = [0u8; 64];
    for i in 0..ROUNDS {
        let msg = [b't', b's', b'g', b'e', b't', b'0' + i as u8];
        let start = time_ns();
        net.udp_send(s, ip, port, &msg).map_err(|_| "udp send")?;
        let end = start + ECHO_MS * 1_000_000;
        loop {
            let now = time_ns();
            if now >= end {
                break;
            }
            net.udp_arm().map_err(|_| "udp arm")?;
            let _ = wait(&[net.handle()], end - now);
            let Ok((n, from, _)) = net.udp_recv(s, &mut buf, 0) else { continue };
            if from == ip && buf[..n] == msg {
                let us = ((time_ns() - start) / 1000) as u32;
                (back, min_us, max_us) = (back + 1, min_us.min(us), max_us.max(us));
                break;
            }
        }
    }
    let _ = net.close(s);
    event!(Info, "tsget.udp", ip = ip, port = port as u32, sent = ROUNDS, back = back, min_us = if back > 0 { min_us } else { 0 }, max_us = max_us);
    if back == 0 { Err("no echo") } else { Ok(()) }
}
