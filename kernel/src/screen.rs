//! The screen as apps see it (M6c, doc/17 §6): the orientation, the touch
//! points (read by the kernel while someone has `touch` open) and who owns
//! the display. The hardware behind it is the platform's.

use t5_abi::{Error, event};

use crate::platform::{Current, Platform, TouchPoint};
use crate::time;

/// Quarter turns clockwise from the panel's top to the user's top.
static mut ORIENTATION: u32 = 0;

pub fn orientation() -> u32 {
    unsafe { ORIENTATION }
}

/// The `rotate` command.
pub fn set_orientation(degrees: u32) -> Result<u32, &'static str> {
    if degrees % 90 != 0 || degrees >= 360 {
        return Err("rotate 0|90|180|270");
    }
    unsafe { ORIENTATION = degrees / 90 };
    event!(Info, "screen.orientation", degrees = degrees);
    Ok(degrees)
}

/// A touch point in panel pixels turned into the user's coordinates.
fn to_user(p: TouchPoint, (w, h): (u16, u16)) -> TouchPoint {
    let (x, y) = match orientation() {
        1 => (p.y, w - 1 - p.x),
        2 => (w - 1 - p.x, h - 1 - p.y),
        3 => (h - 1 - p.y, p.x),
        _ => (p.x, p.y),
    };
    TouchPoint { x, y, ..p }
}

// ---- touch ----

struct Touch {
    /// Handles open on `touch`; the kernel reads the controller only then.
    users: u32,
    next_poll: u64,
    /// Changes so far; a handle reports CHANGED when it has not seen the
    /// latest.
    seq: u32,
    n: usize,
    points: [TouchPoint; t5_abi::touch::MAX_POINTS],
    errors: u32,
}

static mut TOUCH: Touch = Touch {
    users: 0,
    next_poll: 0,
    seq: 0,
    n: 0,
    points: [TouchPoint { x: 0, y: 0, id: 0, size: 0 }; t5_abi::touch::MAX_POINTS],
    errors: 0,
};

fn touch() -> &'static mut Touch {
    unsafe { &mut *core::ptr::addr_of_mut!(TOUCH) }
}

const TOUCH_POLL_MS: u64 = 16;

/// Reads the touch controller every `TOUCH_POLL_MS` while a handle is open.
/// Called from the service loop.
pub fn poll() {
    let t = touch();
    let now = Current::now();
    if t.users == 0 || now < t.next_poll {
        return;
    }
    t.next_poll = now + time::ms_to_ticks(TOUCH_POLL_MS);
    let mut raw = [TouchPoint::default(); 5];
    let n = match Current::touch_read(&mut raw) {
        Ok(n) => n,
        Err(why) => {
            if t.errors == 0 {
                event!(Warn, "touch.read_failed", error = why);
            }
            t.errors += 1;
            return;
        }
    };
    let size = Current::display_size().unwrap_or((720, 1280));
    let mut pts = [TouchPoint::default(); 5];
    for (o, p) in pts.iter_mut().zip(&raw[..n]) {
        *o = to_user(*p, size);
    }
    if n != t.n || pts[..n] != t.points[..n] {
        t.n = n;
        t.points = pts;
        t.seq = t.seq.wrapping_add(1);
    }
}

/// A handle on `touch`.
pub struct TouchHandle {
    seen: u32,
}

impl TouchHandle {
    pub fn open() -> Result<TouchHandle, Error> {
        if Current::touch_read(&mut [TouchPoint::default(); 5]).is_err() {
            return Err(Error::NoDevice);
        }
        let t = touch();
        t.users += 1;
        Ok(TouchHandle { seen: t.seq })
    }

    /// The `wait` event: CHANGED with the number of points.
    pub fn poll(&mut self) -> Option<(usize, usize)> {
        let t = touch();
        (t.seq != self.seen).then(|| {
            self.seen = t.seq;
            (t5_abi::touch::CHANGED, t.n)
        })
    }

    /// The current points, `POINT_SIZE` bytes each.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let t = touch();
        self.seen = t.seq;
        let mut len = 0;
        for p in &t.points[..t.n] {
            let Some(o) = out.get_mut(len..len + t5_abi::touch::POINT_SIZE) else { break };
            for (k, v) in [p.x, p.y, p.id, p.size].iter().enumerate() {
                o[2 * k..2 * k + 2].copy_from_slice(&v.to_le_bytes());
            }
            len += t5_abi::touch::POINT_SIZE;
        }
        Ok(len)
    }
}

impl Drop for TouchHandle {
    fn drop(&mut self) {
        touch().users -= 1;
    }
}

// ---- display ----

static mut DISPLAY_OWNED: bool = false;

/// The handle on `display`; only one exists at a time.
pub struct DisplayHandle {
    seen_frames: u32,
}

impl DisplayHandle {
    pub fn open() -> Result<DisplayHandle, Error> {
        if Current::display_buffer().is_none() {
            return Err(Error::NoDevice);
        }
        if unsafe { DISPLAY_OWNED } {
            return Err(Error::Busy);
        }
        unsafe { DISPLAY_OWNED = true };
        Ok(DisplayHandle { seen_frames: Current::display_frames() })
    }

    pub fn ctl(&mut self, op: usize, arg: u64) -> Result<usize, Error> {
        use t5_abi::display::*;
        let (fb, len) = Current::display_buffer().ok_or(Error::NoDevice)?;
        let (w, h) = Current::display_size().ok_or(Error::NoDevice)?;
        match op {
            INFO => Ok(w as usize | (h as usize) << 16),
            // The caller (the process) adds the region to its PMP.
            MAP => Ok(fb),
            FLUSH => {
                let (y0, y1) = ((arg & 0xFFFF) as usize, ((arg >> 16) & 0xFFFF) as usize);
                if y0 >= y1 || y1 > h as usize {
                    return Err(Error::Invalid);
                }
                let row = len / h as usize;
                Current::display_flush(fb + y0 * row, (y1 - y0) * row);
                Ok(0)
            }
            ORIENTATION => Ok(orientation() as usize),
            _ => Err(Error::NoSupport),
        }
    }

    /// The `wait` event: FRAME with the frames sent since the last one.
    pub fn poll(&mut self) -> Option<(usize, usize)> {
        let f = Current::display_frames();
        (f != self.seen_frames).then(|| {
            let n = f.wrapping_sub(self.seen_frames);
            self.seen_frames = f;
            (t5_abi::display::FRAME, n as usize)
        })
    }
}

impl Drop for DisplayHandle {
    fn drop(&mut self) {
        unsafe { DISPLAY_OWNED = false };
    }
}
