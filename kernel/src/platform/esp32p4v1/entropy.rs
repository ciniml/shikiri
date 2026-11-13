// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The RNG's entropy source (doc/47): SAR ADC1 converting an internal
//! channel continuously, which makes `LP_SYSTEM_REG_RNG_DATA_REG` a true
//! random number generator. The sequence is ESP-IDF 6.0's
//! `bootloader_random_enable` (`bootloader_support/src/
//! bootloader_random_esp32p4.c`), with the fields from `esp_hal_ana_conv/
//! esp32p4/include/hal/adc_ll.h`, `hal/esp32p4/include/hal/regi2c_ctrl_ll.h`
//! and `soc/esp32p4/register/hw_ver1/soc/{hp_sys_clkrst,adc,lp_adc,pmu}_reg.h`.
//! It touches no pad: channel 10 is past ADC1's 8 GPIO channels, and the
//! pad bitmap stays 0. The source stays on, as in an ESP-IDF application
//! that keeps it; the ADC is not used for anything else here.

use super::psram::{
    CK_EN_LP_I2CMST, I2C_SAR_ADC, LPPERI_CLK_EN, PMU_RF_PWC, clear_bits, delay_us, regi2c_write_mask, set_bits,
    set_field,
};
use super::{RNG_DATA, rd};

const HP_SYS_CLKRST: usize = 0x500E_6000;
const HP_RST_EN2: usize = HP_SYS_CLKRST + 0xC8;
const RST_EN_ADC: u32 = 1 << 10;
const SOC_CLK_CTRL2: usize = HP_SYS_CLKRST + 0x1C;
const ADC_APB_CLK_EN: u32 = 1 << 5;
const PERI_CLK_CTRL22: usize = HP_SYS_CLKRST + 0x9C;
const ADC_CLK_SRC_SEL_S: u32 = 30; // 2 bits, 0 = XTAL
const PERI_CLK_CTRL23: usize = HP_SYS_CLKRST + 0xA0;
const ADC_CLK_EN: u32 = 1 << 0;
const ADC_CLK_DIV_NUM_S: u32 = 1;
const ADC_CLK_DIV_NUMERATOR_S: u32 = 9;
const ADC_CLK_DIV_DENOMINATOR_S: u32 = 17;

const PMU_PERIF_I2C_RSTB: u32 = 1 << 26;
const PMU_XPD_PERIF_I2C: u32 = 1 << 27;

const ADC: usize = 0x500D_E000;
const ADC_CTRL: usize = ADC;
const SAR_CLK_GATED: u32 = 1 << 5;
const SAR_CLK_DIV_S: u32 = 6; // 8 bits
const SAR1_PATT_LEN_S: u32 = 14; // 4 bits
const XPD_SAR1_FORCE_S: u32 = 26; // 2 bits, 3 = power up
const ADC_CTRL2: usize = ADC + 0x4;
const TIMER_SEL: u32 = 1 << 11;
const TIMER_TARGET_S: u32 = 12; // 12 bits
const TIMER_EN: u32 = 1 << 24;
const SAR1_PATT_TAB1: usize = ADC + 0x18;
const SAR1_DATA_STATUS: usize = ADC + 0x40;

const LP_ADC: usize = 0x5012_7000;
const MEAS1_CTRL2: usize = LP_ADC + 0xC;
const MEAS1_START_FORCE: u32 = 1 << 18;
const SAR1_EN_PAD_FORCE: u32 = 1 << 31;
const MEAS1_MUX: usize = LP_ADC + 0x10;
const SAR1_DIG_FORCE: u32 = 1 << 31;

// regi2c_saradc.h
const SAR1_INITIAL_CODE_LOW: u32 = 0x0; // [7:0]
const SAR1_INITIAL_CODE_HIGH: u32 = 0x1; // [3:0]
const SAR_DTEST_VDD_GRP1: u32 = 0x9; // [3:0]
const SAR_ENT_VDD_GRP1: u32 = 0x9; // [4:4]
const INIT_CODE: u32 = 2166;

/// ADC1, channel 10, 12 dB (atten 3): one pattern-table entry.
const PATTERN: u32 = (10 << 2) | 3;

/// Starts the entropy source and runs the start-up health test (§6);
/// returns the distinct values among a few ADC samples, which shows that it
/// converts.
pub fn enable() -> Result<u32, &'static str> {
    // Reset the ADC, clock it (from XTAL, undivided) and its APB side.
    set_bits(HP_RST_EN2, RST_EN_ADC);
    clear_bits(HP_RST_EN2, RST_EN_ADC);
    set_bits(SOC_CLK_CTRL2, ADC_APB_CLK_EN);
    set_bits(PERI_CLK_CTRL23, ADC_CLK_EN);
    set_field(PERI_CLK_CTRL22, ADC_CLK_SRC_SEL_S, 2, 0);
    set_bits(ADC_CTRL, SAR_CLK_GATED);
    set_field(PERI_CLK_CTRL23, ADC_CLK_DIV_NUM_S, 8, 0);
    set_field(PERI_CLK_CTRL23, ADC_CLK_DIV_NUMERATOR_S, 8, 0);
    set_field(PERI_CLK_CTRL23, ADC_CLK_DIV_DENOMINATOR_S, 8, 0);

    // The SAR ADC's analog registers sit behind the PMU's peripheral regi2c.
    clear_bits(PMU_RF_PWC, PMU_PERIF_I2C_RSTB);
    set_bits(PMU_RF_PWC, PMU_XPD_PERIF_I2C);
    set_bits(PMU_RF_PWC, PMU_PERIF_I2C_RSTB);
    // The analog I2C master's clock stays on while the source runs.
    set_bits(LPPERI_CLK_EN, CK_EN_LP_I2CMST);

    regi2c_write_mask(&I2C_SAR_ADC, SAR_DTEST_VDD_GRP1, 3, 0, 0)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR_ENT_VDD_GRP1, 4, 4, 1)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR1_INITIAL_CODE_HIGH, 3, 0, (INIT_CODE >> 8) as u8)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR1_INITIAL_CODE_LOW, 7, 0, INIT_CODE as u8)?;

    // Entries 0..3 of the first table register, length 1.
    let tab = (0..4).fold(0, |t, i| t | (PATTERN << (18 - 6 * i)));
    set_field(SAR1_PATT_TAB1, 0, 24, tab);
    set_field(ADC_CTRL, SAR1_PATT_LEN_S, 4, 0);

    // ADC1 under the digital controller, powered on by software.
    set_bits(MEAS1_MUX, SAR1_DIG_FORCE);
    set_bits(MEAS1_CTRL2, MEAS1_START_FORCE | SAR1_EN_PAD_FORCE);
    set_bits(ADC_CTRL, SAR_CLK_GATED);
    set_field(ADC_CTRL, XPD_SAR1_FORCE_S, 2, 3);

    set_field(ADC_CTRL, SAR_CLK_DIV_S, 8, 15);
    set_field(ADC_CTRL2, TIMER_TARGET_S, 12, 100);
    set_bits(ADC_CTRL2, TIMER_SEL | TIMER_EN);

    let mut seen = [0u32; 8];
    let mut distinct = 0;
    for _ in 0..seen.len() {
        delay_us(20);
        let v = unsafe { rd(SAR1_DATA_STATUS) } & 0x1_FFFF;
        if !seen[..distinct as usize].contains(&v) {
            seen[distinct as usize] = v;
            distinct += 1;
        }
    }
    if !startup_test() {
        return Err("start-up health test failed");
    }
    Ok(distinct)
}

const SAR2_INITIAL_CODE_LOW: u32 = 0x3; // [7:0]
const SAR2_INITIAL_CODE_HIGH: u32 = 0x4; // [3:0]
const SAR2_PATT_TAB1: usize = ADC + 0x28;

/// Stops the entropy source, as `bootloader_random_disable` does (only to
/// compare the generator without it, doc/47 §7): the trigger off, the
/// pattern tables and the calibration cleared, the regi2c and its clock
/// off, the clock divider back to ESP-IDF's default, ADC1 back to the ULP
/// controller.
pub fn disable() -> Result<(), &'static str> {
    clear_bits(ADC_CTRL2, TIMER_EN);
    for i in 0..4 {
        set_field(SAR1_PATT_TAB1 + 4 * i, 0, 24, 0xFF_FFFF);
        set_field(SAR2_PATT_TAB1 + 4 * i, 0, 24, 0xFF_FFFF);
    }
    regi2c_write_mask(&I2C_SAR_ADC, SAR1_INITIAL_CODE_HIGH, 3, 0, 0)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR1_INITIAL_CODE_LOW, 7, 0, 0)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR2_INITIAL_CODE_HIGH, 3, 0, 0)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR2_INITIAL_CODE_LOW, 7, 0, 0)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR_DTEST_VDD_GRP1, 3, 0, 0)?;
    regi2c_write_mask(&I2C_SAR_ADC, SAR_ENT_VDD_GRP1, 4, 4, 0)?;
    clear_bits(PMU_RF_PWC, PMU_XPD_PERIF_I2C);
    clear_bits(LPPERI_CLK_EN, CK_EN_LP_I2CMST);
    set_field(PERI_CLK_CTRL23, ADC_CLK_DIV_NUM_S, 8, 4);
    set_field(PERI_CLK_CTRL23, ADC_CLK_DIV_NUMERATOR_S, 8, 0);
    set_field(PERI_CLK_CTRL23, ADC_CLK_DIV_DENOMINATOR_S, 8, 0);
    set_field(PERI_CLK_CTRL22, ADC_CLK_SRC_SEL_S, 2, 0);
    set_bits(ADC_CTRL, SAR_CLK_GATED);
    clear_bits(MEAS1_MUX, SAR1_DIG_FORCE);
    set_bits(MEAS1_CTRL2, MEAS1_START_FORCE | SAR1_EN_PAD_FORCE);
    Ok(())
}

// ---- health tests (NIST SP 800-90B §4.4; doc/47 §6) ----------------------
// On every word read from the RNG register. The cutoffs take a claimed
// min-entropy of 16 bits a word (measured: about 25, doc/47 §5.2) and a
// false-alarm rate of 2^-30: the repetition count test fails on 3 equal
// words in a row (C = 1 + ceil(30 / 16)), the adaptive proportion test
// when the first word of a 512-word window comes 5 times in it.
const RCT_CUTOFF: u32 = 3;
const APT_WINDOW: u32 = 512;
const APT_CUTOFF: u32 = 5;

struct Health {
    last: u32,
    run: u32,
    apt_first: u32,
    apt_seen: u32,
    apt_n: u32,
    failures: u32,
}

// Used under the kernel lock only.
static mut HEALTH: Health = Health { last: 0, run: 0, apt_first: 0, apt_seen: 0, apt_n: 0, failures: 0 };

/// Puts one word from the register through the tests; false when one
/// fails (logged: the first failure and then every 1024th).
pub fn check(v: u32) -> bool {
    let h = unsafe { &mut *core::ptr::addr_of_mut!(HEALTH) };
    let mut failed = None;
    if v == h.last {
        h.run += 1;
        if h.run >= RCT_CUTOFF {
            failed = Some("repetition");
            h.run = 1;
        }
    } else {
        h.last = v;
        h.run = 1;
    }
    if h.apt_n == 0 {
        h.apt_first = v;
        h.apt_seen = 1;
    } else if v == h.apt_first {
        h.apt_seen += 1;
        if h.apt_seen >= APT_CUTOFF {
            failed = Some("proportion");
            h.apt_n = APT_WINDOW - 1;
        }
    }
    h.apt_n = (h.apt_n + 1) % APT_WINDOW;
    let Some(test) = failed else { return true };
    if h.failures % 1024 == 0 {
        t5_abi::event!(Error, "rng.health_fail", test = test, failures = h.failures + 1, word = v);
    }
    h.failures += 1;
    false
}

/// Health test failures since boot.
pub fn failures() -> u32 {
    unsafe { (*core::ptr::addr_of!(HEALTH)).failures }
}

/// The start-up test (§4.3): 1024 words through both tests, from a fresh
/// state, before anything uses the generator.
fn startup_test() -> bool {
    let failures = failures();
    unsafe { *core::ptr::addr_of_mut!(HEALTH) = Health { last: !rd(RNG_DATA), run: 0, apt_first: 0, apt_seen: 0, apt_n: 0, failures } };
    for _ in 0..1024 {
        check(unsafe { rd(RNG_DATA) });
    }
    self::failures() == failures
}
