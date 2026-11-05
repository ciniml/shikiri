//! `capture` (doc/54): sends the framebuffer on the console line as
//! `t5_abi::capture` frames, a little at a time from the scheduler loop so
//! the processes keep running. The pixels are read as they are at the time
//! (no copy): a screen that changes meanwhile comes out mixed.

use t5_abi::capture as wire;
use t5_abi::event;

use crate::platform::{Current, Platform};
use crate::{screen, time};

/// Time spent sending per call (the loop runs a process slice in between).
const BUDGET_US: u64 = 1000;

struct Capture {
    active: bool,
    id: u32,
    fb: usize,
    size: (u16, u16),
    started: bool,
    next: usize,
    crc: u32,
    bytes: u32,
    since: u64,
    /// Scratch (static: the kernel stack is small).
    plain: [u8; wire::DATA_MAX],
    out: [u8; wire::DATA_MAX + wire::DATA_MAX / 254 + 2],
}

static mut CAP: Capture = Capture {
    active: false,
    id: 0,
    fb: 0,
    size: (0, 0),
    started: false,
    next: 0,
    crc: 0,
    bytes: 0,
    since: 0,
    plain: [0; wire::DATA_MAX],
    out: [0; wire::DATA_MAX + wire::DATA_MAX / 254 + 2],
};

fn cap() -> &'static mut Capture {
    // Core 0, under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(CAP) }
}

/// The `capture` command: starts sending; the result is the id.
pub fn start() -> Result<u32, &'static str> {
    let c = cap();
    if c.active {
        return Err("capture in progress");
    }
    let (fb, _) = Current::display_buffer().ok_or("no display")?;
    let size = Current::display_size().ok_or("no display")?;
    c.id = c.id.wrapping_add(1);
    c.active = true;
    c.fb = fb;
    c.size = size;
    c.started = false;
    c.next = 0;
    c.crc = 0;
    c.bytes = 0;
    c.since = Current::now();
    Ok(c.id)
}

/// Whether frames are still to be sent (the loop does not sleep then).
pub fn pending() -> bool {
    cap().active
}

/// Sends frames for up to `BUDGET_US` (from the scheduler loop).
pub fn poll() {
    let c = cap();
    if !c.active {
        return;
    }
    let deadline = Current::now() + time::ns_to_ticks(BUDGET_US * 1000);
    let pixels = c.size.0 as usize * c.size.1 as usize;
    while c.active && Current::now() < deadline {
        let id = c.id.to_le_bytes();
        c.plain[0] = wire::FRAME;
        c.plain[2..6].copy_from_slice(&id);
        let n = if !c.started {
            c.plain[1] = wire::START;
            c.plain[6..8].copy_from_slice(&c.size.0.to_le_bytes());
            c.plain[8..10].copy_from_slice(&c.size.1.to_le_bytes());
            c.plain[10] = screen::orientation() as u8;
            c.plain[11] = wire::RGB565;
            c.started = true;
            12
        } else if c.next < pixels {
            let k = wire::CHUNK.min(pixels - c.next);
            // The framebuffer is in PSRAM, written by the processes and the
            // 2D DMA (which leaves the caches in agreement, doc/44).
            let px = unsafe { core::slice::from_raw_parts((c.fb as *const u16).add(c.next), k) };
            let raw = unsafe { core::slice::from_raw_parts(px.as_ptr().cast::<u8>(), 2 * k) };
            c.crc = wire::crc32(c.crc, raw);
            c.plain[1] = wire::DATA;
            c.plain[6..10].copy_from_slice(&(c.next as u32).to_le_bytes());
            c.next += k;
            10 + wire::encode_runs(px, &mut c.plain[10..])
        } else {
            c.plain[1] = wire::END;
            c.plain[6..10].copy_from_slice(&(pixels as u32).to_le_bytes());
            c.plain[10..14].copy_from_slice(&c.crc.to_le_bytes());
            c.active = false;
            14
        };
        let Some(m) = t5_abi::log::cobs_encode(&c.plain[..n], &mut c.out) else {
            c.active = false;
            event!(Error, "capture.failed", id = c.id, error = "frame too long");
            return;
        };
        if !Current::console_write(&c.out[..m]) {
            // No one reads the line: the frames would be lost.
            c.active = false;
            event!(Warn, "capture.failed", id = c.id, error = "console line stalled", pixel = c.next as u32);
            return;
        }
        c.bytes += m as u32;
    }
    if !c.active {
        let ms = (time::ticks_to_ns(Current::now() - c.since) / 1_000_000) as u32;
        event!(Info, "capture.done", id = c.id, width = c.size.0, height = c.size.1, bytes = c.bytes, ms = ms, crc = c.crc);
    }
}
