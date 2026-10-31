//! The ESP32-C6 Wi-Fi chip on SDMMC slot 1 (N2b, doc/22): power, reset and
//! the SDIO card's identification. Pins through the GPIO matrix: CLK 12,
//! CMD 13, D0-D3 11/10/9/8; CHIP_EN = GPIO15 (doc/research/net-stack.md
//! §1.1). The host is shared with the SD card (`sd.rs`); everything here
//! uses slot 1's divider and card number only.
//!
//! The C6's firmware (esp_hosted, as M5Stack or TCC left it) is not
//! changed (user, 2026-10-31).

use t5_abi::event;

use super::i2c::{func_in_sel, func_out_sel, io_mux_reg};
use super::psram::delay_us;
use super::sd::{self, Data};
use super::{Esp32p4v1, Platform, tab5, wr};

const SLOT: u32 = 1;
const CLK_PIN: usize = 12;
const CMD_PIN: usize = 13;
const DATA_PINS: [usize; 4] = [11, 10, 9, 8];
const CHIP_EN: usize = 15;

// GPIO matrix signals of SDMMC slot 1 (gpio_sig_map.h: SD_CARD_*_2_PAD).
const SIG_CCLK: u32 = 0;
const SIG_CCMD: u32 = 1;
const SIG_CDATA0: u32 = 2;
const SIG_CD: u32 = 127;
const SIG_INT: u32 = 129;
const SIG_WP: u32 = 131;
const GPIO_SIG_IN_SEL: u32 = 1 << 7;
const IN_CONST_LOW: u32 = 0xBE;
const IN_CONST_HIGH: u32 = 0xBF;
const SIG_GPIO_OUT: u32 = 256;
/// MCU_SEL 1 (GPIO matrix), drive 3, input enabled, pull-up.
const IO_MUX_MATRIX: u32 = (1 << 12) | (3 << 10) | (1 << 9) | (1 << 8);
const GPIO_OUT_W1TS: usize = 0x500E_0008;
const GPIO_OUT_W1TC: usize = 0x500E_000C;
const GPIO_ENABLE_W1TS: usize = 0x500E_0024;

// Command register values for slot 0 (the slot is added by `command_on`):
// start, use_hold_reg, wait_prvdata_complete, response expected, CRC check.
const CMD0: u32 = 0xA000_8000;
/// IO_SEND_OP_COND: R4, no CRC.
const CMD5: u32 = 0xA000_2045;
const CMD3: u32 = 0xA000_2143;
const CMD7: u32 = 0xA000_2147;
/// IO_RW_DIRECT: R5.
const CMD52: u32 = 0xA000_2174;
/// IO_RW_EXTENDED with a data phase, read and write (bit 10).
const CMD53_READ: u32 = 0xA000_2375;
const CMD53_WRITE: u32 = 0xA000_2775;

/// R4: card ready, number of I/O functions, memory present, OCR.
const R4_READY: u32 = 1 << 31;
/// 3.2-3.4 V (bits 20, 21): the C6 runs at 3.3 V.
const OCR_33V: u32 = 0x0030_0000;
/// R5 flags [15:8]: COM_CRC_ERROR, ILLEGAL_COMMAND, ERROR, FUNCTION_NUMBER,
/// OUT_OF_RANGE.
const R5_ERRORS: u32 = 0xCB00;

// CCCR registers (SDIO spec 6.9).
const CCCR_IO_ENABLE: u32 = 0x02;
const CCCR_IO_READY: u32 = 0x03;
const CCCR_INT_ENABLE: u32 = 0x04;
const CCCR_IO_ABORT: u32 = 0x06;
const CCCR_BUS_IF: u32 = 0x07;
const CCCR_FN0_BLOCK_SIZE: u32 = 0x10;
const FBR1_BLOCK_SIZE: u32 = 0x110;
const CCCR_LEN: usize = 0x14;
const FUNC1: u8 = 1 << 1;
const BLOCK: usize = 512;

// The C6's SLC host registers, seen by function 1 at their address & 0x3FF
// (esp_hosted host/drivers/transport/sdio/sdio_reg.h:37-99).
const SLC_TOKEN_RDATA: u32 = 0x044;
const SLC_INT_RAW: u32 = 0x050;
const SLC_PACKET_LEN: u32 = 0x060;
const SLC_INT_CLR: u32 = 0x0D4;
/// SCRATCH_REG_7: the host's interrupts to the slave.
const SLC_HOST_TO_SLAVE: u32 = 0x08C;
const INT_NEW_PACKET: u32 = 1 << 23;
/// START_THROTTLE and STOP_THROTTLE: the C6 asks the host to pause or
/// resume sending.
const INT_THROTTLE: u32 = (1 << 7) | (1 << 6);
/// The interrupts the host acts on. Others are set too (bit 19 comes back
/// at once after clearing, seen on the Tab5); esp_hosted ignores them.
const INT_USED: u32 = INT_NEW_PACKET | INT_THROTTLE;
const LEN_MASK: u32 = 0xF_FFFF;
/// Data is read from and written to the addresses just below this.
const CMD53_END_ADDR: u32 = 0x1F800;
/// ESP_HOST_INTERRUPT (esp_hosted_transport_init.h): open the data path.
const ESP_OPEN_DATA_PATH: u8 = 0;

// esp_hosted's payload header and the INIT event (esp_hosted_header.h,
// esp_hosted_transport.h, esp_hosted_transport_init.h).
const HEADER_LEN: usize = 12;
const IF_PRIV: u8 = 5;
const PRIV_EVENT: u8 = 0x33;
const PRIV_EVENT_INIT: u8 = 0x22;
const TAG_CHIP_ID: u8 = 0x12;
const TAG_FW_VERSION: u8 = 0x17;
const TAG_SDIO_MODE: u8 = 0x18;

fn ms(n: u64) {
    // In 100 ms steps, feeding the watchdog (power-up waits are 1.5 s).
    let mut left = n;
    while left > 0 {
        let step = left.min(100);
        delay_us(step * 1000);
        Esp32p4v1::feed_watchdog();
        left -= step;
    }
}

fn cmd(c: u32, arg: u32) -> Result<u32, &'static str> {
    sd::command_on(SLOT, 512, c, arg, Data::None).map(|r| r[0])
}

/// CMD52: one byte of function `func`'s register `addr`; `write` stores
/// it and returns what the card reads back.
fn rw_direct(func: u32, addr: u32, write: Option<u8>) -> Result<u8, &'static str> {
    let arg = match write {
        Some(v) => 1 << 31 | func << 28 | 1 << 27 | addr << 9 | v as u32,
        None => func << 28 | addr << 9,
    };
    let r = cmd(CMD52, arg)?;
    if r & R5_ERRORS != 0 {
        event!(Warn, "wlan.cmd52_error", func = func, addr = addr, r5 = r);
        return Err("CMD52 error");
    }
    Ok(r as u8)
}

/// CMD53 on function 1, incrementing address: whole blocks in block mode,
/// otherwise up to 512 bytes in byte mode.
fn rw_extended(addr: u32, data: Data) -> Result<(), &'static str> {
    let len = match &data {
        Data::Read(b) => b.len(),
        Data::Write(b) => b.len(),
        Data::None => 0,
    };
    let (block_mode, count) = if len >= BLOCK && len % BLOCK == 0 {
        (1, len / BLOCK)
    } else if len > 0 && len <= BLOCK {
        (0, len % BLOCK) // 0 stands for 512
    } else {
        return Err("bad CMD53 length");
    };
    let write = matches!(data, Data::Write(_));
    let arg = (write as u32) << 31 | 1 << 28 | block_mode << 27 | 1 << 26 | addr << 9 | count as u32;
    let r = sd::command_on(SLOT, BLOCK, if write { CMD53_WRITE } else { CMD53_READ }, arg, data)?[0];
    if r & R5_ERRORS != 0 {
        event!(Warn, "wlan.cmd53_error", addr = addr, len = len as u32, r5 = r);
        return Err("CMD53 error");
    }
    Ok(())
}

fn read_reg(reg: u32) -> Result<u32, &'static str> {
    let mut b = [0u8; 4];
    rw_extended(reg, Data::Read(&mut b))?;
    Ok(u32::from_le_bytes(b))
}

fn write_reg(reg: u32, v: u32) -> Result<(), &'static str> {
    rw_extended(reg, Data::Write(&v.to_le_bytes()))
}

fn set_block_size(fbr: u32) -> Result<(), &'static str> {
    rw_direct(0, fbr, Some(BLOCK as u8))?;
    rw_direct(0, fbr + 1, Some((BLOCK >> 8) as u8))?;
    Ok(())
}

/// Logs one transfer from the C6: its payload header and, for the INIT
/// event, each of its tags (esp_hosted transport_drv.c:802-880).
fn log_packet(p: &[u8]) {
    if p.len() < HEADER_LEN {
        return;
    }
    let len = u16::from_le_bytes([p[2], p[3]]) as usize;
    let offset = u16::from_le_bytes([p[4], p[5]]) as usize;
    event!(
        Info,
        "wlan.rx",
        if_type = p[0] & 0xF,
        if_num = p[0] >> 4,
        flags = p[1],
        len = len as u32,
        offset = offset as u32,
        seq = u16::from_le_bytes([p[8], p[9]]),
        priv_type = p[11],
        head = &p[..p.len().min(48)]
    );
    let Some(body) = p.get(offset..offset + len) else { return };
    if p[0] & 0xF != IF_PRIV || p[11] != PRIV_EVENT || body.len() < 2 || body[0] != PRIV_EVENT_INIT {
        return;
    }
    let mut tags = &body[2..body.len().min(2 + body[1] as usize)];
    while tags.len() >= 2 {
        let (tag, n) = (tags[0], tags[1] as usize);
        let Some(value) = tags.get(2..2 + n) else { break };
        let word = |v: &[u8]| v.iter().rev().fold(0u32, |a, &b| a << 8 | b as u32);
        match tag {
            // (major << 16) | (minor << 8) | patch (esp_hosted_host_fw_ver.h:23).
            TAG_FW_VERSION => {
                let v = word(value);
                event!(Info, "wlan.fw_version", major = (v >> 16) & 0xFF, minor = (v >> 8) & 0xFF, patch = v & 0xFF);
            }
            TAG_CHIP_ID => event!(Info, "wlan.chip_id", id = word(value)),
            TAG_SDIO_MODE => event!(Info, "wlan.sdio_mode", streaming = word(value) != 0),
            _ => event!(Info, "wlan.init_tag", tag = tag, value = value),
        }
        tags = &tags[2 + n..];
    }
}

fn pins() {
    unsafe {
        wr(io_mux_reg(CLK_PIN), IO_MUX_MATRIX);
        wr(func_out_sel(CLK_PIN), SIG_CCLK);
        let io = core::iter::once((CMD_PIN, SIG_CCMD)).chain(DATA_PINS.iter().enumerate().map(|(i, &p)| (p, SIG_CDATA0 + i as u32)));
        for (pin, sig) in io {
            wr(io_mux_reg(pin), IO_MUX_MATRIX);
            // The host drives the output enable (OE_SEL 0).
            wr(func_out_sel(pin), sig);
            wr(func_in_sel(sig), pin as u32 | GPIO_SIG_IN_SEL);
        }
        wr(func_in_sel(SIG_CD), IN_CONST_LOW);
        wr(func_in_sel(SIG_WP), IN_CONST_LOW);
        wr(func_in_sel(SIG_INT), IN_CONST_HIGH);
    }
}

fn chip_en(high: bool) {
    unsafe {
        wr(io_mux_reg(CHIP_EN), (1 << 12) | (1 << 9));
        wr(func_out_sel(CHIP_EN), SIG_GPIO_OUT);
        wr(if high { GPIO_OUT_W1TS } else { GPIO_OUT_W1TC }, 1 << CHIP_EN);
        wr(GPIO_ENABLE_W1TS, 1 << CHIP_EN);
    }
}

/// `wlanprobe`: powers the C6 (if needed), resets it, identifies the SDIO
/// card at 400 kHz, logs the CCCR and enables function 1 (N2b-1).
pub fn probe() -> Result<u32, &'static str> {
    let was_on = tab5::wlan_power_on()?;
    // A reset pulse on CHIP_EN, as the esp_hosted host does; then the time
    // TCC gives the C6 to boot (wifi_setup.cpp:117-121).
    chip_en(false);
    ms(if was_on { 50 } else { 300 });
    chip_en(true);
    ms(1500);

    sd::host_up()?;
    pins();
    sd::set_card_clock(SLOT, 50, 400)?;
    delay_us(1000);
    // Ignored by a card that has just started; resets one that was in use.
    let _ = rw_direct(0, CCCR_IO_ABORT, Some(0x08));
    let _ = cmd(CMD0, 0);
    delay_us(20_000);
    let r4 = cmd(CMD5, 0)?;
    let functions = (r4 >> 28) & 7;
    event!(Info, "wlan.ocr", r4 = r4, functions = functions, memory = r4 & (1 << 27) != 0);
    let mut tries = 0;
    let mut r = r4;
    while r & R4_READY == 0 {
        tries += 1;
        if tries > 100 {
            return Err("SDIO card did not become ready");
        }
        r = cmd(CMD5, r4 & OCR_33V)?;
        delay_us(10_000);
    }
    let rca = cmd(CMD3, 0)? >> 16;
    cmd(CMD7, rca << 16)?;
    let mut cccr = [0u8; CCCR_LEN];
    for (i, b) in cccr.iter_mut().enumerate() {
        *b = rw_direct(0, i as u32, None)?;
    }
    event!(Info, "wlan.card", rca = rca, ready_tries = tries, was_on = was_on, cccr = &cccr[..]);

    let en = rw_direct(0, CCCR_IO_ENABLE, None)?;
    rw_direct(0, CCCR_IO_ENABLE, Some(en | FUNC1))?;
    let mut ready = 0;
    for _ in 0..100 {
        ready = rw_direct(0, CCCR_IO_READY, None)?;
        if ready & FUNC1 != 0 {
            break;
        }
        delay_us(10_000);
    }
    event!(Info, "wlan.func1", io_ready = ready, ready = ready & FUNC1 != 0);
    if ready & FUNC1 == 0 {
        return Err("function 1 not ready");
    }

    // N2b-2: as esp_hosted's card init (port_esp_hosted_host_sdio.c:150-218):
    // interrupts of function 1 and the master enable, 512-byte blocks, then
    // 4 lines at 20 MHz (TCC's clock: 40 MHz failed there).
    let ie = rw_direct(0, CCCR_INT_ENABLE, None)?;
    rw_direct(0, CCCR_INT_ENABLE, Some(ie | 1 | FUNC1))?;
    set_block_size(CCCR_FN0_BLOCK_SIZE)?;
    set_block_size(FBR1_BLOCK_SIZE)?;
    let bus = rw_direct(0, CCCR_BUS_IF, None)?;
    rw_direct(0, CCCR_BUS_IF, Some((bus & !3) | 2))?;
    sd::set_wide(SLOT, true);
    sd::set_card_clock(SLOT, 1, 20_000)?;
    let token = read_reg(SLC_TOKEN_RDATA)?;
    event!(
        Info,
        "wlan.slc",
        int_raw = read_reg(SLC_INT_RAW)?,
        packet_len = read_reg(SLC_PACKET_LEN)?,
        tx_buffers = (token >> 16) & 0xFFF,
        bus_if = rw_direct(0, CCCR_BUS_IF, None)?
    );

    // Open the data path, then read what the C6 sends for up to 2 s
    // (sdio_drv.c:1096-1290, polling instead of the card interrupt).
    rw_direct(1, SLC_HOST_TO_SLAVE, Some(1 << ESP_OPEN_DATA_PATH))?;
    let mut rx_count = 0u32;
    let mut buf = [0u8; 3 * BLOCK];
    let end = Esp32p4v1::now() + 2 * 16_000_000;
    let mut packets = 0u32;
    let mut seen = 0u32;
    while Esp32p4v1::now() < end && packets < 4 {
        let ints = read_reg(SLC_INT_RAW)?;
        if ints & !seen != 0 {
            seen |= ints;
            event!(Info, "wlan.int", ints = ints);
        }
        if ints & INT_USED == 0 {
            delay_us(1000);
            continue;
        }
        write_reg(SLC_INT_CLR, ints & INT_USED)?;
        if ints & INT_NEW_PACKET == 0 {
            continue;
        }
        let total = read_reg(SLC_PACKET_LEN)? & LEN_MASK;
        let len = (total + 0x10_0000 - rx_count) % 0x10_0000;
        if len == 0 || len as usize > buf.len() {
            event!(Warn, "wlan.bad_len", packet_len = total, rx_count = rx_count);
            break;
        }
        // Whole blocks; the C6 pads with zeros.
        let n = (len as usize).next_multiple_of(BLOCK);
        rw_extended(CMD53_END_ADDR - len, Data::Read(&mut buf[..n]))?;
        rx_count = total;
        packets += 1;
        log_packet(&buf[..len as usize]);
    }
    event!(Info, "wlan.probe_done", packets = packets, rx_bytes = rx_count);
    Ok(rca)
}
