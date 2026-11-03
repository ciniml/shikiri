#![no_std]
#![no_main]

//! The Matter service (doc/49 §5): simple-matter's core (`device`) on the
//! Wi-Fi link: Matter's UDP 5540 and mDNS 5353 (ff02::fb and 224.0.0.251
//! joined) through `svc:net`, the store on the SD card (`mtport::FileKvs`),
//! the kernel's `random`. For now an On/Off light on endpoint 1 with the
//! development passcode 20202021, discriminator 3840 and the test DAC
//! (VID 0xFFF1, PID 0x8001); its changes are logged.
//!
//! Built with `--permit svc:net --permit svc:fs --permit random`.

mod device;

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use device::{Change, Device, Identity};
use mtport::FileKvs;
use simple_matter::transport::net::PeerAddr;
use t5_rt::net::{iface, link};
use t5_rt::{FsError, Net, event, sleep_ms, time_ns};

t5_rt::entry!(main);

const MATTER_PORT: u16 = 5540;
const MDNS_PORT: u16 = 5353;
const MDNS_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const DISCRIMINATOR: u16 = 3840;
/// The test vendor and product (simple-matter's test DAC).
const VENDOR: u16 = 0xFFF1;
const PRODUCT: u16 = 0x8001;
/// The loop's longest sleep: UDP has no readiness event yet (doc/49).
const TICK_MS: u64 = 20;
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

fn main(_: usize, _: usize) -> usize {
    match run() {
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
    }
}

// `PeerAddr` has only UDP without the core's `ble` feature (MT4 adds BLE).
#[allow(irrefutable_let_patterns)]
fn run() -> Result<(), u32> {
    let mut net = Net::connect().map_err(|_| 1u32)?;
    // The Wi-Fi link's MAC is known once the link is up.
    let addrs = loop {
        match wifi_addrs(&net) {
            Some(a) if a.mac != [0; 6] => break a,
            _ => sleep_ms(1000),
        }
    };
    let kvs = FileKvs::open().map_err(|e| {
        let (status, ipc) = match e {
            FsError::Status(s) => (s, 0),
            FsError::Ipc(e) => (0, e as u32),
        };
        event!(Error, "matter.no_store", status = status, ipc = ipc);
        2u32
    })?;
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

    let udp = net.udp_bind_at(0, link::WLAN0, MATTER_PORT).map_err(|_| 4u32)?;
    let mdns = net.udp_bind_at(0, link::WLAN0, MDNS_PORT).map_err(|_| 5u32)?;
    for group in [IpAddr::V6(MDNS_V6), IpAddr::V4(MDNS_V4)] {
        if net.udp_join(mdns, group).is_err() {
            event!(Warn, "matter.no_multicast", v6 = group.is_ipv6());
        }
    }
    event!(Info, "matter.ready", fabrics = dev.fabrics() as u32, discriminator = DISCRIMINATOR as u32, mac = &addrs.mac[..], on = dev.on());

    let mut rx = [0u8; PKT];
    let mut tx = [0u8; PKT];
    let mut next_addrs = now_ms() + ADDRS_MS;
    let mut has_v4 = addrs.ipv4.is_some();
    let send = |net: &mut Net, sock: u16, tx: &[u8], to: SocketAddr| {
        if net.udp_send_ip(sock, to.ip(), to.port(), tx).is_err() {
            event!(Debug, "matter.send_failed", port = to.port() as u32, len = tx.len() as u32);
        }
    };
    loop {
        let mut busy = false;
        // Matter's datagrams.
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
        // mDNS queries: answered to the sender (a QU question) or the group.
        while let Ok((n, from, port)) = net.udp_recv_ip(mdns, &mut rx, 0) {
            busy = true;
            if let Some((k, unicast)) = dev.mdns_rx(&rx[..n], &mut tx) {
                let to = match (unicast, from) {
                    (true, _) => SocketAddr::new(from, port),
                    (false, IpAddr::V6(_)) => SocketAddr::new(IpAddr::V6(MDNS_V6), MDNS_PORT),
                    (false, IpAddr::V4(_)) => SocketAddr::new(IpAddr::V4(MDNS_V4), MDNS_PORT),
                };
                send(&mut net, mdns, &tx[..k], to);
            }
        }
        let now = now_ms();
        dev.housekeep(now, log_change);
        // Retransmissions, acknowledgements, reports; announcements.
        while let Some(d) = dev.poll(now_ms(), &mut tx) {
            if let PeerAddr::Udp(to) = d.addr {
                send(&mut net, udp, &tx[..d.len], to);
            }
        }
        while let Some(k) = dev.mdns_poll(now_ms(), &mut tx) {
            let to = if has_v4 { SocketAddr::new(IpAddr::V4(MDNS_V4), MDNS_PORT) } else { SocketAddr::new(IpAddr::V6(MDNS_V6), MDNS_PORT) };
            send(&mut net, mdns, &tx[..k], to);
        }
        let now = now_ms();
        if now >= next_addrs {
            next_addrs = now + ADDRS_MS;
            if let Some(a) = wifi_addrs(&net) {
                has_v4 = a.ipv4.is_some();
                dev.set_addrs(a.ipv4, a.ipv6_ll, now);
            }
        }
        if !busy {
            let deadline = dev.next_deadline(now).unwrap_or(u64::MAX);
            sleep_ms(deadline.saturating_sub(now).clamp(1, TICK_MS));
        }
    }
}
