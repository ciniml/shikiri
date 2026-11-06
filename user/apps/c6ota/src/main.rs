#![no_std]
#![no_main]

//! Puts a new esp_hosted firmware on the Wi-Fi chip (doc/58 R5): `run
//! c6ota IP PORT` fetches `http://IP:PORT/c6.bin` (IP an IPv4 address as a
//! number) and hands it to `wifi` (`svc:wifi/ota`), which writes it with
//! esp_hosted's OTA. The chip boots it about 5 s after the end; restart the
//! board then.
//!
//! The image is checked before anything is sent: an ESP image for the C6
//! (magic 0xE9, chip id 13), no larger than the chip's OTA partition.
//!
//! Built with `--permit svc:net/tcp-connect --permit svc:wifi/ota`. Exit 0,
//! or the step that failed.

use core::net::{IpAddr, Ipv4Addr};

use t5_rt::{Channel, Net, chan, event, sleep_ms, time_ns, wifi};

t5_rt::entry!(main);

fn main(ip: usize, port: usize) -> usize {
    match run(ip as u32, port as u16) {
        Ok(bytes) => {
            event!(Warn, "c6ota.done", bytes = bytes as u32);
            0
        }
        Err(step) => {
            event!(Error, "c6ota.failed", step = step);
            step as usize
        }
    }
}

fn run(ip: u32, port: u16) -> Result<usize, u32> {
    let mut net = Net::connect().map_err(|_| 1u32)?;
    let s = net.tcp_connect_ip(IpAddr::V4(Ipv4Addr::from_bits(ip)), port).map_err(|_| 2u32)?;
    net.wait_established(s, 10_000).map_err(|_| 3u32)?;
    net.send_all(s, b"GET /c6.bin HTTP/1.0\r\nConnection: close\r\n\r\n").map_err(|_| 4u32)?;
    let t = time_ns();

    // The header: status 200 and Content-Length.
    let mut head = [0u8; 1024];
    let mut have = 0;
    let end = loop {
        let n = net.recv(s, &mut head[have..]).map_err(|_| 5u32)?;
        if n == 0 {
            return Err(5);
        }
        have += n;
        if let Some(i) = head[..have].windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if have == head.len() {
            return Err(5);
        }
    };
    let text = core::str::from_utf8(&head[..end]).map_err(|_| 6u32)?;
    if !text.starts_with("HTTP/1.0 200") && !text.starts_with("HTTP/1.1 200") {
        return Err(6);
    }
    let size = text
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse::<usize>().ok()))
        .ok_or(7u32)?;
    // The first bytes say whose image it is.
    let first = &head[end..have];
    if size > wifi::OTA_MAX || first.len() < 16 || first[0] != 0xE9 || u16::from_le_bytes([first[12], first[13]]) != 0x0D {
        event!(Error, "c6ota.bad_image", bytes = size as u32);
        return Err(8);
    }
    event!(Warn, "c6ota.image", bytes = size as u32);

    let w = connect_wifi()?;
    let n = (size as u32).to_le_bytes();
    w.send(&[wifi::OTA_BEGIN, n[0], n[1], n[2], n[3]]).map_err(|_| 10u32)?;
    status(&w, 11)?;

    // The body, in pieces of one message each: what came with the header,
    // then the rest.
    let mut sent = pieces(&w, &head[end..have])?;
    let mut buf = [0u8; 2048];
    while sent < size {
        let n = net.recv(s, &mut buf).map_err(|_| 13u32)?;
        if n == 0 {
            break;
        }
        sent += pieces(&w, &buf[..n])?;
    }
    let _ = net.close(s);
    event!(Info, "c6ota.fetched", bytes = sent as u32, ms = ((time_ns() - t) / 1_000_000) as u32);
    // `wifi` checks the count too; a short body ends in a refusal there.
    w.send(&[wifi::OTA_END]).map_err(|_| 14u32)?;
    status(&w, 15)?;
    Ok(sent)
}

/// Sends `data` as `OTA_DATA` requests; returns its length.
fn pieces(w: &Channel, data: &[u8]) -> Result<usize, u32> {
    let mut msg = [0u8; chan::MSG_MAX];
    msg[0] = wifi::OTA_DATA;
    for piece in data.chunks(chan::MSG_MAX - 1) {
        msg[1..1 + piece.len()].copy_from_slice(piece);
        w.send(&msg[..1 + piece.len()]).map_err(|_| 12u32)?;
    }
    Ok(data.len())
}

fn connect_wifi() -> Result<Channel, u32> {
    for _ in 0..30 {
        if let Ok(c) = Channel::connect(wifi::SVC) {
            return Ok(c);
        }
        sleep_ms(100);
    }
    Err(9)
}

/// Waits for `[OTA_STATUS, ok, step, esp_err]`; `fail` if not ok.
fn status(w: &Channel, fail: u32) -> Result<(), u32> {
    let mut r = [0u8; chan::MSG_MAX];
    loop {
        let n = w.recv(&mut r).map_err(|_| fail)?;
        if n >= 7 && r[0] == wifi::OTA_STATUS {
            let err = i32::from_le_bytes([r[3], r[4], r[5], r[6]]);
            event!(Info, "c6ota.status", ok = r[1] != 0, step = r[2], esp_err = err);
            return if r[1] != 0 { Ok(()) } else { Err(fail) };
        }
        if n >= 2 && r[0] == wifi::DENIED {
            return Err(fail);
        }
    }
}
