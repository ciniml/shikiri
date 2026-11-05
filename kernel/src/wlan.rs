//! Wi-Fi (N2b-3, doc/22): the esp_hosted transport to the ESP32-C6 (the
//! platform's SDIO driver) split into three objects. `wlan:ctl` carries the
//! serial interface (RPC) for the Wi-Fi service; `netdev:wlan0` carries the
//! station's Ethernet frames for the network service; `bt:hci` carries the
//! Bluetooth controller's HCI packets (doc/48). The kernel only moves
//! payloads; it does not look into them.
//!
//! The chip is started by the first `wlan:ctl` open and then read from the
//! scheduler loop (`poll`) while either object is open.

use core::sync::atomic::Ordering;

use t5_abi::netdev::ring::{self, Rings};
use t5_abi::{Error, bt, event, netdev, wlan};

use crate::platform::{Current, Platform};

/// esp_hosted interface types (esp_hosted_interface.h:14-23).
const IF_STA: u8 = 1;
/// The soft-AP (ESP_AP_IF, doc/58 R4).
const IF_AP: u8 = 2;
const IF_SERIAL: u8 = 3;
const IF_HCI: u8 = 4;

// Sized by the kernel RAM that is left (internal SRAM; doc/22 §6).
const CTL_QUEUE: usize = 3;
const STA_QUEUE: usize = 5;
const HCI_QUEUE: usize = 6;
const AP_QUEUE: usize = 4;
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

/// `netdev:wlan0`'s frame rings while a process has them mapped (doc/35
/// §5). The kernel's counters live here, in its own memory; the process's
/// (`RX_TAIL`, `TX_HEAD`) and the lengths it writes are read once and
/// checked. Only station frames are copied in: the chip's other packets
/// (the Wi-Fi service's) never reach the shared memory.
struct KRings {
    at: Rings,
    rx_head: u32,
    tx_tail: u32,
    rx_dropped: u32,
}

impl KRings {
    /// Copies a received frame into the next free slot; false if none is
    /// (or the process's counter makes no sense).
    fn rx_push(&mut self, f: &[u8]) -> bool {
        let tail = self.at.word(ring::RX_TAIL).load(Ordering::Acquire);
        if f.len() > ring::SLOT || self.rx_head.wrapping_sub(tail) >= ring::SLOTS {
            self.rx_dropped += 1;
            return false;
        }
        let i = self.rx_head;
        unsafe { core::ptr::copy_nonoverlapping(f.as_ptr(), self.at.slot(ring::RX_DATA, i), f.len()) };
        self.at.word(ring::RX_LEN + (i % ring::SLOTS) as usize * 4).store(f.len() as u32, Ordering::Relaxed);
        self.rx_head = i.wrapping_add(1);
        self.at.word(ring::RX_HEAD).store(self.rx_head, Ordering::Release);
        true
    }

    /// Received frames the process has not taken.
    fn rx_waiting(&self) -> usize {
        let n = self.rx_head.wrapping_sub(self.at.word(ring::RX_TAIL).load(Ordering::Acquire));
        n.min(ring::SLOTS) as usize
    }

    /// The oldest frame to send, left in place. The process may change
    /// the slot meanwhile; that garbles only its own frame.
    fn tx_front(&self) -> Option<&[u8]> {
        let head = self.at.word(ring::TX_HEAD).load(Ordering::Acquire);
        let n = head.wrapping_sub(self.tx_tail);
        if n == 0 || n > ring::SLOTS {
            return None;
        }
        let len = self.at.word(ring::TX_LEN + (self.tx_tail % ring::SLOTS) as usize * 4).load(Ordering::Relaxed) as usize;
        Some(unsafe { core::slice::from_raw_parts(self.at.slot(ring::TX_DATA, self.tx_tail), len.min(ring::SLOT)) })
    }

    fn tx_drop_front(&mut self) {
        self.tx_tail = self.tx_tail.wrapping_add(1);
        self.at.word(ring::TX_TAIL).store(self.tx_tail, Ordering::Release);
    }
}

/// Where the rings go: set at boot when there is room (`set_ring_area`).
static mut RING_AREA: Option<usize> = None;

/// Memory for `netdev:wlan0`'s rings (`ring::SIZE` bytes at `base`), which
/// no process may otherwise reach.
pub fn set_ring_area(base: usize) {
    unsafe { RING_AREA = Some(base) };
    event!(Info, "wlan.ring_area", base = t5_abi::log::Addr(base as u32), size = ring::SIZE as u32);
}

struct Wlan {
    up: bool,
    ctl_open: bool,
    dev_open: bool,
    hci_open: bool,
    ctl: Queue<CTL_QUEUE, { wlan::MSG_MAX }>,
    sta: Queue<STA_QUEUE, { netdev::ETH_FRAME }>,
    /// HCI packets from the controller, with their type byte first.
    hci: Queue<HCI_QUEUE, { bt::PKT_MAX }>,
    hci_rx: u32,
    hci_tx: u32,
    /// The station's frames to send (written on core 1).
    tx: Queue<TX_QUEUE, { netdev::ETH_FRAME }>,
    /// The station's MAC address (set by the Wi-Fi service), 0 = unknown.
    mac: u64,
    /// The soft-AP (doc/58 R4): its MAC (0 = no AP), frames received and
    /// to send, counters.
    ap_mac: u64,
    ap_open: bool,
    ap: Queue<AP_QUEUE, { netdev::ETH_FRAME }>,
    ap_tx: Queue<AP_QUEUE, { netdev::ETH_FRAME }>,
    ap_rx_n: u32,
    ap_tx_n: u32,
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
    rings: Option<KRings>,
}

static mut WLAN: Wlan = Wlan {
    up: false,
    ctl_open: false,
    dev_open: false,
    hci_open: false,
    ctl: Queue::new(),
    sta: Queue::new(),
    hci: Queue::new(),
    hci_rx: 0,
    hci_tx: 0,
    tx: Queue::new(),
    mac: 0,
    ap_mac: 0,
    ap_open: false,
    ap: Queue::new(),
    ap_tx: Queue::new(),
    ap_rx_n: 0,
    ap_tx_n: 0,
    other: 0,
    errors: 0,
    sta_rx: 0,
    sta_tx: 0,
    tx_busy: 0,
    tx_throttled: 0,
    stats_at: 0,
    rings: None,
};

fn w() -> &'static mut Wlan {
    unsafe { &mut *core::ptr::addr_of_mut!(WLAN) }
}

/// Whether the chip has signalled something the next `poll` reads, or
/// frames wait to be sent.
pub fn pending() -> bool {
    let w = w();
    w.up && (w.ctl_open || w.dev_open || w.hci_open || w.ap_open)
        && (Current::wlan_pending() || w.tx.count > 0 || w.ap_tx.count > 0 || w.rings.as_ref().is_some_and(|r| r.tx_front().is_some()))
}

/// Sends the queued frames, oldest first, while the bus is free: one frame
/// goes out while the caller goes on, and the end of its DMA (the SDIO
/// interrupt, on core 0) sends the next (doc/37). Also stops when the chip
/// takes no more (`Busy`: they stay for the next call). Core 0 does this; a
/// frame that fails otherwise is dropped and counted.
pub fn flush_tx() {
    let w = w();
    while let Some(f) = w.tx.front() {
        if !Current::wlan_tx_idle() {
            return;
        }
        match send(IF_STA, f) {
            Ok(_) => w.sta_tx += 1,
            Err(Error::Busy) => return,
            Err(_) => w.tx.dropped += 1,
        }
        w.tx.drop_front();
    }
    // The soft-AP's frames (doc/58 R4).
    while let Some(f) = w.ap_tx.front() {
        if !Current::wlan_tx_idle() {
            return;
        }
        match send(IF_AP, f) {
            Ok(_) => w.ap_tx_n += 1,
            Err(Error::Busy) => return,
            Err(_) => w.ap_tx.dropped += 1,
        }
        w.ap_tx.drop_front();
    }
    // Then the transmit ring.
    let Some(r) = w.rings.as_mut() else { return };
    while let Some(f) = r.tx_front() {
        if !Current::wlan_tx_idle() {
            return;
        }
        let sent = if f.len() > netdev::ETH_FRAME || f.is_empty() { Err(Error::Size) } else { send(IF_STA, f) };
        match sent {
            Ok(_) => w.sta_tx += 1,
            Err(Error::Busy) => return,
            Err(_) => w.tx.dropped += 1,
        }
        r.tx_drop_front();
    }
}

/// Reads what the chip has sent (from the scheduler loop).
pub fn poll() {
    let w = w();
    if !w.up || !(w.ctl_open || w.dev_open || w.hci_open || w.ap_open) {
        return;
    }
    flush_tx();
    let (ctl, sta, dev_open, rings) = (&mut w.ctl, &mut w.sta, w.dev_open, &mut w.rings);
    let (ap, ap_open) = (&mut w.ap, w.ap_open);
    let mut ap_rx = 0;
    let (hci, hci_open, hci_seen) = (&mut w.hci, w.hci_open, w.hci_rx);
    let (mut other, mut sta_rx, mut hci_rx) = (0, 0, 0);
    let r = Current::wlan_poll(&mut |if_type, p| match if_type {
        IF_SERIAL => ctl.push(p),
        IF_STA => {
            if w.sta_rx + sta_rx < 4 {
                event!(Debug, "wlan.sta_rx", len = p.len() as u32, open = dev_open, head = &p[..p.len().min(48)]);
            }
            sta_rx += 1;
            if let Some(r) = rings.as_mut() {
                r.rx_push(p);
            } else if dev_open {
                sta.push(p);
            }
        }
        IF_AP => {
            ap_rx += 1;
            if ap_open {
                ap.push(p);
            }
        }
        // The payload starts with the packet type (slave_bt.c:41-72).
        IF_HCI => {
            if hci_seen + hci_rx < 4 {
                event!(Debug, "wlan.hci_rx", len = p.len() as u32, open = hci_open, head = &p[..p.len().min(32)]);
            }
            hci_rx += 1;
            if hci_open {
                hci.push(p);
            }
        }
        _ => other += 1,
    });
    w.other += other;
    w.ap_rx_n += ap_rx;
    w.hci_rx += hci_rx;
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
    send_typed(if_type, 0, data)
}

fn send_typed(if_type: u8, pkt_type: u8, data: &[u8]) -> Result<usize, Error> {
    match Current::wlan_send(if_type, pkt_type, data) {
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
            wlan::SET_AP_MAC => {
                w().ap_mac = arg & 0xFFFF_FFFF_FFFF;
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

    /// The frame is queued and the caller does not wait for the bus. On
    /// core 1 (with two cores) core 0 sends it, which owns the SDIO
    /// transfers then (doc/34 §11); on core 0 it goes now if the bus is
    /// free, else when the frame before it is out (doc/37).
    pub fn write(&mut self, frame: &[u8]) -> Result<usize, Error> {
        let w = w();
        if !w.up {
            return Err(Error::Busy);
        }
        if frame.len() > netdev::ETH_FRAME {
            return Err(Error::Size);
        }
        if w.tx.count == TX_QUEUE {
            // Full: send here rather than drop.
            flush_tx();
            if w.tx.count == TX_QUEUE {
                return Err(Error::Busy);
            }
        }
        if w.sta_tx < 4 {
            event!(Debug, "wlan.sta_tx", len = frame.len() as u32, head = &frame[frame.len().min(14)..frame.len().min(76)]);
        }
        let was_empty = w.tx.count == 0;
        w.tx.push(frame);
        if !(crate::smp::two_cores() && crate::smp::hartid() & 1 == 1) {
            flush_tx();
        } else if was_empty {
            // Core 0 sends it from its loop, or from the interrupt if it
            // runs a process.
            Current::ipi_send(0);
        }
        Ok(frame.len())
    }

    pub fn ctl(&mut self, op: usize, _arg: u64) -> Result<usize, Error> {
        let w = w();
        let mac = w.mac;
        match op {
            _ if mac == 0 && (op == netdev::MAC_LO || op == netdev::MAC_HI) => Err(Error::Busy),
            netdev::MAC_LO => Ok(mac as u32 as usize),
            netdev::MAC_HI => Ok((mac >> 32) as usize),
            netdev::MAP_RING => {
                let base = unsafe { RING_AREA }.ok_or(Error::NoSupport)?;
                if w.rings.is_none() {
                    // Nothing an earlier owner left may show.
                    unsafe { core::ptr::write_bytes(base as *mut u8, 0, ring::SIZE) };
                    w.rings = Some(KRings { at: Rings(base), rx_head: 0, tx_tail: 0, rx_dropped: 0 });
                    event!(Info, "wlan.rings_mapped", base = t5_abi::log::Addr(base as u32));
                }
                Ok(base)
            }
            netdev::KICK => {
                if crate::smp::two_cores() && crate::smp::hartid() & 1 == 1 {
                    // Core 0 sends them, as `write`'s queue.
                    Current::ipi_send(0);
                } else {
                    flush_tx();
                }
                Ok(0)
            }
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let w = w();
        let n = w.rings.as_ref().map_or(w.sta.count, KRings::rx_waiting);
        (n > 0).then_some((netdev::READABLE, n))
    }
}

impl Drop for WlanDev {
    fn drop(&mut self) {
        let w = w();
        w.dev_open = false;
        w.tx.clear();
        let ring_dropped = w.rings.take().map_or(0, |r| r.rx_dropped);
        event!(Info, "wlan.dev_closed", rx = w.sta_rx, tx = w.sta_tx, dropped = w.sta.dropped + ring_dropped, tx_dropped = w.tx.dropped);
    }
}

/// The handle on `netdev:ap0` (doc/58 R4): the soft-AP's frames, once the
/// Wi-Fi service has started the AP. Like `netdev:wlan0` without rings.
pub struct ApDev;

impl ApDev {
    pub fn open() -> Result<ApDev, Error> {
        let w = w();
        if !w.up || w.ap_mac == 0 {
            return Err(Error::NoDevice);
        }
        if w.ap_open {
            return Err(Error::Busy);
        }
        w.ap.clear();
        w.ap_tx.clear();
        w.ap_open = true;
        Ok(ApDev)
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        w().ap.pop(out)
    }

    /// Queued; core 0 sends it (now, if this is core 0 and the bus is
    /// free), as `WlanDev::write`.
    pub fn write(&mut self, frame: &[u8]) -> Result<usize, Error> {
        let w = w();
        if frame.len() > netdev::ETH_FRAME || frame.len() < 14 {
            return Err(Error::Size);
        }
        if w.ap_tx.count == AP_QUEUE {
            flush_tx();
            if w.ap_tx.count == AP_QUEUE {
                return Err(Error::Busy);
            }
        }
        let was_empty = w.ap_tx.count == 0;
        w.ap_tx.push(frame);
        if !(crate::smp::two_cores() && crate::smp::hartid() & 1 == 1) {
            flush_tx();
        } else if was_empty {
            Current::ipi_send(0);
        }
        Ok(frame.len())
    }

    pub fn ctl(&mut self, op: usize, _arg: u64) -> Result<usize, Error> {
        let mac = w().ap_mac;
        match op {
            netdev::MAC_LO => Ok(mac as u32 as usize),
            netdev::MAC_HI => Ok((mac >> 32) as usize),
            _ => Err(Error::NoSupport),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = w().ap.count;
        (n > 0).then_some((netdev::READABLE, n))
    }
}

impl Drop for ApDev {
    fn drop(&mut self) {
        let w = w();
        w.ap_open = false;
        w.ap_tx.clear();
        event!(Info, "wlan.ap_closed", rx = w.ap_rx_n, tx = w.ap_tx_n, dropped = w.ap.dropped, tx_dropped = w.ap_tx.dropped);
    }
}

/// The handle on `bt:hci` (doc/48); one at a time.
pub struct BtHci;

impl BtHci {
    /// Only once the chip runs. Whether its controller is started is the
    /// Wi-Fi service's business; before that, nothing answers.
    pub fn open() -> Result<BtHci, Error> {
        let w = w();
        if !w.up {
            return Err(Error::NoDevice);
        }
        if w.hci_open {
            return Err(Error::Busy);
        }
        w.hci.clear();
        w.hci_open = true;
        Ok(BtHci)
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        w().hci.pop(out)
    }

    /// One packet, type byte first: the type goes into the transport
    /// header and the rest after it. `Busy` when the chip has no room.
    pub fn write(&mut self, pkt: &[u8]) -> Result<usize, Error> {
        let w = w();
        if !w.up {
            return Err(Error::Busy);
        }
        match pkt.first() {
            Some(&t) if (bt::CMD..=bt::ISO).contains(&t) && pkt.len() <= bt::PKT_MAX => {
                send_typed(IF_HCI, t, &pkt[1..])?;
                w.hci_tx += 1;
                Ok(pkt.len())
            }
            Some(_) => Err(Error::Invalid),
            None => Err(Error::Size),
        }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = w().hci.count;
        (n > 0).then_some((netdev::READABLE, n))
    }
}

impl Drop for BtHci {
    fn drop(&mut self) {
        let w = w();
        w.hci_open = false;
        event!(Info, "wlan.hci_closed", rx = w.hci_rx, tx = w.hci_tx, dropped = w.hci.dropped);
    }
}
