//! tab5-os kernel (M-mode). See doc/07-architecture-proposal.md.
//!
//! Self-test, run at boot:
//! - M1: runs the embedded apps one at a time in U-mode to test the trap path
//!   and PMP isolation.
//! - M2: runs several apps together to test timer preemption, sleep/yield/
//!   time and lazy FPU switching.
//! Then reports the result as a `selftest.result` event.
//!
//! All output is structured log events (doc/12); decode it with `t5log`.

#![no_std]
#![no_main]

mod arch;
mod flash;
mod handle;
mod loader;
mod log;
mod platform;
mod process;
mod sched;
mod store;
mod time;

use arch::riscv32::csr::{cause, read_csr};
use arch::riscv32::{self as rv, pmp};
use platform::{Current, Platform};
use process::{Exit, Process};
use t5_abi::event;
use t5_abi::log::Addr;

/// The self-test's apps, read from the app store in flash (doc/14 §2).
struct Apps {
    hello: &'static [u8],
    fault: &'static [u8],
    ticker: &'static [u8],
    spin: &'static [u8],
    /// Signed by a key the kernel does not trust.
    hello_untrusted: &'static [u8],
    /// Correctly signed, but for ABI 9.0.
    hello_badabi: &'static [u8],
    /// M3d (handles and timers). Optional: a store written before M3d does
    /// not have it, and installing it needs the kernel running.
    timers: Option<&'static [u8]>,
}

impl Apps {
    fn from_store(s: &store::Store) -> Option<Apps> {
        let get = |name: &'static str| {
            let image = s.image(name);
            if image.is_none() {
                event!(Error, "selftest.missing_app", name = name);
            }
            image
        };
        Some(Apps {
            hello: get("hello")?,
            fault: get("fault")?,
            ticker: get("ticker")?,
            spin: get("spin")?,
            hello_untrusted: get("hello-untrusted")?,
            hello_badabi: get("hello-badabi")?,
            timers: s.image("timers"),
        })
    }
}

/// Kernel data the fault test tries to read and overwrite: one in read-only
/// data (flash on ESP32-P4) and one in RAM.
static KERNEL_SECRET: u32 = 0x5EC2_E7;
static mut KERNEL_RAM_CANARY: u32 = 0;

// Modes understood by user/apps/fault.
const MODE_OK: usize = 0;
const MODE_LOAD: usize = 1;
const MODE_STORE: usize = 2;
const MODE_EXEC_DATA: usize = 3;
const MODE_STORE_TEXT: usize = 4;
const MODE_BAD_LOG: usize = 5;
const MODE_OPEN_DENIED: usize = 6;

pub extern "C" fn kmain() -> ! {
    Current::init();
    rv::init_trap_vector(Current::MTVEC_MODE);
    let boot_id = log::init();

    event!(
        Info,
        "boot.start",
        platform = Current::NAME,
        boot_id = boot_id,
        reset_cause = Current::reset_cause(),
        abi_major = t5_abi::ABI_MAJOR,
        abi_minor = t5_abi::ABI_MINOR
    );
    event!(
        Info,
        "boot.cpu",
        misa = read_csr!("misa"),
        mstatus = read_csr!("mstatus"),
        mtvec = Addr(read_csr!("mtvec") as u32)
    );
    Current::log_info();
    dump_pmp();
    if let Some(i) = pmp::first_locked() {
        // Locked entries apply to U-mode too, so isolation cannot hold.
        panic!("PMP entry {i} is locked; rebuild the bootloader with BOOTLOADER_REGION_PROTECTION_ENABLE=n");
    }
    pmp::reset();

    let store = Current::map_flash().map(|window| {
        // A check that the window shows the flash: the CRC of the first
        // 4 KiB of ota_0 (0x10000), which is the start of the kernel's own
        // image file, so the host can compare.
        let ota0 = unsafe { core::slice::from_raw_parts((window + 0x1_0000) as *const u8, 4096) };
        event!(Info, "flash.mapped", window = Addr(window as u32), ota0_crc32 = t5_abi::store::crc32(ota0));
        let store = store::Store::open(window);
        store.log_contents();
        store
    });
    let failures = match store.as_ref().map(Apps::from_store) {
        Some(Some(apps)) => selftest_m3a(&apps) + selftest_m1(&apps) + selftest_m2(&apps) + selftest_m3d(&apps),
        Some(None) => 1,
        None => {
            event!(Error, "selftest.no_flash");
            1
        }
    };
    if failures == 0 {
        event!(Info, "selftest.result", failures = failures, pass = true);
    } else {
        event!(Error, "selftest.result", failures = failures, pass = false);
    }
    #[cfg(feature = "test-lockup")]
    lockup_test();
    match store {
        Some(mut store) if Current::SERVE => serve(&mut store),
        _ => Current::exit(failures),
    }
}

/// Process ids handed out by the service loop (the self-test numbers its
/// processes by slot).
static mut NEXT_PID: u32 = 100;

fn next_pid() -> u32 {
    // Single core, interrupts off in the kernel.
    unsafe {
        let p = &mut *core::ptr::addr_of_mut!(NEXT_PID);
        *p += 1;
        *p
    }
}

/// Control frames handled per scheduler poll (every `sched::POLL_MS`).
const FRAMES_PER_POLL: usize = 4;

/// Runs apps for good: starts the autostart apps, then starts and stops
/// apps on commands from the host (RTT down-channel, doc/14 §3). Results
/// are log events.
fn serve(store: &mut store::Store) -> ! {
    let mut s = sched::Scheduler::new();
    for e in store.entries().filter(|e| e.flags & t5_abi::t5a::tlv::FLAG_AUTOSTART as u8 != 0) {
        let name = e.name();
        if let Err(why) = start(&mut s, store, name, 0, 0) {
            event!(Error, "ctl.autostart_failed", name = name, error = why);
        }
    }
    let (stack_size, stack_used) = rv::stack_usage();
    event!(Info, "ctl.ready", slots = sched::MAX_PROCS as u32, stack_size = stack_size, stack_used = stack_used);
    loop {
        s.run(None, &mut |s| {
            s.reap();
            // A few frames per poll, so an install (one frame per 256 B,
            // some with a sector erase) does not stop the apps meanwhile.
            let mut frame = [0u8; t5_abi::ctl::MAX_COMMAND];
            for _ in 0..FRAMES_PER_POLL {
                let Some(n) = log::read_command(&mut frame) else { break };
                handle(s, store, &frame[..n]);
            }
        });
    }
}

fn handle(s: &mut sched::Scheduler, store: &mut store::Store, frame: &[u8]) {
    use t5_abi::ctl::Command;
    let Some((id, cmd)) = t5_abi::ctl::decode(frame) else {
        event!(Warn, "ctl.bad_command", len = frame.len() as u32);
        return;
    };
    let result: Result<u32, &'static str> = match cmd {
        Command::List => {
            store.log_contents();
            Ok(store.entries().count() as u32)
        }
        Command::Ps => {
            let mut n = 0;
            for p in s.slots.iter().flatten() {
                let state = match p.state {
                    process::State::Ready => "ready",
                    process::State::Sleeping { .. } => "sleeping",
                    process::State::Waiting { .. } => "waiting",
                    process::State::Exited(_) => "exited",
                };
                event!(Info, "proc.info", pid = p.pid, name = p.name, state = state, handles = p.handles.count() as u32);
                n += 1;
            }
            let (size, used) = rv::stack_usage();
            event!(Info, "kernel.stack", size = size, used = used);
            Ok(n)
        }
        Command::Run { name, arg0, arg1 } => start(s, store, name, arg0, arg1),
        Command::Kill { pid } => {
            if s.kill(pid) {
                Ok(pid)
            } else {
                Err("no such process")
            }
        }
        Command::InstallBegin { length } => store.install_begin(length),
        // Answered only on failure (doc/14 §3).
        Command::InstallData { offset, data } => match store.install_data(offset, data) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        Command::InstallEnd => store.install_end().map(|e| e.length),
        Command::Remove { name } => store.remove(name),
        Command::FlashTest => store.flash_test(),
        Command::Psram => Current::psram_init().map(|(_, size)| size as u32),
    };
    match result {
        Ok(value) => event!(Info, "ctl.done", request_id = id, cmd = cmd.name(), ok = true, value = value),
        Err(error) => event!(Warn, "ctl.failed", request_id = id, cmd = cmd.name(), ok = false, error = error),
    }
}

/// Loads `name` from the store into a free slot and makes it runnable.
fn start(s: &mut sched::Scheduler, store: &store::Store, name: &str, arg0: u32, arg1: u32) -> Result<u32, &'static str> {
    let slot_index = s.free_slot().ok_or("no free slot")?;
    let image = store.image(name).ok_or("no such app")?;
    let (base, size) = slot(slot_index);
    let loaded = unsafe { loader::load(image, base, size) }.map_err(|e| {
        let mut why = t5_abi::log::FmtBuf::<64>::new();
        let _ = core::fmt::Write::write_fmt(&mut why, format_args!("{e:?}"));
        event!(Error, "loader.failed", name = name, error = why.as_str());
        "load failed"
    })?;
    let pid = next_pid();
    s.slots[slot_index] = Some(Process::new(pid, loaded.name, &loaded, arg0 as usize, arg1 as usize));
    Ok(pid)
}

/// L3 test (doc/12 §7): a kernel fault after the processes have run. The
/// kernel is still inside the trap taken from the last process (it never
/// mret'ed), so on the ESP32-P4 the core locks up instead of reaching the
/// panic handler (doc/11 §4). The next boot must replay this event.
#[cfg(feature = "test-lockup")]
fn lockup_test() {
    event!(Warn, "selftest.lockup_now");
    unsafe { core::ptr::read_volatile(4 as *const u32) };
}

fn dump_pmp() {
    let cfg = pmp::read_cfg();
    for (i, c) in cfg.iter().enumerate() {
        if *c != 0 {
            event!(Info, "boot.pmp", index = i, cfg = *c, addr = pmp::read_addr(i));
        }
    }
}

/// Slot `i` of the app pool; each process gets one.
fn slot(i: usize) -> (usize, usize) {
    let pool = Current::APP_POOL;
    let size = (pool.end - pool.start) / sched::MAX_PROCS / pmp::GRANULE * pmp::GRANULE;
    (pool.start + i * size, size)
}

fn load(name: &'static str, image: &'static [u8], slot_index: usize, arg0: usize, arg1: usize) -> Option<Process> {
    let (base, size) = slot(slot_index);
    match unsafe { loader::load(image, base, size) } {
        Ok(l) => Some(Process::new(slot_index as u32 + 1, name, &l, arg0, arg1)),
        Err(e) => {
            let mut why = t5_abi::log::FmtBuf::<64>::new();
            let _ = core::fmt::Write::write_fmt(&mut why, format_args!("{e:?}"));
            event!(Error, "loader.failed", name = name, error = why.as_str());
            None
        }
    }
}

fn selftest_m1(apps: &Apps) -> u32 {
    let kernel_addr = &KERNEL_SECRET as *const u32 as usize;
    let kernel_ram = core::ptr::addr_of!(KERNEL_RAM_CANARY) as usize;
    unsafe { core::ptr::write_volatile(kernel_ram as *mut u32, 0xC0FFEE) };
    let cases: [(&'static str, &'static [u8], usize, usize, Exit); 11] = [
        ("hello", apps.hello, 0, 0, Exit::Code(0)),
        ("fault/ok", apps.fault, MODE_OK, 0, Exit::Code(0)),
        ("fault/load-mmio", apps.fault, MODE_LOAD, Current::PROTECTED_MMIO, fault(cause::LOAD_ACCESS)),
        ("fault/load-kernel", apps.fault, MODE_LOAD, kernel_addr, fault(cause::LOAD_ACCESS)),
        ("fault/store-kernel", apps.fault, MODE_STORE, kernel_addr, fault(cause::STORE_ACCESS)),
        ("fault/load-kernel-ram", apps.fault, MODE_LOAD, kernel_ram, fault(cause::LOAD_ACCESS)),
        ("fault/store-kernel-ram", apps.fault, MODE_STORE, kernel_ram, fault(cause::STORE_ACCESS)),
        ("fault/exec-data", apps.fault, MODE_EXEC_DATA, 0, fault(cause::INSN_ACCESS)),
        ("fault/store-text", apps.fault, MODE_STORE_TEXT, 0, fault(cause::STORE_ACCESS)),
        ("fault/bad-log", apps.fault, MODE_BAD_LOG, kernel_addr, Exit::Code(0)),
        ("fault/open-denied", apps.fault, MODE_OPEN_DENIED, kernel_addr, Exit::Code(0)),
    ];

    let mut failures = 0;
    for (name, image, arg0, arg1, expected) in cases {
        // One process at a time, so each reuses slot 0.
        let Some(p) = load(name, image, 0, arg0, arg1) else {
            failures += 1;
            continue;
        };
        let mut s = sched::Scheduler::new();
        s.slots[0] = Some(p);
        s.run(Some(1000), &mut |_| {});
        let p = s.slots[0].take().unwrap();
        let exit = p.exit().unwrap();
        let ok = matches(&exit, &expected);
        log_case(&p, &exit, ok);
        if !ok {
            let f = &p.frame;
            event!(
                Error,
                "selftest.unexpected",
                name = p.name,
                mcause = f.mcause,
                mstatus = read_csr!("mstatus"),
                sp = Addr(f.regs[2] as u32),
                ra = Addr(f.regs[1] as u32),
                a0 = f.regs[10]
            );
            failures += 1;
        }
    }
    // Kernel data must be untouched.
    if unsafe { core::ptr::read_volatile(&KERNEL_SECRET) } != 0x5EC2_E7
        || unsafe { core::ptr::read_volatile(kernel_ram as *const u32) } != 0xC0FFEE
    {
        event!(Error, "selftest.kernel_memory_modified");
        failures += 1;
    }
    failures
}

/// Images the loader must reject before running anything (doc/14 §1.4).
fn selftest_m3a(apps: &Apps) -> u32 {
    use loader::LoadError;
    use t5_abi::t5a::Error;

    // A copy of a good image with one byte of the app's code flipped. The
    // copy lives in the last pool slot, which nothing else uses yet.
    let (scratch, scratch_size) = slot(sched::MAX_PROCS - 1);
    assert!(apps.hello.len() <= scratch_size);
    let tampered = unsafe {
        core::ptr::copy_nonoverlapping(apps.hello.as_ptr(), scratch as *mut u8, apps.hello.len());
        let b = core::slice::from_raw_parts_mut(scratch as *mut u8, apps.hello.len());
        b[apps.hello.len() / 2] ^= 0x01;
        &*b
    };
    let cases: [(&'static str, &[u8], fn(&LoadError) -> bool); 3] = [
        ("verify/tampered", tampered, |e| matches!(e, LoadError::Image(Error::BadSignature))),
        ("verify/untrusted-key", apps.hello_untrusted, |e| matches!(e, LoadError::Image(Error::UnknownKey))),
        ("verify/abi-mismatch", apps.hello_badabi, |e| matches!(e, LoadError::Image(Error::AbiMismatch { .. }))),
    ];
    let mut failures = 0;
    for (name, image, expected) in cases {
        let (base, size) = slot(0);
        let result = unsafe { loader::load(image, base, size) };
        let mut why = t5_abi::log::FmtBuf::<64>::new();
        let ok = match &result {
            Ok(_) => {
                let _ = core::fmt::Write::write_str(&mut why, "loaded");
                false
            }
            Err(e) => {
                let _ = core::fmt::Write::write_fmt(&mut why, format_args!("{e:?}"));
                expected(e)
            }
        };
        event!(Info, "selftest.case", name = name, ok = ok, result = why.as_str());
        if !ok {
            failures += 1;
        }
    }
    failures
}

/// Two tickers sleep on different periods while two spinners compete for the
/// CPU with all FP registers in use.
fn selftest_m2(apps: &Apps) -> u32 {
    event!(Info, "selftest.m2_start", processes = 4u32, slice_ms = sched::SLICE_MS);
    let specs: [(&'static str, &'static [u8], usize, usize); 4] = [
        ("ticker1", apps.ticker, 1, 30),
        ("ticker2", apps.ticker, 2, 45),
        ("spin1", apps.spin, 1, 100),
        ("spin2", apps.spin, 2, 100),
    ];
    let mut failures = 0;
    let mut slot_index = 0;
    let loaded = specs.map(|(name, image, arg0, arg1)| {
        let p = load(name, image, slot_index, arg0, arg1);
        slot_index += 1;
        p
    });
    let [Some(a), Some(b), Some(c), Some(d)] = loaded else {
        return 1;
    };
    let mut s = sched::Scheduler::new();
    s.slots = [Some(a), Some(b), Some(c), Some(d)];

    let start = Current::now();
    s.run(Some(3000), &mut |_| {});
    let stats = s.stats;
    let elapsed_ms = time::ticks_to_ns(Current::now() - start) / 1_000_000;

    for p in s.slots.iter().flatten() {
        let exit = p.exit().unwrap();
        let ok = exit == Exit::Code(0);
        log_case(p, &exit, ok);
        if !ok {
            failures += 1;
        }
    }
    event!(
        Info,
        "selftest.m2_stats",
        elapsed_ms = elapsed_ms,
        preemptions = stats.preemptions,
        fpu_switches = stats.fpu_switches,
        fpu_saves = stats.fpu_saves,
        idle_waits = stats.idle_waits
    );
    // Each path must have been taken at least once.
    for (what, n) in [
        ("preemption", stats.preemptions),
        ("fpu save", stats.fpu_saves),
        ("idle wait", stats.idle_waits),
    ] {
        if n == 0 {
            event!(Error, "selftest.path_not_taken", path = what);
            failures += 1;
        }
    }
    failures
}

/// Two processes wait on timers of different periods while a third keeps
/// the CPU busy; each checks its own intervals and the handle rules and
/// exits 0 if they held (user/apps/timers).
fn selftest_m3d(apps: &Apps) -> u32 {
    let Some(timers) = apps.timers else {
        event!(Warn, "selftest.skipped", part = "m3d", reason = "no timers app in the store");
        return 0;
    };
    let specs: [(&'static str, &'static [u8], usize, usize); 3] =
        [("timers20", timers, 20, 10), ("timers35", timers, 35, 6), ("spin", apps.spin, 3, 150)];
    let mut slot_index = 0;
    let loaded = specs.map(|(name, image, arg0, arg1)| {
        let p = load(name, image, slot_index, arg0, arg1);
        slot_index += 1;
        p
    });
    let [Some(a), Some(b), Some(c)] = loaded else {
        return 1;
    };
    let mut s = sched::Scheduler::new();
    s.slots = [Some(a), Some(b), Some(c), None];
    s.run(Some(2000), &mut |_| {});
    let mut failures = 0;
    for p in s.slots.iter().flatten() {
        let exit = p.exit().unwrap();
        // spin exits 2 when nothing preempted it for long; the timer
        // processes only run for microseconds, so that is expected here.
        let ok = exit == Exit::Code(0) || (p.name == "spin" && exit == Exit::Code(2));
        log_case(p, &exit, ok);
        if !ok {
            failures += 1;
        }
    }
    failures
}

const fn fault(code: usize) -> Exit {
    Exit::Fault { code, pc: 0, tval: 0 }
}

fn matches(got: &Exit, want: &Exit) -> bool {
    match (got, want) {
        (Exit::Code(a), Exit::Code(b)) => a == b,
        (Exit::Fault { code: a, .. }, Exit::Fault { code: b, .. }) => a == b,
        _ => false,
    }
}

fn log_case(p: &Process, exit: &Exit, ok: bool) {
    let (what, code) = process::describe(exit);
    match *exit {
        Exit::Fault { pc, tval, .. } => event!(
            Info,
            "selftest.case",
            name = p.name,
            ok = ok,
            result = what,
            code = code,
            pc = Addr(pc as u32),
            tval = Addr(tval as u32)
        ),
        _ => event!(Info, "selftest.case", name = p.name, ok = ok, result = what, code = code),
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write as _;
    let mut msg = t5_abi::log::FmtBuf::<{ t5_abi::log::MAX_STR }>::new();
    let _ = write!(msg, "{}", info.message());
    let (file, line) = info.location().map_or(("?", 0), |l| (l.file(), l.line()));
    event!(Error, "kernel.panic", file = file, line = line, msg = msg.as_str());
    Current::exit(0xFF)
}
