#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! The settings service from tests, where nobody types the shell's `cfg`
//! (doc/50): `run cfgctl MODE`.
//!
//! - 0: a self-test: sets, reads, replaces and deletes `test.*` values,
//!   checks a secret's value is not given over `svc:cfg`, the limits, and
//!   lists; leaves `test.kept` = `1` (to see it after `cfg` restarts).
//! - 1: logs every key (`cfgctl.entry`; a value only if it is not secret).
//! - 2: deletes `wifi.ssid`, `wifi.psk` and `test.kept` (`wifi` joins its
//!   built-in network from its next start).
//!
//! Built with `--permit svc:cfg/r: --permit svc:cfg/w:test. --permit
//! svc:cfg/w:wifi.` (doc/52): it may read every key but no secret's value,
//! and change only `test.*` and `wifi.*`. Exit 0, or the step that failed.

use t5_rt::cfg::{self, err};
use t5_rt::{CfgError, Config, event};

t5_rt::entry!(main);

fn main(mode: usize, _: usize) -> usize {
    let r = Config::connect_within(3000).map_err(|_| 1u32).and_then(|c| match mode {
        0 => self_test(&c),
        1 => list(&c),
        2 => forget(&c),
        _ => Err(2),
    });
    match r {
        Ok(()) => {
            event!(Info, "cfgctl.ok", mode = mode as u32);
            0
        }
        Err(step) => {
            event!(Error, "cfgctl.failed", mode = mode as u32, step = step);
            step as usize
        }
    }
}

/// `NOT_SAVED` counts as done (t5emu has no SD card).
fn done(r: Result<(), CfgError>) -> bool {
    matches!(r, Ok(()) | Err(CfgError::Code(err::NOT_SAVED)))
}

fn self_test(c: &Config) -> Result<(), u32> {
    let mut v = [0u8; cfg::VALUE_MAX];
    let check = |ok: bool, step: u32| if ok { Ok(()) } else { Err(step) };
    check(done(c.set("test.a", b"one", false)), 10)?;
    check(done(c.set("test.a", b"two", false)), 11)?;
    let got = c.get("test.a", &mut v).map_err(|_| 12u32)?;
    check(got.is_some_and(|g| g.flags == 0 && &v[..g.len] == b"two"), 13)?;
    // A secret: set, but its value not given back here.
    check(done(c.set("test.secret", b"hidden", true)), 20)?;
    let got = c.get("test.secret", &mut v).map_err(|_| 21u32)?;
    check(got.is_some_and(|g| g.flags & cfg::SECRET != 0 && g.len == 0), 22)?;
    // Set again without the flag: still secret.
    check(done(c.set("test.secret", b"again", false)), 23)?;
    let got = c.get("test.secret", &mut v).map_err(|_| 24u32)?;
    check(got.is_some_and(|g| g.flags & cfg::SECRET != 0 && g.len == 0), 25)?;
    // Keys outside its grants (doc/52).
    check(c.set("other.key", b"x", false) == Err(CfgError::Code(err::DENIED)), 26)?;
    check(c.remove("other.key") == Err(CfgError::Code(err::DENIED)), 27)?;
    // Limits.
    check(c.set("test.Bad", b"", false) == Err(CfgError::Code(err::BAD)), 30)?;
    check(c.set("test.long", &[b'x'; cfg::VALUE_MAX + 1], false) == Err(CfgError::Code(err::TOO_LONG)), 31)?;
    check(done(c.set("test.max", &[b'x'; cfg::VALUE_MAX], false)), 32)?;
    // Listing sees them all.
    let mut key = [0u8; cfg::KEY_MAX];
    let mut seen = 0;
    for i in 0..=u8::MAX {
        let Some((k, _)) = c.entry(i, &mut key, &mut v).map_err(|_| 40u32)? else { break };
        seen += key[..k].starts_with(b"test.") as u32;
    }
    check(seen >= 3, 41)?;
    // Deleting, twice.
    for k in ["test.a", "test.secret", "test.max"] {
        check(done(c.remove(k)), 50)?;
        check(done(c.remove(k)), 51)?;
        check(c.get(k, &mut v).map_err(|_| 52u32)?.is_none(), 53)?;
    }
    check(done(c.set("test.kept", b"1", false)), 60)?;
    Ok(())
}

fn list(c: &Config) -> Result<(), u32> {
    let mut key = [0u8; cfg::KEY_MAX];
    let mut v = [0u8; cfg::VALUE_MAX];
    for i in 0..=u8::MAX {
        let Some((k, g)) = c.entry(i, &mut key, &mut v).map_err(|_| 10u32)? else { break };
        let key = core::str::from_utf8(&key[..k]).unwrap_or("?");
        if g.flags & cfg::SECRET != 0 {
            event!(Info, "cfgctl.entry", key = key, secret = true);
        } else {
            event!(Info, "cfgctl.entry", key = key, secret = false, value = core::str::from_utf8(&v[..g.len]).unwrap_or("?"));
        }
    }
    Ok(())
}

fn forget(c: &Config) -> Result<(), u32> {
    for k in ["wifi.ssid", "wifi.psk", "test.kept"] {
        if !done(c.remove(k)) {
            return Err(10);
        }
    }
    Ok(())
}
