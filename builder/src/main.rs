//! Runs on the HOST. Stitches the compiled kernel ELF together with the
//! `bootloader` crate's BIOS/UEFI boot code to produce disk images QEMU
//! (or real hardware, via `dd`) can boot straight into.
//!
//! Usage: `cargo run -p builder --release -- <path-to-kernel-elf>`

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Bytes for the kernel's data disk (`kernel/src/fs.rs` mounts FAT32 off
/// of it via `kernel/src/disk/ata.rs`) — plenty for now; it's just a raw
/// file QEMU's second `-drive` points at.
const DATA_DISK_SIZE: u64 = 64 * 1024 * 1024;

/// LBA the one partition starts at. `embedded-sdmmc` (the kernel-side FAT
/// reader — see `kernel/src/fs.rs`) expects a real MBR-partitioned disk,
/// like an SD card, not a filesystem starting at byte 0: the partition
/// table itself needs to live somewhere, so it takes sector 0.
const PARTITION_START_LBA: u32 = 2048; // 1 MiB in, the usual convention
const SECTOR_SIZE: u64 = 512;
const PARTITION_TYPE_FAT32_LBA: u8 = 0x0C;

/// A `Read + Write + Seek` view of `file` that's offset so byte 0 is
/// `PARTITION_START_LBA`, not the start of the disk — lets `fatfs`
/// format just the partition without knowing anything about the MBR
/// sitting before it.
struct PartitionView<'a> {
    file: &'a mut std::fs::File,
    part_start: u64,
    part_len: u64,
}

impl<'a> Read for PartitionView<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl<'a> Write for PartitionView<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl<'a> Seek for PartitionView<'a> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target: i64 = match pos {
            SeekFrom::Start(p) => self.part_start as i64 + p as i64,
            SeekFrom::Current(p) => self.file.stream_position()? as i64 + p,
            SeekFrom::End(p) => (self.part_start + self.part_len) as i64 + p,
        };
        let target = target.max(self.part_start as i64) as u64;
        let new_abs = self.file.seek(SeekFrom::Start(target))?;
        Ok(new_abs - self.part_start)
    }
}

fn main() {
    let kernel_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("target")
                .join("x86_64-unknown-none")
                .join("release")
                .join("myos-kernel")
        });

    if !kernel_path.exists() {
        eprintln!(
            "kernel binary not found at {}\nBuild it first: cargo build -p myos-kernel --release",
            kernel_path.display()
        );
        std::process::exit(1);
    }

    // Real, separately-compiled ELF64 programs (`kernel/src/elf.rs`
    // loads them; see the Milestone 4 plan) — (crate dir name, binary
    // name, name to embed them under on the data disk). Each builds the
    // same way, landing in the same shared target dir since they all
    // target x86_64-unknown-none.
    const USERLAND_PROGRAMS: &[(&str, &str, &str)] = &[
        ("hello", "hello", "hello.elf"),
        ("counter", "counter", "counter.elf"),
        ("spawner", "spawner", "spawner.elf"),
        ("httpget", "httpget", "httpget.elf"),
        ("pipedemo", "pipedemo", "pipedemo.elf"),
    ];

    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("target"));
    let target_dir = target_root.join("x86_64-unknown-none").join("release");
    let userland_binaries: Vec<(PathBuf, &str)> = USERLAND_PROGRAMS
        .iter()
        .map(|(crate_dir, binary_name, disk_name)| {
            let path = target_dir.join(binary_name);
            if !path.exists() {
                eprintln!(
                    "userland/{crate_dir} binary not found at {}\nBuild it first (from userland/{crate_dir}/): cargo build --release",
                    path.display()
                );
                std::process::exit(1);
            }
            (path, *disk_name)
        })
        .collect();

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("dist");
    std::fs::create_dir_all(&out_dir).expect("failed to create dist/ output directory");

    let bios_path = out_dir.join("myos-bios.img");
    let uefi_path = out_dir.join("myos-uefi.img");

    // The physical-memory mapping the NIC driver's DMA buffers depend on
    // is compile-time kernel config (`kernel/src/main.rs`'s
    // `BOOTLOADER_CONFIG`, passed to `entry_point!`), not something set
    // here — this just packages the kernel ELF into bootable images.
    bootloader::BiosBoot::new(&kernel_path)
        .create_disk_image(&bios_path)
        .expect("failed to create BIOS disk image");
    println!("BIOS image:  {}", bios_path.display());

    bootloader::UefiBoot::new(&kernel_path)
        .create_disk_image(&uefi_path)
        .expect("failed to create UEFI disk image");
    println!("UEFI image:  {}", uefi_path.display());

    let data_path = out_dir.join("myos-data.img");
    if !data_path.exists() {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&data_path)
            .expect("failed to create data disk image");
        file.set_len(DATA_DISK_SIZE).expect("failed to size data disk image");

        let part_start_bytes = PARTITION_START_LBA as u64 * SECTOR_SIZE;
        let part_len_bytes = DATA_DISK_SIZE - part_start_bytes;
        let part_len_sectors = (part_len_bytes / SECTOR_SIZE) as u32;

        // Format just the partition (everything from PARTITION_START_LBA
        // onward) as FAT32, through the offsetting view above — `fatfs`
        // itself has no idea an MBR exists before it. Scoped in its own
        // block so every borrow of `file` (via `view`) ends before the
        // MBR write below needs `file` back.
        {
            let mut view = PartitionView { file: &mut file, part_start: part_start_bytes, part_len: part_len_bytes };
            fatfs::format_volume(&mut view, fatfs::FormatVolumeOptions::new())
                .expect("failed to format data disk partition as FAT32");
        }

        // MBR: one partition entry (bytes 446..462) pointing at the FAT32
        // volume just formatted above, signature 0x55AA at 510..512.
        // Everything else (boot code, CHS fields) is zeroed — we only
        // ever boot through UEFI/BIOS-bootloader's own image, never this
        // disk, so none of that matters, only that `embedded-sdmmc`
        // (kernel-side) sees a structurally valid MBR.
        let mut mbr = [0u8; 512];
        let entry = &mut mbr[446..462];
        entry[0] = 0x00; // not bootable
        entry[4] = PARTITION_TYPE_FAT32_LBA;
        entry[8..12].copy_from_slice(&PARTITION_START_LBA.to_le_bytes());
        entry[12..16].copy_from_slice(&part_len_sectors.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        file.seek(SeekFrom::Start(0)).expect("failed to seek to MBR");
        file.write_all(&mbr).expect("failed to write MBR");

        println!("Data image: {} (MBR + FAT32 partition, seeded hello.txt + userland binaries)", data_path.display());
    }
    println!("Data image: {} (refreshing seeded files)", data_path.display());

    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&data_path)
            .expect("failed to open data disk image for seeding");
        let part_start_bytes = PARTITION_START_LBA as u64 * SECTOR_SIZE;
        let part_len_bytes = DATA_DISK_SIZE - part_start_bytes;
        let mut view = PartitionView { file: &mut file, part_start: part_start_bytes, part_len: part_len_bytes };
        view.seek(SeekFrom::Start(0))
            .expect("failed to seek to data partition");
        let fs = fatfs::FileSystem::new(&mut view, fatfs::FsOptions::new())
            .expect("failed to mount data disk partition for seeding");

        let root = fs.root_dir();
        let mut hello = root
            .create_file("hello.txt")
            .expect("failed to create hello.txt on data disk");
        hello.truncate().expect("failed to truncate hello.txt");
        hello
            .write_all(b"Hello from the host filesystem!\n")
            .expect("failed to write hello.txt");

        for (path, disk_name) in &userland_binaries {
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            let mut file = root
                .create_file(disk_name)
                .unwrap_or_else(|e| panic!("failed to create {disk_name} on data disk: {e}"));
            file.truncate().unwrap_or_else(|e| panic!("failed to truncate {disk_name}: {e}"));
            file.write_all(&bytes).unwrap_or_else(|e| panic!("failed to write {disk_name}: {e}"));
        }
    }

    println!(
        "\nRun with QEMU:\n  qemu-system-x86_64 -drive format=raw,file={} -drive format=raw,file={} -netdev user,id=n0 -device rtl8139,netdev=n0 -serial stdio\n",
        bios_path.display(),
        data_path.display()
    );
}
