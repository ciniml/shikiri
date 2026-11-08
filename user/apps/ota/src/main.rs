#![no_std]
#![no_main]

//! The update service (doc/67): installs an app (T5A) or a kernel package
//! (T5K) from the SD card through the kernel's `update` object, for a client
//! on `svc:ota` (the shell's `ota`). A request is one message:
//!
//! - `app PATH`: installs the app image at PATH.
//! - `kernel PATH`: writes the kernel package at PATH into the other slot;
//!   it takes effect after `reboot` and confirms itself after its self-test.
//! - `reboot`: restarts.
//!
//! Each request gets one reply: `ok ...` or `error ...`. Images are checked
//! by the kernel as from the host (signature, ABI, `secure_version`).
//!
//! Built with `--permit listen:ota --permit update --permit svc:fs/r:`.

use core::fmt::Write as _;

use t5_rt::log::FmtBuf;
use t5_rt::{Channel, Error, FOREVER, Files, Handle, Listener, chan, event, rights, update, wait, yield_now};

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

fn handle(req: &str, reply: &mut FmtBuf<200>) -> Result<(), &'static str> {
    let (cmd, arg) = req.split_once(' ').unwrap_or((req, ""));
    match cmd {
        "app" | "kernel" => {
            let kernel = cmd == "kernel";
            let path = arg.trim();
            if path.is_empty() {
                return Err("usage: app|kernel PATH");
            }
            let len = from_file(path, kernel)?;
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

/// Installs the image in file `path`; its length.
fn from_file(path: &str, kernel: bool) -> Result<u32, &'static str> {
    let mut f = Files::connect().map_err(|_| "no file service")?;
    let id = f.open(path, t5_rt::fs::flag::READ).map_err(|_| "cannot open the file")?;
    let len = id.size;
    let r = install(kernel, len, |buf| f.read(id, buf).map_err(|_| "cannot read the file"));
    let _ = f.close(id);
    r.map(|()| len)
}

/// Sends an image of `len` bytes, read by `read` a piece at a time, to the
/// kernel: begin, data, end. On a failure the kernel drops what was begun
/// when the handle closes.
pub fn install(kernel: bool, len: u32, mut read: impl FnMut(&mut [u8]) -> Result<usize, &'static str>) -> Result<(), &'static str> {
    let h = Handle::open(update::NAME, rights::WRITE | rights::WAIT).map_err(|_| "update is busy")?;
    let mut begin = [if kernel { update::op::BEGIN_KERNEL } else { update::op::BEGIN_APP }, 0, 0, 0, 0];
    begin[1..5].copy_from_slice(&len.to_le_bytes());
    step(&h, &begin).map_err(|_| "refused at the start (too big?)")?;
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
