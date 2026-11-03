#![no_std]
#![no_main]

//! The BLE service (doc/48 M2, M3): TrouBLE (trouble-host) over the
//! kernel's `bt:hci`, run by this process's own executor (`exec`; the only
//! async code in the OS). Clients connect to `svc:ble` (`t5_abi::ble`) one
//! at a time: a scan, its advertising reports sent to the client; or, as
//! the central, one connection and its GATT client (services,
//! characteristics, read, write, one subscription); or, as the peripheral,
//! a GATT service of the client's, advertised (`periph`). Needs the controller
//! started (`wifi 1`).
//!
//! Built with `--permit bt:hci --permit listen:ble`.

mod exec;
mod hci;
mod periph;

use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::task::Poll;

use bt_hci::controller::ExternalController;
use bt_hci::param::LeAdvReportsIter;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use t5_rt::{Channel, Error, Handle, Listener, ble, bt, event, rights};
use trouble_host::prelude::*;

use exec::want;
use hci::Hci;

t5_rt::entry!(main);

/// Command slots for the controller (commands in flight).
const SLOTS: usize = 4;
const SERVICES_MAX: usize = 16;
const CHARS_MAX: usize = 24;

type Ctl = ExternalController<Hci, SLOTS>;
type Pool = DefaultPacketPool;
type Host<'a> = Stack<'a, Ctl, Pool>;

fn main(_: usize, _: usize) -> usize {
    let Ok(h) = Handle::open(bt::HCI, rights::READ | rights::WRITE | rights::WAIT) else {
        event!(Error, "ble.no_hci");
        return 1;
    };
    let Ok(l) = Listener::open(ble::SVC) else {
        event!(Error, "ble.no_listen");
        return 2;
    };
    let controller: ExternalController<Hci, SLOTS> = ExternalController::new(Hci { h });
    let mut resources: HostResources<DefaultPacketPool, 1, 1> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).build();
    let reports = Reports { client: RefCell::new(None), scanning: Cell::new(false), sent: Cell::new(0), dropped: Cell::new(0) };
    let mut runner = stack.runner();
    // `serve` never ends; the host does only if it fails.
    if let Either::First(Err(e)) = exec::block_on(select(runner.run_with_handler(&reports), serve(&stack, &l, &reports))) {
        event!(Error, "ble.host_failed", error = ble_error(&e));
    }
    3
}

fn ble_error<E>(e: &BleHostError<E>) -> &'static str {
    match e {
        BleHostError::Controller(_) => "controller",
        BleHostError::BleHost(_) => "host",
    }
}

/// The client being served, and where its scan's reports go.
struct Reports {
    client: RefCell<Option<Channel>>,
    scanning: Cell<bool>,
    sent: Cell<u32>,
    dropped: Cell<u32>,
}

impl Reports {
    /// Sends without waiting; false if the client's queue is full or it
    /// has gone.
    fn send(&self, msg: &[u8]) -> bool {
        self.client.borrow().as_ref().is_some_and(|c| c.0.write(msg).is_ok())
    }

    /// Sends, waiting while the client's queue is full (answers to its
    /// own requests, which it reads).
    async fn send_wait(&self, msg: &[u8]) {
        loop {
            let r = self.client.borrow().as_ref().map(|c| c.0.write(msg));
            match r {
                Some(Err(Error::Busy)) => Timer::after_millis(1).await,
                _ => return,
            }
        }
    }

    async fn fail(&self, request: u8, code: u8) {
        self.send_wait(&[ble::FAILED, request, code]).await;
    }
}

/// `FAILED` codes.
const BAD_REQUEST: u8 = 1;
const NOT_NOW: u8 = 2;
const STACK_FAILED: u8 = 3;
const TIMED_OUT: u8 = 4;
const NOT_FOUND: u8 = 5;

impl EventHandler for Reports {
    fn on_adv_reports(&self, reports: LeAdvReportsIter) {
        if !self.scanning.get() {
            return;
        }
        for r in reports.flatten() {
            let mut m = [0u8; 11 + 31];
            let data = &r.data[..r.data.len().min(31)];
            m[0] = ble::ADV;
            m[1] = r.addr_kind.into_inner();
            for (i, b) in r.addr.raw().iter().rev().enumerate() {
                m[2 + i] = *b;
            }
            m[8] = r.event_kind as u8;
            m[9] = r.rssi as u8;
            m[10] = data.len() as u8;
            m[11..11 + data.len()].copy_from_slice(data);
            // A full channel drops the report rather than stall the host.
            if self.send(&m[..11 + data.len()]) {
                self.sent.set(self.sent.get() + 1);
            } else {
                self.dropped.set(self.dropped.get() + 1);
            }
        }
    }
}

async fn accept(l: &Listener) -> Channel {
    poll_fn(|_| match l.accept() {
        Ok(c) => Poll::Ready(c),
        Err(_) => {
            want(l.0.raw());
            Poll::Pending
        }
    })
    .await
}

/// The client's next request, or `None` once it has gone.
async fn request(reports: &Reports, buf: &mut [u8]) -> Option<usize> {
    poll_fn(|_| {
        let client = reports.client.borrow();
        let Some(c) = client.as_ref() else { return Poll::Ready(None) };
        match c.0.read(buf) {
            Ok(n) => Poll::Ready(Some(n)),
            Err(Error::Busy) if !exec::closed(c.0.raw()) => {
                want(c.0.raw());
                Poll::Pending
            }
            Err(_) => Poll::Ready(None),
        }
    })
    .await
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

async fn serve(stack: &Host<'_>, l: &Listener, reports: &Reports) -> ! {
    let mut central = stack.central();
    event!(Info, "ble.ready");
    loop {
        let c = accept(l).await;
        let raw = c.0.raw();
        *reports.client.borrow_mut() = Some(c);
        event!(Debug, "ble.client");
        let mut buf = [0u8; ble::MSG_MAX];
        while let Some(n) = request(reports, &mut buf).await {
            let req = &buf[..n];
            let gone = match req.first().copied() {
                Some(ble::SCAN) if n >= 7 => scan(&mut central, reports, req).await,
                Some(ble::CONNECT) if n >= 9 => connected(stack, &mut central, reports, req).await,
                Some(ble::ADVERTISE) => periph::advertise(stack, reports, req).await,
                Some(r) => {
                    reports.fail(r, if r <= ble::SEND { NOT_NOW } else { BAD_REQUEST }).await;
                    false
                }
                None => false,
            };
            if gone {
                break;
            }
        }
        exec::forget(raw);
        *reports.client.borrow_mut() = None;
        event!(Debug, "ble.client_gone");
    }
}

/// `SCAN`; true if the client left meanwhile.
async fn scan(central: &mut Central<'_, Ctl, Pool>, reports: &Reports, req: &[u8]) -> bool {
    let (seconds, active) = (req[1] as u64, req[2] != 0);
    let interval = u16_at(req, 3).max(3) as u64;
    let window = (u16_at(req, 5) as u64).clamp(3, interval);
    let config = ScanConfig { active, interval: Duration::from_millis(interval), window: Duration::from_millis(window), ..Default::default() };
    let mut scanner = Scanner::new(central);
    let session = match scanner.scan(&config).await {
        Ok(s) => s,
        Err(e) => {
            event!(Warn, "ble.scan_failed", error = ble_error(&e));
            reports.fail(ble::SCAN, STACK_FAILED).await;
            return false;
        }
    };
    reports.sent.set(0);
    reports.dropped.set(0);
    reports.scanning.set(true);
    event!(Info, "ble.scan", seconds = seconds as u32, active = active, interval_ms = interval as u32, window_ms = window as u32);
    let until = async {
        if seconds == 0 {
            core::future::pending::<()>().await
        } else {
            Timer::after_secs(seconds).await
        }
    };
    let mut more = [0u8; 16];
    // A `STOP` (or any request) ends the scan, as does the end of the
    // time or the client leaving.
    let gone = match select(until, request(reports, &mut more)).await {
        Either::First(()) => false,
        Either::Second(r) => r.is_none(),
    };
    reports.scanning.set(false);
    session.stop().await;
    let (sent, dropped) = (reports.sent.get(), reports.dropped.get());
    event!(Info, "ble.scan_done", reports = sent, dropped = dropped);
    if !gone {
        let mut done = [0u8; 9];
        done[0] = ble::DONE;
        done[1..5].copy_from_slice(&sent.to_le_bytes());
        done[5..9].copy_from_slice(&dropped.to_le_bytes());
        reports.send_wait(&done).await;
    }
    gone
}

/// `CONNECT` and the GATT requests until the link or the client goes;
/// true if the client left.
async fn connected(stack: &Host<'_>, central: &mut Central<'_, Ctl, Pool>, reports: &Reports, req: &[u8]) -> bool {
    let mut addr = [0u8; 6];
    for (i, b) in req[2..8].iter().rev().enumerate() {
        addr[i] = *b;
    }
    let target = [Address { kind: AddrKind::new(req[1]), addr: BdAddr::new(addr) }];
    let seconds = (req[8] as u64).max(1);
    let config = ConnectConfig {
        scan_config: ScanConfig { filter_accept_list: &target, timeout: Duration::from_secs(seconds), ..Default::default() },
        connect_params: Default::default(),
    };
    event!(Info, "ble.connecting", addr = &req[2..8], addr_type = req[1], seconds = seconds as u32);
    let conn = match embassy_time::with_timeout(Duration::from_secs(seconds), central.connect(&config)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            event!(Warn, "ble.connect_failed", error = ble_error(&e));
            reports.fail(ble::CONNECT, STACK_FAILED).await;
            return false;
        }
        Err(_) => {
            event!(Warn, "ble.connect_failed", error = "timeout");
            reports.fail(ble::CONNECT, TIMED_OUT).await;
            return false;
        }
    };
    let client = match GattClient::<Ctl, Pool, SERVICES_MAX>::new(stack, &conn).await {
        Ok(c) => c,
        Err(e) => {
            event!(Warn, "ble.gatt_failed", error = ble_error(&e));
            conn.disconnect();
            reports.fail(ble::CONNECT, STACK_FAILED).await;
            return false;
        }
    };
    let mtu = conn.att_mtu();
    event!(Info, "ble.connected", att_mtu = mtu as u32);
    let mut m = [ble::CONNECTED, 0, 0];
    m[1..3].copy_from_slice(&mtu.to_le_bytes());
    reports.send_wait(&m).await;
    let r = select(client.task(), gatt(&client, &conn, reports)).await;
    let gone = match r {
        // The link went down (the task ends when it does).
        Either::First(_) => false,
        Either::Second(gone) => gone,
    };
    conn.disconnect();
    event!(Info, "ble.disconnected", client_gone = gone);
    if !gone {
        reports.send_wait(&[ble::DISCONNECTED]).await;
    }
    gone
}

/// The GATT requests of a connection; true if the client left, false
/// after `DISCONNECT`.
async fn gatt(client: &GattClient<'_, Ctl, Pool, SERVICES_MAX>, conn: &Connection<'_, Pool>, reports: &Reports) -> bool {
    let mut services: heapless::Vec<ServiceHandle, SERVICES_MAX> = heapless::Vec::new();
    let mut chars: heapless::Vec<Characteristic<[u8]>, CHARS_MAX> = heapless::Vec::new();
    let mut listener: Option<NotificationListener<'_, _>> = None;
    let mut buf = [0u8; ble::MSG_MAX];
    let mut out = [0u8; ble::MSG_MAX];
    loop {
        let next = select(request(reports, &mut buf), async {
            match listener.as_mut() {
                Some(l) => l.next().await,
                None => core::future::pending().await,
            }
        })
        .await;
        let n = match next {
            Either::First(None) => return true,
            Either::First(Some(n)) => n,
            Either::Second(note) => {
                let data = note.as_ref();
                let len = data.len().min(ble::MSG_MAX - 4);
                out[0] = ble::NOTIFY;
                out[1..3].copy_from_slice(&note.handle().to_le_bytes());
                out[3] = note.is_indication() as u8;
                out[4..4 + len].copy_from_slice(&data[..len]);
                reports.send_wait(&out[..4 + len]).await;
                continue;
            }
        };
        let req = &buf[..n];
        let op = req[0];
        match op {
            ble::DISCONNECT => return false,
            ble::SERVICES => match client.services().await {
                Ok(list) => {
                    services = list;
                    chars.clear();
                    for (i, s) in services.iter().enumerate() {
                        let range = s.handle_range();
                        let uuid = s.uuid();
                        let raw = uuid.as_raw();
                        out[0] = ble::SERVICE;
                        out[1] = i as u8;
                        out[2..4].copy_from_slice(&range.start().to_le_bytes());
                        out[4..6].copy_from_slice(&range.end().to_le_bytes());
                        out[6..6 + raw.len()].copy_from_slice(raw);
                        reports.send_wait(&out[..6 + raw.len()]).await;
                    }
                    reports.send_wait(&[ble::END]).await;
                }
                Err(e) => {
                    event!(Warn, "ble.gatt_failed", op = op, error = ble_error(&e));
                    reports.fail(op, STACK_FAILED).await;
                }
            },
            ble::CHARS if n >= 2 => {
                let Some(s) = services.get(req[1] as usize) else {
                    reports.fail(op, NOT_FOUND).await;
                    continue;
                };
                match client.characteristics::<CHARS_MAX>(s).await {
                    Ok(list) => {
                        // Kept from every service asked for (until the next
                        // `SERVICES`): `WRITE` and `SUBSCRIBE` look here.
                        for c in list.iter() {
                            let raw = c.uuid.as_raw();
                            out[0] = ble::CHAR;
                            out[1..3].copy_from_slice(&c.handle.to_le_bytes());
                            out[3..5].copy_from_slice(&c.cccd_handle.unwrap_or(0).to_le_bytes());
                            out[5] = c.props.to_raw();
                            out[6..6 + raw.len()].copy_from_slice(raw);
                            reports.send_wait(&out[..6 + raw.len()]).await;
                        }
                        for c in list {
                            if !chars.iter().any(|k| k.handle == c.handle) {
                                let _ = chars.push(c);
                            }
                        }
                        reports.send_wait(&[ble::END]).await;
                    }
                    Err(e) => {
                        event!(Warn, "ble.gatt_failed", op = op, error = ble_error(&e));
                        reports.fail(op, STACK_FAILED).await;
                    }
                }
            }
            ble::READ if n >= 3 => {
                let handle = u16_at(req, 1);
                match client.read_handle(handle, &mut out[3..]).await {
                    Ok(len) => {
                        out[0] = ble::VALUE;
                        out[1..3].copy_from_slice(&handle.to_le_bytes());
                        reports.send_wait(&out[..3 + len]).await;
                    }
                    Err(e) => {
                        event!(Warn, "ble.gatt_failed", op = op, error = ble_error(&e));
                        reports.fail(op, STACK_FAILED).await;
                    }
                }
            }
            ble::WRITE if n >= 4 => {
                let handle = u16_at(req, 1);
                let r = if req[3] != 0 {
                    client.write_handle(handle, &req[4..]).await
                } else {
                    match chars.iter().find(|c| c.handle == handle) {
                        Some(c) => client.write_characteristic_without_response(c, &req[4..]).await,
                        None => {
                            reports.fail(op, NOT_FOUND).await;
                            continue;
                        }
                    }
                };
                match r {
                    Ok(()) => {
                        let mut m = [ble::WRITTEN, 0, 0];
                        m[1..3].copy_from_slice(&handle.to_le_bytes());
                        reports.send_wait(&m).await;
                    }
                    Err(e) => {
                        event!(Warn, "ble.gatt_failed", op = op, error = ble_error(&e));
                        reports.fail(op, STACK_FAILED).await;
                    }
                }
            }
            ble::SUBSCRIBE if n >= 4 => {
                let handle = u16_at(req, 1);
                let Some(c) = chars.iter().find(|c| c.handle == handle) else {
                    reports.fail(op, NOT_FOUND).await;
                    continue;
                };
                // One subscription: the one before goes first.
                listener = None;
                match client.subscribe(c, req[3] != 0).await {
                    Ok(l) => {
                        listener = Some(l);
                        let mut m = [ble::SUBSCRIBED, 0, 0];
                        m[1..3].copy_from_slice(&handle.to_le_bytes());
                        reports.send_wait(&m).await;
                    }
                    Err(e) => {
                        event!(Warn, "ble.gatt_failed", op = op, error = ble_error(&e));
                        reports.fail(op, STACK_FAILED).await;
                    }
                }
            }
            _ => reports.fail(op, if op == ble::SCAN || op == ble::CONNECT { NOT_NOW } else { BAD_REQUEST }).await,
        }
        let _ = conn;
    }
}
