//! The screen as apps see it (M6c, doc/17 §6): the orientation, the touch
//! points (read by the kernel while someone has `touch` open, or given by
//! the host with `tap`/`swipe`, doc/54) and who owns the display. The
//! hardware behind it is the platform's.

use t5_abi::{Error, event};

use crate::platform::{Current, Gfx, Platform, TouchPoint};
use crate::time;

/// Quarter turns clockwise from the panel's top to the user's top. The
/// Tab5's standard position is landscape with the panel's top on the
/// user's left: 270 degrees, as with the keyboard and as the ports are
/// laid out (doc/17 §11). `rotate` changes it until the next boot.
static mut ORIENTATION: u32 = 3;

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
    /// A finger from the host (doc/54): it replaces the controller's
    /// points until it is lifted.
    inject: Option<Inject>,
}

/// A finger down at `from` at `start`, at `to` at `end`, then up; the
/// user's coordinates.
#[derive(Clone, Copy)]
struct Inject {
    from: (u16, u16),
    to: (u16, u16),
    start: u64,
    end: u64,
}

/// The size of an injected finger (as the controller reports it).
const INJECT_SIZE: u16 = 30;

static mut TOUCH: Touch = Touch {
    users: 0,
    next_poll: 0,
    seq: 0,
    n: 0,
    points: [TouchPoint { x: 0, y: 0, id: 0, size: 0 }; t5_abi::touch::MAX_POINTS],
    errors: 0,
    inject: None,
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
    if let Some(i) = t.inject {
        let (n, p) = if now < i.end {
            let span = (i.end - i.start).max(1) as i64;
            let at = (now - i.start).min(i.end - i.start) as i64;
            let lerp = |a: u16, b: u16| (a as i64 + (b as i64 - a as i64) * at / span) as u16;
            (1, TouchPoint { x: lerp(i.from.0, i.to.0), y: lerp(i.from.1, i.to.1), id: 0, size: INJECT_SIZE })
        } else {
            t.inject = None;
            event!(Info, "touch.injected", x = i.to.0, y = i.to.1);
            (0, TouchPoint::default())
        };
        if n != t.n || (n == 1 && t.points[0] != p) {
            t.n = n;
            t.points[0] = p;
            t.seq = t.seq.wrapping_add(1);
        }
        return;
    }
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

/// The `tap` and `swipe` commands (doc/54): a finger from (x0, y0) to
/// (x1, y1) over `ms`, the user's coordinates. Only while someone has
/// `touch` open (no one would see it otherwise).
pub fn inject(x0: u32, y0: u32, x1: u32, y1: u32, ms: u32) -> Result<u32, &'static str> {
    let t = touch();
    if t.users == 0 {
        return Err("touch is not open");
    }
    if t.inject.is_some() {
        return Err("a touch is being injected");
    }
    let (w, h) = Current::display_size().ok_or("no display")?;
    let (w, h) = if orientation() % 2 == 1 { (h as u32, w as u32) } else { (w as u32, h as u32) };
    if x0.max(x1) >= w || y0.max(y1) >= h {
        return Err("outside the screen");
    }
    // Down for at least two reads.
    let ms = ms.clamp(2 * TOUCH_POLL_MS as u32, 10_000);
    let now = Current::now();
    t.inject = Some(Inject {
        from: (x0 as u16, y0 as u16),
        to: (x1 as u16, y1 as u16),
        start: now,
        end: now + time::ms_to_ticks(ms as u64),
    });
    // Read at once rather than at the next poll.
    t.next_poll = now;
    Ok(ms)
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
        let t = touch();
        t.users -= 1;
        if t.users == 0 {
            t.inject = None;
        }
    }
}

// ---- display ----

static mut DISPLAY_OWNED: bool = false;

/// The handle on `display`; only one exists at a time.
pub struct DisplayHandle {
    seen_frames: u32,
    /// 2D requests (finished, failed) the process has been told about.
    seen_done: (u32, u32),
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
        Ok(DisplayHandle { seen_frames: Current::display_frames(), seen_done: Current::gfx_finished() })
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

    /// A 2D request (`t5_abi::display::op`, doc/44): starts it.
    pub fn write(&mut self, req: &[u8]) -> Result<usize, Error> {
        use t5_abi::display::op;
        let mut v = [0u16; 7];
        for (i, c) in req.chunks_exact(2).take(7).enumerate() {
            v[i] = u16::from_le_bytes([c[0], c[1]]);
        }
        let gfx = match (v[0], req.len()) {
            (op::FILL, 12) => Gfx::Fill { x: v[1], y: v[2], w: v[3], h: v[4], rgb565: v[5] },
            (op::COPY, 14) => Gfx::Copy { sx: v[1], sy: v[2], dx: v[3], dy: v[4], w: v[5], h: v[6] },
            (op::FILL | op::COPY, _) => return Err(Error::Size),
            _ => return Err(Error::NoSupport),
        };
        if Current::gfx_busy() {
            return Err(Error::Busy);
        }
        Current::gfx_start(gfx).map_err(|why| {
            event!(Debug, "display.gfx_refused", error = why);
            Error::Invalid
        })?;
        Ok(req.len())
    }

    /// The `wait` events: FAILED or DONE with the 2D requests finished
    /// since the last one, else FRAME with the frames sent since the last.
    pub fn poll(&mut self) -> Option<(usize, usize)> {
        let (done, failed) = Current::gfx_finished();
        if (done, failed) != self.seen_done {
            let (n, bad) = (done.wrapping_sub(self.seen_done.0), failed.wrapping_sub(self.seen_done.1));
            self.seen_done = (done, failed);
            return Some(if bad != 0 { (t5_abi::display::FAILED, bad as usize) } else { (t5_abi::display::DONE, n as usize) });
        }
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
