//! Commissioning over BLE, the device's side (doc/49 §10, MT4): through
//! `svc:ble` (doc/48 M4). While the node is commissionable it advertises
//! the Matter service (0xFFF6, the discriminator in its service data) with
//! C1 (the commissioner writes) and C2 (indicated back); the core's BTP
//! carries Matter's messages over them. And the controller's side: scan
//! for a commissionable device's service data, connect, find C1 and C2,
//! the BTP handshake, then the commissioning's messages as C1 writes (one
//! at a time) and C2 indications, until the controller lets the link go
//! (after AddNOC; CASE is over the network). `ble` serves one client at a
//! time, so the channel is held only while it is used.

use heapless::{Deque, Vec};
use simple_matter::btp::AdvData;
use simple_matter::btp::gatt::{ADV_TOTAL_LEN, C1_UUID128, C2_UUID128, MATTER_SERVICE_UUID16};
use simple_matter::btp::{Btp, BtpRole};
use simple_matter::transport::net::{BtpConnId, PeerAddr};
use t5_rt::{Channel, Error, Handle, ble, chan, event, wait};

use crate::controller::Controller;
use crate::device::Device;

const PROP_READ: u8 = 0x02;
const PROP_WRITE: u8 = 0x08;
const PROP_INDICATE: u8 = 0x20;
/// The GAP Device Name (not in the advertisement).
const NAME: &[u8] = b"Tab5";
const INTERVAL_MS: u16 = 100;
/// How long to wait before asking `ble` again (not running, refused), and
/// for its answer to `ADVERTISE` (it has no controller without `wifi 1`).
const RETRY_MS: u64 = 5000;
/// How often to look whether `ble` is still there (a closed channel shows
/// only as an event to `wait`).
const ALIVE_MS: u64 = 2000;
/// Matter messages waiting for BTP, which takes one at a time.
const QUEUE: usize = 3;
const SDU: usize = 1280;
/// The controller's side: scanning (passive; window 30 ms of 100 ms), and
/// how long until the BTP link is up.
const SCAN_S: u8 = 10;
const LINK_MS: u64 = 30_000;
/// The one BLE connection, as the core sees it.
pub const CONN: PeerAddr = PeerAddr::Ble(BtpConnId(0));

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    /// Not advertising (the channel may be open).
    Idle,
    /// `ADVERTISE` sent, `ADVERTISING` not yet back.
    Asked,
    Advertising,
    /// `STOP` sent, `DISCONNECTED` not yet back.
    Stopping,
    Connected,
    // The controller's side.
    /// `SCAN` until the device's advertisement; `STOP`, waiting for `DONE`.
    Scan,
    ScanStop,
    /// `CONNECT`, waiting for `CONNECTED`.
    Connect,
    /// `SERVICES`, `CHARS`: until `END`.
    Services,
    Chars,
    /// The handshake request written to C1, waiting for `WRITTEN`; C2
    /// subscribed, waiting for the handshake response.
    Handshake,
    Subscribe,
    Linked,
    /// `DISCONNECT` sent.
    Leaving,
}

impl State {
    fn central(self) -> bool {
        !matches!(self, State::Idle | State::Asked | State::Advertising | State::Stopping | State::Connected)
    }
}

pub struct BleLink {
    chan: Option<Channel>,
    state: State,
    /// What is advertised.
    adv: [u8; ADV_TOTAL_LEN],
    c1: u16,
    c2_cccd: u16,
    c2: u16,
    /// The connection's ATT MTU, once `ble` has said (BTP's fragments fit
    /// it: smctl does not know it and asks for 20-byte ones otherwise).
    mtu: Option<u16>,
    subscribed: bool,
    /// An indication is out (`SENT` not yet back).
    sending: bool,
    btp: Btp<6>,
    queue: Deque<Vec<u8, SDU>, QUEUE>,
    retry_at: u64,
    /// This link's C1 writes and indications, and when it came.
    writes: u32,
    sent: u32,
    since: u64,
    /// When `ADVERTISE` was sent; when `ble` was last looked at.
    asked_at: u64,
    alive_at: u64,
    /// The controller's side: the discriminator looked for, the device's
    /// address type and address, the Matter service's index, a C1 write is
    /// out, since when.
    want: u16,
    /// `want` is the top 4 bits only (a manual pairing code's).
    want_short: bool,
    peer: [u8; 7],
    service: Option<u8>,
    writing: bool,
    started: u64,
}

impl BleLink {
    pub const fn new() -> BleLink {
        BleLink {
            chan: None,
            state: State::Idle,
            adv: [0; ADV_TOTAL_LEN],
            c1: 0,
            c2: 0,
            c2_cccd: 0,
            mtu: None,
            subscribed: false,
            sending: false,
            btp: Btp::new(BtpRole::Peripheral),
            queue: Deque::new(),
            retry_at: 0,
            writes: 0,
            sent: 0,
            since: 0,
            asked_at: 0,
            alive_at: 0,
            want: 0,
            want_short: false,
            peer: [0; 7],
            service: None,
            writing: false,
            started: 0,
        }
    }

    fn send(&mut self, msg: &[u8], now: u64) {
        if let Some(c) = &self.chan
            && c.send(msg).is_err()
        {
            event!(Warn, "matter.ble_gone");
            self.gone(now);
        }
    }

    fn gone(&mut self, now: u64) {
        self.chan = None;
        self.drop_link();
        self.state = State::Idle;
        self.retry_at = now + RETRY_MS;
    }

    fn drop_link(&mut self) {
        self.btp.reset();
        self.mtu = None;
        self.subscribed = false;
        self.sending = false;
        self.writing = false;
        self.queue.clear();
    }

    /// The controller's side: finds the commissionable device with
    /// `discriminator`, connects and sets up BTP; `Controller::ble_linked`
    /// once it is up, `ble_failed` if not.
    pub fn central_start(&mut self, discriminator: u16, short: bool, now: u64) -> Result<(), &'static str> {
        if self.state != State::Idle {
            return Err("BLE busy");
        }
        if self.chan.is_none() {
            self.chan = Some(Channel::connect(ble::SVC).map_err(|_| "no BLE service")?);
        }
        self.drop_link();
        self.btp = Btp::new(BtpRole::Central);
        (self.want, self.want_short, self.service, self.c1, self.c2, self.started) = (discriminator, short, None, 0, 0, now);
        let mut req = [ble::SCAN, SCAN_S, 0, 0, 0, 0, 0];
        req[3..5].copy_from_slice(&100u16.to_le_bytes());
        req[5..7].copy_from_slice(&30u16.to_le_bytes());
        self.send(&req, now);
        self.state = State::Scan;
        Ok(())
    }

    /// The controller is done with the link (or gave up).
    pub fn central_stop(&mut self, now: u64) {
        match self.state {
            State::Scan => {
                self.send(&[ble::STOP], now);
                self.state = State::ScanStop;
                self.want = u16::MAX;
            }
            s if s.central() && s != State::Leaving && s != State::ScanStop => {
                self.send(&[ble::DISCONNECT], now);
                self.state = State::Leaving;
            }
            _ => {}
        }
    }

    fn central_failed(&mut self, ctrl: &mut Controller, why: &'static str, now: u64) {
        event!(Warn, "matter.ble_central_failed", why = why, state = self.state as u32);
        ctrl.ble_failed(why);
        let s = self.state;
        self.drop_link();
        match s {
            State::Scan | State::ScanStop => {
                if s == State::Scan {
                    self.send(&[ble::STOP], now);
                }
                self.state = State::ScanStop;
                self.want = u16::MAX;
            }
            State::Leaving => self.state = State::Idle,
            _ => {
                self.send(&[ble::DISCONNECT], now);
                self.state = State::Leaving;
            }
        }
    }

    /// One of `ble`'s events while the controller's side runs.
    fn central_event(&mut self, m: &[u8], ctrl: &mut Controller, now: u64) {
        let n = m.len();
        let at = |i: usize| u16::from_le_bytes([m[i], m[i + 1]]);
        match (m[0], self.state) {
            (ble::ADV, State::Scan) if n >= 11 => {
                let data = &m[11..n.min(11 + m[10] as usize)];
                let (want, short) = (self.want, self.want_short);
                if service_data(data).is_some_and(|a| if short { a.discriminator >> 8 == want } else { a.discriminator == want }) {
                    self.peer.copy_from_slice(&m[1..8]);
                    event!(Info, "matter.ble_found", addr = &m[2..8], rssi = m[9] as i8 as i32);
                    self.send(&[ble::STOP], now);
                    self.state = State::ScanStop;
                }
            }
            (ble::DONE, State::ScanStop) if self.want != u16::MAX => {
                let mut req = [0u8; 9];
                req[0] = ble::CONNECT;
                req[1..8].copy_from_slice(&self.peer);
                req[8] = 10;
                self.send(&req, now);
                self.state = State::Connect;
            }
            (ble::DONE, State::ScanStop) => self.state = State::Idle,
            (ble::DONE, State::Scan) => {
                event!(Info, "matter.ble_not_found", discriminator = self.want as u32);
                ctrl.ble_failed("not found over BLE");
                self.state = State::Idle;
            }
            (ble::CONNECTED, State::Connect) if n >= 3 => {
                self.mtu = Some(at(1));
                self.send(&[ble::SERVICES], now);
                self.state = State::Services;
            }
            (ble::SERVICE, State::Services) if n >= 6 => {
                let uuid = &m[6..];
                if uuid == MATTER_SERVICE_UUID16.to_le_bytes() || uuid == le(simple_matter::btp::gatt::MATTER_SERVICE_UUID128) {
                    self.service = Some(m[1]);
                }
            }
            (ble::END, State::Services) => match self.service {
                Some(i) => {
                    self.send(&[ble::CHARS, i], now);
                    self.state = State::Chars;
                }
                None => self.central_failed(ctrl, "no Matter service", now),
            },
            (ble::CHAR, State::Chars) if n >= 22 => {
                let uuid = &m[6..22];
                if uuid == le(C1_UUID128) {
                    self.c1 = at(1);
                } else if uuid == le(C2_UUID128) {
                    self.c2 = at(1);
                }
            }
            (ble::END, State::Chars) => {
                if self.c1 == 0 || self.c2 == 0 {
                    self.central_failed(ctrl, "no C1 or C2", now);
                    return;
                }
                // The handshake request to C1, then C2 subscribed (chip's
                // order: the response comes once C2 is subscribed).
                let mut req = [0u8; ble::MSG_MAX];
                req[0] = ble::WRITE;
                req[1..3].copy_from_slice(&self.c1.to_le_bytes());
                req[3] = 1;
                match self.btp.start_handshake(&mut req[4..], self.mtu, now) {
                    Ok(k) => {
                        self.send(&req[..4 + k], now);
                        self.state = State::Handshake;
                    }
                    Err(_) => self.central_failed(ctrl, "BTP", now),
                }
            }
            (ble::WRITTEN, State::Handshake) => {
                let mut req = [ble::SUBSCRIBE, 0, 0, 1];
                req[1..3].copy_from_slice(&self.c2.to_le_bytes());
                self.send(&req, now);
                self.state = State::Subscribe;
            }
            (ble::SUBSCRIBED, State::Subscribe) => {}
            (ble::NOTIFY, State::Subscribe | State::Linked) if n >= 4 && at(1) == self.c2 => {
                if let Err(e) = self.btp.process_incoming(&m[4..], self.mtu, now) {
                    event!(Warn, "matter.btp_rx", error = e as u32);
                    return;
                }
                if self.state == State::Subscribe && self.btp.is_established() {
                    self.state = State::Linked;
                    event!(Info, "matter.btp_up", fragment = self.btp.fragment_size() as u32, window = self.btp.window() as u32, ms = (now - self.started) as u32);
                    ctrl.ble_linked(now);
                }
                let mut sdu = [0u8; SDU];
                loop {
                    let k = match self.btp.recv() {
                        Some(s) if s.len() <= SDU => {
                            sdu[..s.len()].copy_from_slice(s);
                            s.len()
                        }
                        Some(_) => continue,
                        None => break,
                    };
                    ctrl.ble_rx(&mut sdu[..k], now);
                }
            }
            (ble::WRITTEN, State::Linked) => self.writing = false,
            (ble::DISCONNECTED, State::Leaving) => self.state = State::Idle,
            (ble::DISCONNECTED, _) => self.central_failed(ctrl, "BLE link lost", now),
            (ble::FAILED, State::Leaving) => self.state = State::Idle,
            (ble::FAILED, _) if n >= 3 => {
                event!(Warn, "matter.ble_failed", request = m[1], code = m[2]);
                self.central_failed(ctrl, "BLE request failed", now);
            }
            _ => {}
        }
    }

    /// Advertises `want` (the device's advertisement, `None` when it is not
    /// commissionable); a connected commissioner stays until it leaves.
    pub fn advertise(&mut self, want: Option<[u8; ADV_TOTAL_LEN]>, now: u64) {
        if self.state.central() {
            return;
        }
        if let Some(c) = &self.chan
            && now >= self.alive_at
        {
            self.alive_at = now + ALIVE_MS;
            if wait(&[&c.0], 0).is_ok_and(|ev| ev.kind == chan::PEER_CLOSED) {
                event!(Warn, "matter.ble_gone");
                self.gone(now);
                return;
            }
        }
        if self.state == State::Asked && now >= self.asked_at + RETRY_MS {
            event!(Warn, "matter.ble_no_answer");
            self.gone(now);
            return;
        }
        match (want, self.state) {
            (_, State::Connected | State::Asked | State::Stopping) => {}
            (Some(adv), State::Idle) => {
                if self.chan.is_none() {
                    if now < self.retry_at {
                        return;
                    }
                    match Channel::connect(ble::SVC) {
                        Ok(c) => self.chan = Some(c),
                        Err(e) => {
                            event!(Debug, "matter.no_ble", error = e as u32);
                            self.retry_at = now + RETRY_MS;
                            return;
                        }
                    }
                }
                self.adv = adv;
                let mut req = [0u8; ble::MSG_MAX];
                let n = advertise_request(&mut req, &adv);
                self.send(&req[..n], now);
                if self.chan.is_some() {
                    self.state = State::Asked;
                    self.asked_at = now;
                }
            }
            (Some(adv), State::Advertising) if adv != self.adv => {
                // A new discriminator: again with the new data.
                self.send(&[ble::STOP], now);
                self.state = State::Stopping;
            }
            (Some(_), State::Advertising) => {}
            (None, State::Advertising) => {
                self.send(&[ble::STOP], now);
                self.state = State::Stopping;
            }
            (None, State::Idle) => {
                // Let other clients have `ble`.
                self.chan = None;
            }
            // The controller's side (returned above).
            _ => {}
        }
    }

    /// `ble`'s events: the C1 writes go through BTP to the device, whose
    /// replies wait for BTP; on the controller's side, the indications go
    /// to the controller. True if anything came.
    pub fn rx(&mut self, dev: &mut Device, ctrl: &mut Controller, now: u64, tx: &mut [u8]) -> bool {
        let mut any = false;
        let mut m = [0u8; ble::MSG_MAX];
        loop {
            let Some(c) = &self.chan else { return any };
            let n = match c.0.read(&mut m) {
                Ok(n) if n > 0 => n,
                Ok(_) | Err(Error::Busy) => return any,
                Err(_) => {
                    event!(Warn, "matter.ble_gone");
                    self.gone(now);
                    return any;
                }
            };
            any = true;
            if self.state.central() {
                self.central_event(&m[..n], ctrl, now);
                continue;
            }
            let at = |i: usize| u16::from_le_bytes([m[i], m[i + 1]]);
            match m[0] {
                ble::ADVERTISING if n >= 10 => {
                    self.c1 = at(2);
                    self.c2 = at(6);
                    self.c2_cccd = at(8);
                    if self.state == State::Asked {
                        self.state = State::Advertising;
                    }
                    let d = u16::from_le_bytes([self.adv[8], self.adv[9]]) & 0x0FFF;
                    event!(Info, "matter.ble_advertising", discriminator = d as u32);
                }
                ble::PEER if n >= 8 => {
                    self.state = State::Connected;
                    self.drop_link();
                    // (The controller's side leaves a central's BTP.)
                    self.btp = Btp::new(BtpRole::Peripheral);
                    (self.writes, self.sent, self.since) = (0, 0, now);
                    event!(Info, "matter.ble_peer", addr = &m[2..8]);
                }
                ble::PEER_WRITE if n >= 3 && self.state == State::Connected => {
                    let handle = at(1);
                    if handle == self.c2_cccd {
                        self.subscribed = n >= 4 && m[3] & 0x02 != 0;
                    } else if handle == self.c1 {
                        self.writes += 1;
                        let was = self.btp.is_established();
                        if let Err(e) = self.btp.process_incoming(&m[3..n], self.mtu, now) {
                            event!(Warn, "matter.btp_rx", error = e as u32);
                            continue;
                        }
                        if !was && self.btp.is_established() {
                            event!(Info, "matter.btp_up", fragment = self.btp.fragment_size() as u32, window = self.btp.window() as u32);
                        }
                        self.deliver(dev, now, tx);
                    }
                }
                ble::MTU if n >= 3 => {
                    self.mtu = Some(at(1));
                    event!(Debug, "matter.ble_mtu", mtu = at(1) as u32);
                }
                ble::SENT => self.sending = false,
                ble::FAILED if n >= 3 => {
                    event!(Warn, "matter.ble_failed", request = m[1], code = m[2]);
                    match m[1] {
                        ble::SEND => self.sending = false,
                        ble::ADVERTISE => self.gone(now),
                        _ => {}
                    }
                }
                ble::DISCONNECTED => {
                    if self.state == State::Connected {
                        event!(Info, "matter.ble_peer_gone", writes = self.writes, sent = self.sent, ms = now - self.since);
                    }
                    self.state = State::Idle;
                    self.drop_link();
                }
                _ => {}
            }
        }
    }

    /// Whole messages out of BTP, to the device.
    fn deliver(&mut self, dev: &mut Device, now: u64, tx: &mut [u8]) {
        let mut sdu = [0u8; SDU];
        loop {
            let n = match self.btp.recv() {
                Some(s) if s.len() <= SDU => {
                    sdu[..s.len()].copy_from_slice(s);
                    s.len()
                }
                Some(_) => continue,
                None => return,
            };
            if let Some(d) = dev.handle_rx(&mut sdu[..n], CONN, now, tx)
                && d.addr == CONN
            {
                self.queue(&tx[..d.len]);
            }
        }
    }

    /// A Matter message for the commissioner (or, on the controller's
    /// side, for the device).
    pub fn queue(&mut self, sdu: &[u8]) {
        if self.state != State::Connected && self.state != State::Linked {
            return;
        }
        let v = Vec::from_slice(sdu).unwrap_or_default();
        if self.queue.push_back(v).is_err() {
            event!(Warn, "matter.ble_dropped", len = sdu.len() as u32);
        }
    }

    /// Messages into BTP, BTP's fragments out as indications (one at a
    /// time), and BTP's timeouts. True if something was sent.
    pub fn pump(&mut self, ctrl: &mut Controller, now: u64) -> bool {
        if self.state.central() {
            return self.central_pump(ctrl, now);
        }
        if self.state != State::Connected {
            return false;
        }
        if self.btp.is_timed_out(now) {
            event!(Warn, "matter.ble_timeout");
            self.send(&[ble::STOP], now);
            self.drop_link();
            self.state = State::Stopping;
            return true;
        }
        if self.btp.is_established()
            && self.btp.can_send()
            && let Some(sdu) = self.queue.pop_front()
            && let Err(e) = self.btp.send(&sdu, now)
        {
            event!(Warn, "matter.btp_tx", error = e as u32, len = sdu.len() as u32);
        }
        if !self.subscribed || self.sending {
            return false;
        }
        let mut m = [0u8; ble::MSG_MAX];
        let n = match self.btp.process_outgoing(&mut m[4..4 + ble::VALUE_MAX], self.mtu, now) {
            Ok(n) if n > 0 => n,
            Ok(_) => return false,
            Err(e) => {
                event!(Warn, "matter.btp_tx", error = e as u32, len = 0u32);
                return false;
            }
        };
        m[0] = ble::SEND;
        m[1..3].copy_from_slice(&self.c2.to_le_bytes());
        m[3] = 1;
        self.sending = true;
        self.sent += 1;
        self.send(&m[..4 + n], now);
        true
    }

    /// The controller's side: the link's deadline, messages into BTP and
    /// BTP's fragments out as C1 writes (one at a time).
    fn central_pump(&mut self, ctrl: &mut Controller, now: u64) -> bool {
        if self.state != State::Linked {
            if self.state != State::Leaving && self.state != State::ScanStop && now >= self.started + LINK_MS {
                self.central_failed(ctrl, "BLE link timed out", now);
                return true;
            }
            return false;
        }
        if self.btp.is_timed_out(now) {
            self.central_failed(ctrl, "BTP timed out", now);
            return true;
        }
        if self.btp.can_send()
            && let Some(sdu) = self.queue.pop_front()
            && let Err(e) = self.btp.send(&sdu, now)
        {
            event!(Warn, "matter.btp_tx", error = e as u32, len = sdu.len() as u32);
        }
        if self.writing {
            return false;
        }
        let mut m = [0u8; ble::MSG_MAX];
        let n = match self.btp.process_outgoing(&mut m[4..4 + ble::VALUE_MAX], self.mtu, now) {
            Ok(n) if n > 0 => n,
            _ => return false,
        };
        m[0] = ble::WRITE;
        m[1..3].copy_from_slice(&self.c1.to_le_bytes());
        m[3] = 1;
        self.writing = true;
        self.sent += 1;
        self.send(&m[..4 + n], now);
        true
    }

    /// The channel to `ble`, to `wait` on.
    pub fn handle(&self) -> Option<&Handle> {
        self.chan.as_ref().map(|c| &c.0)
    }

    /// `wait` said `ble` closed its end.
    pub fn closed(&mut self, now: u64) {
        event!(Warn, "matter.ble_gone");
        self.gone(now);
    }

    pub fn next_deadline(&self) -> Option<u64> {
        match self.state {
            State::Connected | State::Linked => self.btp.next_deadline(),
            s if s.central() => Some(self.started + LINK_MS),
            _ => None,
        }
    }
}

/// The Matter service data in an advertisement, if any.
fn service_data(mut data: &[u8]) -> Option<AdvData> {
    while data.len() >= 2 {
        let len = data[0] as usize;
        let ad = data.get(1..1 + len)?;
        if len == 11 && ad[0] == 0x16 && ad[1..3] == MATTER_SERVICE_UUID16.to_le_bytes() {
            return AdvData::parse_service_data(ad[3..11].try_into().ok()?).ok();
        }
        data = &data[1 + len..];
    }
    None
}

/// A UUID as `svc:ble` takes it (the core's are in the string's order).
const fn le(mut u: [u8; 16]) -> [u8; 16] {
    let mut i = 0;
    while i < 8 {
        let t = u[i];
        u[i] = u[15 - i];
        u[15 - i] = t;
        i += 1;
    }
    u
}

/// `ADVERTISE`: the Matter service (16-bit UUID), C1 (write) and C2 (read,
/// indicate); the advertising data is the core's (flags and the service
/// data), no scan response.
fn advertise_request(req: &mut [u8; ble::MSG_MAX], adv: &[u8; ADV_TOTAL_LEN]) -> usize {
    let mut n = 0;
    let mut put = |b: &[u8]| {
        req[n..n + b.len()].copy_from_slice(b);
        n += b.len();
    };
    put(&[ble::ADVERTISE]);
    put(&INTERVAL_MS.to_le_bytes());
    put(&[NAME.len() as u8]);
    put(NAME);
    put(&[2]);
    put(&MATTER_SERVICE_UUID16.to_le_bytes());
    put(&[2, 16]);
    put(&le(C1_UUID128));
    put(&[PROP_WRITE, 16]);
    put(&le(C2_UUID128));
    put(&[PROP_READ | PROP_INDICATE]);
    put(&[ADV_TOTAL_LEN as u8]);
    put(adv);
    put(&[0]);
    n
}
