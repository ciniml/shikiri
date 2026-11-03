//! The device role (doc/49 §5): simple-matter's core put together for an
//! On/Off light, as `simple-matter-cffi` does it (its `Light`, `Owned`,
//! `Shim::housekeep`), without composition, custom clusters, BLE or
//! Thread. Endpoint 0 has the node's clusters, endpoint 1 Identify,
//! Groups, On/Off and Descriptor.

use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::net::{Ipv4Addr, Ipv6Addr};
use core::num::NonZeroU8;
use core::ptr::addr_of_mut;

use mtport::{FileKvs, KernelRng};
use simple_matter::acl::{AclHandle, AclTable};
use simple_matter::crypto::rustcrypto::RustCrypto;
use simple_matter::discovery::{Commissionable, CommissioningMode, Host, MATTER_PORT, MdnsResponder, Operational};
use simple_matter::dm::clusters::{
    AccessControlCluster, AdminCommissioningCluster, BasicInfoConfig, BasicInformationCluster, CommissioningWindow,
    DescriptorCluster, GeneralCommissioning, GroupKeyManagementCluster, GroupsCluster, IdentifyCluster,
    NetworkCommissioning, OnOffCluster, OpCredsCluster, TestDacProvider, WindowEvent,
};
use simple_matter::dm::meta::{ClusterId, DeviceType, EndpointId, EndpointMeta, EventId};
use simple_matter::dm::{DataModel, ServerCluster, tick_clusters};
use simple_matter::fabric::FabricTable;
use simple_matter::groups::DefaultGroupStore;
use simple_matter::im::engine::InteractionModel;
use simple_matter::im::events::PRIORITY_INFO;
use simple_matter::sc::SecureChannel;
use simple_matter::stack::{MultiAdminStack, SendDirective, SharedFabricCreds};
use simple_matter::tlv::TlvTag;
use simple_matter::transport::net::PeerAddr;
use t5_rt::event;

/// Fabrics, ACL entries (5 fabrics × 4).
const NF: usize = 5;
const NACL: usize = 20;

type Backend = RustCrypto<KernelRng>;
type OpCreds = OpCredsCluster<Backend, TestDacProvider<Backend>, NF, &'static RefCell<FabricTable<Backend, NF>>>;
/// Several controllers at once (Apple, Google, the user's own), as the
/// cffi's device.
type Stack = MultiAdminStack<'static, Backend, KernelRng, Light>;

static EP0_SERVERS: &[ClusterId] = &[
    ClusterId(0x001F), // Access Control
    ClusterId(0x0028), // Basic Information
    ClusterId(0x0030), // General Commissioning
    ClusterId(0x0031), // Network Commissioning
    ClusterId(0x003C), // Administrator Commissioning
    ClusterId(0x003E), // Operational Credentials
    ClusterId(0x003F), // Group Key Management
    ClusterId(0x001D), // Descriptor
];
static EP1_SERVERS: &[ClusterId] = &[ClusterId(0x0003), ClusterId(0x0004), ClusterId(0x0006), ClusterId(0x001D)];
/// Root node; On/Off light.
static EP0_DT: &[DeviceType] = &[DeviceType::new(0x0016, 1)];
static EP1_DT: &[DeviceType] = &[DeviceType::new(0x0100, 3)];
static EP0_PARTS: &[EndpointId] = &[EndpointId(1)];
static ENDPOINTS: &[EndpointMeta] = &[
    EndpointMeta { id: EndpointId(0), device_types: EP0_DT, clusters: EP0_SERVERS },
    EndpointMeta { id: EndpointId(1), device_types: EP1_DT, clusters: EP1_SERVERS },
];

pub struct Light {
    acl: &'static RefCell<AclTable<NACL>>,
    access_control: AccessControlCluster<'static, NACL>,
    basic: BasicInformationCluster,
    gc: GeneralCommissioning,
    net: NetworkCommissioning,
    admin: AdminCommissioningCluster<'static>,
    opcreds: OpCreds,
    gkm: GroupKeyManagementCluster<'static, Backend, NF, 6, 8, 8>,
    desc0: DescriptorCluster,
    identify: IdentifyCluster,
    groups_cl: GroupsCluster<'static, 6, 8, 8>,
    onoff: OnOffCluster,
    desc1: DescriptorCluster,
    groups: &'static RefCell<DefaultGroupStore>,
    removed_fabric: Option<NonZeroU8>,
}

impl DataModel for Light {
    fn endpoints(&self) -> &[EndpointMeta] {
        ENDPOINTS
    }

    fn clusters_on(&self, ep: EndpointId) -> &[ClusterId] {
        ENDPOINTS.iter().find(|m| m.id == ep).map_or(&[], |m| m.clusters)
    }

    fn cluster(&self, ep: EndpointId, cl: ClusterId) -> Option<&dyn ServerCluster> {
        Some(match (ep.0, cl.0) {
            (0, 0x001F) => &self.access_control,
            (0, 0x0028) => &self.basic,
            (0, 0x0030) => &self.gc,
            (0, 0x0031) => &self.net,
            (0, 0x003C) => &self.admin,
            (0, 0x003E) => &self.opcreds,
            (0, 0x003F) => &self.gkm,
            (0, 0x001D) => &self.desc0,
            (1, 0x0003) => &self.identify,
            (1, 0x0004) => &self.groups_cl,
            (1, 0x0006) => &self.onoff,
            (1, 0x001D) => &self.desc1,
            _ => return None,
        })
    }

    fn cluster_mut(&mut self, ep: EndpointId, cl: ClusterId) -> Option<&mut dyn ServerCluster> {
        Some(match (ep.0, cl.0) {
            (0, 0x001F) => &mut self.access_control,
            (0, 0x0028) => &mut self.basic,
            (0, 0x0030) => &mut self.gc,
            (0, 0x0031) => &mut self.net,
            (0, 0x003C) => &mut self.admin,
            (0, 0x003E) => &mut self.opcreds,
            (0, 0x003F) => &mut self.gkm,
            (0, 0x001D) => &mut self.desc0,
            (1, 0x0003) => &mut self.identify,
            (1, 0x0004) => &mut self.groups_cl,
            (1, 0x0006) => &mut self.onoff,
            (1, 0x001D) => &mut self.desc1,
            _ => return None,
        })
    }

    fn on_tick(&mut self, now_ms: u64) -> Option<u64> {
        if self.gc.on_tick(now_ms)
            && let Some(idx) = self.opcreds.on_failsafe_expired()
        {
            self.removed_fabric = Some(idx);
        }
        let _ = self.admin.on_tick(now_ms);
        let next = tick_clusters(self, now_ms);
        let identifying = self.identify.is_identifying();
        self.groups_cl.set_identifying(identifying);
        next
    }

    fn group_endpoints(&self, fabric: NonZeroU8, group_id: u16, idx: usize) -> Option<EndpointId> {
        let store = self.groups.borrow();
        store.member_endpoints(fabric, group_id)?.get(idx).map(|&e| EndpointId(e))
    }

    fn on_failsafe_cleanup(&mut self) -> Option<NonZeroU8> {
        self.gc.disarm();
        self.opcreds.on_failsafe_expired()
    }

    fn on_commissioning_complete(&mut self) {
        self.opcreds.on_commissioning_complete();
    }

    fn take_removed_fabric(&mut self) -> Option<NonZeroU8> {
        self.removed_fabric.take()
    }

    fn acl(&self) -> Option<&dyn AclHandle> {
        Some(self.acl)
    }
}

/// What the stack and the clusters borrow for the life of the process.
struct Owned {
    crypto: Backend,
    fabrics: RefCell<FabricTable<Backend, NF>>,
    acl: RefCell<AclTable<NACL>>,
    window: RefCell<CommissioningWindow>,
    groups: RefCell<DefaultGroupStore>,
    basic: BasicInfoConfig,
}

static mut OWNED: MaybeUninit<Owned> = MaybeUninit::uninit();
/// The device itself (26 KB): in static memory, not on the stack, which
/// the stack's deep calls (CASE, P-256) need (doc/49 §5).
static mut DEVICE: MaybeUninit<Device> = MaybeUninit::uninit();

/// The node's identity: what the advertisements and Basic Information say.
pub struct Identity {
    pub mac: [u8; 6],
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub product_name: &'static str,
    pub serial_number: &'static str,
}

/// Something that changed for the service to log.
pub enum Change {
    OnOff(bool),
    Commissioned(usize),
    FabricRemoved(usize),
    Window(bool),
}

/// The device: the stack, the mDNS responder, and what must be saved or
/// advertised again when it changes.
pub struct Device {
    o: &'static Owned,
    stack: Stack,
    mdns: MdnsResponder<NF>,
    kvs: FileKvs,
    id: Identity,
    instance_id: u64,
    ipv4: Option<[u8; 4]>,
    ipv6_ll: Option<[u8; 16]>,
    boot_window_open: bool,
    last_fabric_gen: u32,
    last_group_gen: u32,
    last_resumption_gen: u32,
    last_fabric_count: usize,
    last_on: bool,
}

impl Device {
    /// Builds the node in static memory, restores fabrics, ACL, groups and
    /// CASE resumption from `kvs`, and starts advertising (commissionable
    /// until it has a fabric). Only once per process.
    pub fn init(id: Identity, mut kvs: FileKvs, now: u64) -> Result<&'static mut Device, &'static str> {
        let basic = BasicInfoConfig {
            vendor_name: "tab5-os",
            vendor_id: id.vendor_id,
            product_name: id.product_name,
            product_id: id.product_id,
            hardware_version: 1,
            hardware_version_string: "Tab5",
            software_version: 0x0001_0000,
            software_version_string: "0.1.0",
            serial_number: id.serial_number,
        };
        let o: &'static Owned = unsafe {
            (*addr_of_mut!(OWNED)).write(Owned {
                crypto: RustCrypto::new(KernelRng),
                fabrics: RefCell::new(FabricTable::new()),
                acl: RefCell::new(AclTable::new()),
                window: RefCell::new(CommissioningWindow::new()),
                groups: RefCell::new(DefaultGroupStore::new()),
                basic,
            })
        };
        let dac = TestDacProvider::new(&o.crypto).map_err(|_| "test DAC")?;
        let light = Light {
            acl: &o.acl,
            access_control: AccessControlCluster::new(&o.acl),
            basic: BasicInformationCluster::new(&o.basic),
            gc: GeneralCommissioning::default_config(),
            net: NetworkCommissioning::new(b"wlan0"),
            admin: AdminCommissioningCluster::new(&o.window),
            opcreds: OpCredsCluster::new_shared(&o.fabrics, RustCrypto::new(KernelRng), dac),
            gkm: GroupKeyManagementCluster::new_shared(&o.groups, &o.fabrics, RustCrypto::new(KernelRng)),
            desc0: DescriptorCluster::new(EndpointId(0), EP0_DT, EP0_SERVERS, &[], EP0_PARTS),
            identify: IdentifyCluster::new(),
            groups_cl: GroupsCluster::new_shared(&o.groups, 1),
            onoff: OnOffCluster::new(),
            desc1: DescriptorCluster::new(EndpointId(1), EP1_DT, EP1_SERVERS, &[], &[]),
            groups: &o.groups,
            removed_fabric: None,
        };
        // The development passcode's verifier (20202021), precomputed.
        let pase = simple_matter::dev_pase::dev_pase_config();
        let creds = SharedFabricCreds::new(&o.fabrics, &o.crypto, 0);
        let sc = SecureChannel::new(&o.crypto, KernelRng, pase, creds);
        let mut stack: Stack = Stack::new(&o.crypto, sc, InteractionModel::new(light));
        stack.set_group_keys(&o.groups);
        let _ = stack.post_startup_event(o.basic.software_version, 0);
        stack.device_mut().opcreds.pregenerate_keypair();

        let _ = o.fabrics.borrow_mut().load_from(&mut kvs, &o.crypto, 0);
        let _ = o.acl.borrow_mut().load_from(&mut kvs);
        let _ = o.groups.borrow_mut().load_from(&mut kvs);
        let _ = stack.load_resumptions_from(&mut kvs);
        let count = o.fabrics.borrow().len();
        if count > 0 {
            stack.set_pase_enabled(false);
        }
        let mut instance = [0u8; 8];
        instance[2..].copy_from_slice(&id.mac);
        let d = unsafe { (*addr_of_mut!(DEVICE)).write(Device {
            o,
            mdns: MdnsResponder::new(Host::from_mac(&id.mac, None, None), MATTER_PORT),
            last_fabric_gen: o.fabrics.borrow().generation(),
            last_group_gen: o.groups.borrow().generation(),
            last_resumption_gen: stack.resumption_generation(),
            last_on: stack.device().onoff.is_on(),
            stack,
            kvs,
            instance_id: u64::from_be_bytes(instance),
            id,
            ipv4: None,
            ipv6_ll: None,
            boot_window_open: count == 0,
            last_fabric_count: count,
        }) };
        d.rebuild_mdns(now);
        Ok(d)
    }

    pub fn fabrics(&self) -> usize {
        self.o.fabrics.borrow().len()
    }

    pub fn on(&self) -> bool {
        self.stack.device().onoff.is_on()
    }

    /// A Matter datagram from `peer`; the reply, if any.
    pub fn handle_rx(&mut self, dg: &mut [u8], peer: PeerAddr, now: u64, tx: &mut [u8]) -> Option<SendDirective> {
        self.stack.handle_rx(dg, peer, now, tx)
    }

    /// Retransmissions, acknowledgements, reports: one datagram at a time.
    pub fn poll(&mut self, now: u64, tx: &mut [u8]) -> Option<SendDirective> {
        self.stack.poll(now, tx)
    }

    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        let s = self.stack.next_deadline(now).unwrap_or(u64::MAX);
        let m = self.mdns.next_announce_deadline();
        let d = s.min(m);
        (d != u64::MAX).then_some(d)
    }

    /// An mDNS packet: the answer's length and whether it goes back to the
    /// sender only (a QU question).
    pub fn mdns_rx(&mut self, pkt: &[u8], tx: &mut [u8]) -> Option<(usize, bool)> {
        let n = self.mdns.handle_query(pkt, tx)?;
        Some((n, self.mdns.query_wants_unicast(pkt)))
    }

    /// An mDNS announcement due now, if any.
    pub fn mdns_poll(&mut self, now: u64, tx: &mut [u8]) -> Option<usize> {
        self.mdns.poll_announce(now, tx)
    }

    /// The Wi-Fi link's addresses (the A and AAAA records).
    pub fn set_addrs(&mut self, ipv4: Option<[u8; 4]>, ipv6_ll: Option<[u8; 16]>, now: u64) {
        if (ipv4, ipv6_ll) != (self.ipv4, self.ipv6_ll) {
            self.ipv4 = ipv4;
            self.ipv6_ll = ipv6_ll;
            self.rebuild_mdns(now);
        }
    }

    fn commissionable(&self, discriminator: u16, mode: CommissioningMode) -> Commissionable {
        Commissionable {
            device_type: Some(0x0100),
            device_name: Some(self.o.basic.product_name),
            sii: None,
            sai: None,
            ..Commissionable::new(self.instance_id, discriminator, self.id.vendor_id, self.id.product_id, mode)
        }
    }

    fn refresh_operational(&mut self) {
        let fb = self.o.fabrics.borrow();
        self.mdns.set_operational(fb.iter().map(|f| Operational::new(f.compressed_fabric_id(), f.node_id())));
    }

    fn rebuild_mdns(&mut self, now: u64) {
        let host = Host::from_mac(&self.id.mac, self.ipv6_ll.map(Ipv6Addr::from), self.ipv4.map(Ipv4Addr::from));
        self.mdns = MdnsResponder::new(host, MATTER_PORT);
        if self.o.fabrics.borrow().len() == 0 && self.boot_window_open {
            let ad = self.commissionable(self.id.discriminator, CommissioningMode::Standard);
            self.mdns.set_commissionable(Some(ad));
        } else {
            self.refresh_operational();
        }
        self.mdns.notify_change(now);
    }

    fn set_commissionable(&mut self, discriminator: Option<(u16, CommissioningMode)>, now: u64) {
        let ad = discriminator.map(|(d, m)| self.commissionable(d, m));
        self.mdns.set_commissionable(ad);
        self.mdns.notify_change(now);
    }

    /// After each datagram and poll (cffi's `housekeep`): On/Off events,
    /// saving what changed, the advertisements, the commissioning window.
    pub fn housekeep(&mut self, now: u64, mut changed: impl FnMut(Change)) {
        let on = self.on();
        if on != self.last_on {
            self.last_on = on;
            let _ = self.stack.post_event(EndpointId(1), ClusterId(0x0006), EventId(0), PRIORITY_INFO, now, |w, tag| {
                w.start_struct(tag)?;
                w.write_bool(&TlvTag::ContextSpecific(0), on)?;
                w.end_container()
            });
            changed(Change::OnOff(on));
        }

        let generation = self.o.fabrics.borrow().generation();
        if generation != self.last_fabric_gen {
            self.last_fabric_gen = generation;
            let saved = self.o.fabrics.borrow().save_to(&mut self.kvs).and(self.o.acl.borrow().save_to(&mut self.kvs));
            if saved.is_err() {
                event!(Warn, "matter.save_failed", what = "fabrics");
            }
            self.refresh_operational();
            self.mdns.notify_change(now);
            let count = self.fabrics();
            if count > self.last_fabric_count {
                changed(Change::Commissioned(count));
                if self.o.window.borrow().is_open() {
                    self.o.window.borrow_mut().close_window();
                }
            } else if count < self.last_fabric_count {
                changed(Change::FabricRemoved(count));
            }
            let window_open = self.o.window.borrow().is_open();
            if self.boot_window_open && count > 0 && !window_open {
                self.boot_window_open = false;
                self.stack.set_pase_enabled(false);
                self.set_commissionable(None, now);
            } else if !self.boot_window_open && count == 0 && !window_open {
                // The last fabric went: commissionable again.
                self.boot_window_open = true;
                self.stack.device_mut().opcreds.pregenerate_keypair();
                self.stack.set_pase_config(simple_matter::dev_pase::dev_pase_config());
                self.stack.set_pase_enabled(true);
                self.set_commissionable(Some((self.id.discriminator, CommissioningMode::Standard)), now);
            }
            self.last_fabric_count = count;
        }

        let generation = self.o.groups.borrow().generation();
        if generation != self.last_group_gen {
            self.last_group_gen = generation;
            if self.o.groups.borrow().save_to(&mut self.kvs).is_err() {
                event!(Warn, "matter.save_failed", what = "groups");
            }
        }
        let generation = self.stack.resumption_generation();
        if generation != self.last_resumption_gen {
            self.last_resumption_gen = generation;
            if self.stack.save_resumptions_to(&mut self.kvs).is_err() {
                event!(Warn, "matter.save_failed", what = "resumption");
            }
        }

        // The commissioning window (opened by a controller, closed, timed out).
        let window = self.o.window.borrow_mut().take_event();
        if let Some(ev) = window {
            match ev {
                WindowEvent::OpenedEnhanced { discriminator } => {
                    self.stack.device_mut().opcreds.pregenerate_keypair();
                    let pase = self.o.window.borrow().pase_config();
                    if let Some(pase) = pase {
                        self.stack.set_pase_config(pase);
                        self.stack.set_pase_enabled(true);
                        self.set_commissionable(Some((discriminator, CommissioningMode::Enhanced)), now);
                        changed(Change::Window(true));
                    }
                }
                WindowEvent::OpenedBasic => {
                    self.stack.device_mut().opcreds.pregenerate_keypair();
                    self.stack.set_pase_config(simple_matter::dev_pase::dev_pase_config());
                    self.stack.set_pase_enabled(true);
                    self.set_commissionable(Some((self.id.discriminator, CommissioningMode::Standard)), now);
                    changed(Change::Window(true));
                }
                WindowEvent::Closed => {
                    self.stack.set_pase_enabled(false);
                    self.set_commissionable(None, now);
                    changed(Change::Window(false));
                }
            }
            let admin = self.o.window.borrow().admin_fabric_index();
            if let Some(idx) = admin {
                let vid = self.o.fabrics.borrow().get(idx).map(|f| f.vendor_id());
                if let Some(vid) = vid {
                    self.o.window.borrow_mut().set_admin_vendor_id(vid);
                }
            }
        }
    }
}
