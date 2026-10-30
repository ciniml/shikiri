//! I2C1 (0x500C_5000) with the Tab5's internal bus devices that M6a talks
//! to (doc/17 §7): the two PI4IOE5V6408 IO expanders (0x43, 0x44) and the
//! GT911 touch controller (0x14) of the ILI9881C lot, as on the Tab5 at hand,
//! reporting one fixed touch point.
//! The command list runs at once when TRANS_START is set; timing is not
//! modelled. Register layout: doc/research/m6-panel-touch.md §1.3.

use std::collections::VecDeque;

pub const I2C1_BASE: u32 = 0x500C_5000;
pub const I2C1_SIZE: u32 = 0x100;

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

pub struct I2c {
    regs: [u32; (I2C1_SIZE / 4) as usize],
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
