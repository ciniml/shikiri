// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The peripheral role (doc/48 M4): the client's GATT service, built at
//! run time behind the GAP and GATT services, advertised until one central
//! connects; its writes go to the client (each after the connection's
//! ATT MTU, when that has changed), which sets values and sends
//! notifications or indications.

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::Duration;
use t5_rt::{ble, event};
use trouble_host::prelude::*;

use crate::{BAD_REQUEST, Host, NOT_FOUND, Pool, Reports, STACK_FAILED, request, u16_at};

/// Attributes: GAP and GATT (7), the service (1), and per characteristic
/// its declaration, value and CCCD (3).
const TABLE: usize = 8 + 3 * ble::CHARS_MAX;
const NAME_MAX: usize = 22;
const ADV_MAX: usize = 31;

type Value = heapless::Vec<u8, { ble::VALUE_MAX }>;

/// An `ADVERTISE` request, copied out of the message.
struct Def {
    interval_ms: u16,
    name: [u8; NAME_MAX],
    name_len: usize,
    service: Uuid,
    chars: [(Uuid, u8); ble::CHARS_MAX],
    count: usize,
    adv: [u8; ADV_MAX],
    adv_len: usize,
    scan: [u8; ADV_MAX],
    scan_len: usize,
}

fn uuid(b: &[u8]) -> Option<Uuid> {
    match b.len() {
        2 => Some(Uuid::new_short(u16::from_le_bytes([b[0], b[1]]))),
        16 => Some(Uuid::new_long(b.try_into().ok()?)),
        _ => None,
    }
}

fn parse(req: &[u8]) -> Option<Def> {
    let mut at = 1;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = req.get(at..at + n)?;
        at += n;
        Some(s)
    };
    let interval = take(2)?;
    let interval_ms = u16::from_le_bytes([interval[0], interval[1]]).clamp(20, 10_240);
    let n = take(1)?[0] as usize;
    let name_in = take(n)?;
    if n > NAME_MAX || core::str::from_utf8(name_in).is_err() {
        return None;
    }
    let mut name = [0u8; NAME_MAX];
    name[..n].copy_from_slice(name_in);
    let n = take(1)?[0] as usize;
    let service = uuid(take(n)?)?;
    let count = take(1)?[0] as usize;
    if count == 0 || count > ble::CHARS_MAX {
        return None;
    }
    let mut chars = [(Uuid::new_short(0), 0u8); ble::CHARS_MAX];
    for c in chars.iter_mut().take(count) {
        let n = take(1)?[0] as usize;
        let u = uuid(take(n)?)?;
        *c = (u, take(1)?[0]);
    }
    let mut adv = [0u8; ADV_MAX];
    let adv_len = take(1)?[0] as usize;
    adv.get_mut(..adv_len)?.copy_from_slice(take(adv_len)?);
    let mut scan = [0u8; ADV_MAX];
    let scan_len = take(1)?[0] as usize;
    scan.get_mut(..scan_len)?.copy_from_slice(take(scan_len)?);
    Some(Def { interval_ms, name, name_len: name_in.len(), service, chars, count, adv, adv_len, scan, scan_len })
}

/// `ADVERTISE` until a central has come and gone, or `STOP`; true if the
/// client left.
pub async fn advertise(stack: &Host<'_>, reports: &Reports, req: &[u8]) -> bool {
    let Some(def) = parse(req) else {
        reports.fail(ble::ADVERTISE, BAD_REQUEST).await;
        return false;
    };
    let name = core::str::from_utf8(&def.name[..def.name_len]).unwrap_or("tab5");
    let mut stores = [[0u8; ble::VALUE_MAX]; ble::CHARS_MAX];
    let mut table: AttributeTable<'_, NoopRawMutex, TABLE> = AttributeTable::new();
    if GapConfig::default(name).build(&mut table).is_err() {
        reports.fail(ble::ADVERTISE, BAD_REQUEST).await;
        return false;
    }
    let mut chars: heapless::Vec<Characteristic<Value>, { ble::CHARS_MAX }> = heapless::Vec::new();
    {
        let mut service = table.add_service(Service::new(def.service));
        for (&(uuid, props), store) in def.chars[..def.count].iter().zip(stores.iter_mut()) {
            let c = service.add_characteristic(uuid, props, Value::new(), store).build();
            let _ = chars.push(c);
        }
        service.build();
    }
    let server = AttributeServer::<NoopRawMutex, Pool, TABLE, 1>::new(table);

    let mut m = [0u8; 2 + 4 * ble::CHARS_MAX];
    m[0] = ble::ADVERTISING;
    m[1] = chars.len() as u8;
    for (i, c) in chars.iter().enumerate() {
        m[2 + 4 * i..4 + 4 * i].copy_from_slice(&c.handle.to_le_bytes());
        m[4 + 4 * i..6 + 4 * i].copy_from_slice(&c.cccd_handle.unwrap_or(0).to_le_bytes());
    }

    let mut peripheral = stack.peripheral();
    let interval = Duration::from_millis(def.interval_ms as u64);
    let params = AdvertisementParameters { interval_min: interval, interval_max: interval, ..Default::default() };
    let data = Advertisement::ConnectableScannableUndirected { adv_data: &def.adv[..def.adv_len], scan_data: &def.scan[..def.scan_len] };
    let advertiser = match peripheral.advertise(&params, data).await {
        Ok(a) => a,
        Err(e) => {
            event!(Warn, "ble.advertise_failed", error = crate::ble_error(&e));
            reports.fail(ble::ADVERTISE, STACK_FAILED).await;
            return false;
        }
    };
    event!(Info, "ble.advertising", name = name, chars = chars.len() as u32, interval_ms = def.interval_ms as u32);
    reports.send_wait(&m[..2 + 4 * chars.len()]).await;

    // Until a central connects, the client stops, or it leaves.
    let mut buf = [0u8; ble::MSG_MAX];
    let conn = match select(advertiser.accept(), request(reports, &mut buf)).await {
        Either::First(Ok(c)) => c,
        Either::First(Err(_)) => {
            reports.fail(ble::ADVERTISE, STACK_FAILED).await;
            return false;
        }
        Either::Second(None) => return true,
        Either::Second(Some(_)) => {
            // Any request (`STOP`) ends the advertising.
            reports.send_wait(&[ble::DISCONNECTED]).await;
            return false;
        }
    };
    let peer = conn.peer_address();
    let gatt = match conn.with_attribute_server(&server) {
        Ok(g) => g,
        Err(_) => {
            reports.fail(ble::ADVERTISE, STACK_FAILED).await;
            return false;
        }
    };
    let mut p = [0u8; 8];
    p[0] = ble::PEER;
    p[1] = peer.kind.into_inner();
    for (i, b) in peer.addr.raw().iter().rev().enumerate() {
        p[2 + i] = *b;
    }
    event!(Info, "ble.peer", addr = &p[2..8], addr_type = p[1]);
    reports.send_wait(&p).await;

    let mut out = [0u8; ble::MSG_MAX];
    let mut mtu = 0;
    loop {
        let next = select(gatt.next(), request(reports, &mut buf)).await;
        match next {
            Either::First(GattConnectionEvent::Disconnected { .. }) => {
                event!(Info, "ble.peer_gone");
                reports.send_wait(&[ble::DISCONNECTED]).await;
                return false;
            }
            Either::First(GattConnectionEvent::Gatt { event }) => {
                // Writes (CCCDs too) go to the client after the reply.
                let mut len = 0;
                if let GattEvent::Write(w) = &event {
                    let handle = w.handle();
                    w.with_data(|_, d| {
                        let d = &d[..d.len().min(ble::MSG_MAX - 3)];
                        out[0] = ble::PEER_WRITE;
                        out[1..3].copy_from_slice(&handle.to_le_bytes());
                        out[3..3 + d.len()].copy_from_slice(d);
                        len = 3 + d.len();
                    });
                }
                if let Ok(reply) = event.accept() {
                    reply.send().await;
                }
                if len > 0 {
                    let now = gatt.raw().att_mtu();
                    if now != mtu {
                        mtu = now;
                        let [a, b] = mtu.to_le_bytes();
                        reports.send_wait(&[ble::MTU, a, b]).await;
                    }
                    reports.send_wait(&out[..len]).await;
                }
            }
            Either::First(_) => {}
            Either::Second(None) => {
                gatt.raw().disconnect();
                return true;
            }
            Either::Second(Some(n)) => {
                let req = &buf[..n];
                let op = req[0];
                let char_at = |h: u16| chars.iter().find(|c| c.handle == h);
                match op {
                    ble::SET if n >= 3 => {
                        let handle = u16_at(req, 1);
                        let Some(c) = char_at(handle) else {
                            reports.fail(op, NOT_FOUND).await;
                            continue;
                        };
                        let value = Value::from_slice(&req[3..]).unwrap_or_default();
                        match c.set(&server, &value) {
                            Ok(()) => reports.send_wait(&[ble::WRITTEN, req[1], req[2]]).await,
                            Err(_) => reports.fail(op, STACK_FAILED).await,
                        }
                    }
                    ble::SEND if n >= 4 => {
                        let handle = u16_at(req, 1);
                        let Some(c) = char_at(handle) else {
                            reports.fail(op, NOT_FOUND).await;
                            continue;
                        };
                        let data = &req[4..];
                        let r = if req[3] != 0 { c.indicate_raw(&gatt, data, true).await } else { c.notify_raw(&gatt, data, true).await };
                        match r {
                            Ok(()) => reports.send_wait(&[ble::SENT, req[1], req[2]]).await,
                            Err(_) => reports.fail(op, STACK_FAILED).await,
                        }
                    }
                    ble::STOP | ble::DISCONNECT => {
                        gatt.raw().disconnect();
                        reports.send_wait(&[ble::DISCONNECTED]).await;
                        return false;
                    }
                    _ => reports.fail(op, BAD_REQUEST).await,
                }
            }
        }
    }
}
