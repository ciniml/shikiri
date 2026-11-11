#![no_std]
#![no_main]

//! Starts the services at boot from a list (doc/38). `run init MODE`:
//!
//! - 0: the list in `T5OS/START.CFG` on the SD card (through `svc:fs`), or
//!   the built-in one if the file cannot be read;
//! - 1: writes the built-in list to `T5OS/START.CFG` (to start from);
//! - 3: writes the panel's list there: the same, with `mtdash` (the Matter
//!   controller's tiles, doc/49 §25) on the screen in place of `term`;
//! - 2: the test list (t5emu): an app, one that is not in the store and
//!   one that fails (the kernel starts it again and gives up, doc/56 §3);
//!   then checks that the kernel refuses a name longer than the store's.
//!
//! A line of the list is `NAME [ARG0 [ARG1]]` (decimal or 0x hex); `#`
//! starts a comment. Each app is started through `proc:spawn`: the kernel
//! starts it with its own permissions and logs `proc.spawned` or
//! `proc.spawn_failed`. The apps of the list are watched: one that faults
//! or ends with a code other than 0 is started again (`spawn::RESTART`,
//! doc/56 §3).
//!
//! The kernel autostarts `fs` and `init` (doc/30 §5). Built with `--permit
//! proc:spawn --permit svc:fs`. Exit 0, or the step that failed.

use t5_rt::{Error, Files, Handle, event, fs, rights, sleep_ms, spawn};

t5_rt::entry!(main);

const LIST_FILE: &str = "T5OS/START.CFG";
const FILE_MAX: usize = 2 << 10;

/// Without a list on the SD card: the settings (doc/50: `wifi` reads its
/// network there), the network services (`wifi 1`: with Bluetooth),
/// Bluetooth LE and Matter (doc/49 §22: `ble` waits for `ready:bt`,
/// `matter` for `svc:net`, doc/51 §3), the font service and the terminal
/// (doc/45: `font` first, `term` connects to it), and Tailscale (doc/71:
/// `ts` with mode 0 runs only on a node that joined a tailnet already).
const DEFAULT_LIST: &str = "\
cfg
wifi 1
net
netctl
ble
matter
font
term
ts
";

/// The panel's (`run init 3`): `mtdash` owns the display instead of `term`.
const PANEL_LIST: &str = "\
cfg
wifi 1
net
netctl
ble
matter
mtdash
ts
";

const TEST_LIST: &str = "\
# The test list (run init 2).
hello 0 0x0
no-such-app
# Ends with 3 each time: started again, then given up.
hello 3
";

fn main(mode: usize, _: usize) -> usize {
    let r = match mode {
        0 => from_file(),
        1 => write_file(DEFAULT_LIST),
        3 => write_file(PANEL_LIST),
        2 => start(TEST_LIST).and_then(|n| too_long().map(|_| n)),
        _ => Err(1),
    };
    match r {
        Ok(n) => {
            event!(Info, "init.done", mode = mode as u32, requested = n);
            0
        }
        Err(step) => {
            event!(Error, "init.failed", mode = mode as u32, step = step);
            step as usize
        }
    }
}

fn files() -> Result<Files, u32> {
    // At boot `fs` may not be up yet.
    for _ in 0..100 {
        if let Ok(f) = Files::connect() {
            return Ok(f);
        }
        sleep_ms(100);
    }
    Err(10)
}

/// Reads the list; `None` if there is none to read.
fn read_list(buf: &mut [u8]) -> Option<usize> {
    let mut files = files().ok()?;
    let f = files.open(LIST_FILE, fs::flag::READ).ok()?;
    let mut n = 0;
    while n < buf.len() {
        match files.read(f, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return None,
        }
    }
    let _ = files.close(f);
    Some(n)
}

fn from_file() -> Result<u32, u32> {
    let mut buf = [0u8; FILE_MAX];
    match read_list(&mut buf).map(|n| core::str::from_utf8(&buf[..n])) {
        Some(Ok(text)) => {
            event!(Info, "init.list", file = LIST_FILE, bytes = text.len() as u32);
            start(text)
        }
        _ => {
            event!(Warn, "init.no_list", file = LIST_FILE);
            start(DEFAULT_LIST)
        }
    }
}

fn write_file(list: &str) -> Result<u32, u32> {
    let mut files = files()?;
    let _ = files.mkdir("T5OS");
    let f = files.open(LIST_FILE, fs::flag::WRITE | fs::flag::CREATE).map_err(|_| 20u32)?;
    files.write(f, b"# tab5-os: apps started at boot (doc/38), read by init. NAME [ARG0 [ARG1]]\n").map_err(|_| 21u32)?;
    files.write(f, list.as_bytes()).map_err(|_| 21u32)?;
    files.close(f).map_err(|_| 22u32)?;
    event!(Info, "init.written", file = LIST_FILE, bytes = list.len() as u32);
    Ok(0)
}

/// Asks for each app of `list` in order; returns how many were queued.
fn start(list: &str) -> Result<u32, u32> {
    let h = Handle::open(spawn::NAME, rights::WRITE | rights::CTL).map_err(|_| 2u32)?;
    h.ctl(spawn::RESTART, 1).map_err(|_| 2u32)?;
    let mut queued = 0;
    for (i, line) in list.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, arg0, arg1)) = parse(line) else {
            event!(Warn, "init.bad_line", line = i as u32 + 1);
            continue;
        };
        if name == "init" {
            continue;
        }
        let mut req = [0u8; spawn::ARGS + spawn::NAME_MAX];
        if name.len() > spawn::NAME_MAX {
            event!(Warn, "init.name_too_long", line = i as u32 + 1);
            continue;
        }
        req[0..4].copy_from_slice(&arg0.to_le_bytes());
        req[4..8].copy_from_slice(&arg1.to_le_bytes());
        req[8..8 + name.len()].copy_from_slice(name.as_bytes());
        // The kernel's queue empties every service round (10 ms).
        let mut tries = 0;
        loop {
            match h.write(&req[..8 + name.len()]) {
                Ok(_) => break,
                Err(Error::Busy) if tries < 100 => {
                    tries += 1;
                    sleep_ms(10);
                }
                Err(_) => return Err(3),
            }
        }
        queued += 1;
    }
    Ok(queued)
}

/// The kernel refuses a name longer than `spawn::NAME_MAX`.
fn too_long() -> Result<(), u32> {
    let h = Handle::open(spawn::NAME, rights::WRITE).map_err(|_| 2u32)?;
    let req = [b'a'; spawn::ARGS + spawn::NAME_MAX + 1];
    match h.write(&req) {
        Err(Error::Size) => Ok(()),
        _ => Err(4),
    }
}

/// `NAME [ARG0 [ARG1]]`.
fn parse(line: &str) -> Option<(&str, u32, u32)> {
    let mut words = line.split_whitespace();
    let name = words.next()?;
    let mut arg = || words.next().map_or(Some(0), number);
    let (arg0, arg1) = (arg()?, arg()?);
    if words.next().is_some() {
        return None;
    }
    Some((name, arg0, arg1))
}

fn number(s: &str) -> Option<u32> {
    match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}
