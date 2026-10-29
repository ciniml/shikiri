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
mod loader;
mod log;
mod platform;
mod process;
mod sched;
mod time;

use arch::riscv32::csr::{cause, read_csr};
use arch::riscv32::{self as rv, pmp};
use platform::{Current, Platform};
use process::{Exit, Process};
use t5_abi::event;
use t5_abi::log::Addr;

static HELLO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.t5a"));
static FAULT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fault.t5a"));
static TICKER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ticker.t5a"));
static SPIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spin.t5a"));

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

pub extern "C" fn kmain() -> ! {
    Current::init();
    rv::init_trap_vector(Current::MTVEC_MODE);
    log::init();

    event!(
        Info,
        "boot.start",
        platform = Current::NAME,
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
    dump_pmp();
    if let Some(i) = pmp::first_locked() {
        // Locked entries apply to U-mode too, so isolation cannot hold.
        panic!("PMP entry {i} is locked; rebuild the bootloader with BOOTLOADER_REGION_PROTECTION_ENABLE=n");
    }
    pmp::reset();

    let failures = selftest_m1() + selftest_m2();
    if failures == 0 {
        event!(Info, "selftest.result", failures = failures, pass = true);
    } else {
        event!(Error, "selftest.result", failures = failures, pass = false);
    }
    Current::exit(failures)
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

fn load(name: &'static str, image: &[u8], slot_index: usize, arg0: usize, arg1: usize) -> Option<Process> {
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

fn selftest_m1() -> u32 {
    let kernel_addr = &KERNEL_SECRET as *const u32 as usize;
    let kernel_ram = core::ptr::addr_of!(KERNEL_RAM_CANARY) as usize;
    unsafe { core::ptr::write_volatile(kernel_ram as *mut u32, 0xC0FFEE) };
    let cases: [(&'static str, &'static [u8], usize, usize, Exit); 10] = [
        ("hello", HELLO, 0, 0, Exit::Code(0)),
        ("fault/ok", FAULT, MODE_OK, 0, Exit::Code(0)),
        ("fault/load-mmio", FAULT, MODE_LOAD, Current::PROTECTED_MMIO, fault(cause::LOAD_ACCESS)),
        ("fault/load-kernel", FAULT, MODE_LOAD, kernel_addr, fault(cause::LOAD_ACCESS)),
        ("fault/store-kernel", FAULT, MODE_STORE, kernel_addr, fault(cause::STORE_ACCESS)),
        ("fault/load-kernel-ram", FAULT, MODE_LOAD, kernel_ram, fault(cause::LOAD_ACCESS)),
        ("fault/store-kernel-ram", FAULT, MODE_STORE, kernel_ram, fault(cause::STORE_ACCESS)),
        ("fault/exec-data", FAULT, MODE_EXEC_DATA, 0, fault(cause::INSN_ACCESS)),
        ("fault/store-text", FAULT, MODE_STORE_TEXT, 0, fault(cause::STORE_ACCESS)),
        ("fault/bad-log", FAULT, MODE_BAD_LOG, kernel_addr, Exit::Code(0)),
    ];

    let mut failures = 0;
    for (name, image, arg0, arg1, expected) in cases {
        // One process at a time, so each reuses slot 0.
        let Some(p) = load(name, image, 0, arg0, arg1) else {
            failures += 1;
            continue;
        };
        let mut procs = [p];
        sched::run(&mut procs, 1000);
        let [p] = procs;
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

/// Two tickers sleep on different periods while two spinners compete for the
/// CPU with all FP registers in use.
fn selftest_m2() -> u32 {
    event!(Info, "selftest.m2_start", processes = 4u32, slice_ms = sched::SLICE_MS);
    let specs: [(&'static str, &'static [u8], usize, usize); 4] = [
        ("ticker1", TICKER, 1, 30),
        ("ticker2", TICKER, 2, 45),
        ("spin1", SPIN, 1, 100),
        ("spin2", SPIN, 2, 100),
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
    let mut procs = [a, b, c, d];

    let start = Current::now();
    let stats = sched::run(&mut procs, 3000);
    let elapsed_ms = time::ticks_to_ns(Current::now() - start) / 1_000_000;

    for p in &procs {
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
