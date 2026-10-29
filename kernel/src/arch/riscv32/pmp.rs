//! PMP management (doc/07 §3).
//!
//! The kernel itself uses no PMP entries: M-mode accesses that match no entry
//! are allowed, U-mode accesses that match no entry are denied. Each process
//! gets a TOR chain over its contiguous image:
//!
//! | entry | pmpaddr          | cfg          |
//! |-------|------------------|--------------|
//! | 0     | -                | OFF (spare)  |
//! | 1     | base             | OFF (bottom) |
//! | 2     | base + text_size | TOR, R-X     |
//! | 3     | end              | TOR, RW-     |

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
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub base: usize,
    pub text_end: usize,
    pub end: usize,
}

/// Grants U-mode R-X on `[base, text_end)` and RW- on `[text_end, end)`.
pub fn set_process(r: &Region) {
    debug_assert!(r.base % GRANULE == 0 && r.text_end % GRANULE == 0 && r.end % GRANULE == 0);
    // Disable the entries before moving their addresses.
    write_csr!("pmpcfg0", 0);
    write_csr!("pmpaddr1", r.base >> 2);
    write_csr!("pmpaddr2", r.text_end >> 2);
    write_csr!("pmpaddr3", r.end >> 2);
    let cfg = u32::from_le_bytes([0, 0, A_TOR | R | X, A_TOR | R | W]);
    write_csr!("pmpcfg0", cfg as usize);
}
