// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! PSRAM bring-up for the Tab5's 32 MiB AP Memory HEX (16-line) PSRAM at
//! 200 MHz, after ESP-IDF 6.0 (doc/research/m4-psram-and-clock.md §2):
//! LDO VO2 at 1.8 V, MPLL at 400 MHz, the PSRAM MSPI controllers (MSPI2 for
//! the cache, MSPI3 for direct commands), the chip's mode registers, the AXI
//! (cache) side, DQS timing tuning, the MMU map at 0x4800_0000, and a memory
//! test. Every step logs an event, so a hang shows where it stopped (the
//! RWDT resets the chip, and the next boot replays the log).
//!
//! Register addresses and fields are from
//! `components/soc/esp32p4/register/hw_ver1/soc/*_reg.h`; the sequence from
//! `esp_psram/device/esp_psram_impl_ap_hex.c`,
//! `esp_hal_mspi/esp32p4/include/hal/psram_ctrlr_ll.h` and `mspi_ll.h`,
//! `esp_hw_support/mspi_timing_tuning/` (DQS tuning), `hal/esp32p4/include/
//! hal/ldo_ll.h` and `clk_tree_ll.h` (LDO, MPLL) and
//! `esp_rom/patches/esp_rom_regi2c_esp32p4.c` (regi2c).

use t5_abi::event;
use t5_abi::log::Addr;

use super::{Esp32p4v1, rd, wr};
use crate::platform::Platform;

pub(super) fn set_bits(addr: usize, mask: u32) {
    unsafe { wr(addr, rd(addr) | mask) }
}

pub(super) fn clear_bits(addr: usize, mask: u32) {
    unsafe { wr(addr, rd(addr) & !mask) }
}

pub(super) fn set_field(addr: usize, shift: u32, width: u32, v: u32) {
    let mask = ((1u32 << width) - 1) << shift;
    unsafe { wr(addr, (rd(addr) & !mask) | ((v << shift) & mask)) }
}

pub(super) fn delay_us(us: u64) {
    let end = Esp32p4v1::now() + us * 16;
    while Esp32p4v1::now() < end {}
}

/// Polls `done` for up to `us` microseconds.
pub(super) fn wait(us: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = Esp32p4v1::now() + us * 16;
    while !done() {
        if Esp32p4v1::now() > end {
            return false;
        }
    }
    true
}

// ---- power: LDO VO2 (unit 1 = PMU ext_ldo[3]; ldo_ll.h:155-216) ----------
const PMU_EXT_LDO_P1_0P1A: usize = 0x5011_51D0;
const PMU_EXT_LDO_P1_0P1A_ANA: usize = 0x5011_51D4;
const LDO_FORCE_TIEH_SEL: u32 = 1 << 7;
const LDO_XPD: u32 = 1 << 8;
const LDO_TIEH_SEL: u32 = 0b111 << 9;
const LDO_TIEH: u32 = 1 << 14;
const LDO_ANA_MUL_S: u32 = 23;
const LDO_ANA_EN_VDET: u32 = 1 << 26;
const LDO_ANA_DREF_S: u32 = 28;
// eFuse calibration of VO2 at 1.8 V (efuse_struct.h:614-638).
const EFUSE_RD_MAC_SYS_2: usize = 0x5012_D04C;
const EFUSE_RD_MAC_SYS_3: usize = 0x5012_D050;

/// Output in mV for `dref`/`mul` without eFuse trimming (ldo_ll.h:58-61).
fn ldo_mv(dref: u32, mul: u32) -> u32 {
    let vref_20 = if dref < 9 { 10 + dref } else { 20 + (dref - 9) * 2 };
    vref_20 * 50 * (4 + mul) / 4
}

fn ldo_vo2_on() -> Result<(), &'static str> {
    let (e_dref, e_mul) = unsafe { (rd(EFUSE_RD_MAC_SYS_2) >> 28, (rd(EFUSE_RD_MAC_SYS_3) >> 3) & 7) };
    // Without calibration: 0.9 V * (1 + 4/4) = 1.8 V, what ESP-IDF's search
    // finds (ldo_ll_voltage_to_dref_mul).
    let (dref, mul, efuse) = if e_dref != 0 && e_mul != 0 { (e_dref, e_mul, true) } else { (8, 4, false) };
    let mv = ldo_mv(dref, mul);
    let before = unsafe { (rd(PMU_EXT_LDO_P1_0P1A), rd(PMU_EXT_LDO_P1_0P1A_ANA)) };
    event!(Info, "psram.ldo", dref = dref, mul = mul, efuse = efuse, nominal_mv = mv, ldo = before.0, ana = before.1);
    // A guard against a bad calibration value: the PSRAM is a 1.8 V part.
    // The eFuse pair is the chip's own trim for 1.8 V, used as is by
    // ESP-IDF (ldo_ll.h:123-136); the nominal formula ignores the trim, so
    // it reads high for it: 2000 mV for the EVB v1.5.2's chip v1.3 (dref 6,
    // mul 6), whose factory firmware runs its PSRAM on that pair (doc/58).
    let range = if efuse { 1700..=2050 } else { 1700..=1950 };
    if !range.contains(&mv) {
        return Err("VO2 setting out of range");
    }
    unsafe {
        clear_bits(PMU_EXT_LDO_P1_0P1A, LDO_TIEH);
        let ana = rd(PMU_EXT_LDO_P1_0P1A_ANA) & !((0xF << LDO_ANA_DREF_S) | (0b111 << LDO_ANA_MUL_S));
        wr(PMU_EXT_LDO_P1_0P1A_ANA, ana | (dref << LDO_ANA_DREF_S) | (mul << LDO_ANA_MUL_S) | LDO_ANA_EN_VDET);
        let ldo = rd(PMU_EXT_LDO_P1_0P1A) & !LDO_TIEH_SEL;
        wr(PMU_EXT_LDO_P1_0P1A, ldo | LDO_FORCE_TIEH_SEL | LDO_XPD);
    }
    delay_us(100);
    Ok(())
}

// ---- MPLL at 400 MHz through regi2c (clk_tree_ll.h:150-160,450-468) -------
pub(super) const PMU_RF_PWC: usize = 0x5011_515C;
const PMU_MSPI_PHY_XPD: u32 = 1 << 24;
const LP_CLKRST_HP_CLK_CTRL: usize = 0x5011_1040;
const HP_MPLL_500M_CLK_EN: u32 = 1 << 28;
pub(super) const LPPERI_CLK_EN: usize = 0x5012_0000;
pub(super) const CK_EN_LP_I2CMST: u32 = 1 << 27;
const HP_SYS_CLKRST: usize = 0x500E_6000;
const ANA_PLL_CTRL0: usize = HP_SYS_CLKRST + 0xBC;
const MSPI_CAL_END: u32 = 1 << 8;
const MSPI_CAL_STOP: u32 = 1 << 9;
// Analog I2C master (i2c_ana_mst_reg.h; esp_rom_regi2c_esp32p4.c:21-80).
const I2C_ANA_MST: usize = 0x5012_4000;
const I2C0_CTRL: usize = I2C_ANA_MST;
const ANA_CONF1: usize = I2C_ANA_MST + 0x1C;
const ANA_CONF2: usize = I2C_ANA_MST + 0x20;
const REGI2C_BUSY: u32 = 1 << 25;
const REGI2C_WRITE: u32 = 1 << 24;

/// A regi2c slave block: its id and its master-select bit in ANA_CONF2.
pub(super) struct Regi2c {
    id: u32,
    sel: u32,
}

const I2C_MPLL: Regi2c = Regi2c { id: 0x63, sel: 1 << 9 };
pub(super) const I2C_SAR_ADC: Regi2c = Regi2c { id: 0x69, sel: 1 << 7 };
const I2C_MPLL_IR_CAL_RSTB: u32 = 1;
const I2C_MPLL_DIV_REG: u32 = 2;
const I2C_MPLL_DHREF: u32 = 3;
pub const MPLL_MHZ: u32 = 400;

fn regi2c_select(slave: &Regi2c) {
    clear_bits(ANA_CONF2, 0x00FF_FFFF);
    clear_bits(ANA_CONF1, 0x00FF_FFFF);
    set_bits(ANA_CONF2, slave.sel);
}

pub(super) fn regi2c_read(slave: &Regi2c, reg: u32) -> Result<u8, &'static str> {
    regi2c_select(slave);
    let idle = || unsafe { rd(I2C0_CTRL) & REGI2C_BUSY == 0 };
    if !wait(1000, idle) {
        return Err("regi2c busy");
    }
    unsafe { wr(I2C0_CTRL, slave.id | (reg << 8)) };
    if !wait(1000, idle) {
        return Err("regi2c busy");
    }
    Ok((unsafe { rd(I2C0_CTRL) } >> 16) as u8)
}

pub(super) fn regi2c_write(slave: &Regi2c, reg: u32, data: u8) -> Result<(), &'static str> {
    regi2c_select(slave);
    let idle = || unsafe { rd(I2C0_CTRL) & REGI2C_BUSY == 0 };
    if !wait(1000, idle) {
        return Err("regi2c busy");
    }
    unsafe { wr(I2C0_CTRL, slave.id | (reg << 8) | REGI2C_WRITE | ((data as u32) << 16)) };
    if !wait(1000, idle) {
        return Err("regi2c busy");
    }
    Ok(())
}

/// Writes bits `msb..=lsb` of a regi2c register (regi2c_write_mask_impl).
pub(super) fn regi2c_write_mask(slave: &Regi2c, reg: u32, msb: u32, lsb: u32, data: u8) -> Result<(), &'static str> {
    let mask = (((1u32 << (msb - lsb + 1)) - 1) << lsb) as u8;
    let old = regi2c_read(slave, reg)?;
    regi2c_write(slave, reg, (old & !mask) | ((data << lsb) & mask))
}

fn mpll_on() -> Result<(), &'static str> {
    set_bits(PMU_RF_PWC, PMU_MSPI_PHY_XPD);
    set_bits(LP_CLKRST_HP_CLK_CTRL, HP_MPLL_500M_CLK_EN);
    let i2c_clock = unsafe { rd(LPPERI_CLK_EN) } & CK_EN_LP_I2CMST;
    set_bits(LPPERI_CLK_EN, CK_EN_LP_I2CMST);
    clear_bits(ANA_PLL_CTRL0, MSPI_CAL_STOP);
    let r = (|| {
        let dhref = regi2c_read(&I2C_MPLL, I2C_MPLL_DHREF)?;
        regi2c_write(&I2C_MPLL, I2C_MPLL_DHREF, dhref | (3 << 4))?;
        let rstb = regi2c_read(&I2C_MPLL, I2C_MPLL_IR_CAL_RSTB)?;
        regi2c_write(&I2C_MPLL, I2C_MPLL_IR_CAL_RSTB, rstb & 0xDF)?;
        regi2c_write(&I2C_MPLL, I2C_MPLL_IR_CAL_RSTB, rstb | (1 << 5))?;
        // MPLL = XTAL 40 MHz * (div + 1) / (ref_div + 1).
        let (div, ref_div) = (MPLL_MHZ / 20 - 1, 1);
        regi2c_write(&I2C_MPLL, I2C_MPLL_DIV_REG, ((div << 3) | ref_div) as u8)?;
        let started = Esp32p4v1::now();
        if !wait(10_000, || unsafe { rd(ANA_PLL_CTRL0) } & MSPI_CAL_END != 0) {
            return Err("MPLL calibration timeout");
        }
        let cal_us = (Esp32p4v1::now() - started) / 16;
        let readback = regi2c_read(&I2C_MPLL, I2C_MPLL_DIV_REG)?;
        event!(Info, "psram.mpll", mhz = MPLL_MHZ, cal_us = cal_us, div_reg = readback);
        Ok(())
    })();
    set_bits(ANA_PLL_CTRL0, MSPI_CAL_STOP);
    if i2c_clock == 0 {
        clear_bits(LPPERI_CLK_EN, CK_EN_LP_I2CMST);
    }
    r
}

// ---- PSRAM controllers -----------------------------------------------------
const SOC_CLK_CTRL0: usize = HP_SYS_CLKRST + 0x14;
const PSRAM_SYS_CLK_EN: u32 = 1 << 31;
const PERI_CLK_CTRL00: usize = HP_SYS_CLKRST + 0x30;
const PSRAM_PLL_CLK_EN: u32 = 1 << 14;
const PSRAM_CORE_CLK_EN: u32 = 1 << 15;
const HP_RST_EN0: usize = HP_SYS_CLKRST + 0xC0;
/// Bits 22 and 24 are the flash MSPI: never touched.
const RST_DUAL_MSPI_AXI: u32 = 1 << 23;
const RST_DUAL_MSPI_APB: u32 = 1 << 25;

/// MSPI2: the cache (AXI) side (spi_mem_s_reg.h).
const MSPI2: usize = 0x5008_E000;
const M2_CTRL1: usize = MSPI2 + 0x0C;
const M2_CACHE_FCTRL: usize = MSPI2 + 0x3C;
const M2_CACHE_SCTRL: usize = MSPI2 + 0x40;
const M2_SRAM_CMD: usize = MSPI2 + 0x44;
const M2_SRAM_DRD_CMD: usize = MSPI2 + 0x48;
const M2_SRAM_DWR_CMD: usize = MSPI2 + 0x4C;
const M2_SRAM_CLK: usize = MSPI2 + 0x50;
const M2_ECC_CTRL: usize = MSPI2 + 0x174;
const M2_TIMING_CALI: usize = MSPI2 + 0x180;
const M2_SMEM_TIMING_CALI: usize = MSPI2 + 0x190;
const M2_SMEM_DDR: usize = MSPI2 + 0xD8;
const M2_SMEM_AC: usize = MSPI2 + 0x1A0;
const M2_MMU_ITEM_CONTENT: usize = MSPI2 + 0x37C;
const M2_MMU_ITEM_INDEX: usize = MSPI2 + 0x380;
/// MSPI3: direct commands (spi1_mem_s_reg.h).
const MSPI3: usize = 0x5008_F000;
const M3_CLOCK: usize = MSPI3 + 0x14;
const M3_DDR: usize = MSPI3 + 0xD4;
const VAR_DUMMY: u32 = 1 << 1;
const DLL_TIMING_CALI: u32 = 1 << 5;

/// MSPI pads (iomux_mspi_pin_reg.h:270-1317): 20 registers from +0x1C in
/// `mspi_ll_pin_t` order: D, Q, WP, HOLD, DQ4-7, DQS0, CK, CS, DQ8-15, DQS1.
const IOMUX_MSPI_PIN: usize = 0x500E_1200;
const PINS: usize = 20;
const PIN_DQS0: usize = 8;
const PIN_DQS1: usize = 19;

fn pin_reg(i: usize) -> usize {
    IOMUX_MSPI_PIN + 0x1C + 4 * i
}

fn is_dqs(i: usize) -> bool {
    i == PIN_DQS0 || i == PIN_DQS1
}

// AP HEX PSRAM at 200 MHz (esp_psram_impl_ap_hex.c:21-56).
const SYNC_READ: u16 = 0x0000;
const SYNC_WRITE: u16 = 0x8080;
const REG_READ: u16 = 0x4040;
const REG_WRITE: u16 = 0xC0C0;
const RD_DUMMY: u32 = 2 * (14 - 1);
const RD_REG_DUMMY: u32 = 2 * (7 - 1);
const WR_DUMMY: u32 = 2 * (7 - 1);
const RD_LATENCY: u8 = 4;
const WR_LATENCY: u8 = 1;
pub const FREQ_MHZ: u32 = 200;
const FIFO: usize = 64;

/// `esp_rom_spi_cmd_t` (esp_rom/esp32p4/include/esp32p4/rom/opi_flash.h).
#[repr(C)]
struct RomSpiCmd {
    cmd: u16,
    cmd_bit_len: u16,
    addr: *mut u32,
    addr_bit_len: u32,
    tx_data: *mut u32,
    tx_data_bit_len: u32,
    rx_data: *mut u32,
    rx_data_bit_len: u32,
    dummy_bit_len: u32,
}

// esp32p4.rom.ld:99-101 (same on v1 and v3).
const ROM_SPI_CMD_CONFIG: usize = 0x4FC0_0108;
const ROM_SPI_CMD_START: usize = 0x4FC0_010C;
const ROM_SPI_SET_OP_MODE: usize = 0x4FC0_0110;
const OPI_DTR_MODE: u32 = 7;
const SPI_PSRAM: i32 = 3;
const CS1: u8 = 1 << 1;

/// One direct command on MSPI3 (psram_ctrlr_ll_common_transaction), at
/// most `FIFO` bytes in or out.
fn transaction(cmd: u16, addr: u32, dummy: u32, tx: &[u8], rx: &mut [u8]) {
    let mut a = addr;
    let mut txw = [0u32; FIFO / 4];
    let mut rxw = [0u32; FIFO / 4];
    for (i, &b) in tx.iter().enumerate() {
        txw[i / 4] |= (b as u32) << (8 * (i % 4));
    }
    let mut c = RomSpiCmd {
        cmd,
        cmd_bit_len: 16,
        addr: &mut a,
        addr_bit_len: 32,
        tx_data: if tx.is_empty() { core::ptr::null_mut() } else { txw.as_mut_ptr() },
        tx_data_bit_len: tx.len() as u32 * 8,
        rx_data: if rx.is_empty() { core::ptr::null_mut() } else { rxw.as_mut_ptr() },
        rx_data_bit_len: rx.len() as u32 * 8,
        dummy_bit_len: dummy,
    };
    unsafe {
        let set_op_mode: unsafe extern "C" fn(i32, u32) = core::mem::transmute(ROM_SPI_SET_OP_MODE);
        let config: unsafe extern "C" fn(i32, *mut RomSpiCmd) = core::mem::transmute(ROM_SPI_CMD_CONFIG);
        let start: unsafe extern "C" fn(i32, *mut u8, u16, u8, bool) = core::mem::transmute(ROM_SPI_CMD_START);
        set_op_mode(SPI_PSRAM, OPI_DTR_MODE);
        config(SPI_PSRAM, &mut c);
        start(SPI_PSRAM, rxw.as_mut_ptr().cast(), rx.len() as u16, CS1, false);
    }
    for (i, b) in rx.iter_mut().enumerate() {
        *b = (rxw[i / 4] >> (8 * (i % 4))) as u8;
    }
}

fn reg_read(addr: u32, len: usize) -> [u8; 2] {
    let mut r = [0u8; 2];
    transaction(REG_READ, addr, RD_REG_DUMMY, &[], &mut r[..len]);
    r
}

fn reg_write(addr: u32, v: [u8; 2]) {
    transaction(REG_WRITE, addr, 0, &v, &mut []);
}

fn psram_write(mut addr: u32, data: &[u8]) {
    for chunk in data.chunks(FIFO) {
        transaction(SYNC_WRITE, addr, WR_DUMMY, chunk, &mut []);
        addr += chunk.len() as u32;
    }
}

fn psram_read(mut addr: u32, data: &mut [u8]) {
    for chunk in data.chunks_mut(FIFO) {
        let n = chunk.len() as u32;
        transaction(SYNC_READ, addr, RD_DUMMY, &[], chunk);
        addr += n;
    }
}

/// Bus clock = MPLL / `div` for both controllers (psram_ctrlr_ll.h:458-482).
fn set_bus_clock(div: u32) {
    let v = ((div - 1) << 16) | ((div / 2 - 1) << 8) | (div - 1);
    unsafe {
        wr(M2_SRAM_CLK, v);
        wr(M3_CLOCK, v);
    }
}

fn controller_init() {
    set_bits(SOC_CLK_CTRL0, PSRAM_SYS_CLK_EN);
    set_bits(PERI_CLK_CTRL00, PSRAM_PLL_CLK_EN);
    set_bits(HP_RST_EN0, RST_DUAL_MSPI_AXI);
    set_bits(HP_RST_EN0, RST_DUAL_MSPI_APB);
    clear_bits(HP_RST_EN0, RST_DUAL_MSPI_APB);
    clear_bits(HP_RST_EN0, RST_DUAL_MSPI_AXI);
    // Source MPLL, core clock on and undivided (400 MHz).
    set_field(PERI_CLK_CTRL00, 12, 2, 1);
    set_bits(PERI_CLK_CTRL00, PSRAM_CORE_CLK_EN);
    set_field(PERI_CLK_CTRL00, 16, 8, 0);

    // Pads: drive strength 2 everywhere, DQS receivers on.
    for i in 0..PINS {
        set_field(pin_reg(i), if is_dqs(i) { 15 } else { 12 }, 2, 2);
    }
    set_bits(pin_reg(PIN_DQS0), 1);
    set_bits(pin_reg(PIN_DQS1), 1);

    // CS setup 4, hold 4, hold delay 3; split transfers; 2 KiB pages.
    set_bits(M2_SMEM_AC, 0b11);
    set_field(M2_SMEM_AC, 2, 5, 4 - 1);
    set_field(M2_SMEM_AC, 7, 5, 4 - 1);
    set_field(M2_SMEM_AC, 25, 6, 3 - 1);
    set_bits(M2_SMEM_AC, 1 << 31);
    set_field(M2_ECC_CTRL, 18, 2, 3);

    set_bus_clock(MPLL_MHZ / FREQ_MHZ);
    set_bits(M2_SMEM_TIMING_CALI, DLL_TIMING_CALI);
    set_bits(M2_TIMING_CALI, DLL_TIMING_CALI);
}

/// MR0 (latency, drive), MR4 (write latency), MR8 (burst, x16), as
/// s_init_psram_mode_reg.
fn mode_registers_init() {
    let mut mr01 = reg_read(0x0, 2);
    let mut mr45 = reg_read(0x4, 2);
    mr01[0] = (mr01[0] & !0b11_1111) | (1 << 5) | (RD_LATENCY << 2);
    mr45[0] = (mr45[0] & !(0b111 << 5)) | (WR_LATENCY << 5);
    reg_write(0x0, mr01);
    reg_write(0x4, mr45);
    let mut mr8 = reg_read(0x8, 1);
    // bl = 3 (2 KiB), bt = 0, rbx = 1, x16 = 1.
    mr8[0] = (mr8[0] & !0b0100_1111) | 3 | (1 << 3) | (1 << 6);
    reg_write(0x8, [mr8[0], 0]);
}

/// The cache side: commands, 32-bit addresses, dummies, DDR, 16 data lines,
/// AXI (s_config_mspi_for_psram).
fn axi_init() {
    set_bits(M2_CACHE_SCTRL, 1 << 20);
    unsafe { wr(M2_SRAM_DWR_CMD, (15 << 28) | SYNC_WRITE as u32) };
    set_bits(M2_CACHE_SCTRL, 1 << 5);
    unsafe { wr(M2_SRAM_DRD_CMD, (15 << 28) | SYNC_READ as u32) };
    set_field(M2_CACHE_SCTRL, 14, 6, 32 - 1);
    set_bits(M2_CACHE_SCTRL, 1 << 0);
    set_bits(M2_CACHE_SCTRL, 1 << 3);
    set_field(M2_CACHE_SCTRL, 22, 6, WR_DUMMY - 1);
    set_bits(M2_CACHE_SCTRL, 1 << 4);
    set_field(M2_CACHE_SCTRL, 6, 6, RD_DUMMY - 1);
    set_bits(M2_SMEM_DDR, VAR_DUMMY);
    set_bits(M2_SRAM_CMD, 1 << 23);
    clear_bits(M2_SMEM_DDR, (1 << 3) | (1 << 2));
    set_bits(M2_SMEM_DDR, 1 << 0);
    set_bits(M2_CACHE_SCTRL, 1 << 21);
    set_bits(M2_SRAM_CMD, (1 << 21) | (1 << 20) | (1 << 19) | (1 << 18));
    set_bits(M2_SRAM_CMD, (1 << 26) | (1 << 27));
    set_bits(M2_CACHE_FCTRL, 1 << 0);
    clear_bits(M2_CACHE_FCTRL, 1 << 31);
    set_bits(M2_CTRL1, (1 << 26) | (1 << 25));
}

// ---- DQS timing tuning (mspi_timing_tuning.c, mspi_timing_by_dqs.c) -------
const TEST_ADDR: u32 = 0x80;
const TEST_DATA: [u32; 32] = [
    0x7f786655, 0xa5ff005a, 0x3f3c33aa, 0xa5ff5a00, 0x1f1e9955, 0xa5005aff, 0x0f0fccaa, 0xa55a00ff, 0x07876655, 0xffa55a00,
    0x03c333aa, 0xff00a55a, 0x01e19955, 0xff005aa5, 0x00f0ccaa, 0xff5a00a5, 0x80786655, 0x00a5ff5a, 0xc03c33aa, 0x00a55aff,
    0xe01e9355, 0x00ff5aa5, 0xf00fccaa, 0x005affa5, 0xf8876655, 0x5aa5ff00, 0xfcc333aa, 0x5affa500, 0xfee19955, 0x5a00a5ff,
    0x11f0ccaa, 0x5a00ffa5,
];
const DELAYLINE_READS: u32 = 100;
/// (data, dqs) delay lines tried: DQS 15..0, then data 1..15.
const DELAYLINES: usize = 31;

fn delayline(id: usize) -> (u32, u32) {
    if id < 16 { (0, 15 - id as u32) } else { (id as u32 - 15, 0) }
}

fn set_phase(phase: u32) {
    set_field(pin_reg(PIN_DQS0), 1, 2, phase);
    set_field(pin_reg(PIN_DQS1), 1, 2, phase);
}

fn set_delaylines(data: u32, dqs: u32) {
    for i in 0..PINS {
        if is_dqs(i) {
            set_field(pin_reg(i), 7, 4, dqs);
            set_field(pin_reg(i), 17, 4, dqs);
        } else {
            set_field(pin_reg(i), 4, 4, data);
        }
    }
}

fn test_read_ok() -> bool {
    let mut got = [0u8; 128];
    psram_read(TEST_ADDR, &mut got);
    got.chunks_exact(4).zip(TEST_DATA).all(|(g, w)| u32::from_le_bytes([g[0], g[1], g[2], g[3]]) == w)
}

/// The longest run of `true` in `ok`: (length, index of its last element).
fn longest_run(ok: impl Iterator<Item = bool>) -> (u32, u32) {
    let (mut best, mut best_end, mut run) = (0, 0, 0);
    for (i, good) in ok.enumerate() {
        run = if good { run + 1 } else { 0 };
        if run > best {
            best = run;
            best_end = i as u32;
        }
    }
    (best, best_end)
}

fn tune() -> Result<(), &'static str> {
    let started = Esp32p4v1::now();
    // Low speed (20 MHz), no tuning, to write the reference data.
    set_bus_clock(MPLL_MHZ / 20);
    set_phase(0);
    set_delaylines(0, 0);
    let mut reference = [0u8; 128];
    for (i, w) in TEST_DATA.iter().enumerate() {
        reference[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    psram_write(TEST_ADDR, &reference);
    // Full speed, fixed dummy on MSPI3.
    clear_bits(M3_DDR, VAR_DUMMY);
    set_bus_clock(MPLL_MHZ / FREQ_MHZ);

    // DQS phase: 67.5, 78.75, 90, 101.25 degrees, one read each; the
    // first phase of the longest good run.
    let mut phase_ok = 0u32;
    for p in 0..4 {
        set_phase(p);
        if test_read_ok() {
            phase_ok |= 1 << p;
        }
    }
    let (len, end) = longest_run((0..4).map(|p| phase_ok & (1 << p) != 0));
    let phase = if len == 0 { 0 } else { end + 1 - len };
    set_phase(phase);

    // Delay lines: 100 reads each; the middle of the longest good run.
    let mut dl_ok = 0u32;
    for id in 0..DELAYLINES {
        let (data, dqs) = delayline(id);
        set_delaylines(data, dqs);
        if (0..DELAYLINE_READS).all(|_| test_read_ok()) {
            dl_ok |= 1 << id;
        }
        Esp32p4v1::feed_watchdog();
    }
    let (dl_len, dl_end) = longest_run((0..DELAYLINES).map(|i| dl_ok & (1 << i) != 0));
    let best = if dl_len <= 1 { 0 } else { dl_end - dl_len / 2 };
    let (data, dqs) = delayline(best as usize);
    set_delaylines(data, dqs);
    event!(
        Info,
        "psram.tuned",
        phase = phase,
        phases_ok = phase_ok,
        delayline_id = best,
        data_delay = data,
        dqs_delay = dqs,
        window = dl_len,
        delaylines_ok = dl_ok,
        ms = (Esp32p4v1::now() - started) / 16_000
    );
    if len == 0 || dl_len <= 1 {
        return Err("tuning found no good setting");
    }
    set_bits(M2_SMEM_DDR, VAR_DUMMY);
    set_bits(M3_DDR, VAR_DUMMY);
    Ok(())
}

// ---- map and test ---------------------------------------------------------
pub const WINDOW: usize = 0x4800_0000;
/// A second linear map of the whole PSRAM, for the data of XIP apps
/// (doc/15 §2): PSRAM MMU entries 512-1023.
pub const ALIAS: usize = 0x4A00_0000;
/// The same memory without the caches (doc/02 §2).
pub const UNCACHED: usize = 0x8800_0000;
const PAGE: usize = 64 << 10;
const MMU_ACCESS_PSRAM: u32 = 1 << 10;
const MMU_VALID: u32 = 1 << 11;

fn map(size: usize) {
    let alias_entry = (ALIAS - WINDOW) / PAGE;
    for page in 0..size / PAGE {
        for entry in [page, alias_entry + page] {
            unsafe {
                wr(M2_MMU_ITEM_INDEX, entry as u32);
                wr(M2_MMU_ITEM_CONTENT, page as u32 | MMU_ACCESS_PSRAM | MMU_VALID);
            }
        }
    }
    // Nothing has touched the window yet; drop anything prefetched anyway
    // (esp_psram.c:263-302 writes the entries without suspending L2).
    super::cache_invalidate(WINDOW, size, true);
    super::cache_invalidate(ALIAS, size, true);
}

// Cache autoload: on a miss in a section, the cache also loads the next
// lines (cache_reg.h: L1 D 0x134-0x144, L2 0x2B4-0x2C4; control, then the
// address and size of sections 0 and 1). Off after reset and in ESP-IDF
// (cache_hal.c:49-54 keeps the ROM's setting). doc/43 §4.
const CACHE: usize = 0x3FF1_0000;
pub const L1D_AUTOLOAD: usize = CACHE + 0x134;
pub const L2_AUTOLOAD: usize = CACHE + 0x2B4;
/// ENA, ascending, sections 0 and 1; trigger (bits 3-4) 0: on a miss.
const AUTOLOAD_WINDOWS: u32 = 1 | (1 << 8) | (1 << 9);

/// Autoload in L1 D and L2 on the first `size` bytes of both linear maps of
/// the PSRAM.
pub fn autoload(size: usize) {
    for ctrl in [L1D_AUTOLOAD, L2_AUTOLOAD] {
        unsafe {
            wr(ctrl, 0);
            wr(ctrl + 4, WINDOW as u32);
            wr(ctrl + 8, size as u32);
            wr(ctrl + 12, ALIAS as u32);
            wr(ctrl + 16, size as u32);
            wr(ctrl, AUTOLOAD_WINDOWS);
        }
    }
}

fn pattern(i: usize) -> u32 {
    let x = (i as u32).wrapping_mul(0x9E37_79B9);
    x ^ (x >> 15) ^ 0xA5A5_0F0F
}

/// Writes a pattern through the cache to every `step`-th word, writes it
/// back, and reads it through the cache and (every 4 KiB) the uncached
/// alias. Returns the bad words.
pub fn memtest(size: usize, step: usize) -> u32 {
    let words = size / 4;
    let started = Esp32p4v1::now();
    for i in (0..words).step_by(step) {
        unsafe { core::ptr::write_volatile((WINDOW + i * 4) as *mut u32, pattern(i)) };
        if i % 0x4_0000 == 0 {
            Esp32p4v1::feed_watchdog();
        }
    }
    let write_us = (Esp32p4v1::now() - started) / 16;
    super::cache_writeback(WINDOW, size, true);
    let mut bad = 0u32;
    let mut first_bad = None;
    let started = Esp32p4v1::now();
    for i in (0..words).step_by(step) {
        let v = unsafe { core::ptr::read_volatile((WINDOW + i * 4) as *const u32) };
        if v != pattern(i) {
            bad += 1;
            first_bad.get_or_insert(i);
        }
        if i % 0x4_0000 == 0 {
            Esp32p4v1::feed_watchdog();
        }
    }
    let read_us = (Esp32p4v1::now() - started) / 16;
    // Every 4 KiB through the uncached alias: the data reached the chip.
    let mut bad_uncached = 0u32;
    for i in (0..words).step_by(step.max(1024)) {
        if unsafe { core::ptr::read_volatile((UNCACHED + i * 4) as *const u32) } != pattern(i) {
            bad_uncached += 1;
        }
    }
    let tested = (words.div_ceil(step) * 4) as u64;
    let mb_s = |us: u64| if us == 0 { 0 } else { tested / us };
    event!(
        Info,
        "psram.memtest",
        bytes = size as u32,
        step_words = step as u32,
        bad_words = bad,
        first_bad = Addr(first_bad.map_or(0, |i| (WINDOW + i * 4) as u32)),
        bad_uncached = bad_uncached,
        write_mb_s = mb_s(write_us),
        read_mb_s = mb_s(read_us)
    );
    bad + bad_uncached
}

/// Words between tested words in the boot-time test: one per 16 KiB.
const QUICK_STEP: usize = 4096;

/// Brings the PSRAM up, maps it at `WINDOW` and checks it with a short
/// test; returns its size.
pub fn init() -> Result<usize, &'static str> {
    let started = Esp32p4v1::now();
    ldo_vo2_on()?;
    mpll_on()?;
    controller_init();
    event!(Info, "psram.controller");
    mode_registers_init();
    // Connected? (s_check_psram_connected)
    psram_write(0, &0x5A6B_7C8Du32.to_le_bytes());
    let mut back = [0u8; 4];
    psram_read(0, &mut back);
    let echo = u32::from_le_bytes(back);
    let [mr0, mr1] = reg_read(0x0, 2);
    let [mr2, mr3] = reg_read(0x2, 2);
    let [mr4, _] = reg_read(0x4, 1);
    let [mr8, _] = reg_read(0x8, 1);
    let size = match mr2 & 7 {
        1 => 4 << 20,
        3 => 8 << 20,
        5 => 16 << 20,
        7 => 32 << 20,
        6 => 64 << 20,
        _ => 0,
    };
    event!(
        Info,
        "psram.id",
        vendor = mr1 & 0x1F,
        density = mr2 & 7,
        kgd = mr2 >> 5,
        size = size as u32,
        echo = echo,
        mr0 = mr0,
        mr3 = mr3,
        mr4 = mr4,
        mr8 = mr8
    );
    if echo != 0x5A6B_7C8D {
        return Err("PSRAM did not echo the test word");
    }
    if size == 0 || size > 32 << 20 {
        return Err("unexpected PSRAM size");
    }
    axi_init();
    tune()?;
    map(size);
    autoload(size);
    let bad = memtest(size, QUICK_STEP);
    event!(Info, "psram.ready", window = Addr(WINDOW as u32), size = size as u32, ms = (Esp32p4v1::now() - started) / 16_000);
    if bad != 0 {
        return Err("memory test failed");
    }
    Ok(size)
}
