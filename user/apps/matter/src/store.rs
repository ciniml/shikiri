//! The store: the SD card's (`mtport::FileKvs`), or, for the emulator's
//! test (doc/49 §16: no SD card there), one in memory that the process
//! forgets when it ends.

use mtport::FileKvs;
use simple_matter::error::{Error, Result};
use simple_matter::kvs::Kvs;

pub enum Store {
    File(FileKvs),
    Mem(MemKvs),
}

impl Kvs for Store {
    fn get(&mut self, key: &[u8], buf: &mut [u8]) -> Result<Option<usize>> {
        match self {
            Store::File(f) => f.get(key, buf),
            Store::Mem(m) => m.get(key, buf),
        }
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        match self {
            Store::File(f) => f.set(key, value),
            Store::Mem(m) => m.set(key, value),
        }
    }

    fn remove(&mut self, key: &[u8]) -> Result<()> {
        match self {
            Store::File(f) => f.remove(key),
            Store::Mem(m) => m.remove(key),
        }
    }
}

/// Records `[key length: u8][value length: u16 LE][key][value]`, one after
/// another.
pub struct MemKvs {
    data: heapless::Vec<u8, 16384>,
}

impl MemKvs {
    pub const fn new() -> MemKvs {
        MemKvs { data: heapless::Vec::new() }
    }

    /// The record of `key`: its start, the value's start and end.
    fn find(&self, key: &[u8]) -> Option<(usize, usize, usize)> {
        let mut at = 0;
        while at + 3 <= self.data.len() {
            let k = self.data[at] as usize;
            let v = u16::from_le_bytes([self.data[at + 1], self.data[at + 2]]) as usize;
            let value = at + 3 + k;
            if &self.data[at + 3..value] == key {
                return Some((at, value, value + v));
            }
            at = value + v;
        }
        None
    }

    fn get(&mut self, key: &[u8], buf: &mut [u8]) -> Result<Option<usize>> {
        let Some((_, v, end)) = self.find(key) else { return Ok(None) };
        let n = end - v;
        buf.get_mut(..n).ok_or(Error::NoSpace)?.copy_from_slice(&self.data[v..end]);
        Ok(Some(n))
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > u8::MAX as usize || value.len() > u16::MAX as usize {
            return Err(Error::NoSpace);
        }
        self.remove(key)?;
        if self.data.len() + 3 + key.len() + value.len() > self.data.capacity() {
            return Err(Error::NoSpace);
        }
        let _ = self.data.push(key.len() as u8);
        let _ = self.data.extend_from_slice(&(value.len() as u16).to_le_bytes());
        let _ = self.data.extend_from_slice(key);
        let _ = self.data.extend_from_slice(value);
        Ok(())
    }

    fn remove(&mut self, key: &[u8]) -> Result<()> {
        if let Some((start, _, end)) = self.find(key) {
            self.data.copy_within(end.., start);
            self.data.truncate(self.data.len() - (end - start));
        }
        Ok(())
    }
}
