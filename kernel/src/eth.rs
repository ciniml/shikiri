//! The wired Ethernet (doc/58 R2): `netdev:eth0` for the network service.
//! The platform's EMAC driver keeps the frames in its DMA rings; this
//! object reads and writes them whole and reports the PHY's link. The
//! first open starts the MAC; the scheduler loop (`poll`) reads the link
//! every `LINK_POLL_MS` while the device is open.
//!
//! With frame rings mapped (`MAP_RING`, doc/35 §5, doc/58 R5) the scheduler
//! loop copies received frames into them and sends what the process put in
//! them: no `read`/`write` per frame. Their memory is internal SRAM after
//! the EMAC's buffers (`eth_ring_area`): in PSRAM they were slower than no
//! rings at all (13.7 against 15.6 Mbps, doc/58 R5).

use t5_abi::netdev::ring;
use t5_abi::{Error, event, netdev};

use crate::platform::{Current, Platform};
use crate::time;
use crate::wlan::KRings;

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
    rings: Option<KRings>,
    /// The rings' memory (the platform's, at the first `MAP_RING`).
    ring_area: Option<usize>,
}

static mut ETH: Eth = Eth { up: false, open: false, mac: 0, link: 0, changed: false, next_link_poll: 0, rx: 0, tx: 0, tx_busy: 0, stats_tick: 0, moved_logged: 0, rings: None, ring_area: None };

/// One frame on its way from the EMAC into the rings.
static mut FRAME: [u8; netdev::ETH_FRAME] = [0; netdev::ETH_FRAME];

fn e() -> &'static mut Eth {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(ETH) }
}

/// From the scheduler loop: frames between the EMAC and the rings, and the
/// link, while the device is open.
pub fn poll() {
    let e = e();
    if !e.open {
        return;
    }
    move_frames(e);
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

/// Received frames into the rings while they have room, then the frames
/// to send while the EMAC takes them.
fn move_frames(e: &mut Eth) {
    let Some(r) = e.rings.as_mut() else { return };
    let frame = unsafe { &mut *core::ptr::addr_of_mut!(FRAME) };
    while r.rx_waiting() < ring::SLOTS as usize {
        let Some(n) = Current::eth_recv(frame) else { break };
        e.rx += 1;
        r.rx_push(&frame[..n]);
    }
    while let Some(f) = r.tx_front() {
        if f.len() < 14 || f.len() > netdev::ETH_FRAME {
            r.tx_drop_front();
            continue;
        }
        match Current::eth_send(f) {
            Ok(()) => {
                e.tx += 1;
                r.tx_drop_front();
            }
            Err(_) => {
                e.tx_busy += 1;
                break;
            }
        }
    }
}

/// Whether the scheduler loop has frames to move into the rings (it does
/// not sleep then). Only with room in them: frames the process has not
/// taken are no work for the loop (doc/58 R5).
pub fn pending() -> bool {
    let e = e();
    e.open && e.rings.as_ref().is_some_and(|r| r.rx_waiting() < ring::SLOTS as usize) && Current::eth_pending()
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
            netdev::MAP_RING => {
                let base = match e.ring_area {
                    Some(b) => b,
                    None => {
                        let b = Current::eth_ring_area().ok_or(Error::NoSupport)?;
                        e.ring_area = Some(b);
                        b
                    }
                };
                if e.rings.is_none() {
                    e.rings = Some(KRings::new(base));
                    event!(Info, "eth.rings_mapped", base = t5_abi::log::Addr(base as u32));
                }
                Ok(base)
            }
            netdev::KICK => {
                move_frames(e);
                Ok(0)
            }
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let e = e();
        if e.changed {
            return Some((netdev::LINK_CHANGED, e.link));
        }
        match e.rings.as_ref() {
            Some(r) => {
                let n = r.rx_waiting();
                (n > 0).then_some((netdev::READABLE, n))
            }
            None => Current::eth_pending().then_some((netdev::READABLE, 1)),
        }
    }
}

impl Drop for EthDev {
    fn drop(&mut self) {
        let e = e();
        e.open = false;
        e.rings = None;
        event!(Info, "eth.closed", rx = e.rx, tx = e.tx, tx_busy = e.tx_busy);
    }
}
