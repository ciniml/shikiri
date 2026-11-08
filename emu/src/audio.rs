//! The AHB-GDMA (0x5008_5000) and I2S0 (0x500C_6000) for audio (doc/70):
//! enough for kumiki-p4v1-hal's I2S driver in the kernel.
//!
//! - The DMA's register file, with the per-channel interrupt blocks
//!   (RAW / ST = RAW & ENA / ENA / CLR) and the link registers. A direction
//!   runs from `*_LINK` START with the address in the link address register.
//! - Time: a running direction moves while I2S0's `C_START` is set for it,
//!   at 48 kHz (4 bytes a frame, 16-bit stereo; the dividers are not
//!   decoded). TX takes each descriptor's `length` bytes: optionally kept
//!   (`--audio-out`), `OUT_EOF_DES_ADDR` set to it, `OUT_DONE | OUT_EOF`
//!   raised. RX fills each descriptor with up to `RXEOF_NUM + 1` bytes of a
//!   440 Hz tone (the hardware closed descriptors at 256 bytes with 255,
//!   kumiki i2s.rs), sets `length` and `suc_eof`, clears the owner bit
//!   (owner checking is off, so a full ring is overwritten as on the chip),
//!   sets `IN_SUC_EOF_DES_ADDR` and raises `IN_DONE | IN_SUC_EOF`.
//! - I2S0: a register file; `C_UPDATE`, `C_RESET` and `C_FIFO_RESET` clear
//!   themselves.
//!
//! Interrupt sources (rev v1, interrupt_core0_reg.h): IN_CHn 56 + n,
//! OUT_CHn 59 + n. `Peripherals::audio_irq` carries them to the interrupt
//! matrix and `audio_next` to `wfi`.

use crate::bus::Bus;
use crate::periph::SYSTIMER_HZ;

pub const DMA_BASE: u32 = 0x5008_5000;
const DMA_SIZE: u32 = 0x400;
pub const I2S_BASE: u32 = 0x500C_6000;
const I2S_SIZE: u32 = 0x100;
pub const SRC_IN0: usize = 56;
pub const SRC_OUT0: usize = 59;
const CHANNELS: usize = 3;

const fn in_int(n: usize) -> u32 {
    0x10 * n as u32
}
const fn out_int(n: usize) -> u32 {
    0x30 + 0x10 * n as u32
}
const fn in_ch(n: usize) -> u32 {
    0x70 + 0xC0 * n as u32
}
const fn out_ch(n: usize) -> u32 {
    0xD0 + 0xC0 * n as u32
}
const fn in_link_addr(n: usize) -> u32 {
    0x3AC + 4 * n as u32
}
const fn out_link_addr(n: usize) -> u32 {
    0x3B8 + 4 * n as u32
}
const INT_RAW: u32 = 0x0;
const INT_ST: u32 = 0x4;
const INT_ENA: u32 = 0x8;
const INT_CLR: u32 = 0xC;
const LINK: u32 = 0x10;
const EOF_DES_ADDR: u32 = 0x18;
const DSCR: u32 = 0x20;
const INLINK_STOP: u32 = 1 << 1;
const INLINK_START: u32 = 1 << 2;
const OUTLINK_STOP: u32 = 1 << 0;
const OUTLINK_START: u32 = 1 << 1;
/// DONE and EOF (TX) / SUC_EOF (RX).
const INT_DONE_EOF: u32 = 0b11;

const RX_CONF: u32 = 0x20;
const TX_CONF: u32 = 0x24;
const RXEOF_NUM: u32 = 0x64;
const C_RESET: u32 = 1 << 0;
const C_FIFO_RESET: u32 = 1 << 1;
const C_START: u32 = 1 << 2;
const C_UPDATE: u32 = 1 << 8;

const SUC_EOF: u32 = 1 << 30;
const RATE: u64 = 48_000;
const FRAME: u32 = 4;
const TONE_HZ: f64 = 440.0;
const TONE_AMPLITUDE: f64 = 4096.0;

/// One direction of one channel.
#[derive(Default, Clone, Copy)]
struct Link {
    /// Started and not stopped (or run off the end of its list).
    running: bool,
    /// The descriptor it works on.
    cur: u32,
    /// When it finishes `cur`, in ticks * 3 (a frame is 1000/3 ticks), or
    /// `None` until both the link and the I2S direction run.
    due_x3: Option<u64>,
}

pub struct Audio {
    dma: Vec<u32>,
    i2s: Vec<u32>,
    tx: [Link; CHANNELS],
    rx: [Link; CHANNELS],
    /// The earliest `due` of all links (ticks), u64::MAX when none.
    pub next_at: u64,
    /// `--audio-out`: what TX played is kept here.
    pub capture: bool,
    pub played: Vec<u8>,
    /// Frames of the tone written so far.
    tone_frames: u64,
    /// Descriptors sent and received.
    pub stats: (u64, u64),
}

impl Default for Audio {
    fn default() -> Audio {
        Audio {
            dma: vec![0; (DMA_SIZE / 4) as usize],
            i2s: vec![0; (I2S_SIZE / 4) as usize],
            tx: Default::default(),
            rx: Default::default(),
            next_at: u64::MAX,
            capture: false,
            played: Vec::new(),
            tone_frames: 0,
            stats: (0, 0),
        }
    }
}

pub fn contains(addr: u32) -> bool {
    (DMA_BASE..DMA_BASE + DMA_SIZE).contains(&addr) || (I2S_BASE..I2S_BASE + I2S_SIZE).contains(&addr)
}

impl Audio {
    fn d(&self, off: u32) -> u32 {
        self.dma[(off / 4) as usize]
    }

    fn set_d(&mut self, off: u32, v: u32) {
        self.dma[(off / 4) as usize] = v;
    }

    fn i2s_started(&self, tx: bool) -> bool {
        self.i2s[((if tx { TX_CONF } else { RX_CONF }) / 4) as usize] & C_START != 0
    }

    /// Starts timing the links whose I2S direction runs, stops it for the
    /// others; recomputes `next_at`.
    fn reschedule(&mut self, ticks: u64) {
        let (tx_on, rx_on) = (self.i2s_started(true), self.i2s_started(false));
        // The first descriptor is read when it is due: timed as a 512-byte
        // one (kumiki's), closed at RXEOF_NUM on RX.
        let rx_first = 512.min((self.i2s[(RXEOF_NUM / 4) as usize] & 0xFFF) + 1);
        for (links, on, first) in [(&mut self.tx, tx_on, 512), (&mut self.rx, rx_on, rx_first)] {
            for l in links.iter_mut() {
                l.due_x3 = match (l.running && on, l.due_x3) {
                    (false, _) => None,
                    (true, Some(d)) => Some(d),
                    (true, None) => Some(ticks * 3 + frames_x3(first)),
                };
            }
        }
        self.next_at = self.tx.iter().chain(&self.rx).filter_map(|l| l.due_x3).map(|d| d.div_ceil(3)).min().unwrap_or(u64::MAX);
    }

    /// The interrupt lines: bit n IN_CHn, bit 3 + n OUT_CHn.
    fn irq(&self) -> u8 {
        let mut m = 0;
        for n in 0..CHANNELS {
            if self.d(in_int(n) + INT_RAW) & self.d(in_int(n) + INT_ENA) != 0 {
                m |= 1 << n;
            }
            if self.d(out_int(n) + INT_RAW) & self.d(out_int(n) + INT_ENA) != 0 {
                m |= 1 << (3 + n);
            }
        }
        m
    }
}

/// Ticks * 3 for `bytes` of 16-bit stereo at 48 kHz.
fn frames_x3(bytes: u32) -> u64 {
    (bytes / FRAME) as u64 * SYSTIMER_HZ * 3 / RATE
}

fn sync(bus: &mut Bus) {
    bus.periph.audio_irq = bus.audio.irq();
    bus.periph.audio_next = (bus.audio.next_at != u64::MAX).then_some(bus.audio.next_at);
}

/// Moves every running link up to `ticks`.
pub fn advance(bus: &mut Bus, ticks: u64) {
    loop {
        // The link due first.
        let mut first: Option<(bool, usize, u64)> = None;
        for (tx, links) in [(true, &bus.audio.tx), (false, &bus.audio.rx)] {
            for (n, l) in links.iter().enumerate() {
                if let Some(d) = l.due_x3
                    && d <= ticks * 3
                    && first.is_none_or(|f| d < f.2)
                {
                    first = Some((tx, n, d));
                }
            }
        }
        let Some((tx, n, due)) = first else { break };
        let next_len = if tx { send(bus, n) } else { receive(bus, n) };
        let links = if tx { &mut bus.audio.tx } else { &mut bus.audio.rx };
        links[n].due_x3 = next_len.map(|len| due + frames_x3(len.max(FRAME)));
        if next_len.is_none() {
            links[n].running = false;
        }
    }
    bus.audio.reschedule(ticks);
    sync(bus);
}

/// TX finishes its descriptor; the length of the next, or `None` at the end
/// of the list.
fn send(bus: &mut Bus, n: usize) -> Option<u32> {
    let cur = bus.audio.tx[n].cur;
    let dw0 = bus.dma_read(cur, 4);
    let buf = bus.dma_read(cur + 4, 4);
    let next = bus.dma_read(cur + 8, 4);
    let len = (dw0 >> 12) & 0xFFF;
    if bus.audio.capture {
        for i in 0..len {
            let b = bus.dma_read(buf + i, 1) as u8;
            bus.audio.played.push(b);
        }
    }
    let a = &mut bus.audio;
    a.stats.0 += 1;
    if dw0 & SUC_EOF != 0 {
        a.set_d(out_ch(n) + EOF_DES_ADDR, cur);
        let raw = a.d(out_int(n) + INT_RAW);
        a.set_d(out_int(n) + INT_RAW, raw | INT_DONE_EOF);
    }
    a.set_d(out_ch(n) + DSCR, next);
    a.tx[n].cur = next;
    (next != 0).then(|| (bus.dma_read(next, 4) >> 12) & 0xFFF)
}

/// RX closes its descriptor with tone; the size of the next, or `None`.
fn receive(bus: &mut Bus, n: usize) -> Option<u32> {
    let cur = bus.audio.rx[n].cur;
    let dw0 = bus.dma_read(cur, 4);
    let buf = bus.dma_read(cur + 4, 4);
    let next = bus.dma_read(cur + 8, 4);
    let size = dw0 & 0xFFF;
    let eof_num = bus.audio.i2s[(RXEOF_NUM / 4) as usize] & 0xFFF;
    let len = size.min(eof_num + 1) / FRAME * FRAME;
    for f in 0..len / FRAME {
        let t = (bus.audio.tone_frames + f as u64) as f64 / RATE as f64;
        let s = ((t * TONE_HZ * core::f64::consts::TAU).sin() * TONE_AMPLITUDE) as i16 as u16 as u32;
        bus.dma_write(buf + f * FRAME, 4, s | (s << 16));
    }
    bus.audio.tone_frames += (len / FRAME) as u64;
    bus.dma_write(cur, 4, size | (len << 12) | SUC_EOF);
    let a = &mut bus.audio;
    a.stats.1 += 1;
    a.set_d(in_ch(n) + EOF_DES_ADDR, cur);
    a.set_d(in_ch(n) + DSCR, next);
    let raw = a.d(in_int(n) + INT_RAW);
    a.set_d(in_int(n) + INT_RAW, raw | INT_DONE_EOF);
    a.rx[n].cur = next;
    // The next one closes after as many bytes (the same size and EOF count).
    (next != 0).then_some(len)
}

pub fn read(bus: &mut Bus, addr: u32) -> u32 {
    let ticks = bus.ticks();
    advance(bus, ticks);
    let a = &bus.audio;
    if addr >= I2S_BASE {
        return a.i2s[((addr - I2S_BASE) / 4) as usize];
    }
    let off = addr - DMA_BASE;
    for n in 0..CHANNELS {
        for base in [in_int(n), out_int(n)] {
            if off == base + INT_ST {
                return a.d(base + INT_RAW) & a.d(base + INT_ENA);
            }
        }
    }
    a.d(off)
}

pub fn write(bus: &mut Bus, addr: u32, v: u32) {
    let ticks = bus.ticks();
    advance(bus, ticks);
    let a = &mut bus.audio;
    if addr >= I2S_BASE {
        let off = addr - I2S_BASE;
        let v = if off == TX_CONF || off == RX_CONF { v & !(C_UPDATE | C_RESET | C_FIFO_RESET) } else { v };
        a.i2s[(off / 4) as usize] = v;
    } else {
        let off = addr - DMA_BASE;
        let mut handled = false;
        for n in 0..CHANNELS {
            for base in [in_int(n), out_int(n)] {
                if off == base + INT_CLR {
                    let raw = a.d(base + INT_RAW);
                    a.set_d(base + INT_RAW, raw & !v);
                    handled = true;
                } else if off == base + INT_RAW || off == base + INT_ST {
                    handled = true;
                }
            }
            if off == out_ch(n) + LINK {
                if v & OUTLINK_STOP != 0 {
                    a.tx[n] = Link::default();
                }
                if v & OUTLINK_START != 0 {
                    a.tx[n] = Link { running: true, cur: a.d(out_link_addr(n)), due_x3: None };
                }
                handled = true;
            }
            if off == in_ch(n) + LINK {
                if v & INLINK_STOP != 0 {
                    a.rx[n] = Link::default();
                }
                if v & INLINK_START != 0 {
                    a.rx[n] = Link { running: true, cur: a.d(in_link_addr(n)), due_x3: None };
                }
                handled = true;
            }
        }
        if !handled {
            a.set_d(off, v);
        }
    }
    bus.audio.reschedule(ticks);
    sync(bus);
}

/// `played` as a WAV file (16-bit stereo, 48 kHz).
pub fn wav(played: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(44 + played.len());
    let len = played.len() as u32;
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&(RATE as u32).to_le_bytes());
    w.extend_from_slice(&(RATE as u32 * FRAME).to_le_bytes());
    w.extend_from_slice(&(FRAME as u16).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&len.to_le_bytes());
    w.extend_from_slice(played);
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::L2MEM_BASE;

    const RING: u32 = L2MEM_BASE + 0x8_0000;

    fn w(bus: &mut Bus, addr: u32, v: u32) {
        assert!(bus.write(addr, 4, v));
    }

    /// Two 512-byte descriptors each way on channel 1, as kumiki's driver
    /// sets them up; run 10 ms.
    #[test]
    fn rings_move_at_48k_and_interrupt() {
        let mut bus = Bus::new(360_000_000, crate::cache::Mode::Off, 1);
        bus.audio.capture = true;
        let (tx, rx) = (RING, RING + 0x40);
        let (tb, rb) = (RING + 0x100, RING + 0x1000);
        for i in 0..2u32 {
            let next = |b: u32| b + 12 * ((i + 1) % 2);
            bus.dma_write(tx + 12 * i, 4, 512 | (512 << 12) | SUC_EOF | (1 << 31));
            bus.dma_write(tx + 12 * i + 4, 4, tb + 512 * i);
            bus.dma_write(tx + 12 * i + 8, 4, next(tx));
            bus.dma_write(rx + 12 * i, 4, 512 | (1 << 31));
            bus.dma_write(rx + 12 * i + 4, 4, rb + 512 * i);
            bus.dma_write(rx + 12 * i + 8, 4, next(rx));
        }
        for k in 0..512 {
            bus.dma_write(tb + k, 1, 0x11);
        }
        w(&mut bus, I2S_BASE + RXEOF_NUM, 255);
        w(&mut bus, DMA_BASE + out_int(1) + INT_ENA, 0b10);
        w(&mut bus, DMA_BASE + in_int(1) + INT_ENA, 0b10);
        w(&mut bus, DMA_BASE + out_link_addr(1), tx);
        w(&mut bus, DMA_BASE + out_ch(1) + LINK, OUTLINK_START);
        w(&mut bus, DMA_BASE + in_link_addr(1), rx);
        w(&mut bus, DMA_BASE + in_ch(1) + LINK, INLINK_START);
        // Nothing moves before the I2S starts.
        bus.cycles += bus.cycles_for_ticks(SYSTIMER_HZ / 100);
        let t = bus.ticks();
        advance(&mut bus, t);
        assert_eq!(bus.audio.stats, (0, 0));
        w(&mut bus, I2S_BASE + TX_CONF, C_START | C_UPDATE);
        w(&mut bus, I2S_BASE + RX_CONF, C_START | C_UPDATE);
        assert_eq!(bus.read(I2S_BASE + TX_CONF, 4), Some(C_START), "C_UPDATE clears itself");
        // 10 ms: 480 frames, 3 TX descriptors (128 frames each), 7 RX
        // (64 frames at RXEOF_NUM 255).
        let t0 = bus.ticks();
        bus.cycles = bus.cycles_for_ticks(t0 + SYSTIMER_HZ / 100);
        let t = bus.ticks();
        advance(&mut bus, t);
        assert_eq!(bus.audio.stats, (3, 7));
        assert_eq!(bus.read(DMA_BASE + out_ch(1) + EOF_DES_ADDR, 4), Some(tx), "the third is descriptor 0 again");
        assert_eq!(bus.periph.audio_irq, (1 << 1) | (1 << 4));
        assert_eq!(bus.read(DMA_BASE + out_int(1) + INT_ST, 4), Some(0b10));
        w(&mut bus, DMA_BASE + out_int(1) + INT_CLR, 0b11);
        w(&mut bus, DMA_BASE + in_int(1) + INT_CLR, 0b11);
        assert_eq!(bus.periph.audio_irq, 0);
        assert_eq!(&bus.audio.played[..512], &[0x11; 512][..]);
        // A closed RX descriptor: 256 bytes, suc_eof, owner cleared, tone.
        let dw0 = bus.dma_read(rx, 4);
        assert_eq!(dw0, 512 | (256 << 12) | SUC_EOF);
        let s = (1..64).map(|f| bus.dma_read(rb + 4 * f, 2) as u16 as i16).map(|v| v.unsigned_abs()).max().unwrap();
        assert!(s > 1000, "tone {s}");
        assert!(bus.audio.next_at != u64::MAX);
    }
}
