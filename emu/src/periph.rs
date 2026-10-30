//! Peripherals of the ESP32-P4 rev v1.x that the kernel uses. Register
//! layouts come from ESP-IDF 6.0 `components/soc/esp32p4/register/hw_ver1`
//! and `components/soc/esp32p4/include/soc/clic_reg.h` (doc/10 §2.1).
//! Behaviour that was checked on the Tab5 is noted as such.

pub const USJ_BASE: u32 = 0x500D_2000;
pub const SYSTIMER_BASE: u32 = 0x500E_2000;
pub const INTMTX_BASE: u32 = 0x500D_6000;
pub const LP_WDT_BASE: u32 = 0x5011_6000;
pub const CLIC_BASE: u32 = 0x2080_0000;
pub const CLIC_CTRL_BASE: u32 = 0x2080_1000;
/// CLIC interrupt lines (`CLIC_INT_INFO_NUM_INT` default 48).
pub const CLIC_LINES: usize = 48;
use crate::dsi::{GDMA_BASE, GDMA_SIZE, HOST_BASE, HOST_SIZE, SRC_GDMA};
use crate::i2c::{I2C1_BASE, I2C1_SIZE};
/// The SYSTIMER counts at 16 MHz (esp_hw_support/port/esp32p4/systimer.c:20).
pub const SYSTIMER_HZ: u64 = 16_000_000;

/// USB Serial/JTAG CDC console. Only the EP1 FIFO is modelled; the FIFO is
/// always free, as when a host has the port open. The bytes are log frames
/// (doc/12), decoded by `main`.
#[derive(Default)]
pub struct Usj {
    pub output: Vec<u8>,
}

impl Usj {
    fn read(&mut self, off: u32) -> u32 {
        match off {
            0x4 => 1 << 1, // EP1_CONF.SERIAL_IN_EP_DATA_FREE
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, v: u32) {
        match off {
            0x0 => self.output.push(v as u8),
            _ => {}
        }
    }
}

/// SYSTIMER with two counter units and three alarms (systimer_reg.h).
/// Alarms compare with `>=`, so a target already in the past fires at once,
/// matching SOC_SYSTIMER_ALARM_MISS_COMPENSATE (soc_caps.h:598).
pub struct Systimer {
    conf: u32,
    unit_offset: [u64; 2],
    unit_snapshot: [u64; 2],
    unit_valid: [bool; 2],
    unit_load: [u64; 2],
    target: [u64; 3],
    target_conf: [u32; 3],
    real_target: [u64; 3],
    armed: [bool; 3],
    int_ena: u32,
    int_raw: u32,
}

impl Default for Systimer {
    fn default() -> Self {
        Systimer {
            // Reset value: TIMER_UNIT0_WORK_EN and both unit1 stall bits.
            conf: (1 << 30) | (1 << 26) | (1 << 25),
            unit_offset: [0; 2],
            unit_snapshot: [0; 2],
            unit_valid: [false; 2],
            unit_load: [0; 2],
            target: [0; 3],
            target_conf: [0; 3],
            real_target: [0; 3],
            armed: [false; 3],
            int_ena: 0,
            int_raw: 0,
        }
    }
}

impl Systimer {
    fn counter(&self, unit: usize, ticks: u64) -> u64 {
        ticks.wrapping_add(self.unit_offset[unit]) & ((1 << 52) - 1)
    }

    fn alarm_enabled(&self, t: usize) -> bool {
        self.conf & (1 << (24 - t)) != 0
    }

    fn alarm_unit(&self, t: usize) -> usize {
        (self.target_conf[t] >> 31) as usize
    }

    /// Fires alarms whose target has been reached.
    pub fn update(&mut self, ticks: u64) {
        for t in 0..3 {
            if self.armed[t] && self.alarm_enabled(t) && self.counter(self.alarm_unit(t), ticks) >= self.real_target[t] {
                self.int_raw |= 1 << t;
                if self.target_conf[t] & (1 << 30) != 0 {
                    let period = (self.target_conf[t] & 0x03FF_FFFF) as u64;
                    self.real_target[t] += period.max(1);
                } else {
                    self.armed[t] = false;
                }
            }
        }
    }

    pub fn int_st(&self) -> u32 {
        self.int_raw & self.int_ena
    }

    /// The tick at which alarm `t` will raise its interrupt, if it is armed,
    /// enabled and has its interrupt enabled.
    pub fn next_event(&self, t: usize, ticks: u64) -> Option<u64> {
        if !(self.armed[t] && self.alarm_enabled(t) && self.int_ena & (1 << t) != 0) {
            return None;
        }
        let now = self.counter(self.alarm_unit(t), ticks);
        Some(ticks + self.real_target[t].saturating_sub(now))
    }

    fn read(&mut self, off: u32, ticks: u64) -> u32 {
        self.update(ticks);
        match off {
            0x00 => self.conf,
            0x04 | 0x08 => {
                let u = ((off - 0x04) / 4) as usize;
                (self.unit_valid[u] as u32) << 29
            }
            0x0C | 0x14 => (self.unit_load[((off - 0x0C) / 8) as usize] >> 32) as u32,
            0x10 | 0x18 => self.unit_load[((off - 0x10) / 8) as usize] as u32,
            0x1C | 0x24 | 0x2C => (self.target[((off - 0x1C) / 8) as usize] >> 32) as u32,
            0x20 | 0x28 | 0x30 => self.target[((off - 0x20) / 8) as usize] as u32,
            0x34 | 0x38 | 0x3C => self.target_conf[((off - 0x34) / 4) as usize],
            0x40 | 0x48 => (self.unit_snapshot[((off - 0x40) / 8) as usize] >> 32) as u32,
            0x44 | 0x4C => self.unit_snapshot[((off - 0x44) / 8) as usize] as u32,
            0x64 => self.int_ena,
            0x68 => self.int_raw,
            0x70 => self.int_st(),
            0x74 | 0x7C | 0x84 => self.real_target[((off - 0x74) / 8) as usize] as u32,
            0x78 | 0x80 | 0x88 => (self.real_target[((off - 0x78) / 8) as usize] >> 32) as u32,
            0xFC => 0x2201_0000, // DATE; any non-zero value
            _ => 0,
        }
    }

    fn write(&mut self, off: u32, v: u32, ticks: u64) {
        self.update(ticks);
        match off {
            0x00 => {
                // Enabling an alarm re-arms it against its current target.
                for t in 0..3 {
                    let bit = 1 << (24 - t);
                    if v & bit != 0 && self.conf & bit == 0 {
                        self.armed[t] = true;
                    }
                }
                self.conf = v;
            }
            0x04 | 0x08 => {
                let u = ((off - 0x04) / 4) as usize;
                if v & (1 << 30) != 0 {
                    self.unit_snapshot[u] = self.counter(u, ticks);
                    self.unit_valid[u] = true;
                }
            }
            0x0C | 0x14 => {
                let u = ((off - 0x0C) / 8) as usize;
                self.unit_load[u] = (self.unit_load[u] & 0xFFFF_FFFF) | (((v & 0xF_FFFF) as u64) << 32);
            }
            0x10 | 0x18 => {
                let u = ((off - 0x10) / 8) as usize;
                self.unit_load[u] = (self.unit_load[u] & !0xFFFF_FFFF) | v as u64;
            }
            0x1C | 0x24 | 0x2C => {
                let t = ((off - 0x1C) / 8) as usize;
                self.target[t] = (self.target[t] & 0xFFFF_FFFF) | (((v & 0xF_FFFF) as u64) << 32);
            }
            0x20 | 0x28 | 0x30 => {
                let t = ((off - 0x20) / 8) as usize;
                self.target[t] = (self.target[t] & !0xFFFF_FFFF) | v as u64;
            }
            0x34 | 0x38 | 0x3C => self.target_conf[((off - 0x34) / 4) as usize] = v,
            0x50 | 0x54 | 0x58 if v & 1 != 0 => {
                // COMPx_LOAD: latch the target (or now + period) and arm.
                let t = ((off - 0x50) / 4) as usize;
                self.real_target[t] = if self.target_conf[t] & (1 << 30) != 0 {
                    self.counter(self.alarm_unit(t), ticks) + (self.target_conf[t] & 0x03FF_FFFF) as u64
                } else {
                    self.target[t]
                };
                self.armed[t] = true;
            }
            0x5C | 0x60 if v & 1 != 0 => {
                let u = ((off - 0x5C) / 4) as usize;
                self.unit_offset[u] = self.unit_load[u].wrapping_sub(ticks);
            }
            0x64 => self.int_ena = v & 7,
            0x6C => self.int_raw &= !(v & 7),
            _ => {}
        }
        self.update(ticks);
    }
}

/// Core 0 interrupt matrix: one map register per source (interrupt_core0_reg.h).
pub struct Intmtx {
    pub map: [u32; 128],
}

impl Default for Intmtx {
    fn default() -> Self {
        Intmtx { map: [0; 128] }
    }
}

/// Interrupt source numbers (soc/esp32p4/include/soc/interrupts.h).
pub const SRC_SYSTIMER_TARGET0: usize = 53;

/// The rev v1.x CLIC (clic_reg.h). One control word per line:
/// [31:24] ctl (priority in the top NLBITS = 3 bits, the rest read as 1),
/// [23:22] mode (RO 0b11), [18:17] trig, [16] shv, [8] ie, [0] ip.
/// The threshold is MMIO (0x2080_0008 [31:24]), not the mintthresh CSR.
pub struct Clic {
    pub thresh: u8,
    ctl: [u8; CLIC_LINES],
    attr: [u8; CLIC_LINES],
    pub ie: [bool; CLIC_LINES],
    ip_edge: [bool; CLIC_LINES],
    /// Level of each input line, driven by the interrupt matrix.
    pub line: [bool; CLIC_LINES],
}

impl Default for Clic {
    fn default() -> Self {
        Clic {
            thresh: 0,
            ctl: [0x1F; CLIC_LINES],
            attr: [0xC0; CLIC_LINES],
            ie: [false; CLIC_LINES],
            ip_edge: [false; CLIC_LINES],
            line: [false; CLIC_LINES],
        }
    }
}

impl Clic {
    pub fn is_edge(&self, i: usize) -> bool {
        self.attr[i] & 0b10 != 0
    }

    pub fn is_vectored(&self, i: usize) -> bool {
        self.attr[i] & 1 != 0
    }

    pub fn pending(&self, i: usize) -> bool {
        if self.is_edge(i) { self.ip_edge[i] } else { self.line[i] }
    }

    pub fn level(&self, i: usize) -> u8 {
        self.ctl[i] | 0x1F
    }

    /// Latches rising edges for edge-triggered lines.
    pub fn set_lines(&mut self, lines: [bool; CLIC_LINES]) {
        for i in 0..CLIC_LINES {
            if lines[i] && !self.line[i] && self.is_edge(i) {
                self.ip_edge[i] = true;
            }
        }
        self.line = lines;
    }

    /// Hardware clears the pending bit of an edge interrupt when it is taken.
    pub fn ack(&mut self, i: usize) {
        if self.is_edge(i) {
            self.ip_edge[i] = false;
        }
    }

    fn read(&mut self, addr: u32) -> u32 {
        match addr {
            0x2080_0000 => 3 << 1, // nlbits = 3 (NLBITS in clic_reg.h)
            0x2080_0004 => (3 << 21) | CLIC_LINES as u32,
            0x2080_0008 => (self.thresh as u32) << 24,
            a if (CLIC_CTRL_BASE..CLIC_CTRL_BASE + 4 * CLIC_LINES as u32).contains(&a) => {
                let i = ((a - CLIC_CTRL_BASE) / 4) as usize;
                ((self.ctl[i] as u32) << 24)
                    | ((self.attr[i] as u32) << 16)
                    | ((self.ie[i] as u32) << 8)
                    | self.pending(i) as u32
            }
            _ => 0,
        }
    }

    fn write(&mut self, addr: u32, v: u32) {
        match addr {
            0x2080_0008 => self.thresh = (v >> 24) as u8,
            a if (CLIC_CTRL_BASE..CLIC_CTRL_BASE + 4 * CLIC_LINES as u32).contains(&a) => {
                let i = ((a - CLIC_CTRL_BASE) / 4) as usize;
                self.ctl[i] = ((v >> 24) as u8 & 0xE0) | 0x1F;
                self.attr[i] = 0xC0 | ((v >> 16) as u8 & 0b111);
                self.ie[i] = v & (1 << 8) != 0;
                if self.is_edge(i) {
                    self.ip_edge[i] = v & 1 != 0;
                }
            }
            _ => {}
        }
    }
}

/// LP_WDT (RWDT). The 2nd stage bootloader leaves it running with a 9 s
/// timeout (CONFIG_BOOTLOADER_WDT_TIME_MS=9000 in boot/sdkconfig) and stage 0
/// set to reset the RTC domain too (4, bootloader_init.c:93); writes to the
/// config registers take effect only while WPROTECT holds the key.
pub struct LpWdt {
    regs: [u32; 16],
    started_at_ms: u64,
}

/// RTC slow clock the RWDT counts (RC_SLOW, ~136 kHz).
const RTC_SLOW_HZ: u64 = 136_000;

const LP_WDT_KEY: u32 = 0x50D8_3AA1;
/// WDT_STG0 actions (esp_hal_wdt/esp32p4/include/hal/lpwdt_ll.h:39-43).
pub const STG_RESET_SYSTEM: u32 = 3;
pub const STG_RESET_RTC: u32 = 4;
/// LP_CLKRST_RESET_CAUSE_REG (lp_clkrst_reg.h); HPCORE0 cause in [12:7].
pub const LP_CLKRST_RESET_CAUSE: u32 = 0x5011_1010;
/// RESET_REASON values (esp_rom/esp32p4/include/esp32p4/rom/rtc.h).
pub mod reset_reason {
    pub const POWERON: u32 = 1;
    pub const HP_SYS_LP_WDT: u32 = 9;
    pub const CHIP_LP_WDT: u32 = 16;
}

impl Default for LpWdt {
    fn default() -> Self {
        let mut regs = [0; 16];
        regs[0] = (1 << 31) | (STG_RESET_RTC << 28); // CONFIG0: WDT_EN, WDT_STG0
        regs[1] = 1_224_000; // CONFIG1.WDT_STG0_HOLD as the bootloader leaves it
        LpWdt { regs, started_at_ms: 0 }
    }
}

impl LpWdt {
    pub fn enabled(&self) -> bool {
        self.regs[0] & (1 << 31) != 0
    }

    /// Stage 0 timeout. Measured on the Tab5: 2 * hold / 136 kHz (the
    /// bootloader's 9 s setting fires after ~18 s; doc/12 §7).
    pub fn timeout_ms(&self) -> u64 {
        self.regs[1] as u64 * 2 * 1000 / RTC_SLOW_HZ
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        self.enabled() && now_ms.saturating_sub(self.started_at_ms) >= self.timeout_ms()
    }

    pub fn stage0_action(&self) -> u32 {
        (self.regs[0] >> 28) & 7
    }

    fn read(&mut self, off: u32) -> u32 {
        self.regs.get((off / 4) as usize).copied().unwrap_or(0)
    }

    fn write(&mut self, off: u32, v: u32, now_ms: u64) {
        let i = (off / 4) as usize;
        match off {
            0x14 if self.regs[6] == LP_WDT_KEY => self.started_at_ms = now_ms, // FEED
            0x18 => self.regs[i] = v,            // WPROTECT
            _ if i < self.regs.len() => {
                if self.regs[6] == LP_WDT_KEY {
                    self.regs[i] = v;
                }
            }
            _ => {}
        }
    }
}

const ROOT_CLK_CTRL0: u32 = 0x500E_6004;
const SOC_CLK_DIV_UPDATE: u32 = 1 << 4;

/// Registers the kernel reads before writing, as the Tab5's bootloader
/// leaves them (doc/research/m4-psram-and-clock.md §7): the CPU at CPLL / 4,
/// the HP root clock from CPLL, and the eFuse of the Tab5: chip revision
/// v1.0 and block version v0.3 (so the kernel takes the PVT path). The emulated CPU keeps its `--mhz` whatever
/// the dividers say.
fn initial_value(addr: u32) -> u32 {
    match addr {
        ROOT_CLK_CTRL0 => 3 << 5,
        0x5011_1040 => 0x1FFF_FFFD, // LP_CLKRST_HP_CLK_CTRL
        0x5012_D04C => 3 << 8 | 1 << 4, // EFUSE_RD_MAC_SYS_2: BLK_VERSION_MINOR, WAFER_VERSION_MAJOR_LO
        // REF_CLK_CTRL1/2 reset values: PLL_F240M and PLL_F20M gates on
        // (hp_sys_clkrst_reg.h:900-1039).
        0x500E_6028 => 23 << 16 | 1 << 30,
        0x500E_602C => 1 << 8,
        _ => 0,
    }
}

/// All modelled peripherals plus a register file for the rest of the
/// peripheral space, so that unmodelled registers read back what was written.
#[derive(Default)]
pub struct Peripherals {
    pub i2c: crate::i2c::I2c,
    pub dsi: crate::dsi::Dsi,
    pub usj: Usj,
    pub systimer: Systimer,
    pub intmtx: Intmtx,
    pub clic: Clic,
    pub lp_wdt: LpWdt,
    /// HPCORE0 reset cause reported by LP_CLKRST.
    pub reset_cause: u32,
    other: std::collections::HashMap<u32, u32>,
    warned: std::collections::HashSet<u32>,
    pub warn_unmodelled: bool,
}

impl Peripherals {
    pub fn read32(&mut self, addr: u32, ticks: u64) -> u32 {
        match addr {
            a if (USJ_BASE..USJ_BASE + 0x1000).contains(&a) => self.usj.read(a - USJ_BASE),
            a if (SYSTIMER_BASE..SYSTIMER_BASE + 0x1000).contains(&a) => self.systimer.read(a - SYSTIMER_BASE, ticks),
            a if (INTMTX_BASE..INTMTX_BASE + 0x200).contains(&a) => self.intmtx.map[((a - INTMTX_BASE) / 4) as usize],
            a if (LP_WDT_BASE..LP_WDT_BASE + 0x40).contains(&a) => self.lp_wdt.read(a - LP_WDT_BASE),
            a if (CLIC_BASE..CLIC_BASE + 0x2000).contains(&a) => self.clic.read(a),
            a if (I2C1_BASE..I2C1_BASE + I2C1_SIZE).contains(&a) => self.i2c.read(a - I2C1_BASE),
            a if (HOST_BASE..HOST_BASE + HOST_SIZE).contains(&a) => self.dsi.host_read(a - HOST_BASE),
            a if (GDMA_BASE..GDMA_BASE + GDMA_SIZE).contains(&a) => self.dsi.gdma_read(a - GDMA_BASE, ticks),
            LP_CLKRST_RESET_CAUSE => self.reset_cause << 7,
            a => {
                self.warn(a, "read");
                self.other.get(&a).copied().unwrap_or_else(|| initial_value(a))
            }
        }
    }

    pub fn write32(&mut self, addr: u32, v: u32, ticks: u64) {
        let now_ms = ticks * 1000 / SYSTIMER_HZ;
        match addr {
            a if (USJ_BASE..USJ_BASE + 0x1000).contains(&a) => self.usj.write(a - USJ_BASE, v),
            a if (SYSTIMER_BASE..SYSTIMER_BASE + 0x1000).contains(&a) => self.systimer.write(a - SYSTIMER_BASE, v, ticks),
            a if (INTMTX_BASE..INTMTX_BASE + 0x200).contains(&a) => {
                self.intmtx.map[((a - INTMTX_BASE) / 4) as usize] = v & 0x3F
            }
            a if (LP_WDT_BASE..LP_WDT_BASE + 0x40).contains(&a) => self.lp_wdt.write(a - LP_WDT_BASE, v, now_ms),
            a if (CLIC_BASE..CLIC_BASE + 0x2000).contains(&a) => self.clic.write(a, v),
            a if (I2C1_BASE..I2C1_BASE + I2C1_SIZE).contains(&a) => self.i2c.write(a - I2C1_BASE, v),
            a if (HOST_BASE..HOST_BASE + HOST_SIZE).contains(&a) => self.dsi.host_write(a - HOST_BASE, v),
            a if (GDMA_BASE..GDMA_BASE + GDMA_SIZE).contains(&a) => self.dsi.gdma_write(a - GDMA_BASE, v, ticks),
            // The divider update completes at once (clk_ll_bus_update).
            ROOT_CLK_CTRL0 => {
                self.other.insert(ROOT_CLK_CTRL0, v & !SOC_CLK_DIV_UPDATE);
            }
            a => {
                self.warn(a, "write");
                self.other.insert(a, v);
            }
        }
    }

    fn warn(&mut self, addr: u32, what: &str) {
        if self.warn_unmodelled && self.warned.insert(addr) {
            eprintln!("t5emu: unmodelled peripheral {what} at {addr:#010x}");
        }
    }

    /// Recomputes the CLIC input lines from the interrupt sources.
    pub fn route_interrupts(&mut self, ticks: u64) {
        self.systimer.update(ticks);
        let mut lines = [false; CLIC_LINES];
        let st = self.systimer.int_st();
        for t in 0..3 {
            if st & (1 << t) != 0 {
                let idx = self.intmtx.map[SRC_SYSTIMER_TARGET0 + t] as usize;
                if idx < CLIC_LINES {
                    lines[idx] = true;
                }
            }
        }
        if self.dsi.irq(ticks) {
            let idx = self.intmtx.map[SRC_GDMA] as usize;
            if idx < CLIC_LINES {
                lines[idx] = true;
            }
        }
        self.clic.set_lines(lines);
    }

    /// The earliest tick at which an enabled CLIC line will become pending.
    /// Used by `wfi` to skip ahead.
    pub fn next_wake(&self, ticks: u64) -> Option<u64> {
        (0..3)
            .filter(|&t| {
                let idx = self.intmtx.map[SRC_SYSTIMER_TARGET0 + t] as usize;
                idx < CLIC_LINES && self.clic.ie[idx]
            })
            .filter_map(|t| self.systimer.next_event(t, ticks))
            .chain(self.dsi.next_event().filter(|_| {
                let idx = self.intmtx.map[SRC_GDMA] as usize;
                idx < CLIC_LINES && self.clic.ie[idx]
            }))
            .min()
    }
}
