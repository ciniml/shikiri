//! Endpoints that apps bring (doc/53): four slots, endpoints 2 to 5, each
//! of one kind made of simple-matter's clusters. A slot belongs to an
//! owner, `APP/LABEL`, kept in the store so the app gets the same endpoint
//! number when it connects again (controllers know endpoints by number).

use core::cell::RefCell;

use crate::store::Store;
use simple_matter::dm::ServerCluster;
use simple_matter::dm::clusters::{
    BooleanStateCluster, DescriptorCluster, GroupsCluster, IdentifyCluster, OnOffCluster, RelativeHumidityMeasurementCluster,
    TemperatureMeasurementCluster,
};
use simple_matter::dm::meta::{ClusterId, DeviceType, EndpointId};
use simple_matter::groups::DefaultGroupStore;
use simple_matter::kvs::Kvs as _;

/// The first slot's endpoint, and how many slots.
pub const FIRST: u16 = 2;
pub const SLOTS: usize = 4;
/// `APP/LABEL`: the kernel's longest name, `/`, 8 bytes.
pub const OWNER_MAX: usize = t5_rt::t5a::MAX_NAME + 1 + LABEL_MAX;
pub const LABEL_MAX: usize = 8;
/// The store's key: per slot, the kind (0: empty), the owner's length and
/// the owner.
const KEY: &[u8] = b"t5-eps";
const RECORD: usize = 2 + OWNER_MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Light,
    Plug,
    Contact,
    Temp,
    Humidity,
}

static LIGHT_DT: &[DeviceType] = &[DeviceType::new(0x0100, 3)];
static PLUG_DT: &[DeviceType] = &[DeviceType::new(0x010A, 3)];
static CONTACT_DT: &[DeviceType] = &[DeviceType::new(0x0015, 1)];
static TEMP_DT: &[DeviceType] = &[DeviceType::new(0x0302, 2)];
static HUMIDITY_DT: &[DeviceType] = &[DeviceType::new(0x0307, 2)];
/// Server clusters, in order: Identify, Groups, On/Off, Descriptor.
static ONOFF_SERVERS: &[ClusterId] = &[ClusterId(0x0003), ClusterId(0x0004), ClusterId(0x0006), ClusterId(0x001D)];
static CONTACT_SERVERS: &[ClusterId] = &[ClusterId(0x0003), ClusterId(0x001D), ClusterId(0x0045)];
static TEMP_SERVERS: &[ClusterId] = &[ClusterId(0x0003), ClusterId(0x001D), ClusterId(0x0402)];
static HUMIDITY_SERVERS: &[ClusterId] = &[ClusterId(0x0003), ClusterId(0x001D), ClusterId(0x0405)];

impl Kind {
    const ALL: [Kind; 5] = [Kind::Light, Kind::Plug, Kind::Contact, Kind::Temp, Kind::Humidity];

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Light => "light",
            Kind::Plug => "plug",
            Kind::Contact => "contact",
            Kind::Temp => "temp",
            Kind::Humidity => "humidity",
        }
    }

    fn code(self) -> u8 {
        self as u8 + 1
    }

    fn from_code(c: u8) -> Option<Kind> {
        Kind::ALL.get((c as usize).checked_sub(1)?).copied()
    }

    pub fn device_types(self) -> &'static [DeviceType] {
        match self {
            Kind::Light => LIGHT_DT,
            Kind::Plug => PLUG_DT,
            Kind::Contact => CONTACT_DT,
            Kind::Temp => TEMP_DT,
            Kind::Humidity => HUMIDITY_DT,
        }
    }

    pub fn servers(self) -> &'static [ClusterId] {
        match self {
            Kind::Light | Kind::Plug => ONOFF_SERVERS,
            Kind::Contact => CONTACT_SERVERS,
            Kind::Temp => TEMP_SERVERS,
            Kind::Humidity => HUMIDITY_SERVERS,
        }
    }
}

enum Main {
    OnOff { onoff: OnOffCluster, groups: GroupsCluster<'static, 6, 8, 8> },
    Contact(BooleanStateCluster),
    Temp(TemperatureMeasurementCluster),
    Humidity(RelativeHumidityMeasurementCluster),
}

pub struct Slot {
    pub kind: Kind,
    owner: [u8; OWNER_MAX],
    owner_len: usize,
    identify: IdentifyCluster,
    desc: DescriptorCluster,
    main: Main,
    /// The On/Off state last told (events, the app).
    pub last_on: bool,
}

impl Slot {
    pub fn new(kind: Kind, ep: u16, owner: &[u8], groups: &'static RefCell<DefaultGroupStore>) -> Slot {
        let main = match kind {
            Kind::Light | Kind::Plug => Main::OnOff { onoff: OnOffCluster::new(), groups: GroupsCluster::new_shared(groups, ep) },
            Kind::Contact => Main::Contact(BooleanStateCluster::new()),
            Kind::Temp => Main::Temp(TemperatureMeasurementCluster::new(Some(-4000), Some(8500))),
            Kind::Humidity => Main::Humidity(RelativeHumidityMeasurementCluster::new(Some(0), Some(10000))),
        };
        let mut o = [0u8; OWNER_MAX];
        let n = owner.len().min(OWNER_MAX);
        o[..n].copy_from_slice(&owner[..n]);
        Slot {
            kind,
            owner: o,
            owner_len: n,
            identify: IdentifyCluster::new(),
            desc: DescriptorCluster::new(EndpointId(ep), kind.device_types(), kind.servers(), &[], &[]),
            main,
            last_on: false,
        }
    }

    pub fn owner(&self) -> &[u8] {
        &self.owner[..self.owner_len]
    }

    pub fn cluster(&self, cl: u32) -> Option<&dyn ServerCluster> {
        Some(match (cl, &self.main) {
            (0x0003, _) => &self.identify,
            (0x001D, _) => &self.desc,
            (0x0006, Main::OnOff { onoff, .. }) => onoff,
            (0x0004, Main::OnOff { groups, .. }) => groups,
            (0x0045, Main::Contact(c)) => c,
            (0x0402, Main::Temp(c)) => c,
            (0x0405, Main::Humidity(c)) => c,
            _ => return None,
        })
    }

    pub fn cluster_mut(&mut self, cl: u32) -> Option<&mut dyn ServerCluster> {
        Some(match (cl, &mut self.main) {
            (0x0003, _) => &mut self.identify,
            (0x001D, _) => &mut self.desc,
            (0x0006, Main::OnOff { onoff, .. }) => onoff,
            (0x0004, Main::OnOff { groups, .. }) => groups,
            (0x0045, Main::Contact(c)) => c,
            (0x0402, Main::Temp(c)) => c,
            (0x0405, Main::Humidity(c)) => c,
            _ => return None,
        })
    }

    /// The app's value: 0 or 1 for On/Off and contact; 0.01 units for the
    /// measurements, `None` for null.
    pub fn set(&mut self, v: Option<i32>) -> Result<(), &'static str> {
        match (&mut self.main, v) {
            (Main::OnOff { onoff, .. }, Some(v @ 0..=1)) => onoff.set(v == 1),
            (Main::Contact(c), Some(v @ 0..=1)) => c.set_state(v == 1),
            (Main::Temp(c), v) if v.is_none_or(|v| (-27315..=32767).contains(&v)) => c.set_measured(v.map(|v| v as i16)),
            (Main::Humidity(c), v) if v.is_none_or(|v| (0..=10000).contains(&v)) => c.set_measured(v.map(|v| v as u16)),
            _ => return Err("value out of range for the kind"),
        }
        Ok(())
    }

    pub fn on(&self) -> Option<bool> {
        match &self.main {
            Main::OnOff { onoff, .. } => Some(onoff.is_on()),
            _ => None,
        }
    }

    /// The value as the app gives it (`null` if none).
    pub fn value(&self) -> Option<i32> {
        match &self.main {
            Main::OnOff { onoff, .. } => Some(onoff.is_on() as i32),
            Main::Contact(c) => Some(c.state() as i32),
            Main::Temp(c) => c.measured().map(i32::from),
            Main::Humidity(c) => c.measured().map(i32::from),
        }
    }

    /// A contact sensor's StateChange not yet posted.
    pub fn take_state_change(&mut self) -> Option<bool> {
        match &mut self.main {
            Main::Contact(c) => c.take_state_change(),
            _ => None,
        }
    }

    pub fn tick(&mut self) {
        let identifying = self.identify.is_identifying();
        if let Main::OnOff { groups, .. } = &mut self.main {
            groups.set_identifying(identifying);
        }
    }
}

/// PartsList for each set of slots in use (bit i: endpoint FIRST + i):
/// endpoint 1, then the slots'. simple-matter's Descriptor keeps it as a
/// `&'static` slice (doc/53 §2).
static PARTS: [[EndpointId; 1 + SLOTS]; 1 << SLOTS] = parts_table();

const fn parts_table() -> [[EndpointId; 1 + SLOTS]; 1 << SLOTS] {
    let mut t = [[EndpointId(0); 1 + SLOTS]; 1 << SLOTS];
    let mut m = 0;
    while m < 1 << SLOTS {
        t[m][0] = EndpointId(1);
        let mut n = 1;
        let mut i = 0;
        while i < SLOTS {
            if m & 1 << i != 0 {
                t[m][n] = EndpointId(FIRST + i as u16);
                n += 1;
            }
            i += 1;
        }
        m += 1;
    }
    t
}

pub fn parts(slots: &[Option<Slot>; SLOTS]) -> &'static [EndpointId] {
    let mask = slots.iter().enumerate().fold(0, |m, (i, s)| if s.is_some() { m | 1 << i } else { m });
    &PARTS[mask][..1 + mask.count_ones() as usize]
}

/// The slots from the store.
pub fn load(kvs: &RefCell<Store>, groups: &'static RefCell<DefaultGroupStore>) -> [Option<Slot>; SLOTS] {
    let mut buf = [0u8; RECORD * SLOTS];
    let mut slots: [Option<Slot>; SLOTS] = Default::default();
    if let Ok(Some(n)) = kvs.borrow_mut().get(KEY, &mut buf)
        && n == buf.len()
    {
        for (i, r) in buf.chunks(RECORD).enumerate() {
            let len = (r[1] as usize).min(OWNER_MAX);
            if let Some(kind) = Kind::from_code(r[0]) {
                slots[i] = Some(Slot::new(kind, FIRST + i as u16, &r[2..2 + len], groups));
            }
        }
    }
    slots
}

pub fn save(kvs: &RefCell<Store>, slots: &[Option<Slot>; SLOTS]) -> bool {
    let mut buf = [0u8; RECORD * SLOTS];
    for (s, r) in slots.iter().zip(buf.chunks_mut(RECORD)) {
        if let Some(s) = s {
            r[0] = s.kind.code();
            r[1] = s.owner_len as u8;
            r[2..2 + s.owner_len].copy_from_slice(s.owner());
        }
    }
    kvs.borrow_mut().set(KEY, &buf).is_ok()
}
