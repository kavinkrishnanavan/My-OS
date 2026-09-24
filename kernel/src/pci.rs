//! Minimal PCI config-space access (legacy I/O ports 0xCF8/0xCFC).
//!
//! We only need enough of PCI to find the network controller, read its
//! BAR (I/O port base) and interrupt line, and enable bus mastering so it
//! can DMA descriptor rings to/from our RAM.

use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub interrupt_line: u8,
}

fn address(bus: u8, slot: u8, function: u8, offset: u8) -> u32 {
    (1 << 31)
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

fn config_read32(bus: u8, slot: u8, function: u8, offset: u8) -> u32 {
    let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
    let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
    unsafe {
        addr_port.write(address(bus, slot, function, offset));
        data_port.read()
    }
}

fn config_write32(bus: u8, slot: u8, function: u8, offset: u8, value: u32) {
    let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
    let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
    unsafe {
        addr_port.write(address(bus, slot, function, offset));
        data_port.write(value);
    }
}

pub fn config_read_bar0(bus: u8, slot: u8, function: u8) -> u32 {
    config_read32(bus, slot, function, 0x10)
}

/// Sets the PCI command register's bus-master (bit 2) and I/O-space
/// (bit 0) enable bits so the device can actually respond to I/O port
/// access and DMA into our memory.
pub fn enable_bus_mastering(bus: u8, slot: u8, function: u8) {
    let command = config_read32(bus, slot, function, 0x04);
    config_write32(bus, slot, function, 0x04, command | 0x7);
}

pub struct PciScan {
    bus: u16,
    slot: u8,
    function: u8,
}

pub fn scan() -> PciScan {
    PciScan {
        bus: 0,
        slot: 0,
        function: 0,
    }
}

impl Iterator for PciScan {
    type Item = PciDevice;

    fn next(&mut self) -> Option<PciDevice> {
        while self.bus < 256 {
            let (bus, slot, function) = (self.bus as u8, self.slot, self.function);

            // Advance the cursor for the next call before we possibly
            // return early below.
            self.function += 1;
            if self.function >= 8 {
                self.function = 0;
                self.slot += 1;
                if self.slot == 0 {
                    self.bus += 1;
                }
            }

            let id = config_read32(bus, slot, function, 0x00);
            let vendor_id = (id & 0xFFFF) as u16;
            if vendor_id == 0xFFFF {
                continue; // no device at this bus/slot/function
            }
            let device_id = (id >> 16) as u16;

            let class_reg = config_read32(bus, slot, function, 0x08);
            let class = (class_reg >> 24) as u8;
            let subclass = (class_reg >> 16) as u8;

            let irq_reg = config_read32(bus, slot, function, 0x3C);
            let interrupt_line = (irq_reg & 0xFF) as u8;

            return Some(PciDevice {
                bus,
                slot,
                function,
                vendor_id,
                device_id,
                class,
                subclass,
                interrupt_line,
            });
        }
        None
    }
}

/// Finds the first RTL8139 (Realtek, device id 0x8139) — QEMU's default
/// `-net nic,model=rtl8139` — on the bus.
pub fn find_rtl8139() -> Option<PciDevice> {
    scan().find(|d| d.vendor_id == 0x10EC && d.device_id == 0x8139)
}
