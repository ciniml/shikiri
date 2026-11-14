// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The tailnet's names (MagicDNS, doc/72 §4): the list `ts0`'s far end
//! sends (`netlink::NAMES`), copied here, looked at before DNS.

use t5_rt::netlink;

static mut TABLE: [u8; netlink::NAMES_MAX] = [0; netlink::NAMES_MAX];
static mut LEN: usize = 0;

fn table() -> &'static mut [u8; netlink::NAMES_MAX] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

/// Takes a copy of the list (the far end may change it meanwhile; a
/// broken entry only ends the list).
pub fn set(src: &[u8]) {
    let n = src.len().min(netlink::NAMES_MAX);
    table()[..n].copy_from_slice(&src[..n]);
    unsafe { LEN = n };
}

pub fn clear() {
    unsafe { LEN = 0 };
}

pub fn count() -> u32 {
    let mut t = &table()[..unsafe { LEN }];
    let mut n = 0;
    while let Some((&len, rest)) = t.split_first() {
        let Some(next) = rest.get(len as usize + 4..) else { break };
        t = next;
        n += 1;
    }
    n
}

/// `name`'s IPv4 address in the list (any case, a final dot or not).
pub fn lookup(name: &str) -> Option<u32> {
    let name = name.trim_end_matches('.').as_bytes();
    let mut t = &table()[..unsafe { LEN }];
    while let Some((&len, rest)) = t.split_first() {
        let (n, rest) = (rest.get(..len as usize)?, rest.get(len as usize..)?);
        let ip = rest.get(..4)?;
        if n.eq_ignore_ascii_case(name) {
            return Some(u32::from_be_bytes([ip[0], ip[1], ip[2], ip[3]]));
        }
        t = &rest[4..];
    }
    None
}
