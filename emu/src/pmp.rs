// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! PMP as implemented on the ESP32-P4 rev v1.x HP cores (doc/01 §3): 16
//! entries, 128-byte granularity (G = 5), TOR and NAPOT (NA4 is not
//! selectable when G >= 1), lock bit. Rules follow the RISC-V Privileged ISA
//! §3.7: the lowest-numbered matching entry decides, an access that only
//! partly overlaps the matching entry fails, M-mode is only checked against
//! locked entries, and U-mode fails when no entry matches.

pub const ENTRIES: usize = 16;
const G: u32 = 5;

const R: u8 = 1 << 0;
const W: u8 = 1 << 1;
const X: u8 = 1 << 2;
const A_SHIFT: u8 = 3;
const A_OFF: u8 = 0;
const A_TOR: u8 = 1;
const A_NA4: u8 = 2;
const A_NAPOT: u8 = 3;
const L: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Exec,
}

#[derive(Default, Clone)]
pub struct Pmp {
    cfg: [u8; ENTRIES],
    addr: [u32; ENTRIES],
    /// The entries that are on, in order, with their decoded ranges:
    /// remade on every CSR write (`check` runs on every access).
    active: [(u64, u64, u8, bool); ENTRIES],
    n_active: usize,
}

impl Pmp {
    fn mode(&self, i: usize) -> u8 {
        (self.cfg[i] >> A_SHIFT) & 3
    }

    fn locked(&self, i: usize) -> bool {
        self.cfg[i] & L != 0
    }

    /// pmpcfg0..3 (RV32: four entries per register).
    pub fn read_cfg(&self, reg: usize) -> u32 {
        u32::from_le_bytes(self.cfg[reg * 4..reg * 4 + 4].try_into().unwrap())
    }

    pub fn write_cfg(&mut self, reg: usize, v: u32) {
        for (k, mut b) in v.to_le_bytes().into_iter().enumerate() {
            let i = reg * 4 + k;
            if self.locked(i) {
                continue;
            }
            b &= !0x60; // reserved bits
            if (b >> A_SHIFT) & 3 == A_NA4 {
                // WARL: NA4 is not selectable with G >= 1.
                b &= !(3 << A_SHIFT);
            }
            self.cfg[i] = b;
        }
        self.decode();
    }

    fn decode(&mut self) {
        self.n_active = 0;
        for i in 0..ENTRIES {
            if let Some((lo, hi)) = self.range(i) {
                self.active[self.n_active] = (lo, hi, self.cfg[i], self.locked(i));
                self.n_active += 1;
            }
        }
    }

    pub fn read_addr(&self, i: usize) -> u32 {
        let a = self.addr[i];
        if self.mode(i) == A_NAPOT {
            a | ((1 << (G - 1)) - 1)
        } else {
            a & !((1 << G) - 1)
        }
    }

    pub fn write_addr(&mut self, i: usize, v: u32) {
        let next_locks_tor = i + 1 < ENTRIES && self.locked(i + 1) && self.mode(i + 1) == A_TOR;
        if self.locked(i) || next_locks_tor {
            return;
        }
        self.addr[i] = v;
        self.decode();
    }

    /// Byte range `[lo, hi)` covered by entry `i`.
    fn range(&self, i: usize) -> Option<(u64, u64)> {
        match self.mode(i) {
            A_OFF => None,
            A_TOR => {
                let lo = if i == 0 { 0 } else { (self.addr[i - 1] & !((1 << G) - 1)) as u64 * 4 };
                let hi = self.read_addr(i) as u64 * 4;
                Some((lo, hi))
            }
            _ => {
                let a = self.read_addr(i) as u64;
                let t = (!a).trailing_zeros();
                let base = (a & !((1u64 << t) - 1)) << 2;
                Some((base, base + (8u64 << t)))
            }
        }
    }

    pub fn check(&self, addr: u32, size: u32, access: Access, m_mode: bool) -> bool {
        let (a, e) = (addr as u64, addr as u64 + size as u64);
        for &(lo, hi, cfg, locked) in &self.active[..self.n_active] {
            if e <= lo || a >= hi {
                continue;
            }
            if a < lo || e > hi {
                return false;
            }
            if m_mode && !locked {
                return true;
            }
            let need = match access {
                Access::Read => R,
                Access::Write => W,
                Access::Exec => X,
            };
            return cfg & need != 0;
        }
        m_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tor_process(p: &mut Pmp) {
        // The kernel's layout (kernel/src/arch/riscv32/pmp.rs).
        p.write_addr(1, 0x4FF4_0000 >> 2);
        p.write_addr(2, 0x4FF4_1000 >> 2);
        p.write_addr(3, 0x4FF4_3000 >> 2);
        p.write_cfg(0, u32::from_le_bytes([0, 0, (A_TOR << 3) | R | X, (A_TOR << 3) | R | W]));
    }

    #[test]
    fn tor_chain() {
        let mut p = Pmp::default();
        tor_process(&mut p);
        assert!(p.check(0x4FF4_0000, 4, Access::Exec, false));
        assert!(!p.check(0x4FF4_0000, 4, Access::Write, false));
        assert!(p.check(0x4FF4_1000, 4, Access::Write, false));
        assert!(!p.check(0x4FF4_1000, 4, Access::Exec, false));
        assert!(!p.check(0x4FF3_FFFC, 4, Access::Read, false));
        assert!(!p.check(0x4FF4_3000, 4, Access::Read, false));
        // Straddling the R-X/RW- boundary fails.
        assert!(!p.check(0x4FF4_0FFE, 4, Access::Read, false));
        // M-mode is unaffected by unlocked entries and by no match.
        assert!(p.check(0x500D_2000, 4, Access::Write, true));
        assert!(p.check(0x4FF4_0000, 4, Access::Write, true));
    }

    #[test]
    fn granularity_and_warl() {
        let mut p = Pmp::default();
        p.write_addr(0, 0xFFFF_FFFF);
        assert_eq!(p.read_addr(0), 0xFFFF_FFE0); // OFF/TOR: low G bits read 0
        p.write_cfg(0, (A_NAPOT as u32) << 3);
        p.write_addr(0, 0x1000_0000);
        assert_eq!(p.read_addr(0), 0x1000_000F); // NAPOT: low G-1 bits read 1
        p.write_cfg(0, (A_NA4 as u32) << 3);
        assert_eq!(p.read_cfg(0) & 0x18, 0); // NA4 -> OFF
    }

    #[test]
    fn napot_minimum_is_128_bytes() {
        let mut p = Pmp::default();
        p.write_addr(0, 0x4FF4_0000 >> 2);
        p.write_cfg(0, ((A_NAPOT << 3) | R) as u32);
        assert!(p.check(0x4FF4_0000 + 124, 4, Access::Read, false));
        assert!(!p.check(0x4FF4_0000 + 128, 4, Access::Read, false));
    }

    #[test]
    fn locked_entries() {
        let mut p = Pmp::default();
        p.write_addr(0, 0x3000_0000 >> 2);
        p.write_cfg(0, ((L | (A_TOR << 3) | R | W) as u32) | (((A_TOR << 3) | R) as u32) << 8);
        // Locked: applies to M-mode, and the address no longer changes.
        assert!(!p.check(0x1000, 4, Access::Exec, true));
        p.write_addr(0, 0);
        assert_eq!(p.read_addr(0), 0x3000_0000 >> 2);
        p.write_cfg(0, 0);
        assert_eq!(p.read_cfg(0) & 0xFF, (L | (A_TOR << 3) | R | W) as u32);
    }
}
