//! Conversions between counter ticks and nanoseconds.

use crate::platform::{Current, Platform};

const NS_PER_S: u64 = 1_000_000_000;

pub fn ticks_to_ns(ticks: u64) -> u64 {
    let hz = Current::TIMER_HZ;
    // Split to avoid overflowing ticks * 1e9 (16 MHz overflows after ~19 min).
    (ticks / hz) * NS_PER_S + (ticks % hz) * NS_PER_S / hz
}

/// Rounds up, so a sleep never ends early.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let hz = Current::TIMER_HZ;
    (ns / NS_PER_S).saturating_mul(hz) + ((ns % NS_PER_S) * hz).div_ceil(NS_PER_S)
}

pub fn ms_to_ticks(ms: u64) -> u64 {
    ms * Current::TIMER_HZ / 1000
}
