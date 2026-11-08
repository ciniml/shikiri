#![no_std]
#![no_main]

//! The terminal (doc/39): draws an 80 x 22 VT/xterm screen with a 16 x 32
//! font, turns the keyboard's events into bytes, and serves `svc:tty`, a
//! pty-like channel: messages from the client are its output, messages
//! to it are the keys. It starts `shell` as its client, and again when the
//! shell ends.
//!
//! Characters beyond ASCII come from the font service (doc/45): wide ones
//! are 32 x 32 glyphs, drawn half in each of their two cells.
//!
//! Built with `--permit display --permit kbd --permit listen:tty --permit
//! proc:spawn --permit svc:font`. Runs until stopped; exit 1 if the display
//! cannot be had.

mod keys;
mod vt;

use t5_rt::{Channel, Display, Error, FOREVER, Font, Handle, Listener, chan, display, event, kbd, rgb, rights, sleep_ms, spawn, time_ns, wait_raw};
use vt::{COLS, Cell, Color, Grid, ROWS, attr};

t5_rt::entry!(main);

/// Glyphs for U+0020..=U+007E, then one for the rest (doc/39 §4).
static FONT: &[u8] = include_bytes!("../../../../third_party/terminus-font/ascii-16x32.bin");
const CW: usize = 16;
const CH: usize = 32;
const GLYPH_BYTES: usize = CW / 8 * CH;
const MISSING: usize = 0x7F - 0x20;

/// The app `term` runs in the session.
const SHELL: &str = "shell";
/// Before the shell is started again after it ended.
const RESTART_MS: u64 = 1000;

const BLANK_GRID: Grid = [[Cell { ch: '\0', fg: Color::Default, bg: Color::Default, attrs: 0 }; COLS]; ROWS];
static mut GRIDS: [Grid; 2] = [BLANK_GRID; 2];
/// What each cell on the screen shows: the cell, and whether the cursor is
/// on it. Cells that would be drawn the same are skipped (doc/42).
static mut SHOWN: [[(Cell, bool); COLS]; ROWS] = [[(BLANK_GRID[0][0], false); COLS]; ROWS];

/// The font service's glyphs, once connected (doc/45).
static mut EXTRA: Option<Font> = None;
/// When to try connecting again.
static mut EXTRA_RETRY: u64 = 0;
const EXTRA_RETRY_NS: u64 = 2_000_000_000;

fn extra() -> Option<&'static Font> {
    unsafe { (*core::ptr::addr_of!(EXTRA)).as_ref() }
}

/// Connects to the font service if not yet (at most every 2 s); true when
/// it has just connected.
fn connect_font(now: u64) -> bool {
    if extra().is_some() || now < unsafe { EXTRA_RETRY } {
        return false;
    }
    unsafe { EXTRA_RETRY = now + EXTRA_RETRY_NS };
    match Font::connect(1000) {
        Ok(f) => {
            unsafe { *core::ptr::addr_of_mut!(EXTRA) = Some(f) };
            event!(Info, "term.font");
            true
        }
        Err(_) => false,
    }
}

fn main(mode: usize, _: usize) -> usize {
    let Ok(mut d) = Display::open() else {
        event!(Error, "term.no_display");
        return 1;
    };
    let keyboard = Handle::open(kbd::NAME, rights::READ | rights::WAIT).ok();
    let Ok(listener) = Listener::open("tty") else {
        event!(Error, "term.no_listener");
        return 2;
    };
    let launcher = Handle::open(spawn::NAME, rights::WRITE).ok();
    let mut t = Term {
        screen: vt::Screen::new(unsafe { &mut *core::ptr::addr_of_mut!(GRIDS) }),
        parser: vt::Parser::new(),
        cursor_drawn: None,
        shown: unsafe { &mut *core::ptr::addr_of_mut!(SHOWN) },
    };
    connect_font(time_ns());
    t.repaint(&mut d);
    if mode == 1 || mode == 2 {
        bench(&mut t, &mut d, mode == 2);
    }
    event!(Info, "term.ready", cols = COLS as u32, rows = ROWS as u32, keyboard = keyboard.is_some());

    let mut client: Option<Channel> = None;
    let mut restart_at = Some(0u64);
    let mut repeat = keys::Repeat::default();
    // Whether the framebuffer is ours (doc/60 §10): while it is not, the
    // screen's changes wait for `display::SHOWN`.
    let mut screen = true;
    loop {
        let now = time_ns();
        if client.is_none() && restart_at.is_some_and(|at| now >= at) {
            restart_at = None;
            start_shell(launcher.as_ref());
        }
        // What may wake us: the display (a seat change), the keyboard, a
        // connection, the client.
        let mut handles = [0u32; 4];
        let mut n = 0;
        for h in [Some(d.handle.raw()), keyboard.as_ref().map(|k| k.raw()), Some(listener.0.raw()), client.as_ref().map(|c| c.0.raw())]
            .into_iter()
            .flatten()
        {
            handles[n] = h;
            n += 1;
        }
        let deadline = [repeat.due(), restart_at.filter(|_| client.is_none())].into_iter().flatten().min();
        let timeout = deadline.map_or(FOREVER, |at| at.saturating_sub(now));
        if let Ok(ev) = wait_raw(&handles[..n], timeout)
            && ev.index == 0
        {
            match ev.kind {
                display::HIDDEN => screen = ev.value == 0,
                display::SHOWN => {
                    screen = true;
                    if ev.value == 1 {
                        t.repaint(&mut d);
                    }
                }
                _ => {}
            }
        }

        let now = time_ns();
        let mut out = [0u8; 64];
        let mut out_len = 0;
        let mut send = |bytes: &[u8], client: &Option<Channel>| {
            // Keys go out a message at a time.
            if out_len + bytes.len() > out.len() {
                if let Some(c) = client {
                    let _ = c.send(&out[..out_len]);
                }
                out_len = 0;
            }
            out[out_len..out_len + bytes.len()].copy_from_slice(bytes);
            out_len += bytes.len();
        };
        if let Some(k) = &keyboard {
            let mut ev = [0u8; kbd::EVENT_SIZE * 16];
            while let Ok(len) = k.read(&mut ev) {
                for e in ev[..len].chunks(kbd::EVENT_SIZE) {
                    repeat.event(e[0], e[1], now);
                    let mut b = [0u8; keys::MAX_BYTES];
                    let m = keys::translate(e[1], e[0], &mut b);
                    send(&b[..m], &client);
                }
            }
        }
        if let Some((m, u)) = repeat.take(now) {
            let mut b = [0u8; keys::MAX_BYTES];
            let len = keys::translate(u, m, &mut b);
            send(&b[..len], &client);
        }
        if let Ok(c) = listener.accept() {
            if client.is_none() {
                event!(Info, "term.session");
                client = Some(c);
            }
            // A second client is refused: dropping it closes it.
        }
        let mut ended = false;
        if let Some(c) = &client {
            let mut buf = [0u8; chan::MSG_MAX];
            loop {
                match c.0.read(&mut buf) {
                    Ok(len) => {
                        for &b in &buf[..len] {
                            t.parser.feed(b, &mut t.screen);
                        }
                    }
                    Err(Error::Busy) => break,
                    // Closed and nothing left.
                    Err(_) => {
                        ended = true;
                        break;
                    }
                }
            }
            let mut r = [0u8; vt::RESPONSE_MAX];
            let len = t.screen.take_response(&mut r);
            send(&r[..len], &client);
        }
        if out_len > 0
            && let Some(c) = &client
        {
            let _ = c.send(&out[..out_len]);
        }
        if ended || client.as_ref().is_some_and(|c| peer_closed(c)) {
            event!(Info, "term.session_ended");
            client = None;
            restart_at = Some(now + RESTART_MS * 1_000_000);
        }
        if screen {
            t.draw(&mut d);
        }
    }
}

/// Whether the client's end is gone (its queue read empty).
fn peer_closed(c: &Channel) -> bool {
    matches!(wait_raw(&[c.0.raw()], 0), Ok(ev) if ev.kind == chan::PEER_CLOSED)
}

fn start_shell(launcher: Option<&Handle>) {
    let Some(h) = launcher else { return };
    let mut req = [0u8; spawn::ARGS + SHELL.len()];
    req[spawn::ARGS..].copy_from_slice(SHELL.as_bytes());
    if h.write(&req).is_err() {
        event!(Warn, "term.spawn_failed");
        // The kernel's queue was full; a later round tries again.
        sleep_ms(10);
    }
}

/// Mode 1 (doc/42): 100 lines of 79 characters, drawn after each line
/// (each scrolls once the screen is full), then the screen is cleared.
/// `keep`: leave the lines on the screen (`run term 2`, to look at them).
fn bench(t: &mut Term, d: &mut Display, keep: bool) {
    const LINES: u32 = 100;
    let t0 = time_ns();
    // The last lines also have wide (the font service's 32 x 32), narrow
    // non-ASCII glyphs and a wide character that wraps (doc/45).
    const MIXED: &str = "日本語の表示：漢字かなカナ。「東京」ｶﾀｶﾅ ─│┌┐ äöü αβγ ";
    for i in 0..LINES {
        if i >= LINES - 4 {
            for _ in 0..2 {
                for &b in MIXED.as_bytes() {
                    t.parser.feed(b, &mut t.screen);
                }
            }
        } else {
            for k in 0..79u32 {
                t.parser.feed(b'!' + ((i + k) % 94) as u8, &mut t.screen);
            }
        }
        for &b in b"\r\n" {
            t.parser.feed(b, &mut t.screen);
        }
        t.draw(d);
    }
    let us = (time_ns() - t0) / 1000;
    let wrong = t.check(d);
    event!(Info, "term.bench", lines = LINES, total_ms = (us / 1000) as u32, per_line_us = (us / LINES as u64) as u32, wrong_cells = wrong, font = extra().is_some());
    if wrong == 0 {
        event!(Info, "term.bench_ok");
    }
    if !keep {
        for &b in b"\x1b[H\x1b[2J" {
            t.parser.feed(b, &mut t.screen);
        }
    }
    t.draw(d);
}

struct Term {
    screen: vt::Screen,
    parser: vt::Parser,
    /// Where the cursor was drawn.
    cursor_drawn: Option<(usize, usize)>,
    shown: &'static mut [[(Cell, bool); COLS]; ROWS],
}

impl Term {
    /// Clears the framebuffer and draws every cell: at the start, and when
    /// shown after the framebuffer was taken away (doc/60 §10).
    fn repaint(&mut self, d: &mut Display) {
        // Matches `SHOWN`'s blank cells. The 2D hardware writes memory
        // itself (doc/44); the CPU's fill needs the flush.
        let (w, h) = d.size();
        if d.start_fill(0, 0, w, h, color(Color::Default, false)).and_then(|_| d.wait_done()).is_err() {
            d.fill(color(Color::Default, false));
            let _ = d.flush_all();
        }
        self.shown.iter_mut().for_each(|r| r.fill((BLANK_GRID[0][0], false)));
        self.cursor_drawn = None;
        self.screen.mark_all();
        self.draw(d);
    }

    /// Draws the changed cells and the cursor, and flushes those rows.
    fn draw(&mut self, d: &mut Display) {
        if connect_font(time_ns()) {
            // Cells drawn without their glyphs before.
            self.shown.iter_mut().for_each(|r| r.fill(STALE));
            self.screen.mark_all();
        }
        let mut dmg = self.screen.take_damage();
        let cursor = self.screen.cursor_visible.then(|| self.screen.cursor());
        // The cursor's old and new cells.
        for (r, c) in [self.cursor_drawn, cursor].into_iter().flatten() {
            if dmg.is_empty() {
                dmg = vt::Damage { row0: r, col0: c, row1: r + 1, col1: c + 1 };
            } else {
                dmg.row0 = dmg.row0.min(r);
                dmg.col0 = dmg.col0.min(c);
                dmg.row1 = dmg.row1.max(r + 1);
                dmg.col1 = dmg.col1.max(c + 1);
            }
        }
        if dmg.is_empty() {
            return;
        }
        let t0 = time_ns();
        // The cells drawn, and the rectangle they span.
        let mut drawn = 0;
        let (mut r0, mut c0, mut r1, mut c1) = (ROWS, COLS, 0, 0);
        let moved = dmg.row1 - dmg.row0 >= MOVE_MIN_ROWS && self.scroll(d);
        if moved {
            // Everything moved: every cell is compared again.
            (r0, c0, r1, c1) = (0, 0, ROWS, COLS);
            dmg = vt::Damage { row0: 0, col0: 0, row1: ROWS, col1: COLS };
        }
        // In the framebuffer's order: when the user's y runs along its rows
        // (90° and 270°), a column of cells is one stretch of framebuffer
        // rows, written while those cache lines are held (doc/43 §5).
        let (h, w) = (dmg.row1 - dmg.row0, dmg.col1 - dmg.col0);
        let by_column = d.steps().1.unsigned_abs() == 1;
        for i in 0..h * w {
            let (r, c) = if by_column { (dmg.row0 + i % h, dmg.col0 + i / h) } else { (dmg.row0 + i / w, dmg.col0 + i % w) };
            let now = (self.cell_at(r, c), cursor == Some((r, c)));
            if self.shown[r][c] == now {
                continue;
            }
            self.shown[r][c] = now;
            draw_cell(d, r, c, &now.0, now.1);
            drawn += 1;
            (r0, c0, r1, c1) = (r0.min(r), c0.min(c), r1.max(r + 1), c1.max(c + 1));
        }
        self.cursor_drawn = cursor;
        if drawn == 0 && !moved {
            return;
        }
        // The framebuffer rows the rectangle covers (the screen is turned).
        let (x0, y0, x1, y1) = (c0 * CW, r0 * CH, c1 * CW - 1, r1 * CH - 1);
        let rows = [d.fb_row(x0, y0), d.fb_row(x1, y0), d.fb_row(x0, y1), d.fb_row(x1, y1)];
        let (lo, hi) = (rows.iter().min().copied().unwrap_or(0), rows.iter().max().copied().unwrap_or(0));
        let t1 = time_ns();
        let _ = d.flush(lo, hi + 1);
        // Large redraws only (a screenful, a scroll), for doc/42.
        if drawn >= COLS * 4 || moved {
            let t2 = time_ns();
            event!(Debug, "term.draw", cells = drawn as u32, moved = moved, draw_us = ((t1 - t0) / 1000) as u32, flush_us = ((t2 - t1) / 1000) as u32);
        }
    }
}

impl Term {
    /// The cell as drawn: the trailing half of a wide character carries
    /// the character and colours of its head (so it is drawn again when
    /// the head changes).
    fn cell_at(&self, r: usize, c: usize) -> Cell {
        let cell = *self.screen.at(r, c);
        if cell.attrs & attr::WIDE_CONT != 0 && c > 0 {
            let head = *self.screen.at(r, c - 1);
            if head.attrs & attr::WIDE != 0 {
                return Cell { attrs: (head.attrs & !attr::WIDE) | attr::WIDE_CONT, ..head };
            }
        }
        cell
    }

    /// Cells whose pixels differ from what the screen holds (the bench's
    /// check that moving the framebuffer kept it right).
    fn check(&self, d: &mut Display) -> u32 {
        let cursor = self.screen.cursor_visible.then(|| self.screen.cursor());
        let mut wrong = 0;
        for r in 0..ROWS {
            for c in 0..COLS {
                let cell = self.cell_at(r, c);
                let (fg, bg) = cell_colors(&cell, cursor == Some((r, c)));
                let lines = glyph_lines(&cell);
                let ok = (0..CH).all(|y| {
                    (0..CW).all(|x| {
                        let want = if lines[y] & (0x8000 >> x) != 0 { fg } else { bg };
                        let i = d.index(c * CW + x, r * CH + y);
                        d.fb_mut()[i] == want
                    })
                });
                wrong += !ok as u32;
            }
        }
        wrong
    }
}

/// Moving the text in the framebuffer costs about as much as drawing this
/// many rows (doc/42: 30 ms against 2.7 ms a row).
const MOVE_MIN_ROWS: usize = 12;
/// A `shown` cell that matches nothing: drawn next time.
const STALE: (Cell, bool) = (Cell { ch: '\0', fg: Color::Default, bg: Color::Default, attrs: u16::MAX }, false);

impl Term {
    /// If the screen's rows are mostly the rows shown `k` rows away (a
    /// scroll), moves the framebuffer and `shown` by `k` rows so that only
    /// the rest needs drawing. Only where user y runs along adjacent
    /// pixels (the 80 x 22 landscape screen).
    fn scroll(&mut self, d: &mut Display) -> bool {
        let (_, sy) = d.steps();
        if sy.abs() != 1 {
            return false;
        }
        let same = |t: &Term, r: usize, shown_r: usize| (0..COLS).all(|c| t.cell_at(r, c) == t.shown[shown_r][c].0);
        let in_place = (0..ROWS).filter(|&r| same(self, r, r)).count();
        // k > 0: content moved up by k rows.
        let mut best = (0isize, in_place);
        for k in 1..ROWS as isize {
            for k in [k, -k] {
                let n = (0..ROWS as isize).filter(|&r| (0..ROWS as isize).contains(&(r + k)) && same(self, r as usize, (r + k) as usize)).count();
                if n > best.1 {
                    best = (k, n);
                }
            }
        }
        let (k, n) = best;
        if k == 0 || n < in_place + MOVE_MIN_ROWS {
            return false;
        }
        let (h, dy) = ((ROWS * CH) as isize, k.unsigned_abs() as isize * CH as isize);
        // Up with the 2D hardware (doc/44). Down, at 270° the copy goes
        // right over itself and the kernel cuts it into 32-pixel strips,
        // slower than the CPU (§3): with the CPU.
        let hw = k > 0 && d.start_copy(0, dy as usize, 0, 0, COLS * CW, (h - dy) as usize).is_ok();
        if hw && d.wait_done().is_err() {
            // Partly copied: draw every cell again.
            self.shown.iter_mut().for_each(|r| r.fill(STALE));
            return true;
        }
        let fb_len = d.fb_mut().len() as isize;
        for x in (0..COLS * CW).filter(|_| !hw) {
            let b = d.index(x, 0) as isize;
            // [src, src + len) to dst, in framebuffer indices.
            let len = h - dy;
            let (src, dst) = match (k > 0, sy > 0) {
                (true, true) => (b + dy, b),
                (true, false) => (b + 1 - h, b + 1 - h + dy),
                (false, true) => (b, b + dy),
                (false, false) => (b + 1 - h + dy, b + 1 - h),
            };
            debug_assert!(src >= 0 && src + len <= fb_len && dst + len <= fb_len);
            d.fb_mut().copy_within(src as usize..(src + len) as usize, dst as usize);
        }
        let rows = ROWS - k.unsigned_abs();
        if k > 0 {
            self.shown.copy_within(k as usize.., 0);
            self.shown[rows..].iter_mut().for_each(|r| r.fill(STALE));
        } else {
            self.shown.copy_within(..rows, k.unsigned_abs());
            self.shown[..k.unsigned_abs()].iter_mut().for_each(|r| r.fill(STALE));
        }
        true
    }
}

/// The 16 ANSI colours (xterm's).
const ANSI: [(u32, u32, u32); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];
const DEFAULT_FG: (u32, u32, u32) = (208, 208, 208);
const DEFAULT_BG: (u32, u32, u32) = (0, 0, 0);

/// RGB565 for `c` as a foreground (`fg`) or background.
fn color(c: Color, fg: bool) -> u16 {
    let (r, g, b) = match c {
        Color::Default if fg => DEFAULT_FG,
        Color::Default => DEFAULT_BG,
        Color::Index(i) if i < 16 => ANSI[i as usize],
        // The 6x6x6 cube, then 24 greys.
        Color::Index(i) if i < 232 => {
            let i = (i - 16) as u32;
            let level = |v: u32| if v == 0 { 0 } else { 55 + 40 * v };
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        Color::Index(i) => {
            let v = 8 + 10 * (i - 232) as u32;
            (v, v, v)
        }
        Color::Rgb(v) => (v >> 16, (v >> 8) & 0xFF, v & 0xFF),
    };
    rgb(r, g, b)
}

/// Foreground and background (RGB565) of `cell`, with the cursor on it.
fn cell_colors(cell: &Cell, cursor: bool) -> (u16, u16) {
    let mut fg = cell.fg;
    // Bold brightens the first 8 colours.
    if cell.attrs & attr::BOLD != 0
        && let Color::Index(i) = fg
        && i < 8
    {
        fg = Color::Index(i + 8);
    }
    let (mut fg, mut bg) = (color(fg, true), color(cell.bg, false));
    if (cell.attrs & attr::REVERSE != 0) ^ cursor {
        core::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

/// The glyph's rows (bit 15 leftmost), underline and hiding applied.
/// The cell's 16 x 32 pixels (bit 15 the leftmost): ASCII from `FONT`,
/// the rest from the font service; a wide character's left half in its
/// head cell, its right half in the trailing one (`Term::cell_at`).
fn glyph_lines(cell: &Cell) -> [u16; CH] {
    let mut lines = [0u16; CH];
    if cell.attrs & attr::HIDDEN != 0 {
        return lines;
    }
    let ascii = |i: usize, lines: &mut [u16; CH]| {
        let bits = &FONT[i * GLYPH_BYTES..(i + 1) * GLYPH_BYTES];
        for (y, l) in lines.iter_mut().enumerate() {
            *l = u16::from_be_bytes([bits[2 * y], bits[2 * y + 1]]);
        }
    };
    let cont = cell.attrs & attr::WIDE_CONT != 0;
    match cell.ch {
        '\0' => {}
        c @ ' '..='~' => ascii(c as usize - 0x20, &mut lines),
        c if cell.attrs & (attr::WIDE | attr::WIDE_CONT) != 0 => match extra().and_then(|f| f.wide(c)) {
            Some(g) => {
                let shift = if cont { 0 } else { 16 };
                for (l, row) in lines.iter_mut().zip(g) {
                    *l = (row >> shift) as u16;
                }
            }
            None if !cont => ascii(MISSING, &mut lines),
            None => {}
        },
        c => match extra().and_then(|f| f.narrow(c)) {
            Some(g) => lines = g,
            None => ascii(MISSING, &mut lines),
        },
    }
    if cell.attrs & attr::UNDERLINE != 0 {
        lines[CH - 3] = 0xFFFF;
    }
    lines
}

fn draw_cell(d: &mut Display, row: usize, col: usize, cell: &Cell, cursor: bool) {
    let (fg, bg) = cell_colors(cell, cursor);
    let lines = glyph_lines(cell);
    // Along the axis whose pixels are adjacent in the framebuffer (doc/42).
    let base = d.index(col * CW, row * CH) as isize;
    let (sx, sy) = d.steps();
    let fb = d.fb_mut();
    if lines == [0; CH] {
        // No ink: a plain fill along the adjacent axis.
        let (along, across, n, m) = if sy.abs() == 1 { (sy, sx, CH, CW) } else { (sx, sy, CW, CH) };
        for k in 0..m {
            let start = (base + k as isize * across) as usize;
            let run = if along == 1 { start..start + n } else { start + 1 - n..start + 1 };
            fb[run].fill(bg);
        }
    } else if sy.abs() == 1 {
        for x in 0..CW {
            let mask = 0x8000 >> x;
            span(fb, base + x as isize * sx, sy, CH, |y| lines[y] & mask != 0, fg, bg);
        }
    } else {
        for (y, &line) in lines.iter().enumerate() {
            span(fb, base + y as isize * sy, sx, CW, |x| line & (0x8000 >> x) != 0, fg, bg);
        }
    }
}

/// `n` adjacent pixels from index `start`, going by `step` (±1): pixel k is
/// `fg` where `on(k)`.
fn span(fb: &mut [u16], start: isize, step: isize, n: usize, on: impl Fn(usize) -> bool, fg: u16, bg: u16) {
    let start = start as usize;
    if step == 1 {
        for (k, p) in fb[start..start + n].iter_mut().enumerate() {
            *p = if on(k) { fg } else { bg };
        }
    } else {
        for (k, p) in fb[start + 1 - n..=start].iter_mut().enumerate() {
            *p = if on(n - 1 - k) { fg } else { bg };
        }
    }
}
