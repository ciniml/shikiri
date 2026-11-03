#![no_std]
#![no_main]

//! The Wi-Fi service (N2b-3, doc/22): drives the ESP32-C6 (esp_hosted
//! 2.12) through `wlan:ctl` with esp_hosted's RPC (protobuf `Rpc` messages,
//! common/proto/esp_hosted_rpc.proto) and joins the access point given at
//! build time (build.rs). Tells the kernel the station's MAC address for
//! `netdev:wlan0`, then stays to log connection events and to reconnect.
//!
//! The passphrase is only sent to the chip; it is never logged.
//!
//! Built with `--permit wlan:ctl`.

use t5_rt::{Error, Handle, event, rights, sleep_ms, time_ns, wait_raw, wlan};

include!(concat!(env!("OUT_DIR"), "/credentials.rs"));

t5_rt::entry!(main);

// RpcType and RpcId (esp_hosted_rpc.proto:46-52, 93-472). A response's id
// is the request's + 256.
const REQ: u64 = 1;
const EVENT: u64 = 3;
const REQ_GET_MAC: u32 = 257;
const REQ_SET_MODE: u32 = 260;
const REQ_WIFI_INIT: u32 = 278;
const REQ_WIFI_START: u32 = 280;
const REQ_WIFI_CONNECT: u32 = 282;
const REQ_WIFI_SET_CONFIG: u32 = 284;
const REQ_WIFI_SET_PS: u32 = 270;
/// Rpc_Req_FeatureControl (esp_hosted 2.12.7, esp_hosted_rpc.proto:55-84,
/// 262, 1674-1685): feature 1 = Bluetooth; command 1 = BT init, 3 = BT
/// enable. The response's field 1 is an esp_err_t.
const REQ_FEATURE_CONTROL: u32 = 387;
const FEATURE_BLUETOOTH: u64 = 1;
const FEATURE_BT_INIT: u64 = 1;
const FEATURE_BT_ENABLE: u64 = 3;
/// wifi_ps_type_t: WIFI_PS_NONE.
const PS_NONE: u64 = 0;
const PS_MIN_MODEM: u64 = 1;
const RESP_OFFSET: u32 = 256;
const EVENT_ESP_INIT: u32 = 769;
const EVENT_HEARTBEAT: u32 = 770;
const EVENT_NO_ARGS: u32 = 773;
const EVENT_STA_CONNECTED: u32 = 775;
const EVENT_STA_DISCONNECTED: u32 = 776;

const MODE_STA: u64 = 1;
/// wifi_interface_t (esp_wifi_types_generic.h).
const WIFI_IF_STA: u64 = 0;
/// wifi_event_t (esp_wifi_types_generic.h).
const WIFI_EVENT_STA_START: i32 = 2;
/// wifi_auth_mode_t (esp_wifi_types_generic.h).
const AUTH_WPA2_PSK: u64 = 3;
const IF_STA: u64 = 0;
/// WIFI_INIT_CONFIG_MAGIC (esp_wifi.h).
const INIT_MAGIC: u64 = 0x1F2F_3F4F;

const RESPONSE_NS: u64 = 5_000_000_000;

// The serial interface's TLVs (virtual_serial_if/serial_if.c:20-62).
const TLV_EPNAME: u8 = 0x01;
const TLV_DATA: u8 = 0x02;
const EP_REQUEST: &[u8] = b"RPCRsp";

/// `arg0` bit 0: also start the Bluetooth controller on the chip (doc/48);
/// bit 1: leave the chip's modem sleep on (BLE gets more of the radio,
/// Wi-Fi answers later; doc/48 §9).
fn main(arg0: usize, _: usize) -> usize {
    match run(arg0 & 1 != 0, arg0 & 2 != 0) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "wifi.failed", step = step);
            step as usize
        }
    }
}

// ---- protobuf, just what these messages need ----

struct Pb<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl<'a> Pb<'a> {
    fn new(buf: &'a mut [u8]) -> Pb<'a> {
        Pb { buf, n: 0 }
    }

    fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            self.buf[self.n] = if v == 0 { b } else { b | 0x80 };
            self.n += 1;
            if v == 0 {
                break;
            }
        }
    }

    fn uint(&mut self, field: u32, v: u64) {
        self.varint((field as u64) << 3);
        self.varint(v);
    }

    fn bytes(&mut self, field: u32, b: &[u8]) {
        self.varint((field as u64) << 3 | 2);
        self.varint(b.len() as u64);
        self.buf[self.n..self.n + b.len()].copy_from_slice(b);
        self.n += b.len();
    }

    /// A nested message, built by `f`.
    fn msg(&mut self, field: u32, f: impl FnOnce(&mut Pb)) {
        let mut tmp = [0u8; 256];
        let mut inner = Pb::new(&mut tmp);
        f(&mut inner);
        let n = inner.n;
        self.bytes(field, &tmp[..n]);
    }

    fn done(&self) -> &[u8] {
        &self.buf[..self.n]
    }
}

#[derive(Clone, Copy)]
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// The fields of a message, in order (fixed-width ones skipped).
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&b, rest) = self.0.split_first()?;
            self.0 = rest;
            v |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    /// The first field numbered `field`.
    fn get(msg: &'a [u8], field: u32) -> Option<Value<'a>> {
        Fields(msg).find(|&(f, _)| f == field).map(|(_, v)| v)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u32, Value<'a>);

    fn next(&mut self) -> Option<(u32, Value<'a>)> {
        loop {
            let key = self.varint()?;
            let field = (key >> 3) as u32;
            match key & 7 {
                0 => return Some((field, Value::Varint(self.varint()?))),
                2 => {
                    let n = self.varint()? as usize;
                    let b = self.0.get(..n)?;
                    self.0 = &self.0[n..];
                    return Some((field, Value::Bytes(b)));
                }
                1 => self.0 = self.0.get(8..)?,
                5 => self.0 = self.0.get(4..)?,
                _ => return None,
            }
        }
    }
}

fn uint(msg: &[u8], field: u32) -> u64 {
    match Fields::get(msg, field) {
        Some(Value::Varint(v)) => v,
        _ => 0,
    }
}

fn int(msg: &[u8], field: u32) -> i32 {
    uint(msg, field) as i32
}

fn bytes<'a>(msg: &'a [u8], field: u32) -> &'a [u8] {
    match Fields::get(msg, field) {
        Some(Value::Bytes(b)) => b,
        _ => &[],
    }
}

// ---- the RPC link ----

struct Rpc {
    ctl: Handle,
    uid: u64,
    buf: [u8; wlan::MSG_MAX],
}

/// A message from the chip: its type, id and payload (the oneof field
/// numbered by the id).
struct Msg {
    kind: u64,
    id: u32,
    len: usize,
}

impl Rpc {
    /// Sends request `id` with the payload `body` builds.
    fn send(&mut self, id: u32, body: impl FnOnce(&mut Pb)) -> Result<(), Error> {
        self.uid += 1;
        let mut rpc = [0u8; 512];
        let mut p = Pb::new(&mut rpc);
        p.uint(1, REQ);
        p.uint(2, id as u64);
        p.uint(3, self.uid);
        p.msg(id, body);
        let data = p.done();
        let mut out = [0u8; 600];
        let mut n = 0;
        for part in [&[TLV_EPNAME, EP_REQUEST.len() as u8, 0][..], EP_REQUEST, &[TLV_DATA, data.len() as u8, (data.len() >> 8) as u8], data] {
            out[n..n + part.len()].copy_from_slice(part);
            n += part.len();
        }
        loop {
            match self.ctl.write(&out[..n]) {
                Err(Error::Busy) => sleep_ms(1),
                r => return r.map(|_| ()),
            }
        }
    }

    /// The next message, waiting up to `timeout_ns`. The payload is in
    /// `self.buf[..len]` after `payload`.
    fn recv(&mut self, timeout_ns: u64) -> Result<Msg, Error> {
        let n = loop {
            match self.ctl.read(&mut self.buf) {
                Ok(n) => break n,
                Err(Error::Busy) => {
                    wait_raw(&[self.ctl.raw()], timeout_ns)?;
                }
                Err(e) => return Err(e),
            }
        };
        // TLVs: endpoint name, then the data (serial_if.c parse_tlv).
        let b = &self.buf[..n];
        let ep_len = b.get(1..3).map_or(0, |l| l[0] as usize | (l[1] as usize) << 8);
        let at = 3 + ep_len;
        if b.first() != Some(&TLV_EPNAME) || b.get(at) != Some(&TLV_DATA) || b.len() < at + 3 {
            return Err(Error::Invalid);
        }
        let len = (b[at + 1] as usize | (b[at + 2] as usize) << 8).min(b.len() - at - 3);
        self.buf.copy_within(at + 3..at + 3 + len, 0);
        let rpc = &self.buf[..len];
        let kind = uint(rpc, 1);
        let id = uint(rpc, 2) as u32;
        let payload = bytes(rpc, id);
        let (start, plen) = (payload.as_ptr() as usize - rpc.as_ptr() as usize, payload.len());
        self.buf.copy_within(start..start + plen, 0);
        Ok(Msg { kind, id, len: plen })
    }

    fn payload(&self, m: &Msg) -> &[u8] {
        &self.buf[..m.len]
    }

    /// Sends request `id` and waits for its response, logging events that
    /// come in between. Returns the response's `resp` (field `resp_field`)
    /// as an error if it is not 0.
    fn call(&mut self, id: u32, resp_field: u32, body: impl FnOnce(&mut Pb)) -> Result<Msg, u32> {
        self.send(id, body).map_err(|_| id)?;
        let end = time_ns() + RESPONSE_NS;
        loop {
            let now = time_ns();
            if now >= end {
                event!(Error, "wifi.no_response", id = id);
                return Err(id);
            }
            let m = self.recv(end - now).map_err(|_| id)?;
            if m.kind == EVENT {
                self.on_event(&m);
                continue;
            }
            if m.id != id + RESP_OFFSET {
                event!(Warn, "wifi.unexpected", id = m.id, kind = m.kind as u32);
                continue;
            }
            let resp = int(self.payload(&m), resp_field);
            if resp != 0 {
                event!(Error, "wifi.request_failed", id = id, esp_err = resp);
                return Err(id);
            }
            return Ok(m);
        }
    }

    /// Logs an event; returns the connection state it implies, if any.
    fn on_event(&mut self, m: &Msg) -> Option<bool> {
        let p = self.payload(m);
        match m.id {
            EVENT_ESP_INIT => event!(Info, "wifi.chip_init", reset_reason = uint(p, 2) as u32),
            EVENT_HEARTBEAT => {}
            EVENT_NO_ARGS => event!(Info, "wifi.event", event_id = int(p, 2)),
            EVENT_STA_CONNECTED => {
                let c = bytes(p, 2);
                let ssid = bytes(c, 1);
                let ssid = &ssid[..(uint(c, 2) as usize).min(ssid.len())];
                event!(
                    Info,
                    "wifi.connected",
                    ssid = core::str::from_utf8(ssid).unwrap_or("?"),
                    bssid = bytes(c, 3),
                    channel = uint(c, 4) as u32,
                    authmode = int(c, 5),
                    aid = int(c, 6)
                );
                return Some(true);
            }
            EVENT_STA_DISCONNECTED => {
                let d = bytes(p, 2);
                event!(Info, "wifi.disconnected", reason = uint(d, 4) as u32, rssi = int(d, 5));
                return Some(false);
            }
            id => event!(Info, "wifi.other_event", id = id, len = p.len() as u32),
        }
        None
    }
}

fn run(bluetooth: bool, modem_sleep: bool) -> Result<(), u32> {
    let ctl = Handle::open(wlan::CTL, rights::READ | rights::WRITE | rights::WAIT | rights::CTL).map_err(|_| 1u32)?;
    let mut rpc = Rpc { ctl, uid: 0, buf: [0; wlan::MSG_MAX] };
    // The chip announces itself once the data path is open.
    if let Ok(m) = rpc.recv(3_000_000_000)
        && m.kind == EVENT
    {
        rpc.on_event(&m);
    }
    if bluetooth {
        start_bluetooth(&mut rpc);
    }

    // wifi_init_config: the values TCC's host sends (its sdkconfig,
    // CONFIG_WIFI_RMT_*); the chip uses them as given
    // (slave_wifi_std.c:150-198, 423-470).
    rpc.call(REQ_WIFI_INIT, 1, |p| {
        p.msg(1, |c| {
            for (field, v) in [
                (1, 10),  // static_rx_buf_num
                (2, 32),  // dynamic_rx_buf_num
                (3, 1),   // tx_buf_type: dynamic
                (4, 0),   // static_tx_buf_num
                (5, 32),  // dynamic_tx_buf_num
                (6, 0),   // cache_tx_buf_num
                (7, 0),   // csi_enable
                (8, 1),   // ampdu_rx_enable
                (9, 1),   // ampdu_tx_enable
                (10, 0),  // amsdu_tx_enable
                (11, 1),  // nvs_enable
                (12, 0),  // nano_enable
                (13, 6),  // rx_ba_win
                (14, 0),  // wifi_task_core_id
                (15, 752), // beacon_max_len
                (16, 32), // mgmt_sbuf_num
                (17, 1),  // feature_caps: WPA3-SAE (the chip keeps its own if different)
                (18, 1),  // sta_disconnected_pm
                (19, 7),  // espnow_max_encrypt_num
                (20, INIT_MAGIC),
                (21, 0),  // rx_mgmt_buf_type: static
                (22, 5),  // rx_mgmt_buf_num
                (23, 3),  // tx_hetb_queue_num
                (24, 0),  // dump_hesigb_enable
            ] {
                c.uint(field, v);
            }
        })
    })?;
    rpc.call(REQ_SET_MODE, 1, |p| p.uint(1, MODE_STA))?;
    // The MAC address is known once Wi-Fi is initialised
    // (ESP_ERR_WIFI_NOT_INIT before, seen on the Tab5).
    // The field is a wifi_interface_t (the chip passes it to
    // esp_wifi_get_mac, slave_wifi_std.c:506), not a mode: 1 would be the
    // soft-AP's address, and frames sent from it never leave the chip.
    let m = rpc.call(REQ_GET_MAC, 2, |p| p.uint(1, WIFI_IF_STA))?;
    let mac = bytes(rpc.payload(&m), 1);
    if mac.len() != 6 {
        return Err(2);
    }
    let mut word = 0u64;
    for (i, &b) in mac.iter().enumerate() {
        word |= (b as u64) << (8 * i);
    }
    event!(Info, "wifi.mac", mac = mac);
    rpc.ctl.ctl(wlan::SET_MAC, word).map_err(|_| 3u32)?;

    if SSID.is_empty() {
        event!(Warn, "wifi.no_credentials");
        rpc.call(REQ_WIFI_START, 1, |_| {})?;
        return idle(&mut rpc);
    }
    rpc.call(REQ_WIFI_SET_CONFIG, 1, |p| {
        p.uint(1, IF_STA);
        p.msg(2, |cfg| {
            cfg.msg(2, |sta| {
                sta.bytes(1, SSID);
                sta.bytes(2, PSK);
                // wifi_scan_threshold.authmode = WIFI_AUTH_WPA2_PSK, as TCC
                // sets it (wifi_setup.cpp:164).
                sta.msg(9, |t| t.uint(2, AUTH_WPA2_PSK));
                // wifi_pmf_config: capable.
                sta.msg(10, |pmf| pmf.uint(1, 1));
            })
        })
    })?;
    event!(Info, "wifi.configured", ssid = core::str::from_utf8(SSID).unwrap_or("?"));
    rpc.call(REQ_WIFI_START, 1, |_| {})?;
    // No modem sleep: with it the AP holds frames for the Tab5 until the
    // next beacon it listens to (25-210 ms pings from the LAN, doc/22 §7.5).
    if modem_sleep {
        // Set, not left: the chip keeps what an earlier `wifi` set.
        let r = rpc.call(REQ_WIFI_SET_PS, 1, |p| p.uint(1, PS_MIN_MODEM));
        event!(Info, "wifi.power_save_kept", ok = r.is_ok());
    } else if rpc.call(REQ_WIFI_SET_PS, 1, |p| p.uint(1, PS_NONE)).is_err() {
        event!(Warn, "wifi.power_save_left_on");
    }
    // Connect once the station has started, as ESP-IDF's host does (TCC:
    // "wifi station started", then esp_wifi_remote_connect). Connecting at
    // once failed twice before joining (reasons 202, 205).
    let end = time_ns() + RESPONSE_NS;
    while time_ns() < end {
        let Ok(m) = rpc.recv(end - time_ns()) else { break };
        let started = m.kind == EVENT && m.id == EVENT_NO_ARGS && int(rpc.payload(&m), 2) == WIFI_EVENT_STA_START;
        if m.kind == EVENT {
            rpc.on_event(&m);
        }
        if started {
            break;
        }
    }
    rpc.call(REQ_WIFI_CONNECT, 1, |_| {})?;
    idle(&mut rpc)
}

/// Starts the chip's Bluetooth controller: since esp_hosted 2.5.2 the host
/// asks for it (CHANGELOG, slave_control.c:520-562); then `bt:hci` carries
/// its packets. A failure is logged and Wi-Fi goes on.
fn start_bluetooth(rpc: &mut Rpc) {
    for (name, command) in [("init", FEATURE_BT_INIT), ("enable", FEATURE_BT_ENABLE)] {
        let r = rpc.call(REQ_FEATURE_CONTROL, 1, |p| {
            p.uint(1, FEATURE_BLUETOOTH);
            p.uint(2, command);
        });
        if r.is_err() {
            event!(Warn, "wifi.bt_failed", step = name);
            return;
        }
    }
    event!(Info, "wifi.bt_started");
}

/// Logs events; after a disconnection, connects again (every 5 s).
fn idle(rpc: &mut Rpc) -> Result<(), u32> {
    let mut retry_at: Option<u64> = None;
    loop {
        let now = time_ns();
        let timeout = retry_at.map_or(t5_rt::FOREVER, |t| t.saturating_sub(now).max(1));
        match rpc.recv(timeout) {
            Ok(m) if m.kind == EVENT => match rpc.on_event(&m) {
                Some(false) if !SSID.is_empty() => retry_at = Some(time_ns() + 5_000_000_000),
                Some(true) => retry_at = None,
                _ => {}
            },
            Ok(m) => event!(Warn, "wifi.unexpected", id = m.id, kind = m.kind as u32),
            Err(Error::TimedOut) => {
                retry_at = None;
                event!(Info, "wifi.reconnect");
                rpc.call(REQ_WIFI_CONNECT, 1, |_| {})?;
            }
            Err(_) => return Err(9),
        }
    }
}
