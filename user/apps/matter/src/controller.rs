//! The controller role (doc/49 MT2): simple-matter's `ControllerStack`
//! and `Commissioner`, as `simple-matter-cffi`'s controller drives them
//! (its `CtrlShim`, `pump`, `start_operation`), cut down to what the shell
//! asks for: find a commissionable device by discriminator over mDNS,
//! commission it on the network, then commands and attribute reads over
//! CASE (resumed when it can). One request at a time. The CA, the node
//! list and each node's CASE resumption are kept in the store.

use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::net::SocketAddr;
use core::ptr::addr_of_mut;

use mtport::{FileKvs, KernelRng};
use simple_matter::controller::ca::{CA_STATE_MAX_LEN, Ca};
use simple_matter::controller::nodes as nodes_codec;
use simple_matter::controller::{
    AttestationPolicy, CONTROLLER_FABRIC_INDEX, Commissioner, ControllerCreds, ControllerStack, OpenWindowParams, Phase,
};
use simple_matter::crypto::rustcrypto::RustCrypto;
use simple_matter::discovery::MATTER_PORT;
use simple_matter::discovery::client::MdnsClient;
use simple_matter::dm::meta::{AttributeId, ClusterId, CommandId, EndpointId};
use simple_matter::im::client::{ImClient, ImEvent};
use simple_matter::im::wire::{AttributePath, AttributeReportRef, CommandPath, ImStatus};
use simple_matter::kvs::Kvs;
use simple_matter::sc::case::common::{CASE_RESUMPTION_ID_LEN, SHARED_SECRET_LEN};
use simple_matter::sc::initiator::{ScEvent, ScInitiator};
use simple_matter::stack::{MAX_PACKET_SIZE, SendDirective};
use simple_matter::tlv::TlvValue;
use simple_matter::transport::net::PeerAddr;
use simple_matter::transport::session::SessionId;
use t5_rt::event;

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
/// CASE resumption record: version, resumption ID, shared secret.
const RESUMPTION_VERSION: u8 = 1;
const RESUMPTION_LEN: usize = 1 + CASE_RESUMPTION_ID_LEN + SHARED_SECRET_LEN;

#[derive(Clone, Copy)]
pub enum Op {
    Invoke { ep: u16, cluster: u32, cmd: u32 },
    Read { ep: u16, cluster: u32, attr: u32 },
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
    Failed { why: &'static str, status: u8 },
}

#[derive(Clone, Copy)]
enum Activity {
    Idle,
    Discover { node: u64, passcode: u32, discriminator: u16, until: u64, next_query: u64 },
    Pairing { node: u64, addr: SocketAddr, since: u64 },
    Connecting { node: u64, addr: SocketAddr, op: Op },
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
    kvs: &'static RefCell<FileKvs>,
    last_phase: Option<Phase>,
    outcome: Option<Outcome>,
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
    pub fn init(kvs: &'static RefCell<FileKvs>) -> Result<&'static mut Controller, &'static str> {
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
                kvs,
                last_phase: None,
                outcome: None,
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
        #[allow(irrefutable_let_patterns)]
        if let PeerAddr::Udp(to) = d.addr {
            self.queue(&buf[..d.len], Some(to));
        }
    }

    /// The next datagram to send: `to` is `None` for an mDNS query (to the
    /// mDNS groups).
    pub fn take_tx(&mut self) -> Option<Tx> {
        self.txq.pop_front()
    }

    // ---- requests ----

    /// Finds the device with `discriminator` over mDNS, then commissions it
    /// as node `node`.
    pub fn pair(&mut self, node: u64, passcode: u32, discriminator: u16, now: u64) -> Result<(), &'static str> {
        if self.busy() {
            return Err("busy");
        }
        self.activity = Activity::Discover { node, passcode, discriminator, until: now + DISCOVER_MS, next_query: now };
        self.pump(now);
        Ok(())
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
        self.activity = Activity::Connecting { node, addr, op };
        Ok(())
    }

    fn issue(&mut self, session: SessionId, op: Op, now: u64, scratch: &mut [u8]) -> simple_matter::error::Result<SendDirective> {
        match op {
            Op::Invoke { ep, cluster, cmd } => {
                let path = CommandPath::new(EndpointId(ep), ClusterId(cluster), CommandId(cmd));
                self.stack.start_invoke(
                    session,
                    path,
                    |w, t| {
                        w.start_struct(t)?;
                        w.end_container()
                    },
                    now,
                    scratch,
                )
            }
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
    }

    /// An mDNS packet (answers to discovery).
    pub fn mdns_rx(&mut self, pkt: &[u8], now: u64) {
        let Activity::Discover { node, passcode, discriminator, .. } = self.activity else { return };
        let Some(found) = MdnsClient::parse_commissionable(pkt) else { return };
        if found.discriminator != Some(discriminator) {
            return;
        }
        // IPv4 first: a link-local IPv6 address would need its scope.
        let Some(ip) = found.addrs.iter().find(|a| a.is_ipv4()).or_else(|| found.addrs.iter().next()).copied() else { return };
        let port = if found.port == 0 { MATTER_PORT } else { found.port };
        let addr = SocketAddr::new(ip, port);
        event!(Info, "matter.found", discriminator = discriminator as u32, v4 = ip.is_ipv4(), port = port as u32);
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
            Activity::Discover { next_query, until, .. } => Some(next_query.min(until)),
            _ => Some(self.stack.next_deadline(now).unwrap_or(now + 20)),
        }
    }

    /// Advances the current request as far as it goes now.
    fn pump(&mut self, now: u64) {
        match self.activity {
            Activity::Idle => {
                // Nothing asked for: drop stray events.
                let _ = self.stack.sc_take_event();
                let _ = self.stack.im_take_event();
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
            Activity::Pairing { node, addr, since } => self.drive_pairing(node, addr, since, now),
            Activity::Connecting { node, addr, op } => match self.stack.sc_take_event() {
                Some(ScEvent::CaseEstablished { session, resumed }) => {
                    self.set_node(node, addr, Some(session));
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
                Some(ScEvent::Failed { .. }) => self.finish(Outcome::Failed { why: "CASE", status: 0 }),
                _ => {}
            },
            Activity::AwaitOp { node, op } => {
                let Some(ev) = self.stack.im_take_event() else { return };
                match (op, ev) {
                    (Op::Invoke { .. } | Op::OpenWindow(_) | Op::Revoke, ImEvent::InvokeDone { status }) => self.finish(Outcome::Invoked { status: status.to_u8() }),
                    (Op::Read { ep, cluster, attr }, ImEvent::ReadDone) => {
                        let o = self.read_value(ep, cluster, attr).map_or(Outcome::Failed { why: "no value", status: 0 }, |(value, null)| Outcome::Read { value, null });
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

    fn drive_pairing(&mut self, node: u64, addr: SocketAddr, since: u64, now: u64) {
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
                Phase::Done { session } => {
                    self.set_node(node, addr, Some(session));
                    self.comm = None;
                    self.save_ca();
                    self.save_nodes();
                    self.save_resumption(node);
                    self.finish(Outcome::Commissioned { node, ms: now - since });
                    return;
                }
                Phase::Failed { stage, .. } => {
                    self.comm = None;
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

    fn read_value(&self, ep: u16, cluster: u32, attr: u32) -> Option<(u64, bool)> {
        for report in self.stack.read_reports() {
            let Ok(AttributeReportRef::Data(d)) = report else { continue };
            let Some(c) = d.path.to_concrete() else { continue };
            if c.endpoint.0 != ep || c.cluster.0 != cluster || c.attribute.0 != attr {
                continue;
            }
            let mut v = d.value();
            if let Ok(Some(e)) = v.read_next() {
                return Some(match e.value {
                    TlvValue::Boolean(b) => (b as u64, false),
                    TlvValue::UnsignedInteger(u) => (u, false),
                    TlvValue::SignedInteger(i) => (i as u64, false),
                    TlvValue::Null => (0, true),
                    _ => (0, false),
                });
            }
        }
        None
    }
}
