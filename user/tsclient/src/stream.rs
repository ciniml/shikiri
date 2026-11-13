// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! A read buffer under the protocols, over TLS: reads with a whole
//! record's room, so that TLS itself never keeps decrypted bytes. What is
//! waiting is then here or still on the TCP connection (embedded-tls
//! reads a record's header and body exactly, never ahead), which lets an
//! event loop serve several connections without blocking on an idle one.

use alloc::boxed::Box;

/// TLS's most plaintext in a record.
const ROOM: usize = 16384;

pub struct Buffered<S> {
    pub io: S,
    buf: Box<[u8; ROOM]>,
    pos: usize,
    len: usize,
}

impl<S> Buffered<S> {
    pub fn new(io: S) -> Buffered<S> {
        Buffered { io, buf: Box::new([0; ROOM]), pos: 0, len: 0 }
    }

    /// Bytes read from TLS and not yet taken.
    pub fn buffered(&self) -> bool {
        self.pos < self.len
    }
}

impl<S: embedded_io::ErrorType> embedded_io::ErrorType for Buffered<S> {
    type Error = S::Error;
}

impl<S: embedded_io::Read> embedded_io::Read for Buffered<S> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, S::Error> {
        if self.pos == self.len {
            self.len = self.io.read(&mut self.buf[..])?;
            self.pos = 0;
        }
        let n = out.len().min(self.len - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl<S: embedded_io::Write> embedded_io::Write for Buffered<S> {
    fn write(&mut self, data: &[u8]) -> Result<usize, S::Error> {
        self.io.write(data)
    }

    fn flush(&mut self) -> Result<(), S::Error> {
        self.io.flush()
    }
}
