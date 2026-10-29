//! A user-mode process and the syscall dispatcher.

use t5_abi::{Error, sys};

use crate::arch::riscv32::csr::cause;
use crate::arch::riscv32::pmp::Region;
use crate::arch::riscv32::trap::{TrapFrame, reg};
use crate::loader::Loaded;
use crate::platform::{Current, Platform};
use crate::time;

pub struct Process {
    pub name: &'static str,
    pub frame: TrapFrame,
    pub region: Region,
    pub state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    /// Waiting until the counter reaches `until` (ticks).
    Sleeping { until: u64 },
    Exited(Exit),
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(usize),
    Fault { code: usize, pc: usize, tval: usize },
    /// The kernel stopped it (the scheduler's time limit ran out).
    Killed,
}

/// What the scheduler should do after a syscall.
pub enum Next {
    /// Keep running the same process.
    Resume,
    /// Pick another process (yield, sleep or exit).
    Switch,
}

impl Process {
    pub fn new(name: &'static str, loaded: &Loaded, arg0: usize, arg1: usize) -> Process {
        let mut frame = TrapFrame::default();
        frame.pc = loaded.entry;
        frame.regs[reg::SP] = loaded.stack_top;
        frame.regs[reg::A0] = arg0;
        frame.regs[reg::A1] = arg1;
        Process { name, frame, region: loaded.region, state: State::Ready }
    }

    pub fn exit(&self) -> Option<Exit> {
        match self.state {
            State::Exited(e) => Some(e),
            _ => None,
        }
    }

    /// Handles the ecall the process just made. `pc` already points past it.
    pub fn syscall(&mut self) -> Next {
        let a = &self.frame.regs;
        let (nr, a0, a1) = (a[reg::A7], a[reg::A0], a[reg::A1]);
        let result = match nr {
            sys::EXIT => {
                self.state = State::Exited(Exit::Code(a0));
                return Next::Switch;
            }
            sys::YIELD => {
                self.set_result(Ok([0, 0]));
                return Next::Switch;
            }
            sys::SLEEP => {
                let ns = ((a1 as u64) << 32) | a0 as u64;
                let until = Current::now().saturating_add(time::ns_to_ticks(ns));
                self.set_result(Ok([0, 0]));
                self.state = State::Sleeping { until };
                return Next::Switch;
            }
            sys::TIME => {
                let ns = time::ticks_to_ns(Current::now());
                Ok([ns as usize, (ns >> 32) as usize])
            }
            sys::DBG_WRITE => self.dbg_write(a0, a1).map(|n| [n, 0]),
            _ => Err(Error::NoSupport),
        };
        self.set_result(result);
        Next::Resume
    }

    /// a0 = 0 and a1/a2 = values on success, a0 = -code on failure.
    fn set_result(&mut self, result: Result<[usize; 2], Error>) {
        let r = &mut self.frame.regs;
        match result {
            Ok([v1, v2]) => {
                r[reg::A0] = 0;
                r[reg::A1] = v1;
                r[reg::A2] = v2;
            }
            Err(e) => r[reg::A0] = (e as usize).wrapping_neg(),
        }
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
        Exit::Killed => ("killed", 0),
    }
}
