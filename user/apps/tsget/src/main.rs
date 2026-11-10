#![no_std]
#![no_main]

//! `run tsget IP PORT`: one HTTP/1.0 `GET /` to a tailnet peer through
//! `svc:ts` (doc/71 §13), to try the service: the status line and the
//! bytes, as `tsget.done`. IP as a number (100.64.0.2 is 1681915906);
//! PORT 0 is 80.

use t5_rt::{Net, event};

t5_rt::entry!(main);

fn main(ip: usize, port: usize) -> usize {
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
