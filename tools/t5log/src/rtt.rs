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
    let mut core = session.core(0).map_err(|e| e.to_string())?;
    if opts.reset {
        // Wipe the control block's id first, so the data the previous boot
        // left unread is not shown as if it came from the new boot; the
        // kernel writes the id again once it has set up the block.
        core.write_8(opts.control_block, &[0u8; 16]).map_err(|e| format!("clearing the RTT id: {e}"))?;
        core.reset().map_err(|e| format!("reset: {e}"))?;
    } else if core.core_halted().map_err(|e| e.to_string())? {
        core.run().map_err(|e| e.to_string())?;
    }
    eprintln!("t5log: attached to {} via {}", opts.chip, opts.probe);

    let target = RttTarget::from(&core);
    let mut rtt = None;
    let mut to_send: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 4096];
    let mut last_try = Instant::now() - Duration::from_secs(1);
    loop {
        // The kernel sets the control block up at boot; until then (or after
        // a reset wiped it) keep trying.
        if rtt.is_none() && last_try.elapsed() >= Duration::from_millis(200) {
            last_try = Instant::now();
            match Rtt::attach_at(&mut core, &target, opts.control_block) {
                Ok(r) => {
                    eprintln!("t5log: RTT control block at {:#x}", opts.control_block);
                    rtt = Some(r);
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
        match turn() {
            Turn::Continue => {}
            Turn::Send(more) => to_send.extend(more),
            Turn::Stop => return Ok(()),
        }
        if !to_send.is_empty()
            && let Some(down) = r.down_channel(0)
        {
            match down.write(&mut core, &to_send) {
                Ok(n) => {
                    to_send.drain(..n);
                }
                Err(e) => eprintln!("t5log: RTT write failed: {e}"),
            }
        }
        let channel = r.up_channel(opts.channel).ok_or(format!("no RTT up-channel {}", opts.channel))?;
        let n = match channel.read(&mut core, &mut buf) {
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
            std::thread::sleep(opts.poll);
        }
    }
}
