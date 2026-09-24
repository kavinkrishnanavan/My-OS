//! ATA PIO (legacy IDE) driver for the primary bus's slave drive — the
//! kernel's data disk (`dist/myos-data.img`, QEMU's second `-drive`; the
//! first is the boot image itself, primary *master*).
//!
//! PIO over AHCI/virtio-blk: no PCI enumeration or DMA setup needed,
//! just port I/O on the well-known legacy ports — the same tradeoff
//! `net::rtl8139` makes for simplicity over throughput. Polling (no
//! IRQ-driven completion) means a read/write busy-waits the calling
//! kernel thread, not the whole machine — safe now that `task::thread`
//! gives every caller its own preemptible stack to wait on.

use x86_64::instructions::port::Port;

const DATA: u16 = 0x1F0;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LOW: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HIGH: u16 = 0x1F5;
const DRIVE_HEAD: u16 = 0x1F6;
const STATUS_COMMAND: u16 = 0x1F7;

const CMD_READ_PIO: u8 = 0x20;
const CMD_WRITE_PIO: u8 = 0x30;
const CMD_CACHE_FLUSH: u8 = 0xE7;

const STATUS_ERR: u8 = 1 << 0;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_BSY: u8 = 1 << 7;

pub const SECTOR_SIZE: usize = 512;

/// Upper bound on how many times `wait_not_busy`/`wait_drq` re-read the
/// status port before giving up. Each iteration under QEMU is a real VM
/// exit (port I/O isn't free to emulate), so this is deliberately
/// generous — tens of seconds' worth in the worst case — rather than
/// tuned tight. Without *some* bound, these were plain `loop {}`: no
/// timeout, no diagnostic, indistinguishable from the kernel just being
/// stuck. That's the leading suspect for a real, if inconsistently
/// reproducible, symptom this session hit more than once: a boot that
/// stalled dead partway through reading a userland ELF off this same
/// disk, with zero further output, that a from-scratch retry sometimes
/// (not always) got past — exactly what "the status bits took
/// unusually long to transition, and nothing was watching the clock"
/// would look like from the serial log, as opposed to a logic bug (nothing
/// in the actual FAT32/ELF-reading code path changed between a stalled
/// attempt and a clean one).
const POLL_LIMIT: u32 = 20_000_000;

#[derive(Clone, Copy)]
pub struct AtaDrive {
    slave: bool,
}

impl AtaDrive {
    /// The primary bus's slave drive — where QEMU's second `-drive`
    /// lands when the first (the boot image) already occupies primary
    /// master. See the README for the exact QEMU invocation.
    pub const fn primary_slave() -> Self {
        AtaDrive { slave: true }
    }

    fn wait_not_busy(&self) -> Result<u8, &'static str> {
        let mut status: Port<u8> = Port::new(STATUS_COMMAND);
        for _ in 0..POLL_LIMIT {
            let s = unsafe { status.read() };
            if s & STATUS_BSY == 0 {
                return Ok(s);
            }
            core::hint::spin_loop();
        }
        Err("ATA: timed out waiting for BSY to clear")
    }

    fn wait_drq(&self) -> Result<(), &'static str> {
        for _ in 0..POLL_LIMIT {
            let s = self.wait_not_busy()?;
            if s & STATUS_ERR != 0 {
                return Err("ATA device reported an error");
            }
            if s & STATUS_DRQ != 0 {
                return Ok(());
            }
        }
        Err("ATA: timed out waiting for DRQ")
    }

    fn select_and_setup(&self, lba: u32) {
        let head = ((lba >> 24) & 0x0F) as u8;
        let select = 0xE0 | if self.slave { 0x10 } else { 0 } | head;
        unsafe {
            Port::<u8>::new(DRIVE_HEAD).write(select);
            Port::<u8>::new(SECTOR_COUNT).write(1u8);
            Port::<u8>::new(LBA_LOW).write((lba & 0xFF) as u8);
            Port::<u8>::new(LBA_MID).write(((lba >> 8) & 0xFF) as u8);
            Port::<u8>::new(LBA_HIGH).write(((lba >> 16) & 0xFF) as u8);
        }
    }

    pub fn read_sector(&self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), &'static str> {
        self.wait_not_busy()?;
        self.select_and_setup(lba);
        unsafe { Port::<u8>::new(STATUS_COMMAND).write(CMD_READ_PIO) };
        self.wait_drq()?;

        let mut data: Port<u16> = Port::new(DATA);
        for word in buf.chunks_exact_mut(2) {
            let w = unsafe { data.read() };
            word[0] = (w & 0xFF) as u8;
            word[1] = (w >> 8) as u8;
        }
        Ok(())
    }

    pub fn write_sector(&self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
        self.wait_not_busy()?;
        self.select_and_setup(lba);
        unsafe { Port::<u8>::new(STATUS_COMMAND).write(CMD_WRITE_PIO) };
        self.wait_drq()?;

        let mut data: Port<u16> = Port::new(DATA);
        for word in buf.chunks_exact(2) {
            let w = (word[0] as u16) | ((word[1] as u16) << 8);
            unsafe { data.write(w) };
        }

        // Flush so the write is actually durable before we tell the
        // caller it succeeded, not just sitting in the drive's cache.
        self.wait_not_busy()?;
        unsafe { Port::<u8>::new(STATUS_COMMAND).write(CMD_CACHE_FLUSH) };
        self.wait_not_busy()?;
        Ok(())
    }
}
