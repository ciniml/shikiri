//! tab5-os kernel (M-mode). See doc/07-architecture-proposal.md.
//!
//! M1: runs the embedded apps once in U-mode as a self-test of the trap path
//! and of PMP isolation, then reports PASS/FAIL.

#![no_std]
#![no_main]

mod arch;
mod loader;
mod platform;
mod process;

use arch::riscv32::csr::{cause, read_csr};
use arch::riscv32::{self as rv, pmp};
use platform::{Current, Platform};
use process::{Exit, Process};

static HELLO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.t5a"));
static FAULT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fault.t5a"));

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

    let failures = selftest();
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

fn selftest() -> u32 {
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

    let pool = Current::APP_POOL;
    let mut failures = 0;
    for (name, image, arg0, arg1, expected) in cases {
        // M1 runs one process at a time, so each reuses the start of the pool.
        let loaded = match unsafe { loader::load(image, pool.start, pool.end - pool.start) } {
            Ok(l) => l,
            Err(e) => {
                kprintln!("[{name}] load failed: {e:?}");
                failures += 1;
                continue;
            }
        };
        let mut p = Process::new(name, &loaded, arg0, arg1);
        let exit = p.run();
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
            Exit::Code(_) => kprintln!("[{}] {} {}: {}", p.name, what, v, verdict(ok)),
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
