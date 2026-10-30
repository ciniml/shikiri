#![no_std]
#![no_main]

//! M5c test app (doc/16 §5.4): both ends of the channel `chan:echo`.
//! - `role` 0, the server: sends every message back with each byte plus 1,
//!   and ends when the client has closed its end.
//! - `role` 1, the client: checks the channel rules, then sends `count`
//!   messages of varying length and checks each reply.
//!
//! - `role` 2, a service server (N0a, doc/19): listens on `svc:echo`, serves
//!   `count` clients at once (each its own channel), ends when all left.
//! - `role` 3, a service client: connects to `svc:echo` and does the same
//!   round trips as role 1 (from its own `count`).
//!
//! Built with `--permit chan:echo --permit listen:echo --permit svc:echo`.
//! Exit 0 if everything held, otherwise the number of the failed check.

use t5_rt::{Channel, Error, FOREVER, Listener, chan, event, svc, wait_raw};

t5_rt::entry!(main);

const SERVER: usize = 0;

fn main(role: usize, count: usize) -> usize {
    let r = match role {
        SERVER => serve(),
        1 => client(count as u32),
        2 => serve_many(count),
        _ => svc_client(count as u32),
    };
    match r {
        Ok(()) => 0,
        Err(check) => {
            event!(Error, "echo.failed", role = role, check = check);
            check as usize
        }
    }
}

fn check(ok: bool, n: u32) -> Result<(), u32> {
    if ok { Ok(()) } else { Err(n) }
}

fn serve() -> Result<(), u32> {
    let c = Channel::open("echo").map_err(|_| 1u32)?;
    let mut buf = [0u8; chan::MSG_MAX];
    let mut served = 0u32;
    loop {
        let n = match c.recv(&mut buf) {
            Ok(n) => n,
            Err(Error::Off) => break,
            Err(_) => return Err(2),
        };
        for b in &mut buf[..n] {
            *b = b.wrapping_add(1);
        }
        c.send(&buf[..n]).map_err(|_| 3u32)?;
        served += 1;
    }
    event!(Info, "echo.served", messages = served);
    Ok(())
}

/// Serves `clients` connections on `svc:echo` together.
fn serve_many(clients: usize) -> Result<(), u32> {
    let l = Listener::open("echo").map_err(|_| 30u32)?;
    // A second listener on the same name is refused.
    check(Listener::open("echo").err() == Some(Error::Busy), 31)?;
    let mut conns: [Option<Channel>; 8] = [const { None }; 8];
    let (mut accepted, mut done, mut served) = (0usize, 0usize, 0u32);
    let mut buf = [0u8; chan::MSG_MAX];
    while done < clients {
        let mut hs = [0u32; 9];
        hs[0] = l.0.raw();
        let mut map = [0usize; 9];
        let mut n = 1;
        for (i, c) in conns.iter().enumerate() {
            if let Some(c) = c {
                hs[n] = c.0.raw();
                map[n] = i;
                n += 1;
            }
        }
        let ev = wait_raw(&hs[..n], FOREVER).map_err(|_| 32u32)?;
        if ev.index == 0 {
            check(ev.kind == svc::PENDING, 33)?;
            let c = l.accept().map_err(|_| 34u32)?;
            let slot = conns.iter().position(Option::is_none).ok_or(35u32)?;
            conns[slot] = Some(c);
            accepted += 1;
            continue;
        }
        let i = map[ev.index];
        let c = conns[i].as_ref().unwrap();
        if ev.kind == chan::PEER_CLOSED {
            conns[i] = None;
            done += 1;
            continue;
        }
        while let Ok(len) = c.0.read(&mut buf) {
            for b in &mut buf[..len] {
                *b = b.wrapping_add(1);
            }
            c.send(&buf[..len]).map_err(|_| 36u32)?;
            served += 1;
        }
    }
    event!(Info, "echo.served_many", clients = accepted as u32, messages = served);
    Ok(())
}

fn svc_client(count: u32) -> Result<(), u32> {
    let c = Channel::connect("echo").map_err(|_| 40u32)?;
    let mut msg = [0u8; chan::MSG_MAX];
    let mut buf = [0u8; chan::MSG_MAX + 1];
    for k in 0..count {
        let len = message(k, &mut msg);
        c.send(&msg[..len]).map_err(|_| 41u32)?;
        let n = c.recv(&mut buf).map_err(|_| 42u32)?;
        check(reply_ok(k, &buf[..n]), 43)?;
    }
    event!(Info, "echo.svc_done", messages = count);
    Ok(())
}

/// Message `k`: its length and its bytes.
fn message(k: u32, buf: &mut [u8; chan::MSG_MAX]) -> usize {
    let len = (k as usize * 37) % (chan::MSG_MAX + 1);
    for (i, b) in buf[..len].iter_mut().enumerate() {
        *b = (k as usize + i) as u8;
    }
    len
}

fn reply_ok(k: u32, got: &[u8]) -> bool {
    let mut want = [0u8; chan::MSG_MAX];
    let len = message(k, &mut want);
    got.len() == len && got.iter().zip(&want[..len]).all(|(g, w)| *g == w.wrapping_add(1))
}

fn client(count: u32) -> Result<(), u32> {
    // Only names in the Permissions TLV.
    check(Channel::open("secret").err() == Some(Error::Denied), 10)?;
    let c = Channel::open("echo").map_err(|_| 11u32)?;
    // Both ends are taken now.
    check(Channel::open("echo").err() == Some(Error::Busy), 12)?;
    let mut buf = [0u8; chan::MSG_MAX + 1];
    check(c.0.write(&buf).err() == Some(Error::Size), 13)?;
    // Channel handles carry no CTL right.
    check(c.0.ctl(1, 0).err() == Some(Error::Denied), 14)?;

    // Fill the queues without reading the replies: the server stops once
    // its replies fill ours, so a write must get Busy within the two queues
    // and the one message the server holds.
    let mut sent = 0u32;
    let mut msg = [0u8; chan::MSG_MAX];
    loop {
        let len = message(sent, &mut msg);
        match c.0.write(&msg[..len]) {
            Ok(n) => check(n == len, 15)?,
            Err(Error::Busy) => break,
            Err(_) => return Err(16),
        }
        sent += 1;
        check(sent <= 2 * chan::QUEUE as u32 + 1, 17)?;
    }
    for k in 0..sent {
        // A reply longer than the buffer stays queued.
        if k == 1 {
            check(c.0.read(&mut buf[..0]).err() == Some(Error::Size), 18)?;
        }
        let n = c.recv(&mut buf).map_err(|_| 19u32)?;
        check(reply_ok(k, &buf[..n]), 20)?;
    }

    // One at a time.
    let mut bytes = 0u32;
    for k in sent..sent + count {
        let len = message(k, &mut msg);
        c.send(&msg[..len]).map_err(|_| 21u32)?;
        let n = c.recv(&mut buf).map_err(|_| 22u32)?;
        check(reply_ok(k, &buf[..n]), 23)?;
        bytes += n as u32;
    }
    event!(Info, "echo.done", messages = sent + count, filled = sent, bytes = bytes);
    Ok(())
}
