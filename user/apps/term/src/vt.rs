//! A VT/xterm subset: the parser and the screen it drives, ported from
//! tab5_claude_client `components/term_core` (Boost Software License 1.0;
//! `parser.cpp`, `screen.cpp`, `east_asian_width.hpp`) to Rust without a
//! heap: the grid has a fixed size and the alternate screen is a second
//! grid.

pub const COLS: usize = 80;
pub const ROWS: usize = 22;
const TAB_WIDTH: usize = 8;

const MAX_PARAMS: usize = 16;
const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Index(u8),
    Rgb(u32),
}

pub mod attr {
    pub const BOLD: u16 = 1 << 0;
    pub const FAINT: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const BLINK: u16 = 1 << 4;
    pub const REVERSE: u16 = 1 << 5;
    pub const HIDDEN: u16 = 1 << 6;
    pub const STRIKE: u16 = 1 << 7;
    /// The leading half of a two-cell glyph.
    pub const WIDE: u16 = 1 << 8;
    /// The trailing half (not drawn: the wide glyph covers it).
    pub const WIDE_CONT: u16 = 1 << 9;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: u16,
}

/// A screen's cells. Zero is a valid blank cell (`'\0'` is drawn as a
/// space), so the grids can live in `.bss`.
pub type Grid = [[Cell; COLS]; ROWS];

/// The cells changed since the last `take_damage`: rows and columns,
/// end exclusive.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Damage {
    pub row0: usize,
    pub col0: usize,
    pub row1: usize,
    pub col1: usize,
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        self.row1 <= self.row0 || self.col1 <= self.col0
    }
}

/// Whether `cp` takes two cells (East Asian wide or fullwidth; a subset of
/// UAX #11, ambiguous ones narrow).
pub fn is_wide(cp: char) -> bool {
    let c = cp as u32;
    matches!(c,
        0x1100..=0x115F | 0x2329..=0x232A | 0x2E80..=0x303E | 0x3041..=0x33FF |
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xA960..=0xA97F |
        0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 |
        0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F680..=0x1F6FF | 0x1F900..=0x1F9FF |
        0x20000..=0x2FFFD | 0x30000..=0x3FFFD)
}

/// What the parser finds.
pub trait Sink {
    fn put_char(&mut self, ch: char);
    /// A C0 control byte.
    fn execute(&mut self, c0: u8);
    fn csi(&mut self, final_byte: u8, params: &[u32], intermediates: &[u8], private_marker: bool);
    fn esc(&mut self, final_byte: u8, intermediates: &[u8]);
    fn osc(&mut self, data: &[u8]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    EscIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
}

/// A Williams-style parser; keeps its state across `feed` calls.
pub struct Parser {
    state: State,
    params: [u32; MAX_PARAMS],
    n_params: usize,
    param_started: bool,
    intermediates: [u8; MAX_INTERMEDIATES],
    n_intermediates: usize,
    private_marker: bool,
    csi_overflow: bool,
    esc_intermediates: [u8; MAX_INTERMEDIATES],
    n_esc_intermediates: usize,
    cp: u32,
    cp_remaining: u32,
    osc: [u8; MAX_OSC],
    osc_len: usize,
    osc_overflow: bool,
}

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1A;

fn is_intermediate(b: u8) -> bool {
    (0x20..=0x2F).contains(&b)
}
fn is_csi_final(b: u8) -> bool {
    (0x40..=0x7E).contains(&b)
}
fn is_private_marker(b: u8) -> bool {
    (0x3C..=0x3F).contains(&b)
}
fn is_esc_final(b: u8) -> bool {
    (0x30..=0x7E).contains(&b)
}

impl Parser {
    pub const fn new() -> Parser {
        Parser {
            state: State::Ground,
            params: [0; MAX_PARAMS],
            n_params: 0,
            param_started: false,
            intermediates: [0; MAX_INTERMEDIATES],
            n_intermediates: 0,
            private_marker: false,
            csi_overflow: false,
            esc_intermediates: [0; MAX_INTERMEDIATES],
            n_esc_intermediates: 0,
            cp: 0,
            cp_remaining: 0,
            osc: [0; MAX_OSC],
            osc_len: 0,
            osc_overflow: false,
        }
    }

    fn enter_esc(&mut self) {
        self.state = State::Esc;
        self.n_esc_intermediates = 0;
    }

    fn enter_csi(&mut self) {
        self.state = State::CsiEntry;
        self.n_params = 0;
        self.param_started = false;
        self.n_intermediates = 0;
        self.private_marker = false;
        self.csi_overflow = false;
    }

    fn enter_osc(&mut self) {
        self.state = State::OscString;
        self.osc_len = 0;
        self.osc_overflow = false;
    }

    fn push_param_digit(&mut self, d: u8) {
        if self.n_params == 0 {
            self.n_params = 1;
            self.params[0] = 0;
        }
        let cur = &mut self.params[self.n_params - 1];
        if *cur > 99_999 {
            self.csi_overflow = true;
            return;
        }
        *cur = *cur * 10 + (d - b'0') as u32;
        self.param_started = true;
    }

    fn next_param(&mut self) {
        if self.n_params == 0 {
            // The first parameter is an implicit empty one.
            self.n_params = 1;
            self.params[0] = 0;
        }
        if self.n_params < MAX_PARAMS {
            self.params[self.n_params] = 0;
            self.n_params += 1;
        } else {
            self.csi_overflow = true;
        }
        self.param_started = false;
    }

    fn push_intermediate(&mut self, b: u8) {
        if self.n_intermediates < MAX_INTERMEDIATES {
            self.intermediates[self.n_intermediates] = b;
            self.n_intermediates += 1;
        }
    }

    fn push_esc_intermediate(&mut self, b: u8) {
        if self.n_esc_intermediates < MAX_INTERMEDIATES {
            self.esc_intermediates[self.n_esc_intermediates] = b;
            self.n_esc_intermediates += 1;
        }
    }

    fn dispatch_csi(&mut self, final_byte: u8, sink: &mut impl Sink) {
        if !self.csi_overflow {
            sink.csi(final_byte, &self.params[..self.n_params], &self.intermediates[..self.n_intermediates], self.private_marker);
        }
        self.state = State::Ground;
    }

    fn dispatch_esc(&mut self, final_byte: u8, sink: &mut impl Sink) {
        sink.esc(final_byte, &self.esc_intermediates[..self.n_esc_intermediates]);
        self.state = State::Ground;
    }

    fn dispatch_osc(&mut self, sink: &mut impl Sink) {
        if !self.osc_overflow {
            sink.osc(&self.osc[..self.osc_len]);
        }
        self.state = State::Ground;
    }

    fn ground(&mut self, b: u8, sink: &mut impl Sink) {
        if b == ESC {
            self.enter_esc();
            return;
        }
        if b < 0x20 || b == 0x7F {
            if b != 0x7F {
                sink.execute(b);
            }
            return;
        }
        // Printable, or UTF-8.
        if self.cp_remaining > 0 {
            if b & 0xC0 == 0x80 {
                self.cp = (self.cp << 6) | (b & 0x3F) as u32;
                self.cp_remaining -= 1;
                if self.cp_remaining == 0 {
                    sink.put_char(char::from_u32(self.cp).unwrap_or('\u{FFFD}'));
                    self.cp = 0;
                }
                return;
            }
            // A bad continuation: start over with this byte.
            self.cp_remaining = 0;
            self.cp = 0;
        }
        match b {
            0x00..=0x7F => sink.put_char(b as char),
            _ if b & 0xE0 == 0xC0 => (self.cp, self.cp_remaining) = ((b & 0x1F) as u32, 1),
            _ if b & 0xF0 == 0xE0 => (self.cp, self.cp_remaining) = ((b & 0x0F) as u32, 2),
            _ if b & 0xF8 == 0xF0 => (self.cp, self.cp_remaining) = ((b & 0x07) as u32, 3),
            _ => (self.cp, self.cp_remaining) = (0, 0),
        }
    }

    pub fn feed(&mut self, b: u8, sink: &mut impl Sink) {
        if b == CAN || b == SUB {
            self.state = State::Ground;
            return;
        }
        match self.state {
            State::Ground => self.ground(b, sink),
            State::Esc => match b {
                b'[' => self.enter_csi(),
                b']' => self.enter_osc(),
                _ if is_intermediate(b) => {
                    self.push_esc_intermediate(b);
                    self.state = State::EscIntermediate;
                }
                _ if is_esc_final(b) => self.dispatch_esc(b, sink),
                // Executed, and the sequence goes on.
                _ if b < 0x20 => sink.execute(b),
                _ => self.state = State::Ground,
            },
            State::EscIntermediate => match b {
                _ if is_intermediate(b) => self.push_esc_intermediate(b),
                _ if is_esc_final(b) => self.dispatch_esc(b, sink),
                _ if b < 0x20 => sink.execute(b),
                _ => self.state = State::Ground,
            },
            State::CsiEntry => match b {
                _ if b < 0x20 => sink.execute(b),
                b'0'..=b'9' => {
                    self.push_param_digit(b);
                    self.state = State::CsiParam;
                }
                b';' | b':' => {
                    self.next_param();
                    self.state = State::CsiParam;
                }
                _ if is_private_marker(b) => {
                    self.private_marker = true;
                    self.state = State::CsiParam;
                }
                _ if is_intermediate(b) => {
                    self.push_intermediate(b);
                    self.state = State::CsiIntermediate;
                }
                _ if is_csi_final(b) => self.dispatch_csi(b, sink),
                _ => self.state = State::CsiIgnore,
            },
            State::CsiParam => match b {
                _ if b < 0x20 => sink.execute(b),
                b'0'..=b'9' => self.push_param_digit(b),
                b';' | b':' => self.next_param(),
                _ if is_intermediate(b) => {
                    self.push_intermediate(b);
                    self.state = State::CsiIntermediate;
                }
                _ if is_csi_final(b) => {
                    if self.n_params == 0 && self.param_started {
                        self.n_params = 1;
                    }
                    self.dispatch_csi(b, sink);
                }
                _ => self.state = State::CsiIgnore,
            },
            State::CsiIntermediate => match b {
                _ if b < 0x20 => sink.execute(b),
                _ if is_intermediate(b) => self.push_intermediate(b),
                _ if is_csi_final(b) => self.dispatch_csi(b, sink),
                _ => self.state = State::CsiIgnore,
            },
            State::CsiIgnore => match b {
                _ if b < 0x20 => sink.execute(b),
                _ if is_csi_final(b) => self.state = State::Ground,
                _ => {}
            },
            State::OscString => match b {
                BEL => self.dispatch_osc(sink),
                // ESC \ ends it: dispatch, and let ESC state take the '\'.
                ESC => {
                    self.dispatch_osc(sink);
                    self.enter_esc();
                }
                _ if self.osc_len < MAX_OSC => {
                    self.osc[self.osc_len] = b;
                    self.osc_len += 1;
                }
                _ => self.osc_overflow = true,
            },
        }
    }
}

/// What DECSC saves.
#[derive(Clone, Copy, Default)]
struct Saved {
    row: usize,
    col: usize,
    attrs: u16,
    fg: Color,
    bg: Color,
    valid: bool,
}

/// The state that the main and the alternate screen each have.
#[derive(Clone, Copy)]
struct Page {
    /// Its grid in `Screen::grids`.
    g: usize,
    row: usize,
    col: usize,
    wrap_pending: bool,
    scroll_top: usize,
    scroll_bot: usize,
    attrs: u16,
    fg: Color,
    bg: Color,
    saved: Saved,
}

impl Page {
    const fn new(g: usize) -> Page {
        Page {
            g,
            row: 0,
            col: 0,
            wrap_pending: false,
            scroll_top: 0,
            scroll_bot: ROWS,
            attrs: 0,
            fg: Color::Default,
            bg: Color::Default,
            saved: Saved { row: 0, col: 0, attrs: 0, fg: Color::Default, bg: Color::Default, valid: false },
        }
    }
}

/// Bytes the terminal sends back (DSR answers).
pub const RESPONSE_MAX: usize = 64;

pub struct Screen {
    grids: &'static mut [Grid; 2],
    p: Page,
    /// The inactive screen.
    other: Page,
    alt_active: bool,
    pub autowrap: bool,
    pub cursor_visible: bool,
    damage: Damage,
    pub bell: bool,
    response: [u8; RESPONSE_MAX],
    response_len: usize,
}

fn clamp(v: i32, hi_inclusive: usize) -> usize {
    v.clamp(0, hi_inclusive as i32) as usize
}

impl Screen {
    /// The grids start blank (zeroed).
    pub fn new(grids: &'static mut [Grid; 2]) -> Screen {
        Screen {
            grids,
            p: Page::new(0),
            other: Page::new(1),
            alt_active: false,
            autowrap: true,
            cursor_visible: true,
            damage: Damage { row0: 0, col0: 0, row1: 0, col1: 0 },
            bell: false,
            response: [0; RESPONSE_MAX],
            response_len: 0,
        }
    }

    pub fn at(&self, row: usize, col: usize) -> &Cell {
        &self.grids[self.p.g][row][col]
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.p.row, self.p.col)
    }

    pub fn take_damage(&mut self) -> Damage {
        core::mem::take(&mut self.damage)
    }

    /// Takes what the terminal has to send back.
    pub fn take_response(&mut self, out: &mut [u8]) -> usize {
        let n = self.response_len.min(out.len());
        out[..n].copy_from_slice(&self.response[..n]);
        self.response_len = 0;
        n
    }

    pub fn mark_all(&mut self) {
        self.mark_rect(0, 0, ROWS, COLS);
    }

    fn mark_rect(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        if r1 <= r0 || c1 <= c0 {
            return;
        }
        let d = &mut self.damage;
        if d.is_empty() {
            *d = Damage { row0: r0, col0: c0, row1: r1, col1: c1 };
            return;
        }
        d.row0 = d.row0.min(r0);
        d.col0 = d.col0.min(c0);
        d.row1 = d.row1.max(r1);
        d.col1 = d.col1.max(c1);
    }

    fn mark(&mut self, row: usize, col: usize) {
        self.mark_rect(row, col, row + 1, col + 1);
    }

    fn blank(&self) -> Cell {
        Cell { ch: ' ', fg: self.p.fg, bg: self.p.bg, attrs: self.p.attrs }
    }

    fn clear_page(&mut self) {
        self.p = Page::new(self.p.g);
        for row in self.grids[self.p.g].iter_mut() {
            row.fill(Cell { ch: ' ', fg: Color::Default, bg: Color::Default, attrs: 0 });
        }
    }

    pub fn reset(&mut self) {
        self.clear_page();
        self.autowrap = true;
        self.cursor_visible = true;
        self.mark_all();
    }

    fn place_cursor(&mut self, row: usize, col: usize) {
        self.p.row = row.min(ROWS - 1);
        self.p.col = col.min(COLS - 1);
        self.p.wrap_pending = false;
    }

    fn line_feed(&mut self) {
        self.p.wrap_pending = false;
        if self.p.row + 1 < self.p.scroll_bot {
            self.p.row += 1;
        } else if self.p.row + 1 == self.p.scroll_bot {
            self.scroll_up(1);
        } else if self.p.row + 1 < ROWS {
            // Below the scroll region: down to the last row.
            self.p.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.p.wrap_pending = false;
        if self.p.row > self.p.scroll_top {
            self.p.row -= 1;
        } else if self.p.row == self.p.scroll_top {
            self.scroll_down(1);
        } else if self.p.row > 0 {
            self.p.row -= 1;
        }
    }

    fn tab(&mut self) {
        self.p.col = ((self.p.col / TAB_WIDTH + 1) * TAB_WIDTH).min(COLS - 1);
        self.p.wrap_pending = false;
    }

    fn clear_cells(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        if r1 <= r0 || c1 <= c0 {
            return;
        }
        let blank = self.blank();
        for row in &mut self.grids[self.p.g][r0..r1] {
            row[c0..c1].fill(blank);
        }
        self.mark_rect(r0, c0, r1, c1);
    }

    /// Content moves up; lines come in blank at the bottom of the region.
    fn scroll_up(&mut self, n: usize) {
        let (top, bot) = (self.p.scroll_top, self.p.scroll_bot);
        if bot <= top || n == 0 {
            return;
        }
        if n >= bot - top {
            self.clear_cells(top, 0, bot, COLS);
            return;
        }
        self.grids[self.p.g].copy_within(top + n..bot, top);
        let blank = self.blank();
        for row in &mut self.grids[self.p.g][bot - n..bot] {
            row.fill(blank);
        }
        self.mark_rect(top, 0, bot, COLS);
    }

    fn scroll_down(&mut self, n: usize) {
        let (top, bot) = (self.p.scroll_top, self.p.scroll_bot);
        if bot <= top || n == 0 {
            return;
        }
        if n >= bot - top {
            self.clear_cells(top, 0, bot, COLS);
            return;
        }
        self.grids[self.p.g].copy_within(top..bot - n, top + n);
        let blank = self.blank();
        for row in &mut self.grids[self.p.g][top..top + n] {
            row.fill(blank);
        }
        self.mark_rect(top, 0, bot, COLS);
    }

    fn insert_lines(&mut self, n: usize) {
        let (row, bot) = (self.p.row, self.p.scroll_bot);
        if row < self.p.scroll_top || row >= bot {
            return;
        }
        let n = n.max(1).min(bot - row);
        self.grids[self.p.g].copy_within(row..bot - n, row + n);
        let blank = self.blank();
        for r in &mut self.grids[self.p.g][row..row + n] {
            r.fill(blank);
        }
        self.mark_rect(row, 0, bot, COLS);
        self.p.col = 0;
        self.p.wrap_pending = false;
    }

    fn delete_lines(&mut self, n: usize) {
        let (row, bot) = (self.p.row, self.p.scroll_bot);
        if row < self.p.scroll_top || row >= bot {
            return;
        }
        let n = n.max(1).min(bot - row);
        self.grids[self.p.g].copy_within(row + n..bot, row);
        let blank = self.blank();
        for r in &mut self.grids[self.p.g][bot - n..bot] {
            r.fill(blank);
        }
        self.mark_rect(row, 0, bot, COLS);
        self.p.col = 0;
        self.p.wrap_pending = false;
    }

    fn insert_chars(&mut self, n: usize) {
        let (row, col) = (self.p.row, self.p.col);
        let n = n.max(1).min(COLS - col);
        let blank = self.blank();
        let line = &mut self.grids[self.p.g][row];
        line.copy_within(col..COLS - n, col + n);
        line[col..col + n].fill(blank);
        self.mark_rect(row, col, row + 1, COLS);
        self.p.wrap_pending = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (row, col) = (self.p.row, self.p.col);
        let n = n.max(1).min(COLS - col);
        let blank = self.blank();
        let line = &mut self.grids[self.p.g][row];
        line.copy_within(col + n..COLS, col);
        line[COLS - n..].fill(blank);
        self.mark_rect(row, col, row + 1, COLS);
        self.p.wrap_pending = false;
    }

    fn erase_chars(&mut self, n: usize) {
        let (row, col) = (self.p.row, self.p.col);
        let end = (col + n.max(1)).min(COLS);
        self.clear_cells(row, col, row + 1, end);
        self.p.wrap_pending = false;
    }

    fn erase_display(&mut self, mode: u32) {
        let (row, col) = (self.p.row, self.p.col);
        match mode {
            0 => {
                self.clear_cells(row, col, row + 1, COLS);
                self.clear_cells(row + 1, 0, ROWS, COLS);
            }
            1 => {
                self.clear_cells(0, 0, row, COLS);
                self.clear_cells(row, 0, row + 1, col + 1);
            }
            2 | 3 => self.clear_cells(0, 0, ROWS, COLS),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u32) {
        let (row, col) = (self.p.row, self.p.col);
        match mode {
            0 => self.clear_cells(row, col, row + 1, COLS),
            1 => self.clear_cells(row, 0, row + 1, col + 1),
            2 => self.clear_cells(row, 0, row + 1, COLS),
            _ => {}
        }
    }

    fn set_dec_modes(&mut self, params: &[u32], set: bool) {
        for &p in params {
            match p {
                7 => self.autowrap = set,
                25 => self.cursor_visible = set,
                47 | 1047 | 1049 => {
                    if set {
                        self.enter_alt_screen()
                    } else {
                        self.exit_alt_screen()
                    }
                }
                // Mouse, bracketed paste and the rest: not yet.
                _ => {}
            }
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.alt_active {
            return;
        }
        core::mem::swap(&mut self.p, &mut self.other);
        // The alternate screen starts cleared, in the default state.
        self.clear_page();
        self.alt_active = true;
        self.mark_all();
    }

    fn exit_alt_screen(&mut self) {
        if !self.alt_active {
            return;
        }
        core::mem::swap(&mut self.p, &mut self.other);
        self.alt_active = false;
        self.mark_all();
    }

    fn save_cursor(&mut self) {
        let p = &mut self.p;
        p.saved = Saved { row: p.row, col: p.col, attrs: p.attrs, fg: p.fg, bg: p.bg, valid: true };
    }

    fn restore_cursor(&mut self) {
        let s = self.p.saved;
        if s.valid {
            self.place_cursor(s.row, s.col);
            self.p.attrs = s.attrs;
            self.p.fg = s.fg;
            self.p.bg = s.bg;
        }
    }

    fn apply_sgr(&mut self, params: &[u32]) {
        use attr::*;
        let p = &mut self.p;
        if params.is_empty() {
            (p.attrs, p.fg, p.bg) = (0, Color::Default, Color::Default);
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => (p.attrs, p.fg, p.bg) = (0, Color::Default, Color::Default),
                1 => p.attrs |= BOLD,
                2 => p.attrs |= FAINT,
                3 => p.attrs |= ITALIC,
                4 => p.attrs |= UNDERLINE,
                5 => p.attrs |= BLINK,
                7 => p.attrs |= REVERSE,
                8 => p.attrs |= HIDDEN,
                9 => p.attrs |= STRIKE,
                22 => p.attrs &= !(BOLD | FAINT),
                23 => p.attrs &= !ITALIC,
                24 => p.attrs &= !UNDERLINE,
                25 => p.attrs &= !BLINK,
                27 => p.attrs &= !REVERSE,
                28 => p.attrs &= !HIDDEN,
                29 => p.attrs &= !STRIKE,
                39 => p.fg = Color::Default,
                49 => p.bg = Color::Default,
                c @ (38 | 48) => {
                    let color = match params.get(i + 1) {
                        Some(5) if i + 2 < params.len() => {
                            i += 2;
                            Some(Color::Index(params[i] as u8))
                        }
                        Some(2) if i + 4 < params.len() => {
                            let rgb = (params[i + 2] & 0xFF) << 16 | (params[i + 3] & 0xFF) << 8 | (params[i + 4] & 0xFF);
                            i += 4;
                            Some(Color::Rgb(rgb))
                        }
                        // Malformed: skip the rest.
                        Some(_) => {
                            i = params.len();
                            None
                        }
                        None => None,
                    };
                    if let Some(color) = color {
                        if c == 38 {
                            p.fg = color;
                        } else {
                            p.bg = color;
                        }
                    }
                }
                n @ 30..=37 => p.fg = Color::Index((n - 30) as u8),
                n @ 40..=47 => p.bg = Color::Index((n - 40) as u8),
                n @ 90..=97 => p.fg = Color::Index((n - 90 + 8) as u8),
                n @ 100..=107 => p.bg = Color::Index((n - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    /// Replaces the partner of a wide pair whose half at `col` is about to
    /// be overwritten, so no half pair is left.
    fn clear_orphan_pair(&mut self, row: usize, col: usize, fill: Cell) {
        if col >= COLS {
            return;
        }
        let victim = self.grids[self.p.g][row][col].attrs;
        if victim & attr::WIDE != 0 && col + 1 < COLS && self.grids[self.p.g][row][col + 1].attrs & attr::WIDE_CONT != 0 {
            self.grids[self.p.g][row][col + 1] = fill;
            self.mark(row, col + 1);
        }
        if victim & attr::WIDE_CONT != 0 && col > 0 && self.grids[self.p.g][row][col - 1].attrs & attr::WIDE != 0 {
            self.grids[self.p.g][row][col - 1] = fill;
            self.mark(row, col - 1);
        }
    }

    fn respond(&mut self, s: &[u8]) {
        let n = s.len().min(RESPONSE_MAX - self.response_len);
        self.response[self.response_len..self.response_len + n].copy_from_slice(&s[..n]);
        self.response_len += n;
    }

    /// DSR: 5 = status ("OK"), 6 = the cursor position (1-based).
    fn device_status_report(&mut self, params: &[u32], private_marker: bool) {
        match params.first() {
            Some(5) => self.respond(b"\x1b[0n"),
            Some(6) => {
                let mut buf = [0u8; 24];
                let mut n = 0;
                let mut push = |b: u8| {
                    buf[n] = b;
                    n += 1;
                };
                push(0x1B);
                push(b'[');
                if private_marker {
                    push(b'?');
                }
                for (i, v) in [self.p.row + 1, self.p.col + 1].into_iter().enumerate() {
                    if i == 1 {
                        push(b';');
                    }
                    let mut digits = [0u8; 5];
                    let mut k = 0;
                    let mut v = v;
                    loop {
                        digits[k] = b'0' + (v % 10) as u8;
                        k += 1;
                        v /= 10;
                        if v == 0 {
                            break;
                        }
                    }
                    for d in digits[..k].iter().rev() {
                        push(*d);
                    }
                }
                push(b'R');
                self.respond(&buf[..n]);
            }
            _ => {}
        }
    }
}

impl Sink for Screen {
    fn put_char(&mut self, ch: char) {
        let wide = is_wide(ch);
        // The deferred wrap of a character put in the last column.
        if self.p.wrap_pending {
            self.p.col = 0;
            self.line_feed();
        }
        // A wide character that does not fit: wrap first.
        if wide && self.p.col + 1 >= COLS {
            if !self.autowrap {
                return;
            }
            self.p.col = 0;
            self.line_feed();
        }
        let blank = self.blank();
        let (row, col) = (self.p.row, self.p.col);
        if wide {
            self.clear_orphan_pair(row, col, blank);
            self.clear_orphan_pair(row, col + 1, blank);
            let head = Cell { ch, attrs: self.p.attrs | attr::WIDE, ..blank };
            let cont = Cell { attrs: blank.attrs | attr::WIDE_CONT, ..blank };
            self.grids[self.p.g][row][col] = head;
            self.grids[self.p.g][row][col + 1] = cont;
            self.mark_rect(row, col, row + 1, col + 2);
            if col + 2 < COLS {
                self.p.col = col + 2;
            } else {
                self.p.col = col + 1;
                self.p.wrap_pending = self.autowrap;
            }
            return;
        }
        self.clear_orphan_pair(row, col, blank);
        self.grids[self.p.g][row][col] = Cell { ch, ..blank };
        self.mark(row, col);
        if col + 1 < COLS {
            self.p.col += 1;
        } else if self.autowrap {
            // The last column: wrap at the next character.
            self.p.wrap_pending = true;
        }
    }

    fn execute(&mut self, c0: u8) {
        match c0 {
            0x07 => self.bell = true,
            0x08 => {
                self.p.col = self.p.col.saturating_sub(1);
                self.p.wrap_pending = false;
            }
            0x09 => self.tab(),
            0x0A..=0x0C => self.line_feed(),
            0x0D => {
                self.p.col = 0;
                self.p.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn csi(&mut self, final_byte: u8, params: &[u32], _intermediates: &[u8], private_marker: bool) {
        // A missing or 0 parameter takes the default.
        let get = |i: usize, def: u32| params.get(i).copied().filter(|&v| v != 0).unwrap_or(def) as i32;
        let (row, col) = (self.p.row as i32, self.p.col as i32);
        let first = params.first().copied().unwrap_or(0);
        match final_byte {
            b'A' => self.place_cursor(clamp(row - get(0, 1), ROWS - 1), col as usize),
            b'B' => self.place_cursor(clamp(row + get(0, 1), ROWS - 1), col as usize),
            b'C' => self.place_cursor(row as usize, clamp(col + get(0, 1), COLS - 1)),
            b'D' => self.place_cursor(row as usize, clamp(col - get(0, 1), COLS - 1)),
            b'E' => self.place_cursor(clamp(row + get(0, 1), ROWS - 1), 0),
            b'F' => self.place_cursor(clamp(row - get(0, 1), ROWS - 1), 0),
            b'G' => self.place_cursor(row as usize, clamp(get(0, 1) - 1, COLS - 1)),
            b'd' => self.place_cursor(clamp(get(0, 1) - 1, ROWS - 1), col as usize),
            b'H' | b'f' => self.place_cursor(clamp(get(0, 1) - 1, ROWS - 1), clamp(get(1, 1) - 1, COLS - 1)),
            b'J' => self.erase_display(first),
            b'K' => self.erase_line(first),
            b'L' => self.insert_lines(get(0, 1) as usize),
            b'M' => self.delete_lines(get(0, 1) as usize),
            b'P' => self.delete_chars(get(0, 1) as usize),
            b'S' => self.scroll_up(get(0, 1) as usize),
            b'T' => self.scroll_down(get(0, 1) as usize),
            b'X' => self.erase_chars(get(0, 1) as usize),
            b'@' => self.insert_chars(get(0, 1) as usize),
            b'h' if private_marker => self.set_dec_modes(params, true),
            b'l' if private_marker => self.set_dec_modes(params, false),
            b'm' if !private_marker => self.apply_sgr(params),
            b'r' => {
                let top = (get(0, 1) - 1).max(0) as usize;
                let bot = params.get(1).copied().filter(|&v| v != 0).map_or(ROWS, |v| (v as usize).min(ROWS));
                if bot > top {
                    self.p.scroll_top = top;
                    self.p.scroll_bot = bot;
                    self.place_cursor(0, 0);
                }
            }
            b'n' => self.device_status_report(params, private_marker),
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn esc(&mut self, final_byte: u8, intermediates: &[u8]) {
        if !intermediates.is_empty() {
            return;
        }
        match final_byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.line_feed(),
            b'M' => self.reverse_index(),
            b'E' => {
                self.p.col = 0;
                self.line_feed();
            }
            b'c' => self.reset(),
            _ => {}
        }
    }

    fn osc(&mut self, _data: &[u8]) {}
}
