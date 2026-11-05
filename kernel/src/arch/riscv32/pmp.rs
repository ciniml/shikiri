//! PMP management (doc/07 §3).
//!
//! The kernel itself uses no PMP entries: M-mode accesses that match no entry
//! are allowed, U-mode accesses that match no entry are denied. Each process
//! gets two TOR pairs, for its text and for its data + stack. They are
//! adjacent for an image copied into RAM and 128 MiB apart for an XIP image
//! (doc/15):
//!
//! | entry | pmpaddr          | cfg          |
//! |-------|------------------|--------------|
//! | 0     | -                | OFF (spare)  |
//! | 1     | text.start       | OFF (bottom) |
//! | 2     | text.end         | TOR, R-X     |
//! | 3     | data.start       | OFF (bottom) |
//! | 4     | data.end         | TOR, RW-     |
//! | 5     | shared.start     | OFF (bottom) |
//! | 6     | shared.end       | TOR, RW- (only with a shared region: the
//! |       |                  | framebuffer, doc/17 §6) |
//! | 7     | shm.start        | OFF (bottom) |
//! | 8     | shm.end          | TOR, RW- (only with mapped shared memory,
//! |       |                  | doc/19 §2) |
//! | 9     | rings.start      | OFF (bottom) |
//! | 10    | rings.end        | TOR, RW- (only with a netdev's frame rings,
//! |       |                  | doc/35 §5) |
//! | 11    | shm[1].start     | OFF (bottom) |
//! | 12    | shm[1].end       | TOR, RW- (a second mapped window, doc/49 §3) |
//! | 13    | rings[1].start   | OFF (bottom) |
//! | 14    | rings[1].end     | TOR, RW- (a second netdev's rings: Wi-Fi and |
//! |       |                  | the wired Ethernet, doc/58 R5) |

use super::csr::{read_csr, write_csr};

pub const R: u8 = 1 << 0;
pub const W: u8 = 1 << 1;
pub const X: u8 = 1 << 2;
pub const A_TOR: u8 = 1 << 3;
pub const L: u8 = 1 << 7;

/// The ESP32-P4 PMP granularity; region boundaries must be multiples of it.
pub const GRANULE: usize = 128;

/// The four pmpcfg registers as bytes (16 entries).
pub fn read_cfg() -> [u8; 16] {
    let regs = [
        read_csr!("pmpcfg0"),
        read_csr!("pmpcfg1"),
        read_csr!("pmpcfg2"),
        read_csr!("pmpcfg3"),
    ];
    let mut out = [0u8; 16];
    for (i, r) in regs.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&(*r as u32).to_le_bytes());
    }
    out
}

pub fn read_addr(i: usize) -> usize {
    match i {
        0 => read_csr!("pmpaddr0"),
        1 => read_csr!("pmpaddr1"),
        2 => read_csr!("pmpaddr2"),
        3 => read_csr!("pmpaddr3"),
        4 => read_csr!("pmpaddr4"),
        5 => read_csr!("pmpaddr5"),
        6 => read_csr!("pmpaddr6"),
        7 => read_csr!("pmpaddr7"),
        8 => read_csr!("pmpaddr8"),
        9 => read_csr!("pmpaddr9"),
        10 => read_csr!("pmpaddr10"),
        11 => read_csr!("pmpaddr11"),
        12 => read_csr!("pmpaddr12"),
        13 => read_csr!("pmpaddr13"),
        14 => read_csr!("pmpaddr14"),
        15 => read_csr!("pmpaddr15"),
        _ => 0,
    }
}

/// Index of the first locked entry, if any. A locked entry also applies to
/// U-mode and cannot be changed until reset, which would break isolation
/// (doc/07 §0).
pub fn first_locked() -> Option<usize> {
    read_cfg().iter().position(|c| c & L != 0)
}

/// Clears all unlocked entries.
pub fn reset() {
    write_csr!("pmpcfg0", 0);
    write_csr!("pmpcfg1", 0);
    write_csr!("pmpcfg2", 0);
    write_csr!("pmpcfg3", 0);
}

/// Process memory as seen by PMP.
#[derive(Clone, Debug)]
pub struct Region {
    /// Text and read-only data (R-X).
    pub text: core::ops::Range<usize>,
    /// Data, bss and stack (RW-).
    pub data: core::ops::Range<usize>,
    /// Memory the kernel lent the process (RW-): the framebuffer.
    pub shared: Option<core::ops::Range<usize>>,
    /// Mapped shared memory (RW-): two windows (doc/49 §3).
    pub shm: [Option<core::ops::Range<usize>>; 2],
    /// Up to two netdevs' frame rings (RW-).
    pub rings: [Option<core::ops::Range<usize>>; 2],
}

/// Grants U-mode R-X on `text` and RW- on `data`.
pub fn set_process(r: &Region) {
    debug_assert!([r.text.start, r.text.end, r.data.start, r.data.end].iter().all(|a| a % GRANULE == 0));
    // Disable the entries before moving their addresses.
    write_csr!("pmpcfg0", 0);
    write_csr!("pmpcfg1", 0);
    write_csr!("pmpaddr1", r.text.start >> 2);
    write_csr!("pmpaddr2", r.text.end >> 2);
    write_csr!("pmpaddr3", r.data.start >> 2);
    write_csr!("pmpaddr4", r.data.end >> 2);
    let shared = match &r.shared {
        Some(s) => {
            write_csr!("pmpaddr5", s.start >> 2);
            write_csr!("pmpaddr6", s.end >> 2);
            A_TOR | R | W
        }
        None => 0,
    };
    let shm = match &r.shm[0] {
        Some(s) => {
            write_csr!("pmpaddr7", s.start >> 2);
            write_csr!("pmpaddr8", s.end >> 2);
            A_TOR | R | W
        }
        None => 0,
    };
    write_csr!("pmpcfg3", 0);
    let shm1 = match &r.shm[1] {
        Some(s) => {
            write_csr!("pmpaddr11", s.start >> 2);
            write_csr!("pmpaddr12", s.end >> 2);
            A_TOR | R | W
        }
        None => 0,
    };
    let rings = match &r.rings[0] {
        Some(s) => {
            write_csr!("pmpaddr9", s.start >> 2);
            write_csr!("pmpaddr10", s.end >> 2);
            A_TOR | R | W
        }
        None => 0,
    };
    let rings1 = match &r.rings[1] {
        Some(s) => {
            write_csr!("pmpaddr13", s.start >> 2);
            write_csr!("pmpaddr14", s.end >> 2);
            A_TOR | R | W
        }
        None => 0,
    };
    write_csr!("pmpcfg2", u32::from_le_bytes([shm, 0, rings, 0]) as usize);
    write_csr!("pmpcfg3", u32::from_le_bytes([shm1, 0, rings1, 0]) as usize);
    let cfg0 = u32::from_le_bytes([0, 0, A_TOR | R | X, 0]);
    let cfg1 = u32::from_le_bytes([A_TOR | R | W, 0, shared, 0]);
    write_csr!("pmpcfg0", cfg0 as usize);
    write_csr!("pmpcfg1", cfg1 as usize);
}
