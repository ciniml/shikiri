//! A user-mode process and the syscall dispatcher.

use t5_abi::log::{self as wire, Addr};
use t5_abi::{Error, event, sys};

use crate::arch::riscv32::csr::cause;
use crate::arch::riscv32::pmp::Region;
use crate::arch::riscv32::trap::{TrapFrame, reg};
use crate::loader::Loaded;
use crate::log;
use crate::platform::{Current, Platform};
use crate::time;

pub struct Process {
    /// Log source id (>= 1; 0 is the kernel).
    pub pid: u32,
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
    pub fn new(pid: u32, name: &'static str, loaded: &Loaded, arg0: usize, arg1: usize) -> Process {
        let mut frame = TrapFrame::default();
        frame.pc = loaded.entry;
        frame.regs[reg::SP] = loaded.stack_top;
        frame.regs[reg::A0] = arg0;
        frame.regs[reg::A1] = arg1;
        event!(
            Info,
            "proc.start",
            pid = pid,
            name = name,
            base = Addr(loaded.region.base as u32),
            build_id = &loaded.build_id[..]
        );
        Process { pid, name, frame, region: loaded.region, state: State::Ready }
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
            sys::LOG_EMIT => self.log_emit(a0, a1).map(|_| [0, 0]),
            sys::LOG_LEVEL => Ok([log::KEEP as usize, 0]),
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

    fn log_emit(&self, ptr: usize, len: usize) -> Result<(), Error> {
        if len > wire::MAX_PAYLOAD {
            return Err(Error::Size);
        }
        self.check_user_range(ptr, len, false)?;
        // Copy in before validating, so the bytes cannot change in between
        // (relevant once Core1 runs processes).
        let mut buf = [0u8; wire::MAX_PAYLOAD];
        unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), len) };
        if log::record(self.pid, &buf[..len]) {
            Ok(())
        } else {
            event!(Warn, "log.rejected", pid = self.pid, len = len);
            Err(Error::Invalid)
        }
    }
}

pub fn describe(exit: &Exit) -> (&'static str, usize) {
    match exit {
        Exit::Code(c) => ("exit", *c),
        Exit::Fault { code, .. } => (cause::name(*code), *code),
        Exit::Killed => ("killed", 0),
    }
}
