//! Reading the log over RTT with probe-rs used as a library (doc/12 §8), so
//! that `t5log rtt` gets the channel's bytes directly instead of the hex text
//! the probe-rs CLI prints for a binary channel.

use std::time::{Duration, Instant};

use probe_rs::config::Registry;
use probe_rs::probe::DebugProbeSelector;
use probe_rs::probe::list::Lister;
use probe_rs::rtt::{Rtt, RttTarget};
use probe_rs::{MemoryInterface, Permissions};

pub struct Options {
    /// `VID:PID:SERIAL`, as `probe-rs list` shows it.
    pub probe: String,
    pub chip: String,
    /// Extra chip description (tools/probe-rs/esp32p4-tab5.yaml adds LP SRAM).
    pub chip_description: Option<String>,
    /// Address of `_SEGGER_RTT`.
    pub control_block: u64,
    pub channel: usize,
    pub poll: Duration,
    /// Reset the target after attaching, to see a boot from the start (RTT
    /// data is consumed when read).
    pub reset: bool,
}

/// Finds `_SEGGER_RTT` in an ELF.
pub fn control_block_address(elf: &[u8]) -> Result<u64, String> {
    use object::read::elf::{FileHeader, Sym};
    let header = object::elf::FileHeader32::<object::Endianness>::parse(elf).map_err(|e| e.to_string())?;
    let endian = header.endian().map_err(|e| e.to_string())?;
    let sections = header.sections(endian, elf).map_err(|e| e.to_string())?;
    let symbols = sections.symbols(endian, elf, object::elf::SHT_SYMTAB).map_err(|e| e.to_string())?;
    for s in symbols.iter() {
        if s.name(endian, symbols.strings()).ok() == Some(b"_SEGGER_RTT".as_slice()) && s.st_shndx(endian) != 0 {
            return Ok(s.st_value(endian) as u64);
        }
    }
    Err("no _SEGGER_RTT symbol (is this a tab5-os kernel ELF?)".into())
}

/// Halts core 0, reads where it is, and lets it run again (`t5log status`):
/// for a target that stopped answering (doc/16 §6.5). Returns lines to
/// print.
pub fn status(probe: &str, chip: &str, chip_description: Option<&str>) -> Result<Vec<String>, String> {
    use probe_rs::RegisterId;
    probe_rs_espressif::register_plugin();
    let mut registry = Registry::from_builtin_families();
    if let Some(path) = chip_description {
        let yaml = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        registry.add_target_family_from_yaml(&yaml).map_err(|e| format!("{path}: {e}"))?;
    }
    let selector: DebugProbeSelector = probe.parse().map_err(|e| format!("--probe {probe}: {e}"))?;
    let p = Lister::new().open(selector).map_err(|e| format!("opening probe {probe}: {e}"))?;
    let mut session = p.attach_with_registry(chip, Permissions::default(), &registry).map_err(|e| format!("attaching to {chip}: {e}"))?;
    let mut core = session.core(0).map_err(|e| e.to_string())?;
    let was_halted = core.core_halted().map_err(|e| e.to_string())?;
    let info = core.halt(Duration::from_millis(500)).map_err(|e| format!("halt: {e}"))?;
    let mut out = vec![format!("core 0: {} pc={:#010x}", if was_halted { "was halted" } else { "was running" }, info.pc)];
    // CSR numbers as debug register ids (RISC-V debug spec: CSRs at 0x0000-0x0FFF).
    for (name, csr) in [("mstatus", 0x300u16), ("mepc", 0x341), ("mcause", 0x342), ("mtval", 0x343)] {
        match core.read_core_reg::<u32>(RegisterId(csr)) {
            Ok(v) => out.push(format!("{name}={v:#010x}")),
            Err(e) => out.push(format!("{name}: {e}")),
        }
    }
    for (name, reg) in [("ra", core.return_address()), ("sp", core.stack_pointer())] {
        match core.read_core_reg::<u32>(reg) {
            Ok(v) => out.push(format!("{name}={v:#010x}")),
            Err(e) => out.push(format!("{name}: {e}")),
        }
    }
    core.run().map_err(|e| format!("resume: {e}"))?;
    Ok(out)
}

/// Reads `len` bytes at `address` (a multiple of 4) through the debug
/// module's system bus, without halting a hart (`t5log read`; doc/47 §5).
pub fn read_memory(probe: &str, chip: &str, chip_description: Option<&str>, address: u64, len: usize) -> Result<Vec<u8>, String> {
    probe_rs_espressif::register_plugin();
    let mut registry = Registry::from_builtin_families();
    if let Some(path) = chip_description {
        let yaml = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        registry.add_target_family_from_yaml(&yaml).map_err(|e| format!("{path}: {e}"))?;
    }
    let selector: DebugProbeSelector = probe.parse().map_err(|e| format!("--probe {probe}: {e}"))?;
    let p = Lister::new().open(selector).map_err(|e| format!("opening probe {probe}: {e}"))?;
    let mut session = p.attach_with_registry(chip, Permissions::default(), &registry).map_err(|e| format!("attaching to {chip}: {e}"))?;
    let r = (|| {
        let mut mem = session.get_riscv_interface(0).map_err(|e| e.to_string())?;
        let mut words = vec![0u32; len / 4];
        for (k, chunk) in words.chunks_mut(16 * 1024).enumerate() {
            let at = address + (k * 64 * 1024) as u64;
            mem.read_32(at, chunk).map_err(|e| format!("reading {at:#x}: {e}"))?;
        }
        Ok(words.iter().flat_map(|w| w.to_le_bytes()).collect())
    })();
    // As in `stream`: dropping the session would halt the harts.
    std::mem::forget(session);
    r
}

/// What the caller wants on each turn of the polling loop.
pub enum Turn {
    Continue,
    /// Write these bytes to down-channel 0 (control commands, doc/14 §3).
    Send(Vec<u8>),
    Stop,
}

/// Attaches and calls `sink` with every chunk read from the up-channel until
/// `sink` returns false, `turn` returns `Stop`, or an error occurs. `turn`
/// is called on every pass, whether or not data arrived. Ctrl-C ends the
/// process.
pub fn stream(
    opts: &Options,
    mut sink: impl FnMut(&[u8]) -> bool,
    mut turn: impl FnMut() -> Turn,
) -> Result<(), String> {
    probe_rs_espressif::register_plugin();
    let mut registry = Registry::from_builtin_families();
    if let Some(path) = &opts.chip_description {
        let yaml = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        registry.add_target_family_from_yaml(&yaml).map_err(|e| format!("{path}: {e}"))?;
    }
    let selector: DebugProbeSelector = opts.probe.parse().map_err(|e| format!("--probe {}: {e}", opts.probe))?;
    let probe = Lister::new().open(selector).map_err(|e| format!("opening probe {}: {e}", opts.probe))?;
    let mut session = probe
        .attach_with_registry(opts.chip.as_str(), Permissions::default(), &registry)
        .map_err(|e| format!("attaching to {}: {e}", opts.chip))?;
    let r = stream_on(&mut session, opts, &mut sink, &mut turn);
    // Not dropped: probe-rs's `Session::drop` halts both harts (to clear
    // the hardware breakpoints and to make each `Core`), and a halt can
    // leave a process in M-mode (doc/36). The probe is let go when the
    // process ends.
    std::mem::forget(session);
    r
}

fn stream_on(
    session: &mut probe_rs::Session,
    opts: &Options,
    sink: &mut impl FnMut(&[u8]) -> bool,
    turn: &mut impl FnMut() -> Turn,
) -> Result<(), String> {
    if opts.reset {
        let mut core = session.core(0).map_err(|e| e.to_string())?;
        // Wipe the control block's id first, so the data the previous boot
        // left unread is not shown as if it came from the new boot; the
        // kernel writes the id again once it has set up the block.
        core.write_8(opts.control_block, &[0u8; 16]).map_err(|e| format!("clearing the RTT id: {e}"))?;
        core.reset().map_err(|e| format!("reset: {e}"))?;
    }
    eprintln!("t5log: attached to {} via {}", opts.chip, opts.probe);

    // The log is read through the debug module's system bus only, so the
    // hart is never halted: a halt that lands as the kernel `mret`s into a
    // process can leave the process running in M-mode (doc/36). So no
    // probe-rs `Core` (making one halts the hart to read `misa` and set
    // `dcsr`), and only 32-bit accesses (`Words`).
    let target = RttTarget { is_64_bit: false, memory_regions: session.target().memory_map.clone() };
    let mut mem = Words(session.get_riscv_interface(0).map_err(|e| e.to_string())?);
    let mut rtt = None;
    let mut to_send: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 4096];
    let mut last_try = Instant::now() - Duration::from_secs(1);
    // Commands wait until the up-channel has been read empty once: a kernel
    // that ran with nobody reading has a full buffer (it drops new frames
    // then), and the answers to the commands would be lost (doc/17 §9).
    let mut drained = false;
    loop {
        // The kernel sets the control block up at boot; until then (or after
        // a reset wiped it) keep trying.
        if rtt.is_none() && last_try.elapsed() >= Duration::from_millis(200) {
            last_try = Instant::now();
            match Rtt::attach_at(&mut mem, &target, opts.control_block) {
                Ok(r) => {
                    eprintln!("t5log: RTT control block at {:#x}", opts.control_block);
                    rtt = Some(r);
                    drained = false;
                }
                Err(_) => {
                    std::thread::sleep(opts.poll);
                    continue;
                }
            }
        }
        let Some(r) = rtt.as_mut() else {
            std::thread::sleep(opts.poll);
            continue;
        };
        if drained {
            match turn() {
                Turn::Continue => {}
                Turn::Send(more) => to_send.extend(more),
                Turn::Stop => return Ok(()),
            }
        }
        if !to_send.is_empty()
            && let Some(down) = r.down_channel(0)
        {
            match down.write(&mut mem, &to_send) {
                Ok(n) => {
                    to_send.drain(..n);
                }
                Err(e) => eprintln!("t5log: RTT write failed: {e}"),
            }
        }
        let channel = r.up_channel(opts.channel).ok_or(format!("no RTT up-channel {}", opts.channel))?;
        let n = match channel.read(&mut mem, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                // The target reset and is rebuilding the control block.
                eprintln!("t5log: RTT read failed ({e}); re-attaching");
                rtt = None;
                continue;
            }
        };
        if n > 0 {
            if !sink(&buf[..n]) {
                return Ok(());
            }
        } else {
            drained = true;
            std::thread::sleep(opts.poll);
        }
    }
}

/// Memory through 32-bit accesses only. On the Tab5 (P4 v1.x) probe-rs
/// reaches LP SRAM through the system bus for 32-bit accesses but through
/// the program buffer for 8- and 16-bit ones, which halts the hart for each
/// (doc/36); RTT writes the unaligned ends of a command with `write_8`.
/// Narrow accesses become word reads, and word read-modify-writes. Only the
/// host writes the down-channel's buffer, so rewriting the rest of a word
/// there changes nothing.
struct Words<M>(M);

impl<M: MemoryInterface> Words<M> {
    /// The aligned words that cover `len` bytes at `address`, read: (first
    /// word's address, the bytes).
    fn covering(&mut self, address: u64, len: usize) -> Result<(u64, Vec<u8>), probe_rs::Error> {
        let start = address & !3;
        let words = ((address - start) as usize + len).div_ceil(4);
        let mut w = vec![0u32; words];
        self.0.read_32(start, &mut w)?;
        Ok((start, w.iter().flat_map(|v| v.to_le_bytes()).collect()))
    }
}

impl<M: MemoryInterface> MemoryInterface for Words<M> {
    fn supports_native_64bit_access(&mut self) -> bool {
        false
    }

    fn read_64(&mut self, address: u64, data: &mut [u64]) -> Result<(), probe_rs::Error> {
        self.0.read_64(address, data)
    }

    fn read_32(&mut self, address: u64, data: &mut [u32]) -> Result<(), probe_rs::Error> {
        self.0.read_32(address, data)
    }

    fn read_16(&mut self, address: u64, data: &mut [u16]) -> Result<(), probe_rs::Error> {
        let (start, bytes) = self.covering(address, data.len() * 2)?;
        let at = (address - start) as usize;
        for (k, d) in data.iter_mut().enumerate() {
            *d = u16::from_le_bytes([bytes[at + 2 * k], bytes[at + 2 * k + 1]]);
        }
        Ok(())
    }

    fn read_8(&mut self, address: u64, data: &mut [u8]) -> Result<(), probe_rs::Error> {
        let (start, bytes) = self.covering(address, data.len())?;
        let at = (address - start) as usize;
        data.copy_from_slice(&bytes[at..at + data.len()]);
        Ok(())
    }

    fn write_64(&mut self, address: u64, data: &[u64]) -> Result<(), probe_rs::Error> {
        self.0.write_64(address, data)
    }

    fn write_32(&mut self, address: u64, data: &[u32]) -> Result<(), probe_rs::Error> {
        self.0.write_32(address, data)
    }

    fn write_16(&mut self, address: u64, data: &[u16]) -> Result<(), probe_rs::Error> {
        let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.write_8(address, &bytes)
    }

    fn write_8(&mut self, address: u64, data: &[u8]) -> Result<(), probe_rs::Error> {
        let (start, mut bytes) = self.covering(address, data.len())?;
        let at = (address - start) as usize;
        bytes[at..at + data.len()].copy_from_slice(data);
        let words: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        self.0.write_32(start, &words)
    }

    fn supports_8bit_transfers(&self) -> Result<bool, probe_rs::Error> {
        Ok(true)
    }

    fn flush(&mut self) -> Result<(), probe_rs::Error> {
        self.0.flush()
    }
}
