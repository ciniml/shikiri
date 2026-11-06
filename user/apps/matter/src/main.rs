#![no_std]
#![no_main]

//! The Matter service (doc/49 §4, MT2): simple-matter's core on the Wi-Fi
//! link, as a device (`device`: an On/Off light on endpoint 1 with the
//! development passcode 20202021, discriminator 3840 and the test DAC, VID
//! 0xFFF1 and PID 0x8001) and as a controller (`controller`, on its own
//! UDP port), driven through `svc:matter` (`t5_abi::matter`: the shell's
//! `mt`). Matter's UDP 5540, the controller's 5541 and mDNS 5353
//! (ff02::fb and 224.0.0.251 joined) through `svc:net`; one store on the
//! SD card (`mtport::FileKvs`) for both; the kernel's `random`. While
//! the device is commissionable it also advertises over BLE through
//! `svc:ble` (`blelink`, MT4), if the `ble` service runs; the controller
//! also commissions over BLE (`pairble`).
//!
//! `run matter 1` is the emulator's test (doc/49 §16): no Wi-Fi, no SD card
//! and no BLE there, so the sockets are on any link (127.0.0.1 works), the
//! store is in memory and the MAC is made up. `run matter 2` is the real
//! thing with the store in memory (without the SD card; nothing is kept).
//!
//! Built with `--permit svc:net --permit svc:fs --permit random --permit
//! listen:matter --permit svc:ble`.

mod blelink;
mod controller;
mod watch;
mod device;
mod endpoints;
mod store;
mod wifilink;

use core::cell::RefCell;
use core::fmt::Write as _;
use core::mem::MaybeUninit;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use blelink::BleLink;
use controller::{ARGS, Controller, Op, Outcome, SUB_PATHS, Val};
use device::{Change, Device, Identity};
use endpoints::{FIRST, Kind, LABEL_MAX, OWNER_MAX, SLOTS};
use mtport::{FileKvs, KernelRng};
use store::{MemKvs, Store};
use wifilink::WifiLink;
use simple_matter::controller::{DEFAULT_WINDOW_ITERATIONS, OpenWindowParams};
use simple_matter::crypto::Rng as _;
use simple_matter::discovery::onboarding;
use simple_matter::transport::net::PeerAddr;
use t5_rt::net::{iface, link};
use t5_rt::log::FmtBuf;
use t5_rt::{Channel, Error, FsError, Handle, Listener, Net, chan, event, matter, sleep_ms, time_ns, wait};

t5_rt::entry!(main);

const MATTER_PORT: u16 = 5540;
const MDNS_PORT: u16 = 5353;
/// The controller's port (replies come back to it).
const CONTROLLER_PORT: u16 = 5541;
/// On/Off cluster and its commands.
const ONOFF: u32 = 0x0006;
const CMD_OFF: u32 = 0;
const CMD_ON: u32 = 1;
const CMD_TOGGLE: u32 = 2;
const MDNS_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const DISCRIMINATOR: u16 = 3840;
/// The test vendor and product (simple-matter's test DAC).
const VENDOR: u16 = 0xFFF1;
const PRODUCT: u16 = 0x8001;
/// The loop's longest sleep: it waits for `net` (UDP_WAIT, doc/49 §18),
/// the clients and `ble`, and the stacks' timers.
const IDLE_MS: u64 = 1000;
/// How often the addresses are read again.
const ADDRS_MS: u64 = 5000;
const PKT: usize = 1280;

fn now_ms() -> u64 {
    time_ns() / 1_000_000
}

/// The Wi-Fi link's MAC, IPv4 address and IPv6 link-local address.
struct Addrs {
    mac: [u8; 6],
    ipv4: Option<[u8; 4]>,
    ipv6_ll: Option<[u8; 16]>,
}

fn wifi_addrs(net: &Net) -> Option<Addrs> {
    for k in 0.. {
        let len = match net.if_info(k) {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(_) => return None,
        };
        let r = &net.slice[..len];
        if !r.starts_with(b"wlan0") || len < iface::HEADER {
            continue;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&r[8..14]);
        let v4 = u32::from_le_bytes([r[16], r[17], r[18], r[19]]);
        let ipv4 = (v4 != 0).then(|| Ipv4Addr::from_bits(v4).octets());
        let ipv6_ll = (0..r[15] as usize)
            .map(|i| &r[iface::HEADER + i * iface::V6_ENTRY..][..16])
            .find(|a| a[0] == 0xFE && a[1] & 0xC0 == 0x80)
            .map(|a| a.try_into().unwrap());
        return Some(Addrs { mac, ipv4, ipv6_ll });
    }
    None
}

fn main(mode: usize, _: usize) -> usize {
    match run(mode & 1 != 0, mode & 2 != 0) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "matter.failed", step = step);
            step as usize
        }
    }
}

/// The serial number: "T5-" and the MAC in hex.
static mut SERIAL: [u8; 15] = *b"T5-000000000000";

fn serial(mac: &[u8; 6]) -> &'static str {
    let s = unsafe { &mut *core::ptr::addr_of_mut!(SERIAL) };
    for (i, b) in mac.iter().enumerate() {
        s[3 + 2 * i] = b"0123456789ABCDEF"[(b >> 4) as usize];
        s[4 + 2 * i] = b"0123456789ABCDEF"[(b & 0xF) as usize];
    }
    core::str::from_utf8(s).unwrap_or("T5")
}

fn log_change(c: Change) {
    match c {
        Change::OnOff(on) => event!(Info, "matter.onoff", on = on),
        Change::Commissioned(n) => event!(Info, "matter.commissioned", fabrics = n as u32),
        Change::FabricRemoved(n) => event!(Info, "matter.fabric_removed", fabrics = n as u32),
        Change::Window(open) => event!(Info, "matter.window", open = open),
        Change::EpOnOff(ep, on) => event!(Info, "matter.ep_onoff", ep = ep as u32, on = on),
    }
}

/// An app's endpoint connection (doc/53).
struct EpSession {
    chan: Channel,
    ep: u16,
}

fn state(c: &Channel, text: &str) {
    let mut m = [0u8; 64];
    let n = text.len().min(m.len() - 1);
    m[0] = matter::STATE;
    m[1..1 + n].copy_from_slice(&text.as_bytes()[..n]);
    let _ = c.send(&m[..1 + n]);
}

fn tell_on(c: &Channel, on: bool) {
    state(c, if on { "on 1" } else { "on 0" });
}

/// `endpoint KIND [LABEL]`: the endpoint, or why not (answered).
fn ep_request(line: &str, c: &Channel, dev: &mut Device) -> Option<u16> {
    let mut w = line.split_ascii_whitespace().skip(1);
    let kind = w.next().and_then(Kind::parse);
    let label = w.next().unwrap_or("0");
    let r = match kind {
        None => Err("endpoint light|plug|contact|temp|humidity [LABEL]"),
        Some(_) if label.len() > LABEL_MAX || w.next().is_some() => Err("endpoint KIND [LABEL]: a label of 8 bytes at most"),
        Some(kind) => {
            let mut name = [0u8; t5_rt::t5a::MAX_NAME];
            let n = c.peer_name(&mut name);
            let mut owner = [0u8; OWNER_MAX];
            owner[..n].copy_from_slice(&name[..n]);
            owner[n] = b'/';
            owner[n + 1..n + 1 + label.len()].copy_from_slice(label.as_bytes());
            let owner = &owner[..n + 1 + label.len()];
            dev.ep_attach(owner, kind).inspect(|&ep| {
                event!(Info, "matter.ep_attached", ep = ep as u32, kind = kind.name(), owner = core::str::from_utf8(owner).unwrap_or("?"));
            })
        }
    };
    match r {
        Ok(ep) => {
            let mut t = FmtBuf::<40>::new();
            let _ = write!(t, "endpoint {ep} {}", kind.map_or("", Kind::name));
            reply(c, t.as_str());
            end(c, true);
            if let Some(on) = dev.ep(ep).and_then(|s| s.on()) {
                tell_on(c, on);
            }
            Some(ep)
        }
        Err(e) => {
            event!(Warn, "matter.ep_refused", why = e);
            reply(c, e);
            end(c, false);
            None
        }
    }
}

/// `set VALUE` from an endpoint's app.
fn ep_message(m: &[u8], ep: u16, dev: &mut Device) {
    let line = core::str::from_utf8(m).unwrap_or("");
    let mut w = line.split_ascii_whitespace();
    let r = match (w.next(), w.next()) {
        (Some("set"), Some("null")) => dev.ep_set(ep, None),
        (Some("set"), Some(v)) => v.parse::<i32>().map_err(|_| "set VALUE: decimal or null").and_then(|v| dev.ep_set(ep, Some(v))),
        _ => Err("set VALUE"),
    };
    if let Err(e) = r {
        event!(Warn, "matter.ep_bad", ep = ep as u32, why = e);
    }
}

/// The store, shared by the device and the controller (one `svc:fs`
/// connection: a process maps two shared windows, doc/19 §2).
static mut KVS: MaybeUninit<RefCell<Store>> = MaybeUninit::uninit();

/// The client being served (one request per connection, as the shell's
/// `mt` asks: dropped after the answer; a closed channel only shows as an
/// event to `wait`, so it is not waited for), and whether it waits for the
/// controller.
struct Client {
    chan: Channel,
    waiting: bool,
}

fn reply(c: &Channel, text: &str) {
    let mut m = [0u8; 128];
    let n = text.len().min(m.len() - 1);
    m[0] = matter::LINE;
    m[1..1 + n].copy_from_slice(&text.as_bytes()[..n]);
    let _ = c.send(&m[..1 + n]);
}

fn end(c: &Channel, ok: bool) {
    let _ = c.send(&[matter::END, ok as u8]);
}

fn num(s: Option<&str>) -> Option<u64> {
    let s = s?;
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// The grant a request needs (doc/52 §3.3): `read` (what the node and the
/// controller know), `control` (operating nodes), `admin` (commissioning,
/// windows, credentials).
fn grant_for(cmd: &str) -> &'static str {
    match cmd {
        "status" | "nodes" | "read" | "sub" | "subev" | "eps" | "values" | "help" | "" => "read",
        "on" | "off" | "toggle" | "invoke" | "write" | "watch" | "unwatch" => "control",
        "endpoint" => "endpoint",
        _ => "admin",
    }
}

/// One request; true if the answer comes later (from the controller).
fn request(line: &str, c: &Channel, dev: &mut Device, ctrl: &mut Controller, watches: &mut watch::Watches, ble: &mut BleLink, now: u64) -> bool {
    let mut w = line.split_ascii_whitespace();
    let cmd = w.next().unwrap_or("");
    let mut text = FmtBuf::<120>::new();
    let started = match cmd {
        "status" => {
            let _ = write!(text, "device: fabrics {}, on {}; controller: nodes {}", dev.fabrics(), dev.on(), ctrl.nodes().count());
            reply(c, text.as_str());
            end(c, true);
            return false;
        }
        "eps" => {
            for ep in FIRST..FIRST + SLOTS as u16 {
                if let Some(s) = dev.ep(ep) {
                    let mut t = FmtBuf::<120>::new();
                    let _ = write!(t, "endpoint {ep} {} of {}: ", s.kind.name(), core::str::from_utf8(s.owner()).unwrap_or("?"));
                    let _ = match s.value() {
                        Some(v) => write!(t, "{v}"),
                        None => write!(t, "null"),
                    };
                    reply(c, t.as_str());
                }
            }
            end(c, true);
            return false;
        }
        "epdel" => {
            let r = num(w.next()).ok_or("epdel N").and_then(|ep| dev.ep_remove(ep as u16).inspect(|_| event!(Info, "matter.ep_removed", ep = ep as u32)));
            match r {
                Ok(()) => {
                    reply(c, "removed");
                    end(c, true);
                }
                Err(e) => {
                    reply(c, e);
                    end(c, false);
                }
            }
            return false;
        }
        "nodes" => {
            for (id, addr, live) in ctrl.nodes() {
                let mut t = FmtBuf::<120>::new();
                let _ = write!(t, "node {id} at {addr}{}", if live { " (session)" } else { "" });
                reply(c, t.as_str());
            }
            end(c, true);
            return false;
        }
        "pair" => match (num(w.next()), num(w.next()), num(w.next())) {
            (Some(node), Some(passcode), Some(d)) => ctrl.pair(node, passcode as u32, controller::Disc::Long(d as u16), now),
            _ => Err("pair NODE PASSCODE DISCRIMINATOR"),
        },
        // A manual pairing code (as an ecosystem shows for an open window),
        // dashes allowed.
        "paircode" => match (num(w.next()), w.next().and_then(manual_code)) {
            (Some(node), Some((passcode, short))) => ctrl.pair(node, passcode, controller::Disc::Short(short), now),
            (Some(_), None) => Err("not a valid 11-digit pairing code"),
            _ => Err("paircode NODE CODE"),
        },
        // PASSCODE may be a manual pairing code (more than 8 digits).
        "relocate" => match num(w.next()) {
            Some(node) => ctrl.relocate(node, now),
            None => Err("relocate NODE"),
        },
        "pairaddr" => match (w.next(), w.next(), w.next().and_then(|a| a.parse::<IpAddr>().ok()), num(w.next())) {
            (Some(node), Some(code), Some(ip), Some(port))
                if let (Some(node), Some(passcode)) = (num(Some(node)), if code.len() > 8 { manual_code(code).map(|c| c.0 as u64) } else { num(Some(code)) }) =>
            {
                ctrl.pair_addr(node, passcode as u32, SocketAddr::new(ip, port as u16), now)
            }
            _ => Err("pairaddr NODE PASSCODE IP PORT"),
        },
        // The passphrase is not logged (nor is the request).
        "netcred" => {
            let r = match (w.next(), w.next()) {
                (Some(ssid), Some(psk)) => ctrl.set_wifi(Some((ssid.as_bytes(), psk.as_bytes()))).map(|_| "set: given to devices commissioned over BLE"),
                (None, _) => ctrl.set_wifi(None).map(|_| "cleared"),
                _ => Err("netcred [SSID PASSPHRASE]"),
            };
            match r {
                Ok(t) => {
                    reply(c, t);
                    end(c, true);
                }
                Err(e) => {
                    reply(c, e);
                    end(c, false);
                }
            }
            return false;
        }
        "pairble" => match (num(w.next()), num(w.next()), num(w.next())) {
            (Some(node), Some(passcode), Some(d)) if !ctrl.busy() => {
                ble.central_start(d as u16, now).and_then(|_| ctrl.pair_ble(node, passcode as u32, now))
            }
            (Some(_), Some(_), Some(_)) => Err("busy"),
            _ => Err("pairble NODE PASSCODE DISCRIMINATOR"),
        },
        // Attributes kept subscribed, and their last values (a panel's).
        "watch" | "unwatch" => {
            let r = match (num(w.next()), num(w.next()), num(w.next()), num(w.next())) {
                (Some(node), Some(ep), Some(cl), Some(attr)) if cmd == "watch" => watches.add(node, ep as u16, cl as u32, attr as u32),
                (Some(node), Some(ep), Some(cl), Some(attr)) => watches.remove(node, ep as u16, cl as u32, attr as u32),
                _ => Err("watch|unwatch NODE EP CLUSTER ATTR"),
            };
            reply(c, r.err().unwrap_or("ok"));
            end(c, r.is_ok());
            return false;
        }
        // A line per entry: NODE EP CLUSTER ATTR, `+` if subscribed (else
        // `-`), seconds since its last report (or `-`), the value (or `-`).
        "values" => {
            for e in watches.iter() {
                let mut t = FmtBuf::<120>::new();
                let _ = write!(t, "{} {} {} {} {} ", e.node, e.ep, e.cluster, e.attr, if e.sub.is_some() { '+' } else { '-' });
                let _ = match (&e.value, e.at) {
                    (Some(v), at) => write!(t, "{} {}", now.saturating_sub(at) / 1000, v.as_str()),
                    (None, _) => write!(t, "- -"),
                };
                reply(c, t.as_str());
            }
            end(c, true);
            return false;
        }
        "forget" => {
            match num(w.next()).ok_or("forget NODE").and_then(|node| ctrl.forget(node).map(|_| watches.forget(node))) {
                Ok(()) => {
                    reply(c, "forgotten");
                    end(c, true);
                }
                Err(e) => {
                    reply(c, e);
                    end(c, false);
                }
            }
            return false;
        }
        "on" | "off" | "toggle" => match num(w.next()) {
            Some(node) => {
                let cmd = match cmd {
                    "on" => CMD_ON,
                    "off" => CMD_OFF,
                    _ => CMD_TOGGLE,
                };
                ctrl.start(node, Op::Invoke { ep: 1, cluster: ONOFF, cmd, args: [Val::Null; ARGS], n: 0 }, now)
            }
            None => Err("on|off|toggle NODE"),
        },
        "read" => match (num(w.next()), num(w.next()), num(w.next()), num(w.next())) {
            (Some(node), Some(ep), Some(cl), Some(attr)) => ctrl.start(node, Op::Read { ep: ep as u16, cluster: cl as u32, attr: attr as u32 }, now),
            _ => Err("read NODE EP CLUSTER ATTR"),
        },
        "invoke" => match (num(w.next()), num(w.next()), num(w.next()), num(w.next())) {
            (Some(node), Some(ep), Some(cl), Some(cmd)) => {
                let mut args = [Val::Null; ARGS];
                let mut n = 0;
                let mut bad = false;
                for a in w.by_ref() {
                    match Val::parse(a) {
                        Some(v) if n < ARGS => {
                            args[n] = v;
                            n += 1;
                        }
                        _ => bad = true,
                    }
                }
                if bad {
                    Err("arguments: numbers, -N, true, false, null; 6 at most")
                } else {
                    ctrl.start(node, Op::Invoke { ep: ep as u16, cluster: cl as u32, cmd: cmd as u32, args, n }, now)
                }
            }
            _ => Err("invoke NODE EP CLUSTER CMD [ARG...]"),
        },
        "write" => match (num(w.next()), num(w.next()), num(w.next()), num(w.next()), w.next().and_then(Val::parse)) {
            (Some(node), Some(ep), Some(cl), Some(attr), Some(val)) => {
                ctrl.start(node, Op::Write { ep: ep as u16, cluster: cl as u32, attr: attr as u32, val }, now)
            }
            _ => Err("write NODE EP CLUSTER ATTR VALUE"),
        },
        "sub" => match (num(w.next()), num(w.next()), num(w.next()), num(w.next())) {
            (Some(node), Some(ep), Some(cl), Some(attr)) => {
                let min_s = num(w.next()).unwrap_or(0).min(3600) as u16;
                let max_s = num(w.next()).unwrap_or(60).clamp(min_s.max(1) as u64, 3600) as u16;
                ctrl.start(node, Op::Subscribe { paths: [(ep as u16, cl as u32, attr as u32); SUB_PATHS], n: 1, min_s, max_s }, now)
            }
            _ => Err("sub NODE EP CLUSTER ATTR [MIN_S [MAX_S]]"),
        },
        "subev" => match (num(w.next()), num(w.next()), num(w.next()), num(w.next())) {
            (Some(node), Some(ep), Some(cl), Some(ev)) => {
                let min_s = num(w.next()).unwrap_or(0).min(3600) as u16;
                let max_s = num(w.next()).unwrap_or(60).clamp(min_s.max(1) as u64, 3600) as u16;
                ctrl.start(node, Op::SubscribeEvent { ep: ep as u16, cluster: cl as u32, event: ev as u32, min_s, max_s }, now)
            }
            _ => Err("subev NODE EP CLUSTER EVENT [MIN_S [MAX_S]]"),
        },
        "ocw" => match num(w.next()) {
            Some(node) => {
                let secs = num(w.next()).unwrap_or(300).clamp(180, 900) as u16;
                match window_params(secs) {
                    Ok(p) => {
                        let code = onboarding::manual_pairing_code(p.discriminator, p.passcode);
                        let _ = write!(
                            text,
                            "passcode {} discriminator {} code {} for {secs} s",
                            p.passcode,
                            p.discriminator,
                            core::str::from_utf8(&code).unwrap_or("?")
                        );
                        ctrl.start(node, Op::OpenWindow(p), now).inspect(|_| reply(c, text.as_str()))
                    }
                    Err(e) => Err(e),
                }
            }
            None => Err("ocw NODE [SECONDS]"),
        },
        "revoke" => match num(w.next()) {
            Some(node) => ctrl.start(node, Op::Revoke, now),
            None => Err("revoke NODE"),
        },
        _ => Err("status | nodes | pair NODE PASSCODE DISCRIMINATOR | paircode NODE CODE | pairaddr NODE PASSCODE IP PORT | pairble NODE PASSCODE DISCRIMINATOR | netcred [SSID PASSPHRASE] | forget NODE | on|off|toggle NODE | read NODE EP CLUSTER ATTR | invoke NODE EP CLUSTER CMD [ARG...] | write NODE EP CLUSTER ATTR VALUE | sub NODE EP CLUSTER ATTR [MIN_S [MAX_S]] | subev NODE EP CLUSTER EVENT [MIN_S [MAX_S]] | watch NODE EP CLUSTER ATTR | unwatch NODE EP CLUSTER ATTR | values | relocate NODE | ocw NODE [SECONDS] | revoke NODE | eps | epdel N | endpoint KIND [LABEL]"),
    };
    match started {
        Ok(()) => true,
        Err(e) => {
            // The usage: a line per request.
            for l in e.split(" | ") {
                reply(c, l);
            }
            end(c, false);
            false
        }
    }
}

/// The passcode and the short discriminator (its top 4 bits) of an
/// 11-digit manual pairing code (Core spec §5.1.4.1), or `None` if it is
/// not one or its check digit is wrong. 10 digits are taken as the code
/// without its check digit (`mtreq` can pass only numbers that fit 32 bits).
fn manual_code(s: &str) -> Option<(u32, u8)> {
    let mut d = [0u8; 11];
    let mut n = 0;
    for b in s.bytes().filter(|&b| b != b'-' && b != b' ') {
        if !b.is_ascii_digit() || n == d.len() {
            return None;
        }
        d[n] = b;
        n += 1;
    }
    // A leading digit above 3 marks the 21-digit form (vendor and product).
    let checked = n == d.len();
    if !(checked || n == d.len() - 1) || d[0] > b'3' {
        return None;
    }
    let val = |r: core::ops::Range<usize>| d[r].iter().fold(0u32, |v, &b| v * 10 + (b - b'0') as u32);
    let (c1, c2, c3) = (val(0..1), val(1..6), val(6..10));
    let short = ((c1 << 2) | (c2 >> 14)) as u8;
    let passcode = (c2 & 0x3FFF) | (c3 << 14);
    // Encoding it again gives the same digits, check digit included.
    let again = onboarding::manual_pairing_code((short as u16) << 8, passcode);
    (if checked { again == d } else { again[..10] == d[..10] }).then_some((passcode, short))
}

/// A fresh passcode, discriminator and salt for a window on another node.
fn window_params(timeout_s: u16) -> Result<OpenWindowParams, &'static str> {
    let mut rng = KernelRng;
    let mut salt = [0u8; 16];
    rng.fill_bytes(&mut salt).map_err(|_| "random")?;
    Ok(OpenWindowParams {
        timeout_s,
        discriminator: onboarding::random_discriminator(&mut rng).map_err(|_| "random")?,
        passcode: onboarding::random_passcode(&mut rng).map_err(|_| "random")?,
        salt,
        iterations: DEFAULT_WINDOW_ITERATIONS,
    })
}

fn outcome(c: &Channel, o: Outcome) {
    let mut t = FmtBuf::<120>::new();
    let ok = match o {
        Outcome::Commissioned { node, ms } => {
            let _ = write!(t, "commissioned node {node} in {ms} ms");
            true
        }
        Outcome::PairFailed { stage, reason } => {
            let _ = write!(t, "commissioning failed: {reason} (stage {stage})");
            false
        }
        Outcome::NotFound => {
            let _ = write!(t, "no device with that discriminator");
            false
        }
        Outcome::Invoked { status } => {
            let _ = write!(t, "status 0x{status:02x}");
            status == 0
        }
        Outcome::Read { value, null } => {
            if null {
                let _ = write!(t, "null");
            } else {
                let _ = write!(t, "{value} (0x{value:x})");
            }
            true
        }
        Outcome::ReadText(v) => {
            let _ = write!(t, "{}", v.as_str());
            true
        }
        Outcome::Subscribed { id, max_s } => {
            let _ = write!(t, "subscribed: id {id}, reports at least every {max_s} s (matter.report, matter.event)");
            true
        }
        Outcome::Failed { why, status } => {
            let _ = write!(t, "failed: {why} (status 0x{status:02x})");
            false
        }
    };
    event!(Info, "matter.request_done", ok = ok, text = t.as_str());
    reply(c, t.as_str());
    end(c, ok);
}

/// `test`: t5emu's loopback (doc/49 §16). `memory`: the store in memory
/// only (the real network; nothing is kept), for when the SD card is not
/// there.
fn run(test: bool, memory: bool) -> Result<(), u32> {
    // `net` may be starting too (at boot, or after it was restarted).
    let mut tries = 0;
    let mut net = loop {
        match Net::connect() {
            Ok(n) => break n,
            Err(_) if tries < 50 => {
                tries += 1;
                sleep_ms(200);
            }
            Err(_) => return Err(1),
        }
    };
    // The Wi-Fi link's MAC is known once the link is up.
    let addrs = if test {
        Addrs { mac: [0x02, 0, 0, 0, 0, 0x01], ipv4: Some([127, 0, 0, 1]), ipv6_ll: None }
    } else {
        loop {
            match wifi_addrs(&net) {
                Some(a) if a.mac != [0; 6] => break a,
                _ => sleep_ms(1000),
            }
        }
    };
    let kvs = if test || memory {
        Store::Mem(MemKvs::new())
    } else {
        Store::File(FileKvs::open().map_err(|e| {
            let (status, ipc) = match e {
                FsError::Status(s) => (s, 0),
                FsError::Ipc(e) => (0, e as u32),
            };
            event!(Error, "matter.no_store", status = status, ipc = ipc);
            2u32
        })?)
    };
    let kvs: &'static RefCell<Store> = unsafe { (*core::ptr::addr_of_mut!(KVS)).write(RefCell::new(kvs)) };
    let id = Identity {
        mac: addrs.mac,
        discriminator: DISCRIMINATOR,
        vendor_id: VENDOR,
        product_id: PRODUCT,
        product_name: "Tab5",
        serial_number: serial(&addrs.mac),
    };
    let dev = Device::init(id, kvs, now_ms()).map_err(|e| {
        event!(Error, "matter.init_failed", error = e);
        3u32
    })?;
    dev.set_addrs(addrs.ipv4, addrs.ipv6_ll, now_ms());
    let ctrl = Controller::init(kvs).map_err(|e| {
        event!(Error, "matter.init_failed", error = e);
        6u32
    })?;

    let on = if test { link::ANY } else { link::WLAN0 };
    let udp = net.udp_bind_at(0, on, MATTER_PORT).map_err(|_| 4u32)?;
    let mdns = net.udp_bind_at(0, on, MDNS_PORT).map_err(|_| 5u32)?;
    let cudp = net.udp_bind_at(0, on, CONTROLLER_PORT).map_err(|_| 7u32)?;
    for group in [IpAddr::V6(MDNS_V6), IpAddr::V4(MDNS_V4)] {
        if net.udp_join(mdns, group).is_err() {
            event!(Warn, "matter.no_multicast", v6 = group.is_ipv6());
        }
    }
    let listener = Listener::open(matter::SVC).map_err(|_| 8u32)?;
    event!(Info, "matter.ready", fabrics = dev.fabrics() as u32, discriminator = DISCRIMINATOR as u32, mac = &addrs.mac[..], on = dev.on(), test = test);

    let mut watches = watch::Watches::load(kvs);
    let mut client: Option<Client> = None;
    let mut eps: [Option<EpSession>; SLOTS] = Default::default();
    let mut ble = BleLink::new();
    let mut wifi = WifiLink::new();
    let mut rx = [0u8; PKT];
    let mut tx = [0u8; PKT];
    let mut next_addrs = now_ms() + ADDRS_MS;
    // After a join, the addresses are read every 500 ms until then.
    let mut fast_addrs_until = 0;
    let mut has_v4 = addrs.ipv4.is_some();
    let mdns_group = |v4: bool| if v4 { SocketAddr::new(IpAddr::V4(MDNS_V4), MDNS_PORT) } else { SocketAddr::new(IpAddr::V6(MDNS_V6), MDNS_PORT) };
    let send = |net: &mut Net, sock: u16, tx: &[u8], to: SocketAddr| {
        if net.udp_send_ip(sock, to.ip(), to.port(), tx).is_err() {
            event!(Debug, "matter.send_failed", port = to.port() as u32, len = tx.len() as u32);
        }
    };
    loop {
        let mut busy = false;
        // A client of `svc:matter`, and its request.
        if client.is_none()
            && let Ok(c) = listener.accept()
        {
            client = Some(Client { chan: c, waiting: false });
        }
        if let Some(cl) = client.as_mut() {
            let mut m = [0u8; 128];
            match cl.chan.0.read(&mut m) {
                Ok(n) if !cl.waiting => {
                    busy = true;
                    let line = core::str::from_utf8(&m[..n]).unwrap_or("");
                    let need = grant_for(line.split_ascii_whitespace().next().unwrap_or(""));
                    if !cl.chan.peer_grants().has(need.as_bytes()) {
                        event!(Warn, "matter.denied", need = need);
                        reply(&cl.chan, "denied: this app may not ask that");
                        end(&cl.chan, false);
                    } else if need == "endpoint" {
                        // The connection becomes the endpoint's (a newer
                        // one for the same endpoint replaces it).
                        if let Some(ep) = ep_request(line, &cl.chan, dev) {
                            let chan = client.take().map(|c| c.chan);
                            eps[(ep - FIRST) as usize] = chan.map(|chan| EpSession { chan, ep });
                        }
                    } else {
                        cl.waiting = request(line, &cl.chan, dev, ctrl, &mut watches, &mut ble, now_ms());
                    }
                    if !client.as_ref().is_some_and(|c| c.waiting) {
                        client = None;
                    }
                }
                Ok(_) | Err(Error::Busy) => {}
                // The client left.
                Err(_) => client = None,
            }
        }
        // The apps' endpoints: their values.
        for slot in eps.iter_mut() {
            let Some(s) = slot.as_ref() else { continue };
            let mut m = [0u8; 64];
            loop {
                match s.chan.0.read(&mut m) {
                    Ok(n) => {
                        busy = true;
                        ep_message(&m[..n], s.ep, dev);
                    }
                    Err(Error::Busy) => break,
                    Err(_) => {
                        event!(Info, "matter.ep_detached", ep = s.ep as u32);
                        *slot = None;
                        break;
                    }
                }
            }
        }
        // Matter's datagrams: the device's and the controller's sockets.
        while let Ok((n, from, port)) = net.udp_recv_ip(udp, &mut rx, 0) {
            busy = true;
            let peer = PeerAddr::Udp(SocketAddr::new(from, port));
            let reply = dev.handle_rx(&mut rx[..n], peer, now_ms(), &mut tx);
            if let Some(d) = reply
                && let PeerAddr::Udp(to) = d.addr
            {
                send(&mut net, udp, &tx[..d.len], to);
            }
        }
        while let Ok((n, from, port)) = net.udp_recv_ip(cudp, &mut rx, 0) {
            busy = true;
            ctrl.handle_rx(&mut rx[..n], SocketAddr::new(from, port), now_ms());
        }
        // mDNS: queries for the device, answers for the controller's search.
        while let Ok((n, from, port)) = net.udp_recv_ip(mdns, &mut rx, 0) {
            busy = true;
            ctrl.mdns_rx(&rx[..n], now_ms());
            if let Some((k, unicast)) = dev.mdns_rx(&rx[..n], &mut tx) {
                let to = if unicast { SocketAddr::new(from, port) } else { mdns_group(from.is_ipv4()) };
                send(&mut net, mdns, &tx[..k], to);
            }
        }
        // BLE: a commissioner's messages over BTP.
        busy |= ble.rx(dev, ctrl, now_ms(), &mut tx);
        let now = now_ms();
        dev.housekeep(now, |c| {
            if let Change::EpOnOff(ep, on) = c
                && let Some(s) = eps.iter().flatten().find(|s| s.ep == ep)
            {
                tell_on(&s.chan, on);
            }
            log_change(c);
        });
        // Retransmissions, acknowledgements, reports; announcements.
        while let Some(d) = dev.poll(now_ms(), &mut tx) {
            match d.addr {
                PeerAddr::Udp(to) => send(&mut net, udp, &tx[..d.len], to),
                PeerAddr::Ble(_) => ble.queue(&tx[..d.len]),
            }
        }
        ble.advertise(if test { None } else { dev.ble_adv() }, now_ms());
        // Wi-Fi: a commissioner's ConnectNetwork, `wifi`'s answers.
        if !test {
            // A commissioner's network is kept once commissioning is
            // complete, left if the fail-safe ends first (doc/50 §5).
            match dev.take_commissioning() {
                Some(true) => wifi.save(),
                Some(false) => wifi.revert(),
                None => {}
            }
            busy |= wifi.pump(dev, now_ms());
            // After a join the old address goes at once (DHCP has started
            // over); the new one comes within `ADDRS_MS`.
            if wifi.take_joined() {
                next_addrs = now_ms();
                fast_addrs_until = now_ms() + 10_000;
            }
        }
        // Announcements and queries go to both groups (doc/49 §22): Matter
        // finds nodes over IPv6 (the reference SDK's minimal mDNS may not
        // answer IPv4), and IPv4 where there is an address.
        while let Some(k) = dev.mdns_poll(now_ms(), &mut tx) {
            send(&mut net, mdns, &tx[..k], mdns_group(false));
            if has_v4 {
                send(&mut net, mdns, &tx[..k], mdns_group(true));
            }
        }
        ctrl.poll(now_ms());
        while let Some(t) = ctrl.take_tx() {
            match t.to {
                Some(to) => send(&mut net, cudp, &t.buf[..t.len], to),
                // A discovery query: to the group, from the mDNS socket.
                None => {
                    send(&mut net, mdns, &t.buf[..t.len], mdns_group(false));
                    if has_v4 {
                        send(&mut net, mdns, &t.buf[..t.len], mdns_group(true));
                    }
                }
            }
            ctrl.poll(now_ms());
        }
        // BLE: the controller's messages for the device; the link's end.
        while let Some(t) = ctrl.take_ble_tx() {
            ble.queue(&t.buf[..t.len]);
        }
        if ctrl.take_ble_close() {
            ble.central_stop(now_ms());
        }
        while ble.pump(ctrl, now_ms()) {
            busy = true;
        }
        if let Some(o) = ctrl.take_outcome() {
            match client.as_mut() {
                Some(cl) if cl.waiting => {
                    outcome(&cl.chan, o);
                    client = None;
                }
                _ if watches.waiting() => watches.outcome(o, now_ms()),
                _ => {}
            }
        }
        while let Some(r) = ctrl.take_report() {
            watches.report(&r, now_ms());
        }
        while let Some((node, id)) = ctrl.take_lost() {
            watches.lost(node, id, now_ms());
        }
        // The watched attributes: subscribe while no one else needs the
        // controller.
        if !ctrl.busy()
            && !client.as_ref().is_some_and(|c| c.waiting)
            && let Some((node, op)) = watches.due(now_ms())
            && ctrl.start(node, op, now_ms()).is_err()
        {
            watches.not_started(now_ms());
        }
        let now = now_ms();
        if now >= next_addrs && !test {
            next_addrs = now + if now < fast_addrs_until { 500 } else { ADDRS_MS };
            if let Some(a) = wifi_addrs(&net) {
                has_v4 = a.ipv4.is_some();
                dev.set_addrs(a.ipv4, a.ipv6_ll, now);
            }
        }
        if !busy {
            let deadline = dev
                .next_deadline(now)
                .unwrap_or(u64::MAX)
                .min(ctrl.next_deadline(now).unwrap_or(u64::MAX))
                .min(watches.next_deadline().unwrap_or(u64::MAX))
                .min(ble.next_deadline().unwrap_or(u64::MAX));
            let deadline = if test { deadline } else { deadline.min(next_addrs) };
            let ms = deadline.saturating_sub(now).clamp(1, IDLE_MS);
            if net.udp_arm().is_err() {
                sleep_ms(ms);
                continue;
            }
            // `net` (a datagram), a new client or its request, `ble`.
            let mut hs: [&Handle; 5 + SLOTS] = [net.handle(); 5 + SLOTS];
            let mut n = 1;
            hs[n] = &listener.0;
            n += 1;
            let client_at = match client.as_ref() {
                Some(cl) if !cl.waiting => {
                    hs[n] = &cl.chan.0;
                    n += 1;
                    n - 1
                }
                _ => usize::MAX,
            };
            let ble_at = match ble.handle() {
                Some(h) => {
                    hs[n] = h;
                    n += 1;
                    n - 1
                }
                None => usize::MAX,
            };
            let wifi_at = match wifi.handle() {
                Some(h) => {
                    hs[n] = h;
                    n += 1;
                    n - 1
                }
                None => usize::MAX,
            };
            // The apps' endpoints: a value, or the app left.
            let eps_at = n;
            for s in eps.iter().flatten() {
                hs[n] = &s.chan.0;
                n += 1;
            }
            if let Ok(ev) = wait(&hs[..n], ms * 1_000_000)
                && ev.kind == chan::PEER_CLOSED
            {
                if ev.index == client_at {
                    client = None;
                } else if ev.index == ble_at {
                    ble.closed(now);
                } else if ev.index == wifi_at {
                    wifi.closed(now);
                } else if ev.index >= eps_at
                    && let Some(slot) = eps.iter_mut().filter(|s| s.is_some()).nth(ev.index - eps_at)
                {
                    event!(Info, "matter.ep_detached", ep = slot.as_ref().map_or(0, |s| s.ep) as u32);
                    *slot = None;
                }
            }
        }
    }
}
