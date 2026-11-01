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
mod channel;
mod flash;
mod handle;
mod hostlink;
mod wlan;
mod kupdate;
mod loader;
mod log;
mod platform;
mod process;
mod prof;
mod sched;
mod screen;
mod shm;
mod smp;
mod spawn;
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
    /// M4b (XIP, doc/15): `hello` and `xipcheck` linked for XIP. Optional
    /// like `timers`.
    hello_xip: Option<&'static [u8]>,
    xipcheck: Option<&'static [u8]>,
    /// M5c (channels, doc/16 §5). Optional like `timers`.
    echo: Option<&'static [u8]>,
    /// M6c (display and touch, doc/17 §6). Optional like `timers`.
    paint: Option<&'static [u8]>,
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
            hello_xip: s.image("hello-xip"),
            xipcheck: s.image("xipcheck"),
            echo: s.image("echo"),
            paint: s.image("paint"),
        })
    }
}

/// The end of the PSRAM kept for the display: room for two 720x1280 RGB565
/// framebuffers (doc/17 §3).
const FB_RESERVE: usize = 4 << 20;
/// Process slots start on MMU pages (and so on PMP granules).
const SLOT_ALIGN: usize = 64 << 10;
/// Below the display's: shared memory (doc/19 §2).
const SHM_RESERVE: usize = 4 << 20;

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
    smp::start();

    let window = Current::map_flash();
    let store = window.map(|window| {
        // A check that the window shows the flash: the CRC of the first
        // 4 KiB of ota_0 (0x10000), which is the start of the kernel's own
        // image file, so the host can compare.
        let ota0 = unsafe { core::slice::from_raw_parts((window + 0x1_0000) as *const u8, 4096) };
        event!(Info, "flash.mapped", window = Addr(window as u32), ota0_crc32 = t5_abi::store::crc32(ota0));
        let store = store::Store::open(window);
        store.log_contents();
        store
    });
    // Kernel slots and otadata (M5a); a rollback is reported here.
    let mut kup = window.map(kupdate::Kupdate::new);
    if let Some(k) = &kup {
        k.log_state();
    }
    if cfg!(feature = "test-bad-panic") {
        panic!("test-bad-panic: this kernel must be rolled back");
    }
    // Apps run from PSRAM when there is one (M4a), else from the L2MEM pool.
    // Apps get the PSRAM but its last `FB_RESERVE`, the display's (doc/17
    // §3), and the `SHM_RESERVE` below it, shared memory's (doc/19 §2).
    let framebuffer = match Current::psram_init() {
        Ok((window, size)) if size > 2 * (FB_RESERVE + SHM_RESERVE) => {
            let shm_base = window + size - FB_RESERVE - SHM_RESERVE;
            set_pool(window..shm_base);
            shm::init(shm_base, SHM_RESERVE);
            // The L2MEM pool apps used before is free now: the Wi-Fi
            // frame rings (doc/35 §5).
            if Current::APP_POOL.len() >= t5_abi::netdev::ring::SIZE {
                wlan::set_ring_area(Current::APP_POOL.start);
            }
            Some(window + size - FB_RESERVE)
        }
        Ok((window, size)) => {
            set_pool(window..window + size);
            None
        }
        Err(why) => {
            event!(Warn, "boot.no_psram", error = why);
            None
        }
    };
    Current::clock_init();
    // The panel and touch (M6a). Not part of the self-test's result: a
    // kernel must be able to confirm itself without them (doc/17 §2).
    let t = Current::now();
    let panel = Current::board_init();
    event!(Info, "board.ready", panel = panel_name(panel), ms = time::ticks_to_ns(Current::now() - t) / 1_000_000);
    if let Some(fb) = framebuffer {
        let t = Current::now();
        match Current::display_init(panel, fb) {
            Ok(_) => event!(Info, "display.ready", ms = time::ticks_to_ns(Current::now() - t) / 1_000_000),
            Err(why) => event!(Warn, "display.failed", error = why),
        }
    }
    let (pool, slot_size) = (pool(), slot(0).1);
    event!(Info, "boot.pool", base = Addr(pool.start as u32), size = (pool.end - pool.start) as u32, slot_size = slot_size as u32);
    let failures = match store.as_ref().map(Apps::from_store) {
        Some(Some(apps)) => selftest_m3a(&apps) + selftest_m1(&apps) + selftest_m2(&apps) + selftest_m3d(&apps) + selftest_m4b(&apps) + selftest_m5c(&apps) + selftest_m6c(&apps),
        Some(None) => 1,
        None => {
            event!(Error, "selftest.no_flash");
            1
        }
    };
    #[cfg(feature = "test-bad-selftest")]
    let failures = failures + 1;
    if failures == 0 {
        event!(Info, "selftest.result", failures = failures, pass = true);
    } else {
        event!(Error, "selftest.result", failures = failures, pass = false);
    }
    if let Some(k) = &kup {
        k.after_selftest(failures == 0);
    }
    #[cfg(feature = "test-lockup")]
    lockup_test();
    match (store, kup.as_mut()) {
        (Some(mut store), Some(kup)) if Current::SERVE => serve(&mut store, kup),
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
fn serve(store: &mut store::Store, kup: &mut kupdate::Kupdate) -> ! {
    let mut s = sched::Scheduler::new();
    let mut touch_log = TouchLog::default();
    for e in store.entries().filter(|e| e.is_current() && e.flags & t5_abi::t5a::tlv::FLAG_AUTOSTART as u8 != 0) {
        let name = e.name();
        if let Err(why) = start(&mut s, store, name, 0, 0) {
            event!(Error, "ctl.autostart_failed", name = name, error = why);
        }
    }
    let (stack_size, stack_used) = rv::stack_usage();
    Current::log_power("selftest_done");
    event!(Info, "ctl.ready", slots = sched::MAX_PROCS as u32, stack_size = stack_size, stack_used = stack_used);
    loop {
        sched::run(&mut s, None, &mut |s| {
            settle_updates(s, store);
            spawn_requested(s, store);
            touch_log.poll();
            screen::poll();
            s.reap();
            // A few frames per poll, so an install (one frame per 256 B,
            // some with a sector erase) does not stop the apps meanwhile.
            let mut frame = [0u8; t5_abi::ctl::MAX_COMMAND];
            for _ in 0..FRAMES_PER_POLL {
                let Some(n) = log::read_command(&mut frame) else { break };
                handle(s, store, kup, &mut touch_log, &frame[..n]);
            }
        });
    }
}

/// Starts the apps processes asked for through `proc:spawn` (doc/38).
fn spawn_requested(s: &mut sched::Scheduler, store: &store::Store) {
    while let Some(r) = spawn::take() {
        match start(s, store, r.name(), r.arg0, r.arg1) {
            Ok(pid) => event!(Info, "proc.spawned", by = r.by, name = r.name(), pid = pid),
            Err(why) => event!(Warn, "proc.spawn_failed", by = r.by, name = r.name(), error = why),
        }
    }
}

/// How long the first run of an updated app must last, unless it ends
/// normally sooner, to confirm the update (doc/16 §4.2).
const CONFIRM_AFTER_MS: u64 = 2000;

/// Decides unconfirmed app versions from how their processes did: a normal
/// end or `CONFIRM_AFTER_MS` of running confirms, a fault rolls back, and a
/// kill from the host decides nothing.
fn settle_updates(s: &mut sched::Scheduler, store: &mut store::Store) {
    let now = Current::now();
    for i in 0..s.slots.len() {
        let Some(p) = s.slots[i].as_mut() else { continue };
        let Some((since, first)) = p.pending else { continue };
        let name = p.name;
        let r = match p.exit() {
            Some(process::Exit::Code(_)) => Some(store.confirm(name, first, "exit")),
            Some(process::Exit::Fault { .. }) => Some(store.rollback(name, Some(first), "fault")),
            Some(process::Exit::Killed) => None,
            None if now - since >= time::ms_to_ticks(CONFIRM_AFTER_MS) => Some(store.confirm(name, first, "running")),
            None => continue,
        };
        if let Some(Err(why)) = r {
            event!(Warn, "store.settle_skipped", name = name, error = why);
        }
        // Decided: other processes of the same version need not decide
        // again.
        for q in s.slots.iter_mut().flatten().filter(|q| q.pending.is_some_and(|(_, f)| f == first)) {
            q.pending = None;
        }
    }
}

fn panel_name(p: platform::Panel) -> &'static str {
    match p {
        platform::Panel::None => "none",
        platform::Panel::Unknown => "unknown",
        platform::Panel::Ili9881c { .. } => "ili9881c",
        platform::Panel::St7123 => "st7123",
        platform::Panel::St7121 => "st7121",
    }
}

/// The `touch` command (M6a): logs touch points for a while, read every
/// `TOUCH_POLL_MS`. A point is logged when it appears or moves; lifting
/// every finger logs `touch.up`.
#[derive(Default)]
struct TouchLog {
    until: u64,
    next: u64,
    last: [platform::TouchPoint; 5],
    last_n: usize,
    reads: u32,
    errors: u32,
}

const TOUCH_POLL_MS: u64 = 16;

impl TouchLog {
    fn start(&mut self, seconds: u32) -> Result<u32, &'static str> {
        let mut probe = [platform::TouchPoint::default(); 5];
        Current::touch_read(&mut probe)?;
        let now = Current::now();
        *self = TouchLog { until: now + time::ms_to_ticks(seconds as u64 * 1000), next: now, ..TouchLog::default() };
        Ok(seconds)
    }

    fn poll(&mut self) {
        let now = Current::now();
        if self.until == 0 || now < self.next {
            return;
        }
        if now >= self.until {
            event!(Info, "touch.log_done", reads = self.reads, errors = self.errors);
            self.until = 0;
            return;
        }
        self.next = now + time::ms_to_ticks(TOUCH_POLL_MS);
        let mut pts = [platform::TouchPoint::default(); 5];
        self.reads += 1;
        let n = match Current::touch_read(&mut pts) {
            Ok(n) => n,
            Err(why) => {
                if self.errors == 0 {
                    event!(Warn, "touch.read_failed", error = why);
                }
                self.errors += 1;
                return;
            }
        };
        for p in &pts[..n] {
            if !self.last[..self.last_n].contains(p) {
                event!(Info, "touch.point", x = p.x, y = p.y, id = p.id, size = p.size, count = n as u32);
            }
        }
        if n == 0 && self.last_n > 0 {
            event!(Info, "touch.up");
        }
        self.last = pts;
        self.last_n = n;
    }
}

fn handle(s: &mut sched::Scheduler, store: &mut store::Store, kup: &mut kupdate::Kupdate, touch_log: &mut TouchLog, frame: &[u8]) {
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
            // CPU use since the last `ps` (doc/31), in per mille.
            let now = Current::now();
            let window = (now - s.cpu_mark.0).max(1);
            s.settle_idle(now);
            let pm = |t: u64| (t * 1000 / window) as u32;
            let mut procs = 0u64;
            let mut n = 0;
            for p in s.slots.iter_mut().flatten() {
                let used = p.cpu - p.cpu_mark;
                p.cpu_mark = p.cpu;
                procs += used;
                let state = match p.state {
                    process::State::Ready => "ready",
                    process::State::Sleeping { .. } => "sleeping",
                    process::State::Waiting { .. } => "waiting",
                    process::State::Exited(_) => "exited",
                };
                event!(
                    Info,
                    "proc.info",
                    pid = p.pid,
                    name = p.name,
                    state = state,
                    handles = p.handles.count() as u32,
                    cpu_ms = (time::ticks_to_ns(p.cpu) / 1_000_000) as u32,
                    cpu_pm = pm(used)
                );
                n += 1;
            }
            // Per core, and over all cores (doc/31, doc/33 §5): a process's
            // per mille is of one core; the totals are of all of them.
            let cores = if smp::two_cores() { 2 } else { 1 };
            let idle = [s.stats.idle_ticks[0] - s.cpu_mark.1[0], s.stats.idle_ticks[1] - s.cpu_mark.1[1]];
            s.cpu_mark = (now, s.stats.idle_ticks);
            let all = |t: u64| (t * 1000 / (window * cores)) as u32;
            let idle_all = all(idle[0] + idle[1]);
            let procs_all = all(procs);
            event!(
                Info,
                "kernel.cpu",
                window_ms = (time::ticks_to_ns(window) / 1_000_000) as u32,
                cores = cores as u32,
                idle_pm = idle_all,
                procs_pm = procs_all,
                kernel_pm = 1000u32.saturating_sub(idle_all + procs_all),
                idle0_pm = pm(idle[0]),
                idle1_pm = pm(idle[1]),
                slices0 = s.stats.slices[0],
                slices1 = s.stats.slices[1],
                spurious0 = s.stats.spurious[0],
                spurious1 = s.stats.spurious[1],
                kicks0 = s.stats.kicks[0],
                kicks1 = s.stats.kicks[1],
                ipis0 = s.stats.ipis[0],
                ipis1 = s.stats.ipis[1],
                lockwait0_ms = (smp::lock_wait_cycles()[0] / 360_000) as u32,
                lockwait1_ms = (smp::lock_wait_cycles()[1] / 360_000) as u32,
                idle_total_ms = (time::ticks_to_ns(s.stats.idle_ticks[0] + s.stats.idle_ticks[1]) / 1_000_000) as u32
            );
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
        Command::InstallBegin { length } => {
            kup.cancel();
            let mut busy = [(0u16, 0u16); sched::MAX_PROCS];
            let mut n = 0;
            for e in s.slots.iter().flatten().filter_map(|p| p.xip_extents) {
                busy[n] = e;
                n += 1;
            }
            store.install_begin(length, &busy[..n])
        }
        // Answered only on failure (doc/14 §3). Data and end go to the
        // session begun last: a kernel update or an app install.
        Command::InstallData { offset, data } => {
            let r = if kup.active() { kup.data(offset, data) } else { store.install_data(offset, data) };
            match r {
                Ok(()) => return,
                Err(e) => Err(e),
            }
        }
        Command::InstallEnd if kup.active() => kup.end(),
        Command::InstallEnd => store.install_end().map(|e| e.length),
        Command::KupdateBegin { length } => {
            store.install_cancel();
            kup.begin(length)
        }
        Command::Kinfo => {
            kup.log_state();
            Ok(0)
        }
        Command::Prof { op, a, b } => match op {
            t5_abi::ctl::prof::START => match s.slots.iter().flatten().find(|p| p.pid == a && p.exit().is_none()) {
                Some(p) => {
                    prof::start(a, b, p.region.text.start);
                    Ok(a)
                }
                None => Err("no such process"),
            },
            t5_abi::ctl::prof::STOP => {
                prof::stop();
                Ok(0)
            }
            t5_abi::ctl::prof::DUMP => {
                prof::dump(a);
                Ok(a)
            }
            _ => Err("bad prof operation"),
        },
        Command::Clock { mhz } => Current::set_cpu_mhz(mhz),
        Command::Pvt { on } => Current::set_pvt(on),
        Command::Touch { seconds } => touch_log.start(seconds),
        Command::I2cScan => Current::i2c_scan(),
        Command::Display => Current::display_stats(),
        Command::Rotate { degrees } => screen::set_orientation(degrees),
        Command::SdInfo => Current::sd_info(),
        Command::SdRead { lba } => Current::sd_dump(lba),
        Command::SdTest => Current::sd_test(),
        Command::WlanProbe => Current::wlan_probe(),
        Command::Power => {
            Current::log_power("cmd");
            Ok(0)
        }
        Command::Reboot => {
            event!(Info, "kernel.reboot");
            // Give the host a moment to read the log (doc/12: what it
            // misses comes back as log.prev on the next boot).
            let until = Current::now() + time::ns_to_ticks(200_000_000);
            while Current::now() < until {}
            Current::reset()
        }
        Command::Remove { name } => store.remove(name),
        Command::Rollback { name } => store.rollback(name, None, "command"),
        Command::FlashTest => store.flash_test(),
        // The full test overwrites the whole PSRAM, processes included.
        Command::Psram if s.slots.iter().any(Option::is_some) => Err("processes running"),
        Command::Psram => Current::psram_test(),
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
    let loaded = unsafe { loader::load(image, &slot_of(slot_index)) }.map_err(|e| {
        let mut why = t5_abi::log::FmtBuf::<64>::new();
        let _ = core::fmt::Write::write_fmt(&mut why, format_args!("{e:?}"));
        event!(Error, "loader.failed", name = name, error = why.as_str());
        "load failed"
    })?;
    let pid = next_pid();
    let mut p = Process::new(pid, loaded.name, &loaded, arg0 as usize, arg1 as usize);
    if loaded.xip {
        p.xip_extents = store.entry(name).map(|e| (e.extent_first, e.extent_count));
    }
    if let Some(e) = store.entry(name).filter(|e| e.state == t5_abi::store::State::PendingVerify) {
        p.pending = Some((Current::now(), e.extent_first));
    }
    s.slots[slot_index] = Some(p);
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

/// Memory processes are loaded into: `Current::APP_POOL` unless the
/// platform brought up PSRAM at boot.
static mut POOL: Option<core::ops::Range<usize>> = None;

fn set_pool(r: core::ops::Range<usize>) {
    // Single core, set once at boot before any process exists.
    unsafe { *core::ptr::addr_of_mut!(POOL) = Some(r) };
}

fn pool() -> core::ops::Range<usize> {
    unsafe { (*core::ptr::addr_of!(POOL)).clone() }.unwrap_or(Current::APP_POOL)
}

fn slot_of(index: usize) -> loader::Slot {
    let (base, capacity) = slot(index);
    loader::Slot { base, capacity }
}

/// Slot `i` of the app pool; each process gets one.
fn slot(i: usize) -> (usize, usize) {
    let pool = pool();
    // Whole 64 KiB pages where the pool is large enough (the PSRAM): an XIP
    // image's text is mapped over its slot (doc/30 §3).
    let share = (pool.end - pool.start) / sched::MAX_PROCS;
    let align = if share >= SLOT_ALIGN { SLOT_ALIGN } else { pmp::GRANULE };
    let size = share / align * align;
    (pool.start + i * size, size)
}

fn load(name: &'static str, image: &'static [u8], slot_index: usize, arg0: usize, arg1: usize) -> Option<Process> {
    match unsafe { loader::load(image, &slot_of(slot_index)) } {
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
        sched::run(&mut s, Some(1000), &mut |_| {});
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
        let result = unsafe { loader::load(image, &slot_of(0)) };
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

/// Two tickers sleep on different periods while three spinners compete for
/// the CPU with all FP registers in use (three, so that they still have to
/// share with two cores, doc/33 §5).
fn selftest_m2(apps: &Apps) -> u32 {
    event!(Info, "selftest.m2_start", processes = 5u32, slice_ms = sched::SLICE_MS);
    let specs: [(&'static str, &'static [u8], usize, usize); 5] = [
        ("ticker1", apps.ticker, 1, 30),
        ("ticker2", apps.ticker, 2, 45),
        ("spin1", apps.spin, 1, 100),
        ("spin2", apps.spin, 2, 100),
        ("spin3", apps.spin, 3, 100),
    ];
    let mut failures = 0;
    let mut slot_index = 0;
    let loaded = specs.map(|(name, image, arg0, arg1)| {
        let p = load(name, image, slot_index, arg0, arg1);
        slot_index += 1;
        p
    });
    let [Some(a), Some(b), Some(c), Some(d), Some(e)] = loaded else {
        return 1;
    };
    let mut s = sched::Scheduler::new();
    s.slots = [Some(a), Some(b), Some(c), Some(d), Some(e), None, None, None];

    let start = Current::now();
    sched::run(&mut s, Some(3000), &mut |_| {});
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
    s.slots = [Some(a), Some(b), Some(c), None, None, None, None, None];
    sched::run(&mut s, Some(2000), &mut |_| {});
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

/// Channels (doc/16 §5.4): the echo server and client run together with a
/// spin process; the client checks the channel rules and its replies, and
/// the server ends when the client's end closes.
fn selftest_m5c(apps: &Apps) -> u32 {
    let Some(echo) = apps.echo else {
        event!(Warn, "selftest.skipped", part = "m5c", reason = "no echo app in the store");
        return 0;
    };
    // Named channel (M5c), then a service with three clients at once (N0a,
    // doc/19): every client gets its own channel to the one server.
    // Then shared memory (N0b, doc/19 §2): two clients fill their slices,
    // a third writes past its slice and must be killed.
    let runs: [[(&'static str, &'static [u8], usize, usize); 4]; 3] = [
        [("echo-server", echo, 0, 0), ("echo-client", echo, 1, 40), ("spin", apps.spin, 4, 100), ("", echo, 0, 0)],
        [("echo-svc", echo, 2, 3), ("echo-c1", echo, 3, 20), ("echo-c2", echo, 3, 30), ("echo-c3", echo, 3, 40)],
        [("echo-shm", echo, 4, 3), ("shm-c1", echo, 5, 0), ("shm-c2", echo, 5, 0), ("shm-bad", echo, 6, 0)],
    ];
    // An echo installed before N0a has no service modes: skip that run.
    let permits = |name: &[u8]| {
        loader::verify(echo)
            .and_then(|signed| t5_abi::t5a::parse(signed, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR))
            .is_ok_and(|img| img.info.permits(name))
    };
    let has_svc = permits(b"listen:echo");
    let has_shm = permits(b"shm") && shm::available();
    let mut failures = 0;
    for (run, specs) in runs.into_iter().enumerate() {
        if run == 1 && !has_svc {
            event!(Warn, "selftest.skipped", part = "n0a", reason = "echo predates service channels");
            continue;
        }
        if run == 2 && !has_shm {
            event!(Warn, "selftest.skipped", part = "n0b", reason = "no shared memory, or echo predates it");
            continue;
        }
        let mut s = sched::Scheduler::new();
        for (i, &(name, image, arg0, arg1)) in specs.iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            match load(name, image, i, arg0, arg1) {
                Some(p) => s.slots[i] = Some(p),
                None => failures += 1,
            }
        }
        // QEMU's clock is the host's: leave room for a busy host.
        sched::run(&mut s, Some(5000), &mut |_| {});
        for p in s.slots.iter().flatten() {
            let exit = p.exit().unwrap();
            let ok = match p.name {
                "spin" => matches!(exit, Exit::Code(0) | Exit::Code(2)),
                "shm-bad" => matches(&exit, &fault(cause::STORE_ACCESS)),
                _ => exit == Exit::Code(0),
            };
            log_case(p, &exit, ok);
            if !ok {
                failures += 1;
            }
        }
    }
    failures
}

/// Display and touch (doc/17 §6): `paint` in self-test mode maps the
/// framebuffer, draws, waits for frames and reads the touch, checking the
/// rules on the way.
fn selftest_m6c(apps: &Apps) -> u32 {
    let Some(paint) = apps.paint else {
        event!(Warn, "selftest.skipped", part = "m6c", reason = "no paint app in the store");
        return 0;
    };
    let Some((fb, _)) = Current::display_buffer() else {
        event!(Warn, "selftest.skipped", part = "m6c", reason = "no display");
        return 0;
    };
    // `paint` maps the framebuffer; `fault`, which did not open the
    // display, must not be able to write it.
    let cases: [(&'static str, &'static [u8], usize, usize, Exit, u64); 2] = [
        ("paint", paint, 1, 0, Exit::Code(0), 3000),
        ("fault/store-framebuffer", apps.fault, MODE_STORE, fb, fault(cause::STORE_ACCESS), 1000),
    ];
    let mut failures = 0;
    for (name, image, arg0, arg1, expected, limit) in cases {
        let Some(p) = load(name, image, 0, arg0, arg1) else {
            failures += 1;
            continue;
        };
        let mut s = sched::Scheduler::new();
        s.slots = [Some(p), None, None, None, None, None, None, None];
        sched::run(&mut s, Some(limit), &mut |_| {});
        let p = s.slots[0].as_ref().unwrap();
        let exit = p.exit().unwrap();
        let ok = matches(&exit, &expected);
        log_case(p, &exit, ok);
        failures += !ok as u32;
    }
    failures
}

/// XIP apps (doc/15) run with their text in flash: `hello-xip` and
/// `xipcheck` (which checks its own addresses and sums a 128 KiB table it
/// reads in place) in slots 1 and 2, while `hello` runs copied in slot 0.
fn selftest_m4b(apps: &Apps) -> u32 {
    let (Some(hello_xip), Some(xipcheck)) = (apps.hello_xip, apps.xipcheck) else {
        event!(Warn, "selftest.skipped", part = "m4b", reason = "no XIP apps in the store");
        return 0;
    };
    let (base, capacity) = slot(0);
    if Current::map_xip(base, capacity, 0, 0).is_none() {
        event!(Warn, "selftest.skipped", part = "m4b", reason = "no XIP on this platform");
        return 0;
    }
    let specs: [(&'static str, &'static [u8]); 3] = [("hello", apps.hello), ("hello-xip", hello_xip), ("xipcheck", xipcheck)];
    let mut slot_index = 0;
    let loaded = specs.map(|(name, image)| {
        let p = load(name, image, slot_index, 0, 0);
        slot_index += 1;
        p
    });
    let [Some(a), Some(b), Some(c)] = loaded else {
        return 1;
    };
    let mut s = sched::Scheduler::new();
    s.slots = [Some(a), Some(b), Some(c), None, None, None, None, None];
    sched::run(&mut s, Some(2000), &mut |_| {});
    let mut failures = 0;
    for p in s.slots.iter().flatten() {
        let exit = p.exit().unwrap();
        let ok = exit == Exit::Code(0);
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
    if kupdate::pending() {
        // An unconfirmed update: restart so the bootloader rolls back.
        event!(Error, "kupdate.panic_reset");
        Current::reset();
    }
    Current::exit(0xFF)
}
