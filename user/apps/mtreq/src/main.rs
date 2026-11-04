#![no_std]
#![no_main]

//! A request to the Matter service with numbers only (`run` passes two),
//! for tests where nobody types the shell's `mt` (doc/49 MT2):
//! `run mtreq OP NODE`. OP: 0 `status`, 1 `nodes`, 2 `pair NODE 20202021
//! 3841` (the development passcode; discriminator 3841), 3 `on`, 4 `off`,
//! 5 `toggle`, 6 `read NODE 1 6 0` (On/Off's OnOff), 7 `ocw NODE` (a
//! commissioning window on the node, MT3), 8 `revoke NODE`, 9 `pairble
//! NODE 20202021 3840` (over BLE, MT4), 10 `forget NODE`, 11 `write NODE 1
//! 3 0 10` (Identify's IdentifyTime), 12 `invoke NODE 1 3 0 5` (Identify,
//! 5 s), 13 `sub NODE 1 6 0 1 10` (On/Off's OnOff, every 1 to 10 s). The
//! answer's lines are logged. Built with `--permit svc:matter`. Exit 0 if
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
