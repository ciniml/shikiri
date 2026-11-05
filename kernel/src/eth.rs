//! The wired Ethernet (doc/58 R2): `netdev:eth0` for the network service.
//! The platform's EMAC driver keeps the frames in its DMA rings; this
//! object reads and writes them whole and reports the PHY's link. The
//! first open starts the MAC; the scheduler loop (`poll`) reads the link
//! every `LINK_POLL_MS` while the device is open.

use t5_abi::{Error, event, netdev};

use crate::platform::{Current, Platform};
use crate::time;

const LINK_POLL_MS: u64 = 500;

struct Eth {
    up: bool,
    open: bool,
    mac: u64,
    link: usize,
    /// The link changed since the owner last asked (`ctl(LINK)`).
    changed: bool,
    next_link_poll: u64,
    rx: u32,
    tx: u32,
    tx_busy: u32,
    stats_tick: u32,
    moved_logged: u32,
}

static mut ETH: Eth = Eth { up: false, open: false, mac: 0, link: 0, changed: false, next_link_poll: 0, rx: 0, tx: 0, tx_busy: 0, stats_tick: 0, moved_logged: 0 };

fn e() -> &'static mut Eth {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(ETH) }
}

/// From the scheduler loop: the link, while the device is open.
pub fn poll() {
    let e = e();
    if !e.open {
        return;
    }
    let now = Current::now();
    if now < e.next_link_poll {
        return;
    }
    e.next_link_poll = now + time::ms_to_ticks(LINK_POLL_MS);
    // Every 10 s while frames move: the counters (as `wlan.stats`).
    e.stats_tick += 1;
    if e.stats_tick % 20 == 0 && e.rx + e.tx != e.moved_logged {
        e.moved_logged = e.rx + e.tx;
        let (irqs, status, missed, overflowed) = Current::eth_stats();
        let [bad, crc, rx_err, split, last] = Current::eth_rx_errors();
        event!(
            Debug,
            "eth.stats",
            irqs = irqs,
            rx = e.rx,
            tx = e.tx,
            tx_busy = e.tx_busy,
            dma_status = status,
            missed = missed,
            overflowed = overflowed,
            rx_bad = bad,
            crc = crc,
            rx_err = rx_err,
            split = split,
            bad_status = last
        );
    }
    let link = Current::eth_link();
    if link != e.link {
        event!(Info, "eth.link", mbps = (link & 0xFFFF) as u32, full = link & netdev::LINK_FULL != 0, up = link != 0);
        e.link = link;
        e.changed = true;
    }
}

pub struct EthDev;

impl EthDev {
    pub fn open() -> Result<EthDev, Error> {
        let e = e();
        if e.open {
            return Err(Error::Busy);
        }
        if !e.up {
            match Current::eth_up() {
                Ok(mac) => {
                    e.mac = mac;
                    e.up = true;
                    event!(Info, "eth.up", mac = mac);
                }
                Err(why) => {
                    event!(Warn, "eth.up_failed", error = why);
                    return Err(Error::NoDevice);
                }
            }
        }
        // Frames left from before are dropped.
        let mut scratch = [0u8; netdev::ETH_FRAME];
        while Current::eth_recv(&mut scratch).is_some() {}
        e.open = true;
        e.link = 0;
        e.changed = false;
        e.next_link_poll = 0;
        Ok(EthDev)
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        if out.len() < netdev::ETH_FRAME {
            return Err(Error::Size);
        }
        let n = Current::eth_recv(out).ok_or(Error::Busy)?;
        e().rx += 1;
        Ok(n)
    }

    pub fn write(&mut self, frame: &[u8]) -> Result<usize, Error> {
        if frame.len() > netdev::ETH_FRAME || frame.len() < 14 {
            return Err(Error::Size);
        }
        let e = e();
        match Current::eth_send(frame) {
            Ok(()) => {
                e.tx += 1;
                Ok(frame.len())
            }
            Err(_) => {
                e.tx_busy += 1;
                Err(Error::Busy)
            }
        }
    }

    pub fn ctl(&mut self, op: usize, _arg: u64) -> Result<usize, Error> {
        let e = e();
        match op {
            netdev::MAC_LO => Ok(e.mac as u32 as usize),
            netdev::MAC_HI => Ok((e.mac >> 32) as usize),
            netdev::LINK => {
                e.changed = false;
                Ok(e.link)
            }
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let e = e();
        if e.changed {
            Some((netdev::LINK_CHANGED, e.link))
        } else if Current::eth_pending() {
            Some((netdev::READABLE, 1))
        } else {
            None
        }
    }
}

impl Drop for EthDev {
    fn drop(&mut self) {
        let e = e();
        e.open = false;
        event!(Info, "eth.closed", rx = e.rx, tx = e.tx, tx_busy = e.tx_busy);
    }
}
