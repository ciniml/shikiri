#![no_std]
#![no_main]

//! M6c app (doc/17 §6): draws on the display where it is touched.
//! - `mode` 0: for `seconds` (0 = 60) draws a square under each finger.
//!   A bar along the top edge as the user sees it shows the orientation.
//! - `mode` 1 (self-test): checks the display and touch rules and exits.
//!
//! Built with `--permit display --permit touch`. Exit 0 if everything held,
//! otherwise the number of the failed check.

use t5_rt::{Display, Error, Handle, Point, Touch, display, event, rgb, rights, sleep_ms, time_ns, touch, wait};

t5_rt::entry!(main);

const BACKGROUND: u16 = rgb(24, 24, 32);
const BAR: u16 = rgb(40, 90, 200);
const COLORS: [u16; 5] = [rgb(255, 80, 80), rgb(80, 255, 80), rgb(80, 160, 255), rgb(255, 220, 60), rgb(255, 255, 255)];
const SQUARE: usize = 16;

fn main(mode: usize, seconds: usize) -> usize {
    let secs = if seconds == 0 { 60 } else { seconds as u64 };
    let r = match mode {
        1 => self_test(),
        2 => keep_drawing(secs),
        _ => paint(secs),
    };
    match r {
        Ok(()) => 0,
        Err(check) => {
            event!(Error, "paint.failed", mode = mode, check = check);
            check as usize
        }
    }
}

fn check(ok: bool, n: u32) -> Result<(), u32> {
    if ok { Ok(()) } else { Err(n) }
}

/// The framebuffer rows a user rectangle touches.
fn rows(d: &Display, x: usize, y: usize, w: usize, h: usize) -> (usize, usize) {
    let (sw, sh) = d.size();
    let (x1, y1) = ((x + w).min(sw) - 1, (y + h).min(sh) - 1);
    let corners = [d.fb_row(x, y), d.fb_row(x1, y), d.fb_row(x, y1), d.fb_row(x1, y1)];
    (*corners.iter().min().unwrap(), *corners.iter().max().unwrap() + 1)
}

/// The background, the bar and the white corner.
fn frame(d: &mut Display) -> Result<(), u32> {
    let (w, _) = d.size();
    d.fill(BACKGROUND);
    d.rect(0, 0, w, 48, BAR);
    d.rect(8, 8, 32, 32, rgb(255, 255, 255)); // top-left corner as the user sees it
    d.flush_all().map_err(|_| 3u32)
}

/// Mode 2 (doc/60 §10): a yellow square around (600, 400) every 50 ms,
/// never looking at the display's events, as an app that knows nothing of
/// seats. While its framebuffer is taken away it is stopped at the first
/// write, and goes on once its seat is in front again.
fn keep_drawing(seconds: u64) -> Result<(), u32> {
    let mut d = Display::open().map_err(|_| 1u32)?;
    let end = time_ns() + seconds * 1_000_000_000;
    let mut squares = 0u32;
    while time_ns() < end {
        d.rect(590, 390, 20, 20, COLORS[3]);
        let (r0, r1) = rows(&d, 590, 390, 20, 20);
        d.flush(r0, r1).map_err(|_| 6u32)?;
        squares += 1;
        sleep_ms(50);
    }
    event!(Info, "paint.done", squares = squares);
    Ok(())
}

fn paint(seconds: u64) -> Result<(), u32> {
    let mut d = Display::open().map_err(|_| 1u32)?;
    // Woken by seat changes and the touch, not every frame.
    let _ = d.handle.ctl(display::FRAMES, 0);
    let t = Touch::open().map_err(|_| 2u32)?;
    let (w, _) = d.size();
    frame(&mut d)?;
    event!(Info, "paint.ready", width = w as u32, height = d.size().1 as u32, quarter = d.quarter as u32);

    let end = time_ns() + seconds * 1_000_000_000;
    let mut squares = 0u32;
    let mut pts = [Point::default(); touch::MAX_POINTS];
    while time_ns() < end {
        match wait(&[&t.0, &d.handle], 100_000_000) {
            // The seat went behind or came back (doc/60). Shown after the
            // framebuffer was taken away (value 1): it is black, the
            // squares are gone.
            Ok(ev) if ev.index == 1 => {
                match ev.kind {
                    display::HIDDEN => event!(Info, "paint.hidden", taken = ev.value as u32),
                    display::SHOWN => {
                        event!(Info, "paint.shown", lost = ev.value as u32);
                        if ev.value == 1 {
                            frame(&mut d)?;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Ok(_) => {}
            Err(Error::TimedOut) => continue,
            Err(_) => return Err(4),
        }
        let n = t.points(&mut pts).map_err(|_| 5u32)?;
        for p in &pts[..n] {
            let (x, y) = ((p.x as usize).saturating_sub(SQUARE / 2), (p.y as usize).saturating_sub(SQUARE / 2));
            d.rect(x, y, SQUARE, SQUARE, COLORS[p.id as usize % COLORS.len()]);
            let (r0, r1) = rows(&d, x, y, SQUARE, SQUARE);
            d.flush(r0, r1).map_err(|_| 6u32)?;
            squares += 1;
        }
    }
    event!(Info, "paint.done", squares = squares);
    Ok(())
}

fn self_test() -> Result<(), u32> {
    let mut d = Display::open().map_err(|_| 10u32)?;
    // One seat per process (doc/60).
    check(Handle::open(display::NAME, rights::CTL | rights::WAIT).err() == Some(Error::Busy), 11)?;
    check((d.fb_w, d.fb_h) == (720, 1280), 12)?;
    // The mapped framebuffer is writable: a grey band along the top.
    let (w, _) = d.size();
    d.rect(0, 0, w, 8, rgb(128, 128, 128));
    let (r0, r1) = rows(&d, 0, 0, w, 8);
    d.flush(r0, r1).map_err(|_| 13u32)?;
    check(d.flush(8, 4).err() == Some(Error::Invalid), 14)?;
    check(d.flush(0, 2000).err() == Some(Error::Invalid), 15)?;
    check(d.handle.ctl(99, 0).err() == Some(Error::NoSupport), 16)?;
    // Frames keep coming.
    let start = time_ns();
    let mut frames = 0;
    while frames < 3 {
        let ev = wait(&[&d.handle], 1_000_000_000).map_err(|_| 17u32)?;
        // A seat change (doc/60) may come between frames.
        if matches!(ev.kind, display::HIDDEN | display::SHOWN) {
            continue;
        }
        check(ev.kind == display::FRAME && ev.value > 0, 18)?;
        frames += ev.value;
    }
    let ms = (time_ns() - start) / 1_000_000;
    let t = Touch::open().map_err(|_| 19u32)?;
    let mut pts = [Point::default(); touch::MAX_POINTS];
    t.points(&mut pts).map_err(|_| 20u32)?;
    check(t.0.write(&[0]).err() == Some(Error::Denied), 21)?;
    event!(Info, "paint.selftest", frames = frames as u32, ms = ms as u32);
    Ok(())
}
