//! Enables the CPU state `fxsave`/`fxrstor` need.
//!
//! x86_64's baseline ABI always has SSE2, so the compiler is free to use
//! `xmm` registers in ordinary code — and does, especially once real
//! crypto (TLS's AES-GCM, P-256, SHA-2) is in the picture. Our interrupt
//! trampolines (`interrupts.rs`) save/restore that state with
//! `fxsave`/`fxrstor` so an interrupt landing mid-computation doesn't
//! silently corrupt it. Those instructions themselves fault (#UD) unless
//! the OS has explicitly opted in via `CR4.OSFXSR` — which, on boot,
//! nothing has done yet. Must run before `interrupts::init()` enables
//! interrupts, since the very first tick would otherwise hit that fault.

use crate::serial_println;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

pub fn init() {
    unsafe {
        Cr0::update(|flags| {
            flags.remove(Cr0Flags::EMULATE_COPROCESSOR);
            flags.insert(Cr0Flags::MONITOR_COPROCESSOR);
        });
        Cr4::update(|flags| {
            flags.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE);
        });
    }
    serial_println!(
        "fpu: cr0={:#x} cr4={:#x} (OSFXSR set: {})",
        Cr0::read_raw(),
        Cr4::read_raw(),
        Cr4::read().contains(Cr4Flags::OSFXSR)
    );
}
