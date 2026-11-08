#![no_std]
#![no_main]

//! The controller's nodes on the screen: `run mtdash`. A tile per
//! attribute `matter` watches (`watch NODE EP CLUSTER ATTR`, kept in its
//! store and subscribed to again after a loss or a restart): an On/Off
//! light (touch it to toggle), a contact sensor, CO2, temperature,
//! humidity, PM2.5, air quality, or the raw value. It asks `values` every
//! second and draws what changed.
//!
//! Its screen is a seat (doc/60): behind `term` or in front, switched with
//! the key, the edge swipe or `fg`; it draws again when shown after its
//! framebuffer was taken away. Built with `--permit display --permit touch --permit svc:matter/read
//! --permit svc:matter/control`. Exit 0 when stopped, or the step that
//! failed (1: no seat free).

use core::fmt::Write as _;

use t5_rt::log::FmtBuf;
use t5_rt::{Channel, Display, Error, Point, Touch, chan, display, event, matter, rgb, sleep_ms, time_ns, touch, wait};

t5_rt::entry!(main);

/// Terminus 16 x 32, U+0020..=U+007E (as `term`, doc/39 §4).
static FONT: &[u8] = include_bytes!("../../../../third_party/terminus-font/ascii-16x32.bin");
const CW: usize = 16;
const CH: usize = 32;

const BACKGROUND: u16 = rgb(20, 22, 30);
const BAR: u16 = rgb(30, 70, 150);
const TEXT: u16 = rgb(235, 235, 235);
const DIM: u16 = rgb(150, 150, 160);
const TILE_BG: u16 = rgb(44, 47, 60);
const ON: u16 = rgb(255, 196, 40);
const OPEN: u16 = rgb(220, 70, 60);
const CLOSED: u16 = rgb(60, 160, 90);
const HIGH: u16 = rgb(220, 120, 40);
const STALE: u16 = rgb(90, 60, 60);
const BAR_H: usize = 64;
const COLS: usize = 3;
const GAP: usize = 24;
const TILES: usize = 8;
const POLL_MS: u64 = 1000;
/// After a toggle the new state comes back in a report: look sooner.
const SOON_MS: u64 = 250;
/// CO2 above this (ppm) is drawn as high.
const CO2_HIGH: f32 = 1000.0;

const ONOFF: u32 = 0x0006;
const BOOLEAN_STATE: u32 = 0x0045;
const AIR_QUALITY: u32 = 0x005B;
const TEMPERATURE: u32 = 0x0402;
const HUMIDITY: u32 = 0x0405;
const CO2: u32 = 0x040D;
const PM25: u32 = 0x042A;

fn main(_: usize, _: usize) -> usize {
    match run() {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "mtdash.failed", step = step);
            step as usize
        }
    }
}

fn now_ms() -> u64 {
    time_ns() / 1_000_000
}

/// Text at (x, y), each font pixel `scale` x `scale`, on `bg`; cut at
/// `max_w` pixels.
fn text(d: &mut Display, x: usize, y: usize, s: &str, scale: usize, fg: u16, bg: u16, max_w: usize) {
    let (w, h) = d.size();
    for (i, c) in s.chars().enumerate() {
        let gx = x + i * CW * scale;
        if gx + CW * scale > w.min(x + max_w) || y + CH * scale > h {
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

/// One `values` line: NODE EP CLUSTER ATTR +|- AGE|- VALUE|-.
#[derive(Clone, PartialEq)]
struct Entry {
    node: u64,
    ep: u16,
    cluster: u32,
    attr: u32,
    live: bool,
    value: heapless::String<48>,
}

impl Entry {
    fn parse(line: &str) -> Option<Entry> {
        let mut w = line.splitn(7, ' ');
        let node = w.next()?.parse().ok()?;
        let ep = w.next()?.parse().ok()?;
        let cluster = w.next()?.parse().ok()?;
        let attr = w.next()?.parse().ok()?;
        let live = w.next()? == "+";
        let _age = w.next()?;
        let mut value = heapless::String::new();
        let v = w.next().unwrap_or("-");
        let _ = value.push_str(&v[..v.floor_char_boundary(48)]);
        Some(Entry { node, ep, cluster, attr, live, value })
    }

    fn number(&self) -> Option<f32> {
        self.value.as_str().parse().ok()
    }

    /// Title, the big text, its colour on the tile.
    fn show(&self) -> (&'static str, FmtBuf<24>, u16) {
        let v = self.value.as_str();
        let mut t = FmtBuf::<24>::new();
        let unknown = v == "-" || v == "null";
        let (title, color) = match self.cluster {
            ONOFF => {
                let on = v == "true";
                let _ = t.write_str(if unknown { "--" } else if on { "ON" } else { "OFF" });
                ("Light", if on { ON } else { TILE_BG })
            }
            // BooleanState of a contact sensor: true is "in contact".
            BOOLEAN_STATE => {
                let closed = v == "true";
                let _ = t.write_str(if unknown { "--" } else if closed { "CLOSED" } else { "OPEN" });
                ("Door", if unknown { TILE_BG } else if closed { CLOSED } else { OPEN })
            }
            CO2 => {
                let n = self.number();
                match n {
                    Some(n) => _ = write!(t, "{} ppm", n as i32),
                    None => _ = t.write_str("--"),
                }
                ("CO2", if n.is_some_and(|n| n >= CO2_HIGH) { HIGH } else { TILE_BG })
            }
            TEMPERATURE => {
                match self.number() {
                    Some(n) => _ = write!(t, "{:.1} C", n / 100.0),
                    None => _ = t.write_str("--"),
                }
                ("Temperature", TILE_BG)
            }
            HUMIDITY => {
                match self.number() {
                    Some(n) => _ = write!(t, "{:.0} %", n / 100.0),
                    None => _ = t.write_str("--"),
                }
                ("Humidity", TILE_BG)
            }
            PM25 => {
                match self.number() {
                    Some(n) => _ = write!(t, "{:.0} ug", n),
                    None => _ = t.write_str("--"),
                }
                ("PM2.5", TILE_BG)
            }
            AIR_QUALITY => {
                const NAMES: [&str; 7] = ["unknown", "good", "fair", "moderate", "poor", "v.poor", "ext.poor"];
                let n = self.number().map(|n| n as usize);
                let _ = t.write_str(n.and_then(|n| NAMES.get(n)).copied().unwrap_or("--"));
                ("Air quality", if n.is_some_and(|n| n >= 4) { HIGH } else { TILE_BG })
            }
            _ => {
                let _ = t.write_str(&v[..v.len().min(12)]);
                ("Value", TILE_BG)
            }
        };
        (title, t, if self.live { color } else { STALE })
    }
}

struct Dash {
    entries: heapless::Vec<Entry, TILES>,
    /// The `matter` service answered the last poll.
    reachable: bool,
    note: FmtBuf<48>,
}

fn tile_rect(d: &Display, i: usize) -> (usize, usize, usize, usize) {
    let (w, _) = d.size();
    let tw = (w - GAP * (COLS + 1)) / COLS;
    let th = 260;
    (GAP + (i % COLS) * (tw + GAP), BAR_H + GAP + (i / COLS) * (th + GAP), tw, th)
}

fn draw(d: &mut Display, s: &Dash) -> Result<(), u32> {
    let (w, h) = d.size();
    d.fill(BACKGROUND);
    d.rect(0, 0, w, BAR_H, BAR);
    text(d, 16, 16, "tab5-os  Matter controller", 1, TEXT, BAR, w);
    if !s.reachable {
        text(d, w - 16 * CW - 16, 16, "matter: no answer", 1, TEXT, BAR, w);
    }
    if s.entries.is_empty() {
        text(d, GAP, BAR_H + GAP, "nothing watched (mt watch NODE EP CLUSTER ATTR)", 1, DIM, BACKGROUND, w);
    }
    for (i, e) in s.entries.iter().enumerate() {
        let (x, y, tw, th) = tile_rect(d, i);
        if y + th > h {
            break;
        }
        let (title, big, color) = e.show();
        d.rect(x, y, tw, th, color);
        let fg = if color == ON { BACKGROUND } else { TEXT };
        text(d, x + 16, y + 16, title, 1, fg, color, tw - 32);
        let scale = if big.as_str().len() * CW * 3 + 32 <= tw { 3 } else { 2 };
        let bw = big.as_str().len() * CW * scale;
        text(d, x + tw.saturating_sub(bw) / 2, y + (th - CH * scale) / 2, big.as_str(), scale, fg, color, tw);
        let mut foot = FmtBuf::<32>::new();
        let _ = write!(foot, "node {} ep {}{}", e.node, e.ep, if e.live { "" } else { "  (waiting)" });
        text(d, x + 16, y + th - CH - 12, foot.as_str(), 1, fg, color, tw - 32);
    }
    text(d, GAP, h - CH - 12, s.note.as_str(), 1, DIM, BACKGROUND, w);
    d.flush_all().map_err(|_| 9u32)
}

/// A request to `matter`: its lines to `line`, then whether it said ok.
/// None if `matter` did not answer within `ms`.
fn ask(req: &str, ms: u64, mut line: impl FnMut(&str)) -> Option<bool> {
    let c = Channel::connect(matter::SVC).ok()?;
    c.send(req.as_bytes()).ok()?;
    let deadline = now_ms() + ms;
    let mut m = [0u8; 128];
    while now_ms() < deadline {
        match c.0.read(&mut m) {
            Ok(k) if k >= 1 && m[0] == matter::LINE => line(core::str::from_utf8(&m[1..k]).unwrap_or("")),
            Ok(k) if k >= 2 && m[0] == matter::END => return Some(m[1] != 0),
            Ok(_) => {}
            Err(Error::Busy) => {
                let _ = wait(&[&c.0], 50_000_000);
            }
            Err(_) => return None,
        }
    }
    None
}

/// `values` into `s`; whether anything changed.
fn poll(s: &mut Dash) -> bool {
    let mut got: heapless::Vec<Entry, TILES> = heapless::Vec::new();
    let r = ask("values", 1000, |l| {
        if let Some(e) = Entry::parse(l) {
            let _ = got.push(e);
        }
    });
    let reachable = r == Some(true);
    let changed = reachable != s.reachable || (reachable && got != s.entries);
    s.reachable = reachable;
    if reachable {
        s.entries = got;
    }
    changed
}

/// Toggles a light; the controller may be busy with a subscribe for a
/// moment, so a few tries.
fn toggle(node: u64, s: &mut Dash) {
    let mut req = FmtBuf::<32>::new();
    let _ = write!(req, "toggle {node}");
    for _ in 0..6 {
        let mut busy = false;
        let r = ask(req.as_str(), 8000, |l| busy |= l.starts_with("busy"));
        s.note = FmtBuf::new();
        match r {
            Some(true) => {
                let _ = write!(s.note, "toggled node {node}");
                return;
            }
            Some(false) if busy => sleep_ms(300),
            _ => {
                let _ = write!(s.note, "toggle node {node} failed");
                return;
            }
        }
    }
    let _ = write!(s.note, "controller busy");
}

fn run() -> Result<(), u32> {
    let mut d = Display::open().map_err(|e| {
        event!(Warn, "mtdash.no_display", busy = e == Error::Busy);
        1u32
    })?;
    // Woken by seat changes, not every frame.
    let _ = d.handle.ctl(display::FRAMES, 0);
    let t = Touch::open().map_err(|_| 2u32)?;
    let mut s = Dash { entries: heapless::Vec::new(), reachable: false, note: FmtBuf::new() };
    poll(&mut s);
    draw(&mut d, &s)?;
    event!(Info, "mtdash.ready", tiles = s.entries.len() as u32);
    let mut next_poll = now_ms() + POLL_MS;
    let mut pts = [Point::default(); touch::MAX_POINTS];
    let mut down = false;
    // Whether the framebuffer is ours (doc/60 §10), and a draw put off
    // while it was not.
    let (mut screen, mut owed) = (true, false);
    loop {
        let mut dirty = false;
        // A finger coming down on a light's tile toggles it.
        let n = t.points(&mut pts).map_err(|_| 8u32)?;
        if n > 0 && !down {
            let (qx, qy) = (pts[0].x as usize, pts[0].y as usize);
            let hit = (0..s.entries.len()).find(|&i| {
                let (x, y, tw, th) = tile_rect(&d, i);
                (x..x + tw).contains(&qx) && (y..y + th).contains(&qy)
            });
            if let Some(e) = hit.map(|i| s.entries[i].clone())
                && e.cluster == ONOFF
            {
                event!(Info, "mtdash.toggle", node = e.node);
                toggle(e.node, &mut s);
                dirty = true;
                next_poll = now_ms() + SOON_MS;
            }
        }
        down = n > 0;
        if now_ms() >= next_poll {
            next_poll = now_ms() + POLL_MS;
            dirty |= poll(&mut s);
        }
        if dirty && screen {
            draw(&mut d, &s)?;
        }
        owed |= dirty && !screen;
        let ms = next_poll.saturating_sub(now_ms()).clamp(1, POLL_MS);
        if let Ok(ev) = wait(&[&t.0, &d.handle], ms * 1_000_000)
            && ev.index == 1
        {
            match ev.kind {
                display::HIDDEN => screen = ev.value == 0,
                display::SHOWN => {
                    screen = true;
                    if ev.value == 1 || owed {
                        owed = false;
                        draw(&mut d, &s)?;
                    }
                }
                _ => {}
            }
        }
        let _ = chan::PEER_CLOSED;
    }
}
