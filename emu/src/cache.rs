//! Model of the L1 cache incoherence found on the Tab5 in M1 (doc/09 §2).
//!
//! L2MEM sits behind a write-back L1 D-cache and an L1 I-cache that do not
//! snoop each other, and `fence.i` does not synchronise them. Code written by
//! the kernel becomes fetchable only after `Cache_WriteBack_Addr(L1_DCACHE)`
//! followed by `Cache_Invalidate_Addr(L1_ICACHE)`. Instead of reproducing the
//! stale bytes (which on hardware executed as garbage), strict mode stops the
//! emulator at the first fetch from a line that has not been synchronised.

pub const LINE: u32 = 64;
const D_DIRTY: u8 = 1;
const I_STALE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Report fetches of unsynchronised code.
    Strict,
    /// Coherent memory (what QEMU virt gives).
    Off,
}

pub struct Cache {
    pub mode: Mode,
    base: u32,
    lines: Vec<u8>,
}

impl Cache {
    pub fn new(mode: Mode, base: u32, size: u32) -> Self {
        Cache { mode, base, lines: vec![0; (size / LINE) as usize] }
    }

    fn lines_of(&self, addr: u32, len: u32) -> std::ops::Range<usize> {
        let size = self.lines.len() as u32 * LINE;
        let lo = addr.saturating_sub(self.base).min(size);
        let hi = addr.saturating_add(len).saturating_sub(self.base).min(size);
        (lo / LINE) as usize..hi.div_ceil(LINE) as usize
    }

    pub fn on_store(&mut self, addr: u32, len: u32) {
        for l in self.lines_of(addr, len) {
            self.lines[l] |= D_DIRTY | I_STALE;
        }
    }

    pub fn writeback_d(&mut self, addr: u32, len: u32) {
        for l in self.lines_of(addr, len) {
            self.lines[l] &= !D_DIRTY;
        }
    }

    /// The I-cache refills from memory, which is only current for lines the
    /// D-cache has written back.
    pub fn invalidate_i(&mut self, addr: u32, len: u32) {
        for l in self.lines_of(addr, len) {
            if self.lines[l] & D_DIRTY == 0 {
                self.lines[l] &= !I_STALE;
            }
        }
    }

    /// Whether fetching from `addr` sees what the data side wrote.
    pub fn fetch_ok(&self, addr: u32) -> bool {
        if self.mode == Mode::Off {
            return true;
        }
        let r = self.lines_of(addr, 1);
        r.is_empty() || self.lines[r.start] & I_STALE == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_writeback_then_invalidate() {
        let mut c = Cache::new(Mode::Strict, 0x1000, 0x1000);
        assert!(c.fetch_ok(0x1000));
        c.on_store(0x1000, 4);
        assert!(!c.fetch_ok(0x1000));
        c.invalidate_i(0x1000, 64); // invalidate before writeback: still stale
        assert!(!c.fetch_ok(0x1000));
        c.writeback_d(0x1000, 64);
        assert!(!c.fetch_ok(0x1000));
        c.invalidate_i(0x1000, 64);
        assert!(c.fetch_ok(0x1000));
        assert!(c.fetch_ok(0x5000)); // outside the modelled region
    }
}
