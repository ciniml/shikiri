//! t5emu: runs a tab5-os kernel ELF built for the ESP32-P4 rev v1.x
//! (`make kernel-p4`) without the hardware. See doc/11-emulator.md.
//!
//! Exit status: 0 = the kernel reported `selftest: PASS` and halted,
//! 1 = it reported a failure or never reported, 2 = the emulator stopped on
//! a hardware-level problem (lock-up, incoherent fetch, watchdog, ...),
//! 3 = time limit, 64 = usage error.

use std::process::ExitCode;

use t5emu::bus::Bus;
use t5emu::cache;
use t5emu::machine::{Machine, Stop};

const USAGE: &str = "usage: t5emu [--mhz N] [--cache strict|off] [--max-ms N] [--seed N] \
                     [--trace-traps] [--warn-unmodelled] [--quiet] KERNEL.elf";

struct Opts {
    mhz: u64,
    cache: cache::Mode,
    max_ms: u64,
    seed: u32,
    trace_traps: bool,
    warn_unmodelled: bool,
    quiet: bool,
    elf: String,
}

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts {
        mhz: 40,
        cache: cache::Mode::Strict,
        max_ms: 10_000,
        seed: 0x7AB5_0001,
        trace_traps: false,
        warn_unmodelled: false,
        quiet: false,
        elf: String::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--mhz" => o.mhz = val()?.parse().map_err(|e| format!("--mhz: {e}"))?,
            "--max-ms" => o.max_ms = val()?.parse().map_err(|e| format!("--max-ms: {e}"))?,
            "--seed" => o.seed = val()?.parse().map_err(|e| format!("--seed: {e}"))?,
            "--cache" => {
                o.cache = match val()?.as_str() {
                    "strict" => cache::Mode::Strict,
                    "off" => cache::Mode::Off,
                    v => return Err(format!("--cache: unknown mode {v}")),
                }
            }
            "--trace-traps" => o.trace_traps = true,
            "--warn-unmodelled" => o.warn_unmodelled = true,
            "--quiet" => o.quiet = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => o.elf = s.to_string(),
        }
    }
    if o.elf.is_empty() {
        return Err("no kernel ELF given".into());
    }
    Ok(o)
}

fn main() -> ExitCode {
    let o = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("t5emu: {e}\n{USAGE}");
            return ExitCode::from(64);
        }
    };
    let image = match std::fs::read(&o.elf) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("t5emu: {}: {e}", o.elf);
            return ExitCode::from(64);
        }
    };
    let elf = match t5emu::elf::parse(&image) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("t5emu: {}: {e}", o.elf);
            return ExitCode::from(64);
        }
    };

    let mut bus = Bus::new(o.mhz * 1_000_000, o.cache, o.seed);
    bus.periph.usj.echo = !o.quiet;
    bus.periph.warn_unmodelled = o.warn_unmodelled;
    for s in &elf.segments {
        if s.data.is_empty() {
            continue; // NOLOAD (.bss, the text gap); the kernel clears .bss itself
        }
        if let Err(e) = bus.load(s.vaddr, &s.data) {
            eprintln!("t5emu: {e}");
            return ExitCode::from(64);
        }
        let _ = s.memsz;
    }

    let mut m = Machine::new(bus, elf.entry);
    m.trace_traps = o.trace_traps;
    let limit = o.max_ms * o.mhz * 1000;
    let stop = m.run(limit);

    let out = String::from_utf8_lossy(&m.bus.periph.usj.output).into_owned();
    let emulated_ms = m.bus.cycles / (o.mhz * 1000);
    eprintln!(
        "t5emu: stopped after {} instructions, {} ms emulated at {} MHz: {}",
        m.cpu.instret,
        emulated_ms,
        o.mhz,
        describe(&stop)
    );
    // Temporary until the structured log service replaces text output
    // (doc/07): the verdict is read from the console text.
    match stop {
        Stop::Halted { .. } if out.contains("selftest: PASS") => ExitCode::SUCCESS,
        Stop::Halted { .. } => ExitCode::from(1),
        Stop::TimeLimit => ExitCode::from(3),
        _ => ExitCode::from(2),
    }
}

fn describe(s: &Stop) -> String {
    match s {
        Stop::Halted { pc } => format!("halted in wfi at {pc:#010x} with no wake-up source"),
        Stop::Lockup { cause, pc, tval } => {
            format!("CPU lock-up: exception {cause} at pc={pc:#010x} tval={tval:#010x} while handling a trap")
        }
        Stop::IncoherentFetch { pc } => format!(
            "fetch from {pc:#010x}, which was written through the D-cache without \
             Cache_WriteBack_Addr + Cache_Invalidate_Addr (the real CPU would execute stale bytes)"
        ),
        Stop::UnknownRom { pc } => format!("call to unmodelled ROM function at {pc:#010x}"),
        Stop::VectoredInterrupt { line } => format!("hardware-vectored CLIC interrupt {line} (not modelled)"),
        Stop::Watchdog => "RWDT expired (reset on real hardware)".into(),
        Stop::TimeLimit => "time limit reached".into(),
    }
}
