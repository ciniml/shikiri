#![no_std]

//! kumiki-gfx on tab5-os (doc/68): [`FbPanel`] puts kumiki's
//! [`FramebufferPanel`] on the `display` object, so an app draws with
//! kumiki's `FbDisplay` (embedded-graphics, fills, blits, sprites) into the
//! framebuffer of its seat (doc/60).
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
