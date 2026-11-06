//! Attributes the controller keeps subscribed to (`watch`), and their last
//! values (`values`), for a panel that shows nodes and must survive
//! restarts on either side: the list is in the store, `matter` subscribes
//! to each entry whenever the controller is free, again after a loss
//! (Core spec §8.5: the subscription's max interval plus a grace), and
//! from the start after a restart.

use core::cell::RefCell;

use simple_matter::kvs::Kvs;
use t5_rt::event;
use t5_rt::log::FmtBuf;

use crate::controller::{Op, Outcome, Report};
use crate::store::Store;

pub const MAX: usize = 8;
/// What a panel line holds of a value.
pub const VALUE: usize = 48;
/// Reports at least this often: a loss shows within it and 30 s more.
const MAX_INTERVAL_S: u16 = 20;
/// After a failed subscribe: 5, 10, 20, then every 30 s.
const RETRY_MS: [u64; 4] = [5_000, 10_000, 20_000, 30_000];
const KEY: &[u8] = b"t5-watch";
/// node u64, ep u16, cluster u32, attr u32 (little-endian).
const RECORD: usize = 18;

pub struct Watch {
    pub node: u64,
    pub ep: u16,
    pub cluster: u32,
    pub attr: u32,
    /// The live subscription's id.
    pub sub: Option<u32>,
    pub value: Option<FmtBuf<VALUE>>,
    /// When the last report came (ms).
    pub at: u64,
    retry_at: u64,
    failures: u8,
}

pub struct Watches {
    list: heapless::Vec<Watch, MAX>,
    /// The entry whose subscribe the controller is doing.
    pending: Option<usize>,
    kvs: &'static RefCell<Store>,
}

impl Watches {
    pub fn load(kvs: &'static RefCell<Store>) -> Watches {
        let mut w = Watches { list: heapless::Vec::new(), pending: None, kvs };
        let mut buf = [0u8; RECORD * MAX];
        if let Ok(Some(n)) = kvs.borrow_mut().get(KEY, &mut buf) {
            for r in buf[..n].chunks_exact(RECORD) {
                let node = u64::from_le_bytes(r[0..8].try_into().unwrap());
                let ep = u16::from_le_bytes([r[8], r[9]]);
                let cluster = u32::from_le_bytes(r[10..14].try_into().unwrap());
                let attr = u32::from_le_bytes(r[14..18].try_into().unwrap());
                let _ = w.list.push(Watch::new(node, ep, cluster, attr));
            }
        }
        event!(Info, "matter.watches", count = w.list.len() as u32);
        w
    }

    fn save(&self) {
        let mut buf = [0u8; RECORD * MAX];
        for (r, w) in buf.chunks_exact_mut(RECORD).zip(&self.list) {
            r[0..8].copy_from_slice(&w.node.to_le_bytes());
            r[8..10].copy_from_slice(&w.ep.to_le_bytes());
            r[10..14].copy_from_slice(&w.cluster.to_le_bytes());
            r[14..18].copy_from_slice(&w.attr.to_le_bytes());
        }
        if self.kvs.borrow_mut().set(KEY, &buf[..RECORD * self.list.len()]).is_err() {
            event!(Warn, "matter.save_failed", what = "watches");
        }
    }

    pub fn add(&mut self, node: u64, ep: u16, cluster: u32, attr: u32) -> Result<(), &'static str> {
        if self.find(node, ep, cluster, attr).is_some() {
            return Ok(());
        }
        self.list.push(Watch::new(node, ep, cluster, attr)).map_err(|_| "too many watches")?;
        self.save();
        Ok(())
    }

    /// The subscription itself runs out: the node is not told.
    pub fn remove(&mut self, node: u64, ep: u16, cluster: u32, attr: u32) -> Result<(), &'static str> {
        let i = self.find(node, ep, cluster, attr).ok_or("not watched")?;
        self.list.remove(i);
        self.pending = None;
        self.save();
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Watch> {
        self.list.iter()
    }

    fn find(&self, node: u64, ep: u16, cluster: u32, attr: u32) -> Option<usize> {
        self.list.iter().position(|w| w.node == node && w.ep == ep && w.cluster == cluster && w.attr == attr)
    }

    /// The next subscribe to start while the controller is free, if one is
    /// due.
    pub fn due(&mut self, now: u64) -> Option<(u64, Op)> {
        if self.pending.is_some() {
            return None;
        }
        let i = self.list.iter().position(|w| w.sub.is_none() && now >= w.retry_at)?;
        self.pending = Some(i);
        let w = &self.list[i];
        Some((w.node, Op::Subscribe { ep: w.ep, cluster: w.cluster, attr: w.attr, min_s: 0, max_s: MAX_INTERVAL_S }))
    }

    /// Whether a controller outcome is the answer to `due`'s subscribe.
    pub fn waiting(&self) -> bool {
        self.pending.is_some()
    }

    /// `due` could not start: try that entry again later.
    pub fn not_started(&mut self, now: u64) {
        if let Some(i) = self.pending.take() {
            self.failed(i, now);
        }
    }

    pub fn outcome(&mut self, o: Outcome, now: u64) {
        let Some(i) = self.pending.take() else { return };
        match o {
            Outcome::Subscribed { id, .. } => {
                let w = &mut self.list[i];
                event!(Info, "matter.watching", node = w.node, ep = w.ep as u32, cluster = w.cluster, attr = w.attr, id = id);
                w.sub = Some(id);
                w.failures = 0;
            }
            o => {
                if let Outcome::Failed { why, status } = o {
                    event!(Info, "matter.watch_failed", node = self.list[i].node, why = why, status = status as u32);
                }
                self.failed(i, now)
            }
        }
    }

    fn failed(&mut self, i: usize, now: u64) {
        let w = &mut self.list[i];
        w.retry_at = now + RETRY_MS[(w.failures as usize).min(RETRY_MS.len() - 1)];
        w.failures = w.failures.saturating_add(1);
        event!(Info, "matter.watch_retry", node = w.node, failures = w.failures as u32);
    }

    /// A report: the entry of its node and path.
    pub fn report(&mut self, r: &Report, now: u64) {
        let i = self.find(r.node, r.ep, r.cluster, r.attr);
        if let Some(i) = i {
            let w = &mut self.list[i];
            let mut v = FmtBuf::<VALUE>::new();
            let _ = core::fmt::Write::write_str(&mut v, r.value.as_str());
            w.value = Some(v);
            w.at = now;
        }
    }

    /// A subscription ended: subscribe again at once.
    pub fn lost(&mut self, id: u32, now: u64) {
        for w in self.list.iter_mut().filter(|w| w.sub == Some(id)) {
            w.sub = None;
            w.retry_at = now;
        }
    }

    /// A node was forgotten: its entries go.
    pub fn forget(&mut self, node: u64) {
        let before = self.list.len();
        self.list.retain(|w| w.node != node);
        if self.list.len() != before {
            self.pending = None;
            self.save();
        }
    }

    pub fn next_deadline(&self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.list.iter().filter(|w| w.sub.is_none()).map(|w| w.retry_at).min()
    }
}

impl Watch {
    fn new(node: u64, ep: u16, cluster: u32, attr: u32) -> Watch {
        Watch { node, ep, cluster, attr, sub: None, value: None, at: 0, retry_at: 0, failures: 0 }
    }
}
