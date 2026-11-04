//! Commissioning over BLE, the device's side (doc/49 §10, MT4): through
//! `svc:ble` (doc/48 M4). While the node is commissionable it advertises
//! the Matter service (0xFFF6, the discriminator in its service data) with
//! C1 (the commissioner writes) and C2 (indicated back); the core's BTP
//! carries Matter's messages over them. `ble` serves one client at a time,
//! so the channel is held only while the node is commissionable or a
//! commissioner is connected.

use heapless::{Deque, Vec};
use simple_matter::btp::gatt::{ADV_TOTAL_LEN, C1_UUID128, C2_UUID128, MATTER_SERVICE_UUID16};
use simple_matter::btp::{Btp, BtpRole};
use simple_matter::transport::net::{BtpConnId, PeerAddr};
use t5_rt::{Channel, Error, ble, chan, event, sleep_ms, wait};

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
        self.queue.clear();
    }

    /// Advertises `want` (the device's advertisement, `None` when it is not
    /// commissionable); a connected commissioner stays until it leaves.
    pub fn advertise(&mut self, want: Option<[u8; ADV_TOTAL_LEN]>, now: u64) {
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
        }
    }

    /// `ble`'s events: the C1 writes go through BTP to the device, whose
    /// replies wait for BTP. True if anything came.
    pub fn rx(&mut self, dev: &mut Device, now: u64, tx: &mut [u8]) -> bool {
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

    /// A Matter message for the commissioner.
    pub fn queue(&mut self, sdu: &[u8]) {
        if self.state != State::Connected {
            return;
        }
        let v = Vec::from_slice(sdu).unwrap_or_default();
        if self.queue.push_back(v).is_err() {
            event!(Warn, "matter.ble_dropped", len = sdu.len() as u32);
        }
    }

    /// Messages into BTP, BTP's fragments out as indications (one at a
    /// time), and BTP's timeouts. True if something was sent.
    pub fn pump(&mut self, now: u64) -> bool {
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

    /// Sleeps up to `ms`, waking early when `ble` has something.
    pub fn idle(&mut self, ms: u64, now: u64) {
        let Some(c) = &self.chan else {
            sleep_ms(ms);
            return;
        };
        if wait(&[&c.0], ms * 1_000_000).is_ok_and(|ev| ev.kind == chan::PEER_CLOSED) {
            event!(Warn, "matter.ble_gone");
            self.gone(now);
        }
    }

    pub fn next_deadline(&self) -> Option<u64> {
        if self.state == State::Connected { self.btp.next_deadline() } else { None }
    }
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
