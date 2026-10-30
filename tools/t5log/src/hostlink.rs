//! `t5log hostlink` (N2a, doc/21): the PC end of the host link. The Tab5's
//! console line (USB Serial/JTAG, a CDC-ACM port) carries COBS frames: log
//! records, and IP packets whose first byte is `PACKET`. Packets go to and
//! come from a TUN device; log records are printed as `t5log decode` does.
//!
//! The TUN device is made once by root, owned by the user, e.g.
//! `sudo ip tuntap add dev t5 mode tun user $USER`,
//! `sudo ip addr add 10.55.0.1/24 dev t5`, `sudo ip link set t5 up`;
//! then `t5log hostlink --port /dev/ttyACM3 --tun t5` needs no root.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use t5_abi::log as wire;

/// First byte of a packet frame (kernel/src/hostlink.rs).
pub const PACKET: u8 = 0xFF;
const MTU: usize = 1500;

/// Opens the serial port in raw mode (no echo, no line editing, 8 bits).
pub fn open_serial(path: &str) -> Result<File, String> {
    let f = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(f.as_raw_fd(), &mut t) != 0 {
            return Err(format!("{path}: tcgetattr: {}", std::io::Error::last_os_error()));
        }
        libc::cfmakeraw(&mut t);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(f.as_raw_fd(), libc::TCSANOW, &t) != 0 {
            return Err(format!("{path}: tcsetattr: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(f)
}

/// Attaches to the TUN device `name` (made beforehand, see above).
pub fn open_tun(name: &str) -> Result<File, String> {
    const TUNSETIFF: libc::c_ulong = 0x4004_54CA;
    const IFF_TUN: libc::c_short = 0x0001;
    const IFF_NO_PI: libc::c_short = 0x1000;
    let f = OpenOptions::new().read(true).write(true).open("/dev/net/tun").map_err(|e| format!("/dev/net/tun: {e}"))?;
    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        flags: libc::c_short,
        pad: [u8; 22],
    }
    let mut req = IfReq { name: [0; 16], flags: IFF_TUN | IFF_NO_PI, pad: [0; 22] };
    if name.len() >= 16 {
        return Err("TUN name too long".into());
    }
    req.name[..name.len()].copy_from_slice(name.as_bytes());
    let r = unsafe { libc::ioctl(f.as_raw_fd(), TUNSETIFF, &mut req) };
    if r != 0 {
        return Err(format!(
            "TUN {name}: {} (make it first: sudo ip tuntap add dev {name} mode tun user $USER)",
            std::io::Error::last_os_error()
        ));
    }
    Ok(f)
}

pub struct Counters {
    pub to_tab5: AtomicU64,
    pub from_tab5: AtomicU64,
}

/// Packets from the TUN device to the Tab5, in a thread.
pub fn spawn_uplink(mut tun: File, mut serial: File, c: Arc<Counters>) {
    std::thread::spawn(move || {
        let mut pkt = [0u8; 65536];
        let mut plain = [0u8; MTU + 1];
        let mut frame = [0u8; MTU + 16];
        loop {
            let n = match tun.read(&mut pkt) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("t5log: TUN read: {e}");
                    return;
                }
            };
            if n == 0 || n > MTU {
                continue;
            }
            plain[0] = PACKET;
            plain[1..=n].copy_from_slice(&pkt[..n]);
            let Some(k) = wire::cobs_encode(&plain[..=n], &mut frame) else { continue };
            if serial.write_all(&frame[..k]).is_err() {
                eprintln!("t5log: serial write failed");
                return;
            }
            c.to_tab5.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Splits the serial stream into frames: packets go to `tun`, other frames
/// (log records) are passed on to `log` whole, with their terminator.
pub struct Demux {
    frame: Vec<u8>,
}

impl Demux {
    pub fn new() -> Demux {
        Demux { frame: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8], tun: &mut File, c: &Counters, log: &mut Vec<u8>) {
        for &b in bytes {
            if b != 0 {
                self.frame.push(b);
                continue;
            }
            let mut decoded = [0u8; MTU + 8];
            let is_packet = matches!(wire::cobs_decode(&self.frame, &mut decoded), Some(n) if n >= 2 && decoded[0] == PACKET);
            if is_packet {
                let n = wire::cobs_decode(&self.frame, &mut decoded).unwrap();
                if tun.write_all(&decoded[1..n]).is_ok() {
                    c.from_tab5.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                log.extend_from_slice(&self.frame);
                log.push(0);
            }
            self.frame.clear();
        }
    }
}
