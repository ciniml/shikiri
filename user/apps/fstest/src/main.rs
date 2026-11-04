#![no_std]
#![no_main]

//! M7c test client (doc/18 §5), in its own folder (`~`, doc/52 §3.4):
//! writes `~/TEST.BIN` (`kib` KiB, default 100), reads it back and
//! compares, creates and deletes `~/DEL.TXT`, and lists `~`. Then checks
//! that the rest of the card is refused: the root's listing, reading
//! `T5OS/START.CFG`, making `T5OS/OTHER`, and `..` out of its folder.
//!
//! Built with `--permit svc:fs` (no grants) and the app key. Exit 0 if
//! everything held, otherwise the number of the failed check.

use t5_rt::fs::{flag, status};
use t5_rt::{Files, FsError, event, sleep_ms, time_ns};

t5_rt::entry!(main);

fn main(kib: usize, _: usize) -> usize {
    match run(if kib == 0 { 100 } else { kib }) {
        Ok(()) => 0,
        Err(check) => {
            event!(Error, "fstest.failed", check = check);
            check as usize
        }
    }
}

fn byte(i: usize) -> u8 {
    (i * 31 + i / 4096) as u8
}

fn run(kib: usize) -> Result<(), u32> {
    // The service may still be starting: try for up to 3 s.
    let mut tries = 0;
    let mut fs = loop {
        match Files::connect() {
            Ok(f) => break f,
            Err(_) if tries < 30 => {
                tries += 1;
                sleep_ms(100);
            }
            Err(_) => return Err(1),
        }
    };
    // Its own folder is made when first named.
    let n = fs.list("~", |name, size, dir| event!(Info, "fstest.entry", dir = "~", name = name, size = size, is_dir = dir)).map_err(|_| 2u32)?;
    event!(Info, "fstest.listed", entries = n);

    let len = kib * 1024;
    let mut chunk = [0u8; 4096];
    let f = fs.open("~/TEST.BIN", flag::WRITE | flag::CREATE).map_err(|_| 4u32)?;
    let t = time_ns();
    let mut off = 0;
    while off < len {
        let k = (len - off).min(chunk.len());
        for (i, b) in chunk[..k].iter_mut().enumerate() {
            *b = byte(off + i);
        }
        fs.write(f, &chunk[..k]).map_err(|_| 5u32)?;
        off += k;
    }
    fs.close(f).map_err(|_| 6u32)?;
    let write_ms = (time_ns() - t) / 1_000_000;

    let f = fs.open("~/TEST.BIN", flag::READ).map_err(|_| 7u32)?;
    if f.size as usize != len {
        event!(Error, "fstest.size", expected = len as u32, got = f.size);
        return Err(8);
    }
    let t = time_ns();
    let mut off = 0;
    loop {
        let k = fs.read(f, &mut chunk).map_err(|_| 9u32)?;
        if k == 0 {
            break;
        }
        if chunk[..k].iter().enumerate().any(|(i, &b)| b != byte(off + i)) {
            return Err(10);
        }
        off += k;
    }
    fs.close(f).map_err(|_| 11u32)?;
    let read_ms = (time_ns() - t) / 1_000_000;
    if off != len {
        return Err(12);
    }

    let f = fs.open("~/DEL.TXT", flag::WRITE | flag::CREATE).map_err(|_| 13u32)?;
    fs.write(f, b"delete me").map_err(|_| 14u32)?;
    fs.close(f).map_err(|_| 15u32)?;
    fs.delete("~/DEL.TXT").map_err(|_| 16u32)?;
    if fs.open("~/DEL.TXT", flag::READ).err() != Some(FsError::Status(status::NOT_FOUND)) {
        return Err(17);
    }
    fs.list("~", |name, size, dir| event!(Info, "fstest.entry", dir = "~", name = name, size = size, is_dir = dir)).map_err(|_| 18u32)?;
    // The rest of the card (doc/52).
    let denied = |r: Result<(), FsError>| r == Err(FsError::Status(status::DENIED));
    if !denied(fs.list("", |_, _, _| {}).map(|_| ())) {
        return Err(19);
    }
    if !denied(fs.open("T5OS/START.CFG", flag::READ).map(|_| ())) {
        return Err(20);
    }
    if !denied(fs.mkdir("T5OS/OTHER")) {
        return Err(21);
    }
    if !(fs.open("~/../START.CFG", flag::READ).err() == Some(FsError::Status(status::BAD_NAME))) {
        return Err(22);
    }
    event!(Info, "fstest.denied_ok");
    event!(Info, "fstest.done", bytes = len as u32, write_ms = write_ms as u32, read_ms = read_ms as u32);
    Ok(())
}
