//! ESP32-P4-Function-EV-Board v1.5.x (doc/58 §2, §3.1): no display or
//! touch is fitted, no IO expander or PMIC. Its schematic (v1.52) wires
//! the ESP32-C6-MINI-1 on SDIO slot 1 (CLK 18, CMD 19, D0-D3 14-17) with
//! C6_EN on GPIO54, and the IP101GRI Ethernet PHY on GPIO28-31, 34, 35,
//! 49-52 (RMII, MDC 31, MDIO 52, REF_CLK in on 50, reset 51).
//!
//! The Tab5's I2C bus (GPIO31/32) and keyboard INT (GPIO50) are not
//! touched here: on this board they are the Ethernet's MDC and REF_CLK.

use crate::platform::{Panel, TouchPoint};

pub const NAME: &str = "p4evb";
pub const I2C: bool = false;
pub const KBD: bool = false;
pub const SDIO: super::sdio::Pins = super::sdio::Pins { clk: 18, cmd: 19, data: [14, 15, 16, 17], chip_en: 54 };

/// Nothing to set up: no panel is fitted (an LCD on the MIPI DSI connector
/// is optional and not supported).
pub fn init() -> Panel {
    Panel::None
}

pub fn touch_read(_out: &mut [TouchPoint; 5]) -> Result<usize, &'static str> {
    Err("no touch on this board")
}

/// The card's power: SD_PWRn on GPIO45 (schematic v1.52), and the card's
/// IO supply appears to involve the chip's LDO VO4. Neither is confirmed
/// on the board yet (doc/58 R0), so nothing is driven: the card works
/// only if the bootloader left it powered.
pub fn sd_power_on() {}

/// The C6 runs from the board's 3.3 V: there is no power switch, only
/// C6_EN (`sdio::chip_en`). Reports it as already powered, so the reset
/// pulse is the short one.
pub fn wlan_power_on() -> Result<bool, &'static str> {
    Ok(true)
}
