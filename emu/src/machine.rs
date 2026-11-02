//! The HP core: RV32IMAFC with M and U modes (no S, no MMU), the rev v1.x
//! CLIC trap model, PMP, and the ESP32-P4 quirks the kernel depends on
//! (doc/11 §2):
//! - An exception taken while already handling a trap (before `mret`) locks
//!   the core up (TRM §2.13.5; doc/01 §0). ecall/ebreak are exempt.
//! - Only the ROM cache functions and the SPI flash erase/write functions
//!   are modelled; any other ROM call stops.
//! - `fence.i` does not make data-side writes fetchable (see `cache`).

use crate::bus::Bus;
use crate::pmp::{Access, Pmp};
use crate::rvc;

/// Why the emulator stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `wfi` with nothing that could ever wake it.
    Halted { pc: u32 },
    /// An exception while handling a trap; the real core locks up.
    Lockup { cause: u32, pc: u32, tval: u32 },
    /// Fetch from code written through the D-cache without writeback +
    /// I-cache invalidation (doc/09 §2).
    IncoherentFetch { pc: u32 },
    /// A ROM function other than the modelled cache functions.
    UnknownRom { pc: u32 },
    /// A hardware-vectored (shv) CLIC interrupt; not modelled.
    VectoredInterrupt { line: usize },
    /// The RWDT expired (the kernel did not disable it).
    Watchdog,
    /// The emulated time limit was reached.
    TimeLimit,
    /// The caller's `run` hook asked to stop (it saw what it waited for).
    Requested,
    /// The flash window was read or fetched from while the L2 cache was
    /// suspended (a flash-map or flash-write routine not running from RAM).
    /// The real core would hang.
    FlashWhileL2Suspended { addr: u32, pc: u32 },
    /// The ROM erase/write functions were called with the L2 cache running:
    /// the cache would read flash while SPI1 changes it (doc/14 §5.2).
    FlashOpWithL2Active { pc: u32 },
    /// A page changed by erase/write was read through the window before it
    /// was invalidated in the L1 D-cache and L2: stale data on the chip.
    StaleFlashRead { addr: u32, pc: u32 },
    /// An erase/write outside otadata, ota_1 and the app store (the
    /// bootloader, the partition table, the running kernel's ota_0). Not a
    /// chip rule: the kernel promises it never does this (doc/14 §5.2,
    /// doc/16 §3.2), and the emulator holds it to that.
    FlashWriteProtected { offset: u32 },
    /// The kernel asked the ROM for a system reset (after a kernel update
    /// the bootloader would start the new kernel; not modelled).
    SoftwareReset,
}

/// An exception (cause, tval) or a reason to stop emulation.
enum Ex {
    E(u32, u32),
    Stop(Stop),
}

impl From<Stop> for Ex {
    fn from(s: Stop) -> Ex {
        Ex::Stop(s)
    }
}

mod cause {
    pub const INSN_ACCESS: u32 = 1;
    pub const ILLEGAL: u32 = 2;
    pub const BREAKPOINT: u32 = 3;
    pub const LOAD_ACCESS: u32 = 5;
    pub const STORE_MISALIGNED: u32 = 6;
    pub const STORE_ACCESS: u32 = 7;
    pub const ECALL_U: u32 = 8;
    pub const ECALL_M: u32 = 11;
}

mod ms {
    pub const MIE: u32 = 1 << 3;
    pub const MPIE: u32 = 1 << 7;
    pub const MPP_SHIFT: u32 = 11;
    pub const MPP: u32 = 3 << MPP_SHIFT;
    pub const FS_SHIFT: u32 = 13;
    pub const FS: u32 = 3 << FS_SHIFT;
    pub const MPRV: u32 = 1 << 17;
    pub const SD: u32 = 1 << 31;
    pub const WRITABLE: u32 = MIE | MPIE | MPP | FS | MPRV;
}

/// misa read on the Tab5 (doc/09): RV32 IMAFC + U + X.
pub const MISA: u32 = 0x4090_1125;

/// ROM cache functions (esp_rom/esp32p4/ld/esp32p4.rom.ld:186,190).
const ROM_CACHE_INVALIDATE_ADDR: u32 = 0x4FC0_03E4;
const ROM_CACHE_WRITEBACK_ADDR: u32 = 0x4FC0_03F4;
/// esp32p4.rom.ld:259-260.
const ROM_CACHE_SUSPEND_L2: u32 = 0x4FC0_0508;
const ROM_CACHE_RESUME_L2: u32 = 0x4FC0_050C;
/// esp32p4.rom.ld:198.
const ROM_CACHE_WRITEBACK_ALL: u32 = 0x4FC0_0414;
/// esp32p4.rom.ld:180, 182, 194 (doc/43 §3).
const ROM_CACHE_GET_L2_LINE_SIZE: u32 = 0x4FC0_03CC;
const ROM_CACHE_SET_L2_MODE: u32 = 0x4FC0_03D4;
const ROM_CACHE_INVALIDATE_ALL: u32 = 0x4FC0_0404;
/// esp32p4.rom.ld:99-101: direct SPI commands (the kernel drives the
/// PSRAM's MSPI3 with them).
const ROM_SPI_CMD_CONFIG: u32 = 0x4FC0_0108;
const ROM_SPI_CMD_START: u32 = 0x4FC0_010C;
const ROM_SPI_SET_OP_MODE: u32 = 0x4FC0_0110;
/// esp32p4.rom.ld:123,125 (rev v1.x).
const ROM_SPIFLASH_ERASE_SECTOR: u32 = 0x4FC0_014C;
const ROM_SPIFLASH_WRITE: u32 = 0x4FC0_0154;
/// ets_update_cpu_frequency (esp32p4.rom.ld:32); the ROM delay is not modelled.
const ROM_UPDATE_CPU_FREQUENCY: u32 = 0x4FC0_0044;
/// Flash the kernel may change (boot/partitions.csv, doc/16 §3.2): otadata,
/// ota_1 (the emulated kernel runs from ota_0, see `Bus::new`) and the app
/// store onwards. Anything else is refused (`Stop::FlashWriteProtected`).
const FLASH_WRITABLE: [core::ops::Range<u32>; 2] = [0xD000..0xF000, 0x21_0000..0x100_0000];
/// esp_rom_software_reset_system (esp32p4.rom.ld:52).
const ROM_SOFTWARE_RESET: u32 = 0x4FC0_0094;
/// ets_set_appcpu_boot_addr (esp32p4.rom.ld:57) and Core1's clock and reset
/// (hp_sys_clkrst_reg.h:147,179,3064,3124).
const ROM_SET_APPCPU_BOOT_ADDR: u32 = 0x4FC0_00A8;
const HP_SYS_CLKRST_SOC_CLK_CTRL0: u32 = 0x500E_6014;
const CORE1_CPU_CLK_EN: u32 = 1 << 4;
const HP_SYS_CLKRST_HP_RST_EN0: u32 = 0x500E_60C0;
const RST_EN_CORE1_GLOBAL: u32 = 1 << 8;
/// Typical times of a 4 KiB erase and a page program on a 128 Mbit NOR
/// flash (W25Q128JV datasheet: tSE 45 ms, tPP 0.4 ms). The Tab5's chip is
/// not known yet; the kernel measures it (flashtest).
const FLASH_ERASE_US: u64 = 45_000;
const FLASH_PAGE_US: u64 = 400;
const CACHE_MAP_L1_ICACHE_MASK: u32 = 0b11;
const CACHE_MAP_L1_DCACHE: u32 = 1 << 4;

const CANONICAL_NAN: u32 = 0x7FC0_0000;
mod fflag {
    pub const NV: u32 = 1 << 4;
    pub const DZ: u32 = 1 << 3;
    pub const NX: u32 = 1 << 0;
}

pub struct Cpu {
    pub x: [u32; 32],
    pub f: [u32; 32],
    pub pc: u32,
    pub m_mode: bool,
    pub mstatus: u32,
    pub mtvec: u32,
    pub mtvt: u32,
    pub mscratch: u32,
    pub mepc: u32,
    pub mtval: u32,
    pub mie: u32,
    pub mcounteren: u32,
    mcause_int: bool,
    mcause_code: u32,
    /// mcause.mpil: interrupt level before the last trap (CLIC).
    mpil: u8,
    /// mintstatus.mil: current interrupt level (CLIC).
    pub mil: u8,
    pub fcsr: u32,
    pub pmp: Pmp,
    reservation: Option<u32>,
    /// Between trap entry and `mret` (mexstatus.EXPT_VLD on the P4).
    pub in_trap: bool,
    pub instret: u64,
    pub mhcr: u32,
    /// mhartid: 0 or 1 (doc/33).
    pub hart: u32,
}

impl Cpu {
    fn new(entry: u32) -> Cpu {
        Cpu {
            x: [0; 32],
            f: [0; 32],
            pc: entry,
            m_mode: true,
            mstatus: 0,
            mtvec: 0,
            mtvt: 0,
            mscratch: 0,
            mepc: 0,
            mtval: 0,
            mie: 0,
            mcounteren: 0,
            mcause_int: false,
            mcause_code: 0,
            mpil: 0,
            mil: 0,
            fcsr: 0,
            pmp: Pmp::default(),
            reservation: None,
            in_trap: false,
            instret: 0,
            mhcr: 0,
            hart: 0,
        }
    }

    fn clic_mode(&self) -> bool {
        self.mtvec & 3 == 3
    }

    fn mpp_m(&self) -> bool {
        self.mstatus & ms::MPP != 0
    }

    fn fs_on(&self) -> bool {
        self.mstatus & ms::FS != 0
    }

    fn set_fs_dirty(&mut self) {
        self.mstatus |= ms::FS;
    }

    fn read_mstatus(&self) -> u32 {
        let sd = if self.mstatus & ms::FS == ms::FS { ms::SD } else { 0 };
        self.mstatus | sd
    }

    fn write_mstatus(&mut self, v: u32) {
        let mut v = v & ms::WRITABLE;
        // MPP is WARL over {U, M}.
        if (v & ms::MPP) >> ms::MPP_SHIFT != 3 {
            v &= !ms::MPP;
        }
        self.mstatus = v;
    }

    fn read_mcause(&self) -> u32 {
        let int = (self.mcause_int as u32) << 31;
        if self.clic_mode() {
            let mpp = (self.mstatus & ms::MPP) >> ms::MPP_SHIFT;
            let mpie = (self.mstatus & ms::MPIE != 0) as u32;
            int | (mpp << 28) | (mpie << 27) | ((self.mpil as u32) << 16) | self.mcause_code
        } else {
            int | self.mcause_code
        }
    }

    fn write_mcause(&mut self, v: u32) {
        self.mcause_int = v & (1 << 31) != 0;
        if self.clic_mode() {
            // mpp/mpie alias the mstatus fields (CLIC spec).
            self.mcause_code = v & 0xFFF;
            self.mpil = (v >> 16) as u8;
            let mpp = if (v >> 28) & 3 == 3 { ms::MPP } else { 0 };
            let mpie = if v & (1 << 27) != 0 { ms::MPIE } else { 0 };
            self.mstatus = (self.mstatus & !(ms::MPP | ms::MPIE)) | mpp | mpie;
        } else {
            self.mcause_code = v & 0x7FFF_FFFF;
        }
    }
}

/// The core that is not running at the moment (doc/33 §3): the machine
/// runs one core at a time, `QUANTUM` instructions each, swapping its state
/// and its CLIC in.
pub struct Core {
    pub cpu: Cpu,
    pub clic: crate::periph::Clic,
    /// In `wfi` with nothing pending: skipped until something is.
    pub waiting: bool,
}

/// Instructions a core runs before the other gets its turn.
const QUANTUM: u32 = 1024;

pub struct Machine {
    /// The core running now (core 0 until core 1 starts).
    pub cpu: Cpu,
    /// The other core, once core 1 has been started.
    pub other: Option<Box<Core>>,
    /// The running core is in `wfi` with nothing pending.
    pub waiting: bool,
    /// The running core ran `pause` (it spins on a lock): its turn ends.
    yielded: bool,
    pub bus: Bus,
    /// Print every trap to stderr.
    pub trace_traps: bool,
    /// Address of the SEGGER RTT control block, if the ELF has one. `run`
    /// then plays the debugger and drains up-channel 0 into `rtt_output`.
    pub rtt_cb: Option<u32>,
    pub rtt_output: Vec<u8>,
    /// Bytes the host still has to put into down-channel 0.
    pub rtt_down_pending: std::collections::VecDeque<u8>,
    /// Instructions between RTT polls (0: no host attached).
    pub rtt_poll_interval: u32,
}

fn sext(v: u32, bits: u32) -> u32 {
    let sh = 32 - bits;
    (((v << sh) as i32) >> sh) as u32
}

impl Machine {
    /// The CPU state after a reset (the bus is reset separately).
    pub fn reset_cpu(&mut self, entry: u32) {
        self.cpu = Cpu::new(entry);
        self.other = None;
        self.waiting = false;
        self.bus.periph.core = 0;
        self.bus.periph.clic = Default::default();
    }

    /// Lets the other core run (no-op while there is only one).
    fn switch_core(&mut self) {
        let Some(o) = self.other.as_mut() else { return };
        core::mem::swap(&mut self.cpu, &mut o.cpu);
        core::mem::swap(&mut self.bus.periph.clic, &mut o.clic);
        core::mem::swap(&mut self.waiting, &mut o.waiting);
        self.bus.periph.core = self.cpu.hart as usize;
    }

    /// Whether the running core has an interrupt pending that ends its
    /// `wfi` (any enabled pending line, whatever MIE says).
    fn wake_pending(&mut self) -> bool {
        let ticks = self.bus.ticks();
        self.bus.periph.route_interrupts(ticks);
        self.best_pending(None).is_some()
    }

    /// Both cores wait: skips to the first time either could wake.
    fn skip_to_wake(&mut self) -> Result<(), Stop> {
        let ticks = self.bus.ticks();
        let mine = self.bus.periph.next_wake(ticks);
        self.switch_core();
        let theirs = self.bus.periph.next_wake(ticks);
        self.switch_core();
        match mine.into_iter().chain(theirs).min() {
            Some(t) => {
                let at = self.bus.cycles_for_ticks(t);
                self.bus.cycles = self.bus.cycles.max(at);
                Ok(())
            }
            None if self.bus.periph.lp_wdt.enabled() => Err(Stop::Watchdog),
            None => Err(Stop::Halted { pc: self.cpu.pc }),
        }
    }

    /// One turn of the core running now: wakes it if it may, else gives
    /// the turn away (or skips time when both wait).
    fn dual_turn(&mut self) -> Result<bool, Stop> {
        if !self.waiting {
            return Ok(true);
        }
        if self.wake_pending() {
            self.waiting = false;
            return Ok(true);
        }
        let other_waits = self.other.as_ref().is_some_and(|o| o.waiting);
        if other_waits {
            self.skip_to_wake()?;
        }
        self.switch_core();
        Ok(false)
    }

    pub fn new(bus: Bus, entry: u32) -> Machine {
        Machine { cpu: Cpu::new(entry), other: None, waiting: false, yielded: false, bus, trace_traps: false, rtt_cb: None, rtt_output: Vec::new(), rtt_down_pending: Default::default(), rtt_poll_interval: 4096 }
    }

    /// Runs until something stops the emulator or `limit_cycles` is reached.
    /// Runs until something stops the emulator, `limit_cycles` is reached,
    /// or `hook` (called every 4096 instructions) returns false.
    pub fn run(&mut self, limit_cycles: u64, hook: &mut dyn FnMut(&mut Machine) -> bool) -> Stop {
        let mut wdt_check = 0u32;
        let mut turn = 0u32;
        loop {
            if self.other.is_some() {
                match self.dual_turn() {
                    Ok(true) => {}
                    Ok(false) => {
                        turn = 0;
                        continue;
                    }
                    Err(s) => return s,
                }
                turn += 1;
                if turn >= QUANTUM || core::mem::take(&mut self.yielded) {
                    // Always hand over: a waiting core checks then whether
                    // its interrupt has come (`dual_turn`) and gives the
                    // turn back if not.
                    turn = 0;
                    self.switch_core();
                }
            }
            if let Err(s) = self.step() {
                return s;
            }
            wdt_check = wdt_check.wrapping_add(1);
            if self.rtt_poll_interval != 0 && wdt_check % self.rtt_poll_interval == 0 {
                self.poll_rtt();
            }
            if wdt_check % 4096 == 0 {
                if !hook(self) {
                    return Stop::Requested;
                }
                if self.bus.cycles >= limit_cycles {
                    return Stop::TimeLimit;
                }
                let ms = self.bus.ticks() * 1000 / crate::periph::SYSTIMER_HZ;
                if self.bus.periph.lp_wdt.expired(ms) {
                    return Stop::Watchdog;
                }
            }
        }
    }

    /// What a debugger does: read up-channel 0 between `RdOff` and `WrOff`
    /// and advance `RdOff`. Ignored until the block carries its id. The
    /// layout is SEGGER_RTT_CB (16-byte id, MaxNumUpBuffers,
    /// MaxNumDownBuffers, then 24-byte buffer descriptors).
    pub fn poll_rtt(&mut self) {
        let Some(cb) = self.rtt_cb else { return };
        let mut id = [0u8; 10];
        for (i, b) in id.iter_mut().enumerate() {
            *b = self.bus.read(cb + i as u32, 1).unwrap_or(0) as u8;
        }
        if &id != b"SEGGER RTT" {
            return;
        }
        let up = cb + 24;
        let rd32 = |bus: &mut Bus, a: u32| bus.read(a, 4).unwrap_or(0);
        let buf = rd32(&mut self.bus, up + 4);
        let size = rd32(&mut self.bus, up + 8);
        let wr = rd32(&mut self.bus, up + 12);
        let mut rd = rd32(&mut self.bus, up + 16);
        if size == 0 || wr >= size || rd >= size {
            return;
        }
        while rd != wr {
            let b = self.bus.read(buf + rd, 1).unwrap_or(0) as u8;
            self.rtt_output.push(b);
            rd = (rd + 1) % size;
        }
        self.bus.write(up + 16, 4, rd);

        // Down-channel 0 follows up-channel 0 (MaxNumUpBuffers = 1).
        let max_up = rd32(&mut self.bus, cb + 16);
        let down = up + 24 * max_up;
        let (buf, size) = (rd32(&mut self.bus, down + 4), rd32(&mut self.bus, down + 8));
        let mut wr = rd32(&mut self.bus, down + 12);
        let rd = rd32(&mut self.bus, down + 16);
        if size == 0 || wr >= size || rd >= size {
            return;
        }
        while (wr + 1) % size != rd {
            let Some(b) = self.rtt_down_pending.pop_front() else { break };
            self.bus.write(buf + wr, 1, b as u32);
            wr = (wr + 1) % size;
        }
        self.bus.write(down + 12, 4, wr);
    }

    pub fn step(&mut self) -> Result<(), Stop> {
        let ticks = self.bus.ticks();
        self.bus.periph.route_interrupts(ticks);
        if let Some(line) = self.interrupt_to_take() {
            self.take_interrupt(line)?;
        }
        self.bus.cycles += 1;
        let pc = self.cpu.pc;
        let r = if Bus::is_rom(pc) { self.rom_call(pc) } else { self.fetch_and_execute(pc) };
        if let Some(addr) = self.bus.flash_violation.take() {
            return Err(Stop::FlashWhileL2Suspended { addr, pc });
        }
        if let Some(addr) = self.bus.stale_read.take() {
            return Err(Stop::StaleFlashRead { addr, pc });
        }
        match r {
            Ok(()) => {
                self.cpu.instret += 1;
                Ok(())
            }
            Err(Ex::E(c, tval)) => self.take_exception(c, tval),
            Err(Ex::Stop(s)) => Err(s),
        }
    }

    // ---- traps -----------------------------------------------------------

    /// The highest-level pending CLIC interrupt that may be taken now.
    fn interrupt_to_take(&self) -> Option<usize> {
        let c = &self.cpu;
        if !c.clic_mode() || (c.m_mode && c.mstatus & ms::MIE == 0) {
            return None;
        }
        // M-mode interrupts are always enabled in U-mode; the threshold and
        // the current level only apply while in M-mode (judgement, doc/11).
        let floor = if c.m_mode { c.mil.max(self.bus.periph.clic.thresh) } else { 0 };
        self.best_pending(Some(floor))
    }

    /// Highest level first, then highest line number (CLIC arbitration).
    fn best_pending(&self, floor: Option<u8>) -> Option<usize> {
        let clic = &self.bus.periph.clic;
        (0..crate::periph::CLIC_LINES)
            .filter(|&i| clic.ie[i] && clic.pending(i))
            .filter(|&i| floor.is_none_or(|f| clic.level(i) > f))
            .max_by_key(|&i| (clic.level(i), i))
    }

    fn enter_trap(&mut self, int: bool, code: u32, tval: u32) {
        let c = &mut self.cpu;
        c.mepc = c.pc;
        c.mcause_int = int;
        c.mcause_code = code;
        c.mtval = tval;
        c.mpil = c.mil;
        let mpp = if c.m_mode { ms::MPP } else { 0 };
        let mpie = if c.mstatus & ms::MIE != 0 { ms::MPIE } else { 0 };
        c.mstatus = (c.mstatus & !(ms::MPP | ms::MPIE | ms::MIE)) | mpp | mpie;
        c.m_mode = true;
        c.in_trap = true;
        c.reservation = None;
        c.pc = if c.clic_mode() {
            c.mtvec & !63
        } else if c.mtvec & 3 == 1 && int {
            (c.mtvec & !3) + 4 * code
        } else {
            c.mtvec & !3
        };
    }

    fn take_interrupt(&mut self, line: usize) -> Result<(), Stop> {
        let clic = &mut self.bus.periph.clic;
        if clic.is_vectored(line) {
            return Err(Stop::VectoredInterrupt { line });
        }
        clic.ack(line);
        let level = clic.level(line);
        if self.trace_traps {
            eprintln!("t5emu: interrupt {line} at pc={:#010x} ({})", self.cpu.pc, self.mode_name());
        }
        self.enter_trap(true, line as u32, 0);
        self.cpu.mil = level;
        Ok(())
    }

    fn take_exception(&mut self, code: u32, tval: u32) -> Result<(), Stop> {
        if self.trace_traps {
            eprintln!(
                "t5emu: exception {code} at pc={:#010x} tval={tval:#010x} ({})",
                self.cpu.pc,
                self.mode_name()
            );
        }
        let exempt = matches!(code, cause::ECALL_U | cause::ECALL_M | cause::BREAKPOINT);
        if self.cpu.in_trap && !exempt {
            return Err(Stop::Lockup { cause: code, pc: self.cpu.pc, tval });
        }
        self.enter_trap(false, code, tval);
        Ok(())
    }

    fn mode_name(&self) -> &'static str {
        if self.cpu.m_mode { "M" } else { "U" }
    }

    fn mret(&mut self) {
        let c = &mut self.cpu;
        c.m_mode = c.mpp_m();
        let mie = if c.mstatus & ms::MPIE != 0 { ms::MIE } else { 0 };
        c.mstatus = (c.mstatus & !(ms::MIE | ms::MPP)) | mie | ms::MPIE;
        if !c.m_mode {
            c.mstatus &= !ms::MPRV;
        }
        if c.clic_mode() {
            c.mil = c.mpil;
        }
        c.in_trap = false;
        c.pc = c.mepc;
    }

    fn wfi(&mut self) -> Result<(), Stop> {
        // Wakes on any enabled pending line, regardless of MIE (seen on the
        // Tab5 in M2: the kernel idles in wfi with MIE=0).
        if self.best_pending(None).is_some() {
            return Ok(());
        }
        // Two cores: this one waits, the run loop gives the other its turn.
        if self.other.is_some() {
            self.waiting = true;
            return Ok(());
        }
        let ticks = self.bus.ticks();
        match self.bus.periph.next_wake(ticks) {
            Some(t) => {
                let at = self.bus.cycles_for_ticks(t);
                self.bus.cycles = self.bus.cycles.max(at);
                Ok(())
            }
            // A running RWDT would still reset the chip.
            None if self.bus.periph.lp_wdt.enabled() => Err(Stop::Watchdog),
            None => Err(Stop::Halted { pc: self.cpu.pc }),
        }
    }

    fn rom_call(&mut self, pc: u32) -> Result<(), Ex> {
        if !self.cpu.pmp.check(pc, 2, Access::Exec, self.cpu.m_mode) {
            return Err(Ex::E(cause::INSN_ACCESS, pc));
        }
        let [a0, a1, a2] = [self.cpu.x[10], self.cpu.x[11], self.cpu.x[12]];
        match pc {
            ROM_CACHE_INVALIDATE_ADDR => {
                if a0 & CACHE_MAP_L1_ICACHE_MASK != 0 {
                    self.bus.cache.invalidate_i(a1, a2);
                    self.bus.psram.cache.invalidate_i(a1, a2);
                }
                self.bus.invalidate_window(a0, a1, a2);
            }
            ROM_SPIFLASH_ERASE_SECTOR | ROM_SPIFLASH_WRITE => {
                if !self.bus.l2_suspended {
                    return Err(Ex::Stop(Stop::FlashOpWithL2Active { pc: self.cpu.x[1] }));
                }
                let erase = pc == ROM_SPIFLASH_ERASE_SECTOR;
                let offset = if erase { a0.wrapping_mul(crate::bus::FLASH_SECTOR) } else { a0 };
                if !FLASH_WRITABLE.iter().any(|r| r.contains(&offset)) {
                    return Err(Ex::Stop(Stop::FlashWriteProtected { offset }));
                }
                let (r, us) = if erase {
                    (self.bus.flash_erase_sector(a0), FLASH_ERASE_US)
                } else {
                    (self.bus.flash_write(a0, a1, a2), FLASH_PAGE_US * a2.div_ceil(crate::bus::FLASH_PAGE) as u64)
                };
                self.bus.cycles += self.bus.cpu_hz * us / 1_000_000;
                self.cpu.x[10] = r;
                self.cpu.pc = self.cpu.x[1] & !1;
                return Ok(());
            }
            ROM_CACHE_WRITEBACK_ADDR => {
                if a0 & CACHE_MAP_L1_DCACHE != 0 {
                    self.bus.cache.writeback_d(a1, a2);
                    self.bus.psram.cache.writeback_d(a1, a2);
                }
            }
            ROM_CACHE_WRITEBACK_ALL => {
                if a0 & CACHE_MAP_L1_DCACHE != 0 {
                    self.bus.cache.writeback_d(0, u32::MAX);
                    self.bus.psram.cache.writeback_d(0, u32::MAX);
                }
            }
            // rom/cache.h:107-110: CACHE_LINE_SIZE_64B = 3, _128B = 4.
            ROM_CACHE_SET_L2_MODE => self.bus.l2_line = if a2 == 4 { 128 } else { 64 },
            ROM_CACHE_GET_L2_LINE_SIZE => {
                self.bus.cycles += 50;
                self.cpu.x[10] = self.bus.l2_line;
                self.cpu.pc = self.cpu.x[1] & !1;
                return Ok(());
            }
            ROM_CACHE_INVALIDATE_ALL => self.bus.invalidate_window(a0, 0, u32::MAX),
            ROM_SPI_SET_OP_MODE | ROM_UPDATE_CPU_FREQUENCY => {}
            ROM_SPI_CMD_CONFIG => {
                // esp_rom_spi_cmd_t: u16 cmd, u16 cmd_bits, u32 *addr,
                // u32 addr_bits, u32 *tx, u32 tx_bits, u32 *rx, u32 rx_bits,
                // u32 dummy_bits (opi_flash.h).
                let word = |bus: &mut crate::bus::Bus, a: u32| bus.read(a, 4).unwrap_or(0);
                let cmd = word(&mut self.bus, a1) as u16;
                let addr_ptr = word(&mut self.bus, a1 + 4);
                let addr = if addr_ptr == 0 { 0 } else { word(&mut self.bus, addr_ptr) };
                let (tx_ptr, tx_bits) = (word(&mut self.bus, a1 + 12), word(&mut self.bus, a1 + 16));
                let rx_bits = word(&mut self.bus, a1 + 24);
                let tx = (0..tx_bits / 8).map(|i| self.bus.read(tx_ptr + i, 1).unwrap_or(0) as u8).collect();
                self.bus.psram.cmd = Some(crate::psram::SpiCmd { cmd, addr, tx, rx_bits });
            }
            ROM_SPI_CMD_START => {
                for (i, b) in self.bus.psram.start(a2 as usize).into_iter().enumerate() {
                    self.bus.write(a1 + i as u32, 1, b as u32);
                }
            }
            ROM_CACHE_SUSPEND_L2 => {
                self.bus.l2_suspended = true;
            }
            ROM_CACHE_RESUME_L2 => {
                self.bus.l2_suspended = false;
            }
            ROM_SOFTWARE_RESET => return Err(Ex::Stop(Stop::SoftwareReset)),
            // Core 1 starts at a0 once its clock is on and its reset off
            // (doc/32 §1, doc/33).
            ROM_SET_APPCPU_BOOT_ADDR => {
                let p = &self.bus.periph;
                let clk = p.peek(HP_SYS_CLKRST_SOC_CLK_CTRL0) & CORE1_CPU_CLK_EN != 0;
                let out_of_reset = p.peek(HP_SYS_CLKRST_HP_RST_EN0) & RST_EN_CORE1_GLOBAL == 0;
                if a0 != 0 && clk && out_of_reset && self.other.is_none() {
                    let mut cpu = Cpu::new(a0);
                    cpu.hart = 1;
                    self.other = Some(Box::new(Core { cpu, clic: Default::default(), waiting: false }));
                }
            }
            _ => return Err(Ex::Stop(Stop::UnknownRom { pc })),
        }
        // Call overhead, plus roughly one cycle per cache line for the
        // functions that take a range (a2 is not a length for the others).
        let lines = if matches!(pc, ROM_CACHE_INVALIDATE_ADDR | ROM_CACHE_WRITEBACK_ADDR) { a2 / crate::cache::LINE } else { 0 };
        self.bus.cycles += 50 + lines as u64;
        self.cpu.x[10] = 0;
        self.cpu.pc = self.cpu.x[1] & !1;
        Ok(())
    }

    // ---- memory ------------------------------------------------------------

    /// Privilege used for loads and stores (mstatus.MPRV).
    fn data_m_mode(&self) -> bool {
        if self.cpu.m_mode && self.cpu.mstatus & ms::MPRV != 0 { self.cpu.mpp_m() } else { self.cpu.m_mode }
    }

    fn fetch16(&mut self, addr: u32) -> Result<u32, Ex> {
        if !self.cpu.pmp.check(addr, 2, Access::Exec, self.cpu.m_mode) {
            return Err(Ex::E(cause::INSN_ACCESS, self.cpu.pc));
        }
        let h = self.bus.fetch16(addr).ok_or(Ex::E(cause::INSN_ACCESS, self.cpu.pc))?;
        if !self.bus.cache.fetch_ok(addr) || !self.bus.psram.cache.fetch_ok(addr) {
            return Err(Ex::Stop(Stop::IncoherentFetch { pc: addr }));
        }
        Ok(h as u32)
    }

    /// Misaligned loads and stores are carried out by the hardware: the TRM's
    /// exception list has no cause 4, and on the Tab5 a misaligned store into
    /// a PMP-denied region raised cause 7, not 6 (doc/11 §3). Cause 6 is left
    /// to misaligned AMOs.
    fn load(&mut self, addr: u32, size: u32) -> Result<u32, Ex> {
        if !self.cpu.pmp.check(addr, size, Access::Read, self.data_m_mode()) {
            return Err(Ex::E(cause::LOAD_ACCESS, addr));
        }
        self.bus.read(addr, size).ok_or(Ex::E(cause::LOAD_ACCESS, addr))
    }

    fn store(&mut self, addr: u32, size: u32, v: u32) -> Result<(), Ex> {
        if !self.cpu.pmp.check(addr, size, Access::Write, self.data_m_mode()) {
            return Err(Ex::E(cause::STORE_ACCESS, addr));
        }
        if !self.bus.write(addr, size, v) {
            return Err(Ex::E(cause::STORE_ACCESS, addr));
        }
        if self.cpu.reservation == Some(addr & !3) {
            self.cpu.reservation = None;
        }
        self.break_other_reservation(addr);
        Ok(())
    }

    /// A store by this core ends the other core's LR reservation on the
    /// same word.
    fn break_other_reservation(&mut self, addr: u32) {
        if let Some(o) = self.other.as_mut()
            && o.cpu.reservation == Some(addr & !3)
        {
            o.cpu.reservation = None;
        }
    }

    // ---- execution -----------------------------------------------------------

    fn fetch_and_execute(&mut self, pc: u32) -> Result<(), Ex> {
        let lo = self.fetch16(pc)?;
        if lo & 3 != 3 {
            let insn = rvc::expand(lo).ok_or(Ex::E(cause::ILLEGAL, lo))?;
            self.execute(insn, 2)
        } else {
            let hi = self.fetch16(pc.wrapping_add(2))?;
            self.execute(lo | (hi << 16), 4)
        }
    }

    fn set_x(&mut self, rd: u32, v: u32) {
        if rd != 0 {
            self.cpu.x[rd as usize] = v;
        }
    }

    fn execute(&mut self, insn: u32, len: u32) -> Result<(), Ex> {
        let illegal = Ex::E(cause::ILLEGAL, insn);
        let pc = self.cpu.pc;
        let mut next = pc.wrapping_add(len);
        let rd = (insn >> 7) & 31;
        let f3 = (insn >> 12) & 7;
        let rs1 = (insn >> 15) & 31;
        let rs2 = (insn >> 20) & 31;
        let f7 = insn >> 25;
        let a = self.cpu.x[rs1 as usize];
        let b = self.cpu.x[rs2 as usize];
        let imm_i = sext(insn >> 20, 12);
        let imm_s = sext(((insn >> 25) << 5) | ((insn >> 7) & 0x1F), 12);

        match insn & 0x7F {
            0x37 => self.set_x(rd, insn & 0xFFFF_F000),
            0x17 => self.set_x(rd, pc.wrapping_add(insn & 0xFFFF_F000)),
            0x6F => {
                let imm = sext(
                    ((insn >> 31) << 20) | (insn & 0xFF000) | ((insn >> 9) & 0x800) | ((insn >> 20) & 0x7FE),
                    21,
                );
                self.set_x(rd, next);
                next = pc.wrapping_add(imm);
            }
            0x67 if f3 == 0 => {
                let target = a.wrapping_add(imm_i) & !1;
                self.set_x(rd, next);
                next = target;
            }
            0x63 => {
                let imm = sext(
                    ((insn >> 31) << 12) | ((insn << 4) & 0x800) | ((insn >> 20) & 0x7E0) | ((insn >> 7) & 0x1E),
                    13,
                );
                let taken = match f3 {
                    0 => a == b,
                    1 => a != b,
                    4 => (a as i32) < (b as i32),
                    5 => (a as i32) >= (b as i32),
                    6 => a < b,
                    7 => a >= b,
                    _ => return Err(illegal),
                };
                if taken {
                    next = pc.wrapping_add(imm);
                }
            }
            0x03 => {
                let addr = a.wrapping_add(imm_i);
                let v = match f3 {
                    0 => sext(self.load(addr, 1)?, 8),
                    1 => sext(self.load(addr, 2)?, 16),
                    2 => self.load(addr, 4)?,
                    4 => self.load(addr, 1)?,
                    5 => self.load(addr, 2)?,
                    _ => return Err(illegal),
                };
                self.set_x(rd, v);
            }
            0x23 => {
                let addr = a.wrapping_add(imm_s);
                match f3 {
                    0 => self.store(addr, 1, b & 0xFF)?,
                    1 => self.store(addr, 2, b & 0xFFFF)?,
                    2 => self.store(addr, 4, b)?,
                    _ => return Err(illegal),
                }
            }
            0x13 => {
                let shamt = rs2;
                let v = match f3 {
                    0 => a.wrapping_add(imm_i),
                    2 => ((a as i32) < (imm_i as i32)) as u32,
                    3 => (a < imm_i) as u32,
                    4 => a ^ imm_i,
                    6 => a | imm_i,
                    7 => a & imm_i,
                    1 if f7 == 0 => a << shamt,
                    5 if f7 == 0 => a >> shamt,
                    5 if f7 == 0x20 => ((a as i32) >> shamt) as u32,
                    _ => return Err(illegal),
                };
                self.set_x(rd, v);
            }
            0x33 => {
                let v = match (f7, f3) {
                    (0, 0) => a.wrapping_add(b),
                    (0x20, 0) => a.wrapping_sub(b),
                    (0, 1) => a << (b & 31),
                    (0, 2) => ((a as i32) < (b as i32)) as u32,
                    (0, 3) => (a < b) as u32,
                    (0, 4) => a ^ b,
                    (0, 5) => a >> (b & 31),
                    (0x20, 5) => ((a as i32) >> (b & 31)) as u32,
                    (0, 6) => a | b,
                    (0, 7) => a & b,
                    (1, 0) => a.wrapping_mul(b),
                    (1, 1) => ((a as i32 as i64 * b as i32 as i64) >> 32) as u32,
                    (1, 2) => ((a as i32 as i64 * b as u64 as i64) >> 32) as u32,
                    (1, 3) => ((a as u64 * b as u64) >> 32) as u32,
                    (1, 4) => {
                        let (x, y) = (a as i32, b as i32);
                        if y == 0 { u32::MAX } else { x.wrapping_div(y) as u32 }
                    }
                    (1, 5) => a.checked_div(b).unwrap_or(u32::MAX),
                    (1, 6) => {
                        let (x, y) = (a as i32, b as i32);
                        if y == 0 { a } else { x.wrapping_rem(y) as u32 }
                    }
                    (1, 7) => a.checked_rem(b).unwrap_or(a),
                    _ => return Err(illegal),
                };
                self.set_x(rd, v);
            }
            // fence / fence.i: memory is coherent for data; fence.i does not
            // reach the L1 caches on the P4 (cache module).
            // `pause` (FENCE W,0; Zihintpause): a core spinning on a lock
            // gives the other its turn, which is what it waits for.
            0x0F if insn == 0x0100_000F => self.yielded = self.other.is_some(),
            0x0F if f3 <= 1 => {}
            0x2F if f3 == 2 => self.amo(insn, rd, a, b)?,
            0x73 => return self.system(insn, rd, f3, rs1, a, len),
            0x07 | 0x27 | 0x53 | 0x43 | 0x47 | 0x4B | 0x4F => {
                self.fp(insn, rd, f3, rs1, rs2, a, imm_i, imm_s)?;
            }
            _ => return Err(illegal),
        }
        self.cpu.pc = next;
        Ok(())
    }

    fn amo(&mut self, insn: u32, rd: u32, addr: u32, b: u32) -> Result<(), Ex> {
        let f5 = insn >> 27;
        match f5 {
            0x02 => {
                // lr.w
                if (insn >> 20) & 31 != 0 {
                    return Err(Ex::E(cause::ILLEGAL, insn));
                }
                if addr % 4 != 0 {
                    return Err(Ex::E(cause::STORE_MISALIGNED, addr));
                }
                let v = self.load(addr, 4)?;
                self.cpu.reservation = Some(addr);
                self.set_x(rd, v);
            }
            0x03 => {
                // sc.w
                if addr % 4 != 0 {
                    return Err(Ex::E(cause::STORE_MISALIGNED, addr));
                }
                if self.cpu.reservation == Some(addr) {
                    self.store(addr, 4, b)?;
                    self.set_x(rd, 0);
                } else {
                    self.set_x(rd, 1);
                }
                self.cpu.reservation = None;
            }
            _ => {
                if addr % 4 != 0 {
                    return Err(Ex::E(cause::STORE_MISALIGNED, addr));
                }
                let m = self.data_m_mode();
                if !self.cpu.pmp.check(addr, 4, Access::Read, m) || !self.cpu.pmp.check(addr, 4, Access::Write, m) {
                    return Err(Ex::E(cause::STORE_ACCESS, addr));
                }
                let old = self.bus.read(addr, 4).ok_or(Ex::E(cause::STORE_ACCESS, addr))?;
                let new = match f5 {
                    0x01 => b,
                    0x00 => old.wrapping_add(b),
                    0x04 => old ^ b,
                    0x0C => old & b,
                    0x08 => old | b,
                    0x10 => (old as i32).min(b as i32) as u32,
                    0x14 => (old as i32).max(b as i32) as u32,
                    0x18 => old.min(b),
                    0x1C => old.max(b),
                    _ => return Err(Ex::E(cause::ILLEGAL, insn)),
                };
                self.store(addr, 4, new)?;
                self.set_x(rd, old);
            }
        }
        Ok(())
    }

    fn system(&mut self, insn: u32, rd: u32, f3: u32, rs1: u32, a: u32, len: u32) -> Result<(), Ex> {
        let illegal = Ex::E(cause::ILLEGAL, insn);
        let pc = self.cpu.pc;
        if f3 == 0 {
            match insn {
                0x0000_0073 => {
                    let c = if self.cpu.m_mode { cause::ECALL_M } else { cause::ECALL_U };
                    return Err(Ex::E(c, 0));
                }
                0x0010_0073 => return Err(Ex::E(cause::BREAKPOINT, pc)),
                0x3020_0073 if self.cpu.m_mode => {
                    self.mret();
                    return Ok(());
                }
                0x1050_0073 if self.cpu.m_mode => self.wfi()?,
                _ => return Err(illegal),
            }
            self.cpu.pc = pc.wrapping_add(len);
            return Ok(());
        }
        let csr = insn >> 20;
        let src = if f3 & 4 != 0 { rs1 } else { a };
        let writes = f3 & 3 == 1 || rs1 != 0;
        if !self.cpu.m_mode && (csr >> 8) & 3 != 0 {
            return Err(illegal);
        }
        if writes && csr >> 10 == 3 {
            return Err(illegal);
        }
        let old = self.csr_read(csr).ok_or(Ex::E(cause::ILLEGAL, insn))?;
        if writes {
            let new = match f3 & 3 {
                1 => src,
                2 => old | src,
                _ => old & !src,
            };
            self.csr_write(csr, new).ok_or(Ex::E(cause::ILLEGAL, insn))?;
        }
        self.set_x(rd, old);
        self.cpu.pc = pc.wrapping_add(len);
        Ok(())
    }

    fn csr_read(&self, csr: u32) -> Option<u32> {
        let c = &self.cpu;
        let cycles = self.bus.cycles;
        Some(match csr {
            0x001..=0x003 if !c.fs_on() => return None,
            0x001 => c.fcsr & 0x1F,
            0x002 => (c.fcsr >> 5) & 7,
            0x003 => c.fcsr & 0xFF,
            0xC00 | 0xC02 | 0xC80 | 0xC82 if !c.m_mode && c.mcounteren & (1 << (csr & 0x1F)) == 0 => return None,
            0xB00 | 0xC00 => cycles as u32,
            0xB80 | 0xC80 => (cycles >> 32) as u32,
            0xB02 | 0xC02 => c.instret as u32,
            0xB82 | 0xC82 => (c.instret >> 32) as u32,
            0x300 => c.read_mstatus(),
            0x301 => MISA,
            0x304 => c.mie,
            0x305 => c.mtvec,
            0x306 => c.mcounteren,
            0x307 => c.mtvt,
            0x340 => c.mscratch,
            0x341 => c.mepc,
            0x342 => c.read_mcause(),
            0x343 => c.mtval,
            0x344 => 0,
            // mintstatus is at 0x346 on rev v1.x, not 0xFB1 (doc/01 §0).
            0x346 => (c.mil as u32) << 24,
            // MHCR: branch prediction control (riscv/include/riscv/rv_utils.h:48-52).
            0x7C1 => c.mhcr,
            0x3A0..=0x3A3 => c.pmp.read_cfg((csr - 0x3A0) as usize),
            0x3B0..=0x3BF => c.pmp.read_addr((csr - 0x3B0) as usize),
            0xF14 => c.hart,
            0xF11..=0xF13 => 0,
            _ => return None,
        })
    }

    fn csr_write(&mut self, csr: u32, v: u32) -> Option<()> {
        let c = &mut self.cpu;
        match csr {
            0x001..=0x003 if !c.fs_on() => return None,
            0x001 => c.fcsr = (c.fcsr & !0x1F) | (v & 0x1F),
            0x002 => c.fcsr = (c.fcsr & 0x1F) | ((v & 7) << 5),
            0x003 => c.fcsr = v & 0xFF,
            0xB00 => self.bus.cycles = (self.bus.cycles & !0xFFFF_FFFF) | v as u64,
            0xB80 => self.bus.cycles = (self.bus.cycles & 0xFFFF_FFFF) | ((v as u64) << 32),
            0xB02 => c.instret = (c.instret & !0xFFFF_FFFF) | v as u64,
            0xB82 => c.instret = (c.instret & 0xFFFF_FFFF) | ((v as u64) << 32),
            0x300 => c.write_mstatus(v),
            0x301 => {}
            0x304 => c.mie = v,
            0x305 => c.mtvec = v,
            0x306 => c.mcounteren = v & 0b101,
            0x307 => c.mtvt = v & !63,
            0x340 => c.mscratch = v,
            0x341 => c.mepc = v & !1,
            0x342 => c.write_mcause(v),
            0x343 => c.mtval = v,
            0x344 => {}
            0x7C1 => c.mhcr = v,
            0x3A0..=0x3A3 => c.pmp.write_cfg((csr - 0x3A0) as usize, v),
            0x3B0..=0x3BF => c.pmp.write_addr((csr - 0x3B0) as usize, v),
            _ => return None,
        }
        if (0x001..=0x003).contains(&csr) {
            self.cpu.set_fs_dirty();
        }
        Some(())
    }

    // ---- F extension -----------------------------------------------------

    /// RV32F. Arithmetic uses the host's round-to-nearest-even regardless of
    /// the rounding mode, and only NV and DZ are raised (doc/11 §5).
    #[allow(clippy::too_many_arguments)]
    fn fp(&mut self, insn: u32, rd: u32, f3: u32, rs1: u32, rs2: u32, a: u32, imm_i: u32, imm_s: u32) -> Result<(), Ex> {
        let illegal = Ex::E(cause::ILLEGAL, insn);
        if !self.cpu.fs_on() {
            return Err(illegal);
        }
        let fa = self.cpu.f[rs1 as usize];
        let fb = self.cpu.f[rs2 as usize];
        let rm = if f3 == 7 { (self.cpu.fcsr >> 5) & 7 } else { f3 };
        let f7 = insn >> 25;
        let mut flags = 0;
        let fr: Option<u32>; // value for f[rd]
        let mut xr: Option<u32> = None; // value for x[rd]
        match insn & 0x7F {
            0x07 if f3 == 2 => fr = Some(self.load(a.wrapping_add(imm_i), 4)?),
            0x27 if f3 == 2 => {
                self.store(a.wrapping_add(imm_s), 4, fb)?;
                return Ok(());
            }
            op @ (0x43 | 0x47 | 0x4B | 0x4F) => {
                if (insn >> 25) & 3 != 0 || rm > 4 {
                    return Err(illegal);
                }
                let fc = self.cpu.f[(insn >> 27) as usize];
                let (x, y, z) = (f32::from_bits(fa), f32::from_bits(fb), f32::from_bits(fc));
                let r = match op {
                    0x43 => x.mul_add(y, z),
                    0x47 => x.mul_add(y, -z),
                    0x4B => (-x).mul_add(y, z),
                    _ => (-x).mul_add(y, -z),
                };
                flags |= nan_flags(&[fa, fb, fc], r);
                fr = Some(canon(r));
            }
            0x53 => {
                let (x, y) = (f32::from_bits(fa), f32::from_bits(fb));
                match f7 {
                    0x00 | 0x04 | 0x08 | 0x0C | 0x2C => {
                        if rm > 4 || (f7 == 0x2C && rs2 != 0) {
                            return Err(illegal);
                        }
                        let r = match f7 {
                            0x00 => x + y,
                            0x04 => x - y,
                            0x08 => x * y,
                            0x0C => {
                                if y == 0.0 && x.is_finite() && x != 0.0 {
                                    flags |= fflag::DZ;
                                }
                                x / y
                            }
                            _ => x.sqrt(),
                        };
                        let ins: &[u32] = if f7 == 0x2C { &[fa] } else { &[fa, fb] };
                        flags |= nan_flags(ins, r);
                        fr = Some(canon(r));
                    }
                    0x10 => {
                        let sign = match f3 {
                            0 => fb & 0x8000_0000,
                            1 => !fb & 0x8000_0000,
                            2 => (fa ^ fb) & 0x8000_0000,
                            _ => return Err(illegal),
                        };
                        fr = Some((fa & 0x7FFF_FFFF) | sign);
                    }
                    0x14 if f3 <= 1 => {
                        if is_snan(fa) || is_snan(fb) {
                            flags |= fflag::NV;
                        }
                        fr = Some(match (x.is_nan(), y.is_nan()) {
                            (true, true) => CANONICAL_NAN,
                            (true, false) => fb,
                            (false, true) => fa,
                            _ => {
                                // -0.0 is less than +0.0 here.
                                let a_less = x < y || (x == y && fa & 0x8000_0000 != 0);
                                if (f3 == 0) == a_less { fa } else { fb }
                            }
                        });
                    }
                    0x60 if rs2 <= 1 => {
                        if rm > 4 {
                            return Err(illegal);
                        }
                        let (v, fl) = fcvt_to_int(x, rm, rs2 == 1);
                        flags |= fl;
                        fr = None;
                        xr = Some(v);
                    }
                    0x70 if rs2 == 0 && f3 == 0 => {
                        fr = None;
                        xr = Some(fa);
                    }
                    0x70 if rs2 == 0 && f3 == 1 => {
                        fr = None;
                        xr = Some(fclass(fa));
                    }
                    0x50 if f3 <= 2 => {
                        let any_nan = x.is_nan() || y.is_nan();
                        let signaling = if f3 == 2 { is_snan(fa) || is_snan(fb) } else { any_nan };
                        if signaling {
                            flags |= fflag::NV;
                        }
                        fr = None;
                        xr = Some(match f3 {
                            2 => (x == y) as u32,
                            1 => (x < y) as u32,
                            _ => (x <= y) as u32,
                        });
                    }
                    0x68 if rs2 <= 1 => {
                        let v = self.cpu.x[rs1 as usize];
                        let r = if rs2 == 0 { v as i32 as f32 } else { v as f32 };
                        if r as f64 != if rs2 == 0 { v as i32 as f64 } else { v as f64 } {
                            flags |= fflag::NX;
                        }
                        fr = Some(r.to_bits());
                    }
                    0x78 if rs2 == 0 && f3 == 0 => fr = Some(self.cpu.x[rs1 as usize]),
                    _ => return Err(illegal),
                }
            }
            _ => return Err(illegal),
        }
        if let Some(v) = fr {
            self.cpu.f[rd as usize] = v;
        }
        if let Some(v) = xr {
            self.set_x(rd, v);
        }
        if fr.is_some() || flags != 0 {
            self.cpu.fcsr |= flags;
            self.cpu.set_fs_dirty();
        }
        Ok(())
    }
}

fn is_snan(b: u32) -> bool {
    b & 0x7F80_0000 == 0x7F80_0000 && b & 0x007F_FFFF != 0 && b & 0x0040_0000 == 0
}

fn canon(r: f32) -> u32 {
    if r.is_nan() { CANONICAL_NAN } else { r.to_bits() }
}

/// NV for a NaN produced from non-NaN inputs (invalid operation) or for any
/// signaling NaN input.
fn nan_flags(inputs: &[u32], r: f32) -> u32 {
    let any_snan = inputs.iter().any(|&b| is_snan(b));
    let any_nan_in = inputs.iter().any(|&b| f32::from_bits(b).is_nan());
    if any_snan || (r.is_nan() && !any_nan_in) { fflag::NV } else { 0 }
}

fn fclass(b: u32) -> u32 {
    let x = f32::from_bits(b);
    let neg = b & 0x8000_0000 != 0;
    let bit = if x.is_nan() {
        if is_snan(b) { 8 } else { 9 }
    } else if x.is_infinite() {
        if neg { 0 } else { 7 }
    } else if x == 0.0 {
        if neg { 3 } else { 4 }
    } else if x.is_subnormal() {
        if neg { 2 } else { 5 }
    } else if neg {
        1
    } else {
        6
    };
    1 << bit
}

/// fcvt.w.s / fcvt.wu.s with RISC-V saturation: NaN converts to the maximum.
fn fcvt_to_int(x: f32, rm: u32, unsigned: bool) -> (u32, u32) {
    if x.is_nan() {
        return (if unsigned { u32::MAX } else { i32::MAX as u32 }, fflag::NV);
    }
    let r = match rm {
        0 => x.round_ties_even(),
        1 => x.trunc(),
        2 => x.floor(),
        3 => x.ceil(),
        _ => x.round(),
    } as f64;
    let nx = if r != x as f64 { fflag::NX } else { 0 };
    if unsigned {
        if r < 0.0 {
            (0, fflag::NV)
        } else if r > u32::MAX as f64 {
            (u32::MAX, fflag::NV)
        } else {
            (r as u32, nx)
        }
    } else if r < i32::MIN as f64 {
        (i32::MIN as u32, fflag::NV)
    } else if r > i32::MAX as f64 {
        (i32::MAX as u32, fflag::NV)
    } else {
        (r as i32 as u32, nx)
    }
}
