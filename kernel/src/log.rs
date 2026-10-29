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
//! debugger without halting the core, kept across resets) and in RAM on
//! QEMU virt. The section is not loaded, so `init` sets everything up.

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

struct State {
    ring_write: usize,
    seq: u64,
    /// Frames the RTT channel had no room for, not yet reported.
    rtt_dropped: u32,
    /// Set while reporting drops, so the report cannot recurse.
    reporting: bool,
}

static mut STATE: State = State { ring_write: 0, seq: 0, rtt_dropped: 0, reporting: false };

fn state() -> &'static mut State {
    // Single core, interrupts off in the kernel: no concurrent access.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

fn rtt() -> *mut RttControlBlock {
    core::ptr::addr_of_mut!(_SEGGER_RTT).cast()
}

/// Sets up the ring and the RTT control block. Call once, before the first
/// event.
pub fn init() {
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
    Current::console_write(&[0]);
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
    ring_push(frame);
    if rtt_write(frame) {
        report_rtt_drops();
    } else {
        st.rtt_dropped += 1;
    }
    Current::console_write(frame);
    true
}

fn ring_push(bytes: &[u8]) {
    let st = state();
    let base = core::ptr::addr_of_mut!(RING).cast::<u8>();
    for &b in bytes {
        unsafe { core::ptr::write_volatile(base.add(st.ring_write), b) };
        st.ring_write = (st.ring_write + 1) % RING_SIZE;
    }
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

fn report_rtt_drops() {
    let st = state();
    if st.rtt_dropped == 0 || st.reporting {
        return;
    }
    st.reporting = true;
    let count = core::mem::take(&mut st.rtt_dropped);
    event!(Warn, "log.dropped", sink = "rtt", count = count);
    st.reporting = false;
}
