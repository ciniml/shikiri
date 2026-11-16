// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The tailnet's IP packets between the node and `net`'s link `ts0`
//! (doc/72): WireGuard's decrypted packets go into `rx` for the caller to
//! hand on; the caller puts packets for peers in `tx` for the node to
//! encrypt to the peer that has the destination address. No IP stack
//! here (form A's smoltcp went in doc/72 §17).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

#[derive(Default)]
pub struct Tun {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: VecDeque<Vec<u8>>,
}
