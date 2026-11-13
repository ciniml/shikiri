#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! Matter on the screen (doc/53 §4): `run mtpanel SECONDS` (0: until
//! stopped). It brings a `light` endpoint (label `panel`) and draws it as
//! a tile: touching the tile switches it, and a controller's change shows
//! at once. Beside it, the apps' endpoints (`eps`, every 3 s).
//!
//! `display` has one owner (doc/17 §6), usually `term`: stop it first.
//! Built with `--permit display --permit touch --permit
//! svc:matter/endpoint --permit svc:matter/read`; once its endpoint is
//! there it gives up `display`, `touch` and `svc:matter/endpoint` (doc/56
//! §4): it keeps what it opened, and can open nothing new but `eps`. Exit
//! 0, or the step that failed (1: the display is someone else's).

use core::fmt::Write as _;

use t5_rt::log::FmtBuf;
use t5_rt::{Channel, Display, Error, Point, Touch, chan, event, matter, permit_drop, rgb, time_ns, touch, wait};

t5_rt::entry!(main);

/// Terminus 16 x 32, U+0020..=U+007E (as `term`, doc/39 §4).
static FONT: &[u8] = include_bytes!("../../../../third_party/terminus-font/ascii-16x32.bin");
const CW: usize = 16;
const CH: usize = 32;

const BACKGROUND: u16 = rgb(20, 22, 30);
const BAR: u16 = rgb(30, 70, 150);
const TEXT: u16 = rgb(235, 235, 235);
const DIM: u16 = rgb(150, 150, 160);
const ON: u16 = rgb(255, 196, 40);
const OFF: u16 = rgb(70, 72, 84);
const BAR_H: usize = 64;
/// The tile: where it is, as the user sees the screen.
const TILE: (usize, usize, usize, usize) = (32, BAR_H + 32, 320, 320);
const LIST_X: usize = TILE.0 + TILE.2 + 48;
const LIST_LINES: usize = 8;
const EPS_MS: u64 = 3000;

fn main(seconds: usize, _: usize) -> usize {
    match run(seconds as u64) {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "mtpanel.failed", step = step);
            step as usize
        }
    }
}

fn now_ms() -> u64 {
    time_ns() / 1_000_000
}

/// Text at (x, y), each font pixel `scale` x `scale`, on `bg`.
fn text(d: &mut Display, x: usize, y: usize, s: &str, scale: usize, fg: u16, bg: u16) {
    let (w, h) = d.size();
    for (i, c) in s.chars().enumerate() {
        let gx = x + i * CW * scale;
        if gx + CW * scale > w || y + CH * scale > h {
            break;
        }
        let g = match c {
            ' '..='~' => c as usize - 0x20,
            _ => '?' as usize - 0x20,
        };
        let bits = &FONT[g * CW / 8 * CH..][..CW / 8 * CH];
        for row in 0..CH {
            let line = u16::from_be_bytes([bits[2 * row], bits[2 * row + 1]]);
            for col in 0..CW {
                let color = if line & 0x8000 >> col != 0 { fg } else { bg };
                d.rect(gx + col * scale, y + row * scale, scale, scale, color);
            }
        }
    }
}

struct Panel {
    ep: u16,
    on: bool,
    lines: [FmtBuf<48>; LIST_LINES],
    n: usize,
}

fn draw(d: &mut Display, p: &Panel) -> Result<(), u32> {
    let (w, h) = d.size();
    d.fill(BACKGROUND);
    d.rect(0, 0, w, BAR_H, BAR);
    let mut t = FmtBuf::<48>::new();
    let _ = write!(t, "Matter  endpoint {}", p.ep);
    text(d, 16, 16, t.as_str(), 1, TEXT, BAR);
    let (x, y, tw, th) = TILE;
    let color = if p.on { ON } else { OFF };
    d.rect(x, y, tw, th, color);
    let label = if p.on { "ON" } else { "OFF" };
    let fg = if p.on { BACKGROUND } else { TEXT };
    text(d, x + (tw - label.len() * CW * 3) / 2, y + (th - CH * 3) / 2, label, 3, fg, color);
    text(d, x, y + th + 16, "touch to switch", 1, DIM, BACKGROUND);
    text(d, LIST_X, BAR_H + 32, "endpoints", 1, DIM, BACKGROUND);
    for (i, l) in p.lines[..p.n].iter().enumerate() {
        let ly = BAR_H + 80 + i * (CH + 8);
        if ly + CH < h {
            text(d, LIST_X, ly, l.as_str(), 1, TEXT, BACKGROUND);
        }
    }
    d.flush_all().map_err(|_| 9u32)
}

/// The apps' endpoints, `eps` (shortened to fit). Errors keep the old
/// list. Whether it changed.
fn refresh(p: &mut Panel) -> bool {
    let Ok(c) = Channel::connect(matter::SVC) else { return false };
    if c.send(b"eps").is_err() {
        return false;
    }
    let mut changed = false;
    let deadline = now_ms() + 1000;
    let mut m = [0u8; 128];
    let mut n = 0;
    while now_ms() < deadline {
        match c.0.read(&mut m) {
            Ok(k) if k >= 1 && m[0] == matter::LINE => {
                // "endpoint N KIND of OWNER: VALUE" -> "N KIND VALUE"
                let line = core::str::from_utf8(&m[1..k]).unwrap_or("");
                let mut w = line.split_ascii_whitespace().skip(1);
                if n < LIST_LINES
                    && let (Some(ep), Some(kind), Some(value)) = (w.next(), w.next(), line.rsplit(' ').next())
                {
                    let mut l = FmtBuf::new();
                    let _ = write!(l, "{ep} {kind} {value}");
                    changed |= n >= p.n || l.as_str() != p.lines[n].as_str();
                    p.lines[n] = l;
                    n += 1;
                }
            }
            Ok(k) if k >= 1 && m[0] == matter::END => {
                changed |= n != p.n;
                p.n = n;
                return changed;
            }
            Ok(_) => {}
            Err(Error::Busy) => {
                let _ = wait(&[&c.0], 100_000_000);
            }
            Err(_) => return false,
        }
    }
    false
}

fn run(seconds: u64) -> Result<(), u32> {
    let mut d = Display::open().map_err(|e| {
        event!(Warn, "mtpanel.no_display", busy = e == Error::Busy);
        1u32
    })?;
    let t = Touch::open().map_err(|_| 2u32)?;
    let c = Channel::connect(matter::SVC).map_err(|_| 3u32)?;
    c.send(b"endpoint light panel").map_err(|_| 4u32)?;
    let mut p = Panel { ep: 0, on: false, lines: [const { FmtBuf::new() }; LIST_LINES], n: 0 };
    let mut m = [0u8; 128];
    loop {
        match c.recv(&mut m).map_err(|_| 5u32)? {
            k if k >= 1 && m[0] == matter::LINE => {
                let line = core::str::from_utf8(&m[1..k]).unwrap_or("");
                p.ep = line.split_ascii_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            }
            k if k >= 2 && m[0] == matter::END => {
                if m[1] == 0 {
                    return Err(6);
                }
                break;
            }
            _ => {}
        }
    }
    for permit in ["display", "touch", "svc:matter/endpoint"] {
        let _ = permit_drop(permit);
    }
    event!(Info, "mtpanel.ready", ep = p.ep as u32);
    refresh(&mut p);
    draw(&mut d, &p)?;
    let end = (seconds != 0).then(|| now_ms() + seconds * 1000);
    let mut next_eps = now_ms() + EPS_MS;
    let mut pts = [Point::default(); touch::MAX_POINTS];
    let mut down = false;
    loop {
        let now = now_ms();
        if end.is_some_and(|e| now >= e) {
            return Ok(());
        }
        let mut dirty = false;
        // `matter`: the state (at first, and a controller's change).
        loop {
            match c.0.read(&mut m) {
                Ok(k) if k >= 1 && m[0] == matter::STATE => {
                    let on = &m[1..k] == b"on 1";
                    event!(Info, "mtpanel.state", on = on);
                    dirty |= on != p.on;
                    p.on = on;
                }
                Ok(_) => {}
                Err(Error::Busy) => break,
                Err(_) => return Err(7),
            }
        }
        // A finger coming down on the tile switches it; `matter` answers
        // with the new state.
        let n = t.points(&mut pts).map_err(|_| 8u32)?;
        let (x, y, tw, th) = TILE;
        if n > 0 && !down {
            let q = pts[0];
            let (qx, qy) = (q.x as usize, q.y as usize);
            if (x..x + tw).contains(&qx) && (y..y + th).contains(&qy) {
                let req: &[u8] = if p.on { b"set 0" } else { b"set 1" };
                c.send(req).map_err(|_| 4u32)?;
                event!(Info, "mtpanel.touched", on = !p.on);
            }
        }
        down = n > 0;
        if now >= next_eps {
            next_eps = now + EPS_MS;
            dirty |= refresh(&mut p);
        }
        if dirty {
            draw(&mut d, &p)?;
        }
        let wake = next_eps.min(end.unwrap_or(u64::MAX));
        let ms = wake.saturating_sub(now_ms()).clamp(1, 1000);
        match wait(&[&c.0, &t.0], ms * 1_000_000) {
            Ok(ev) if ev.index == 0 && ev.kind == chan::PEER_CLOSED => return Err(7),
            _ => {}
        }
    }
}
