//! The screen as apps see it (M6c, doc/17 §6): the orientation, the touch
//! points (read by the kernel while someone has `touch` open, or given by
//! the host with `tap`/`swipe`, doc/54) and the seats (doc/60): each
//! process with `display` open has a seat and a framebuffer of its own, the
//! front seat's is shown, and the keyboard and touch go to the front seat's
//! process. The hardware behind it is the platform's.

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
    /// A swipe in from the left edge being followed (doc/60 §8): the
    /// first point and the latest, the user's coordinates.
    edge: Option<((u16, u16), (u16, u16))>,
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
    edge: None,
};

fn touch() -> &'static mut Touch {
    unsafe { &mut *core::ptr::addr_of_mut!(TOUCH) }
}

const TOUCH_POLL_MS: u64 = 16;

/// The switch gesture (doc/60 §8): a finger down within `EDGE_PX` of the
/// left edge, lifted at least `SWIPE_PX` to the right of where it began.
const EDGE_PX: u16 = 24;
const SWIPE_PX: u16 = 200;

/// Reads the touch controller every `TOUCH_POLL_MS` while a handle is open
/// or two seats are held (for the switch gesture). Called from the service
/// loop.
pub fn poll() {
    let t = touch();
    let now = Current::now();
    if (t.users == 0 && taken() < 2) || now < t.next_poll {
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
            let was = t.n;
            t.n = n;
            t.points[0] = p;
            t.seq = t.seq.wrapping_add(1);
            gesture(was);
        }
        release_hold();
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
        let was = t.n;
        t.n = n;
        t.points = pts;
        t.seq = t.seq.wrapping_add(1);
        gesture(was);
    }
    release_hold();
}

/// Follows the switch gesture after the points changed (`was` points
/// before). Only while two seats are held; its points reach no one.
fn gesture(was: usize) {
    let t = touch();
    let p = (t.points[0].x, t.points[0].y);
    match t.edge {
        None if was == 0 && t.n > 0 && p.0 < EDGE_PX && taken() >= 2 => t.edge = Some((p, p)),
        Some((from, _)) if t.n > 0 => t.edge = Some((from, p)),
        Some((from, last)) => {
            t.edge = None;
            if last.0 >= from.0 + SWIPE_PX {
                switch("swipe");
            }
        }
        None => {}
    }
}

/// Lifts the hold of a seat change (doc/60 §4) once no finger is down.
fn release_hold() {
    let t = touch();
    if t.n == 0 && seats().hold_touch {
        seats().hold_touch = false;
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
    // Read at once rather than at the next poll. The finger replaces the
    // controller's points: it comes down on none (for the gesture).
    t.next_poll = now;
    t.n = 0;
    Ok(ms)
}

/// A handle on `touch`.
pub struct TouchHandle {
    pid: u32,
    seen: u32,
    /// The number of points last reported: a process that loses the input
    /// is told once that every finger is up (doc/60 §4).
    reported: usize,
}

impl TouchHandle {
    pub fn open(pid: u32) -> Result<TouchHandle, Error> {
        if Current::touch_read(&mut [TouchPoint::default(); 5]).is_err() {
            return Err(Error::NoDevice);
        }
        let t = touch();
        t.users += 1;
        Ok(TouchHandle { pid, seen: t.seq, reported: 0 })
    }

    /// The points this process sees: none while the input is another's or
    /// held after a seat change.
    fn visible(&self) -> usize {
        if input_for(self.pid) && !seats().hold_touch && touch().edge.is_none() { touch().n } else { 0 }
    }

    /// The `wait` event: CHANGED with the number of points.
    pub fn poll(&mut self) -> Option<(usize, usize)> {
        let t = touch();
        if t.seq == self.seen {
            return None;
        }
        self.seen = t.seq;
        let n = self.visible();
        // Nothing to tell someone who sees no fingers and was told so.
        if n == 0 && self.reported == 0 {
            return None;
        }
        self.reported = n;
        Some((t5_abi::touch::CHANGED, n))
    }

    /// The current points, `POINT_SIZE` bytes each.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let t = touch();
        self.seen = t.seq;
        let n = self.visible();
        self.reported = n;
        let mut len = 0;
        for p in &t.points[..n] {
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

// ---- seats and the display (doc/60) ----

/// Seats (doc/60 §10): processes with `display` open.
pub const SEATS: usize = 6;
/// Framebuffers: as many as the display's memory holds (doc/60 §4).
const FBS: usize = 2;

struct Seats {
    /// The process in each seat.
    pid: [Option<u32>; SEATS],
    /// Each seat's framebuffer, for its whole life: a process draws at a
    /// fixed address (no MMU), so a seat can only get its own back.
    home: [usize; SEATS],
    /// The seat each framebuffer is lent to. A seat whose framebuffer is
    /// another's has none (taken away): it is not in its PMP, and a
    /// process touching it waits until its seat comes in front again.
    holder: [Option<usize>; FBS],
    /// The seat's picture was lost (its framebuffer taken away) since it
    /// was last shown: `SHOWN` asks it to draw everything again.
    lost: [bool; SEATS],
    front: Option<usize>,
    /// After a change, the new front sees no touch until every finger is up.
    hold_touch: bool,
    /// Bumped when a framebuffer is taken away: a core reloads its
    /// process's PMP before it runs it again.
    generation: u32,
}

static mut SEAT: Seats =
    Seats { pid: [None; SEATS], home: [0; SEATS], holder: [None; FBS], lost: [false; SEATS], front: None, hold_touch: false, generation: 0 };

fn seats() -> &'static mut Seats {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(SEAT) }
}

/// Seat `i`'s framebuffer.
fn seat_fb(i: usize) -> Option<usize> {
    Current::display_buffer().map(|(fb, len)| fb + seats().home[i] * len)
}

/// Whether seat `i` holds its framebuffer.
fn holds(i: usize) -> bool {
    seats().holder[seats().home[i]] == Some(i)
}

/// Whether `pid` has a seat whose framebuffer was taken away: the
/// framebuffer is left out of its PMP, and touching it waits.
pub fn taken_away(pid: u32) -> bool {
    let st = seats();
    st.pid.iter().position(|&p| p == Some(pid)).is_some_and(|i| !holds(i))
}

/// Changes when a framebuffer is taken away (see `Seats::generation`).
pub fn generation() -> u32 {
    seats().generation
}

/// Lends seat `i` its framebuffer, taking it from the seat that has it
/// (behind: the front's is never asked for). Clears it: the picture there
/// is another's.
fn lend(i: usize) {
    let st = seats();
    let f = st.home[i];
    if st.holder[f] == Some(i) {
        return;
    }
    if let Some(o) = st.holder[f] {
        st.lost[o] = true;
        st.generation = st.generation.wrapping_add(1);
        // The other core may be running that process with the
        // framebuffer in its PMP: it reloads it before running a process
        // again, and it cannot until this core lets go of the lock.
        crate::smp::quiesce();
        event!(Info, "screen.taken_away", pid = st.pid[o].unwrap_or(0), by = st.pid[i].unwrap_or(0));
    }
    st.holder[f] = Some(i);
    if let Some((fb, len)) = Current::display_buffer() {
        let base = fb + f * len;
        unsafe { core::ptr::write_bytes(base as *mut u8, 0, len) };
        Current::display_flush(base, len);
    }
}

/// The framebuffer shown (`capture` sends it).
pub fn shown_fb() -> Option<usize> {
    seat_fb(seats().front.unwrap_or(0))
}

/// Seats held.
pub fn taken() -> usize {
    seats().pid.iter().flatten().count()
}

/// Whether the keyboard and touch go to `pid`: the front seat's process,
/// or anyone while no process has a seat.
pub fn input_for(pid: u32) -> bool {
    let st = seats();
    match st.front {
        Some(i) => st.pid[i] == Some(pid),
        None => st.pid.iter().all(Option::is_none),
    }
}

/// Puts seat `to` in front (or none): shows its framebuffer, tells the
/// process leaving the front that every key and finger is up.
fn set_front(to: Option<usize>, reason: &'static str) {
    let st = seats();
    if st.front == to {
        return;
    }
    let from = st.front.and_then(|i| st.pid[i]);
    if let Some(i) = to {
        lend(i);
    }
    st.front = to;
    if let Some(fb) = to.and_then(seat_fb) {
        Current::display_show(fb);
    }
    if let Some(p) = from {
        crate::kbd::released(p);
    }
    let t = touch();
    st.hold_touch = t.n > 0;
    t.seq = t.seq.wrapping_add(1);
    event!(Info, "screen.focus", from = from.unwrap_or(0), to = to.and_then(|i| st.pid[i]).unwrap_or(0), reason = reason);
}

/// The `focus` command: puts the seat of `pid` in front.
pub fn focus(pid: u32) -> Result<u32, &'static str> {
    let st = seats();
    let i = st.pid.iter().position(|&p| p == Some(pid)).ok_or("no seat for that process")?;
    set_front(Some(i), "host");
    Ok(pid)
}

/// A write to `display:focus` (doc/60 F3): `by` puts the seat of `pid` in
/// front.
pub fn focus_by(pid: u32, by: u32) -> Result<(), Error> {
    let st = seats();
    let i = st.pid.iter().position(|&p| p == Some(pid)).ok_or(Error::Invalid)?;
    event!(Info, "screen.focus_by", by = by, pid = pid);
    set_front(Some(i), "app");
    Ok(())
}

/// The switch key or gesture (doc/60 §8): puts the next held seat in
/// front.
pub fn switch(reason: &'static str) {
    let st = seats();
    let from = st.front.unwrap_or(SEATS - 1);
    let next = (1..=SEATS).map(|k| (from + k) % SEATS).find(|&i| st.pid[i].is_some());
    if next != st.front {
        set_front(next, reason);
    }
}

/// A handle on `display`: the process's seat.
pub struct DisplayHandle {
    seat: usize,
    /// What the process was last told (`SHOWN`/`HIDDEN`): whether its seat
    /// is in front, and whether it holds its framebuffer; at first, where
    /// it opened.
    told: (bool, bool),
    seen_frames: u32,
    /// 2D requests (finished, failed) the process has been told about.
    seen_done: (u32, u32),
}

impl DisplayHandle {
    pub fn open(pid: u32) -> Result<DisplayHandle, Error> {
        Current::display_buffer().ok_or(Error::NoDevice)?;
        let st = seats();
        // One seat per process.
        if st.pid.contains(&Some(pid)) {
            return Err(Error::Busy);
        }
        let seat = st.pid.iter().position(Option::is_none).ok_or(Error::Busy)?;
        st.pid[seat] = Some(pid);
        // The framebuffer not shown: the one a seat behind uses, if
        // another is in front. Taken from the seat behind that has it.
        st.home[seat] = st.front.map_or(0, |i| (st.home[i] + 1) % FBS);
        lend(seat);
        st.lost[seat] = false;
        // In front if no one is, or if the front process started this one
        // (`term` -> `shell` -> `run mtdash`); else behind.
        let front_pid = st.front.and_then(|i| st.pid[i]);
        match front_pid {
            None => set_front(Some(seat), "first"),
            Some(f) if crate::ident::descends_from(pid, f) => set_front(Some(seat), "child"),
            Some(_) => event!(Info, "screen.seat_behind", pid = pid, front = front_pid.unwrap_or(0)),
        }
        let told = (st.front == Some(seat), true);
        Ok(DisplayHandle { seat, told, seen_frames: Current::display_frames(), seen_done: Current::gfx_finished() })
    }

    fn fb(&self) -> Result<usize, Error> {
        seat_fb(self.seat).ok_or(Error::NoDevice)
    }

    pub fn ctl(&mut self, op: usize, arg: u64) -> Result<usize, Error> {
        use t5_abi::display::*;
        let (_, len) = Current::display_buffer().ok_or(Error::NoDevice)?;
        let fb = self.fb()?;
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
                // Another seat's picture now.
                if !holds(self.seat) {
                    return Ok(0);
                }
                let row = len / h as usize;
                Current::display_flush(fb + y0 * row, (y1 - y0) * row);
                Ok(0)
            }
            ORIENTATION => Ok(orientation() as usize),
            _ => Err(Error::NoSupport),
        }
    }

    /// A 2D request (`t5_abi::display::op`, doc/44) on the seat's
    /// framebuffer: starts it.
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
        if !holds(self.seat) {
            return Err(Error::Off);
        }
        if Current::gfx_busy() {
            return Err(Error::Busy);
        }
        let fb = self.fb()?;
        Current::gfx_start(gfx, fb).map_err(|why| {
            event!(Debug, "display.gfx_refused", error = why);
            Error::Invalid
        })?;
        Ok(req.len())
    }

    /// The `wait` events: HIDDEN or SHOWN when the seat left or came to
    /// the front, else FAILED or DONE with the 2D requests finished since
    /// the last one, else FRAME with the frames sent since the last.
    pub fn poll(&mut self) -> Option<(usize, usize)> {
        let st = seats();
        let now = (st.front == Some(self.seat), holds(self.seat));
        if now != self.told {
            self.told = now;
            return Some(if now.0 {
                (t5_abi::display::SHOWN, core::mem::take(&mut st.lost[self.seat]) as usize)
            } else {
                (t5_abi::display::HIDDEN, !now.1 as usize)
            });
        }
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
        let st = seats();
        let pid = st.pid[self.seat].take();
        if holds(self.seat) {
            st.holder[st.home[self.seat]] = None;
        }
        if st.front == Some(self.seat) {
            // A seat that kept its picture, else any.
            let next = (0..SEATS).filter(|&i| st.pid[i].is_some()).max_by_key(|&i| holds(i));
            // `set_front` names who left; the seat is already empty.
            st.front = None;
            if next.is_some() {
                set_front(next, "closed");
            } else {
                event!(Info, "screen.focus", from = pid.unwrap_or(0), to = 0u32, reason = "closed");
            }
        }
    }
}
