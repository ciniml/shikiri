// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Shared memory (N0b, doc/19 §2): a fixed area of PSRAM handed out in
//! 64 KiB units. A handle on `shm` refers to a window of one region; the
//! process that allocated a region can grant a 128-byte-aligned part of it
//! to the other end of a channel, which then takes it as a handle of its
//! own and maps only that part. A region is freed when no handle or
//! pending grant refers to it any more. There is no allocator: the region
//! table and the unit bitmap are fixed.

use t5_abi::{Error, event};

const UNIT: usize = 64 << 10;
const MAX_UNITS: usize = 64;
const MAX_REGIONS: usize = 16;
/// Grants are in PMP granules.
pub const GRAIN: usize = 128;

struct Region {
    base: usize,
    len: usize,
    /// Handles and pending grants that refer to it.
    refs: u32,
}

struct Area {
    base: usize,
    units: usize,
    used: u64,
    regions: [Option<Region>; MAX_REGIONS],
}

static mut AREA: Area = Area { base: 0, units: 0, used: 0, regions: [const { None }; MAX_REGIONS] };

fn area() -> &'static mut Area {
    unsafe { &mut *core::ptr::addr_of_mut!(AREA) }
}

/// Sets the area up (once, at boot): `[base, base + len)` in PSRAM.
pub fn init(base: usize, len: usize) {
    let a = area();
    a.base = base;
    a.units = (len / UNIT).min(MAX_UNITS);
    event!(Info, "shm.area", base = t5_abi::log::Addr(base as u32), units = a.units as u32, unit_kib = (UNIT >> 10) as u32);
}

pub fn available() -> bool {
    area().units > 0
}

/// What a handle (or a pending grant) refers to: part of a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    region: usize,
    offset: usize,
    pub len: usize,
}

impl Window {
    pub fn addr(&self) -> usize {
        area().regions[self.region].as_ref().unwrap().base + self.offset
    }

    /// A part of this window, as a new reference.
    pub fn part(&self, offset: usize, len: usize) -> Result<Window, Error> {
        if len == 0 || offset % GRAIN != 0 || len % GRAIN != 0 || offset + len > self.len {
            return Err(Error::Invalid);
        }
        area().regions[self.region].as_mut().unwrap().refs += 1;
        Ok(Window { region: self.region, offset: self.offset + offset, len })
    }

    /// Drops this reference; frees the region with the last one.
    pub fn release(self) {
        let a = area();
        let r = a.regions[self.region].as_mut().unwrap();
        r.refs -= 1;
        if r.refs == 0 {
            let (first, n) = ((r.base - a.base) / UNIT, r.len / UNIT);
            a.used &= !(((1u64 << n) - 1) << first);
            event!(Info, "shm.freed", base = t5_abi::log::Addr(r.base as u32), len = r.len as u32);
            a.regions[self.region] = None;
        }
    }
}

/// A new region of at least `bytes` (whole units).
pub fn alloc(bytes: usize) -> Result<Window, Error> {
    let a = area();
    if a.units == 0 {
        return Err(Error::NoDevice);
    }
    let n = bytes.div_ceil(UNIT);
    if n == 0 || n > a.units {
        return Err(Error::Size);
    }
    let slot = a.regions.iter().position(Option::is_none).ok_or(Error::NoMem)?;
    let mask = (1u64 << n) - 1;
    let first = (0..=a.units - n).find(|&f| a.used & (mask << f) == 0).ok_or(Error::NoMem)?;
    a.used |= mask << first;
    let (base, len) = (a.base + first * UNIT, n * UNIT);
    a.regions[slot] = Some(Region { base, len, refs: 1 });
    // A fresh region must not show what an earlier owner left.
    unsafe { core::ptr::write_bytes(base as *mut u8, 0, len) };
    Ok(Window { region: slot, offset: 0, len })
}

/// A handle on `shm`: empty until `ALLOC` or `TAKE_SHM` gives it a window.
pub struct ShmHandle {
    pub window: Option<Window>,
}

impl Drop for ShmHandle {
    fn drop(&mut self) {
        if let Some(w) = self.window.take() {
            w.release();
        }
    }
}
