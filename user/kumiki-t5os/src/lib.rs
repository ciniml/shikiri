#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0


//! kumiki on tab5-os. [`FbPanel`] (doc/68) puts kumiki's
//! [`FramebufferPanel`] on the `display` object, so an app draws with
//! kumiki's `FbDisplay` (embedded-graphics, fills, blits, sprites) into the
//! framebuffer of its seat (doc/60). [`I2cDevice`] and [`Delay`] (doc/69)
//! are embedded-hal's `I2c` and `DelayNs`, for kumiki's device drivers.
//!
//! - `back_buffer`: the mapped framebuffer (the panel's 720 x 1280 RGB565,
//!   little-endian; the app's view is turned by `Display::quarter`, give
//!   [`FbPanel::rotation`] to `FbDisplay::with_rotation`).
//! - `flush_cache`: `FLUSH` of the rows the area covers.
//! - `present`: nothing (one buffer per seat).
//! - `hw_fill` / `hw_copy`: the 2D requests `FILL` / `COPY` (doc/44),
//!   waiting for `DONE`. The kernel writes the rows involved back from the
//!   cache first and drops the destination's, as the trait asks. Left to the
//!   CPU (`false`): areas narrower than `HW_MIN_WIDTH` in the panel's
//!   coordinates (the write-back is per row, whatever the width), copies to
//!   the right over themselves (cut into slow strips, doc/44 §3), and
//!   whatever the kernel refuses or fails.
//!
//! Seat changes (doc/60 §10): a `SHOWN` with value 1 means the framebuffer
//! was taken away and is black. The app waits on [`FbPanel::handle`]; a
//! `SHOWN` that came while a 2D request was waited for is kept for
//! [`FbPanel::take_redraw`]. Either way the app calls
//! `FbDisplay::invalidate` and draws everything again.

use embedded_graphics_core::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use kumiki_gfx::{FramebufferPanel, Rgb565Le, Rotation};
use t5_rt::{Display, Error, Event, FOREVER, Handle, display, wait};

/// Narrower areas (panel pixels) are drawn by the CPU.
pub const HW_MIN_WIDTH: u32 = 64;
/// Smaller areas are drawn by the CPU (kumiki's default for the DSI panel;
/// a request's round trip is a few tens of µs).
pub const HW_THRESHOLD: u32 = 64 * 64;

/// The `display` object as a kumiki [`FramebufferPanel`].
pub struct FbPanel {
    d: Display,
    /// A `SHOWN` after the framebuffer was taken away came while waiting.
    redraw: bool,
}

impl FbPanel {
    /// Opens the display (`--permit display`). `FRAME` events are turned
    /// off: the app is woken by seat changes and its 2D requests only.
    pub fn open() -> Result<FbPanel, Error> {
        let d = Display::open()?;
        let _ = d.handle.ctl(display::FRAMES, 0);
        Ok(FbPanel { d, redraw: false })
    }

    /// How the app's view is turned from the panel's (for
    /// `FbDisplay::with_rotation`).
    pub fn rotation(&self) -> Rotation {
        Rotation::from_quarter_turns(self.d.quarter as u8)
    }

    /// The display handle, to wait on for `HIDDEN` / `SHOWN`.
    pub fn handle(&self) -> &Handle {
        &self.d.handle
    }

    /// Whether everything must be drawn again: a `SHOWN` with value 1 came
    /// while a 2D request was waited for (or `ev` is one).
    pub fn take_redraw(&mut self, ev: Option<&Event>) -> bool {
        let now = ev.is_some_and(|e| e.kind == display::SHOWN && e.value == 1);
        core::mem::take(&mut self.redraw) || now
    }

    /// Sends a 2D request and waits for it; whether it was done.
    fn request(&mut self, words: &[u16]) -> bool {
        let mut b = [0u8; 14];
        for (i, w) in words.iter().enumerate() {
            b[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
        }
        if self.d.handle.write(&b[..2 * words.len()]).is_err() {
            return false;
        }
        loop {
            match wait(&[&self.d.handle], FOREVER) {
                Ok(ev) if ev.kind == display::DONE => return true,
                Ok(ev) if ev.kind == display::FAILED => return false,
                Ok(ev) if ev.kind == display::SHOWN && ev.value == 1 => self.redraw = true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
}

/// `r`'s corner and size as u16 (inside the panel).
fn parts(r: &Rectangle) -> (u16, u16, u16, u16) {
    (r.top_left.x as u16, r.top_left.y as u16, r.size.width as u16, r.size.height as u16)
}

impl FramebufferPanel for FbPanel {
    type Format = Rgb565Le;
    type Error = Error;

    fn size(&self) -> Size {
        Size::new(self.d.fb_w as u32, self.d.fb_h as u32)
    }

    fn back_buffer(&mut self) -> &mut [u8] {
        let fb = self.d.fb_mut();
        // The same memory as bytes: RGB565 little-endian, Rgb565Le.
        unsafe { core::slice::from_raw_parts_mut(fb.as_mut_ptr() as *mut u8, fb.len() * 2) }
    }

    fn flush_cache(&mut self, area: &Rectangle) {
        let y0 = area.top_left.y.max(0) as usize;
        let y1 = (y0 + area.size.height as usize).min(self.d.fb_h);
        if y0 < y1 {
            let _ = self.d.flush(y0, y1);
        }
    }

    fn present(&mut self, _changed: Option<&Rectangle>) -> Result<(), Error> {
        Ok(())
    }

    fn hw_fill(&mut self, area: &Rectangle, pixel: &[u8]) -> bool {
        if area.size.width < HW_MIN_WIDTH || pixel.len() != 2 {
            return false;
        }
        let (x, y, w, h) = parts(area);
        self.request(&[display::op::FILL, x, y, w, h, u16::from_le_bytes([pixel[0], pixel[1]])])
    }

    fn hw_copy(&mut self, src: &Rectangle, dst: Point) -> bool {
        if src.size.width < HW_MIN_WIDTH {
            return false;
        }
        // To the right over itself: slow strips in the kernel; the CPU.
        let overlap = Rectangle::new(dst, src.size).intersection(src);
        if dst.x > src.top_left.x && overlap.size.width > 0 && overlap.size.height > 0 {
            return false;
        }
        let (sx, sy, w, h) = parts(src);
        self.request(&[display::op::COPY, sx, sy, dst.x as u16, dst.y as u16, w, h])
    }

    fn hw_threshold(&self) -> u32 {
        HW_THRESHOLD
    }
}

// ---- embedded-hal (doc/69) ----

/// A device on the internal I²C bus as embedded-hal's [`I2c`](embedded_hal::i2c::I2c)
/// (`i2c:int/0xNN`, doc/69): kumiki's drivers (and others on embedded-hal)
/// run unchanged in an app. A write followed by a read is one transfer
/// (repeated start), as `write_read` needs; at most `t5_rt::i2c::MAX`
/// bytes each way.
pub struct I2cDevice {
    h: Handle,
    addr: u8,
}

/// An I²C error: the device did not answer, or anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cError(pub Error);

impl embedded_hal::i2c::Error for I2cError {
    fn kind(&self) -> embedded_hal::i2c::ErrorKind {
        use embedded_hal::i2c::{ErrorKind, NoAcknowledgeSource};
        match self.0 {
            Error::NoAck => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown),
            _ => ErrorKind::Other,
        }
    }
}

impl I2cDevice {
    /// Opens device `addr` (the permission `i2c:int/0xNN`).
    pub fn open(addr: u8) -> Result<I2cDevice, Error> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = [0u8; 12];
        let p = t5_rt::i2c::PREFIX.as_bytes();
        name[..p.len()].copy_from_slice(p);
        name[p.len()..p.len() + 4].copy_from_slice(&[b'0', b'x', HEX[(addr >> 4) as usize], HEX[(addr & 15) as usize]]);
        let name = core::str::from_utf8(&name[..p.len() + 4]).map_err(|_| Error::Invalid)?;
        Ok(I2cDevice { h: Handle::open(name, t5_rt::rights::READ | t5_rt::rights::WRITE)?, addr })
    }

    /// One transfer: `tx` sent, then `rx` read after a repeated start.
    fn transfer(&self, tx: &[u8], rx: &mut [u8]) -> Result<(), I2cError> {
        if tx.len() > t5_rt::i2c::MAX || rx.len() > t5_rt::i2c::MAX {
            return Err(I2cError(Error::Size));
        }
        let mut req = [0u8; 1 + t5_rt::i2c::MAX];
        req[0] = rx.len() as u8;
        req[1..1 + tx.len()].copy_from_slice(tx);
        self.h.write(&req[..1 + tx.len()]).map_err(I2cError)?;
        if !rx.is_empty() && self.h.read(rx).map_err(I2cError)? != rx.len() {
            return Err(I2cError(Error::Fail));
        }
        Ok(())
    }
}

impl embedded_hal::i2c::ErrorType for I2cDevice {
    type Error = I2cError;
}

impl embedded_hal::i2c::I2c for I2cDevice {
    fn transaction(&mut self, address: u8, operations: &mut [embedded_hal::i2c::Operation<'_>]) -> Result<(), I2cError> {
        use embedded_hal::i2c::Operation;
        // The handle is for one device.
        if address != self.addr {
            return Err(I2cError(Error::Denied));
        }
        let mut i = 0;
        while i < operations.len() {
            let (a, b) = operations.split_at_mut(i + 1);
            match (&mut a[i], b.first_mut()) {
                // A write and the read after it: one transfer.
                (Operation::Write(tx), Some(Operation::Read(rx))) => {
                    self.transfer(tx, rx)?;
                    i += 2;
                }
                (Operation::Write(tx), _) => {
                    self.transfer(tx, &mut [])?;
                    i += 1;
                }
                (Operation::Read(rx), _) => {
                    self.transfer(&[], rx)?;
                    i += 1;
                }
            }
        }
        Ok(())
    }
}

/// embedded-hal's [`DelayNs`](embedded_hal::delay::DelayNs) on tab5-os:
/// sleeps for whole milliseconds, spins for the rest.
pub struct Delay;

impl embedded_hal::delay::DelayNs for Delay {
    fn delay_ns(&mut self, ns: u32) {
        let end = t5_rt::time_ns() + ns as u64;
        if ns >= 1_000_000 {
            t5_rt::sleep_ms(ns as u64 / 1_000_000);
        }
        while t5_rt::time_ns() < end {}
    }
}
