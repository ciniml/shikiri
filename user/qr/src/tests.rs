// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

use super::*;

use crate::vectors;

fn ecl(c: char) -> Ecl {
    match c {
        'L' => Ecl::L,
        'M' => Ecl::M,
        'Q' => Ecl::Q,
        _ => Ecl::H,
    }
}

fn rows(q: &Qr) -> Vec<u64> {
    (0..q.size()).map(|y| (0..q.size()).filter(|&x| q.get(x, y)).map(|x| 1u64 << x).sum()).collect()
}

/// The same symbol as qrencode with one of the 8 masks: the version, the
/// level, the codewords, the layout and the format bits all right. (The
/// mask chosen may differ: libqrencode's penalty is not the standard's;
/// any mask reads, the format bits say which.)
#[test]
fn same_as_qrencode() {
    for &(text, level, version, want) in vectors::VECTORS {
        let e = ecl(level);
        let masks: Vec<u8> = (0..8).filter(|&m| rows(&Qr::with(text.as_bytes(), version, e, Some(m)).unwrap()) == want).collect();
        assert_eq!(masks.len(), 1, "{text:?} at {level} v{version}: masks {masks:?}");
        // The smallest version for the level.
        assert_eq!((1..=MAX_VERSION).find(|&v| text.len() <= capacity(v, e)), Some(version));
    }
}

#[test]
fn capacities() {
    // The standard's table for byte mode.
    assert_eq!([capacity(1, Ecl::L), capacity(1, Ecl::M), capacity(1, Ecl::Q), capacity(1, Ecl::H)], [17, 14, 11, 7]);
    assert_eq!(capacity(3, Ecl::L), 53);
    assert_eq!(capacity(7, Ecl::M), 122);
    assert_eq!(capacity(10, Ecl::L), 271);
    assert_eq!(capacity(10, Ecl::H), 119);
}

#[test]
fn encode_raises_the_level() {
    // 14 bytes: version 1 at M, the best that fits; 18: version 2 at Q.
    let q = Qr::encode(b"0123456789abcd", Ecl::L).unwrap();
    assert_eq!((q.version, q.ecl), (1, Ecl::M));
    let q = Qr::encode(&[b'x'; 18], Ecl::L).unwrap();
    assert_eq!((q.version, q.ecl), (2, Ecl::Q));
    assert!(Qr::encode(&[0; 272], Ecl::L).is_none());
}

/// The mask chosen has the lowest penalty of the 8.
#[test]
fn lowest_penalty() {
    for &(text, level, version, _) in vectors::VECTORS {
        let q = Qr::with(text.as_bytes(), version, ecl(level), None).unwrap();
        let p = |m| {
            let t = Qr::with(text.as_bytes(), version, ecl(level), Some(m)).unwrap();
            t.penalty()
        };
        assert!((0..8).all(|m| p(q.mask) <= p(m)));
    }
}
