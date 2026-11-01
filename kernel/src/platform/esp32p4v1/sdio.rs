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
use super::{Esp32p4v1, Platform, rd, tab5, wr};

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
const GPIO_IN: usize = 0x500E_003C;
/// DAT1, which the card pulls low to interrupt (4-bit mode).
const D1_PIN: usize = DATA_PINS[1];
/// Without an interrupt, the C6 is still asked this often (a missed
/// edge costs at most this long).
const FALLBACK_MS: u64 = 20;

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
/// Bus Speed Select: SHS (bit 0, the card supports High Speed), EHS (bit
/// 1, use it).
const CCCR_BUS_SPEED: u32 = 0x13;
const SHS: u8 = 1 << 0;
const EHS: u8 = 1 << 1;
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
/// ESP_PRIV_FIRMWARE_CHIP_ESP32C6.
const CHIP_ESP32C6: u8 = 0x0D;

fn ms(n: u64) {
    // Power-up waits are 1.5 s: the watchdog is fed every 100 ms, and the
    // kernel lock is let go after each millisecond (`smp::pause`), so the
    // other core's processes go on meanwhile. Held for the whole wait, it
    // kept `fs` spinning on the lock for 1.5 s at boot (doc/34 §12).
    for k in 0..n {
        delay_us(1000);
        if k % 100 == 99 {
            Esp32p4v1::feed_watchdog();
        }
        crate::smp::pause();
    }
    Esp32p4v1::feed_watchdog();
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
        seq_num = u16::from_le_bytes([p[8], p[9]]),
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

/// The transport's state once `up` has run.
struct Link {
    up: bool,
    /// Bytes read so far (mod 2^20), against the C6's PACKET_LEN.
    rx_count: u32,
    /// Buffers used so far (mod 2^12), against TOKEN_RDATA, and how many
    /// are known to be free (sdio_drv.c:420-440, 531-580).
    tx_count: u32,
    tx_free: u32,
    tx_seq: u16,
    /// The C6 asked to pause sending (START/STOP_THROTTLE).
    throttled: bool,
    /// The INIT event came: answer with the host's configuration.
    send_config: bool,
    /// Received stream (packets back to back, streaming mode); `have` bytes.
    stream: [u8; STREAM],
    have: usize,
    tx: [u8; TX_MAX],
    dropped: u32,
    /// When to ask the C6 even without its interrupt (ticks).
    next_poll: u64,
    /// DAT1 was still low when the interrupt was armed: no edge will come,
    /// so the next poll asks at once.
    again: bool,
    /// Card interrupts taken, and arms that found DAT1 low.
    irqs: u32,
    d1_low: u32,
}

/// Room for a 4 KiB read (the DMA bounce buffer's size) on top of a
/// partial packet, rounded to whole blocks.
const STREAM: usize = 8 * BLOCK;
const READ_MAX: usize = 8 * BLOCK;
/// Header and the longest payload (MAX_SDIO_BUFFER_SIZE), whole blocks.
const BUF_SIZE: usize = 1536;
const TX_MAX: usize = 4 * BLOCK;
const RX_BYTE_MAX: u32 = 0x10_0000;
const TX_BUFFER_MAX: u32 = 0x1000;

static mut LINK: Link = Link {
    up: false,
    rx_count: 0,
    tx_count: 0,
    tx_free: 0,
    tx_seq: 0,
    throttled: false,
    send_config: false,
    stream: [0; STREAM],
    have: 0,
    tx: [0; TX_MAX],
    dropped: 0,
    next_poll: 0,
    again: false,
    irqs: 0,
    d1_low: 0,
};

fn link() -> &'static mut Link {
    unsafe { &mut *core::ptr::addr_of_mut!(LINK) }
}

/// Powers the C6 (if needed), resets it, identifies it on SDIO, sets the
/// card up as esp_hosted's host does and opens the data path (N2b-1, N2b-2).
pub fn up() -> Result<u32, &'static str> {
    let l = link();
    l.up = false;
    let was_on = tab5::wlan_power_on()?;
    // A reset pulse on CHIP_EN, as the esp_hosted host does; then the time
    // TCC gives the C6 to boot (wifi_setup.cpp:117-121).
    chip_en(false);
    ms(if was_on { 50 } else { 300 });
    chip_en(true);
    ms(1500);

    sd::host_up()?;
    pins();
    sd::set_wide(SLOT, false);
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

    // As esp_hosted's card init (port_esp_hosted_host_sdio.c:150-218):
    // interrupts of function 1 and the master enable, 512-byte blocks, then
    // 4 lines at 20 MHz (TCC's clock: 40 MHz failed there), then High Speed
    // if the card takes it.
    let ie = rw_direct(0, CCCR_INT_ENABLE, None)?;
    rw_direct(0, CCCR_INT_ENABLE, Some(ie | 1 | FUNC1))?;
    set_block_size(CCCR_FN0_BLOCK_SIZE)?;
    set_block_size(FBR1_BLOCK_SIZE)?;
    let bus = rw_direct(0, CCCR_BUS_IF, None)?;
    rw_direct(0, CCCR_BUS_IF, Some((bus & !3) | 2))?;
    sd::set_wide(SLOT, true);
    sd::set_card_clock(SLOT, 1, 20_000)?;
    high_speed()?;
    let token = read_reg(SLC_TOKEN_RDATA)?;
    event!(
        Info,
        "wlan.slc",
        int_raw = read_reg(SLC_INT_RAW)?,
        packet_len = read_reg(SLC_PACKET_LEN)?,
        tx_buffers = (token >> 16) & 0xFFF,
        bus_if = rw_direct(0, CCCR_BUS_IF, None)?
    );
    // The C6 has just restarted: its counters start from 0.
    l.rx_count = 0;
    l.tx_count = 0;
    l.tx_free = 0;
    l.have = 0;
    l.throttled = false;
    // Open the data path (sdio_drv.c:395-405,1146).
    rw_direct(1, SLC_HOST_TO_SLAVE, Some(1 << ESP_OPEN_DATA_PATH))?;
    l.up = true;
    l.again = true;
    Ok(rca)
}

fn d1_low() -> bool {
    unsafe { rd(GPIO_IN) & (1 << D1_PIN) == 0 }
}

/// From the interrupt handler: the card interrupt came. It stays masked
/// until `poll` has asked the C6 (the line stays low until then).
pub fn on_irq() {
    // The host interrupt is also the end of a deferred write's DMA.
    sd::finish_write();
    if sd::card_int_came(SLOT) {
        sd::card_int_enable(SLOT, false);
        link().irqs += 1;
    }
}

/// Whether the C6 has something to say: the scheduler does not sleep then.
pub fn pending() -> bool {
    let l = link();
    l.up && (l.again || sd::card_int_came(SLOT))
}

/// High Speed (40 MHz) if the card has it (SDIO spec 6.9, CCCR 0x13), as
/// the SD card (doc/31 §4). Back to 20 MHz if registers cannot be read at
/// 40 MHz.
fn high_speed() -> Result<(), &'static str> {
    let speed = rw_direct(0, CCCR_BUS_SPEED, None)?;
    if speed & SHS == 0 {
        event!(Info, "wlan.speed", khz = 20_000u32, cccr_speed = speed);
        return Ok(());
    }
    rw_direct(0, CCCR_BUS_SPEED, Some(speed | EHS))?;
    sd::set_card_clock(SLOT, 0, 40_000)?;
    let ok = (0..8).all(|_| read_reg(SLC_TOKEN_RDATA).is_ok());
    if !ok {
        sd::set_card_clock(SLOT, 1, 20_000)?;
        rw_direct(0, CCCR_BUS_SPEED, Some(speed & !EHS))?;
    }
    event!(Info, "wlan.speed", khz = if ok { 40_000u32 } else { 20_000 }, cccr_speed = speed);
    Ok(())
}

/// Reads what the C6 has sent (sdio_drv.c:1096-1290) when its card
/// interrupt came, or every `FALLBACK_MS` in case an edge was missed, and
/// passes each payload to `sink` with its interface type. The INIT event is
/// logged here.
pub fn poll(sink: &mut dyn FnMut(u8, &[u8])) -> Result<(), &'static str> {
    let l = link();
    if !l.up {
        return Ok(());
    }
    // A frame's write may have ended: its DMA interrupt (TI) stays raised
    // until the write is finished, and would keep waking this core.
    sd::finish_write();
    let now = Esp32p4v1::now();
    let signalled = sd::card_int_take(SLOT);
    if !signalled && !l.again && now < l.next_poll {
        return Ok(());
    }
    l.next_poll = now + crate::time::ms_to_ticks(FALLBACK_MS);
    let r = drain(l, sink);
    // Re-arm. The interrupt is an edge: if DAT1 is already low (the C6
    // raised another one meanwhile), none will come, so ask again at once.
    l.again = d1_low();
    if l.again {
        l.d1_low += 1;
    }
    sd::card_int_enable(SLOT, true);
    r
}

fn drain(l: &mut Link, sink: &mut dyn FnMut(u8, &[u8])) -> Result<(), &'static str> {
    // INT_RAW (0x50) to PACKET_LEN (0x60) in one transfer (doc/31 §4).
    // Whatever the interrupt bits say, the length decides whether there is
    // data: a packet that comes between this read and the clear below is
    // read next time.
    let mut regs = [0u8; (SLC_PACKET_LEN - SLC_INT_RAW + 4) as usize];
    rw_extended(SLC_INT_RAW, Data::Read(&mut regs))?;
    let word = |o: usize| u32::from_le_bytes([regs[o], regs[o + 1], regs[o + 2], regs[o + 3]]);
    let ints = word(0);
    let total = word((SLC_PACKET_LEN - SLC_INT_RAW) as usize) & LEN_MASK;
    if ints & INT_USED != 0 {
        write_reg(SLC_INT_CLR, ints & INT_USED)?;
        if ints & (1 << 7) != 0 {
            l.throttled = true;
        }
        if ints & (1 << 6) != 0 {
            l.throttled = false;
        }
    }
    let mut left = (total + RX_BYTE_MAX - l.rx_count) % RX_BYTE_MAX;
    while left > 0 {
        // Whole blocks except for the last piece, which is rounded up (the
        // C6 pads it with zeros).
        let k = (left as usize).min(READ_MAX);
        let xfer = k.next_multiple_of(BLOCK);
        if l.have + xfer > STREAM {
            // A packet longer than any the C6 sends: the stream is lost.
            l.dropped += 1;
            event!(Warn, "wlan.stream_lost", have = l.have as u32, left = left);
            l.have = 0;
        }
        let have = l.have;
        rw_extended(CMD53_END_ADDR - left, Data::Read(&mut l.stream[have..have + xfer]))?;
        l.have += k;
        left -= k as u32;
        l.rx_count = (l.rx_count + k as u32) % RX_BYTE_MAX;
        split(l, sink);
    }
    if l.send_config {
        l.send_config = false;
        // As the esp_hosted host after the INIT event (transport_drv.c:726-
        // 780, 945): host capabilities 0, the chip id it found, no raw
        // throughput test, flow control at 80 % / 60 % (TCC's values).
        let config = [
            PRIV_EVENT_INIT, 15, //
            0x44, 1, 0, // HOST_CAPABILITIES
            0x45, 1, CHIP_ESP32C6, // RCVD_ESP_FIRMWARE_CHIP_ID
            0x46, 1, 0, // SLV_CONFIG_TEST_RAW_TP
            0x47, 1, 80, // SLV_CONFIG_THROTTLE_HIGH_THRESHOLD
            0x48, 1, 60, // SLV_CONFIG_THROTTLE_LOW_THRESHOLD
        ];
        send(IF_PRIV, &config)?;
        event!(Info, "wlan.host_config_sent");
    }
    Ok(())
}

/// Passes on the whole packets at the start of the stream.
fn split(l: &mut Link, sink: &mut dyn FnMut(u8, &[u8])) {
    let mut at = 0;
    while l.have - at >= HEADER_LEN {
        let p = &l.stream[at..l.have];
        let len = u16::from_le_bytes([p[2], p[3]]) as usize;
        let offset = u16::from_le_bytes([p[4], p[5]]) as usize;
        if len == 0 || offset != HEADER_LEN || len > BUF_SIZE {
            // As esp_hosted: the rest of the stream cannot be decoded.
            event!(Warn, "wlan.bad_packet", len = len as u32, offset = offset as u32);
            l.dropped += 1;
            at = l.have;
            break;
        }
        if p.len() < offset + len {
            break;
        }
        let if_type = p[0] & 0xF;
        if if_type == IF_PRIV {
            log_packet(&p[..offset + len]);
            l.send_config = true;
        } else {
            sink(if_type, &p[offset..offset + len]);
        }
        at += offset + len;
    }
    l.stream.copy_within(at..l.have, 0);
    l.have -= at;
}

/// Sends one payload on interface `if_type` (sdio_drv.c:586-770). `Err`
/// "busy" when the C6 has no free buffer, "throttled" while it asked us to
/// pause.
pub fn send(if_type: u8, payload: &[u8]) -> Result<(), &'static str> {
    let l = link();
    if !l.up {
        return Err("Wi-Fi not up");
    }
    let total = HEADER_LEN + payload.len();
    if total > BUF_SIZE {
        return Err("too long");
    }
    if l.throttled {
        return Err("throttled");
    }
    if l.tx_free == 0 {
        let token = read_reg(SLC_TOKEN_RDATA)?;
        l.tx_free = (((token >> 16) & 0xFFF) + TX_BUFFER_MAX - l.tx_count) % TX_BUFFER_MAX;
        if l.tx_free == 0 {
            return Err("busy");
        }
    }
    let xfer = total.next_multiple_of(BLOCK);
    let h = &mut l.tx[..HEADER_LEN];
    h.fill(0);
    h[0] = if_type & 0xF;
    h[2..4].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    h[4..6].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    h[8..10].copy_from_slice(&l.tx_seq.to_le_bytes());
    l.tx[HEADER_LEN..total].copy_from_slice(payload);
    l.tx[total..xfer].fill(0);
    rw_extended(CMD53_END_ADDR - total as u32, Data::Write(&l.tx[..xfer]))?;
    l.tx_seq = l.tx_seq.wrapping_add(1);
    l.tx_count = (l.tx_count + 1) % TX_BUFFER_MAX;
    l.tx_free -= 1;
    Ok(())
}

/// Counters for `wlan` statistics: (card interrupts, re-arms with DAT1 low,
/// ms in SDIO commands, SDIO commands).
pub fn irq_stats() -> (u32, u32, u32, u32) {
    let l = link();
    let (ticks, cmds) = sd::busy(SLOT);
    (l.irqs, l.d1_low, (ticks / 16_000) as u32, cmds)
}

/// `wlanprobe`: starts the transport and logs what the C6 sends for 2 s.
pub fn probe() -> Result<u32, &'static str> {
    let rca = up()?;
    let end = Esp32p4v1::now() + 2 * 16_000_000;
    let mut n = 0u32;
    while Esp32p4v1::now() < end {
        poll(&mut |if_type, p| {
            n += 1;
            event!(Info, "wlan.rx", if_type = if_type, len = p.len() as u32, head = &p[..p.len().min(48)]);
        })?;
        delay_us(1000);
    }
    event!(Info, "wlan.probe_done", packets = n, rx_bytes = link().rx_count);
    Ok(rca)
}
