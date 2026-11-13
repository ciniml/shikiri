// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The SDMMC host (Synopsys DesignWare MMC) and a microSD card on slot 0,
//! as the kernel's `platform/esp32p4v1/sd.rs` drives them (doc/64): a
//! command runs at once, when CMD is written with its start bit; data moves
//! through the internal DMA's (IDMAC) descriptor at DBADDR, on memory
//! itself. The card is an SDHC card (block addressed) whose sectors are a
//! raw disk image (`--sd FILE`), kept in memory. Without an image, and on
//! slot 1 (the Wi-Fi chip's SDIO), commands get no response.

use crate::bus::Bus;

pub const BASE: u32 = 0x5008_3000;
const SIZE: u32 = 0x1000;

const CTRL: u32 = 0x00;
const CMDARG: u32 = 0x28;
const CMD: u32 = 0x2C;
const RESP0: u32 = 0x30;
const RINTSTS: u32 = 0x44;
const STATUS: u32 = 0x48;
const CDETECT: u32 = 0x50;
const BYTCNT: u32 = 0x20;
const VERID: u32 = 0x6C;
const DBADDR: u32 = 0x88;
const IDSTS: u32 = 0x8C;

/// controller, FIFO and DMA resets: done at once.
const CTRL_RESETS: u32 = 0b111;
const CMD_START: u32 = 1 << 31;
const CMD_UPDATE_CLOCK: u32 = 1 << 21;
const CMD_RESP_EXPECT: u32 = 1 << 6;
const CMD_RESP_LONG: u32 = 1 << 7;
const CMD_DATA: u32 = 1 << 9;
const CMD_WRITE: u32 = 1 << 10;
const CMD_DONE: u32 = 1 << 2;
const DTO: u32 = 1 << 3;
const RTO: u32 = 1 << 8;
const IDSTS_TI: u32 = 1 << 0;
const IDSTS_RI: u32 = 1 << 1;
const DES0_OWN: u32 = 1 << 31;

const SECTOR: usize = 512;
const RCA: u32 = 0x5E01;
/// R1's card status: ready for data, and the state (4: transfer).
const R1_READY: u32 = 1 << 8;
const R1_APP_CMD: u32 = 1 << 5;

pub fn contains(addr: u32) -> bool {
    (BASE..BASE + SIZE).contains(&addr)
}

/// The card's states that matter here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Ready,
    Ident,
    Stby,
    Tran,
}

pub struct Sdmmc {
    regs: Vec<u32>,
    /// The card's sectors (`--sd`), if a card is in.
    pub image: Option<Vec<u8>>,
    state: State,
    /// The last command was CMD55: this one is an application command.
    app: bool,
    /// Commands run, sectors read and written (for the summary).
    pub stats: (u64, u64, u64),
}

impl Default for Sdmmc {
    fn default() -> Sdmmc {
        Sdmmc { regs: vec![0; (SIZE / 4) as usize], image: None, state: State::Idle, app: false, stats: (0, 0, 0) }
    }
}

impl Sdmmc {
    fn reg(&self, off: u32) -> u32 {
        self.regs[(off / 4) as usize]
    }

    fn set(&mut self, off: u32, v: u32) {
        self.regs[(off / 4) as usize] = v;
    }

    pub fn read(&self, off: u32) -> u32 {
        match off {
            // Never busy: data is programmed at once.
            STATUS => 0,
            // Card detect (low = present) on slot 0.
            CDETECT => !(self.image.is_some() as u32) & 1,
            VERID => 0x5342_270A,
            _ => self.reg(off),
        }
    }

    fn sectors(&self) -> u32 {
        self.image.as_ref().map_or(0, |i| (i.len() / SECTOR) as u32)
    }

    fn r1(&self) -> u32 {
        let state = match self.state {
            State::Idle => 0,
            State::Ready => 1,
            State::Ident => 2,
            State::Stby => 3,
            State::Tran => 4,
        };
        state << 9 | R1_READY | if self.app { R1_APP_CMD } else { 0 }
    }

    /// The CSD (version 2.0) as the host's response registers hold it:
    /// word `b / 32` has bit `b`.
    fn csd(&self) -> [u32; 4] {
        let c_size = (self.sectors() / 1024).saturating_sub(1);
        let mut bits = [0u32; 4];
        let mut put = |hi: u32, lo: u32, v: u32| {
            for b in lo..=hi {
                if v >> (b - lo) & 1 != 0 {
                    bits[(b / 32) as usize] |= 1 << (b % 32);
                }
            }
        };
        put(127, 126, 1); // CSD_STRUCTURE 2.0
        put(111, 104, 0x0E); // TAAC
        put(103, 96, 0x32); // TRAN_SPEED: 25 MHz
        put(95, 84, 0x5B5); // CCC
        put(83, 80, 9); // READ_BL_LEN: 512
        put(69, 48, c_size);
        put(25, 22, 9); // WRITE_BL_LEN
        bits
    }

    /// The CID: manufacturer 0xEE, OEM "T5", name "T5EMU" (bits 103:64).
    fn cid() -> [u32; 4] {
        let w = |b: [u8; 4]| u32::from_be_bytes(b);
        [0, 0x0100_0000, w(*b"5EMU"), w([0xEE, b'T', b'5', b'T'])]
    }

    /// Runs command `index` with `arg`: the response (4 words), or `None`
    /// for none (a timeout). `data` gets a read's bytes; a write's bytes
    /// come in `written`.
    fn command(&mut self, index: u32, arg: u32, data: &mut Vec<u8>, len: usize, written: &[u8]) -> Option<[u32; 4]> {
        let app = core::mem::take(&mut self.app);
        let r1 = |s: &Sdmmc| Some([s.r1(), 0, 0, 0]);
        match (app, index) {
            (_, 0) => {
                self.state = State::Idle;
                Some([0; 4])
            }
            (false, 8) => Some([arg & 0xFFF, 0, 0, 0]),
            (_, 55) => {
                self.app = true;
                let r = self.r1();
                Some([r | R1_APP_CMD, 0, 0, 0])
            }
            (true, 41) => {
                // Powered up at once; SDHC if the host supports it.
                if arg & 0x00FF_8000 != 0 {
                    self.state = State::Ready;
                }
                let ccs = if arg & (1 << 30) != 0 { 1 << 30 } else { 0 };
                Some([1 << 31 | ccs | 0x00FF_8000, 0, 0, 0])
            }
            (false, 2) => {
                self.state = State::Ident;
                Some(Self::cid())
            }
            (false, 3) => {
                self.state = State::Stby;
                Some([RCA << 16 | 0x0500, 0, 0, 0])
            }
            (false, 9) => Some(self.csd()),
            (false, 7) => {
                self.state = if arg >> 16 == RCA { State::Tran } else { State::Stby };
                r1(self)
            }
            (false, 13 | 16 | 12) => r1(self),
            (true, 6) => r1(self),
            (true, 51) => {
                // SCR: SD 2.0 (spec 2), 1 and 4 data lines.
                data.extend_from_slice(&[0x02, 0x05, 0, 0, 0, 0, 0, 0]);
                r1(self)
            }
            (false, 6) => {
                // Switch status: group 1 function 1 (High Speed) selected.
                let mut st = [0u8; 64];
                st[16] = if arg & 0xF == 1 { 1 } else { 0 };
                data.extend_from_slice(&st);
                r1(self)
            }
            (false, 17 | 18) => {
                let image = self.image.as_ref()?;
                let start = arg as usize * SECTOR;
                match image.get(start..start + len) {
                    Some(b) => data.extend_from_slice(b),
                    None => return None,
                }
                self.stats.1 += (len / SECTOR) as u64;
                r1(self)
            }
            (false, 24 | 25) => {
                let start = arg as usize * SECTOR;
                let image = self.image.as_mut()?;
                image.get_mut(start..start + written.len())?.copy_from_slice(written);
                self.stats.2 += (written.len() / SECTOR) as u64;
                r1(self)
            }
            _ => None,
        }
    }
}

/// A read of a host register.
pub fn read(bus: &Bus, addr: u32) -> u32 {
    bus.sd.read(addr - BASE)
}

/// A write of a host register: W1C for the interrupt status registers, a
/// command when CMD gets its start bit.
pub fn write(bus: &mut Bus, addr: u32, v: u32) {
    let off = addr - BASE;
    match off {
        CTRL => bus.sd.set(CTRL, v & !CTRL_RESETS),
        RINTSTS | IDSTS => {
            let old = bus.sd.reg(off);
            bus.sd.set(off, old & !v);
        }
        CMD if v & CMD_START != 0 => {
            bus.sd.set(CMD, v & !CMD_START);
            if v & CMD_UPDATE_CLOCK != 0 {
                return;
            }
            run(bus, v);
        }
        _ => bus.sd.set(off, v),
    }
}

/// Runs a command written to CMD.
fn run(bus: &mut Bus, cmd: u32) {
    bus.sd.stats.0 += 1;
    let slot = (cmd >> 16) & 0x1F;
    let index = cmd & 0x3F;
    let arg = bus.sd.reg(CMDARG);
    let data_phase = cmd & CMD_DATA != 0;
    let len = bus.sd.reg(BYTCNT) as usize;
    // The descriptor: DES2 is the buffer (one descriptor, as the kernel
    // sets up).
    let desc = bus.sd.reg(DBADDR);
    let buf = bus.dma_read(desc + 8, 4);
    let mut written = Vec::new();
    if data_phase && cmd & CMD_WRITE != 0 {
        written = (0..len as u32).map(|i| bus.dma_read(buf + i, 1) as u8).collect();
    }
    let mut data = Vec::new();
    let resp = if slot == 0 && bus.sd.image.is_some() { bus.sd.command(index, arg, &mut data, len, &written) } else { None };
    let mut ints = bus.sd.reg(RINTSTS) | CMD_DONE;
    match resp {
        None if cmd & CMD_RESP_EXPECT != 0 || data_phase => ints |= RTO,
        None => {}
        Some(r) => {
            let r = if cmd & CMD_RESP_LONG != 0 { r } else { [r[0], 0, 0, 0] };
            for (k, w) in r.iter().enumerate() {
                bus.sd.set(RESP0 + 4 * k as u32, *w);
            }
            if data_phase {
                let mut ids = bus.sd.reg(IDSTS);
                if cmd & CMD_WRITE != 0 {
                    ids |= IDSTS_TI;
                } else {
                    for (i, b) in data.iter().take(len).enumerate() {
                        bus.dma_write(buf + i as u32, 1, *b as u32);
                    }
                    ids |= IDSTS_RI;
                }
                bus.sd.set(IDSTS, ids);
                let des0 = bus.dma_read(desc, 4);
                bus.dma_write(desc, 4, des0 & !DES0_OWN);
                ints |= DTO;
            }
        }
    }
    bus.sd.set(RINTSTS, ints);
}
