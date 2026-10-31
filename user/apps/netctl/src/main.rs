#![no_std]
#![no_main]

//! Loads the network service's filter rules (N4, doc/24 §4) over
//! `svc:netctl`. `run netctl MODE`:
//!
//! - 0: the rule file `T5OS/NET.RUL` on the SD card (through `svc:fs`);
//! - 1: the test set (t5emu: the PC may start anything towards the Tab5 and
//!   the test LAN; the test LAN may start nothing);
//! - 2: the guard set (only the PC on the host link may start traffic to
//!   the Tab5; nothing is forwarded that the PC did not start);
//! - 3: no rules (everything passes);
//! - 4: log each rule's count (`net.rule_hits`);
//! - 5: the NAT test set (t5emu, doc/25): the test LAN masqueraded behind
//!   the host link, a DHCP server on the test LAN;
//! - 6: the NAT set for the Tab5: the test LAN and the PC masqueraded
//!   behind Wi-Fi, a DHCP server on the test LAN.
//!
//! Built with `--permit svc:netctl --permit svc:fs`. Exit 0 if the service
//! took the rules; otherwise the step that failed, or 100 + the first line
//! the service could not read.

use t5_rt::net::{self, ctl};
use t5_rt::{Channel, Files, chan, event, fs, sleep_ms};

t5_rt::entry!(main);

const RULE_FILE: &str = "T5OS/NET.RUL";
const FILE_MAX: usize = 8 << 10;

const TEST_SET: &str = "\
policy in drop log
policy forward drop log
pass in on host
pass in state established,related
pass forward on host out test0
pass forward state established,related
";

const GUARD_SET: &str = "\
policy in drop
policy forward drop
pass in on host
pass in state established,related
pass forward on host
pass forward state established,related
";

const NAT_TEST_SET: &str = "\
masquerade on host from 192.168.50.0/24
dhcp-server on test0 range 192.168.50.100-192.168.50.149
";

const NAT_SET: &str = "\
masquerade on wlan0 from 192.168.50.0/24
masquerade on wlan0 from 10.55.0.0/24
dhcp-server on test0 range 192.168.50.100-192.168.50.149
";

fn main(mode: usize, _: usize) -> usize {
    let r = match mode {
        0 => from_file(),
        1 => load(TEST_SET),
        2 => load(GUARD_SET),
        3 => simple(ctl::CLEAR),
        4 => simple(ctl::STATS),
        5 => load(NAT_TEST_SET),
        6 => load(NAT_SET),
        _ => Err(1),
    };
    match r {
        Ok(n) => {
            event!(Info, "netctl.done", mode = mode as u32, rules = n);
            0
        }
        Err(step) => {
            event!(Error, "netctl.failed", mode = mode as u32, step = step);
            step as usize
        }
    }
}

/// Connects to the service, which may still be starting (up to 3 s).
fn connect() -> Result<Channel, u32> {
    for _ in 0..30 {
        if let Ok(c) = Channel::connect(ctl::SERVICE) {
            return Ok(c);
        }
        sleep_ms(100);
    }
    Err(2)
}

/// Sends one request; (status, v0).
fn call(c: &Channel, msg: &[u8]) -> Result<(u32, u32), u32> {
    c.send(msg).map_err(|_| 3u32)?;
    let mut r = [0u8; chan::MSG_MAX];
    let n = c.recv(&mut r).map_err(|_| 4u32)?;
    if n < net::REPLY_SIZE {
        return Err(5);
    }
    let u = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
    Ok((u(0), u(4)))
}

fn simple(op: u8) -> Result<u32, u32> {
    let c = connect()?;
    match call(&c, &[op])? {
        (net::status::OK, v) => Ok(v),
        _ => Err(6),
    }
}

/// Sends `text` line by line and puts it in force.
fn load(text: &str) -> Result<u32, u32> {
    let c = connect()?;
    call(&c, &[ctl::BEGIN])?;
    let mut msg = [0u8; chan::MSG_MAX];
    for line in text.lines() {
        let n = line.len().min(chan::MSG_MAX - 1);
        msg[0] = ctl::LINE;
        msg[1..1 + n].copy_from_slice(&line.as_bytes()[..n]);
        call(&c, &msg[..1 + n])?;
    }
    match call(&c, &[ctl::COMMIT])? {
        (net::status::OK, rules) => Ok(rules),
        (_, line) => Err(100 + line),
    }
}

fn from_file() -> Result<u32, u32> {
    let mut files = Files::connect().map_err(|_| 10u32)?;
    let f = files.open(RULE_FILE, fs::flag::READ).map_err(|_| 11u32)?;
    let mut buf = [0u8; FILE_MAX];
    let mut n = 0;
    while n < buf.len() {
        let k = files.read(f, &mut buf[n..]).map_err(|_| 12u32)?;
        if k == 0 {
            break;
        }
        n += k;
    }
    let _ = files.close(f);
    drop(files);
    let text = core::str::from_utf8(&buf[..n]).map_err(|_| 13u32)?;
    load(text)
}
