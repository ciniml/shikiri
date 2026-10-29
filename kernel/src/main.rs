//! tab5-os kernel (M-mode). See doc/07-architecture-proposal.md.
//!
//! Self-test, run at boot:
//! - M1: runs the embedded apps one at a time in U-mode to test the trap path
//!   and PMP isolation.
//! - M2: runs several apps together to test timer preemption, sleep/yield/
//!   time and lazy FPU switching.
//! Then reports PASS/FAIL.

#![no_std]
#![no_main]

mod arch;
mod loader;
mod platform;
mod process;
mod sched;
mod time;

use arch::riscv32::csr::{cause, read_csr};
use arch::riscv32::{self as rv, pmp};
use platform::{Current, Platform};
use process::{Exit, Process};

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

pub extern "C" fn kmain() -> ! {
    Current::init();
    rv::init_trap_vector(Current::MTVEC_MODE);

    kprintln!("tab5-os kernel ({})", Current::NAME);
    kprintln!(
        "misa={:#010x} mstatus={:#010x} mtvec={:#010x}",
        read_csr!("misa"),
        read_csr!("mstatus"),
        read_csr!("mtvec")
    );
    dump_pmp();
    if let Some(i) = pmp::first_locked() {
        // Locked entries apply to U-mode too, so isolation cannot hold.
        panic!("PMP entry {i} is locked; rebuild the bootloader with BOOTLOADER_REGION_PROTECTION_ENABLE=n");
    }
    pmp::reset();

    let failures = selftest_m1() + selftest_m2();
    if failures == 0 {
        kprintln!("selftest: PASS");
    } else {
        kprintln!("selftest: FAIL ({failures})");
    }
    Current::exit(failures as u32)
}

fn dump_pmp() {
    let cfg = pmp::read_cfg();
    for (i, c) in cfg.iter().enumerate() {
        if *c != 0 {
            kprintln!("pmp{i}: cfg={c:#04x} addr={:#010x}", pmp::read_addr(i));
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
        Ok(l) => Some(Process::new(name, &l, arg0, arg1)),
        Err(e) => {
            kprintln!("[{name}] load failed: {e:?}");
            None
        }
    }
}

fn selftest_m1() -> u32 {
    let kernel_addr = &KERNEL_SECRET as *const u32 as usize;
    let kernel_ram = core::ptr::addr_of!(KERNEL_RAM_CANARY) as usize;
    unsafe { core::ptr::write_volatile(kernel_ram as *mut u32, 0xC0FFEE) };
    let cases: [(&'static str, &'static [u8], usize, usize, Exit); 9] = [
        ("hello", HELLO, 0, 0, Exit::Code(0)),
        ("fault/ok", FAULT, MODE_OK, 0, Exit::Code(0)),
        ("fault/load-mmio", FAULT, MODE_LOAD, Current::PROTECTED_MMIO, fault(cause::LOAD_ACCESS)),
        ("fault/load-kernel", FAULT, MODE_LOAD, kernel_addr, fault(cause::LOAD_ACCESS)),
        ("fault/store-kernel", FAULT, MODE_STORE, kernel_addr, fault(cause::STORE_ACCESS)),
        ("fault/load-kernel-ram", FAULT, MODE_LOAD, kernel_ram, fault(cause::LOAD_ACCESS)),
        ("fault/store-kernel-ram", FAULT, MODE_STORE, kernel_ram, fault(cause::STORE_ACCESS)),
        ("fault/exec-data", FAULT, MODE_EXEC_DATA, 0, fault(cause::INSN_ACCESS)),
        ("fault/store-text", FAULT, MODE_STORE_TEXT, 0, fault(cause::STORE_ACCESS)),
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
        let (what, v) = process::describe(&exit);
        match exit {
            Exit::Fault { pc, tval, .. } => {
                kprintln!("[{}] {} ({}) pc={:#x} tval={:#x}: {}", p.name, what, v, pc, tval, verdict(ok));
                if !ok {
                    let f = &p.frame;
                    kprintln!(
                        "    mcause={:#010x} mstatus={:#010x} sp={:#x} ra={:#x} a0={:#x}",
                        f.mcause,
                        read_csr!("mstatus"),
                        f.regs[2],
                        f.regs[1],
                        f.regs[10]
                    );
                }
            }
            _ => kprintln!("[{}] {} {}: {}", p.name, what, v, verdict(ok)),
        }
        if !ok {
            failures += 1;
        }
    }
    // Kernel data must be untouched.
    if unsafe { core::ptr::read_volatile(&KERNEL_SECRET) } != 0x5EC2_E7
        || unsafe { core::ptr::read_volatile(kernel_ram as *const u32) } != 0xC0FFEE
    {
        kprintln!("kernel memory was modified by U-mode: FAIL");
        failures += 1;
    }
    failures
}

/// Two tickers sleep on different periods while two spinners compete for the
/// CPU with all FP registers in use.
fn selftest_m2() -> u32 {
    kprintln!("m2: 4 processes, {} ms slices", sched::SLICE_MS);
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
        let (what, v) = process::describe(&exit);
        kprintln!("[{}] {} {}: {}", p.name, what, v, verdict(ok));
        if !ok {
            failures += 1;
        }
    }
    kprintln!(
        "m2: {} ms, preemptions={} fpu_switches={} fpu_saves={} idle_waits={}",
        elapsed_ms,
        stats.preemptions,
        stats.fpu_switches,
        stats.fpu_saves,
        stats.idle_waits
    );
    // Each path must have been taken at least once.
    for (what, n) in [
        ("preemption", stats.preemptions),
        ("fpu save", stats.fpu_saves),
        ("idle wait", stats.idle_waits),
    ] {
        if n == 0 {
            kprintln!("m2: no {what} happened: UNEXPECTED");
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

fn verdict(ok: bool) -> &'static str {
    if ok { "ok" } else { "UNEXPECTED" }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("KERNEL PANIC: {info}");
    Current::exit(0xFF)
}
