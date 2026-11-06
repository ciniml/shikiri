//! The controller role (doc/49 MT2): simple-matter's `ControllerStack`
//! and `Commissioner`, as `simple-matter-cffi`'s controller drives them
//! (its `CtrlShim`, `pump`, `start_operation`), cut down to what the shell
//! asks for: find a commissionable device by discriminator over mDNS,
//! commission it on the network, then commands and attribute reads over
//! CASE (resumed when it can). Or commission it over BLE (MT4, `blelink`
//! finds it and carries the messages): PASE to AddNOC over BLE, then the
//! node's operational address from mDNS and CASE over the network.
//! Commands take arguments (context tags 0, 1, …), attributes can be
//! written, and subscriptions report into the log. One request at a time. The CA, the node list and each node's CASE
//! resumption are kept in the store.

use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::net::SocketAddr;
use core::ptr::addr_of_mut;

use mtport::KernelRng;

use crate::store::Store;
use simple_matter::controller::ca::{CA_STATE_MAX_LEN, Ca};
use simple_matter::controller::nodes as nodes_codec;
use simple_matter::controller::{
    AttestationPolicy, CONTROLLER_FABRIC_INDEX, Commissioner, ControllerCreds, ControllerStack, OpenWindowParams, Phase,
};
use simple_matter::crypto::rustcrypto::RustCrypto;
use simple_matter::discovery::MATTER_PORT;
use simple_matter::discovery::client::MdnsClient;
use simple_matter::dm::meta::{AttributeId, ClusterId, CommandId, EndpointId, EventId};
use simple_matter::im::client::{ImClient, ImEvent};
use simple_matter::im::wire::{AttributePath, AttributeReportRef, CommandPath, EventPath, EventReportRef, ImStatus};
use simple_matter::kvs::Kvs;
use simple_matter::sc::case::common::{CASE_RESUMPTION_ID_LEN, SHARED_SECRET_LEN};
use simple_matter::sc::initiator::{ScEvent, ScInitiator};
use simple_matter::stack::{MAX_PACKET_SIZE, SendDirective};
use simple_matter::tlv::{TlvTag, TlvValue, TlvWriter};
use simple_matter::transport::net::{BtpConnId, PeerAddr};
use simple_matter::transport::session::SessionId;
use t5_rt::event;
use t5_rt::log::FmtBuf;

type Backend = RustCrypto<KernelRng>;
type Creds = ControllerCreds<'static, Backend>;
/// Sessions, exchanges, transmit buffers, result bytes (cffi's sizing).
type Stack = ControllerStack<'static, Backend, KernelRng, Creds, 6, 8, 3, 1280>;
type Comm = Commissioner<'static, Backend>;

const MAX_NODES: usize = 8;
const TX_QUEUE: usize = 4;
/// This controller's fabric (generated once, then from the store).
const VENDOR: u16 = 0xFFF1;
const CONTROLLER_NODE: u64 = 0x0000_0000_7AB5_0001;
/// How long discovery looks, and how often it asks again.
const DISCOVER_MS: u64 = 10_000;
const DISCOVER_EVERY_MS: u64 = 1000;
/// A search by short discriminator, through every candidate (PASE gives
/// up on a silent one after 60 s).
const SEARCH_MS: u64 = 200_000;
/// Over BLE: how long PASE to AddNOC may take.
const BLE_PAIR_MS: u64 = 90_000;
const BLE: PeerAddr = PeerAddr::Ble(BtpConnId(0));
/// CASE resumption record: version, resumption ID, shared secret.
const RESUMPTION_VERSION: u8 = 1;
const RESUMPTION_LEN: usize = 1 + CASE_RESUMPTION_ID_LEN + SHARED_SECRET_LEN;

/// A value for an argument or an attribute.
#[derive(Clone, Copy, Debug)]
pub enum Val {
    U(u64),
    I(i64),
    B(bool),
    Null,
}

impl Val {
    /// `true`, `false`, `null`, `-N`, or a number (decimal or 0x hex).
    pub fn parse(s: &str) -> Option<Val> {
        Some(match s {
            "true" => Val::B(true),
            "false" => Val::B(false),
            "null" => Val::Null,
            _ => match s.strip_prefix('-') {
                Some(n) => Val::I(-(n.parse::<i64>().ok()?)),
                None => Val::U(match s.strip_prefix("0x") {
                    Some(h) => u64::from_str_radix(h, 16).ok()?,
                    None => s.parse().ok()?,
                }),
            },
        })
    }

    fn write(self, w: &mut TlvWriter<'_>, t: &TlvTag) -> simple_matter::error::Result<()> {
        match self {
            Val::U(u) => w.write_u64(t, u),
            Val::I(i) => w.write_i64(t, i),
            Val::B(b) => w.write_bool(t, b),
            Val::Null => w.write_null(t),
        }
    }
}

/// A command's arguments, at most.
pub const ARGS: usize = 6;
/// A subscription's paths, at most.
pub const SUB_PATHS: usize = 4;

#[derive(Clone, Copy)]
pub enum Op {
    Invoke { ep: u16, cluster: u32, cmd: u32, args: [Val; ARGS], n: usize },
    Read { ep: u16, cluster: u32, attr: u32 },
    Write { ep: u16, cluster: u32, attr: u32, val: Val },
    /// Reports go to the log (`matter.report`) until the subscription is
    /// lost or the process ends. Several paths (endpoint, cluster,
    /// attribute) in one: AirQ takes few subscriptions (doc/49 §25).
    Subscribe { paths: [(u16, u32, u32); SUB_PATHS], n: u8, min_s: u16, max_s: u16 },
    /// Events go to the log (`matter.event`), as reports do (doc/49 §22).
    SubscribeEvent { ep: u16, cluster: u32, event: u32, min_s: u16, max_s: u16 },
    /// AdministratorCommissioning: open an enhanced window (OCW), revoke it.
    OpenWindow(OpenWindowParams),
    Revoke,
}

/// The end of a request, for the client that asked.
pub enum Outcome {
    Commissioned { node: u64, ms: u64 },
    PairFailed { stage: u8, reason: &'static str },
    NotFound,
    Invoked { status: u8 },
    Read { value: u64, null: bool },
    /// A value that is not a number: as text (`fmt_value`).
    ReadText(FmtBuf<VALUE_TEXT>),
    Subscribed { id: u32, max_s: u16 },
    Failed { why: &'static str, status: u8 },
}

/// A subscription's attribute report, for `watch`.
pub struct Report {
    pub node: u64,
    pub ep: u16,
    pub cluster: u32,
    pub attr: u32,
    pub value: FmtBuf<VALUE_TEXT>,
}

/// The discriminator a device is looked for by: all 12 bits (QR code,
/// `pair`) or the top 4 (a manual pairing code, `paircode`).
#[derive(Clone, Copy)]
pub enum Disc {
    Long(u16),
    Short(u8),
}

impl Disc {
    fn matches(self, d: Option<u16>) -> bool {
        match (self, d) {
            (Disc::Long(want), Some(d)) => d == want,
            (Disc::Short(want), Some(d)) => (d >> 8) as u8 == want,
            _ => false,
        }
    }
}

/// The address to reach a device at, of those mDNS gave: IPv4 first, then
/// a routable IPv6 one (a Thread device has only those, through its border
/// router); a link-local one would need its scope.
fn pick_addr<'a, I: Iterator<Item = &'a core::net::IpAddr>>(addrs: impl Fn() -> I) -> Option<core::net::IpAddr> {
    let routable = |a: &&core::net::IpAddr| match a {
        core::net::IpAddr::V4(_) => true,
        core::net::IpAddr::V6(v6) => !v6.is_unicast_link_local(),
    };
    addrs().find(|a| a.is_ipv4()).or_else(|| addrs().find(routable)).or_else(|| addrs().next()).copied()
}

#[derive(Clone, Copy)]
enum Activity {
    Idle,
    Discover { node: u64, passcode: u32, discriminator: Disc, until: u64, next_query: u64 },
    Pairing { node: u64, addr: SocketAddr, since: u64 },
    /// Over BLE: waiting for `blelink`'s link; PASE to AddNOC; then the
    /// node's operational address (mDNS).
    BleLink { node: u64, passcode: u32, since: u64 },
    BlePairing { node: u64, since: u64 },
    Resolve { node: u64, since: u64, until: u64, next_query: u64 },
    /// `relocated`: the address came from mDNS after CASE to the stored
    /// one failed; CASE is not tried a third time.
    Connecting { node: u64, addr: SocketAddr, op: Op, relocated: bool },
    /// CASE to the stored address failed: the node's operational address
    /// from mDNS (it may have a new one), then CASE again.
    Locate { node: u64, op: Op, until: u64, next_query: u64 },
    AwaitOp { node: u64, op: Op },
}

#[derive(Clone, Copy)]
struct Node {
    id: u64,
    addr: SocketAddr,
    session: Option<SessionId>,
}

/// A datagram waiting to go: the Matter socket, or an mDNS query.
pub struct Tx {
    pub buf: [u8; MAX_PACKET_SIZE],
    pub len: usize,
    pub to: Option<SocketAddr>,
}

struct Owned {
    crypto: Backend,
    ca: Ca<Backend>,
}

static mut OWNED: MaybeUninit<Owned> = MaybeUninit::uninit();
static mut CONTROLLER: MaybeUninit<Controller> = MaybeUninit::uninit();

pub struct Controller {
    o: &'static Owned,
    stack: Stack,
    comm: Option<Comm>,
    activity: Activity,
    nodes: heapless::Vec<Node, MAX_NODES>,
    txq: heapless::Deque<Tx, TX_QUEUE>,
    /// Messages for the device over BLE; whether the link can go.
    ble_txq: heapless::Deque<Tx, TX_QUEUE>,
    ble_close: bool,
    /// The Wi-Fi network a device commissioned over BLE is given (SSID,
    /// passphrase): in memory only, never logged.
    wifi: Option<(heapless::Vec<u8, 32>, heapless::Vec<u8, 64>)>,
    kvs: &'static RefCell<Store>,
    last_phase: Option<Phase>,
    outcome: Option<Outcome>,
    /// Reports and lost subscriptions, for `watch` (the oldest go if no
    /// one takes them).
    reports: heapless::Deque<Report, 8>,
    /// (node, subscription id).
    lost: heapless::Deque<(u64, u32), 4>,
    /// Each live subscription's session, id and node: its reports may come
    /// on a session the node no longer uses (CASE was made again). Each
    /// device numbers its own: the Light and the Door both gave 15, and by
    /// the id alone the Door's reports went to the Light (doc/49 §25).
    subs: heapless::Vec<(SessionId, u32, u64), 16>,
    /// A search by short discriminator: the request, and the devices
    /// PASE already failed with. Another device may share the 4 bits; it
    /// refuses the passcode and the next one is tried.
    search: Option<Search>,
    /// A request's event taken with a report (simple-matter's `im_take_event`
    /// gives either); `pump` takes it.
    im_event: Option<ImEvent>,
}

#[derive(Clone)]
struct Search {
    node: u64,
    passcode: u32,
    discriminator: Disc,
    until: u64,
    tried: heapless::Vec<SocketAddr, 4>,
}

fn resumption_key(node: u64) -> [u8; 19] {
    let mut k = *b"rsm0000000000000000";
    for i in 0..16 {
        k[3 + i] = b"0123456789ABCDEF"[((node >> (4 * (15 - i))) & 0xF) as usize];
    }
    k
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Idle => "idle",
        Phase::Pase => "pase",
        Phase::ArmFailSafe => "arm_fail_safe",
        Phase::Attestation => "attestation",
        Phase::Csr => "csr",
        Phase::AddTrustedRoot => "add_trusted_root",
        Phase::AddNoc => "add_noc",
        Phase::AddWifiNetwork => "add_wifi_network",
        Phase::ConnectNetwork => "connect_network",
        Phase::Case => "case",
        Phase::Complete => "complete",
        Phase::Done { .. } => "done",
        Phase::Failed { .. } => "failed",
    }
}

impl Controller {
    /// Restores the CA (or makes one) and the nodes from the store. Only
    /// once per process.
    pub fn init(kvs: &'static RefCell<Store>) -> Result<&'static mut Controller, &'static str> {
        let crypto = RustCrypto::new(KernelRng);
        let mut rec = [0u8; CA_STATE_MAX_LEN];
        let saved = kvs.borrow_mut().get(b"cast", &mut rec);
        let ca = match saved {
            Ok(Some(n)) => Ca::decode_state(&crypto, &rec[..n], 0).ok(),
            _ => None,
        };
        let generated = ca.is_none();
        let ca = match ca {
            Some(ca) => ca,
            None => {
                let mut id = [0u8; 8];
                t5_rt::random_fill(&mut id).map_err(|_| "random")?;
                // A fabric ID is never 0.
                let fabric = u64::from_le_bytes(id) | 1;
                Ca::generate(&crypto, &mut KernelRng, fabric, CONTROLLER_NODE, VENDOR, 0).map_err(|_| "CA")?
            }
        };
        let o: &'static Owned = unsafe { (*addr_of_mut!(OWNED)).write(Owned { crypto: RustCrypto::new(KernelRng), ca }) };
        let creds = ControllerCreds::new(&o.ca, &o.crypto, 0);
        let stack: Stack = ControllerStack::new(&o.crypto, ScInitiator::new(&o.crypto, KernelRng, creds), ImClient::new());
        let c = unsafe {
            (*addr_of_mut!(CONTROLLER)).write(Controller {
                o,
                stack,
                comm: None,
                activity: Activity::Idle,
                nodes: heapless::Vec::new(),
                txq: heapless::Deque::new(),
                ble_txq: heapless::Deque::new(),
                ble_close: false,
                wifi: None,
                kvs,
                last_phase: None,
                outcome: None,
                reports: heapless::Deque::new(),
                lost: heapless::Deque::new(),
                subs: heapless::Vec::new(),
                search: None,
                im_event: None,
            })
        };
        if generated {
            c.save_ca();
        }
        c.restore();
        event!(Info, "matter.controller", nodes = c.nodes.len() as u32, new_ca = generated);
        Ok(c)
    }

    fn restore(&mut self) {
        let mut buf = [0u8; nodes_codec::nodes_max_len(MAX_NODES)];
        let got = self.kvs.borrow_mut().get(b"nods", &mut buf);
        if let Ok(Some(n)) = got {
            let nodes = &mut self.nodes;
            let _ = nodes_codec::decode_nodes(&buf[..n], |rec| {
                let _ = nodes.push(Node { id: rec.node_id, addr: rec.last_addr, session: None });
            });
        }
        for i in 0..self.nodes.len() {
            let id = self.nodes[i].id;
            let mut rec = [0u8; RESUMPTION_LEN];
            let got = self.kvs.borrow_mut().get(&resumption_key(id), &mut rec);
            if let Ok(Some(RESUMPTION_LEN)) = got
                && rec[0] == RESUMPTION_VERSION
            {
                let rid: [u8; CASE_RESUMPTION_ID_LEN] = rec[1..1 + CASE_RESUMPTION_ID_LEN].try_into().unwrap();
                let ss: [u8; SHARED_SECRET_LEN] = rec[1 + CASE_RESUMPTION_ID_LEN..].try_into().unwrap();
                self.stack.resumption_import(CONTROLLER_FABRIC_INDEX, id, &rid, &ss);
            }
        }
    }

    fn save_ca(&mut self) {
        let mut rec = [0u8; CA_STATE_MAX_LEN];
        if let Ok(n) = self.o.ca.encode_state(&mut rec)
            && self.kvs.borrow_mut().set(b"cast", &rec[..n]).is_err()
        {
            event!(Warn, "matter.save_failed", what = "ca");
        }
    }

    fn save_nodes(&mut self) {
        let mut recs: heapless::Vec<nodes_codec::NodeRecord, MAX_NODES> = heapless::Vec::new();
        for n in &self.nodes {
            if let Ok(rec) = nodes_codec::NodeRecord::new(n.id, n.addr, "") {
                let _ = recs.push(rec);
            }
        }
        let mut buf = [0u8; nodes_codec::nodes_max_len(MAX_NODES)];
        if let Ok(n) = nodes_codec::encode_nodes(&mut buf, &recs)
            && self.kvs.borrow_mut().set(b"nods", &buf[..n]).is_err()
        {
            event!(Warn, "matter.save_failed", what = "nodes");
        }
    }

    fn save_resumption(&mut self, node: u64) {
        let key = resumption_key(node);
        let r = match self.stack.resumption_export(CONTROLLER_FABRIC_INDEX, node) {
            Some((rid, ss)) => {
                let mut rec = [0u8; RESUMPTION_LEN];
                rec[0] = RESUMPTION_VERSION;
                rec[1..1 + CASE_RESUMPTION_ID_LEN].copy_from_slice(&rid);
                rec[1 + CASE_RESUMPTION_ID_LEN..].copy_from_slice(&ss);
                self.kvs.borrow_mut().set(&key, &rec)
            }
            None => self.kvs.borrow_mut().remove(&key),
        };
        if r.is_err() {
            event!(Warn, "matter.save_failed", what = "resumption");
        }
    }

    fn node_index(&self, id: u64) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    fn set_node(&mut self, id: u64, addr: SocketAddr, session: Option<SessionId>) {
        match self.node_index(id) {
            Some(i) => {
                self.nodes[i].addr = addr;
                if session.is_some() {
                    self.nodes[i].session = session;
                }
            }
            None => {
                let _ = self.nodes.push(Node { id, addr, session });
            }
        }
    }

    pub fn nodes(&self) -> impl Iterator<Item = (u64, SocketAddr, bool)> + '_ {
        self.nodes.iter().map(|n| (n.id, n.addr, n.session.is_some()))
    }

    /// Looks the node up over mDNS and opens CASE there (the address it
    /// has is kept unless that works), then reads VendorName: a test of a
    /// node's other addresses.
    pub fn relocate(&mut self, node: u64, now: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        let i = self.node_index(node).ok_or("unknown node")?;
        self.nodes[i].session = None;
        let op = Op::Read { ep: 0, cluster: 0x28, attr: 1 };
        self.activity = Activity::Locate { node, op, until: now + DISCOVER_MS, next_query: now };
        self.pump(now);
        Ok(())
    }

    pub fn take_report(&mut self) -> Option<Report> {
        self.reports.pop_front()
    }

    pub fn take_lost(&mut self) -> Option<(u64, u32)> {
        self.lost.pop_front()
    }

    pub fn busy(&self) -> bool {
        !matches!(self.activity, Activity::Idle)
    }

    /// The finished request's result, once.
    pub fn take_outcome(&mut self) -> Option<Outcome> {
        self.outcome.take()
    }

    fn finish(&mut self, o: Outcome) {
        self.activity = Activity::Idle;
        self.outcome = Some(o);
    }

    fn queue(&mut self, buf: &[u8], to: Option<SocketAddr>) {
        let mut t = Tx { buf: [0; MAX_PACKET_SIZE], len: buf.len(), to };
        t.buf[..buf.len()].copy_from_slice(buf);
        if self.txq.push_back(t).is_err() {
            event!(Warn, "matter.tx_dropped");
        }
    }

    fn queue_directive(&mut self, buf: &[u8], d: SendDirective) {
        match d.addr {
            PeerAddr::Udp(to) => self.queue(&buf[..d.len], Some(to)),
            PeerAddr::Ble(_) => {
                let mut t = Tx { buf: [0; MAX_PACKET_SIZE], len: d.len, to: None };
                t.buf[..d.len].copy_from_slice(&buf[..d.len]);
                if self.ble_txq.push_back(t).is_err() {
                    event!(Warn, "matter.tx_dropped");
                }
            }
        }
    }

    /// The next message for the device over BLE.
    pub fn take_ble_tx(&mut self) -> Option<Tx> {
        self.ble_txq.pop_front()
    }

    /// True once, when the BLE link is no longer needed.
    pub fn take_ble_close(&mut self) -> bool {
        core::mem::take(&mut self.ble_close)
    }

    /// The next datagram to send: `to` is `None` for an mDNS query (to the
    /// mDNS groups).
    pub fn take_tx(&mut self) -> Option<Tx> {
        self.txq.pop_front()
    }

    // ---- requests ----

    /// Finds the device with `discriminator` over mDNS, then commissions it
    /// as node `node`.
    pub fn pair(&mut self, node: u64, passcode: u32, discriminator: Disc, now: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        self.search = matches!(discriminator, Disc::Short(_)).then(|| Search { node, passcode, discriminator, until: now + SEARCH_MS, tried: heapless::Vec::new() });
        self.activity = Activity::Discover { node, passcode, discriminator, until: now + DISCOVER_MS, next_query: now };
        self.pump(now);
        Ok(())
    }

    /// Forgets node `node` (its address and CASE resumption), as after the
    /// device left; does not tell the device.
    pub fn forget(&mut self, node: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        let i = self.node_index(node).ok_or("unknown node")?;
        self.nodes.remove(i);
        self.save_nodes();
        if self.kvs.borrow_mut().remove(&resumption_key(node)).is_err() {
            event!(Warn, "matter.save_failed", what = "resumption");
        }
        Ok(())
    }

    /// The Wi-Fi network for devices commissioned over BLE (`None`: none,
    /// they are on the network already).
    pub fn set_wifi(&mut self, net: Option<(&[u8], &[u8])>) -> Result<(), &'static str> {
        self.wifi = match net {
            Some((ssid, psk)) => Some((
                heapless::Vec::from_slice(ssid).map_err(|_| "SSID: 32 bytes at most")?,
                heapless::Vec::from_slice(psk).map_err(|_| "passphrase: 64 bytes at most")?,
            )),
            None => None,
        };
        Ok(())
    }

    pub fn has_wifi(&self) -> bool {
        self.wifi.is_some()
    }

    /// Commissions the device that `blelink` finds over BLE (its
    /// `central_start`, by the caller) as node `node`.
    pub fn pair_ble(&mut self, node: u64, passcode: u32, now: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        self.activity = Activity::BleLink { node, passcode, since: now };
        Ok(())
    }

    /// `blelink`: the BTP link is up.
    pub fn ble_linked(&mut self, now: u64) {
        let Activity::BleLink { node, passcode, since } = self.activity else { return };
        let mut comm = Commissioner::new(&self.o.ca, &self.o.crypto, AttestationPolicy::VerifyNoPaa);
        // CASE over the network after AddNOC (and the Wi-Fi network, if
        // there is one to give): the link goes then.
        comm.suspend_before_case();
        if let Some((ssid, psk)) = &self.wifi
            && comm.set_wifi_credentials(ssid, psk).is_err()
        {
            self.finish(Outcome::PairFailed { stage: 0, reason: "Wi-Fi credentials" });
            self.ble_close = true;
            return;
        }
        if comm.commission(BLE, passcode, node, now).is_err() {
            self.finish(Outcome::PairFailed { stage: 0, reason: "start" });
            self.ble_close = true;
            return;
        }
        self.comm = Some(comm);
        self.last_phase = None;
        self.activity = Activity::BlePairing { node, since };
        self.pump(now);
    }

    /// `blelink`: no link, or it went.
    pub fn ble_failed(&mut self, why: &'static str) {
        if matches!(self.activity, Activity::BleLink { .. } | Activity::BlePairing { .. }) {
            let stage = match self.last_phase {
                Some(Phase::Failed { stage, .. }) => stage,
                _ => 0,
            };
            self.comm = None;
            self.abandon_pairing();
            self.finish(Outcome::PairFailed { stage, reason: why });
        }
    }

    /// A commissioning that failed or was given up may leave its
    /// transaction and handshake in the stack: the IM client takes one
    /// transaction at a time, and every subscribe after a failed attestation
    /// over BLE failed to start (doc/49 §25).
    fn abandon_pairing(&mut self) {
        self.stack.abort_op();
        self.stack.abort_handshake();
    }

    /// `blelink`: a message from the device over BLE.
    pub fn ble_rx(&mut self, dg: &mut [u8], now: u64) {
        let mut scratch = [0u8; MAX_PACKET_SIZE];
        if let Some(d) = self.stack.handle_rx(dg, BLE, now, &mut scratch) {
            self.queue_directive(&scratch, d);
        }
        self.take_reports();
    }

    /// `op` on node `node`: at once on a live session, else after CASE.
    pub fn start(&mut self, node: u64, op: Op, now: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        let i = self.node_index(node).ok_or("unknown node")?;
        let addr = self.nodes[i].addr;
        if let Some(session) = self.nodes[i].session {
            let mut scratch = [0u8; MAX_PACKET_SIZE];
            if let Ok(d) = self.issue(session, op, now, &mut scratch) {
                self.queue_directive(&scratch, d);
                self.activity = Activity::AwaitOp { node, op };
                return Ok(());
            }
            // The session went stale: CASE again.
            self.nodes[i].session = None;
        }
        let mut scratch = [0u8; MAX_PACKET_SIZE];
        let d = self.stack.start_case(PeerAddr::Udp(addr), CONTROLLER_FABRIC_INDEX, node, now, &mut scratch).map_err(|_| "CASE")?;
        self.queue_directive(&scratch, d);
        self.activity = Activity::Connecting { node, addr, op, relocated: false };
        Ok(())
    }

    fn issue(&mut self, session: SessionId, op: Op, now: u64, scratch: &mut [u8]) -> simple_matter::error::Result<SendDirective> {
        match op {
            Op::Invoke { ep, cluster, cmd, args, n } => {
                let path = CommandPath::new(EndpointId(ep), ClusterId(cluster), CommandId(cmd));
                self.stack.start_invoke(
                    session,
                    path,
                    |w, t| {
                        w.start_struct(t)?;
                        for (i, a) in args[..n].iter().enumerate() {
                            a.write(w, &TlvTag::ContextSpecific(i as u8))?;
                        }
                        w.end_container()
                    },
                    now,
                    scratch,
                )
            }
            Op::Write { ep, cluster, attr, val } => self.stack.start_write(
                session,
                &AttributePath::concrete(EndpointId(ep), ClusterId(cluster), AttributeId(attr)),
                |w, t| val.write(w, t),
                now,
                scratch,
            ),
            Op::Subscribe { paths, n, min_s, max_s } => {
                let ps: heapless::Vec<AttributePath, SUB_PATHS> = paths[..n as usize]
                    .iter()
                    .map(|&(ep, cluster, attr)| AttributePath::concrete(EndpointId(ep), ClusterId(cluster), AttributeId(attr)))
                    .collect();
                self.stack.start_subscribe(session, &ps, min_s, max_s, now, scratch)
            }
            Op::SubscribeEvent { ep, cluster, event, min_s, max_s } => self.stack.start_subscribe_events(
                session,
                &[],
                &[EventPath::concrete(EndpointId(ep), ClusterId(cluster), EventId(event))],
                None,
                min_s,
                max_s,
                now,
                scratch,
            ),
            Op::Read { ep, cluster, attr } => self.stack.start_read(
                session,
                &[AttributePath::concrete(EndpointId(ep), ClusterId(cluster), AttributeId(attr))],
                now,
                scratch,
            ),
            // The verifier (PBKDF2, 1000 rounds) takes about 220 ms here (doc/49 §2).
            Op::OpenWindow(p) => self.stack.start_open_commissioning_window(session, &p, now, scratch),
            Op::Revoke => self.stack.start_revoke_commissioning(session, now, scratch),
        }
    }

    // ---- I/O ----

    /// A datagram on the controller's socket. The request advances from
    /// `poll`, once the transport has nothing pending (cffi's order): the
    /// acknowledgement of a reply goes out before the next request. Sent
    /// at once (23 ms after AddTrustedRoot's reply), AddNOC got no reply
    /// from simple-matter's example device (doc/49 §6).
    pub fn handle_rx(&mut self, dg: &mut [u8], from: SocketAddr, now: u64) {
        let mut scratch = [0u8; MAX_PACKET_SIZE];
        if let Some(d) = self.stack.handle_rx(dg, PeerAddr::Udp(from), now, &mut scratch) {
            self.queue_directive(&scratch, d);
        }
        self.take_reports();
    }

    /// A report at once: simple-matter keeps only the last one, and `pump`
    /// waits for the transport to settle (the report's StatusResponse
    /// acknowledged). A second node's report within that wait replaced the
    /// first (Door, then the Light HA turned on for it, doc/49 §25).
    /// Not while commissioning: the Commissioner takes its own events from
    /// the stack.
    fn take_reports(&mut self) {
        if self.comm.is_some() {
            return;
        }
        while let Some(ev) = self.stack.im_take_event() {
            if !self.subscription_event(ev) {
                self.im_event = Some(ev);
            }
        }
    }

    fn im_take_event(&mut self) -> Option<ImEvent> {
        self.im_event.take().or_else(|| self.stack.im_take_event())
    }

    /// An mDNS packet (answers to discovery).
    pub fn mdns_rx(&mut self, pkt: &[u8], now: u64) {
        if let Activity::Locate { node, op, .. } = self.activity {
            let cfid = self.o.ca.compressed_fabric_id_bytes();
            let Some(found) = MdnsClient::parse_operational(pkt, &cfid, node) else { return };
            let Some(ip) = pick_addr(|| found.addrs.iter()) else { return };
            let addr = SocketAddr::new(ip, if found.port == 0 { MATTER_PORT } else { found.port });
            event!(Info, "matter.relocated", node = node, v4 = ip.is_ipv4(), port = addr.port() as u32);
            // Kept once CASE there succeeds.
            let mut scratch = [0u8; MAX_PACKET_SIZE];
            match self.stack.start_case(PeerAddr::Udp(addr), CONTROLLER_FABRIC_INDEX, node, now, &mut scratch) {
                Ok(d) => {
                    self.queue_directive(&scratch, d);
                    self.activity = Activity::Connecting { node, addr, op, relocated: true };
                }
                Err(_) => self.finish(Outcome::Failed { why: "CASE", status: 0 }),
            }
            return;
        }
        if let Activity::Resolve { node, since, .. } = self.activity {
            let cfid = self.o.ca.compressed_fabric_id_bytes();
            let Some(found) = MdnsClient::parse_operational(pkt, &cfid, node) else { return };
            let Some(ip) = pick_addr(|| found.addrs.iter()) else { return };
            let addr = SocketAddr::new(ip, if found.port == 0 { MATTER_PORT } else { found.port });
            event!(Info, "matter.resolved", node = node, v4 = ip.is_ipv4(), port = addr.port() as u32);
            let Some(comm) = self.comm.as_mut() else { return };
            comm.set_peer(PeerAddr::Udp(addr));
            comm.resume();
            self.activity = Activity::Pairing { node, addr, since };
            self.pump(now);
            return;
        }
        let Activity::Discover { node, passcode, discriminator, .. } = self.activity else { return };
        let Some(found) = MdnsClient::parse_commissionable(pkt) else { return };
        if !discriminator.matches(found.discriminator) {
            return;
        }
        let Some(ip) = pick_addr(|| found.addrs.iter()) else { return };
        let port = if found.port == 0 { MATTER_PORT } else { found.port };
        let addr = SocketAddr::new(ip, port);
        if self.search.as_ref().is_some_and(|s| s.tried.contains(&addr)) {
            return;
        }
        let mut at = FmtBuf::<48>::new();
        let _ = core::fmt::Write::write_fmt(&mut at, format_args!("{ip}"));
        event!(Info, "matter.found", discriminator = found.discriminator.unwrap_or(0) as u32, v4 = ip.is_ipv4(), port = port as u32, addr = at.as_str());
        self.begin_pair(node, passcode, addr, now);
    }

    /// Commissions the device at `addr` as node `node`, without looking
    /// for it (as smctl's `pairing address`).
    pub fn pair_addr(&mut self, node: u64, passcode: u32, addr: SocketAddr, now: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        self.begin_pair(node, passcode, addr, now);
        Ok(())
    }

    fn begin_pair(&mut self, node: u64, passcode: u32, addr: SocketAddr, now: u64) {
        let mut comm = Commissioner::new(&self.o.ca, &self.o.crypto, AttestationPolicy::VerifyNoPaa);
        if comm.commission(PeerAddr::Udp(addr), passcode, node, now).is_err() {
            self.finish(Outcome::PairFailed { stage: 0, reason: "start" });
            return;
        }
        self.comm = Some(comm);
        self.last_phase = None;
        self.activity = Activity::Pairing { node, addr, since: now };
        self.pump(now);
    }

    /// Timers; the next datagram to send is in the queue.
    pub fn poll(&mut self, now: u64) {
        if !self.txq.is_empty() {
            return;
        }
        let mut scratch = [0u8; MAX_PACKET_SIZE];
        if let Some(d) = self.stack.poll(now, &mut scratch) {
            self.queue_directive(&scratch, d);
            return;
        }
        if self.stack.transport_deadline().is_some() {
            return;
        }
        self.pump(now);
    }

    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        if !self.txq.is_empty() {
            return Some(now);
        }
        match self.activity {
            Activity::Idle => self.stack.next_deadline(now),
            Activity::Discover { next_query, until, .. } | Activity::Resolve { next_query, until, .. } | Activity::Locate { next_query, until, .. } => Some(next_query.min(until)),
            _ => Some(self.stack.next_deadline(now).unwrap_or(now + 20)),
        }
    }

    /// Advances the current request as far as it goes now.
    /// A subscription's report or loss (logged); other events stay.
    fn subscription_event(&mut self, ev: ImEvent) -> bool {
        match ev {
            ImEvent::SubscriptionReport { session, subscription_id } => {
                let node = self
                    .subs
                    .iter()
                    .find(|s| s.0 == session && s.1 == subscription_id)
                    .map(|s| s.2)
                    .or_else(|| self.nodes.iter().find(|n| n.session == Some(session)).map(|n| n.id))
                    .unwrap_or(0);
                for report in self.stack.sub_reports() {
                    let Ok(AttributeReportRef::Data(d)) = report else { continue };
                    let Some(c) = d.path.to_concrete() else { continue };
                    let mut value = FmtBuf::<VALUE_TEXT>::new();
                    fmt_value(d.value(), &mut value);
                    event!(
                        Info,
                        "matter.report",
                        node = node,
                        id = subscription_id,
                        ep = c.endpoint.0 as u32,
                        cluster = c.cluster.0,
                        attr = c.attribute.0,
                        value = value.as_str()
                    );
                    if self.reports.is_full() {
                        self.reports.pop_front();
                    }
                    let _ = self.reports.push_back(Report { node, ep: c.endpoint.0, cluster: c.cluster.0, attr: c.attribute.0, value });
                }
                for report in self.stack.sub_event_reports() {
                    let Ok(EventReportRef::Data(d)) = report else { continue };
                    let mut value = FmtBuf::<VALUE_TEXT>::new();
                    fmt_value(d.value(), &mut value);
                    event!(
                        Info,
                        "matter.event",
                        node = node,
                        id = subscription_id,
                        ep = d.path.endpoint.map_or(0xFFFF, |e| e.0 as u32),
                        cluster = d.path.cluster.map_or(0, |c| c.0),
                        ev = d.path.event.map_or(0, |e| e.0),
                        number = d.number,
                        priority = d.priority,
                        value = value.as_str()
                    );
                }
                true
            }
            ImEvent::SubscriptionLost { session, subscription_id } => {
                let Some(i) = self.subs.iter().position(|s| s.0 == session && s.1 == subscription_id) else {
                    event!(Warn, "matter.subscription_lost", node = 0u64, id = subscription_id);
                    return true;
                };
                let node = self.subs.remove(i).2;
                event!(Warn, "matter.subscription_lost", node = node, id = subscription_id);
                if self.lost.is_full() {
                    self.lost.pop_front();
                }
                let _ = self.lost.push_back((node, subscription_id));
                true
            }
            _ => false,
        }
    }

    fn pump(&mut self, now: u64) {
        match self.activity {
            Activity::Idle => {
                // Nothing asked for: reports; drop stray events.
                let _ = self.stack.sc_take_event();
                if let Some(ev) = self.im_take_event() {
                    self.subscription_event(ev);
                }
            }
            Activity::Discover { node, passcode, discriminator, until, next_query } => {
                if now >= until {
                    self.finish(Outcome::NotFound);
                } else if now >= next_query {
                    let mut q = [0u8; 128];
                    // All commissionable devices, filtered here by the
                    // discriminator in their TXT (as smctl does): not every
                    // responder answers the `_L<d>` subtype. Answers by
                    // unicast (QU): large multicast answers from the wired
                    // LAN mostly did not reach the Tab5 over Wi-Fi (doc/49 §6).
                    if let Ok(n) = MdnsClient::build_browse_commissionable(&mut q, true) {
                        self.queue(&q[..n], None);
                    }
                    self.activity = Activity::Discover { node, passcode, discriminator, until, next_query: now + DISCOVER_EVERY_MS };
                }
            }
            Activity::Pairing { node, addr, since } => self.drive_pairing(node, Some(addr), since, now),
            Activity::BleLink { .. } => {}
            Activity::BlePairing { node, since } => {
                if now >= since + BLE_PAIR_MS {
                    self.comm = None;
                    self.ble_close = true;
                    self.finish(Outcome::PairFailed { stage: 0, reason: "timed out over BLE" });
                } else {
                    self.drive_pairing(node, None, since, now);
                }
            }
            Activity::Resolve { node, since, until, next_query } => {
                if now >= until {
                    self.comm = None;
                    self.finish(Outcome::PairFailed { stage: 0, reason: "no operational address" });
                } else if now >= next_query {
                    let mut q = [0u8; 160];
                    let cfid = self.o.ca.compressed_fabric_id_bytes();
                    if let Ok(n) = MdnsClient::build_resolve_operational(&mut q, &cfid, node, true) {
                        self.queue(&q[..n], None);
                    }
                    self.activity = Activity::Resolve { node, since, until, next_query: now + DISCOVER_EVERY_MS };
                }
            }
            Activity::Locate { node, op, until, next_query } => {
                if now >= until {
                    self.finish(Outcome::Failed { why: "CASE (no address)", status: 0 });
                } else if now >= next_query {
                    let mut q = [0u8; 160];
                    let cfid = self.o.ca.compressed_fabric_id_bytes();
                    if let Ok(n) = MdnsClient::build_resolve_operational(&mut q, &cfid, node, true) {
                        self.queue(&q[..n], None);
                    }
                    self.activity = Activity::Locate { node, op, until, next_query: now + DISCOVER_EVERY_MS };
                }
            }
            Activity::Connecting { node, addr, op, relocated } => match self.stack.sc_take_event() {
                Some(ScEvent::CaseEstablished { session, resumed }) => {
                    self.set_node(node, addr, Some(session));
                    if relocated {
                        self.save_nodes();
                    }
                    self.save_resumption(node);
                    event!(Info, "matter.case", node = node, resumed = resumed);
                    let mut scratch = [0u8; MAX_PACKET_SIZE];
                    match self.issue(session, op, now, &mut scratch) {
                        Ok(d) => {
                            self.queue_directive(&scratch, d);
                            self.activity = Activity::AwaitOp { node, op };
                        }
                        Err(_) => self.finish(Outcome::Failed { why: "start", status: 0 }),
                    }
                }
                // The node may have another address now (DHCP, a new
                // border router): look it up once.
                Some(ScEvent::Failed { .. }) if !relocated => {
                    event!(Info, "matter.locate", node = node);
                    self.activity = Activity::Locate { node, op, until: now + DISCOVER_MS, next_query: now };
                    self.pump(now);
                }
                Some(ScEvent::Failed { .. }) => self.finish(Outcome::Failed { why: "CASE", status: 0 }),
                _ => {}
            },
            Activity::AwaitOp { node, op } => {
                let Some(ev) = self.im_take_event() else { return };
                if self.subscription_event(ev) {
                    return;
                }
                match (op, ev) {
                    (Op::Invoke { .. } | Op::OpenWindow(_) | Op::Revoke, ImEvent::InvokeDone { status }) => self.finish(Outcome::Invoked { status: status.to_u8() }),
                    (Op::Write { .. }, ImEvent::WriteDone { status }) => self.finish(Outcome::Invoked { status: status.to_u8() }),
                    (Op::Subscribe { paths, n, .. }, ImEvent::SubscribeDone { session, subscription_id, max_interval_s }) => {
                        if self.subs.is_full() {
                            self.subs.remove(0);
                        }
                        let _ = self.subs.push((session, subscription_id, node));
                        // The priming report (the values at the start) is
                        // kept as a read's result; only these paths'.
                        for report in self.stack.read_reports() {
                            let Ok(AttributeReportRef::Data(d)) = report else { continue };
                            let Some(c) = d.path.to_concrete() else { continue };
                            if !paths[..n as usize].contains(&(c.endpoint.0, c.cluster.0, c.attribute.0)) {
                                continue;
                            }
                            let mut value = FmtBuf::<VALUE_TEXT>::new();
                            fmt_value(d.value(), &mut value);
                            if self.reports.is_full() {
                                self.reports.pop_front();
                            }
                            let _ = self.reports.push_back(Report { node, ep: c.endpoint.0, cluster: c.cluster.0, attr: c.attribute.0, value });
                        }
                        self.finish(Outcome::Subscribed { id: subscription_id, max_s: max_interval_s })
                    }
                    (Op::SubscribeEvent { .. }, ImEvent::SubscribeDone { subscription_id, max_interval_s, .. }) => {
                        self.finish(Outcome::Subscribed { id: subscription_id, max_s: max_interval_s })
                    }
                    (Op::Read { ep, cluster, attr }, ImEvent::ReadDone) => {
                        let o = self.read_value(ep, cluster, attr).unwrap_or(Outcome::Failed { why: "no value", status: 0 });
                        self.finish(o);
                    }
                    (_, ImEvent::Failed { status }) => {
                        // A timeout: the device may not know this session any
                        // more (it restarted); CASE again next time.
                        if matches!(status, ImStatus::Timeout)
                            && let Some(i) = self.node_index(node)
                        {
                            self.nodes[i].session = None;
                        }
                        self.finish(Outcome::Failed { why: "IM", status: status.to_u8() });
                    }
                    _ => self.finish(Outcome::Failed { why: "unexpected", status: 0 }),
                }
            }
        }
    }

    /// `addr` is `None` while over BLE.
    fn drive_pairing(&mut self, node: u64, addr: Option<SocketAddr>, since: u64, now: u64) {
        loop {
            let mut scratch = [0u8; MAX_PACKET_SIZE];
            let Some(comm) = self.comm.as_mut() else {
                self.activity = Activity::Idle;
                return;
            };
            let prev = comm.phase();
            let out = comm.drive(&mut self.stack, now, &mut scratch);
            if let Some(d) = out.send {
                self.queue_directive(&scratch, d);
            }
            if self.last_phase != Some(out.phase) {
                self.last_phase = Some(out.phase);
                event!(Debug, "matter.pair_phase", phase = phase_name(out.phase));
            }
            match out.phase {
                // Over BLE, AddNOC done: CASE waits for the operational
                // address; the link can go.
                Phase::Case if addr.is_none() && out.send.is_none() => {
                    event!(Info, "matter.ble_handoff", node = node, ms = now - since);
                    self.ble_close = true;
                    self.activity = Activity::Resolve { node, since, until: now + DISCOVER_MS, next_query: now };
                    self.pump(now);
                    return;
                }
                Phase::Done { session } => {
                    let Some(addr) = addr else { return };
                    self.set_node(node, addr, Some(session));
                    self.comm = None;
                    self.save_ca();
                    self.save_nodes();
                    self.save_resumption(node);
                    self.finish(Outcome::Commissioned { node, ms: now - since });
                    return;
                }
                Phase::Failed { stage, reason } => {
                    let mut why = FmtBuf::<64>::new();
                    let _ = core::fmt::Write::write_fmt(&mut why, format_args!("{reason:?}"));
                    event!(Info, "matter.pair_failed", node = node, stage = stage as u32, why = why.as_str());
                    self.comm = None;
                    self.abandon_pairing();
                    if addr.is_none() {
                        self.ble_close = true;
                    }
                    // Found by 4 bits of the discriminator, refused in
                    // PASE: maybe another device; look for the next.
                    if let (Some(mut s), Some(a), true) = (self.search.take(), addr, matches!(prev, Phase::Pase | Phase::Idle))
                        && now < s.until
                        && s.tried.push(a).is_ok()
                    {
                        event!(Info, "matter.next_candidate", node = node, tried = s.tried.len() as u32);
                        self.activity = Activity::Discover { node: s.node, passcode: s.passcode, discriminator: s.discriminator, until: now + DISCOVER_MS, next_query: now };
                        self.search = Some(s);
                        return;
                    }
                    self.search = None;
                    self.finish(Outcome::PairFailed { stage, reason: "commissioning" });
                    return;
                }
                _ => {}
            }
            if out.send.is_none() && out.phase == prev {
                return;
            }
        }
    }

    fn read_value(&self, ep: u16, cluster: u32, attr: u32) -> Option<Outcome> {
        for report in self.stack.read_reports() {
            let Ok(AttributeReportRef::Data(d)) = report else { continue };
            let Some(c) = d.path.to_concrete() else { continue };
            if c.endpoint.0 != ep || c.cluster.0 != cluster || c.attribute.0 != attr {
                continue;
            }
            if let Some((value, null)) = first_value(d.value()) {
                return Some(Outcome::Read { value, null });
            }
            let mut t = FmtBuf::new();
            fmt_value(d.value(), &mut t);
            return Some(Outcome::ReadText(t));
        }
        None
    }
}

/// An attribute's value as a number (booleans 0 or 1), or null; `None`
/// for other types (`fmt_value`).
fn first_value(mut v: simple_matter::tlv::TlvReader<'_>) -> Option<(u64, bool)> {
    let e = v.read_next().ok()??;
    match e.value {
        TlvValue::Boolean(b) => Some((b as u64, false)),
        TlvValue::UnsignedInteger(u) => Some((u, false)),
        TlvValue::SignedInteger(i) => Some((i as u64, false)),
        TlvValue::Null => Some((0, true)),
        _ => None,
    }
}

/// The text of a value (doc/49 §22).
pub const VALUE_TEXT: usize = 120;

/// A value (its first element) as text: numbers, `true`/`false`, `null`,
/// `"text"`, `h'hex'` for bytes, `{0: …, 1: …}` for structures, `[…]` for
/// arrays and lists; two levels deep, cut with `…` where the room ends.
fn fmt_value(mut v: simple_matter::tlv::TlvReader<'_>, out: &mut FmtBuf<VALUE_TEXT>) {
    let _ = fmt_element(&mut v, out, 0);
}

fn fmt_element(r: &mut simple_matter::tlv::TlvReader<'_>, out: &mut FmtBuf<VALUE_TEXT>, depth: u8) -> Option<bool> {
    use core::fmt::Write as _;
    let e = r.read_next().ok()??;
    if matches!(e.value, TlvValue::ContainerEnd) {
        return Some(false);
    }
    if depth > 0 && let TlvTag::ContextSpecific(t) = e.tag {
        let _ = write!(out, "{t}: ");
    }
    match e.value {
        TlvValue::Boolean(b) => _ = write!(out, "{b}"),
        TlvValue::UnsignedInteger(u) => _ = write!(out, "{u}"),
        TlvValue::SignedInteger(i) => _ = write!(out, "{i}"),
        TlvValue::Float(f) => _ = write!(out, "{f}"),
        TlvValue::Double(f) => _ = write!(out, "{f}"),
        TlvValue::Null => _ = write!(out, "null"),
        TlvValue::Utf8String(t) => _ = write!(out, "{t:?}"),
        TlvValue::ByteString(b) => {
            let _ = write!(out, "h'");
            for x in b.iter().take(16) {
                let _ = write!(out, "{x:02x}");
            }
            let _ = write!(out, "{}'", if b.len() > 16 { "…" } else { "" });
        }
        TlvValue::ContainerStart(kind) => {
            let (open, close) = if matches!(kind, simple_matter::tlv::ContainerType::Structure) { ("{", "}") } else { ("[", "]") };
            let _ = write!(out, "{open}");
            let mut first = true;
            loop {
                if depth >= 2 {
                    // Deeper: skipped, but counted out of the container.
                    let e = r.read_next().ok()??;
                    if matches!(e.value, TlvValue::ContainerEnd) {
                        let _ = write!(out, "…{close}");
                        break;
                    }
                    r.skip(&e).ok()?;
                    continue;
                }
                let mark = out.as_str().len();
                if !first {
                    let _ = write!(out, ", ");
                }
                match fmt_element(r, out, depth + 1)? {
                    true => first = false,
                    false => {
                        out.truncate(mark);
                        let _ = write!(out, "{close}");
                        break;
                    }
                }
            }
        }
        TlvValue::ContainerEnd => {}
    }
    Some(true)
}
