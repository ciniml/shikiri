#![no_std]
#![no_main]

//! The font service (doc/45): listens on `svc:font` and grants each client
//! the glyph file (`third_party/jp-font/glyphs.bin`, Terminus Font and Noto
//! Sans Mono CJK JP, SIL OFL 1.1) as shared memory. Built for XIP: the file
//! stays in flash until it is copied once into the shared memory.
//!
//! Built with `--permit listen:font --permit shm`.

use t5_rt::{Channel, FOREVER, Listener, Shm, chan, event, font, shm, svc, wait_raw};

t5_rt::entry!(main);

static GLYPHS: &[u8] = include_bytes!("../../../../third_party/jp-font/glyphs.bin");
const CLIENTS: usize = 8;

fn main(_: usize, _: usize) -> usize {
    match serve() {
        Ok(()) => 0,
        Err(step) => {
            event!(Error, "font.failed", step = step);
            step as usize
        }
    }
}

fn serve() -> Result<(), u32> {
    // Copied before listening: a client that connects gets its grant at
    // once (on the chip, about 0.4 s from start to listening).
    let len = GLYPHS.len().next_multiple_of(shm::GRAIN);
    let pool = Shm::alloc(len).map_err(|_| 2u32)?;
    let mem = pool.map().map_err(|_| 3u32)?;
    mem[..GLYPHS.len()].copy_from_slice(GLYPHS);
    let l = Listener::open(font::SVC).map_err(|_| 1u32)?;
    let word = |i: usize| u32::from_le_bytes([GLYPHS[4 * i], GLYPHS[4 * i + 1], GLYPHS[4 * i + 2], GLYPHS[4 * i + 3]]);
    event!(Info, "font.ready", bytes = GLYPHS.len() as u32, narrow = word(1), wide = word(4));
    let mut conns: [Option<Channel>; CLIENTS] = [const { None }; CLIENTS];
    loop {
        let mut hs = [0u32; 1 + CLIENTS];
        let mut who = [0usize; 1 + CLIENTS];
        hs[0] = l.0.raw();
        let mut n = 1;
        for (i, c) in conns.iter().enumerate() {
            if let Some(c) = c {
                hs[n] = c.0.raw();
                who[n] = i;
                n += 1;
            }
        }
        let ev = wait_raw(&hs[..n], FOREVER).map_err(|_| 4u32)?;
        if ev.index == 0 {
            if ev.kind != svc::PENDING {
                continue;
            }
            let Ok(c) = l.accept() else { continue };
            // A full table turns the newcomer away (its channel closes).
            let Some(slot) = conns.iter().position(Option::is_none) else { continue };
            if pool.grant(&c, 0, len).is_ok() {
                event!(Debug, "font.granted", slot = slot as u32);
                conns[slot] = Some(c);
            }
        } else if ev.kind == chan::PEER_CLOSED {
            conns[who[ev.index]] = None;
        }
    }
}
