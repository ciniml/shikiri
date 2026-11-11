//! The display path of M6b (doc/17 §7), as far as the kernel's bring-up
//! sees it: the DSI host reports a locked PHY and empty command FIFOs (DCS
//! reads get no answer), the bridge and the rest are plain registers, and
//! DW-GDMA channel 1, once enabled, "sends" a frame per frame time and
//! raises BLOCK_TFR_DONE (interrupt source 24) after each, as with the
//! kernel's ring of list items, until aborted. The list items themselves and
//! the pixels are not modelled.

pub const HOST_BASE: u32 = 0x500A_0000;
pub const HOST_SIZE: u32 = 0x1000;
pub const GDMA_BASE: u32 = 0x5008_1000;
pub const GDMA_SIZE: u32 = 0x500;
pub const SRC_GDMA: usize = 24;

const PHY_STATUS: u32 = 0xB0;
const CMD_PKT_STATUS: u32 = 0x74;
const BRG_DPI_CONFIG_UPDATE: u32 = 0x844;

const CHEN0: u32 = 0x18;
const CHEN1: u32 = 0x1C;
const RESET0: u32 = 0x58;
const CH1_LLP0: u32 = 0x128;
const CH1_STATUS0: u32 = 0x130;
const CH1_INTSTATUS0: u32 = 0x188;
const CH1_INTSIGNAL_ENABLE0: u32 = 0x190;
const CH1_INTCLEAR0: u32 = 0x198;
const BLOCK_TFR_DONE: u32 = 1 << 0;

/// One 940 x 1324 frame at 80 MHz, in 16 MHz SYSTIMER ticks.
const FRAME_TICKS: u64 = 940 * 1324 / 5;

#[derive(Default)]
pub struct Dsi {
    host: crate::regmap::RegMap,
    gdma: crate::regmap::RegMap,
    /// The interrupt line (status & enable), kept as they change: `irq` is
    /// asked every instruction.
    irq_level: bool,
    /// When the frame in flight ends (ticks); None while stopped.
    busy_until: Option<u64>,
    pub frames: u64,
    /// LLP0 of the last start (the list item's address | LMS).
    pub last_llp: u32,
}

impl Dsi {
    pub fn host_read(&mut self, off: u32) -> u32 {
        match off {
            PHY_STATUS => 0x95,             // locked; clock lane and lanes 0, 1 stopped
            CMD_PKT_STATUS => 0x15,         // command, write and read FIFOs empty
            BRG_DPI_CONFIG_UPDATE => 0,
            _ => self.host.get(&off).copied().unwrap_or(match off {
                0x880 => 1, // bridge HOST_CTRL: DSI_CFG_REF_CLK_EN
                _ => 0,
            }),
        }
    }

    pub fn host_write(&mut self, off: u32, v: u32) {
        self.host.insert(off, v);
    }

    pub fn gdma_read(&mut self, off: u32, ticks: u64) -> u32 {
        self.update(ticks);
        match off {
            RESET0 => 0,
            CHEN0 => self.busy_until.is_some() as u32,
            // The block in flight's count, as the chip shows it while
            // sending (the kernel checks that the frame DMA started).
            CH1_STATUS0 => if self.busy_until.is_some() { 230_399 } else { 0 },
            _ => self.gdma.get(&off).copied().unwrap_or(0),
        }
    }

    pub fn gdma_write(&mut self, off: u32, v: u32, ticks: u64) {
        self.update(ticks);
        match off {
            CHEN0 if v & 0x101 == 0x101 => {
                self.busy_until = Some(ticks + FRAME_TICKS);
                self.last_llp = self.gdma.get(&CH1_LLP0).copied().unwrap_or(0);
            }
            CHEN1 if v & 0x101 == 0x101 => self.busy_until = None, // abort
            CH1_INTCLEAR0 => {
                let st = self.gdma.entry(CH1_INTSTATUS0).or_default();
                *st &= !v;
            }
            _ => {
                self.gdma.insert(off, v);
            }
        }
        self.relevel();
    }

    fn relevel(&mut self) {
        let st = self.gdma.get(&CH1_INTSTATUS0).copied().unwrap_or(0);
        let en = self.gdma.get(&CH1_INTSIGNAL_ENABLE0).copied().unwrap_or(0);
        self.irq_level = st & en != 0;
    }

    fn update(&mut self, ticks: u64) {
        while let Some(t) = self.busy_until
            && ticks >= t
        {
            self.busy_until = Some(t + FRAME_TICKS);
            self.frames += 1;
            *self.gdma.entry(CH1_INTSTATUS0).or_default() |= BLOCK_TFR_DONE;
            self.relevel();
        }
    }

    /// The level of DW-GDMA's interrupt line.
    pub fn irq(&mut self, ticks: u64) -> bool {
        self.update(ticks);
        self.irq_level
    }

    /// When the frame in flight ends, for `wfi`.
    pub fn next_event(&self) -> Option<u64> {
        self.busy_until
    }
}
