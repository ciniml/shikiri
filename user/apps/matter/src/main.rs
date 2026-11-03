#![no_std]
#![no_main]

//! The Matter service (doc/49 MT1): simple-matter's device stack (through
//! `simple-matter-cffi`'s sans-IO functions, called from Rust) on the
//! Wi-Fi link: Matter's UDP 5540 and mDNS 5353 (ff02::fb and 224.0.0.251
//! joined) through `svc:net`, the store on the SD card (`mtport::FileKvs`),
//! the kernel's `random`. For now an On/Off light on endpoint 1 with the
//! development passcode 20202021, discriminator 3840 and the test DAC
//! (VID 0xFFF1, PID 0x8001); its changes are logged.
//!
//! Built with `--permit svc:net --permit svc:fs --permit random`.

use core::ffi::{c_char, c_void};
use core::ptr::null;

use mtport::{FileKvs, KernelRng};
use simple_matter::crypto::Rng;
use simple_matter::kvs::Kvs;
use simple_matter_cffi::*;
use t5_rt::net::{iface, link};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use t5_rt::{Net, event, sleep_ms, time_ns};

t5_rt::entry!(main);

const MATTER_PORT: u16 = 5540;
const MDNS_PORT: u16 = 5353;
const DISCRIMINATOR: u16 = 3840;
const PASSCODE: u32 = 20202021;
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

unsafe extern "C" fn rng_fill(_: *mut c_void, buf: *mut u8, len: usize) {
    let _ = KernelRng.fill_bytes(unsafe { core::slice::from_raw_parts_mut(buf, len) });
}

fn key<'a>(k: *const c_char) -> &'a [u8] {
    unsafe { core::ffi::CStr::from_ptr(k) }.to_bytes()
}

unsafe extern "C" fn kvs_get(ctx: *mut c_void, k: *const c_char, buf: *mut u8, cap: usize) -> i32 {
    let kvs = unsafe { &mut *(ctx as *mut FileKvs) };
    match kvs.get(key(k), unsafe { core::slice::from_raw_parts_mut(buf, cap) }) {
        Ok(Some(n)) => n as i32,
        Ok(None) => -1,
        Err(_) => -2,
    }
}

unsafe extern "C" fn kvs_set(ctx: *mut c_void, k: *const c_char, val: *const u8, len: usize) -> i32 {
    let kvs = unsafe { &mut *(ctx as *mut FileKvs) };
    match kvs.set(key(k), unsafe { core::slice::from_raw_parts(val, len) }) {
        Ok(()) => 0,
        Err(_) => -2,
    }
}

unsafe extern "C" fn kvs_delete(ctx: *mut c_void, k: *const c_char) -> i32 {
    let kvs = unsafe { &mut *(ctx as *mut FileKvs) };
    match kvs.remove(key(k)) {
        Ok(()) => 0,
        Err(_) => -2,
    }
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

fn to_ip(a: &sm_addr_t) -> IpAddr {
    if a.is_v6 {
        IpAddr::V6(Ipv6Addr::from(a.ip))
    } else {
        IpAddr::V4(Ipv4Addr::new(a.ip[0], a.ip[1], a.ip[2], a.ip[3]))
    }
}

fn from_ip(ip: IpAddr, port: u16) -> sm_addr_t {
    let mut a = sm_addr_t { ip: [0; 16], is_v6: false, port, scope_id: 0 };
    match ip {
        IpAddr::V4(v4) => a.ip[..4].copy_from_slice(&v4.octets()),
        IpAddr::V6(v6) => {
            a.ip = v6.octets();
            a.is_v6 = true;
        }
    }
    a
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

fn run() -> Result<(), u32> {
    let mut net = Net::connect().map_err(|_| 1u32)?;
    // The Wi-Fi link's MAC is known once the link is up.
    let addrs = loop {
        match wifi_addrs(&net) {
            Some(a) if a.mac != [0; 6] => break a,
            _ => sleep_ms(1000),
        }
    };
    let mut kvs = match FileKvs::open() {
        Ok(k) => k,
        Err(e) => {
            let (status, ipc) = match e {
                t5_rt::FsError::Status(s) => (s, 0),
                t5_rt::FsError::Ipc(e) => (0, e as u32),
            };
            event!(Error, "matter.no_store", status = status, ipc = ipc);
            return Err(2);
        }
    };

    let mut serial = *b"T5-000000000000\0";
    for (i, b) in addrs.mac.iter().enumerate() {
        serial[3 + 2 * i] = b"0123456789ABCDEF"[(b >> 4) as usize];
        serial[4 + 2 * i] = b"0123456789ABCDEF"[(b & 0xF) as usize];
    }
    let cfg = sm_config_t {
        discriminator: DISCRIMINATOR,
        passcode: PASSCODE,
        vendor_id: VENDOR,
        product_id: PRODUCT,
        device_name: c"Tab5".as_ptr(),
        mac: addrs.mac,
        kvs_get: Some(kvs_get),
        kvs_set: Some(kvs_set),
        kvs_delete: Some(kvs_delete),
        kvs_ctx: &mut kvs as *mut FileKvs as *mut c_void,
        rng_fill: Some(rng_fill),
        rng_ctx: core::ptr::null_mut(),
        network: sm_network_t::SM_NET_ETHERNET,
        verifier_iterations: 0,
        verifier_salt: null(),
        verifier_salt_len: 0,
        verifier_w0_l: null(),
        dac_der: null(),
        dac_der_len: 0,
        pai_der: null(),
        pai_der_len: 0,
        cd_der: null(),
        cd_der_len: 0,
        dac_privkey: null(),
        dac_sign: None,
        dac_sign_ctx: core::ptr::null_mut(),
        composition: null(),
        composition_len: 0,
        on_cluster_change: None,
        cluster_change_ctx: core::ptr::null_mut(),
        report_chunk_limit: 0,
        product_name: c"Tab5".as_ptr(),
        serial_number: serial.as_ptr() as *const c_char,
    };
    let r = sm_init(&cfg, now_ms());
    if r != 0 {
        event!(Error, "matter.init_failed", code = r);
        return Err(3);
    }
    let set_addrs = |a: &Addrs| {
        sm_set_addrs(a.ipv4.as_ref().map_or(null(), |v| v.as_ptr()), a.ipv6_ll.as_ref().map_or(null(), |v| v.as_ptr()));
    };
    set_addrs(&addrs);

    let udp = net.udp_bind_at(0, link::WLAN0, MATTER_PORT).map_err(|_| 4u32)?;
    let mdns = net.udp_bind_at(0, link::WLAN0, MDNS_PORT).map_err(|_| 5u32)?;
    for group in [IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb)), IpAddr::V4(Ipv4Addr::new(224, 0, 0, 251))] {
        if net.udp_join(mdns, group).is_err() {
            event!(Warn, "matter.no_multicast", v6 = matches!(group, IpAddr::V6(_)));
        }
    }
    event!(Info, "matter.ready", fabrics = sm_fabric_count(), discriminator = DISCRIMINATOR as u32, mac = &addrs.mac[..]);

    let mut rx = [0u8; PKT];
    let mut tx = [0u8; PKT];
    let mut dst = sm_addr_t { ip: [0; 16], is_v6: false, port: 0, scope_id: 0 };
    let mut next_addrs = now_ms() + ADDRS_MS;
    let send = |net: &mut Net, sock: u16, tx: &[u8], dst: &sm_addr_t| {
        if net.udp_send_ip(sock, to_ip(dst), dst.port, tx).is_err() {
            event!(Debug, "matter.send_failed", port = dst.port as u32, len = tx.len() as u32);
        }
    };
    loop {
        let mut busy = false;
        // Matter's datagrams.
        while let Ok((n, from, port)) = net.udp_recv_ip(udp, &mut rx, 0) {
            busy = true;
            let src = from_ip(from, port);
            let k = sm_udp_rx(rx.as_mut_ptr(), n, &src, now_ms(), tx.as_mut_ptr(), tx.len(), &mut dst);
            if k > 0 {
                send(&mut net, udp, &tx[..k], &dst);
            }
        }
        // mDNS queries.
        while let Ok((n, from, port)) = net.udp_recv_ip(mdns, &mut rx, 0) {
            busy = true;
            let src = from_ip(from, port);
            let k = sm_mdns_rx(rx.as_ptr(), n, &src, tx.as_mut_ptr(), tx.len(), &mut dst);
            if k > 0 {
                send(&mut net, mdns, &tx[..k], &dst);
            }
        }
        // Retransmissions, acknowledgements, reports; announcements.
        loop {
            let k = sm_poll(now_ms(), tx.as_mut_ptr(), tx.len(), &mut dst);
            if k == 0 {
                break;
            }
            send(&mut net, udp, &tx[..k], &dst);
        }
        loop {
            let k = sm_mdns_poll(now_ms(), tx.as_mut_ptr(), tx.len(), &mut dst);
            if k == 0 {
                break;
            }
            send(&mut net, mdns, &tx[..k], &dst);
        }
        let mut ev = sm_event_t { kind: sm_event_kind_t::SM_EV_NONE, arg: 0 };
        while sm_take_event(&mut ev) {
            event!(Info, "matter.event", kind = ev.kind as u32, arg = ev.arg, fabrics = sm_fabric_count());
        }
        let now = now_ms();
        if now >= next_addrs {
            next_addrs = now + ADDRS_MS;
            if let Some(a) = wifi_addrs(&net) {
                set_addrs(&a);
            }
        }
        if !busy {
            let deadline = sm_next_deadline(now);
            sleep_ms(deadline.saturating_sub(now).clamp(1, TICK_MS));
        }
    }
}
