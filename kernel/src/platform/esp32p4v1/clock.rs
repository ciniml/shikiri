//! PVT (automatic adjustment of the HP core voltage) and the 360 MHz CPU
//! clock, after ESP-IDF 6.0 (doc/research/m4-psram-and-clock.md §10):
//! `esp_hw_support/port/esp32p4/pmu_pvt.c` (`pvt_auto_dbias_init`,
//! `pvt_func_enable(true)`, called from `pmu_init` before the app raises the
//! clock) and `rtc_clk.c` (`rtc_clk_cpu_freq_to_cpll_mhz`). The bootloader
//! leaves CPLL at 360 MHz and the CPU at 360/4 = 90 MHz; going to 360 MHz is
//! a change of dividers only. The clock goes up only once PVT runs, as in
//! ESP-IDF, where PVT is "a must for stable mass production"
//! (esp_hw_support/Kconfig:288-295).
//!
//! Register addresses and fields are from
//! `components/soc/esp32p4/register/hw_ver1/soc/{pvt,pmu,hp_sys_clkrst,
//! lp_clkrst,efuse}_reg.h`; the constants from
//! `esp_hw_support/port/esp32p4/include/soc/rtc.h:100-112`.

use t5_abi::event;

use super::psram::{clear_bits, delay_us, set_bits, set_field};
use super::{HP_SYS_CLKRST, rd};

// eFuse (efuse_reg.h:749-791,953-967; efuse_hal_blk_version = major * 100
// + minor).
const EFUSE_RD_MAC_SYS_2: usize = 0x5012_D04C;
const EFUSE_RD_MAC_SYS_5: usize = 0x5012_D058;

fn blk_version() -> u32 {
    let v = unsafe { rd(EFUSE_RD_MAC_SYS_2) };
    ((v >> 11) & 3) * 100 + ((v >> 8) & 7)
}

/// PVT is calibrated in eFuse block versions 2..99 (pmu_pvt.c:80).
fn pvt_supported(blk: u32) -> bool {
    (2..100).contains(&blk)
}

// Clock gates (hp_sys_clkrst_reg.h:376,558,982,989,2581-2649).
const SOC_CLK_CTRL1: usize = HP_SYS_CLKRST + 0x18;
const PVT_SYS_CLK_EN: u32 = 1 << 25;
const REF_CLK_CTRL2: usize = HP_SYS_CLKRST + 0x2C;
const REF_160M_CLK_EN: u32 = 1 << 0;
const PERI_CLK_CTRL24: usize = HP_SYS_CLKRST + 0xA4;
const PVT_CLK_DIV_NUM_S: u32 = 16;
const PVT_CLK_EN: u32 = 1 << 24;
const PERI_CLK_CTRL25: usize = HP_SYS_CLKRST + 0xA8;
const PVT_PERI_GROUP_CLK_DIV_NUM_S: u32 = 0;
const PVT_PERI_GROUP_CLK_EN: u32 = 0xF << 8; // groups 1-4

// PVT (DR_REG_PVT_BASE = 0x5009_E000; pvt_reg.h).
const PVT: usize = 0x5009_E000;
const PVT_CLK_CFG: usize = PVT + 0x30;
const MONITOR_CLK_PVT_EN: u32 = 1 << 8;
const DBIAS_CHANNEL_SEL0: usize = PVT + 0x34;
const DBIAS_CHANNEL0_SEL_S: u32 = 25;
const DBIAS_CHANNEL1_SEL_S: u32 = 18;
const DBIAS_CHANNEL_CFG: [usize; 3] = [PVT + 0x3C, PVT + 0x40, PVT + 0x44];
const DBIAS_CMD: [usize; 3] = [PVT + 0x50, PVT + 0x54, PVT + 0x58];
const CMD_OFFSET_VALUE_S: u32 = 11;
const CMD_OFFSET_FLAG_S: u32 = 16;
const DBIAS_TIMER: usize = PVT + 0x64;
const TIMER_TARGET_S: u32 = 15;
const TIMER_EN: u32 = 1 << 31;
// The monitor cell: site 3, units 0 (too high) and 1 (too low), VT1.
const SITE3_UNIT0_VT1_CONF1: usize = PVT + 0x108;
const SITE3_UNIT1_VT1_CONF1: usize = PVT + 0x10C;
const MONITOR_EN: u32 = 1 << 0;
const DELAY_LIMIT_S: u32 = 2;
const SITE3_UNIT0_VT1_CONF2: usize = PVT + 0x1C8;

// rtc.h:100-112
const CHANNEL0_SEL: u32 = 49;
const CHANNEL1_SEL: u32 = 53;
const CHANNEL_CFG: [u32; 3] = [0x13E80, 0x13E80, 0x10000];
const CMD: [u32; 3] = [0x24, 0x5, 0x427];
const TARGET: u32 = 0x7D00;
const CLK_DIV: u32 = 1;
const EDG_MODE: u32 = 1;
const DELAY_NUM_HIGH: u32 = 164;
const DELAY_NUM_LOW: u32 = 157;

// PMU (DR_REG_PMU_BASE = 0x5011_5000; pmu_reg.h:180-187,361-431).
const PMU_HP_ACTIVE_BIAS: usize = 0x5011_5018;
const PMU_HP_ACTIVE_HP_REGULATOR0: usize = 0x5011_5028;
const DIG_REGULATOR0_DBIAS_SEL: u32 = 1 << 14;
const DIG_DBIAS_INIT: u32 = 1 << 15;

/// The voltage settings PVT works on, for the log: the DCDC setting
/// (`DCM_VSET`), the HP regulator setting, and the dbias PVT currently
/// asks for (`HP_DBIAS_VOL`, `LP_DBIAS_VOL`, read only).
fn voltages() -> (u32, u32, u32, u32, bool) {
    let (bias, reg0) = unsafe { (rd(PMU_HP_ACTIVE_BIAS), rd(PMU_HP_ACTIVE_HP_REGULATOR0)) };
    (
        (bias >> 18) & 0x1F,
        reg0 >> 27,
        (reg0 >> 9) & 0x1F,
        (reg0 >> 4) & 0x1F,
        reg0 & DIG_REGULATOR0_DBIAS_SEL == 0,
    )
}

pub fn log_voltages(stage: &'static str) {
    let (dcm_vset, regulator_dbias, hp_dbias, lp_dbias, pvt_control) = voltages();
    event!(Info, "clock.voltage", stage = stage, dcm_vset = dcm_vset, regulator_dbias = regulator_dbias, hp_dbias = hp_dbias, lp_dbias = lp_dbias, pvt_control = pvt_control);
}

/// The HP/LP dbias gap from eFuse, in PVT's sign-magnitude form
/// (pmu_pvt.c:31-52).
fn lp_hp_gap(efuse: u32) -> u32 {
    let abs = (efuse & 0xF) as i32;
    let gap = if efuse >> 4 & 1 != 0 { -abs } else { abs } - 8;
    if gap < 0 { (16 - gap) as u32 } else { gap as u32 }
}

/// `pvt_auto_dbias_init` + `pvt_func_enable(true)` + the 1 ms `pmu_init`
/// waits for it to take effect. Returns false if the chip has no PVT
/// calibration (the voltage then stays where the bootloader set it).
pub fn pvt_on() -> bool {
    let blk = blk_version();
    let gap_efuse = unsafe { (rd(EFUSE_RD_MAC_SYS_5) >> 5) & 0x1F };
    event!(Info, "clock.pvt", blk_version = blk, gap_efuse = gap_efuse, supported = pvt_supported(blk));
    log_voltages("bootloader");
    if !pvt_supported(blk) {
        return false;
    }
    // pvt_auto_dbias_init
    set_bits(REF_CLK_CTRL2, REF_160M_CLK_EN);
    set_bits(SOC_CLK_CTRL1, PVT_SYS_CLK_EN);
    clear_bits(DBIAS_TIMER, TIMER_EN);
    delay_us(100);
    set_field(DBIAS_CHANNEL_SEL0, DBIAS_CHANNEL0_SEL_S, 7, CHANNEL0_SEL);
    set_field(DBIAS_CHANNEL_SEL0, DBIAS_CHANNEL1_SEL_S, 7, CHANNEL1_SEL);
    for (reg, v) in DBIAS_CHANNEL_CFG.iter().zip(CHANNEL_CFG) {
        set_field(*reg, 0, 17, v);
    }
    for (reg, v) in DBIAS_CMD.iter().zip(CMD) {
        set_field(*reg, 0, 11, v);
    }
    set_bits(PMU_HP_ACTIVE_HP_REGULATOR0, DIG_DBIAS_INIT);
    set_field(DBIAS_TIMER, TIMER_TARGET_S, 16, TARGET);
    set_field(PERI_CLK_CTRL24, PVT_CLK_DIV_NUM_S, 8, CLK_DIV);
    set_field(PERI_CLK_CTRL25, PVT_PERI_GROUP_CLK_DIV_NUM_S, 8, CLK_DIV);
    set_bits(PERI_CLK_CTRL24, PVT_CLK_EN);
    set_bits(PERI_CLK_CTRL25, PVT_PERI_GROUP_CLK_EN);
    set_bits(PVT_CLK_CFG, MONITOR_CLK_PVT_EN); // cannot be turned off again
    delay_us(100);
    set_field(SITE3_UNIT0_VT1_CONF2, 0, 2, EDG_MODE);
    set_field(SITE3_UNIT0_VT1_CONF1, DELAY_LIMIT_S, 8, DELAY_NUM_HIGH);
    set_field(SITE3_UNIT1_VT1_CONF1, DELAY_LIMIT_S, 8, DELAY_NUM_LOW);
    let gap = lp_hp_gap(gap_efuse);
    for reg in DBIAS_CMD {
        set_field(reg, CMD_OFFSET_FLAG_S, 1, gap >> 4);
        set_field(reg, CMD_OFFSET_VALUE_S, 5, gap & 0xF);
    }
    // pvt_func_enable(true): the gates again, the monitor on, then dbias
    // handed from the PMU to PVT.
    set_bits(REF_CLK_CTRL2, REF_160M_CLK_EN);
    set_bits(SOC_CLK_CTRL1, PVT_SYS_CLK_EN);
    set_bits(PERI_CLK_CTRL24, PVT_CLK_EN);
    set_bits(PERI_CLK_CTRL25, PVT_PERI_GROUP_CLK_EN);
    set_bits(PMU_HP_ACTIVE_HP_REGULATOR0, DIG_DBIAS_INIT);
    set_bits(PVT_CLK_CFG, MONITOR_CLK_PVT_EN);
    set_bits(SITE3_UNIT0_VT1_CONF1, MONITOR_EN);
    delay_us(1000);
    clear_bits(PMU_HP_ACTIVE_HP_REGULATOR0, DIG_REGULATOR0_DBIAS_SEL);
    clear_bits(PMU_HP_ACTIVE_HP_REGULATOR0, DIG_DBIAS_INIT);
    set_bits(DBIAS_TIMER, TIMER_EN);
    delay_us(1000);
    event!(Info, "clock.pvt_on", gap = gap);
    log_voltages("pvt");
    true
}

// Root clock dividers (hp_sys_clkrst_reg.h:29-123; lp_clkrst_reg.h:662-669).
// Each field holds divider - 1.
const ROOT_CLK_CTRL0: usize = HP_SYS_CLKRST + 0x04;
const SOC_CLK_DIV_UPDATE: u32 = 1 << 4;
const CPU_DIV_S: u32 = 5;
const ROOT_CLK_CTRL1: usize = HP_SYS_CLKRST + 0x08;
const MEM_DIV_S: u32 = 0;
const SYS_DIV_S: u32 = 24;
const ROOT_CLK_CTRL2: usize = HP_SYS_CLKRST + 0x0C;
const APB_DIV_S: u32 = 16;
const LP_CLKRST_HP_CLK_CTRL: usize = 0x5011_1040;
const HP_ROOT_CLK_SRC_CPLL: u32 = 1;
/// ets_update_cpu_frequency (esp32p4.rom.ld:32): the ROM's ticks per µs.
const ROM_UPDATE_CPU_FREQUENCY: usize = 0x4FC0_0044;
/// CPLL as the bootloader leaves it: 360 MHz on v1.x, 400 MHz on v3
/// (rtc_clk.c, the CONFIG_ESP32P4_SELECTS_REV_LESS_V3 branches).
#[cfg(not(feature = "chip-v3"))]
pub const CPLL_MHZ: u32 = 360;
#[cfg(feature = "chip-v3")]
pub const CPLL_MHZ: u32 = 400;

/// (CPU, MEM, SYS, APB) dividers - 1, as in the registers. CPU 360, MEM
/// 180, SYS 180, APB 90 (rtc_clk.c:231-245); on v3 400, 200, 200, 100.
const DIV_FULL: [u32; 4] = [0, 1, 0, 1];
const DIV_QUARTER: [u32; 4] = [3, 0, 0, 0];

fn dividers() -> [u32; 4] {
    unsafe {
        let (c0, c1, c2) = (rd(ROOT_CLK_CTRL0), rd(ROOT_CLK_CTRL1), rd(ROOT_CLK_CTRL2));
        [(c0 >> CPU_DIV_S) & 0xFF, (c1 >> MEM_DIV_S) & 0xFF, (c1 >> SYS_DIV_S) & 0xFF, (c2 >> APB_DIV_S) & 0xFF]
    }
}

/// Changes the dividers one update at a time: from APB to CPU when going
/// up, from CPU to APB when going down, so no intermediate state has MEM
/// above 200 MHz or APB above 100 MHz (rtc_clk.c:283-308). Runs
/// from RAM: the cache clock changes under it. Interrupts are off in the
/// kernel.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
unsafe fn rescale(d: &[u32; 4], up: bool) {
    #[inline(always)]
    unsafe fn field(addr: usize, shift: u32, v: u32) {
        unsafe {
            let r = core::ptr::read_volatile(addr as *const u32);
            core::ptr::write_volatile(addr as *mut u32, (r & !(0xFF << shift)) | (v << shift));
            let c0 = core::ptr::read_volatile(ROOT_CLK_CTRL0 as *const u32);
            core::ptr::write_volatile(ROOT_CLK_CTRL0 as *mut u32, c0 | SOC_CLK_DIV_UPDATE);
            while core::ptr::read_volatile(ROOT_CLK_CTRL0 as *const u32) & SOC_CLK_DIV_UPDATE != 0 {}
        }
    }
    unsafe {
        if up {
            field(ROOT_CLK_CTRL2, APB_DIV_S, d[3]);
            field(ROOT_CLK_CTRL1, SYS_DIV_S, d[2]);
            field(ROOT_CLK_CTRL1, MEM_DIV_S, d[1]);
            field(ROOT_CLK_CTRL0, CPU_DIV_S, d[0]);
        } else {
            field(ROOT_CLK_CTRL0, CPU_DIV_S, d[0]);
            field(ROOT_CLK_CTRL1, MEM_DIV_S, d[1]);
            field(ROOT_CLK_CTRL1, SYS_DIV_S, d[2]);
            field(ROOT_CLK_CTRL2, APB_DIV_S, d[3]);
        }
    }
}

/// Raises the CPU to CPLL (360 MHz; 400 on v3) if it runs at the
/// bootloader's CPLL / 4. A warm reset may come back already at full speed
/// (the bootloader keeps the clocks when APB is at 90 MHz,
/// bootloader_clock_init.c:51), so the current dividers decide. Returns the
/// CPU clock in MHz the registers now say.
pub fn cpu_full() -> Option<u32> {
    set_cpu_mhz(CPLL_MHZ)
}

/// Sets the CPU to 360 or 90 MHz (400 or 100 on v3: CPLL / 1 or / 4) if the dividers are in
/// one of those two known states (the `clock` control command, doc/16
/// §6.9, as well as boot). Returns the CPU clock the registers now say.
pub fn set_cpu_mhz(mhz: u32) -> Option<u32> {
    let src = unsafe { rd(LP_CLKRST_HP_CLK_CTRL) } & 3;
    let before = dividers();
    let ok = src == HP_ROOT_CLK_SRC_CPLL && (before == DIV_QUARTER || before == DIV_FULL);
    event!(Info, "clock.dividers", root_src = src, cpu = before[0], mem = before[1], sys = before[2], apb = before[3], known = ok);
    let target = match mhz {
        m if m == CPLL_MHZ => DIV_FULL,
        m if m == CPLL_MHZ / 4 => DIV_QUARTER,
        _ => return None,
    };
    if !ok {
        return None;
    }
    if before != target {
        unsafe {
            rescale(&target, mhz == CPLL_MHZ);
            let update: unsafe extern "C" fn(u32) = core::mem::transmute(ROM_UPDATE_CPU_FREQUENCY);
            update(mhz);
        }
    }
    let after = dividers();
    Some(CPLL_MHZ / (after[0] + 1))
}

/// Hands the HP dbias back from PVT to the PMU (the `pvt off` control
/// command, doc/16 §6.9). Unlike ESP-IDF's `pvt_func_enable(false)`, which
/// keeps the level PVT reached by copying it into `DCM_VSET`
/// (pmu_pvt.c:141-146), this leaves `DCM_VSET` at the bootloader's value:
/// the state before PVT was turned on.
pub fn pvt_off() {
    clear_bits(DBIAS_TIMER, TIMER_EN);
    set_bits(PMU_HP_ACTIVE_HP_REGULATOR0, DIG_REGULATOR0_DBIAS_SEL);
    clear_bits(PERI_CLK_CTRL24, PVT_CLK_EN);
    delay_us(1000);
    event!(Info, "clock.pvt_off");
    log_voltages("pvt_off");
}
