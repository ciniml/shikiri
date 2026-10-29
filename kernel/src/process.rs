//! A user-mode process and the syscall dispatcher.

use t5_abi::{Error, sys};

use crate::arch::riscv32::csr::cause;
use crate::arch::riscv32::pmp::{self, Region};
use crate::arch::riscv32::trap::{self, Trap, TrapFrame, reg};
use crate::loader::Loaded;
use crate::platform::{Current, Platform};

pub struct Process {
    pub name: &'static str,
    pub frame: TrapFrame,
    pub region: Region,
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(usize),
    Fault { code: usize, pc: usize, tval: usize },
}

enum Next {
    Resume,
    Exit(usize),
}

impl Process {
    pub fn new(name: &'static str, loaded: &Loaded, arg0: usize, arg1: usize) -> Process {
        let mut frame = TrapFrame::default();
        frame.pc = loaded.entry;
        frame.regs[reg::SP] = loaded.stack_top;
        frame.regs[reg::A0] = arg0;
        frame.regs[reg::A1] = arg1;
        Process { name, frame, region: loaded.region }
    }

    /// Runs the process until it exits or faults.
    pub fn run(&mut self) -> Exit {
        pmp::set_process(&self.region);
        loop {
            match trap::run_user(&mut self.frame) {
                Trap::Syscall => {
                    // ecall is always 4 bytes long.
                    self.frame.pc += 4;
                    match self.syscall() {
                        Next::Resume => {}
                        Next::Exit(code) => return Exit::Code(code),
                    }
                }
                Trap::Exception { code, pc, tval } => return Exit::Fault { code, pc, tval },
                Trap::Interrupt { code } => panic!("unexpected interrupt {code} in M1"),
            }
        }
    }

    fn syscall(&mut self) -> Next {
        let a = &self.frame.regs;
        let (nr, a0, a1) = (a[reg::A7], a[reg::A0], a[reg::A1]);
        let result = match nr {
            sys::EXIT => return Next::Exit(a0),
            sys::DBG_WRITE => self.dbg_write(a0, a1),
            _ => Err(Error::NoSupport),
        };
        let r = &mut self.frame.regs;
        match result {
            Ok(v) => {
                r[reg::A0] = 0;
                r[reg::A1] = v;
            }
            Err(e) => r[reg::A0] = (e as usize).wrapping_neg(),
        }
        Next::Resume
    }

    /// Checks that `[ptr, ptr + len)` lies entirely inside memory the process
    /// can read (`write == false`) or write. The range is checked in software
    /// before the kernel touches it (doc/07 §4.1).
    fn check_user_range(&self, ptr: usize, len: usize, write: bool) -> Result<(), Error> {
        if len == 0 {
            return Ok(());
        }
        let end = ptr.checked_add(len).ok_or(Error::BadAddress)?;
        let lo = if write { self.region.text_end } else { self.region.base };
        if ptr >= lo && end <= self.region.end {
            Ok(())
        } else {
            Err(Error::BadAddress)
        }
    }

    fn dbg_write(&self, ptr: usize, len: usize) -> Result<usize, Error> {
        self.check_user_range(ptr, len, false)?;
        // Copy in before use so another core cannot change the bytes between
        // the check and the write (relevant once Core1 runs processes).
        let mut buf = [0u8; 64];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(buf.len());
            unsafe { core::ptr::copy_nonoverlapping((ptr + done) as *const u8, buf.as_mut_ptr(), n) };
            <Current as Platform>::console_write(&buf[..n]);
            done += n;
        }
        Ok(len)
    }
}

pub fn describe(exit: &Exit) -> (&'static str, usize) {
    match exit {
        Exit::Code(c) => ("exit", *c),
        Exit::Fault { code, .. } => (cause::name(*code), *code),
    }
}
