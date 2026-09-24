//! Programmable Interval Timer (PIT), channel 0, IRQ0.
//!
//! Two jobs: give smoltcp a millisecond clock for its own retransmit/
//! lease timers, and give the async executor a periodic nudge
//! (`NET_WAKER.wake()`) so those timers actually get serviced even when
//! no packet has arrived to wake things up via the NIC IRQ.

use crate::interrupts;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

const PIT_FREQUENCY_HZ: u32 = 100; // 10ms tick — plenty fine for TCP timers
const PIT_BASE_FREQUENCY: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn uptime_ms() -> u64 {
    TICKS.load(Ordering::Relaxed) * (1000 / PIT_FREQUENCY_HZ as u64)
}

pub fn init() {
    let divisor = PIT_BASE_FREQUENCY / PIT_FREQUENCY_HZ;
    unsafe {
        let mut command: Port<u8> = Port::new(0x43);
        let mut channel0: Port<u8> = Port::new(0x40);
        command.write(0x36); // channel 0, lobyte/hibyte, mode 3 (square wave)
        channel0.write((divisor & 0xFF) as u8);
        channel0.write((divisor >> 8) as u8);
    }
    interrupts::unmask_irq(0);
}

pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::net::stack::wake_net_waiters();
}
