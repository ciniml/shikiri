//! Erasing and writing flash (doc/14 §5.2). Only the app store partitions
//! (`appmeta` and `apps`), otadata and the kernel slot the running kernel
//! did not start from (doc/16 §3.2) can be changed: the bootloader, the
//! partition table and the running kernel are refused here, whatever the
//! caller asks for.

use t5_abi::otadata::{self, OTADATA_OFFSET, SLOT_SIZE, SLOTS};
use t5_abi::store::{APPMETA_OFFSET, APPS_OFFSET, APPS_SIZE, SECTOR_SIZE};

use crate::platform::{Current, Platform};
use crate::time;

const STORE: core::ops::Range<u32> = APPMETA_OFFSET..APPS_OFFSET + APPS_SIZE;
const OTADATA: core::ops::Range<u32> = OTADATA_OFFSET..OTADATA_OFFSET + 2 * otadata::SECTOR_SIZE;
pub const PAGE: usize = 256;

/// The kernel slot that is not running, if the platform knows which one is.
pub fn spare_slot() -> Option<usize> {
    let running = SLOTS.iter().position(|&s| Some(s) == Current::kernel_slot())?;
    Some(1 - running)
}

/// Whether `[offset, end)` lies within one region that may be written.
fn writable(offset: u32, end: u32) -> bool {
    let spare = spare_slot().map(|i| SLOTS[i]..SLOTS[i] + SLOT_SIZE);
    [Some(STORE), Some(OTADATA), spare].into_iter().flatten().any(|r| r.start <= offset && end <= r.end)
}

/// Time spent in flash operations, with interrupts off (doc/14 §5.2).
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub erases: u32,
    pub writes: u32,
    pub erase_us: u64,
    pub write_us: u64,
    /// The longest single operation.
    pub max_us: u64,
}

impl Stats {
    fn add(&mut self, erase: bool, started: u64) {
        let us = time::ticks_to_ns(Current::now() - started) / 1000;
        if erase {
            self.erases += 1;
            self.erase_us += us;
        } else {
            self.writes += 1;
            self.write_us += us;
        }
        self.max_us = self.max_us.max(us);
    }
}

pub fn erase_sector(offset: u32, stats: &mut Stats) -> Result<(), &'static str> {
    if !writable(offset, offset.saturating_add(SECTOR_SIZE as u32)) || offset as usize % SECTOR_SIZE != 0 {
        return Err("not writable");
    }
    let started = Current::now();
    let r = Current::flash_erase_sector(offset);
    stats.add(true, started);
    Current::feed_watchdog();
    r
}

/// Writes `data` at `offset`, page by page. The range must be erased.
pub fn write(offset: u32, data: &[u8], stats: &mut Stats) -> Result<(), &'static str> {
    let end = offset.checked_add(data.len() as u32).ok_or("not writable")?;
    if !writable(offset, end) || offset % 4 != 0 {
        return Err("not writable");
    }
    let mut done = 0;
    while done < data.len() {
        let o = offset as usize + done;
        let n = (PAGE - o % PAGE).min(data.len() - done);
        let started = Current::now();
        let r = Current::flash_write(o as u32, &data[done..done + n]);
        stats.add(false, started);
        r?;
        done += n;
    }
    Current::feed_watchdog();
    Ok(())
}
