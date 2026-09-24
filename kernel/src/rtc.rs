//! CMOS Real-Time Clock reader.
//!
//! Every PC (and QEMU, faithfully) exposes the battery-backed hardware
//! clock through a pair of legacy ports: 0x70 selects a CMOS register
//! index, 0x71 reads/writes it. This is the same polled-port-I/O
//! approach `disk/ata.rs` uses for the ATA controller — no IRQ, just
//! read the register and go. Registers of interest here: 0x00 seconds,
//! 0x02 minutes, 0x04 hours, 0x07 day of month, 0x08 month, 0x09
//! two-digit year, 0x0A status A (update-in-progress flag), 0x0B status
//! B (BCD/binary and 12/24-hour format flags).

use x86_64::instructions::port::Port;

const CMOS_ADDRESS: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

const STATUS_A_UPDATE_IN_PROGRESS: u8 = 1 << 7;
const STATUS_B_24_HOUR: u8 = 1 << 1;
const STATUS_B_BINARY: u8 = 1 << 2;
const HOUR_PM_FLAG: u8 = 1 << 7;

/// Same bounded-poll precedent as `disk/ata.rs`'s `POLL_LIMIT`: this
/// kernel never runs an unbounded `loop {}`, so a stuck "update in
/// progress" flag gets a best-effort reading instead of hanging.
const POLL_LIMIT: u32 = 20_000_000;

/// Bounded retries for the read/re-read-and-compare stabilization loop
/// below, not a hardware-derived number — just "try a handful of times,
/// then give up and trust the last read" rather than loop forever.
const STABLE_READ_ATTEMPTS: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn read_reg(index: u8) -> u8 {
    unsafe {
        let mut addr: Port<u8> = Port::new(CMOS_ADDRESS);
        let mut data: Port<u8> = Port::new(CMOS_DATA);
        addr.write(index);
        data.read()
    }
}

/// The RTC sets this bit while it's in the middle of updating its
/// registers; a read that lands mid-update can see a torn value (e.g.
/// seconds already rolled over but minutes hasn't yet), so callers wait
/// for it to clear before reading.
fn update_in_progress() -> bool {
    read_reg(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0
}

fn wait_update_complete() {
    for _ in 0..POLL_LIMIT {
        if !update_in_progress() {
            return;
        }
        core::hint::spin_loop();
    }
}

fn bcd_to_binary(val: u8) -> u8 {
    ((val >> 4) * 10) + (val & 0x0F)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RawReading {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
}

fn read_raw() -> RawReading {
    RawReading {
        second: read_reg(REG_SECONDS),
        minute: read_reg(REG_MINUTES),
        hour: read_reg(REG_HOURS),
        day: read_reg(REG_DAY),
        month: read_reg(REG_MONTH),
        year: read_reg(REG_YEAR),
    }
}

/// Reads the current date/time from the CMOS RTC.
pub fn now() -> RtcTime {
    wait_update_complete();

    let mut reading = read_raw();
    for _ in 0..STABLE_READ_ATTEMPTS {
        wait_update_complete();
        let next = read_raw();
        if next == reading {
            break;
        }
        reading = next;
    }

    let status_b = read_reg(REG_STATUS_B);
    let is_binary = status_b & STATUS_B_BINARY != 0;
    let is_24_hour = status_b & STATUS_B_24_HOUR != 0;

    let mut second = reading.second;
    let mut minute = reading.minute;
    let mut hour_raw = reading.hour;
    let mut day = reading.day;
    let mut month = reading.month;
    let mut year = reading.year;

    let pm = !is_24_hour && (hour_raw & HOUR_PM_FLAG != 0);
    hour_raw &= !HOUR_PM_FLAG;

    if !is_binary {
        second = bcd_to_binary(second);
        minute = bcd_to_binary(minute);
        hour_raw = bcd_to_binary(hour_raw);
        day = bcd_to_binary(day);
        month = bcd_to_binary(month);
        year = bcd_to_binary(year);
    }

    let mut hour = hour_raw;
    if !is_24_hour {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }

    // CMOS has no standard century register location (implementations
    // vary, and QEMU's default doesn't expose one), so just assume
    // 2000-2099 rather than reading a register that may not exist.
    RtcTime {
        year: 2000 + year as u32,
        month,
        day,
        hour,
        minute,
        second,
    }
}
