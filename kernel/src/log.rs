//! The kernel side of structured logging (doc/12 §2.4): the sink behind
//! `event!`, the record ring, the RTT up-channel and the console output.
//!
//! Every record gets a sequence number, a timestamp and its source here, so
//! processes cannot forge them. Records are COBS frames, which lets a reader
//! resynchronise at any 0x00. The kernel runs with interrupts off, so
//! nothing here needs locking.
//!
//! The ring, the RTT control block and its buffer live in `.log_mem`, which
//! the linker puts in LP SRAM on the ESP32-P4 (uncached, readable by a
//! debugger without halting the core, kept across system resets) and in RAM
//! on QEMU virt. The section is not loaded, so `init` sets everything up.
//!
//! The ring has a header (`RingHeader`). If the header left by the previous
//! boot is intact, `init` replays that boot's frames to the outputs between
//! `log.prev_begin` and `log.prev_end` before starting a new ring, so the
//! records leading up to a crash, lock-up or watchdog reset are recovered
//! (doc/12 L3).

use core::mem::MaybeUninit;
use core::sync::atomic::{Ordering, fence};

use t5_abi::event;
use t5_abi::log::{self as wire, Level};

use crate::platform::{Current, Platform};
use crate::time;

/// Records below this level are dropped (doc/12 §2.1). Returned by the
/// `log_level` syscall.
pub const KEEP: Level = Level::Trace;

const FRAME_MAX: usize = wire::MAX_RECORD + wire::MAX_RECORD / 254 + 2;
const RING_SIZE: usize = 16 * 1024;
const RTT_UP_SIZE: usize = 8 * 1024;
const RTT_DOWN_SIZE: usize = 16;

/// SEGGER RTT buffer descriptor (SEGGER_RTT_BUFFER_UP/DOWN in SEGGER_RTT.h).
#[repr(C)]
struct RttBuffer {
    name: *const u8,
    buf: *mut u8,
    size: u32,
    /// Written by the target only (up) / by the host only (down).
    wr_off: u32,
    /// Written by the host only (up) / by the target only (down).
    rd_off: u32,
    /// 0 = SEGGER_RTT_MODE_NO_BLOCK_SKIP: a frame that does not fit is
    /// dropped whole, never cut.
    flags: u32,
}

/// SEGGER RTT control block. Debuggers find it through the `_SEGGER_RTT`
/// symbol (probe-rs) or by scanning for the id (OpenOCD `rtt setup`).
#[repr(C)]
pub struct RttControlBlock {
    id: [u8; 16],
    max_up: u32,
    max_down: u32,
    up: [RttBuffer; 1],
    down: [RttBuffer; 1],
}

#[unsafe(link_section = ".log_mem")]
#[unsafe(no_mangle)]
static mut _SEGGER_RTT: MaybeUninit<RttControlBlock> = MaybeUninit::uninit();

#[unsafe(link_section = ".log_mem")]
static mut RTT_NAME: MaybeUninit<[u8; 8]> = MaybeUninit::uninit();
#[unsafe(link_section = ".log_mem")]
static mut RTT_UP: MaybeUninit<[u8; RTT_UP_SIZE]> = MaybeUninit::uninit();
#[unsafe(link_section = ".log_mem")]
static mut RTT_DOWN: MaybeUninit<[u8; RTT_DOWN_SIZE]> = MaybeUninit::uninit();
#[unsafe(link_section = ".log_mem")]
static mut RING: MaybeUninit<[u8; RING_SIZE]> = MaybeUninit::uninit();

/// Header of the ring, updated with every record. `check` covers the other
/// fields, so power-on garbage is not mistaken for a ring.
#[repr(C)]
#[derive(Clone, Copy)]
struct RingHeader {
    magic: u32,
    boot_id: u32,
    write: u32,
    wrapped: u32,
    check: u32,
}

const RING_MAGIC: u32 = u32::from_le_bytes(*b"T5LR");

impl RingHeader {
    fn checksum(&self) -> u32 {
        let mut b = [0u8; 16];
        for (i, v) in [self.magic, self.boot_id, self.write, self.wrapped].iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        wire::fnv1a(&b)
    }

    fn valid(&self) -> bool {
        self.magic == RING_MAGIC
            && (self.write as usize) < RING_SIZE
            && self.wrapped <= 1
            && self.check == self.checksum()
    }
}

#[unsafe(link_section = ".log_mem")]
static mut HEADER: MaybeUninit<RingHeader> = MaybeUninit::uninit();

fn header() -> *mut RingHeader {
    core::ptr::addr_of_mut!(HEADER).cast()
}

struct State {
    /// False while the previous boot's frames are being replayed: the ring
    /// still holds them.
    ring_on: bool,
    boot_id: u32,
    ring_write: usize,
    ring_wrapped: bool,
    seq: u64,
    /// Frames the RTT channel had no room for, not yet reported.
    rtt_dropped: u32,
    /// Frames the console could not take whole (no reader), not yet
    /// reported.
    console_dropped: u32,
    /// Set while reporting drops, so the report cannot recurse.
    reporting: bool,
}

static mut STATE: State = State {
    ring_on: false,
    boot_id: 0,
    ring_write: 0,
    ring_wrapped: false,
    seq: 0,
    rtt_dropped: 0,
    console_dropped: 0,
    reporting: false,
};

fn state() -> &'static mut State {
    // Single core, interrupts off in the kernel: no concurrent access.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

fn rtt() -> *mut RttControlBlock {
    core::ptr::addr_of_mut!(_SEGGER_RTT).cast()
}

/// Sets up the RTT control block, replays the previous boot's ring if it
/// survived, and starts a new ring. Call once, before the first event.
/// Returns this boot's id (1 after power-on, then counting up).
pub fn init() -> u32 {
    unsafe {
        let name = core::ptr::addr_of_mut!(RTT_NAME).cast::<u8>();
        core::ptr::copy_nonoverlapping(b"t5log\0".as_ptr(), name, 6);
        let cb = rtt();
        let fields = RttControlBlock {
            id: [0; 16],
            max_up: 1,
            max_down: 1,
            up: [RttBuffer {
                name,
                buf: core::ptr::addr_of_mut!(RTT_UP).cast(),
                size: RTT_UP_SIZE as u32,
                wr_off: 0,
                rd_off: 0,
                flags: 0,
            }],
            down: [RttBuffer {
                name,
                buf: core::ptr::addr_of_mut!(RTT_DOWN).cast(),
                size: RTT_DOWN_SIZE as u32,
                wr_off: 0,
                rd_off: 0,
                flags: 0,
            }],
        };
        core::ptr::write_volatile(cb, fields);
        // The id goes last, as in SEGGER_RTT.c, so a debugger scanning
        // memory never finds a half-initialised block.
        fence(Ordering::SeqCst);
        let id = core::ptr::addr_of_mut!((*cb).id).cast::<u8>();
        for (i, &b) in b"SEGGER RTT\0\0\0\0\0\0".iter().enumerate() {
            core::ptr::write_volatile(id.add(i), b);
        }
    }
    // A lone 0x00 ends whatever came before on the console (the bootloader's
    // text), so the first record starts a clean frame.
    let _ = Current::console_write(&[0]);

    let prev = unsafe { core::ptr::read_volatile(header()) };
    let boot_id = if prev.valid() {
        replay(&prev);
        prev.boot_id.wrapping_add(1)
    } else {
        1
    };
    let st = state();
    st.boot_id = boot_id;
    st.ring_write = 0;
    st.ring_wrapped = false;
    st.ring_on = true;
    save_header();
    boot_id
}

/// Sends the previous boot's frames, oldest first, to the outputs (not to
/// the ring, which still holds them).
fn replay(prev: &RingHeader) {
    let base = core::ptr::addr_of!(RING).cast::<u8>();
    let (start, len) = if prev.wrapped != 0 {
        (prev.write as usize, RING_SIZE)
    } else {
        (0, prev.write as usize)
    };
    event!(Info, "log.prev_begin", boot_id = prev.boot_id, bytes = len as u32);
    let mut frames = 0u32;
    // After a wrap the oldest frame is cut; skip to its terminator.
    let mut synced = prev.wrapped == 0;
    let mut chunk = [0u8; 64];
    let mut n = 0;
    for i in 0..len {
        let b = unsafe { core::ptr::read_volatile(base.add((start + i) % RING_SIZE)) };
        if !synced {
            synced = b == 0;
            continue;
        }
        chunk[n] = b;
        n += 1;
        if b == 0 {
            frames += 1;
        }
        if n == chunk.len() || i + 1 == len {
            output(&chunk[..n]);
            n = 0;
        }
    }
    event!(Info, "log.prev_end", boot_id = prev.boot_id, frames = frames);
}

fn save_header() {
    let st = state();
    let mut h = RingHeader {
        magic: RING_MAGIC,
        boot_id: st.boot_id,
        write: st.ring_write as u32,
        wrapped: st.ring_wrapped as u32,
        check: 0,
    };
    h.check = h.checksum();
    unsafe { core::ptr::write_volatile(header(), h) };
}

/// Raw bytes to RTT and the console, as whole frames (replay only).
fn output(bytes: &[u8]) {
    // Replayed frames are already COBS-encoded; RTT takes them unsplit only
    // if they fit, so a partial chunk at the end of a frame is fine here.
    let _ = rtt_write(bytes);
    let _ = Current::console_write(bytes);
}

/// The sink of `event!` inside the kernel.
#[unsafe(no_mangle)]
pub fn t5log_emit(payload: &[u8]) {
    record(wire::SOURCE_KERNEL, payload);
}

/// Validates `payload` (header, schema id, fields) and stores it as a record
/// from `source`. Returns false if the payload is malformed.
pub fn record(source: u32, payload: &[u8]) -> bool {
    if payload.len() < 5 || payload.len() > wire::MAX_PAYLOAD {
        return false;
    }
    let level = payload[0] >> 5;
    let count = (payload[0] & 0x1F) as usize;
    if Level::from_u8(level).is_none() || !wire::validate_fields(&payload[5..], count) {
        return false;
    }
    if level > KEEP as u8 {
        return true;
    }

    let st = state();
    let t_us = time::ticks_to_ns(Current::now()) / 1000;
    let mut rec = [0u8; wire::MAX_RECORD];
    rec[..5].copy_from_slice(&payload[..5]);
    let mut o = 5;
    for v in [st.seq, t_us, source as u64] {
        o += wire::put_varint(&mut rec[o..], v).unwrap();
    }
    let fields = &payload[5..];
    rec[o..o + fields.len()].copy_from_slice(fields);
    o += fields.len();
    st.seq += 1;

    let mut frame = [0u8; FRAME_MAX];
    let n = wire::cobs_encode(&rec[..o], &mut frame).unwrap();
    let frame = &frame[..n];
    if st.ring_on {
        ring_push(frame);
    }
    let rtt_ok = rtt_write(frame);
    if !rtt_ok {
        st.rtt_dropped += 1;
    }
    let console_ok = Current::console_write(frame);
    if !console_ok {
        st.console_dropped += 1;
    }
    report_drops(rtt_ok, console_ok);
    true
}

fn ring_push(bytes: &[u8]) {
    let st = state();
    let base = core::ptr::addr_of_mut!(RING).cast::<u8>();
    for &b in bytes {
        unsafe { core::ptr::write_volatile(base.add(st.ring_write), b) };
        st.ring_write += 1;
        if st.ring_write == RING_SIZE {
            st.ring_write = 0;
            st.ring_wrapped = true;
        }
    }
    save_header();
}

/// Appends a whole frame to the RTT up-buffer, or nothing if it does not fit
/// (the host has not read far enough, or no host is attached).
fn rtt_write(frame: &[u8]) -> bool {
    unsafe {
        let up = core::ptr::addr_of_mut!((*rtt()).up[0]);
        let size = RTT_UP_SIZE;
        let wr = core::ptr::read_volatile(core::ptr::addr_of!((*up).wr_off)) as usize;
        let rd = core::ptr::read_volatile(core::ptr::addr_of!((*up).rd_off)) as usize;
        if rd >= size {
            return false; // corrupted by the host
        }
        let free = if rd > wr { rd - wr - 1 } else { size - (wr - rd) - 1 };
        if frame.len() > free {
            return false;
        }
        let buf = core::ptr::addr_of_mut!(RTT_UP).cast::<u8>();
        let mut w = wr;
        for &b in frame {
            core::ptr::write_volatile(buf.add(w), b);
            w = (w + 1) % size;
        }
        // Data before the offset that publishes it.
        fence(Ordering::SeqCst);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*up).wr_off), w as u32);
        true
    }
}

/// Reports earlier drops on a sink once it takes a frame again.
fn report_drops(rtt_ok: bool, console_ok: bool) {
    let st = state();
    if st.reporting {
        return;
    }
    st.reporting = true;
    if rtt_ok && st.rtt_dropped > 0 {
        let count = core::mem::take(&mut st.rtt_dropped);
        event!(Warn, "log.dropped", sink = "rtt", count = count);
    }
    if console_ok && st.console_dropped > 0 {
        let count = core::mem::take(&mut st.console_dropped);
        event!(Warn, "log.dropped", sink = "console", count = count);
    }
    st.reporting = false;
}
