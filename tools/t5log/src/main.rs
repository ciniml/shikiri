//! t5log: tools for tab5-os structured logs (doc/12).
//!
//!   t5log schemas -o OUT ELF...     collect schemas into a file (and check them)
//!   t5log check ELF...              check schemas: id collisions, naming rules
//!   t5log rtt --probe VID:PID:SERIAL --elf KERNEL [--elf APP]... [--schemas F]...
//!             [--chip esp32p4] [--chip-description YAML] [--reset] [--json]
//!             [--cmd COMMAND]... [--until-done]
//!                                   read the log over RTT through probe-rs (used
//!                                   as a library) and decode it as it arrives.
//!                                   Commands (`list`, `ps`, `run NAME [A0 [A1]]`,
//!                                   `kill PID`, `install FILE.t5a`, `remove NAME`,
//!                                   `flashtest`) from --cmd and from stdin go to
//!                                   the kernel's control channel (doc/14 §3).
//!   t5log serial --port SERIAL [--elf E]... [--schemas F]... [--json]
//!             [--cmd COMMAND]... [--capture-dir DIR] [--until-done]
//!                                   the same commands on the console line (USB
//!                                   Serial/JTAG, doc/54), no probe needed;
//!                                   also `capture` (the screen, saved as PNG in
//!                                   DIR), `tap X Y [MS]` and `swipe X0 Y0 X1 Y1
//!                                   [MS]` (a finger, the user's coordinates).
//!   t5log decode [--schemas F]... [--elf E]... [--pretty] [--tcp HOST:PORT | INPUT]
//!                                   decode a log stream to JSON Lines (default)
//!                                   or one line per event (--pretty). INPUT is a
//!                                   file or device (default: stdin). For a tty,
//!                                   set it to raw mode first (stty raw -echo).
//!                                   --hex reads the bytes as hex pairs ("0a 1b "),
//!                                   which is how probe-rs prints a binary RTT
//!                                   channel; other text is ignored.

use std::io::{Read, Write};
use std::process::ExitCode;

mod hostlink;

use t5log::{BootContext, Item, SchemaTable, StreamDecoder};

const USAGE: &str = "usage:
  t5log schemas -o OUT ELF...
  t5log check ELF...
  t5log rtt --probe VID:PID:SERIAL --elf KERNEL [--elf APP]... [--schemas FILE]... \
            [--chip esp32p4] [--chip-description YAML] [--reset [--attach-delay MS]] [--json] \
            [--cmd COMMAND]... [--until-done]
  t5log decode [--schemas FILE]... [--elf ELF]... [--pretty] [--hex] [--tcp HOST:PORT | INPUT]
  t5log status --probe VID:PID:SERIAL [--chip esp32p4] [--chip-description YAML]
  t5log read --probe VID:PID:SERIAL [--chip esp32p4] [--chip-description YAML] ADDR LEN -o OUT
  t5log hostlink --port SERIAL --tun NAME [--schemas FILE]... [--elf ELF]...
  t5log serial --port SERIAL [--elf ELF]... [--schemas FILE]... [--json] [--cmd COMMAND]... \
            [--capture-dir DIR] [--until-done]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("schemas") => cmd_schemas(&args[1..]),
        Some("check") => cmd_check(&args[1..]),
        Some("decode") => cmd_decode(&args[1..]),
        Some("hostlink") => cmd_hostlink(&args[1..]),
        Some("serial") => cmd_serial(&args[1..]),
        Some("rtt") => cmd_rtt(&args[1..]),
        Some("status") => cmd_status(&args[1..]),
        Some("read") => cmd_read(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("t5log: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load_elfs(paths: &[String]) -> Result<SchemaTable, String> {
    let mut t = SchemaTable::new();
    for p in paths {
        t.add_elf(p)?;
    }
    Ok(t)
}

fn check(t: &SchemaTable) -> Result<(), String> {
    let problems = t.check();
    for p in &problems {
        eprintln!("t5log: {p}");
    }
    if problems.is_empty() { Ok(()) } else { Err(format!("{} schema problem(s)", problems.len())) }
}

fn cmd_check(args: &[String]) -> Result<(), String> {
    let t = load_elfs(args)?;
    check(&t)?;
    eprintln!("t5log: {} schemas OK", t.len());
    Ok(())
}

fn cmd_schemas(args: &[String]) -> Result<(), String> {
    let mut out = None;
    let mut elfs = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("-o needs a path")?.clone()),
            _ => elfs.push(a.clone()),
        }
    }
    let out = out.ok_or("missing -o OUT")?;
    let t = load_elfs(&elfs)?;
    check(&t)?;
    std::fs::write(&out, t.to_file_bytes()).map_err(|e| format!("{out}: {e}"))?;
    eprintln!("t5log: {} schemas from {} ELF(s) -> {out}", t.len(), elfs.len());
    Ok(())
}

fn cmd_decode(args: &[String]) -> Result<(), String> {
    let mut t = SchemaTable::new();
    let mut pretty = false;
    let mut hex = false;
    let mut tcp = None;
    let mut input = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--schemas" => {
                t.add_file(it.next().ok_or("--schemas needs a path")?)?;
            }
            "--elf" => {
                t.add_elf(it.next().ok_or("--elf needs a path")?)?;
            }
            "--pretty" => pretty = true,
            "--hex" => hex = true,
            "--json" => pretty = false,
            "--tcp" => tcp = Some(it.next().ok_or("--tcp needs HOST:PORT")?.clone()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            s => input = Some(s.to_string()),
        }
    }
    if t.is_empty() {
        eprintln!("t5log: no schemas given (--schemas/--elf); events will show as ids");
    }
    let mut reader: Box<dyn Read> = match (tcp, input.as_deref()) {
        (Some(addr), _) => Box::new(std::net::TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?),
        (None, None | Some("-")) => Box::new(std::io::stdin()),
        (None, Some(path)) => Box::new(std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?),
    };

    let mut d = StreamDecoder::new();
    let mut ctx = BootContext::new();
    let mut hexd = HexDecoder::default();
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        let bytes = if hex { hexd.push(&buf[..n]) } else { buf[..n].to_vec() };
        for item in d.push(&bytes) {
            print_item(&mut out, &t, &mut ctx, &item, pretty)?;
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    if let Some(item) = d.finish() {
        print_item(&mut out, &t, &mut ctx, &item, pretty)?;
    }
    Ok(())
}

/// `t5log hostlink` (doc/21): IP between the Tab5's console line and a TUN
/// device; the log on the same line is printed.
fn cmd_hostlink(args: &[String]) -> Result<(), String> {
    let mut t = SchemaTable::new();
    let (mut port, mut tun_name) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--port" => port = Some(val()?),
            "--tun" => tun_name = Some(val()?),
            "--schemas" => {
                t.add_file(&val()?)?;
            }
            "--elf" => {
                t.add_elf(&val()?)?;
            }
            s => return Err(format!("unknown option {s}\n{USAGE}")),
        }
    }
    let (port, tun_name) = port.zip(tun_name).ok_or(USAGE)?;
    let mut serial = hostlink::open_serial(&port)?;
    let mut tun = hostlink::open_tun(&tun_name)?;
    let counters = std::sync::Arc::new(hostlink::Counters { to_tab5: 0.into(), from_tab5: 0.into() });
    hostlink::spawn_uplink(
        tun.try_clone().map_err(|e| e.to_string())?,
        serial.try_clone().map_err(|e| e.to_string())?,
        counters.clone(),
    );
    eprintln!("t5log: host link {port} <-> {tun_name}");
    let mut demux = hostlink::Demux::new();
    let mut d = StreamDecoder::new();
    let mut ctx = BootContext::new();
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match serial.read(&mut buf) {
            Ok(0) => return Err(format!("{port}: closed")),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{port}: {e}")),
        };
        let mut log = Vec::new();
        demux.push(&buf[..n], &mut tun, &counters, &mut log);
        for item in d.push(&log) {
            print_item(&mut out, &t, &mut ctx, &item, true)?;
        }
        out.flush().map_err(|e| e.to_string())?;
    }
}

/// `t5log serial` (doc/54): control commands and the log on the console
/// line, and the screen captures it carries. A command goes out when the
/// one before it was answered (and, for `capture`, its screen has come).
fn cmd_serial(args: &[String]) -> Result<(), String> {
    let mut t = SchemaTable::new();
    let mut port = None;
    let mut pretty = true;
    let mut cmds: Vec<String> = Vec::new();
    let mut dir = std::path::PathBuf::from(".");
    let mut until_done = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--port" => port = Some(val()?),
            "--elf" => {
                t.add_elf(&val()?)?;
            }
            "--schemas" => {
                t.add_file(&val()?)?;
            }
            "--json" => pretty = false,
            "--cmd" => {
                let v = val()?;
                t5log::ctl::check_line(&v).map_err(|e| format!("--cmd {v}: {e}"))?;
                cmds.push(v);
            }
            "--capture-dir" => dir = val()?.into(),
            "--until-done" => until_done = true,
            s => return Err(format!("unknown argument {s}\n{USAGE}")),
        }
    }
    let port = port.ok_or(USAGE)?;
    if t.is_empty() {
        eprintln!("t5log: no schemas given (--schemas/--elf); events will show as ids");
    }
    let serial = hostlink::open_serial(&port)?;
    let mut writer = serial.try_clone().map_err(|e| e.to_string())?;
    // The line is read in a thread so the loop can time out.
    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
    {
        let mut serial = serial;
        let port = port.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let r = match serial.read(&mut buf) {
                    Ok(0) => Err(format!("{port}: closed")),
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(format!("{port}: {e}")),
                };
                let end = r.is_err();
                if tx.send(r).is_err() || end {
                    break;
                }
            }
        });
    }
    let (ltx, lines) = std::sync::mpsc::channel::<String>();
    for c in &cmds {
        ltx.send(c.clone()).unwrap();
    }
    if !until_done {
        let interactive = ltx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() && interactive.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(ltx);

    let mut d = StreamDecoder::new();
    let mut ctx = BootContext::new();
    let mut out = std::io::stdout().lock();
    let mut asm = t5log::capture::Assembler::new();
    let mut next_id = 1u32;
    let mut pending = std::collections::BTreeSet::new();
    // A capture asked for and not finished (its id, once answered).
    let mut capturing: Option<Option<u32>> = None;
    let mut capture_request = 0u32;
    let mut failed = false;
    let mut sent = 0usize;
    let mut done_at: Option<std::time::Instant> = None;
    let mut last_activity = std::time::Instant::now();
    loop {
        if pending.is_empty() && capturing.is_none() {
            match lines.try_recv() {
                Ok(line) => {
                    let first = next_id;
                    match t5log::ctl::frames(&line, &mut next_id) {
                        Ok(f) => {
                            writer.write_all(&f.bytes).map_err(|e| format!("{port}: {e}"))?;
                            eprintln!("t5log: -> #{first} {line}");
                            pending.extend(f.answered);
                            if line.split_whitespace().next() == Some("capture") {
                                capturing = Some(None);
                                capture_request = first;
                            }
                        }
                        Err(e) => eprintln!("t5log: {line}: {e}"),
                    }
                    sent += 1;
                    last_activity = std::time::Instant::now();
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) if until_done || sent >= cmds.len() => {
                    if until_done {
                        let t = *done_at.get_or_insert_with(std::time::Instant::now);
                        if t.elapsed() > std::time::Duration::from_millis(500) {
                            break;
                        }
                    }
                }
                Err(_) => {}
            }
        }
        if !pending.is_empty() && last_activity.elapsed() > STALL {
            return Err(format!("no answer to request(s) {pending:?} and no log for {} s", STALL.as_secs()));
        }
        let bytes = match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(r) => r?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        last_activity = std::time::Instant::now();
        for item in d.push(&bytes) {
            match &item {
                Item::Capture(f) => match asm.push(f) {
                    Some(Ok(shot)) => {
                        let path = dir.join(format!(
                            "capture-{}-{}.png",
                            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                            shot.id
                        ));
                        std::fs::write(&path, shot.png()).map_err(|e| format!("{}: {e}", path.display()))?;
                        let (w, h, _) = shot.user_view();
                        eprintln!("t5log: capture {} ({w} x {h}) -> {}", shot.id, path.display());
                        if capturing == Some(Some(shot.id)) {
                            capturing = None;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("t5log: {e}");
                        failed = true;
                        capturing = None;
                    }
                    None => {}
                },
                Item::Event(e) => match e.name(&t) {
                    Some("ctl.done" | "ctl.failed") => {
                        if let Some(t5log::Value::U(id)) = e.field(&t, "request_id") {
                            let id = *id as u32;
                            pending.remove(&id);
                            if id == capture_request && capturing.is_some() {
                                let ok = e.name(&t) == Some("ctl.done");
                                capturing = match e.field(&t, "value") {
                                    Some(t5log::Value::U(v)) if ok => Some(Some(*v as u32)),
                                    _ => None,
                                };
                                failed |= !ok;
                            }
                        }
                    }
                    Some("capture.failed") => {
                        failed = true;
                        capturing = None;
                    }
                    _ => {}
                },
                _ => {}
            }
            print_item(&mut out, &t, &mut ctx, &item, pretty)?;
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    if failed { Err("a capture failed".into()) } else { Ok(()) }
}

#[cfg(not(feature = "rtt"))]
fn cmd_rtt(_: &[String]) -> Result<(), String> {
    Err("this t5log was built without the `rtt` feature".into())
}

#[cfg(feature = "rtt")]
fn cmd_rtt(args: &[String]) -> Result<(), String> {
    let mut t = SchemaTable::new();
    let mut elfs = Vec::new();
    let mut probe = None;
    let mut chip = "esp32p4".to_string();
    let mut chip_description = None;
    let mut reset = false;
    let mut attach_delay = 0;
    let mut pretty = true;
    let mut cmds: Vec<String> = Vec::new();
    let mut until_done = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--probe" => probe = Some(val()?),
            "--elf" => elfs.push(val()?),
            "--schemas" => {
                t.add_file(&val()?)?;
            }
            "--chip" => chip = val()?,
            "--chip-description" => chip_description = Some(val()?),
            "--reset" => reset = true,
            "--attach-delay" => attach_delay = val()?.parse().map_err(|_| "--attach-delay takes milliseconds")?,
            "--json" => pretty = false,
            "--cmd" => {
                let v = val()?;
                t5log::ctl::check_line(&v).map_err(|e| format!("--cmd {v}: {e}"))?;
                cmds.push(v);
            }
            "--until-done" => until_done = true,
            s => return Err(format!("unknown argument {s}\n{USAGE}")),
        }
    }
    let probe = probe.ok_or("--probe is required (see `probe-rs list`)")?;
    let kernel = elfs.first().ok_or("--elf KERNEL is required (it locates _SEGGER_RTT)")?;
    for e in &elfs {
        t.add_elf(e)?;
    }
    let image = std::fs::read(kernel).map_err(|e| format!("{kernel}: {e}"))?;
    let opts = t5log::rtt::Options {
        probe,
        chip,
        chip_description,
        control_block: t5log::rtt::control_block_address(&image).map_err(|e| format!("{kernel}: {e}"))?,
        channel: 0,
        poll: std::time::Duration::from_millis(10),
        reset,
        attach_delay: std::time::Duration::from_millis(attach_delay),
    };
    // Lines typed on stdin become commands too.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    for c in cmds.iter() {
        tx.send(c.clone()).unwrap();
    }
    let interactive = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if !line.is_empty() && interactive.send(line).is_err() {
                break;
            }
        }
    });
    drop(tx);

    let ctl = std::cell::RefCell::new(Ctl {
        // After a reset, commands written before the kernel sets up its
        // control block would be lost: wait for `ctl.ready`.
        ready: !reset,
        queue: std::collections::VecDeque::new(),
        next_id: 1,
        pending: std::collections::BTreeSet::new(),
        sent_lines: 0,
        done_at: None,
        last_activity: std::time::Instant::now(),
        stalled: false,
    });
    let mut d = StreamDecoder::new();
    let mut ctx = BootContext::new();
    let mut out = std::io::stdout().lock();
    let wanted = cmds.len();
    let r = t5log::rtt::stream(
        &opts,
        |bytes| {
            for item in d.push(bytes) {
                if let Item::Event(e) = &item {
                    ctl.borrow_mut().on_event(e, &t);
                }
                if print_item(&mut out, &t, &mut ctx, &item, pretty).is_err() {
                    return false;
                }
            }
            out.flush().is_ok()
        },
        || {
            let mut c = ctl.borrow_mut();
            while let Ok(line) = rx.try_recv() {
                c.queue.push_back(line);
            }
            // --until-done: every --cmd answered, then one more second of log.
            if until_done && c.sent_lines >= wanted && c.pending.is_empty() {
                let t = *c.done_at.get_or_insert_with(std::time::Instant::now);
                if t.elapsed() > std::time::Duration::from_secs(1) {
                    return t5log::rtt::Turn::Stop;
                }
            }
            if until_done && !c.pending.is_empty() && c.last_activity.elapsed() > STALL {
                c.stalled = true;
                return t5log::rtt::Turn::Stop;
            }
            match c.take_frames() {
                Some(bytes) => t5log::rtt::Turn::Send(bytes),
                None => t5log::rtt::Turn::Continue,
            }
        },
    );
    let c = ctl.borrow();
    if c.stalled {
        return Err(format!(
            "no answer to request(s) {:?} and no log for {} s: the kernel is not running (a probe attach also stops the watchdog)",
            c.pending,
            STALL.as_secs()
        ));
    }
    r
}

/// `t5log status`: where core 0 is (halt, read, resume).
fn cmd_status(args: &[String]) -> Result<(), String> {
    let (mut probe, mut chip, mut desc) = (None, "esp32p4".to_string(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--probe" => probe = Some(val()?),
            "--chip" => chip = val()?,
            "--chip-description" => desc = Some(val()?),
            s => return Err(format!("unknown argument {s}\n{USAGE}")),
        }
    }
    let probe = probe.ok_or("--probe is required")?;
    for line in t5log::rtt::status(&probe, &chip, desc.as_deref())? {
        println!("{line}");
    }
    Ok(())
}

/// `t5log read`: target memory to a file, without halting (e.g. the buffer
/// `rngdump` fills; doc/47 §5).
#[cfg(not(feature = "rtt"))]
fn cmd_read(_: &[String]) -> Result<(), String> {
    Err("t5log was built without the rtt feature".into())
}

#[cfg(feature = "rtt")]
fn cmd_read(args: &[String]) -> Result<(), String> {
    let (mut probe, mut chip, mut desc, mut out) = (None, "esp32p4".to_string(), None, None);
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--probe" => probe = Some(val()?),
            "--chip" => chip = val()?,
            "--chip-description" => desc = Some(val()?),
            "-o" => out = Some(val()?),
            s if !s.starts_with('-') => pos.push(s.to_string()),
            s => return Err(format!("unknown argument {s}\n{USAGE}")),
        }
    }
    let num = |s: &str| match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).map_err(|e| format!("{s}: {e}")),
        None => s.parse::<u64>().map_err(|e| format!("{s}: {e}")),
    };
    let [addr, len] = pos.as_slice() else { return Err(USAGE.to_string()) };
    let (addr, len) = (num(addr)?, num(len)? as usize);
    if addr % 4 != 0 || len % 4 != 0 {
        return Err("ADDR and LEN must be multiples of 4".into());
    }
    let (probe, out) = (probe.ok_or("--probe is required")?, out.ok_or("-o is required")?);
    let t = std::time::Instant::now();
    let data = t5log::rtt::read_memory(&probe, &chip, desc.as_deref(), addr, len)?;
    std::fs::write(&out, &data).map_err(|e| format!("{out}: {e}"))?;
    eprintln!("t5log: read {len} bytes at {addr:#x} in {:.1} s", t.elapsed().as_secs_f64());
    Ok(())
}

/// Control commands waiting to go to the kernel, and those not yet answered.
struct Ctl {
    ready: bool,
    queue: std::collections::VecDeque<String>,
    next_id: u32,
    pending: std::collections::BTreeSet<u32>,
    /// Command lines sent so far (an `install` line is many frames).
    sent_lines: usize,
    done_at: Option<std::time::Instant>,
    /// When the last log event arrived (or the last command went out).
    last_activity: std::time::Instant,
    /// Set when --until-done gave up waiting.
    stalled: bool,
}

/// --until-done gives up when commands are unanswered and the kernel has
/// logged nothing for this long (a stopped core would otherwise keep t5log
/// waiting forever; doc/16 §6.4).
const STALL: std::time::Duration = std::time::Duration::from_secs(30);

impl Ctl {
    fn on_event(&mut self, e: &t5log::Event, t: &SchemaTable) {
        self.last_activity = std::time::Instant::now();
        match e.name(t) {
            Some("ctl.ready") => self.ready = true,
            Some("ctl.done" | "ctl.failed") => {
                if let Some(t5log::Value::U(id)) = e.field(t, "request_id") {
                    self.pending.remove(&(*id as u32));
                }
            }
            _ => {}
        }
    }

    /// COBS frames for the queued commands, once the kernel is ready.
    fn take_frames(&mut self) -> Option<Vec<u8>> {
        if !self.ready || self.queue.is_empty() {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(line) = self.queue.pop_front() {
            self.sent_lines += 1;
            let first = self.next_id;
            let f = match t5log::ctl::frames(&line, &mut self.next_id) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("t5log: {line}: {e}");
                    continue;
                }
            };
            bytes.extend_from_slice(&f.bytes);
            if self.next_id - first == 1 {
                eprintln!("t5log: -> #{first} {line}");
            } else {
                eprintln!("t5log: -> #{first}..#{} {line} ({} bytes)", self.next_id - 1, f.bytes.len());
            }
            self.pending.extend(f.answered);
        }
        self.last_activity = std::time::Instant::now();
        (!bytes.is_empty()).then_some(bytes)
    }
}

/// Turns "0a 1b ..." text into bytes. A token is two hex digits surrounded
/// by whitespace; anything else (status lines) is skipped.
#[derive(Default)]
struct HexDecoder {
    token: Vec<u8>,
}

impl HexDecoder {
    fn push(&mut self, text: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &c in text {
            if c.is_ascii_whitespace() {
                if self.token.len() == 2 && self.token.iter().all(u8::is_ascii_hexdigit) {
                    let s = std::str::from_utf8(&self.token).unwrap();
                    out.push(u8::from_str_radix(s, 16).unwrap());
                }
                self.token.clear();
            } else if self.token.len() < 3 {
                self.token.push(c);
            }
        }
        out
    }
}

fn print_item(
    out: &mut impl Write,
    t: &SchemaTable,
    ctx: &mut BootContext,
    item: &Item,
    pretty: bool,
) -> Result<(), String> {
    let line = match (item, pretty) {
        (Item::Event(e), true) => e.to_pretty_in(t, ctx.classify(e, t)),
        (Item::Event(e), false) => e.to_json_in(t, ctx.classify(e, t)),
        (Item::Text(s), true) => s.lines().map(|l| format!("| {l}")).collect::<Vec<_>>().join("\n"),
        (Item::Text(s), false) => {
            let mut j = String::from("{\"text\":");
            j.push_str(&json_string(s));
            j.push('}');
            j
        }
        (Item::Capture(_), _) => return Ok(()),
        (Item::Bad { bytes, error }, _) => {
            eprintln!("t5log: undecodable frame ({error}, {} bytes)", bytes.len());
            return Ok(());
        }
    };
    if line.is_empty() {
        return Ok(());
    }
    writeln!(out, "{line}").map_err(|e| e.to_string())
}

fn json_string(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
