// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! ESP32-P4-Function-EV-Board v1.5.x (doc/58 §2, §3.1): no display or
//! touch is fitted, no IO expander or PMIC. Its schematic (v1.52) wires
//! the ESP32-C6-MINI-1 on SDIO slot 1 (CLK 18, CMD 19, D0-D3 14-17) with
//! C6_EN on GPIO54, and the IP101GRI Ethernet PHY on GPIO28-31, 34, 35,
//! 49-52 (RMII, MDC 31, MDIO 52, REF_CLK in on 50, reset 51).
//!
//! The Tab5's I2C bus (GPIO31/32) and keyboard INT (GPIO50) are not
//! touched here: on this board they are the Ethernet's MDC and REF_CLK.

use t5_abi::event;

use super::psram::delay_us;
use super::{rd, wr};
use crate::platform::{Panel, TouchPoint};

pub const NAME: &str = "p4evb";
pub const I2C: bool = false;
pub const KBD: bool = false;
/// No internal I²C bus set up (doc/69).
pub const INTERNAL_I2C: bool = false;
/// The IP101GRI on RMII (`emac.rs`).
pub const ETH: bool = true;
pub const SDIO: super::sdio::Pins = super::sdio::Pins { clk: 18, cmd: 19, data: [14, 15, 16, 17], chip_en: 54 };

/// Nothing to set up: no panel is fitted (an LCD on the MIPI DSI connector
/// is optional and not supported).
pub fn init() -> Panel {
    Panel::None
}

pub fn touch_read(_out: &mut [TouchPoint; 5]) -> Result<usize, &'static str> {
    Err("no touch on this board")
}

/// The SD pins' supply (GPIO39-44): the chip's LDO VO4 passing the 3.3 V
/// rail through, as the factory firmware does at every start ("LDO Channel
/// 4 powered on at 3.3V"); approved for the EVB only (user, 2026-11-06,
/// doc/58). The card's own power (SD_PWRn, GPIO45) is left as the board
/// has it. Order as ESP-IDF's esp_ldo_acquire_channel (ldo_ll.h: VO4 is
/// unit 3, PMU ext_ldo[4] = EXT_LDO_P1_0P2A).
pub fn sd_power_on() {
    const LDO: usize = 0x5011_51D8;
    const ANA: usize = 0x5011_51DC;
    const FORCE_TIEH_SEL: u32 = 1 << 7;
    const XPD: u32 = 1 << 8;
    const TIEH_SEL: u32 = 0b111 << 9;
    const TIEH: u32 = 1 << 14;
    const EN_VDET: u32 = 1 << 26;
    const EN_CUR_LIM: u32 = 1 << 27;
    unsafe {
        let (ldo, ana) = (rd(LDO), rd(ANA));
        event!(Info, "board.sd_ldo", ldo = ldo, ana = ana);
        if ldo & XPD != 0 && ldo & TIEH != 0 {
            return;
        }
        wr(ANA, ana | EN_CUR_LIM);
        // TIEH: the 3.3 V rail, not Vref x Mul (dref and mul do not apply).
        wr(LDO, (rd(LDO) | TIEH | FORCE_TIEH_SEL) & !TIEH_SEL);
        wr(ANA, rd(ANA) | EN_VDET);
        wr(LDO, rd(LDO) | XPD);
        wr(ANA, rd(ANA) & !EN_CUR_LIM);
    }
    delay_us(10_000);
}

/// The C6 runs from the board's 3.3 V: there is no power switch, only
/// C6_EN (`sdio::chip_en`). Reports it as already powered, so the reset
/// pulse is the short one.
pub fn wlan_power_on() -> Result<bool, &'static str> {
    Ok(true)
}
