//! The kernel side of structured logging (doc/12 §2.4): the sink behind
//! `event!`, the record ring and the console output.
//!
//! Every record gets a sequence number, a timestamp and its source here, so
//! processes cannot forge them. Records are stored COBS-framed, which lets a
//! reader resynchronise at any 0x00 after the ring wraps. The kernel runs
//! with interrupts off, so nothing here needs locking.

use t5_abi::log::{self as wire, Level};

use crate::platform::{Current, Platform};
use crate::time;

/// Records below this level are dropped (doc/12 §2.1). Returned by the
/// `log_level` syscall.
pub const KEEP: Level = Level::Trace;

const FRAME_MAX: usize = wire::MAX_RECORD + wire::MAX_RECORD / 254 + 2;

struct Ring {
    base: *mut u8,
    len: usize,
    write: usize,
    seq: u64,
}

static mut RING: Ring = Ring { base: core::ptr::null_mut(), len: 0, write: 0, seq: 0 };

fn ring() -> &'static mut Ring {
    // Single core, interrupts off in the kernel: no concurrent access.
    unsafe { &mut *core::ptr::addr_of_mut!(RING) }
}

/// Sets up the ring. Call once, before the first event.
pub fn init() {
    let (base, len) = Current::log_ring();
    let r = ring();
    r.base = base as *mut u8;
    r.len = len;
    r.write = 0;
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

    let r = ring();
    let t_us = time::ticks_to_ns(Current::now()) / 1000;
    let mut rec = [0u8; wire::MAX_RECORD];
    rec[..5].copy_from_slice(&payload[..5]);
    let mut o = 5;
    for v in [r.seq, t_us, source as u64] {
        o += wire::put_varint(&mut rec[o..], v).unwrap();
    }
    let fields = &payload[5..];
    rec[o..o + fields.len()].copy_from_slice(fields);
    o += fields.len();
    r.seq += 1;

    let mut frame = [0u8; FRAME_MAX];
    let n = wire::cobs_encode(&rec[..o], &mut frame).unwrap();
    r.push(&frame[..n]);
    Current::console_write(&frame[..n]);
    true
}

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        if self.len == 0 {
            return;
        }
        for &b in bytes {
            unsafe { core::ptr::write_volatile(self.base.add(self.write), b) };
            self.write = (self.write + 1) % self.len;
        }
    }
}
