//! RTL8139 NIC driver — QEMU's default emulated Ethernet card
//! (`-net nic,model=rtl8139`, or just `-net nic` with no model given).
//!
//! Register map and ring-buffer behaviour per the OSDev wiki / Realtek
//! RTL8139(C) programming guide. Everything here is interrupt-driven: we
//! program the ring addresses once at init, unmask the IRQ, and from then
//! on the ISR just flips flags that the async network task checks — see
//! the "non-blocking" note in `interrupts.rs`.

use crate::{interrupts, memory, pci, serial_println};
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use spin::Once;
use x86_64::instructions::port::Port;

const RX_BUF_LEN: usize = 8192 + 16 + 1500; // 8K ring + 16 header slack + max-frame overrun slack
const TX_BUF_LEN: usize = 4096; // one page comfortably holds a max 1518-byte frame
const NUM_TX_DESC: usize = 4;

struct Regs {
    io_base: u16,
}

impl Regs {
    fn port8(&self, offset: u16) -> Port<u8> {
        Port::new(self.io_base + offset)
    }
    fn port16(&self, offset: u16) -> Port<u16> {
        Port::new(self.io_base + offset)
    }
    fn port32(&self, offset: u16) -> Port<u32> {
        Port::new(self.io_base + offset)
    }
}

struct Rtl8139 {
    regs: Regs,
    rx_buf_phys: u64,
    rx_offset: AtomicU16, // CAPR-relative read cursor into the RX ring
    tx_bufs_phys: [u64; NUM_TX_DESC],
    next_tx_desc: AtomicU32,
    mac: [u8; 6],
}

static NIC: Once<Rtl8139> = Once::new();

const REG_MAC0: u16 = 0x00;
const REG_TSAD: [u16; 4] = [0x20, 0x24, 0x28, 0x2C];
const REG_TSD: [u16; 4] = [0x10, 0x14, 0x18, 0x1C];
const REG_RBSTART: u16 = 0x30;
const REG_CMD: u16 = 0x37;
const REG_CAPR: u16 = 0x38;
const REG_IMR: u16 = 0x3C;
const REG_ISR: u16 = 0x3E;
const REG_TCR: u16 = 0x40;
const REG_RCR: u16 = 0x44;
const REG_CONFIG1: u16 = 0x52;

const CMD_RESET: u8 = 0x10;
const CMD_RX_ENABLE: u8 = 0x08;
const CMD_TX_ENABLE: u8 = 0x04;

const ISR_ROK: u16 = 0x01; // receive OK
const ISR_TOK: u16 = 0x04; // transmit OK

/// Finds the RTL8139 on the PCI bus and brings it up: power on, reset,
/// program the RX/TX ring physical addresses, unmask its IRQ. Returns the
/// card's burned-in MAC address, which the IP stack needs for its
/// Ethernet header.
pub fn init() -> Option<[u8; 6]> {
    let dev = pci::find_rtl8139()?;
    pci::enable_bus_mastering(dev.bus, dev.slot, dev.function);

    let bar0 = pci::config_read_bar0(dev.bus, dev.slot, dev.function);
    assert_eq!(bar0 & 0x1, 1, "RTL8139 BAR0 should be I/O space");
    let io_base = (bar0 & 0xFFFC) as u16;
    let regs = Regs { io_base };

    unsafe {
        // Power on.
        regs.port8(REG_CONFIG1).write(0x00);

        // Software reset; RST bit self-clears when done. This is a
        // microsecond-scale hardware handshake at driver init, not the
        // network-waiting this architecture avoids — nothing here blocks
        // on data ever arriving from the wire.
        regs.port8(REG_CMD).write(CMD_RESET);
        while regs.port8(REG_CMD).read() & CMD_RESET != 0 {
            core::hint::spin_loop();
        }

        let rx_buf_phys = memory::alloc_dma_region(RX_BUF_LEN);
        regs.port32(REG_RBSTART).write(rx_buf_phys as u32);

        let mut tx_bufs_phys = [0u64; NUM_TX_DESC];
        for (i, slot) in tx_bufs_phys.iter_mut().enumerate() {
            *slot = memory::alloc_dma_region(TX_BUF_LEN);
            regs.port32(REG_TSAD[i]).write(*slot as u32);
        }

        // Accept broadcast + multicast + physical-match packets, and
        // (WRAP=1, bit 7) let a packet straddling the ring's end write
        // into the 1500-byte overrun slack instead of wrapping mid-frame.
        // RBLEN=00 selects the 8K ring size that matches RX_BUF_LEN.
        regs.port32(REG_RCR).write(0x0000_008F);

        // Standard TCR: default interframe gap, DMA burst unrestricted.
        regs.port32(REG_TCR).write(0x0000_0000);

        regs.port8(REG_CMD)
            .write(CMD_RX_ENABLE | CMD_TX_ENABLE);

        // Only ROK/TOK — we don't care about the error counters for a
        // toy stack, and fewer unmasked sources means fewer spurious
        // wakeups of the async net task.
        regs.port16(REG_IMR).write(ISR_ROK | ISR_TOK);

        let mut mac = [0u8; 6];
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = regs.port8(REG_MAC0 + i as u16).read();
        }

        interrupts::unmask_irq(dev.interrupt_line);

        serial_println!(
            "rtl8139: io_base=0x{:x} irq={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            io_base,
            dev.interrupt_line,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );

        NIC.call_once(|| Rtl8139 {
            regs,
            rx_buf_phys,
            rx_offset: AtomicU16::new(0),
            tx_bufs_phys,
            next_tx_desc: AtomicU32::new(0),
            mac,
        });

        Some(mac)
    }
}

/// Called from the IDT handler. Deliberately tiny: read+ack ISR, wake
/// whatever's waiting on the network stack, return. All real work
/// happens in `Rtl8139Device::receive`, driven by the async executor
/// rather than from interrupt context.
pub fn handle_interrupt() {
    if let Some(nic) = NIC.get() {
        unsafe {
            let status = nic.regs.port16(REG_ISR).read();
            nic.regs.port16(REG_ISR).write(status); // write-1-to-clear
            if status & (ISR_ROK | ISR_TOK) != 0 {
                crate::net::stack::wake_net_waiters();
            }
        }
    }
}

pub fn mac() -> [u8; 6] {
    NIC.get().expect("rtl8139 not initialized").mac
}

/// Whether the NIC's hardware write cursor (CBR) has moved past our
/// software read cursor, i.e. there's at least one unread frame sitting
/// in the ring. Cheap register peek, not a consumed flag, so it stays
/// correct no matter how many frames arrived between polls.
pub fn rx_ready() -> bool {
    let Some(nic) = NIC.get() else { return false };
    unsafe {
        let cbr = nic.regs.port16(0x3A).read();
        cbr != nic.rx_offset.load(Ordering::Relaxed)
    }
}

/// Copies the next queued frame (if any) out of the RX ring into `buf`,
/// returning the frame length. The RTL8139 prefixes each frame in the
/// ring with a 4-byte header (status u16, length u16) that we skip.
pub fn recv(buf: &mut [u8]) -> Option<usize> {
    let nic = NIC.get()?;
    unsafe {
        let ring_virt = memory::phys_to_virt(nic.rx_buf_phys) as *const u8;
        let offset = nic.rx_offset.load(Ordering::Relaxed) as usize;

        let header = core::ptr::read_unaligned(ring_virt.add(offset) as *const u16);
        let status = header;
        let length = core::ptr::read_unaligned(ring_virt.add(offset + 2) as *const u16) as usize;

        if status & 0x01 == 0 || length < 4 || length > 1600 {
            return None; // not a valid ROK frame; avoid reading garbage on a spurious wake
        }

        let payload_len = length - 4; // exclude the trailing CRC
        let copy_len = payload_len.min(buf.len());
        for i in 0..copy_len {
            buf[i] = *ring_virt.add(offset + 4 + i);
        }

        // Advance the read cursor past this frame, 4-byte aligned, and
        // wrap within the 8K ring.
        let mut new_offset = (offset + length + 4 + 3) & !3;
        if new_offset >= 8192 {
            new_offset -= 8192;
        }
        nic.rx_offset.store(new_offset as u16, Ordering::Relaxed);
        nic.regs
            .port16(REG_CAPR)
            .write((new_offset as u16).wrapping_sub(16));

        Some(copy_len)
    }
}

/// Queues `frame` for transmission on the next round-robin TX descriptor.
/// The RTL8139 has four independent descriptors; we cycle through them so
/// back-to-back sends don't have to wait for the previous one to
/// physically finish going out on the wire.
pub fn send(frame: &[u8]) {
    let Some(nic) = NIC.get() else { return };
    assert!(frame.len() <= TX_BUF_LEN, "frame exceeds TX buffer");

    let desc = (nic.next_tx_desc.fetch_add(1, Ordering::Relaxed) as usize) % NUM_TX_DESC;
    let buf_virt = memory::phys_to_virt(nic.tx_bufs_phys[desc]) as *mut u8;

    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buf_virt, frame.len());
        // TSD: bits 0-12 = descriptor size, rest cleared -> starts the
        // transmit DMA for this descriptor.
        nic.regs
            .port32(REG_TSD[desc])
            .write((frame.len() as u32) & 0x1FFF);
    }
}
