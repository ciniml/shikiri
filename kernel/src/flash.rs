//! Erasing and writing flash (doc/14 §5.2). Only the app store partitions
//! (`appmeta` and `apps`), otadata, the kernel slot the running kernel did
//! not start from (doc/16 §3.2), the settings' 64 KiB (`flash:cfg`, doc/51
//! §2.2) and the old `nvs` partition (erased once, `nvserase`) can be
//! changed: the bootloader, the partition table and the running kernel are
//! refused here, whatever the caller asks for.

use t5_abi::otadata::{self, OTADATA_OFFSET, SLOT_SIZE, SLOTS};
use core::sync::atomic::{AtomicUsize, Ordering};

use t5_abi::cfgflash;
use t5_abi::store::{APPMETA_OFFSET, APPS_OFFSET, APPS_SIZE, SECTOR_SIZE};
use t5_abi::{Error, event};

use crate::platform::{Current, Platform};
use crate::time;

const STORE: core::ops::Range<u32> = APPMETA_OFFSET..APPS_OFFSET + APPS_SIZE;
const OTADATA: core::ops::Range<u32> = OTADATA_OFFSET..OTADATA_OFFSET + 2 * otadata::SECTOR_SIZE;
const CFG: core::ops::Range<u32> = cfgflash::OFFSET..cfgflash::OFFSET + cfgflash::SIZE;
/// ESP-IDF's `nvs` partition (boot/partitions.csv).
const NVS: core::ops::Range<u32> = 0x9000..0xD000;
pub const PAGE: usize = 256;

/// The address of flash offset 0 (`map_flash`), 0 without flash.
static WINDOW: AtomicUsize = AtomicUsize::new(0);

pub fn set_window(window: usize) {
    WINDOW.store(window, Ordering::Release);
}

/// `len` bytes of flash at `offset`, as the window shows them.
fn bytes(offset: u32, len: usize) -> Option<&'static [u8]> {
    let w = WINDOW.load(Ordering::Acquire);
    (w != 0).then(|| unsafe { core::slice::from_raw_parts((w + offset as usize) as *const u8, len) })
}

/// The kernel slot that is not running, if the platform knows which one is.
pub fn spare_slot() -> Option<usize> {
    let running = SLOTS.iter().position(|&s| Some(s) == Current::kernel_slot())?;
    Some(1 - running)
}

/// Whether `[offset, end)` lies within one region that may be written.
fn writable(offset: u32, end: u32) -> bool {
    let spare = spare_slot().map(|i| SLOTS[i]..SLOTS[i] + SLOT_SIZE);
    [Some(STORE), Some(OTADATA), Some(CFG), Some(NVS), spare].into_iter().flatten().any(|r| r.start <= offset && end <= r.end)
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

/// `nvserase` (doc/51 §2.2): erases the `nvs` partition; true if it reads
/// back erased. Skips sectors that are erased already.
pub fn erase_nvs() -> Result<usize, &'static str> {
    let mut stats = Stats::default();
    let mut erased = 0;
    for at in NVS.step_by(SECTOR_SIZE) {
        if bytes(at, SECTOR_SIZE).ok_or("no flash")?.iter().all(|&b| b == 0xFF) {
            continue;
        }
        erase_sector(at, &mut stats)?;
        erased += 1;
    }
    let clean = bytes(NVS.start, (NVS.end - NVS.start) as usize).ok_or("no flash")?.iter().all(|&b| b == 0xFF);
    event!(Info, "flash.nvs_erased", sectors = erased as u32, clean = clean, erase_us = stats.erase_us);
    if clean { Ok(erased) } else { Err("not erased") }
}

/// `flash:cfg`: the settings' flash (doc/51 §2.2), one handle at a time.
pub struct CfgFlash {
    pos: u32,
}

static CFG_OPEN: AtomicUsize = AtomicUsize::new(0);

impl CfgFlash {
    pub fn open() -> Result<CfgFlash, Error> {
        if WINDOW.load(Ordering::Acquire) == 0 {
            return Err(Error::NoDevice);
        }
        if CFG_OPEN.swap(1, Ordering::AcqRel) != 0 {
            return Err(Error::Busy);
        }
        Ok(CfgFlash { pos: 0 })
    }

    /// The part of the region at the position, `len` bytes long.
    fn span(&self, len: usize) -> Result<u32, Error> {
        if len == 0 || len > cfgflash::MAX_IO {
            return Err(Error::Size);
        }
        if self.pos as u64 + len as u64 > cfgflash::SIZE as u64 {
            return Err(Error::Invalid);
        }
        Ok(cfgflash::OFFSET + self.pos)
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let at = self.span(buf.len())?;
        buf.copy_from_slice(bytes(at, buf.len()).ok_or(Error::NoDevice)?);
        self.pos += buf.len() as u32;
        Ok(buf.len())
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, Error> {
        let at = self.span(data.len())?;
        write(at, data, &mut Stats::default()).map_err(|_| Error::Invalid)?;
        self.pos += data.len() as u32;
        Ok(data.len())
    }

    pub fn ctl(&mut self, op: usize, arg: u64) -> Result<usize, Error> {
        match op {
            cfgflash::SEEK if arg <= cfgflash::SIZE as u64 => {
                self.pos = arg as u32;
                Ok(0)
            }
            cfgflash::ERASE if arg < cfgflash::SIZE as u64 && arg % cfgflash::SECTOR as u64 == 0 => {
                erase_sector(cfgflash::OFFSET + arg as u32, &mut Stats::default()).map_err(|_| Error::Fail)?;
                Ok(0)
            }
            cfgflash::SEEK | cfgflash::ERASE => Err(Error::Invalid),
            _ => Err(Error::NoSupport),
        }
    }
}

impl Drop for CfgFlash {
    fn drop(&mut self) {
        CFG_OPEN.store(0, Ordering::Release);
    }
}
