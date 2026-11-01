//! Wi-Fi (N2b-3, doc/22): the esp_hosted transport to the ESP32-C6 (the
//! platform's SDIO driver) split into two objects. `wlan:ctl` carries the
//! serial interface (RPC) for the Wi-Fi service; `netdev:wlan0` carries the
//! station's Ethernet frames for the network service. The kernel only
//! moves payloads; it does not look into them.
//!
//! The chip is started by the first `wlan:ctl` open and then read from the
//! scheduler loop (`poll`) while either object is open.

use t5_abi::{Error, event, netdev, wlan};

use crate::platform::{Current, Platform};

/// esp_hosted interface types (esp_hosted_interface.h:14-23).
const IF_STA: u8 = 1;
const IF_SERIAL: u8 = 3;

// Sized by the kernel RAM that is left (internal SRAM; doc/22 §6).
const CTL_QUEUE: usize = 3;
const STA_QUEUE: usize = 5;
/// Frames written on core 1, waiting for core 0 to send them (doc/34 §11).
/// QEMU has no Wi-Fi, and less RAM.
#[cfg(not(feature = "platform-virt"))]
const TX_QUEUE: usize = 8;
#[cfg(feature = "platform-virt")]
const TX_QUEUE: usize = 1;
/// How often the counters are logged.
const STATS_MS: u64 = 30_000;

/// Received payloads waiting for `read`.
struct Queue<const N: usize, const S: usize> {
    items: [[u8; S]; N],
    lens: [usize; N],
    head: usize,
    count: usize,
    dropped: u32,
}

impl<const N: usize, const S: usize> Queue<N, S> {
    const fn new() -> Self {
        Queue { items: [[0; S]; N], lens: [0; N], head: 0, count: 0, dropped: 0 }
    }

    fn push(&mut self, p: &[u8]) {
        if self.count == N || p.len() > S {
            self.dropped += 1;
            return;
        }
        let i = (self.head + self.count) % N;
        self.items[i][..p.len()].copy_from_slice(p);
        self.lens[i] = p.len();
        self.count += 1;
    }

    fn pop(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        if self.count == 0 {
            return Err(Error::Busy);
        }
        let n = self.lens[self.head];
        if n > out.len() {
            return Err(Error::Size);
        }
        out[..n].copy_from_slice(&self.items[self.head][..n]);
        self.head = (self.head + 1) % N;
        self.count -= 1;
        Ok(n)
    }

    fn clear(&mut self) {
        self.count = 0;
    }

    /// The oldest item, left in place.
    fn front(&self) -> Option<&[u8]> {
        (self.count > 0).then(|| &self.items[self.head][..self.lens[self.head]])
    }

    fn drop_front(&mut self) {
        if self.count > 0 {
            self.head = (self.head + 1) % N;
            self.count -= 1;
        }
    }
}

struct Wlan {
    up: bool,
    ctl_open: bool,
    dev_open: bool,
    ctl: Queue<CTL_QUEUE, { wlan::MSG_MAX }>,
    sta: Queue<STA_QUEUE, { netdev::ETH_FRAME }>,
    /// The station's frames to send (written on core 1).
    tx: Queue<TX_QUEUE, { netdev::ETH_FRAME }>,
    /// The station's MAC address (set by the Wi-Fi service), 0 = unknown.
    mac: u64,
    other: u32,
    errors: u32,
    sta_rx: u32,
    sta_tx: u32,
    /// Sends refused for want of a C6 buffer, and while the C6 asked us to
    /// hold back (flow control).
    tx_busy: u32,
    tx_throttled: u32,
    /// When to log the counters next (ticks).
    stats_at: u64,
}

static mut WLAN: Wlan = Wlan {
    up: false,
    ctl_open: false,
    dev_open: false,
    ctl: Queue::new(),
    sta: Queue::new(),
    tx: Queue::new(),
    mac: 0,
    other: 0,
    errors: 0,
    sta_rx: 0,
    sta_tx: 0,
    tx_busy: 0,
    tx_throttled: 0,
    stats_at: 0,
};

fn w() -> &'static mut Wlan {
    unsafe { &mut *core::ptr::addr_of_mut!(WLAN) }
}

/// Whether the chip has signalled something the next `poll` reads, or
/// frames wait to be sent.
pub fn pending() -> bool {
    let w = w();
    w.up && (w.ctl_open || w.dev_open) && (Current::wlan_pending() || w.tx.count > 0)
}

/// Sends the queued frames, oldest first, until the chip takes no more
/// (`Busy`: they stay for the next call). Core 0 does this; a frame that
/// fails otherwise is dropped and counted.
pub fn flush_tx() {
    let w = w();
    while let Some(f) = w.tx.front() {
        match send(IF_STA, f) {
            Ok(_) => w.sta_tx += 1,
            Err(Error::Busy) => return,
            Err(_) => w.tx.dropped += 1,
        }
        w.tx.drop_front();
    }
}

/// Reads what the chip has sent (from the scheduler loop).
pub fn poll() {
    let w = w();
    if !w.up || !(w.ctl_open || w.dev_open) {
        return;
    }
    flush_tx();
    let (ctl, sta, dev_open) = (&mut w.ctl, &mut w.sta, w.dev_open);
    let (mut other, mut sta_rx) = (0, 0);
    let r = Current::wlan_poll(&mut |if_type, p| match if_type {
        IF_SERIAL => ctl.push(p),
        IF_STA => {
            if w.sta_rx + sta_rx < 4 {
                event!(Debug, "wlan.sta_rx", len = p.len() as u32, open = dev_open, head = &p[..p.len().min(48)]);
            }
            sta_rx += 1;
            if dev_open {
                sta.push(p);
            }
        }
        _ => other += 1,
    });
    w.other += other;
    w.sta_rx += sta_rx;
    let now = Current::now();
    if now >= w.stats_at {
        if w.stats_at != 0 {
            let (irqs, d1_low, bus_ms, bus_cmds) = Current::wlan_irq_stats();
            event!(Debug, "wlan.stats", irqs = irqs, d1_low = d1_low, sta_rx = w.sta_rx, sta_tx = w.sta_tx, tx_busy = w.tx_busy, tx_throttled = w.tx_throttled, errors = w.errors, bus_ms = bus_ms, bus_cmds = bus_cmds);
        }
        w.stats_at = now + crate::time::ms_to_ticks(STATS_MS);
    }
    if let Err(e) = r {
        w.errors += 1;
        // Rate-limited: the first errors and then every 1000th.
        if w.errors <= 3 || w.errors % 1000 == 0 {
            event!(Warn, "wlan.poll_failed", error = e, count = w.errors);
        }
    }
}

fn send(if_type: u8, data: &[u8]) -> Result<usize, Error> {
    match Current::wlan_send(if_type, data) {
        Ok(()) => Ok(data.len()),
        Err("busy") => {
            w().tx_busy += 1;
            Err(Error::Busy)
        }
        Err("throttled") => {
            w().tx_throttled += 1;
            Err(Error::Busy)
        }
        Err("too long") => Err(Error::Size),
        Err(e) => {
            event!(Warn, "wlan.send_failed", error = e, len = data.len() as u32);
            Err(Error::NoDevice)
        }
    }
}

/// The handle on `wlan:ctl`; one at a time.
pub struct WlanCtl;

impl WlanCtl {
    pub fn open() -> Result<WlanCtl, Error> {
        let w = w();
        if w.ctl_open {
            return Err(Error::Busy);
        }
        if !w.up {
            if let Err(e) = Current::wlan_up() {
                event!(Error, "wlan.up_failed", error = e);
                return Err(Error::NoDevice);
            }
            w.up = true;
            event!(Info, "wlan.up");
        }
        w.ctl.clear();
        w.ctl_open = true;
        Ok(WlanCtl)
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        w().ctl.pop(out)
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, Error> {
        send(IF_SERIAL, data)
    }

    pub fn ctl(&mut self, op: usize, arg: u64) -> Result<usize, Error> {
        match op {
            wlan::SET_MAC => {
                w().mac = arg & 0xFFFF_FFFF_FFFF;
                Ok(0)
            }
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = w().ctl.count;
        (n > 0).then_some((netdev::READABLE, n))
    }
}

impl Drop for WlanCtl {
    fn drop(&mut self) {
        let w = w();
        w.ctl_open = false;
        event!(Info, "wlan.ctl_closed", dropped = w.ctl.dropped, other = w.other, errors = w.errors);
    }
}

/// The handle on `netdev:wlan0`; one at a time.
pub struct WlanDev;

impl WlanDev {
    /// Only once the chip runs (the Wi-Fi service has opened `wlan:ctl`).
    pub fn open() -> Result<WlanDev, Error> {
        let w = w();
        if !w.up {
            return Err(Error::NoDevice);
        }
        if w.dev_open {
            return Err(Error::Busy);
        }
        w.sta.clear();
        w.dev_open = true;
        Ok(WlanDev)
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        w().sta.pop(out)
    }

    /// On core 1 (with two cores) the frame is queued for core 0, which
    /// owns the SDIO transfers then: the caller does not wait for the bus
    /// (doc/34 §11). On core 0 the queue is sent first, then the frame.
    pub fn write(&mut self, frame: &[u8]) -> Result<usize, Error> {
        let w = w();
        if !w.up {
            return Err(Error::Busy);
        }
        if frame.len() > netdev::ETH_FRAME {
            return Err(Error::Size);
        }
        if crate::smp::two_cores() && crate::smp::hartid() & 1 == 1 {
            if w.tx.count == TX_QUEUE {
                // Full: send here rather than drop.
                flush_tx();
                if w.tx.count == TX_QUEUE {
                    return Err(Error::Busy);
                }
            }
            let was_empty = w.tx.count == 0;
            w.tx.push(frame);
            if was_empty {
                // Core 0 sends it from its loop, or from the interrupt if
                // it runs a process.
                Current::ipi_send(0);
            }
            return Ok(frame.len());
        }
        flush_tx();
        if w.tx.count > 0 {
            return Err(Error::Busy);
        }
        let r = send(IF_STA, frame);
        if w.sta_tx < 4 {
            event!(Debug, "wlan.sta_tx", len = frame.len() as u32, ok = r.is_ok(), head = &frame[frame.len().min(14)..frame.len().min(76)]);
        }
        if r.is_ok() {
            w.sta_tx += 1;
        }
        r
    }

    pub fn ctl(&mut self, op: usize, _arg: u64) -> Result<usize, Error> {
        let mac = w().mac;
        match op {
            _ if mac == 0 && (op == netdev::MAC_LO || op == netdev::MAC_HI) => Err(Error::Busy),
            netdev::MAC_LO => Ok(mac as u32 as usize),
            netdev::MAC_HI => Ok((mac >> 32) as usize),
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = w().sta.count;
        (n > 0).then_some((netdev::READABLE, n))
    }
}

impl Drop for WlanDev {
    fn drop(&mut self) {
        let w = w();
        w.dev_open = false;
        w.tx.clear();
        event!(Info, "wlan.dev_closed", rx = w.sta_rx, tx = w.sta_tx, dropped = w.sta.dropped, tx_dropped = w.tx.dropped);
    }
}
