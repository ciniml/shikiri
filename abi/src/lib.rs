//! Definitions shared between the kernel, the user runtime (`t5-rt`) and host
//! tools. See doc/07-architecture-proposal.md §4 for the syscall ABI and
//! doc/04-dynamic-loading-and-abi.md and doc/14-m3-design.md for the T5A image format.

#![no_std]

pub mod ctl;
pub mod fs;
pub mod log;
pub mod mem;
pub mod net;
pub mod otadata;
pub mod store;
pub mod t5a;
pub mod t5k;

/// ABI version carried by every app image. The kernel accepts an image when
/// the major numbers match and the image's minor is not newer than its own.
pub const ABI_MAJOR: u16 = 0;
/// Minor 2 (M2): yield, sleep, time. Minor 3 (L1, doc/12): log_emit and
/// log_level replace dbg_write. Minor 4 (M3a, doc/14): signed T5A v1 images
/// (pre-1.0, so no compatibility is kept). Minor 5 (M3d, doc/14 §4): handles,
/// open/ctl/wait/close and the timer object. Minor 6 (M5c, doc/16 §5):
/// read/write and the channel object. Minor 7 (M6c, doc/17 §6): the
/// display and touch objects. Minor 8 (N0a, doc/19): service channels
/// (`listen:`/`svc:`). Minor 9 (N0b, doc/19 §2): shared memory. Minor 10
/// (M7c, doc/18 §3): the `sd` object. Minor 11 (N2a, doc/21): `netdev`.
/// Minor 12 (N2b-3, doc/22): `netdev:wlan0` and `wlan:ctl`. Minor 13
/// (doc/35 §5): `netdev`'s frame rings (`MAP_RING`, `KICK`). Minor 14
/// (doc/38): `proc:spawn`. Minor 15 (doc/39): `kbd`. Minor 16 (doc/40):
/// `proc:list`, `proc:apps`, `proc:kill`. Minor 17 (doc/44): the display's
/// 2D requests (`write`, `display::DONE`). Minor 18 (doc/46): `random`.
/// Minor 19 (doc/48): `bt:hci`.
pub const ABI_MINOR: u16 = 19;

/// Syscall numbers (passed in a7). Numbers are only ever added, never reused.
pub mod sys {
    /// a0 = exit code. Does not return.
    pub const EXIT: usize = 0x000;
    /// Gives up the rest of the time slice.
    pub const YIELD: usize = 0x001;
    /// a0/a1 = nanoseconds (lo/hi). Sleeps at least that long.
    pub const SLEEP: usize = 0x002;
    pub const SYS_INFO: usize = 0x003;
    /// Returns monotonic nanoseconds since boot in a1/a2 (lo/hi).
    pub const TIME: usize = 0x004;
    /// a0/a1 = payload (see `log`), at most `log::MAX_PAYLOAD` bytes. The
    /// kernel adds the sequence number, timestamp and source.
    pub const LOG_EMIT: usize = 0x005;
    /// Returns the lowest level (a1) the kernel currently keeps, so apps can
    /// skip building events that would be dropped.
    pub const LOG_LEVEL: usize = 0x006;
    /// a0 = handle. Frees the handle.
    pub const CLOSE: usize = 0x080;
    /// a0/a1 = array of `u32` handles and its length (at most
    /// `WAIT_MAX`), a2/a3 = timeout in ns (lo/hi; `FOREVER` = none, 0 =
    /// just poll). Returns the index of the handle in the array (a1), the
    /// event (a2) and its value (a3) of the first handle that has one;
    /// `Error::TimedOut` if none came in time.
    pub const WAIT: usize = 0x082;
    /// a0/a1 = object name (UTF-8), a2 = rights (`rights`). Returns the
    /// handle in a1. Only names in the image's Permissions TLV can be
    /// opened (`Error::Denied` otherwise).
    pub const OPEN: usize = 0x100;
    /// a0 = handle, a1/a2 = buffer. Returns the bytes read in a1
    /// (channels: one whole message), and in a2 how many frames are still
    /// queued (devices that queue frames: `netdev`, `wlan`; 0 otherwise).
    pub const READ: usize = 0x101;
    /// a0 = handle, a1/a2 = buffer. Returns the bytes written in a1
    /// (channels: one whole message).
    pub const WRITE: usize = 0x102;
    /// a0 = handle, a1 = operation (per object), a2/a3 = arguments.
    /// Returns a value in a1.
    pub const CTL: usize = 0x103;
}

/// Handles per process.
pub const MAX_HANDLES: usize = 16;
/// Handles one `wait` can watch.
pub const WAIT_MAX: usize = MAX_HANDLES;
/// `wait` timeout meaning "no timeout".
pub const FOREVER: u64 = u64::MAX;

/// Rights asked for in `open`, checked on each use of the handle.
pub mod rights {
    pub const READ: usize = 1 << 0;
    pub const WRITE: usize = 1 << 1;
    pub const CTL: usize = 1 << 2;
    pub const WAIT: usize = 1 << 3;
}

/// The `timer` object: a periodic or one-shot timer whose expiries `wait`
/// reports. Expiries that come before the app waits are added up, not
/// queued: the event's value is how many there were.
pub mod timer {
    pub const NAME: &str = "timer";
    /// ctl: a2/a3 = period in ns (lo/hi). The first expiry is one period
    /// from now.
    pub const SET_PERIODIC: usize = 1;
    /// ctl: a2/a3 = delay in ns (lo/hi).
    pub const SET_ONESHOT: usize = 2;
    /// ctl: stops the timer and drops expiries not yet waited for.
    pub const STOP: usize = 3;
    /// Shortest period or delay the kernel accepts (ns).
    pub const MIN_NS: u64 = 100_000;
    /// `wait` event: the timer expired (value = how many times).
    pub const EXPIRED: usize = 1;
}
/// Channels (doc/16 §5): `open("chan:NAME")` joins one of the two ends of
/// the channel NAME; a message `write` on one end is `read` whole on the
/// other. The image's Permissions TLV must name `chan:NAME`.
pub mod chan {
    pub const PREFIX: &str = "chan:";
    /// Longest message (`write` refuses more with `Error::Size`).
    pub const MSG_MAX: usize = 256;
    /// Messages each end holds before `write` gets `Error::Busy`.
    pub const QUEUE: usize = 4;
    /// `wait` event: messages are queued (value = how many).
    pub const READABLE: usize = 1;
    /// `wait` event: nothing is queued and the other end was closed.
    pub const PEER_CLOSED: usize = 2;
    /// `wait` event: the other end granted shared memory; take it with
    /// `ctl(TAKE_SHM)`.
    pub const SHM: usize = 3;
    /// ctl: returns a new `shm` handle for the granted memory.
    pub const TAKE_SHM: usize = 1;
}

/// Shared memory (doc/19 §2): regions of PSRAM in 64 KiB units. One
/// `open("shm")` handle holds one window; a process maps at most one at a
/// time (besides the framebuffer).
pub mod shm {
    pub const NAME: &str = "shm";
    /// ctl: a2 = bytes; gives the (empty) handle a new zeroed region.
    pub const ALLOC: usize = 1;
    /// ctl: maps the window read/write into the caller; returns its address.
    /// Two windows can be mapped at once (`Busy` for a third; doc/49 §3).
    pub const MAP: usize = 2;
    /// ctl: returns the window's length in bytes.
    pub const SIZE: usize = 3;
    /// ctl: a2/a3 = channel handle | (offset / GRAIN) << 8 | (length / GRAIN)
    /// << 32: grants that part of the window to the channel's other end.
    pub const GRANT: usize = 4;
    /// Offsets and lengths of grants are multiples of this (PMP granule).
    pub const GRAIN: usize = 128;

    /// The `GRANT` argument.
    pub const fn grant_arg(chan: u32, offset: usize, len: usize) -> u64 {
        chan as u64 | ((offset / GRAIN) as u64) << 8 | ((len / GRAIN) as u64) << 32
    }
}

/// A network link for the network service (doc/21): `read` takes one
/// received frame, `write` sends one. `netdev:host` carries IP packets over
/// the console line to the PC; `netdev:wlan0` carries Ethernet frames of the
/// Wi-Fi station (doc/22).
pub mod netdev {
    pub const HOST: &str = "netdev:host";
    pub const WLAN0: &str = "netdev:wlan0";
    pub const MTU: usize = 1500;
    /// An Ethernet frame without the FCS.
    pub const ETH_FRAME: usize = MTU + 14;
    /// `wait` event: frames are waiting (value = how many).
    pub const READABLE: usize = 1;
    /// `ctl` on `netdev:wlan0`: the station's MAC address, bytes 0-3 and
    /// 4-5 (little-endian; `Busy` until the Wi-Fi service has set it).
    pub const MAC_LO: usize = 1;
    pub const MAC_HI: usize = 2;
    /// `ctl`: maps the device's frame rings (`ring`) into the caller and
    /// returns their address; `NoSupport` where the device has none,
    /// `NoMem` while another process has them. `read`/`write` go on
    /// working beside them. Closing the handle unmaps them.
    pub const MAP_RING: usize = 3;
    /// `ctl`: frames were added to the transmit ring: send them.
    pub const KICK: usize = 4;

    /// Frame rings shared by the kernel and the process that has the
    /// device (doc/35 §5): frames pass without `read`/`write`. Each ring
    /// has `SLOTS` fixed slots and two free-running counters, one written
    /// by each side; the slot of count `c` is `c % SLOTS`. Receive: the
    /// kernel fills slots and advances `RX_HEAD`, the process takes them
    /// and advances `RX_TAIL`. Transmit: the other way round, with
    /// `TX_HEAD` (process) and `TX_TAIL` (kernel). A slot's length is set
    /// before its counter moves. The kernel keeps its own counters in its
    /// own memory and checks the process's, so a process that writes
    /// nonsense here only loses its frames.
    pub mod ring {
        use core::sync::atomic::{AtomicU32, Ordering};

        pub const SLOTS: u32 = 16;
        /// Room for one frame (`ETH_FRAME` rounded up).
        pub const SLOT: usize = 1536;
        pub const RX_HEAD: usize = 0;
        pub const RX_TAIL: usize = 4;
        pub const TX_HEAD: usize = 8;
        pub const TX_TAIL: usize = 12;
        /// `[u32; SLOTS]`: the length of the frame in each slot.
        pub const RX_LEN: usize = 64;
        pub const TX_LEN: usize = RX_LEN + 4 * SLOTS as usize;
        pub const RX_DATA: usize = 256;
        pub const TX_DATA: usize = RX_DATA + SLOTS as usize * SLOT;
        /// The whole area (a multiple of the PMP's 128 bytes).
        pub const SIZE: usize = TX_DATA + SLOTS as usize * SLOT;

        /// The rings at `base`, as the process sees them.
        #[derive(Clone, Copy)]
        pub struct Rings(pub usize);

        impl Rings {
            /// The counter or length word at `off`.
            pub fn word(&self, off: usize) -> &AtomicU32 {
                unsafe { &*((self.0 + off) as *const AtomicU32) }
            }

            /// Slot `i` (mod `SLOTS`) of the ring whose data is at `data`.
            pub fn slot(&self, data: usize, i: u32) -> *mut u8 {
                (self.0 + data + (i % SLOTS) as usize * SLOT) as *mut u8
            }

            /// The oldest received frame, until `rx_pop`.
            pub fn rx_peek(&self) -> Option<&'static mut [u8]> {
                let tail = self.word(RX_TAIL).load(Ordering::Relaxed);
                if self.word(RX_HEAD).load(Ordering::Acquire) == tail {
                    return None;
                }
                let len = (self.word(RX_LEN + (tail % SLOTS) as usize * 4).load(Ordering::Relaxed) as usize).min(SLOT);
                Some(unsafe { core::slice::from_raw_parts_mut(self.slot(RX_DATA, tail), len) })
            }

            /// Gives the oldest received frame's slot back.
            pub fn rx_pop(&self) {
                let tail = self.word(RX_TAIL).load(Ordering::Relaxed);
                self.word(RX_TAIL).store(tail.wrapping_add(1), Ordering::Release);
            }

            /// A free transmit slot (whole), if any; `tx_push` sends it.
            pub fn tx_slot(&self) -> Option<&'static mut [u8; SLOT]> {
                let head = self.word(TX_HEAD).load(Ordering::Relaxed);
                if head.wrapping_sub(self.word(TX_TAIL).load(Ordering::Acquire)) >= SLOTS {
                    return None;
                }
                Some(unsafe { &mut *(self.slot(TX_DATA, head) as *mut [u8; SLOT]) })
            }

            /// Queues the slot `tx_slot` gave with `len` bytes of frame.
            pub fn tx_push(&self, len: usize) {
                let head = self.word(TX_HEAD).load(Ordering::Relaxed);
                self.word(TX_LEN + (head % SLOTS) as usize * 4).store(len as u32, Ordering::Relaxed);
                self.word(TX_HEAD).store(head.wrapping_add(1), Ordering::Release);
            }
        }
    }
}

/// The Wi-Fi chip's control path (doc/22), for the Wi-Fi service: `read`
/// takes one message from the chip, `write` sends one. A message is
/// esp_hosted's serial payload (endpoint and data TLVs around an RPC). The
/// first `open` starts the chip (about 2 s).
pub mod wlan {
    pub const CTL: &str = "wlan:ctl";
    /// Longest message.
    pub const MSG_MAX: usize = 1536;
    /// `ctl`: the station's MAC address (`arg` = its 6 bytes, little-endian),
    /// for `netdev:wlan0`'s `MAC_LO`/`MAC_HI`.
    pub const SET_MAC: usize = 1;
}

/// Bluetooth (doc/48): `bt:hci` carries HCI packets to and from the
/// controller on the Wi-Fi chip (the ESP32-C6), once the Wi-Fi service has
/// started it. `read` and `write` move one packet in the UART (H4) form:
/// the packet type (`CMD`, `ACL`, `SCO`, `EVT`, `ISO`), then the packet.
/// One handle at a time; `NoDevice` before the chip runs.
pub mod bt {
    pub const HCI: &str = "bt:hci";
    pub const CMD: u8 = 1;
    pub const ACL: u8 = 2;
    pub const SCO: u8 = 3;
    pub const EVT: u8 = 4;
    pub const ISO: u8 = 5;
    /// Longest packet with its type byte: an event (2 + 255 bytes) or LE
    /// ACL data of 251 bytes (4 + 251).
    pub const PKT_MAX: usize = 1 + 2 + 255;
}

/// The BLE service (doc/48 M2): clients connect to `svc:ble` and send
/// requests as channel messages; the service answers with events on the
/// same channel. One scan at a time.
pub mod ble {
    pub const SVC: &str = "ble";
    /// Request `[SCAN, seconds, active, interval_ms: u16 LE, window_ms:
    /// u16 LE]`: scan for `seconds` (0: until `STOP` or the channel
    /// closes), active (1) or passive (0). The window must not be longer
    /// than the interval; Wi-Fi shares the radio (doc/48 §5).
    pub const SCAN: u8 = 1;
    /// Request `[STOP]`: ends the scan.
    pub const STOP: u8 = 2;
    /// Event `[ADV, addr_type, addr[6] (most significant first), kind,
    /// rssi: i8, data_len, data…]`: one advertising report.
    pub const ADV: u8 = 1;
    /// Event `[DONE, reports: u32 LE, dropped: u32 LE]`: the scan ended.
    pub const DONE: u8 = 2;
    /// Event `[FAILED, request, code]`: the request was refused or failed
    /// (`code`: 1 bad request, 2 not now (busy or not connected), 3 the
    /// stack failed, 4 timed out, 5 not found).
    pub const FAILED: u8 = 3;

    // ---- the central role (doc/48 M3): one connection at a time. ----
    /// Request `[CONNECT, addr_type, addr[6] (most significant first),
    /// timeout_s]`: connect to an advertiser (it must be advertising).
    pub const CONNECT: u8 = 3;
    /// Request `[DISCONNECT]`.
    pub const DISCONNECT: u8 = 4;
    /// Request `[SERVICES]`: discover the primary services; `SERVICE`
    /// events, then `END`.
    pub const SERVICES: u8 = 5;
    /// Request `[CHARS, service]` (its index in the `SERVICE` events):
    /// `CHAR` events, then `END`.
    pub const CHARS: u8 = 6;
    /// Request `[READ, handle: u16 LE]`: `VALUE`.
    pub const READ: u8 = 7;
    /// Request `[WRITE, handle: u16 LE, with_response, data…]`: `WRITTEN`.
    pub const WRITE: u8 = 8;
    /// Request `[SUBSCRIBE, handle: u16 LE, indicate]` (a characteristic
    /// from `CHARS`): `SUBSCRIBED`, then `NOTIFY` events. One at a time.
    pub const SUBSCRIBE: u8 = 9;

    /// Event `[CONNECTED, att_mtu: u16 LE]`.
    pub const CONNECTED: u8 = 4;
    /// Event `[DISCONNECTED]`: the link is gone.
    pub const DISCONNECTED: u8 = 5;
    /// Event `[SERVICE, index, start: u16 LE, end: u16 LE, uuid…]` (2 or 16
    /// bytes, little-endian).
    pub const SERVICE: u8 = 6;
    /// Event `[CHAR, handle: u16 LE, cccd: u16 LE (0: none), properties,
    /// uuid…]`.
    pub const CHAR: u8 = 7;
    /// Event `[END]`: the end of a list.
    pub const END: u8 = 8;
    /// Event `[VALUE, handle: u16 LE, data…]`.
    pub const VALUE: u8 = 9;
    /// Event `[WRITTEN, handle: u16 LE]`.
    pub const WRITTEN: u8 = 10;
    /// Event `[SUBSCRIBED, handle: u16 LE]`.
    pub const SUBSCRIBED: u8 = 11;
    /// Event `[NOTIFY, handle: u16 LE, indication, data…]`.
    pub const NOTIFY: u8 = 12;

    // ---- the peripheral role (doc/48 M4): one GATT service of the
    // client's, advertised until one central connects. ----
    /// Request `[ADVERTISE, interval_ms: u16 LE, name_len, name…,
    /// service_uuid_len (2 or 16), service_uuid… (LE), chars, then per
    /// characteristic: uuid_len, uuid… (LE), properties; adv_len, adv…,
    /// scan_len, scan…]` (the advertising and scan response data as sent,
    /// at most 31 bytes each; the name is the GAP Device Name, at most 22
    /// bytes). `ADVERTISING`, then `PEER` once a central connects, then
    /// `PEER_WRITE`s; `DISCONNECTED` when it leaves. `STOP` ends it.
    pub const ADVERTISE: u8 = 10;
    /// Request `[SET, handle: u16 LE, data…]`: the value a peer reads.
    /// `WRITTEN`.
    pub const SET: u8 = 11;
    /// Request `[SEND, handle: u16 LE, indicate, data…]`: a notification
    /// (or an indication, answered by the peer) to the connected peer, if
    /// it has subscribed. `SENT`.
    pub const SEND: u8 = 12;

    /// Event `[ADVERTISING, chars, then per characteristic: handle: u16 LE,
    /// cccd: u16 LE (0: none)]`, in the order of the request.
    pub const ADVERTISING: u8 = 13;
    /// Event `[PEER, addr_type, addr[6] (most significant first)]`: a
    /// central connected.
    pub const PEER: u8 = 14;
    /// Event `[PEER_WRITE, handle: u16 LE, data…]`: the peer wrote a value
    /// (or a CCCD: subscribing).
    pub const PEER_WRITE: u8 = 15;
    /// Event `[SENT, handle: u16 LE]`.
    pub const SENT: u8 = 16;
    /// Characteristics in an `ADVERTISE`, at most.
    pub const CHARS_MAX: usize = 4;
    /// Longest characteristic value.
    pub const VALUE_MAX: usize = 244;

    /// Longest message (a channel message).
    pub const MSG_MAX: usize = 256;
}

/// Starting apps (doc/38): `write` of `[arg0: u32 LE][arg1: u32 LE][name]`
/// asks the kernel to start the store app `name` with those arguments. It
/// returns once the request is queued (`Busy`: the queue is full); the
/// kernel starts the app on its next service round and logs
/// `proc.spawned` or `proc.spawn_failed`. The app gets its own image's
/// permissions, nothing of the caller's.
pub mod spawn {
    pub const NAME: &str = "proc:spawn";
    /// The arguments before the name.
    pub const ARGS: usize = 8;
    /// Longest name (the store's).
    pub const NAME_MAX: usize = 32;
}

/// The processes, the store's apps, and stopping a process (doc/40).
/// Each needs its name in the Permissions TLV.
///
/// `proc:list` and `proc:apps` are read as whole records, as many as fit,
/// from a snapshot the kernel takes on its service round after the
/// handle was opened; until then `read` returns `Busy`. Each `read` gives
/// the snapshot from its start (open again for a newer one).
///
/// `proc:kill`: writing a `u32` pid (LE) queues the kill; the kernel does
/// it on its next service round and logs `proc.killed` or
/// `proc.kill_failed`.
pub mod proc {
    pub const LIST: &str = "proc:list";
    pub const APPS: &str = "proc:apps";
    pub const KILL: &str = "proc:kill";

    /// A `proc:list` record: `u32 pid | u8 state | u8 handles | u16 0 |
    /// u32 cpu_ms | u32 ram (bytes: data, bss and stack) | name[16]`
    /// (NUL-padded, cut if longer).
    pub const PROC_RECORD: usize = 32;
    pub const STATE_READY: u8 = 0;
    pub const STATE_SLEEPING: u8 = 1;
    pub const STATE_WAITING: u8 = 2;
    pub const STATE_EXITED: u8 = 3;

    /// A `proc:apps` record: `name[32] | u32 length | u32 version[3] | u8
    /// state (0 current, 1 previous, 2 unconfirmed) | u8 flags (bit 0
    /// autostart) | u16 0`.
    pub const APP_RECORD: usize = 52;
    pub const APP_CURRENT: u8 = 0;
    pub const APP_PREVIOUS: u8 = 1;
    pub const APP_UNCONFIRMED: u8 = 2;
    pub const APP_AUTOSTART: u8 = 1;

    /// Processes and apps a snapshot holds at most.
    pub const MAX_PROCS: usize = 16;
    pub const MAX_APPS: usize = 32;
}

/// The SD card as sectors (doc/18 §3), for the file service: one opener at
/// a time. `read`/`write` move whole sectors (at most `MAX_IO` bytes) at
/// the position and advance it.
pub mod sd {
    pub const NAME: &str = "sd";
    pub const SECTOR: usize = 512;
    pub const MAX_IO: usize = 4096;
    /// ctl: returns the number of sectors.
    pub const SECTORS: usize = 1;
    /// ctl: a2 = sector number: the position of the next read or write.
    pub const SEEK: usize = 2;
}

/// Service channels (doc/19): many clients, one server. The server opens
/// `listen:NAME` (a listener); each client that opens `svc:NAME` gets a new
/// channel of its own (as `chan:`), whose other end the server takes with
/// `ctl(ACCEPT)`. Both names must be in the image's Permissions TLV.
pub mod svc {
    pub const LISTEN_PREFIX: &str = "listen:";
    pub const CONNECT_PREFIX: &str = "svc:";
    /// ctl on a listener: returns the handle of the oldest waiting
    /// connection (`Error::Busy` if none).
    pub const ACCEPT: usize = 1;
    /// `wait` event on a listener: connections are waiting (value = how many).
    pub const PENDING: usize = 1;
    /// Connections a listener holds before `open("svc:…")` gets `Busy`.
    pub const BACKLOG: usize = 4;
}

/// The display (doc/17 §6): one process at a time gets the framebuffer
/// mapped into its memory. The framebuffer is the panel's: 720 x 1280
/// RGB565, rows of 720 pixels, top row first as the panel scans it.
/// `ORIENTATION` says how the Tab5 is held; apps rotate what they draw.
pub mod display {
    pub const NAME: &str = "display";
    /// ctl: returns width | height << 16 of the framebuffer (pixels).
    pub const INFO: usize = 1;
    /// ctl: maps the framebuffer into the caller (read/write); returns its
    /// address. Stays mapped until the handle is closed.
    pub const MAP: usize = 2;
    /// ctl: a2 = first row | (end row << 16): makes those rows visible
    /// (writes them back from the cache).
    pub const FLUSH: usize = 3;
    /// ctl: returns the orientation in quarter turns clockwise (0-3): how
    /// far the panel's top is turned from the top the user sees.
    pub const ORIENTATION: usize = 4;
    /// `wait` event: frames were sent to the panel (value = how many).
    pub const FRAME: usize = 1;
    /// `wait` event: 2D requests finished (value = how many; doc/44). One
    /// runs at a time: `write` returns `Busy` until it has finished.
    pub const DONE: usize = 2;
    /// `wait` event: 2D requests failed (timed out, DMA error), in place of
    /// their `DONE` (value = how many). The rows may be partly written.
    pub const FAILED: usize = 3;

    /// 2D requests, written to the display handle (doc/44): little-endian
    /// u16s, in framebuffer pixels (the panel's, not the orientation's).
    /// The hardware does them while the caller runs; it must not touch the
    /// rows involved until `DONE`. The kernel writes those rows back from
    /// the cache first (and drops the destination's).
    pub mod op {
        /// `FILL, x, y, w, h, rgb565`
        pub const FILL: u16 = 1;
        /// `COPY, src_x, src_y, dst_x, dst_y, w, h`. Any overlap works; a
        /// copy down or to the right over itself is done in pieces no
        /// larger than the shift, and refused (`Invalid`) if that would
        /// take more than 32 pieces.
        pub const COPY: u16 = 2;
    }
}

/// Random bytes (doc/46 §4): `read` fills up to `MAX` bytes. On the
/// ESP32-P4 from the RNG register with the SAR ADC as its entropy source
/// (doc/47; the kernel logs `rng.entropy` when it started, else
/// `rng.no_entropy`). On virt boards a timer mixer, not random.
pub mod random {
    pub const NAME: &str = "random";
    pub const MAX: usize = 64;
}

/// Glyphs beyond ASCII (doc/45): the service `svc:font` grants each client
/// the whole glyph file as shared memory (read it, do not write it).
pub mod font {
    pub const SVC: &str = "font";
    /// The file: a header of little-endian u32s (`MAGIC`, then the narrow
    /// glyphs' count, codes offset and bitmaps offset, then the same for
    /// the wide ones); codes are sorted code points (u32).
    pub const MAGIC: [u8; 4] = *b"T5F1";
    pub const HEADER: usize = 28;
    pub const HEIGHT: usize = 32;
    /// A narrow glyph: 32 rows of u16, bit 15 the leftmost of 16 pixels.
    pub const NARROW_BYTES: usize = 64;
    /// A wide glyph: 32 rows of u32, bit 31 the leftmost of 32 pixels.
    pub const WIDE_BYTES: usize = 128;
}

/// Touch input (doc/17 §6). Points are in the orientation's coordinates
/// (`display::ORIENTATION`): x to the right, y down as the user sees it.
pub mod touch {
    pub const NAME: &str = "touch";
    /// Points `read` returns at most.
    pub const MAX_POINTS: usize = 5;
    /// Bytes per point: u16 x, y, id, size (little-endian).
    pub const POINT_SIZE: usize = 8;
    /// `wait` event: the touch changed (value = points now, 0 = released).
    pub const CHANGED: usize = 1;
}

/// The keyboard (doc/39): key events as the USB HID keyboard page has
/// them. One process at a time. Opening works without a keyboard; events
/// come once one is attached.
pub mod kbd {
    pub const NAME: &str = "kbd";
    /// Bytes per event: the modifier byte (bit 0 left Ctrl, 1 left Shift,
    /// 2 left Alt, 3 left GUI, 4-7 the right ones), then the usage (0 = all
    /// keys released).
    pub const EVENT_SIZE: usize = 2;
    /// Events the kernel keeps; older ones are dropped.
    pub const QUEUE: usize = 32;
    /// `wait` event: events wait (value = how many).
    pub const READABLE: usize = 1;
}

/// Error codes. A syscall returns `-(code as isize)` in a0 on failure.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Fail = 1,
    Busy = 2,
    Already = 3,
    Off = 4,
    Reserve = 5,
    Invalid = 6,
    Size = 7,
    Cancel = 8,
    NoMem = 9,
    NoSupport = 10,
    NoDevice = 11,
    Uninstalled = 12,
    NoAck = 13,
    /// A user pointer was outside the calling process's memory.
    BadAddress = 14,
    /// The image's Permissions TLV does not allow this (`open`).
    Denied = 15,
    /// `wait` saw no event before its timeout.
    TimedOut = 16,
}

impl Error {
    pub fn from_code(code: usize) -> Error {
        match code {
            2 => Error::Busy,
            3 => Error::Already,
            4 => Error::Off,
            5 => Error::Reserve,
            6 => Error::Invalid,
            7 => Error::Size,
            8 => Error::Cancel,
            9 => Error::NoMem,
            10 => Error::NoSupport,
            11 => Error::NoDevice,
            12 => Error::Uninstalled,
            13 => Error::NoAck,
            14 => Error::BadAddress,
            15 => Error::Denied,
            16 => Error::TimedOut,
            _ => Error::Fail,
        }
    }
}

/// Exit code a process reports when it panics.
pub const EXIT_PANIC: usize = 0xDEAD;
