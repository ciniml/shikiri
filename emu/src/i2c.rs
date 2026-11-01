//! I2C1 (0x500C_5000) with the Tab5's internal bus devices that M6a talks
//! to (doc/17 §7): the two PI4IOE5V6408 IO expanders (0x43, 0x44) and the
//! GT911 touch controller (0x14) of the ILI9881C lot, as on the Tab5 at hand,
//! reporting one fixed touch point.
//! The command list runs at once when TRANS_START is set; timing is not
//! modelled. Register layout: doc/research/m6-panel-touch.md §1.3.

use std::collections::VecDeque;

pub const I2C1_BASE: u32 = 0x500C_5000;
pub const I2C1_SIZE: u32 = 0x100;
/// I2C0, on ExtPort1 with the clip-on keyboard (doc/39).
pub const I2C0_BASE: u32 = 0x500C_4000;
pub const KBD_ADDR: u8 = 0x6D;

const CTR: u32 = 0x04;
const SR: u32 = 0x08;
const FIFO_CONF: u32 = 0x18;
const DATA: u32 = 0x1C;
const INT_RAW: u32 = 0x20;
const INT_CLR: u32 = 0x24;
const COMD0: u32 = 0x58;
const SCL_SP_CONF: u32 = 0x80;

const CTR_TRANS_START: u32 = 1 << 5;
const FIFO_RX_RST: u32 = 1 << 12;
const FIFO_TX_RST: u32 = 1 << 13;
const INT_TRANS_COMPLETE: u32 = 1 << 7;
const INT_NACK: u32 = 1 << 10;

/// The point the touch controller reports (panel pixels).
pub const TOUCH_X: u16 = 100;
pub const TOUCH_Y: u16 = 200;

#[derive(Default)]
struct Expander {
    regs: [u8; 0x14],
    /// Register pointer.
    reg: u8,
    /// Bytes written since the start condition.
    written: usize,
}

impl Expander {
    fn new() -> Expander {
        let mut e = Expander::default();
        e.regs[0x01] = 0xA2; // Device ID and Control after power-on
        e.regs[0x07] = 0xFF;
        e.regs[0x0B] = 0xFF;
        e
    }
}

#[derive(Default)]
struct Gt911 {
    reg: u16,
    written: usize,
}

impl Gt911 {
    fn read_reg(&self, reg: u16) -> u8 {
        let [xl, xh] = TOUCH_X.to_le_bytes();
        let [yl, yh] = TOUCH_Y.to_le_bytes();
        match reg {
            0x814E => 0x81, // data ready, one point
            0x814F => 0,    // track id
            0x8150 => xl,
            0x8151 => xh,
            0x8152 => yl,
            0x8153 => yh,
            0x8154 => 20,
            _ => 0,
        }
    }
}

/// The clip-on keyboard in HID mode (doc/39): text typed with `type_text`
/// comes out as press and release events, one key every 30 ms.
#[derive(Default)]
pub struct Keyboard {
    reg: u8,
    written: usize,
    /// (due ms, modifier, usage), oldest first.
    script: VecDeque<(u64, u8, u8)>,
    queue: VecDeque<(u8, u8)>,
    /// The event HID_EVENT (0x30) returned; 0x31 is its second byte.
    current: (u8, u8),
}

impl Keyboard {
    pub fn type_text(&mut self, text: &str, now_ms: u64) {
        let mut t = self.script.back().map_or(now_ms, |e| e.0 + 20).max(now_ms);
        for c in text.chars() {
            if let Some((m, u)) = hid_usage(c) {
                self.script.push_back((t, m, u));
                self.script.push_back((t + 10, 0, 0));
                t += 30;
            }
        }
    }

    /// Moves the events due by `now_ms` into the keyboard's queue.
    pub fn release(&mut self, now_ms: u64) {
        while self.script.front().is_some_and(|e| e.0 <= now_ms) {
            let (_, m, u) = self.script.pop_front().unwrap();
            self.queue.push_back((m, u));
        }
    }

    /// INT is low while events wait.
    pub fn int_low(&self) -> bool {
        !self.queue.is_empty()
    }

    fn read_byte(&mut self) -> u8 {
        let v = match self.reg {
            0x02 => self.queue.len().min(32) as u8,
            0x30 => {
                self.current = self.queue.pop_front().unwrap_or((0xFF, 0xFF));
                self.current.0
            }
            0x31 => self.current.1,
            0xFE => 0x10,
            _ => 0,
        };
        self.reg = self.reg.wrapping_add(1);
        v
    }
}

/// The HID usage (and Shift) for an ASCII character.
fn hid_usage(c: char) -> Option<(u8, u8)> {
    const SHIFT: u8 = 1 << 1;
    let plain = |u: u8| Some((0, u));
    let shifted = |u: u8| Some((SHIFT, u));
    match c {
        'a'..='z' => plain(0x04 + (c as u8 - b'a')),
        'A'..='Z' => shifted(0x04 + (c as u8 - b'A')),
        '1'..='9' => plain(0x1E + (c as u8 - b'1')),
        '0' => plain(0x27),
        '\n' => plain(0x28),
        '\x1b' => plain(0x29),
        '\x08' => plain(0x2A),
        '\t' => plain(0x2B),
        ' ' => plain(0x2C),
        '-' => plain(0x2D),
        '=' => plain(0x2E),
        '[' => plain(0x2F),
        ']' => plain(0x30),
        '\\' => plain(0x31),
        ';' => plain(0x33),
        '\'' => plain(0x34),
        '`' => plain(0x35),
        ',' => plain(0x36),
        '.' => plain(0x37),
        '/' => plain(0x38),
        _ => {
            let i = "!@#$%^&*()".find(c);
            if let Some(i) = i {
                return shifted(0x1E + i as u8);
            }
            let (from, to) = ("_+{}|:\"~<>?", [0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38]);
            from.chars().position(|f| f == c).and_then(|i| shifted(to[i]))
        }
    }
}

pub struct I2c {
    regs: [u32; (I2C1_SIZE / 4) as usize],
    /// On I2C0 only.
    pub kbd: Option<Keyboard>,
    tx: VecDeque<u8>,
    rx: VecDeque<u8>,
    exp: [Expander; 2],
    touch: Gt911,
    /// Device addressed since the last start, if it answered.
    target: Option<u8>,
    /// Transfers that ran, for tests.
    pub transfers: u64,
    /// Register writes to expander #2 (must stay 0).
    pub exp2_writes: u32,
}

impl Default for I2c {
    fn default() -> I2c {
        I2c {
            regs: [0; (I2C1_SIZE / 4) as usize],
            kbd: None,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            exp: [Expander::new(), Expander::new()],
            touch: Gt911::default(),
            target: None,
            transfers: 0,
            exp2_writes: 0,
        }
    }
}

impl I2c {
    pub fn read(&mut self, off: u32) -> u32 {
        match off {
            DATA => self.rx.pop_front().unwrap_or(0) as u32,
            SR => ((self.rx.len() as u32) << 8) | ((self.tx.len() as u32) << 18),
            _ => self.regs[(off / 4) as usize],
        }
    }

    pub fn write(&mut self, off: u32, v: u32) {
        match off {
            DATA => self.tx.push_back(v as u8),
            INT_CLR => self.regs[(INT_RAW / 4) as usize] &= !v,
            FIFO_CONF => {
                if v & FIFO_TX_RST != 0 {
                    self.tx.clear();
                }
                if v & FIFO_RX_RST != 0 {
                    self.rx.clear();
                }
                self.regs[(off / 4) as usize] = v;
            }
            // Bus clear and the FSM reset finish at once.
            SCL_SP_CONF => self.regs[(off / 4) as usize] = v & !1,
            CTR => {
                self.regs[(off / 4) as usize] = v & !(CTR_TRANS_START | (1 << 10) | (1 << 11));
                if v & CTR_TRANS_START != 0 {
                    self.run();
                }
            }
            _ => self.regs[(off / 4) as usize] = v,
        }
    }

    fn run(&mut self) {
        self.transfers += 1;
        let mut raw = 0;
        for i in 0..8 {
            let c = self.regs[((COMD0 + 4 * i) / 4) as usize];
            let n = (c & 0xFF) as usize;
            match (c >> 11) & 7 {
                6 => self.target = None,                     // RSTART
                1 => {
                    // WRITE: the first byte after a start is the address.
                    for _ in 0..n {
                        let b = self.tx.pop_front().unwrap_or(0);
                        if self.target.is_none() {
                            let addr = b >> 1;
                            if !self.start(addr, b & 1 == 1) {
                                raw |= INT_NACK;
                                break;
                            }
                            self.target = Some(addr);
                        } else {
                            self.write_byte(b);
                        }
                    }
                    if raw & INT_NACK != 0 {
                        break;
                    }
                }
                3 => {
                    for _ in 0..n {
                        let b = self.read_byte();
                        self.rx.push_back(b);
                    }
                }
                2 => {
                    raw |= INT_TRANS_COMPLETE; // STOP
                    break;
                }
                _ => break,
            }
        }
        self.regs[(INT_RAW / 4) as usize] |= raw;
    }

    /// A start addressed to `addr`; whether a device acknowledges.
    fn start(&mut self, addr: u8, read: bool) -> bool {
        match addr {
            0x43 | 0x44 => {
                if !read {
                    self.exp[(addr - 0x43) as usize].written = 0;
                }
                true
            }
            0x14 => {
                if !read {
                    self.touch.written = 0;
                }
                true
            }
            KBD_ADDR if self.kbd.is_some() => {
                if !read {
                    self.kbd.as_mut().unwrap().written = 0;
                }
                true
            }
            _ => false,
        }
    }

    fn write_byte(&mut self, b: u8) {
        match self.target {
            Some(a @ (0x43 | 0x44)) => {
                let e = &mut self.exp[(a - 0x43) as usize];
                if e.written == 0 {
                    e.reg = b;
                } else if a == 0x44 {
                    // The kernel must never write #2: one of its bits
                    // powers the Tab5 off (doc/17 §2).
                    self.exp2_writes += 1;
                } else if (e.reg as usize) < e.regs.len() && e.reg != 0x0F {
                    e.regs[e.reg as usize] = b;
                    e.reg += 1;
                }
                e.written += 1;
            }
            Some(KBD_ADDR) => {
                // Register, then data (mode and interrupt settings: taken
                // as given).
                let k = self.kbd.as_mut().unwrap();
                if k.written == 0 {
                    k.reg = b;
                }
                k.written += 1;
            }
            Some(0x14) => {
                // Register (big-endian), then data: writing the status
                // clears it; the fixed touch reports again next time.
                let t = &mut self.touch;
                match t.written {
                    0 => t.reg = b as u16,
                    1 => t.reg = (t.reg << 8) | b as u16,
                    _ => {}
                }
                t.written += 1;
            }
            _ => {}
        }
    }

    fn read_byte(&mut self) -> u8 {
        match self.target {
            Some(a @ (0x43 | 0x44)) => {
                let e = &mut self.exp[(a - 0x43) as usize];
                let v = e.regs.get(e.reg as usize).copied().unwrap_or(0);
                e.reg = e.reg.wrapping_add(1);
                v
            }
            Some(KBD_ADDR) => self.kbd.as_mut().unwrap().read_byte(),
            Some(0x14) => {
                let v = self.touch.read_reg(self.touch.reg);
                self.touch.reg = self.touch.reg.wrapping_add(1);
                v
            }
            _ => 0xFF,
        }
    }

    /// Direction and output registers of expander #1 (0x43), for tests.
    pub fn expander1(&self) -> (u8, u8) {
        (self.exp[0].regs[0x03], self.exp[0].regs[0x05])
    }
}
