#![no_std]
#![no_main]

//! The update service (doc/67): installs an app (T5A) or a kernel package
//! (T5K) from the SD card through the kernel's `update` object, for a client
//! on `svc:ota` (the shell's `ota`). A request is one message:
//!
//! - `app PATH`: installs the app image at PATH: a file on the SD card, or
//!   `http://HOST[:PORT]/PATH` (fetched with HTTP/1.0 `GET`; the answer
//!   must be 200 with a `Content-Length`). A tailnet host (100.64.0.0/10
//!   or `.ts.net`) is reached through `svc:ts`, which `ts` serves.
//! - `kernel PATH`: writes the kernel package at PATH (as for `app`) into
//!   the other slot; it takes effect after `reboot` and confirms itself
//!   after its self-test.
//! - `reboot`: restarts.
//!
//! Each request gets one reply: `ok ...` or `error ...`. Images are checked
//! by the kernel as from the host (signature, ABI, `secure_version`).
//!
//! A kernel fetched over the network confirms itself only once the network
//! works (doc/67 §5): `ota` gives the kernel the server, and when it starts
//! (at boot) and the running kernel waits for that, it connects to the
//! server until it can, then tells the kernel (`update` `CONFIRM`).
//!
//! Built with `--permit listen:ota --permit update --permit svc:fs/r:
//! --permit svc:net/tcp-connect --permit svc:net/resolve --permit
//! svc:ts/tcp-connect --permit svc:ts/resolve`.

use core::fmt::Write as _;

use t5_rt::log::FmtBuf;
use t5_rt::{Channel, Error, FOREVER, Files, Handle, Listener, Net, chan, event, rights, sleep_ms, time_ns, update, wait, yield_now};

t5_rt::entry!(main);

/// How long one request to the kernel may take (an erase is ~50 ms; a
/// kernel package's check reads it all).
const STEP_NS: u64 = 30_000_000_000;

fn main(_: usize, _: usize) -> usize {
    let Ok(l) = Listener::open("ota") else {
        event!(Error, "ota.no_listener");
        return 1;
    };
    event!(Info, "ota.ready");
    confirm_network();
    loop {
        if wait(&[&l.0], FOREVER).is_err() {
            return 2;
        }
        let Ok(c) = l.accept() else { continue };
        serve(&c);
    }
}

/// Answers a client's requests until it goes away.
fn serve(c: &Channel) {
    let mut msg = [0u8; chan::MSG_MAX];
    while let Ok(n) = c.recv(&mut msg) {
        let req = core::str::from_utf8(&msg[..n]).unwrap_or("");
        let mut reply = FmtBuf::<200>::new();
        match handle(req, &mut reply) {
            Ok(()) => {}
            Err(why) => {
                reply = FmtBuf::new();
                let _ = write!(reply, "error {why}");
            }
        }
        let _ = c.send(reply.as_str().as_bytes());
    }
}

/// How often to try the server while the kernel waits for the network.
const CONFIRM_RETRY_MS: u64 = 3000;

/// If the running kernel waits for the network (doc/67 §5): connects to
/// its server until that works, then confirms the kernel. The kernel goes
/// back to the old one by itself if this does not come in time.
fn confirm_network() {
    let Ok(h) = Handle::open(update::NAME, rights::READ | rights::WRITE | rights::WAIT) else { return };
    let mut st = [0u8; update::STATUS_SIZE];
    if h.read(&mut st).is_err() || u16::from_le_bytes([st[6], st[7]]) & update::AWAITING == 0 {
        return;
    }
    let ip = u32::from_le_bytes([st[0], st[1], st[2], st[3]]);
    let port = u16::from_le_bytes([st[4], st[5]]);
    event!(Info, "ota.confirming", ip = t5_rt::log::Addr(ip), port = port as u32);
    let mut tries = 0u32;
    loop {
        tries += 1;
        if let Ok(net) = Net::connect_to(service(ip, ""))
            && let Ok(s) = net.tcp_connect(ip, port)
        {
            let ok = net.wait_established(s, 5000).is_ok();
            let _ = net.close(s);
            if ok {
                break;
            }
        }
        sleep_ms(CONFIRM_RETRY_MS);
    }
    match step(&h, &[update::op::CONFIRM]) {
        Ok(()) => event!(Info, "ota.confirmed", tries = tries),
        Err(why) => event!(Warn, "ota.confirm_failed", error = why),
    }
}

fn handle(req: &str, reply: &mut FmtBuf<200>) -> Result<(), &'static str> {
    let (cmd, arg) = req.split_once(' ').unwrap_or((req, ""));
    match cmd {
        "app" | "kernel" => {
            let kernel = cmd == "kernel";
            let path = arg.trim();
            if path.is_empty() {
                return Err("usage: app|kernel PATH");
            }
            let len = if path.starts_with("http://") { from_http(path, kernel)? } else { from_file(path, kernel)? };
            event!(Info, "ota.installed", kind = cmd, path = path, bytes = len);
            let _ = write!(reply, "ok {cmd} {path}: {len} bytes");
            if kernel {
                let _ = write!(reply, "; reboot to start it");
            }
            Ok(())
        }
        "reboot" => {
            let h = Handle::open(update::NAME, rights::WRITE | rights::WAIT).map_err(|_| "update is busy")?;
            step(&h, &[update::op::REBOOT])?;
            Ok(())
        }
        _ => Err("unknown request (app PATH, kernel PATH, reboot)"),
    }
}

/// No data for this long ends a download.
const IDLE_NS: u64 = 20_000_000_000;

/// Installs the image at `url` (`http://HOST[:PORT]/PATH`); its length.
fn from_http(url: &str, kernel: bool) -> Result<u32, &'static str> {
    let rest = &url["http://".len()..];
    let (hostport, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "bad port")?),
        None => (hostport, 80),
    };
    let mut net = Net::connect_to(service(parse_ip(host).unwrap_or(0), host)).map_err(|_| "no network service")?;
    let ip = match parse_ip(host) {
        Some(ip) => ip,
        None => net.resolve(host, 3000).map_err(|_| "cannot resolve the host")?,
    };
    let s = net.tcp_connect(ip, port).map_err(|_| "cannot connect")?;
    let r = (|| {
        net.wait_established(s, 5000).map_err(|_| "cannot connect")?;
        let mut req = FmtBuf::<300>::new();
        let _ = write!(req, "GET {path} HTTP/1.0\r\nHost: {hostport}\r\nUser-Agent: tab5-os-ota\r\n\r\n");
        net.send_all(s, req.as_str().as_bytes()).map_err(|_| "cannot send the request")?;
        // The head, and the body's first bytes after it.
        let mut head = [0u8; 2048];
        let mut n = 0;
        let end = loop {
            if let Some(i) = head[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if n == head.len() {
                return Err("the answer's head is too long");
            }
            let k = recv(&mut net, s, &mut head[n..])?;
            if k == 0 {
                return Err("the server closed early");
            }
            n += k;
        };
        let text = core::str::from_utf8(&head[..end]).map_err(|_| "bad answer")?;
        let status = text.split_whitespace().nth(1).unwrap_or("");
        if status != "200" {
            event!(Warn, "ota.http_status", status = status);
            return Err("the server did not answer 200");
        }
        let len = text
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim()))
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or("no Content-Length")?;
        let (mut from, extra) = (end, n);
        install(kernel, len, Some((ip, port)), |buf| {
            if from < extra {
                let k = (extra - from).min(buf.len());
                buf[..k].copy_from_slice(&head[from..from + k]);
                from += k;
                return Ok(k);
            }
            recv(&mut net, s, buf)
        })?;
        Ok(len)
    })();
    let _ = net.close(s);
    r
}

/// Data from TCP connection `s`, waiting up to `IDLE_NS`; 0 when the peer
/// closed.
fn recv(net: &mut Net, s: u16, buf: &mut [u8]) -> Result<usize, &'static str> {
    let deadline = time_ns() + IDLE_NS;
    loop {
        let r = net.recv_now(s, buf).map_err(|_| "the connection broke")?;
        if r.len > 0 || r.ended {
            return Ok(r.len);
        }
        if time_ns() > deadline {
            return Err("the download stalled");
        }
        sleep_ms(1);
    }
}

/// The network service for a host: `ts` (the tailnet, doc/71 §14) for an
/// address in 100.64.0.0/10 or a name in `.ts.net`, else `net`.
fn service(ip: u32, host: &str) -> &'static str {
    if ip >> 22 == (100 << 2 | 1) || host.trim_end_matches('.').ends_with(".ts.net") { "ts" } else { "net" }
}

fn parse_ip(s: &str) -> Option<u32> {
    let mut ip = 0u32;
    let mut parts = 0;
    for p in s.split('.') {
        ip = ip << 8 | p.parse::<u8>().ok()? as u32;
        parts += 1;
    }
    (parts == 4).then_some(ip)
}

/// Installs the image in file `path`; its length.
fn from_file(path: &str, kernel: bool) -> Result<u32, &'static str> {
    let mut f = Files::connect().map_err(|_| "no file service")?;
    let id = f.open(path, t5_rt::fs::flag::READ).map_err(|_| "cannot open the file")?;
    let len = id.size;
    let r = install(kernel, len, None, |buf| f.read(id, buf).map_err(|_| "cannot read the file"));
    let _ = f.close(id);
    r.map(|()| len)
}

/// Sends an image of `len` bytes, read by `read` a piece at a time, to the
/// kernel: begin, data, end. A kernel from `server` (fetched over the
/// network) confirms itself only once that is reached again. On a failure
/// the kernel drops what was begun when the handle closes.
pub fn install(
    kernel: bool,
    len: u32,
    server: Option<(u32, u16)>,
    mut read: impl FnMut(&mut [u8]) -> Result<usize, &'static str>,
) -> Result<(), &'static str> {
    let h = Handle::open(update::NAME, rights::WRITE | rights::WAIT).map_err(|_| "update is busy")?;
    let mut begin = [0u8; 11];
    begin[0] = if kernel { update::op::BEGIN_KERNEL } else { update::op::BEGIN_APP };
    begin[1..5].copy_from_slice(&len.to_le_bytes());
    let n = match server.filter(|_| kernel) {
        Some((ip, port)) => {
            begin[5..9].copy_from_slice(&ip.to_le_bytes());
            begin[9..11].copy_from_slice(&port.to_le_bytes());
            11
        }
        None => 5,
    };
    step(&h, &begin[..n]).map_err(|_| "refused at the start (too big?)")?;
    let mut req = [0u8; update::REQUEST_MAX];
    let mut offset = 0u32;
    while offset < len {
        let want = ((len - offset) as usize).min(update::DATA_MAX);
        let mut got = 0;
        while got < want {
            let n = read(&mut req[5 + got..5 + want])?;
            if n == 0 {
                return Err("the image ended early");
            }
            got += n;
        }
        req[0] = update::op::DATA;
        req[1..5].copy_from_slice(&offset.to_le_bytes());
        step(&h, &req[..5 + want]).map_err(|_| "writing failed")?;
        offset += want as u32;
    }
    step(&h, &[update::op::END]).map_err(|_| "the image was rejected (signature, ABI or version)")
}

/// One request to the kernel, waiting for its `DONE`.
fn step(h: &Handle, req: &[u8]) -> Result<(), &'static str> {
    loop {
        match h.write(req) {
            Ok(_) => break,
            Err(Error::Busy) => yield_now(),
            Err(_) => return Err("bad request"),
        }
    }
    match wait(&[h], STEP_NS) {
        Ok(ev) if ev.kind == update::DONE && ev.value == update::status::OK => Ok(()),
        Ok(_) => Err("failed"),
        Err(_) => Err("no answer"),
    }
}
