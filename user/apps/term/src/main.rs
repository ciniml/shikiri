#![no_std]
#![no_main]

//! The terminal (doc/39): draws an 80 x 22 VT/xterm screen with a 16 x 32
//! font, turns the keyboard's events into bytes, and serves `svc:tty`, a
//! pty-like channel: messages from the client are its output, messages
//! to it are the keys. It starts `shell` as its client, and again when the
//! shell ends.
//!
//! Built with `--permit display --permit kbd --permit listen:tty --permit
//! proc:spawn`. Runs until stopped; exit 1 if the display cannot be had.

mod keys;
mod vt;

use t5_rt::{Channel, Display, Error, FOREVER, Handle, Listener, chan, event, kbd, rgb, rights, sleep_ms, spawn, time_ns, wait_raw};
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

fn main(_: usize, _: usize) -> usize {
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
    };
    d.fill(color(Color::Default, false));
    let _ = d.flush_all();
    t.screen.mark_all();
    t.draw(&mut d);
    event!(Info, "term.ready", cols = COLS as u32, rows = ROWS as u32, keyboard = keyboard.is_some());

    let mut client: Option<Channel> = None;
    let mut restart_at = Some(0u64);
    let mut repeat = keys::Repeat::default();
    loop {
        let now = time_ns();
        if client.is_none() && restart_at.is_some_and(|at| now >= at) {
            restart_at = None;
            start_shell(launcher.as_ref());
        }
        // What may wake us: the keyboard, a connection, the client.
        let mut handles = [0u32; 3];
        let mut n = 0;
        for h in [keyboard.as_ref().map(|k| k.raw()), Some(listener.0.raw()), client.as_ref().map(|c| c.0.raw())].into_iter().flatten() {
            handles[n] = h;
            n += 1;
        }
        let deadline = [repeat.due(), restart_at.filter(|_| client.is_none())].into_iter().flatten().min();
        let timeout = deadline.map_or(FOREVER, |at| at.saturating_sub(now));
        let _ = wait_raw(&handles[..n], timeout);

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
        t.draw(&mut d);
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

struct Term {
    screen: vt::Screen,
    parser: vt::Parser,
    /// Where the cursor was drawn.
    cursor_drawn: Option<(usize, usize)>,
}

impl Term {
    /// Draws the changed cells and the cursor, and flushes those rows.
    fn draw(&mut self, d: &mut Display) {
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
        for r in dmg.row0..dmg.row1 {
            for c in dmg.col0..dmg.col1 {
                let cell = *self.screen.at(r, c);
                draw_cell(d, r, c, &cell, cursor == Some((r, c)));
            }
        }
        self.cursor_drawn = cursor;
        // The framebuffer rows the rectangle covers (the screen is turned).
        let (x0, y0, x1, y1) = (dmg.col0 * CW, dmg.row0 * CH, dmg.col1 * CW - 1, dmg.row1 * CH - 1);
        let rows = [d.fb_row(x0, y0), d.fb_row(x1, y0), d.fb_row(x0, y1), d.fb_row(x1, y1)];
        let (lo, hi) = (rows.iter().min().copied().unwrap_or(0), rows.iter().max().copied().unwrap_or(0));
        let _ = d.flush(lo, hi + 1);
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

fn draw_cell(d: &mut Display, row: usize, col: usize, cell: &Cell, cursor: bool) {
    // The trailing half of a wide glyph is drawn with its head.
    if cell.attrs & attr::WIDE_CONT != 0 && !cursor {
        return;
    }
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
    let glyph = match cell.ch {
        '\0' => 0,
        c @ ' '..='~' => c as usize - 0x20,
        _ => MISSING,
    };
    let bits = &FONT[glyph * GLYPH_BYTES..(glyph + 1) * GLYPH_BYTES];
    let hidden = cell.attrs & attr::HIDDEN != 0;
    let (x0, y0) = (col * CW, row * CH);
    for y in 0..CH {
        let line = u16::from_be_bytes([bits[2 * y], bits[2 * y + 1]]);
        let under = cell.attrs & attr::UNDERLINE != 0 && y == CH - 3;
        for x in 0..CW {
            let on = !hidden && (under || line & (0x8000 >> x) != 0);
            d.put(x0 + x, y0 + y, if on { fg } else { bg });
        }
    }
}
