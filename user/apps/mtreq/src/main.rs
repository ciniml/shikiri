#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

//! A request to the Matter service with numbers only (`run` passes two),
//! for tests where nobody types the shell's `mt` (doc/49 MT2):
//! `run mtreq OP NODE`. OP: 0 `status`, 1 `nodes`, 2 `pair NODE 20202021
//! 3841` (the development passcode; discriminator 3841), 3 `on`, 4 `off`,
//! 5 `toggle`, 6 `read NODE 1 6 0` (On/Off's OnOff), 7 `ocw NODE` (a
//! commissioning window on the node, MT3), 8 `revoke NODE`, 9 `pairble
//! NODE 20202021 3840` (over BLE, MT4), 10 `forget NODE`, 11 `write NODE 1
//! 3 0 10` (Identify's IdentifyTime), 12 `invoke NODE 1 3 0 5` (Identify,
//! 5 s), 13 `sub NODE 1 6 0 1 10` (On/Off's OnOff, every 1 to 10 s), 14
//! `pairaddr NODE 20202021 127.0.0.1 5540` (its own device, the
//! emulator's test), 15 `netcred t5-test not-a-secret` (a made-up network
//! for the PC's BLE test device, which only pretends to join), 16
//! `netcred` (none), 17 `subev NODE 1 6 0 1 10` (On/Off's StateChange
//! events), 18 `read NODE 0 0x28 1` (Basic Information's VendorName, a
//! string), 19 `eps` (the apps' endpoints, doc/53), 20 `read NODE 2
//! 0x402 0` (endpoint 2's temperature), 21 `invoke NODE 3 6 2` (toggle
//! endpoint 3), 22 `read NODE 0 0x1d 3` (PartsList), 23 `epdel NODE` (NODE
//! is the endpoint), 24 + 256 * NODE `paircode NODE CODE` (NODE: the
//! first 10 digits of a manual pairing code), 25 `watch NODE 1 6 0`
//! (On/Off), 26 `watch NODE 1 0x45 0` (BooleanState), 27 `watch NODE 1
//! 0x40d 0` (CO2), 28 `values`, 29 `watch NODE 2 0x402 0` (temperature),
//! 30 `watch NODE 3 0x405 0` (humidity), 31 + 256 * NODE + 65536 * N
//! `pairaddr NODE CODE 192.168.8.N 5540`, 32 `relocate NODE`, 34 + 256 * NODE
//! `pairble NODE CODE`, 33 `unwatch
//! NODE 3 0x405 0`. The answer's lines are logged. Built with `--permit svc:matter`. Exit 0 if
//! the answer ended well.

use core::fmt::Write as _;

use t5_rt::log::FmtBuf;
use t5_rt::{Channel, event, matter};

t5_rt::entry!(main);

fn main(op: usize, node: usize) -> usize {
    let mut req = FmtBuf::<64>::new();
    let _ = match op {
        0 => write!(req, "status"),
        1 => write!(req, "nodes"),
        2 => write!(req, "pair {node} 20202021 3841"),
        3 => write!(req, "on {node}"),
        4 => write!(req, "off {node}"),
        5 => write!(req, "toggle {node}"),
        6 => write!(req, "read {node} 1 6 0"),
        7 => write!(req, "ocw {node}"),
        8 => write!(req, "revoke {node}"),
        9 => write!(req, "pairble {node} 20202021 3840"),
        10 => write!(req, "forget {node}"),
        11 => write!(req, "write {node} 1 3 0 10"),
        12 => write!(req, "invoke {node} 1 3 0 5"),
        13 => write!(req, "sub {node} 1 6 0 1 10"),
        14 => write!(req, "pairaddr {node} 20202021 127.0.0.1 5540"),
        15 => write!(req, "netcred t5-test not-a-secret"),
        16 => write!(req, "netcred"),
        17 => write!(req, "subev {node} 1 6 0 1 10"),
        18 => write!(req, "read {node} 0 0x28 1"),
        19 => write!(req, "eps"),
        20 => write!(req, "read {node} 2 0x402 0"),
        21 => write!(req, "invoke {node} 3 6 2"),
        22 => write!(req, "read {node} 0 0x1d 3"),
        23 => write!(req, "epdel {node}"),
        // The demo's (doc/49 §25): OP 24 + 256 * NODE with the first 10
        // digits of a manual pairing code (the check digit is worked out).
        o if o & 0xFF == 24 => write!(req, "paircode {} {:010}", o >> 8, node),
        // 31 + 256 * NODE + 65536 * N: `pairaddr NODE CODE 192.168.8.N 5540`
        // (matter-test; NODE here is the code's first 10 digits).
        o if o & 0xFF == 31 => write!(req, "pairaddr {} {:010} 192.168.8.{} 5540", (o >> 8) & 0xFF, node, o >> 16),
        32 => write!(req, "relocate {node}"),
        // 34 + 256 * NODE: `pairble NODE CODE10` (as OP 24, over BLE).
        o if o & 0xFF == 34 => write!(req, "pairble {} {:010}", o >> 8, node),
        33 => write!(req, "unwatch {node} 3 0x405 0"),
        25 => write!(req, "watch {node} 1 6 0"),
        26 => write!(req, "watch {node} 1 0x45 0"),
        27 => write!(req, "watch {node} 1 0x40d 0"),
        28 => write!(req, "values"),
        29 => write!(req, "watch {node} 2 0x402 0"),
        30 => write!(req, "watch {node} 3 0x405 0"),
        _ => return 1,
    };
    let Ok(c) = Channel::connect(matter::SVC) else {
        event!(Error, "mtreq.no_service");
        return 2;
    };
    event!(Info, "mtreq.request", text = req.as_str());
    if c.send(req.as_str().as_bytes()).is_err() {
        return 3;
    }
    let mut m = [0u8; 128];
    loop {
        match c.recv(&mut m) {
            Ok(k) if k >= 1 && m[0] == matter::LINE => {
                event!(Info, "mtreq.line", text = core::str::from_utf8(&m[1..k]).unwrap_or("?"));
            }
            Ok(k) if k >= 2 && m[0] == matter::END => {
                event!(Info, "mtreq.done", ok = m[1] != 0);
                return (m[1] == 0) as usize;
            }
            Ok(_) => {}
            Err(_) => return 4,
        }
    }
}
