//! Key events (USB HID keyboard page) to the bytes a terminal sends, US
//! layout, after tab5_claude_client `main/hid_translate.cpp`, and key
//! repeat: the keyboard reports only presses and releases.

const MOD_CTRL: u8 = 0x01 | 0x10;
const MOD_SHIFT: u8 = 0x02 | 0x20;
const MOD_ALT: u8 = 0x04 | 0x40;

/// Alt+Up / Alt+Down (the Tab5's keyboard has no Shift or PgUp), or
/// Shift+PgUp / Shift+PgDn (doc/62): moves the view through the history by
/// `page` lines instead of going to the client. Positive: back.
pub fn view_scroll(modifiers: u8, usage: u8, page: isize) -> Option<isize> {
    let only = |m: u8| modifiers & m != 0 && modifiers & !m == 0;
    match usage {
        UP if only(MOD_ALT) => Some(page),
        DOWN if only(MOD_ALT) => Some(-page),
        PAGE_UP if only(MOD_SHIFT) => Some(page),
        PAGE_DOWN if only(MOD_SHIFT) => Some(-page),
        _ => None,
    }
}

/// Longest output of one key.
pub const MAX_BYTES: usize = 12;

// Usages (HID Usage Tables, keyboard page 0x07).
const ENTER: u8 = 0x28;
const ESCAPE: u8 = 0x29;
const BACKSPACE: u8 = 0x2A;
const TAB: u8 = 0x2B;
const F1: u8 = 0x3A;
const F12: u8 = 0x45;
const INSERT: u8 = 0x49;
const HOME: u8 = 0x4A;
const PAGE_UP: u8 = 0x4B;
const DELETE: u8 = 0x4C;
const END: u8 = 0x4D;
const PAGE_DOWN: u8 = 0x4E;
const RIGHT: u8 = 0x4F;
const LEFT: u8 = 0x50;
const DOWN: u8 = 0x51;
const UP: u8 = 0x52;

/// (unshifted, shifted) for usages 0x04..=0x38; 0 = not printable.
const MAP: [(u8, u8); 0x39] = {
    let mut m = [(0u8, 0u8); 0x39];
    let mut i = 0;
    while i < 26 {
        m[0x04 + i] = (b'a' + i as u8, b'A' + i as u8);
        i += 1;
    }
    let digits = *b"1234567890";
    let shifted = *b"!@#$%^&*()";
    let mut i = 0;
    while i < 10 {
        m[0x1E + i] = (digits[i], shifted[i]);
        i += 1;
    }
    m[0x2C] = (b' ', b' ');
    m[0x2D] = (b'-', b'_');
    m[0x2E] = (b'=', b'+');
    m[0x2F] = (b'[', b'{');
    m[0x30] = (b']', b'}');
    m[0x31] = (b'\\', b'|');
    m[0x33] = (b';', b':');
    m[0x34] = (b'\'', b'"');
    m[0x35] = (b'`', b'~');
    m[0x36] = (b',', b'<');
    m[0x37] = (b'.', b'>');
    m[0x38] = (b'/', b'?');
    m
};

struct Out {
    buf: [u8; MAX_BYTES],
    n: usize,
}

impl Out {
    fn push(&mut self, b: u8) {
        if self.n < MAX_BYTES {
            self.buf[self.n] = b;
            self.n += 1;
        }
    }

    fn push_all(&mut self, s: &[u8]) {
        for &b in s {
            self.push(b);
        }
    }

    fn number(&mut self, v: u32) {
        if v >= 10 {
            self.number(v / 10);
        }
        self.push(b'0' + (v % 10) as u8);
    }
}

/// The final byte of `CSI 1;m X` for cursor keys and F1-F4.
fn cursor_final(usage: u8) -> Option<u8> {
    Some(match usage {
        RIGHT => b'C',
        LEFT => b'D',
        DOWN => b'B',
        UP => b'A',
        HOME => b'H',
        END => b'F',
        u if (F1..F1 + 4).contains(&u) => b"PQRS"[(u - F1) as usize],
        _ => return None,
    })
}

/// The number of `CSI n~` for editing keys and F5-F12.
fn tilde_param(usage: u8) -> Option<u32> {
    Some(match usage {
        INSERT => 2,
        DELETE => 3,
        PAGE_UP => 5,
        PAGE_DOWN => 6,
        u if (F1 + 4..=F12).contains(&u) => [15, 17, 18, 19, 20, 21, 23, 24][(u - F1 - 4) as usize],
        _ => return None,
    })
}

/// The bytes for `usage` with `modifiers` into `out`; how many (0: none).
pub fn translate(usage: u8, modifiers: u8, out: &mut [u8; MAX_BYTES]) -> usize {
    let (shift, ctrl, alt) = (modifiers & MOD_SHIFT != 0, modifiers & MOD_CTRL != 0, modifiers & MOD_ALT != 0);
    let mut o = Out { buf: [0; MAX_BYTES], n: 0 };
    // Alt sends ESC first, except where the modifier goes into the sequence.
    let prefix = |o: &mut Out| {
        if alt {
            o.push(0x1B);
        }
    };
    match usage {
        ENTER => {
            prefix(&mut o);
            o.push(b'\r');
        }
        ESCAPE => {
            prefix(&mut o);
            o.push(0x1B);
        }
        BACKSPACE => {
            prefix(&mut o);
            o.push(0x7F);
        }
        // Shift+Tab is back-tab.
        TAB if shift => {
            prefix(&mut o);
            o.push_all(b"\x1b[Z");
        }
        TAB => {
            prefix(&mut o);
            o.push(b'\t');
        }
        _ if cursor_final(usage).is_some() || tilde_param(usage).is_some() => {
            // xterm: 1 + shift 1 + alt 2 + ctrl 4.
            let m = 1 + shift as u32 + 2 * alt as u32 + 4 * ctrl as u32;
            match (cursor_final(usage), tilde_param(usage)) {
                (Some(f), _) if m != 1 => {
                    o.push_all(b"\x1b[1;");
                    o.number(m);
                    o.push(f);
                }
                (_, Some(p)) => {
                    o.push_all(b"\x1b[");
                    o.number(p);
                    if m != 1 {
                        o.push(b';');
                        o.number(m);
                    }
                    o.push(b'~');
                }
                // F1-F4 unmodified are SS3; the rest CSI.
                (Some(f), _) if (F1..F1 + 4).contains(&usage) => o.push_all(&[0x1B, b'O', f]),
                (Some(f), _) => o.push_all(&[0x1B, b'[', f]),
                (None, None) => {}
            }
        }
        0x04..=0x38 => {
            let (base, shifted) = MAP[usage as usize];
            let mut ch = if shift { shifted } else { base };
            if ch == 0 {
                return 0;
            }
            if ctrl {
                ch = match ch {
                    b'a'..=b'z' => ch - b'a' + 1,
                    b'A'..=b'Z' => ch - b'A' + 1,
                    b'@'..=b'_' => ch - b'@',
                    _ => ch,
                };
            }
            prefix(&mut o);
            o.push(ch);
        }
        _ => return 0,
    }
    out.copy_from_slice(&o.buf);
    o.n
}

/// Before the first repeat, and between repeats (ns): as tab5_claude_client.
const REPEAT_DELAY_NS: u64 = 400_000_000;
const REPEAT_RATE_NS: u64 = 60_000_000;

/// The key held down, for repeat.
#[derive(Default)]
pub struct Repeat {
    held: Option<(u8, u8)>,
    next_ns: u64,
}

impl Repeat {
    /// A key event: a press (usage != 0) starts the repeat, anything else
    /// stops it.
    pub fn event(&mut self, modifiers: u8, usage: u8, now_ns: u64) {
        self.held = (usage != 0).then_some((modifiers, usage));
        self.next_ns = now_ns + REPEAT_DELAY_NS;
    }

    /// When the next repeat is due, if a key is held.
    pub fn due(&self) -> Option<u64> {
        self.held.map(|_| self.next_ns)
    }

    /// The key to send again if its repeat is due.
    pub fn take(&mut self, now_ns: u64) -> Option<(u8, u8)> {
        let held = self.held?;
        if now_ns < self.next_ns {
            return None;
        }
        self.next_ns = (self.next_ns + REPEAT_RATE_NS).max(now_ns);
        Some(held)
    }
}
