//! t5emu: runs a tab5-os kernel ELF built for the ESP32-P4 rev v1.x
//! (`make kernel-p4`) without the hardware. See doc/11-emulator.md.
//!
//! The console carries structured log frames (doc/12); they are decoded with
//! the schemas of the kernel ELF plus any `--elf`/`--schemas` given (the app
//! ELFs, so app events get names).
//!
//! Exit status: 0 = the kernel logged `selftest.result` with `pass=true` and
//! halted, 1 = it reported a failure or never reported, 2 = the emulator
//! stopped on a hardware-level problem (lock-up, incoherent fetch, watchdog,
//! ...), 3 = time limit, 64 = usage error.

use std::process::ExitCode;

use t5emu::bus::Bus;
use t5emu::cache;
use t5emu::machine::{Machine, Stop};

const USAGE: &str = "usage: t5emu [--mhz N] [--cache strict|off] [--max-ms N] [--seed N] \
                     [--elf APP.elf]... [--schemas FILE]... [--json] [--console-out FILE] \
                     [--rtt-poll N] [--log-from-rtt] \
                     [--trace-traps] [--warn-unmodelled] [--quiet] KERNEL.elf";

struct Opts {
    mhz: u64,
    cache: cache::Mode,
    max_ms: u64,
    seed: u32,
    trace_traps: bool,
    warn_unmodelled: bool,
    quiet: bool,
    json: bool,
    extra_elfs: Vec<String>,
    schema_files: Vec<String>,
    console_out: Option<String>,
    rtt_poll: u32,
    log_from_rtt: bool,
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
        json: false,
        extra_elfs: Vec::new(),
        schema_files: Vec::new(),
        console_out: None,
        rtt_poll: 4096,
        log_from_rtt: false,
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
            "--json" => o.json = true,
            "--elf" => o.extra_elfs.push(val()?),
            "--schemas" => o.schema_files.push(val()?),
            "--console-out" => o.console_out = Some(val()?),
            "--rtt-poll" => o.rtt_poll = val()?.parse().map_err(|e| format!("--rtt-poll: {e}"))?,
            "--log-from-rtt" => o.log_from_rtt = true,
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

    let mut schemas = t5log::SchemaTable::new();
    let loaded: Result<(), String> = (|| {
        for p in std::iter::once(&o.elf).chain(&o.extra_elfs) {
            schemas.add_elf(p)?;
        }
        for p in &o.schema_files {
            schemas.add_file(p)?;
        }
        Ok(())
    })();
    if let Err(e) = loaded {
        eprintln!("t5emu: {e}");
        return ExitCode::from(64);
    }

    let mut bus = Bus::new(o.mhz * 1_000_000, o.cache, o.seed);
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
    m.rtt_cb = elf.symbol("_SEGGER_RTT");
    m.rtt_poll_interval = o.rtt_poll;
    let limit = o.max_ms * o.mhz * 1000;
    let stop = m.run(limit);
    m.poll_rtt();

    // The RTT channel must carry exactly the frames the console carried (the
    // console has one extra 0x00 that separates the bootloader's text).
    let usj = &m.bus.periph.usj.output;
    let rtt_ok = match m.rtt_cb {
        // A slow or absent host loses frames by design (NO_BLOCK_SKIP), so
        // the streams are only compared with the default polling.
        Some(_) if o.rtt_poll != 4096 => true,
        Some(_) => {
            let trim = |b: &[u8]| b.iter().position(|&x| x != 0).map_or(Vec::new(), |i| b[i..].to_vec());
            let same = trim(usj) == trim(&m.rtt_output);
            if same {
                eprintln!("t5emu: RTT stream matches the console ({} bytes)", m.rtt_output.len());
            } else {
                eprintln!(
                    "t5emu: RTT stream ({} bytes) differs from the console stream ({} bytes)",
                    m.rtt_output.len(),
                    usj.len()
                );
            }
            same
        }
        None => {
            eprintln!("t5emu: no _SEGGER_RTT symbol; RTT not checked");
            true
        }
    };

    let console = if o.log_from_rtt { &m.rtt_output } else { &m.bus.periph.usj.output };
    if let Some(path) = &o.console_out {
        if let Err(e) = std::fs::write(path, console) {
            eprintln!("t5emu: {path}: {e}");
        }
    }
    let mut decoder = t5log::StreamDecoder::new();
    let mut items = decoder.push(console);
    items.extend(decoder.finish());
    let mut passed = false;
    for item in &items {
        match item {
            t5log::Item::Event(e) => {
                if !o.quiet {
                    println!("{}", if o.json { e.to_json(&schemas) } else { e.to_pretty(&schemas) });
                }
                if e.name(&schemas) == Some("selftest.result") {
                    passed = e.field(&schemas, "pass") == Some(&t5log::Value::Bool(true));
                }
            }
            t5log::Item::Text(t) if !o.quiet => println!("| {t}"),
            t5log::Item::Bad { error, .. } => eprintln!("t5emu: undecodable log frame: {error}"),
            _ => {}
        }
    }

    let emulated_ms = m.bus.cycles / (o.mhz * 1000);
    eprintln!(
        "t5emu: stopped after {} instructions, {} ms emulated at {} MHz: {}",
        m.cpu.instret,
        emulated_ms,
        o.mhz,
        describe(&stop)
    );
    match stop {
        Stop::Halted { .. } if passed && rtt_ok => ExitCode::SUCCESS,
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
