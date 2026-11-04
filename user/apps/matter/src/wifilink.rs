//! The Wi-Fi network for Matter's NetworkCommissioning (doc/49 §20): the
//! cluster's driver (`TabWifi`) keeps a commissioner's ConnectNetwork for
//! the main loop, which asks `svc:wifi` (`WifiLink`) to join and feeds the
//! answer back. At start the network the Tab5 is on is seeded into the
//! cluster, so commissioners see it as set up. Once commissioning is
//! complete `wifi` keeps the network in the settings; if the fail-safe ends
//! first it goes back (doc/50 §5). The passphrase is passed on, never
//! logged.

use heapless::Vec;
use simple_matter::wifi::{WifiDriver, WifiStatus};
use t5_rt::{Channel, Error, Handle, event, wifi};

use crate::device::Device;

/// How long to wait before asking `wifi` again.
const RETRY_MS: u64 = 5000;

/// The cluster's driver: a join to ask for, and its state.
pub struct TabWifi {
    join: Option<(Vec<u8, { wifi::SSID_MAX }>, Vec<u8, { wifi::PSK_MAX }>)>,
    pub(crate) status: WifiStatus,
}

impl TabWifi {
    pub const fn new() -> TabWifi {
        TabWifi { join: None, status: WifiStatus::Idle }
    }
}

impl WifiDriver for TabWifi {
    fn connect(&mut self, ssid: &[u8], creds: &[u8]) {
        match (Vec::from_slice(ssid), Vec::from_slice(creds)) {
            (Ok(s), Ok(p)) => {
                self.join = Some((s, p));
                self.status = WifiStatus::Connecting;
            }
            _ => self.status = WifiStatus::Failed { reason: 0 },
        }
    }

    fn status(&self) -> WifiStatus {
        self.status
    }
}

/// The channel to `svc:wifi`.
pub struct WifiLink {
    chan: Option<Channel>,
    retry_at: u64,
    /// A join has just ended: the addresses are read again.
    joined: bool,
    /// A join was asked for and is neither saved nor left yet.
    unsaved: bool,
}

impl WifiLink {
    pub const fn new() -> WifiLink {
        WifiLink { chan: None, retry_at: 0, joined: false, unsaved: false }
    }

    /// Commissioning is complete: `wifi` keeps the network it joined.
    pub fn save(&mut self) {
        self.after_join(wifi::SAVE);
    }

    /// The fail-safe ended first: `wifi` goes back to its network.
    pub fn revert(&mut self) {
        self.after_join(wifi::REVERT);
    }

    fn after_join(&mut self, req: u8) {
        if core::mem::take(&mut self.unsaved)
            && let Some(c) = &self.chan
        {
            let _ = c.send(&[req]);
        }
    }

    /// True once after a join: the caller reads the addresses again.
    pub fn take_joined(&mut self) -> bool {
        core::mem::take(&mut self.joined)
    }

    pub fn handle(&self) -> Option<&Handle> {
        self.chan.as_ref().map(|c| &c.0)
    }

    /// `wait` said `wifi` closed its end.
    pub fn closed(&mut self, now: u64) {
        self.chan = None;
        self.retry_at = now + RETRY_MS;
    }

    /// Connects (and asks what network the Tab5 is on), passes a join on,
    /// takes `wifi`'s answers. True if anything happened.
    pub fn pump(&mut self, dev: &mut Device, now: u64) -> bool {
        if self.chan.is_none() {
            if now < self.retry_at {
                return false;
            }
            match Channel::connect(wifi::SVC) {
                Ok(c) => {
                    let _ = c.0.write(&[wifi::STATUS]);
                    self.chan = Some(c);
                }
                Err(_) => {
                    self.retry_at = now + RETRY_MS;
                    return false;
                }
            }
        }
        let mut any = false;
        if let Some((ssid, psk)) = dev.wifi().join.take() {
            let mut m = [0u8; 3 + wifi::SSID_MAX + wifi::PSK_MAX];
            m[0] = wifi::JOIN;
            m[1] = ssid.len() as u8;
            m[2..2 + ssid.len()].copy_from_slice(&ssid);
            m[2 + ssid.len()] = psk.len() as u8;
            m[3 + ssid.len()..3 + ssid.len() + psk.len()].copy_from_slice(&psk);
            event!(Info, "matter.wifi_join", ssid = core::str::from_utf8(&ssid).unwrap_or("?"));
            match &self.chan {
                Some(c) if c.send(&m[..3 + ssid.len() + psk.len()]).is_ok() => self.unsaved = true,
                _ => dev.wifi().status = WifiStatus::Failed { reason: 0 },
            }
            any = true;
        }
        let mut m = [0u8; 3 + wifi::SSID_MAX];
        loop {
            let Some(c) = &self.chan else { break };
            let n = match c.0.read(&mut m) {
                Ok(n) if n > 0 => n,
                Ok(_) | Err(Error::Busy) => break,
                Err(_) => {
                    self.closed(now);
                    break;
                }
            };
            any = true;
            match m[0] {
                wifi::STATUS if n >= 3 => {
                    let ssid = &m[3..n.min(3 + m[2] as usize)];
                    let on = m[1] != 0;
                    event!(Info, "matter.wifi", ssid = core::str::from_utf8(ssid).unwrap_or("?"), on = on);
                    dev.seed_wifi(ssid, on);
                }
                wifi::JOINED => {
                    event!(Info, "matter.wifi_joined");
                    dev.wifi().status = WifiStatus::Connected;
                    // (`wifi` has asked `net` for a new address.)
                    self.joined = true;
                }
                wifi::SAVED if n >= 2 => event!(Info, "matter.wifi_saved", ok = m[1] != 0),
                wifi::FAILED if n >= 3 => {
                    let reason = u16::from_le_bytes([m[1], m[2]]) as i32;
                    event!(Warn, "matter.wifi_failed", reason = reason);
                    dev.wifi().status = WifiStatus::Failed { reason };
                }
                _ => {}
            }
        }
        any
    }
}
