//! The app store in flash (`t5_abi::store`, doc/14 §2), read through the
//! window the platform maps. Images are verified when they are loaded.

use t5_abi::store::{self, Catalog, Entry};
use t5_abi::{event, log::Addr};

pub struct Store {
    window: usize,
    catalog: Option<Catalog<'static>>,
}

impl Store {
    /// `window` is the address of flash offset 0.
    pub fn open(window: usize) -> Store {
        let sector = |i: usize| unsafe {
            core::slice::from_raw_parts((window + store::APPMETA_OFFSET as usize + i * store::SECTOR_SIZE) as *const u8, store::SECTOR_SIZE)
        };
        let catalog = store::active(sector(0), sector(1));
        match &catalog {
            Some(c) => event!(Info, "store.open", window = Addr(window as u32), catalog_seq = c.seq, apps = c.count as u32),
            None => event!(Warn, "store.empty", window = Addr(window as u32)),
        }
        Store { window, catalog }
    }

    pub fn entries(&self) -> impl Iterator<Item = Entry> + 'static {
        self.catalog.into_iter().flat_map(|c| c.entries())
    }

    /// The image bytes of the app named `name` (not yet verified).
    pub fn image(&self, name: &str) -> Option<&'static [u8]> {
        let e = self.entries().find(|e| e.name() == name)?;
        Some(unsafe { core::slice::from_raw_parts((self.window + e.offset() as usize) as *const u8, e.length as usize) })
    }

    pub fn log_contents(&self) {
        for e in self.entries() {
            event!(
                Info,
                "store.app",
                name = e.name(),
                major = e.version[0],
                minor = e.version[1],
                patch = e.version[2],
                offset = Addr(e.offset()),
                length = e.length,
                state = e.state as u8,
                flags = e.flags
            );
        }
    }
}
