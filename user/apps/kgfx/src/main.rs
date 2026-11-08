#![no_std]
#![no_main]

//! kumiki-gfx on tab5-os (doc/68): draws with kumiki's `FbDisplay` on the
//! display through `kumiki_t5os::FbPanel`, turned as the user sees the
//! screen. A background and a bar (large: the 2D engine), colour bars, a
//! white square at the top left of the user's view (for the tests), and a
//! square going back and forth for `run kgfx SECONDS` (0: 10 s). Draws
//! everything again when its framebuffer comes back (doc/60 §10).
//!
//! Built with `--permit display`. Exit 0, or the step that failed.

use embedded_graphics_core::{
    geometry::{OriginDimensions, Point, Size},
    pixelcolor::{Rgb565, RgbColor},
    primitives::Rectangle,
};
use kumiki_gfx::{FastDraw, FbDisplay};
use kumiki_t5os::FbPanel;
use t5_rt::{Error, display, event, time_ns, wait};

t5_rt::entry!(main);

const BACKGROUND: Rgb565 = Rgb565::new(3, 6, 4);
const BAR: Rgb565 = Rgb565::new(5, 22, 25);
const SQUARE: u32 = 60;
const STEP_NS: u64 = 40_000_000;

fn main(seconds: usize, _: usize) -> usize {
    match run(if seconds == 0 { 10 } else { seconds as u64 }) {
        Ok(frames) => {
            event!(Info, "kgfx.done", frames = frames);
            0
        }
        Err(step) => {
            event!(Error, "kgfx.failed", step = step);
            step as usize
        }
    }
}

/// Everything but the moving square.
fn scene(d: &mut FbDisplay<FbPanel>) -> Result<(), u32> {
    let size = d.size();
    d.fill_rect(&Rectangle::new(Point::zero(), size), BACKGROUND).map_err(|_| 3u32)?;
    d.fill_rect(&Rectangle::new(Point::zero(), Size::new(size.width, 48)), BAR).map_err(|_| 4u32)?;
    let colors = [Rgb565::RED, Rgb565::GREEN, Rgb565::BLUE, Rgb565::YELLOW, Rgb565::CYAN, Rgb565::MAGENTA];
    let w = size.width / colors.len() as u32;
    for (i, c) in colors.iter().enumerate() {
        let r = Rectangle::new(Point::new(i as i32 * w as i32, size.height as i32 - 80), Size::new(w, 80));
        d.fill_rect(&r, *c).map_err(|_| 5u32)?;
    }
    // The top left as the user sees it.
    d.fill_rect(&Rectangle::new(Point::new(8, 8), Size::new(32, 32)), Rgb565::WHITE).map_err(|_| 6u32)?;
    d.flush().map_err(|_| 7u32)
}

fn run(seconds: u64) -> Result<u32, u32> {
    let panel = FbPanel::open().map_err(|_| 1u32)?;
    let rotation = panel.rotation();
    let mut d = FbDisplay::new(panel).with_rotation(rotation);
    scene(&mut d)?;
    let size = d.size();
    event!(Info, "kgfx.ready", width = size.width, height = size.height, rotation = rotation as u32);

    let end = time_ns() + seconds * 1_000_000_000;
    let (mut x, mut dx) = (0i32, 12i32);
    let y = size.height as i32 / 2 - SQUARE as i32 / 2;
    let mut frames = 0u32;
    while time_ns() < end {
        let ev = match wait(&[d.panel_mut().handle()], STEP_NS) {
            Ok(ev) => Some(ev),
            Err(Error::TimedOut) => None,
            Err(_) => return Err(8),
        };
        if d.panel_mut().take_redraw(ev.as_ref()) {
            event!(Info, "kgfx.redraw");
            d.invalidate();
            scene(&mut d)?;
        }
        if ev.is_some_and(|e| e.kind == display::HIDDEN && e.value == 1) {
            // No framebuffer until shown again: nothing to draw.
            continue;
        }
        let old = Rectangle::new(Point::new(x, y), Size::new(SQUARE, SQUARE));
        d.fill_rect(&old, BACKGROUND).map_err(|_| 9u32)?;
        x += dx;
        if x < 0 || x + SQUARE as i32 > size.width as i32 {
            dx = -dx;
            x += 2 * dx;
        }
        d.fill_rect(&Rectangle::new(Point::new(x, y), Size::new(SQUARE, SQUARE)), Rgb565::WHITE).map_err(|_| 10u32)?;
        d.flush().map_err(|_| 11u32)?;
        frames += 1;
    }
    Ok(frames)
}
