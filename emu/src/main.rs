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
use t5emu::periph;

/// Watches the console while the kernel runs (the kernel no longer stops
/// after its self-test, doc/14 §3). Without commands it ends the run once the
/// self-test has reported; with `--ctl` commands it sends them over the RTT
/// down-channel when the kernel says `ctl.ready`, waits for each one's
/// `ctl.done`/`ctl.failed`, then lets the run go on for `linger_ms` to
/// catch what follows (e.g. a started app's output). A `wait-ms N` among
/// the commands holds the ones after it until everything before it was
/// answered and N more milliseconds have run (an app ending, a timeout).
struct Monitor<'a> {
    schemas: &'a t5log::SchemaTable,
    decoder: t5log::StreamDecoder,
    consumed: usize,
    ctx: t5log::BootContext,
    stop_after_selftest: bool,
    selftest_done: bool,
    commands: Vec<String>,
    /// Commands sent so far, and the next request id.
    sent: usize,
    next_id: u32,
    /// End of the `wait-ms` in progress (cycles).
    wait_until: Option<u64>,
    cycles_per_ms: u64,
    ready: bool,
    injected: bool,
    pending: std::collections::BTreeSet<u32>,
    linger_cycles: u64,
    stop_at: Option<u64>,
    /// Events (current boot) seen so far, by name.
    seen: std::collections::BTreeSet<String>,
}

impl Monitor<'_> {
    fn tick(&mut self, m: &mut Machine) -> bool {
        let out = &m.bus.periph.usj.output;
        let items = self.decoder.push(&out[self.consumed..]);
        self.consumed = out.len();
        for item in items {
            let t5log::Item::Event(e) = item else { continue };
            if self.ctx.classify(&e, self.schemas).is_some() {
                continue; // replayed from a previous boot
            }
            let Some(name) = e.name(self.schemas) else { continue };
            self.seen.insert(name.to_string());
            match name {
                "selftest.result" => self.selftest_done = true,
                "ctl.ready" => self.ready = true,
                "ctl.done" | "ctl.failed" => {
                    if let Some(t5log::Value::U(id)) = e.field(self.schemas, "request_id") {
                        self.pending.remove(&(*id as u32));
                    }
                }
                _ => {}
            }
        }
        if self.commands.is_empty() {
            return !(self.stop_after_selftest && self.selftest_done);
        }
        while self.ready && !self.injected {
            let line = &self.commands[self.sent];
            if let Some(ms) = wait_ms(line) {
                if !self.pending.is_empty() {
                    break;
                }
                match self.wait_until {
                    None => self.wait_until = Some(m.bus.cycles + ms * self.cycles_per_ms),
                    Some(t) if m.bus.cycles >= t => self.wait_until = None,
                    Some(_) => break,
                }
                if self.wait_until.is_some() {
                    break;
                }
            } else {
                let f = t5log::ctl::frames(line, &mut self.next_id).expect("checked in parse_args");
                m.rtt_down_pending.extend(&f.bytes);
                self.pending.extend(f.answered);
            }
            self.sent += 1;
            self.injected = self.sent == self.commands.len();
        }
        if self.injected && self.pending.is_empty() && self.stop_at.is_none() {
            self.stop_at = Some(m.bus.cycles + self.linger_cycles);
        }
        self.stop_at.is_none_or(|t| m.bus.cycles < t)
    }
}

/// `wait-ms N`: a pause between commands (see `Monitor`), not sent.
fn wait_ms(line: &str) -> Option<u64> {
    line.strip_prefix("wait-ms ")?.trim().parse().ok()
}

/// Puts the ELF's loaded segments where the bootloader would.
fn load_image(bus: &mut Bus, elf: &t5emu::elf::Elf) -> Result<(), String> {
    for s in &elf.segments {
        if s.data.is_empty() {
            continue; // NOLOAD (.bss, the text gap, .log_mem); the kernel sets them up
        }
        bus.load(s.vaddr, &s.data)?;
    }
    Ok(())
}

const USAGE: &str = "usage: t5emu [--mhz N] [--cache strict|off] [--max-ms N] [--seed N] \
                     [--elf APP.elf]... [--schemas FILE]... [--json] [--console-out FILE] \
                     [--rtt-poll N] [--log-from-rtt] [--resets N] [--expect-prev-event NAME] [--expect-reset] \
                     [--flash FILE@HEXOFFSET]... [--ctl COMMAND|'wait-ms N']... [--expect-event NAME]... \
                     [--linger-ms N] \
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
    resets: u32,
    expect_prev_event: Option<String>,
    /// The run must end in a software reset before the self-test passes
    /// (a kernel that rolls itself back, doc/16 §6.10).
    expect_reset: bool,
    flash_images: Vec<(u32, String)>,
    ctl: Vec<String>,
    expect_events: Vec<String>,
    linger_ms: u64,
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
        resets: 0,
        expect_prev_event: None,
        expect_reset: false,
        flash_images: Vec::new(),
        ctl: Vec::new(),
        expect_events: Vec::new(),
        linger_ms: 300,
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
            "--resets" => o.resets = val()?.parse().map_err(|e| format!("--resets: {e}"))?,
            "--expect-prev-event" => o.expect_prev_event = Some(val()?),
            "--expect-reset" => o.expect_reset = true,
            "--ctl" => {
                let v = val()?;
                if wait_ms(&v).is_none() {
                    t5log::ctl::frames(&v, &mut 0).map_err(|e| format!("--ctl {v}: {e}"))?;
                }
                o.ctl.push(v);
            }
            "--expect-event" => o.expect_events.push(val()?),
            "--linger-ms" => o.linger_ms = val()?.parse().map_err(|e| format!("--linger-ms: {e}"))?,
            "--flash" => {
                let v = val()?;
                let (file, off) = v.rsplit_once('@').ok_or(format!("--flash FILE@OFFSET: {v}"))?;
                let off = u32::from_str_radix(off.trim_start_matches("0x"), 16).map_err(|e| format!("--flash {v}: {e}"))?;
                o.flash_images.push((off, file.to_string()));
            }
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
    bus.periph.reset_cause = periph::reset_reason::POWERON;
    if let Err(e) = load_image(&mut bus, &elf) {
        eprintln!("t5emu: {e}");
        return ExitCode::from(64);
    }
    for (off, file) in &o.flash_images {
        let r = std::fs::read(file).map_err(|e| format!("{file}: {e}")).and_then(|d| bus.load_phys_flash(*off, &d));
        if let Err(e) = r {
            eprintln!("t5emu: {e}");
            return ExitCode::from(64);
        }
    }

    let mut m = Machine::new(bus, elf.entry);
    m.trace_traps = o.trace_traps;
    m.rtt_cb = elf.symbol("_SEGGER_RTT");
    m.rtt_poll_interval = o.rtt_poll;
    let limit = o.max_ms * o.mhz * 1000;
    let mut resets_left = o.resets;
    let mut boots = 1;
    let mut mon = Monitor {
        schemas: &schemas,
        decoder: t5log::StreamDecoder::new(),
        consumed: 0,
        ctx: t5log::BootContext::new(),
        // The L3 test must run on into the lock-up after the self-test.
        stop_after_selftest: o.expect_prev_event.is_none(),
        selftest_done: false,
        commands: o.ctl.clone(),
        sent: 0,
        next_id: 1,
        wait_until: None,
        cycles_per_ms: o.mhz * 1000,
        ready: false,
        injected: false,
        pending: Default::default(),
        linger_cycles: o.linger_ms * o.mhz * 1000,
        stop_at: None,
        seen: Default::default(),
    };
    let stop = loop {
        let stop = m.run(limit, &mut |m| mon.tick(m));
        m.poll_rtt();
        // A locked-up core does nothing until the RWDT fires (judgement;
        // doc/12 §7 checks this on the Tab5).
        let fires = matches!(stop, Stop::Watchdog)
            || (matches!(stop, Stop::Lockup { .. }) && m.bus.periph.lp_wdt.enabled());
        if !fires || resets_left == 0 {
            break stop;
        }
        resets_left -= 1;
        let action = m.bus.periph.lp_wdt.stage0_action();
        let keep_lp = action == periph::STG_RESET_SYSTEM;
        let cause = if keep_lp { periph::reset_reason::HP_SYS_LP_WDT } else { periph::reset_reason::CHIP_LP_WDT };
        eprintln!(
            "t5emu: boot {boots}: {} -> RWDT reset (stage action {action}, LP SRAM {})",
            describe(&stop),
            if keep_lp { "kept" } else { "lost" }
        );
        m.bus.reset_system(keep_lp, cause, o.seed.wrapping_add(boots));
        if let Err(e) = load_image(&mut m.bus, &elf) {
            eprintln!("t5emu: {e}");
            return ExitCode::from(64);
        }
        m.reset_cpu(elf.entry);
        boots += 1;
    };

    // The RTT channel must carry exactly the frames the console carried (the
    // console has one extra 0x00 that separates the bootloader's text).
    let usj = &m.bus.periph.usj.output;
    let rtt_ok = match m.rtt_cb {
        // A slow or absent host loses frames by design (NO_BLOCK_SKIP), so
        // the streams are only compared with the default polling.
        Some(_) if o.rtt_poll != 4096 => true,
        Some(_) => {
            // Compare frame by frame: each boot starts the console (only) with
            // an empty frame.
            let frames = |b: &[u8]| b.split(|&x| x == 0).filter(|f| !f.is_empty()).map(<[u8]>::to_vec).collect::<Vec<_>>();
            let same = frames(usj) == frames(&m.rtt_output);
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
    let mut expected_seen = false;
    let mut ctx = t5log::BootContext::new();
    for item in &items {
        match item {
            t5log::Item::Event(e) => {
                let prev = ctx.classify(e, &schemas);
                if !o.quiet {
                    println!(
                        "{}",
                        if o.json { e.to_json_in(&schemas, prev) } else { e.to_pretty_in(&schemas, prev) }
                    );
                }
                match (e.name(&schemas), prev) {
                    (Some("selftest.result"), None) => {
                        passed = e.field(&schemas, "pass") == Some(&t5log::Value::Bool(true));
                    }
                    (Some(n), Some(_)) if o.expect_prev_event.as_deref() == Some(n) => expected_seen = true,
                    // Events after the last control-channel poll (a run
                    // that ends in a reset) only show up here.
                    (Some(n), None) => {
                        mon.seen.insert(n.to_string());
                    }
                    _ => {}
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
    if let Some(name) = &o.expect_prev_event {
        // L3 test: the event must have come back as a replayed record.
        eprintln!(
            "t5emu: {} boot(s); {name} {} replayed from a previous boot",
            boots,
            if expected_seen { "was" } else { "was NOT" }
        );
        return if expected_seen && rtt_ok { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }
    let missing: Vec<&String> = o.expect_events.iter().filter(|n| !mon.seen.contains(*n)).collect();
    if !missing.is_empty() {
        eprintln!("t5emu: expected events not seen: {missing:?}");
    }
    if !mon.pending.is_empty() {
        eprintln!("t5emu: commands without ctl.done/ctl.failed: {:?}", mon.pending);
    }
    if o.expect_reset {
        let reset = matches!(stop, Stop::SoftwareReset);
        eprintln!("t5emu: {}", if reset { "reset as expected" } else { "expected a software reset" });
        return if reset && rtt_ok && missing.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }
    let i2c = &m.bus.periph.i2c;
    let board_ok = i2c.exp2_writes == 0;
    if i2c.transfers > 0 {
        let (dir, out) = i2c.expander1();
        eprintln!(
            "t5emu: I2C: {} transfers; expander #1 DIR={dir:#04x} OUT={out:#04x}; {} write(s) to expander #2",
            i2c.transfers, i2c.exp2_writes
        );
    }
    let dsi = &m.bus.periph.dsi;
    if dsi.frames > 0 {
        eprintln!("t5emu: display: {} frame(s) sent; list item at {:#010x}", dsi.frames, dsi.last_llp & !0x3F);
    }
    let all_ok = passed && rtt_ok && board_ok && missing.is_empty() && mon.pending.is_empty();
    match stop {
        // A reset is an end like any other once everything expected was
        // seen (`reboot`, doc/16 §3.6).
        Stop::Halted { .. } | Stop::Requested | Stop::SoftwareReset if all_ok => ExitCode::SUCCESS,
        Stop::Halted { .. } | Stop::Requested | Stop::SoftwareReset => ExitCode::from(1),
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
        Stop::Requested => "done (the self-test reported, and the control commands finished)".into(),
        Stop::FlashWhileL2Suspended { addr, pc } => format!(
            "flash window access at {addr:#010x} (pc={pc:#010x}) while the L2 cache was suspended \
             (the real core would hang; the code must run from RAM)"
        ),
        Stop::FlashOpWithL2Active { pc } => format!(
            "ROM flash erase/write called from {pc:#010x} with the L2 cache running \
             (the cache would read flash while it changes)"
        ),
        Stop::StaleFlashRead { addr, pc } => format!(
            "read of {addr:#010x} (pc={pc:#010x}) after the flash there changed, without invalidating \
             the L1 D-cache and L2 (stale data on real hardware)"
        ),
        Stop::FlashWriteProtected { offset } => {
            format!("flash erase/write at offset {offset:#x}, outside otadata, ota_1 and the app store (doc/16 §3.2)")
        }
        Stop::SoftwareReset => "software reset (the bootloader is not modelled)".into(),
    }
}
