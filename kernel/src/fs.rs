//! A real filesystem, at last: FAT32 (via the `embedded-sdmmc` crate,
//! natively no_std) on top of the ATA PIO data disk (`disk::ata`). The
//! disk is laid out the way real disks are — an MBR at LBA 0 with one
//! FAT32 partition — because `embedded-sdmmc` expects that (it's built
//! for SD cards, which always carry a partition table); see
//! `builder/src/main.rs` for where that MBR + the FAT32 filesystem
//! inside it actually get created, using the same `fatfs` crate this
//! module's doc comment used to mention (moved host-side only — its
//! no_std path depends on an unmaintained shim that doesn't build here).
//!
//! Mounting (`VolumeManager::new` + `open_volume`) is cheap enough — a
//! few sector reads — that every call here mounts fresh rather than
//! keeping a long-lived handle around, which keeps this simple. Each
//! call *does* hold `DISK_LOCK` across its own disk I/O, deliberately —
//! see that static's doc comment for why the underlying hardware needs
//! that now that real concurrent ring-3 programs can trigger it.

use crate::disk::ata::{AtaDrive, SECTOR_SIZE};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Error as FsError, Mode, RawDirectory, RawVolume,
    TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

/// The ATA disk is one piece of shared hardware, and `disk::ata::AtaDrive`
/// does nothing to serialize access to it — each PIO transaction is a
/// sequence of port reads/writes (select drive, set LBA, issue command,
/// poll status, read/write 256 words) that has to complete uninterrupted
/// by another transaction touching the same ports, or the drive's
/// internal state machine gets two callers' requests interleaved and
/// hands back garbage (or nothing) to both. That was invisible as long
/// as only one thing ever did disk I/O at a time; once real ring-3
/// programs started opening/reading files themselves *while* other
/// threads (including the kernel's own boot-time checks) could also be
/// mid-transaction, it wasn't anymore — this is what actually serializes
/// every `fs` operation against every other one.
///
/// Every public function here wraps its whole body (lock acquire through
/// release) in `without_interrupts` — not just for the ATA transaction
/// itself, but to close a real deadlock: this `Mutex` isn't reentrant,
/// and a ring-3 thread's own file syscall reaches these same functions
/// from inside its `int 0x80` handler, which — like every interrupt gate
/// in this kernel — runs with `IF` cleared for its whole duration. If the
/// kernel's own boot thread were preempted mid-operation while holding
/// `DISK_LOCK`, and the timer tick switched to a thread whose syscall
/// handler then spun on this same lock, that spin would run with
/// interrupts disabled — meaning the PIT could never fire again to
/// preempt back to the actual lock holder and let it finish. Nothing
/// (not even that thread's own timeslice-expired switch) could ever
/// break the deadlock. `without_interrupts` around the whole critical
/// section makes it un-preemptible, so a syscall handler is guaranteed
/// to only ever observe `DISK_LOCK` either fully free or held by code
/// that's about to release it and return control to the timer — never
/// held by a thread that isn't currently running. Reproduced in practice,
/// not theoretical: booting with enough concurrent ELF spawns (each
/// doing its own file I/O) made this hang on essentially every attempt
/// before this fix.
static DISK_LOCK: Mutex<()> = Mutex::new(());

/// The data disk built by `builder/src/main.rs` — see `DATA_DISK_SIZE`
/// there. We don't probe the real size; `embedded-sdmmc` only needs an
/// upper bound for its own sanity checks, and this only ever undersells
/// what's really on disk if someone changes one without the other
/// (harmless: just an artificially small apparent disk, not a crash).
const DATA_DISK_SIZE_BYTES: u64 = 64 * 1024 * 1024;

impl BlockDevice for AtaDrive {
    type Error = &'static str;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx, _reason: &str) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter_mut().enumerate() {
            self.read_sector(start_block_idx.0 + i as u32, &mut block.contents)?;
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter().enumerate() {
            self.write_sector(start_block_idx.0 + i as u32, &block.contents)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        Ok(BlockCount((DATA_DISK_SIZE_BYTES / SECTOR_SIZE as u64) as u32))
    }
}

/// No RTC in this kernel — every file gets the same fixed timestamp.
/// `embedded-sdmmc` requires *a* `TimeSource`, not a correct one.
struct NoTimeSource;

impl TimeSource for NoTimeSource {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

type Mgr = VolumeManager<AtaDrive, NoTimeSource>;

fn mount() -> Mgr {
    VolumeManager::new(AtaDrive::primary_slave(), NoTimeSource)
}

/// Splits a path like `"a/b/c.txt"` into its directory components
/// (`["a", "b"]`) and final filename (`"c.txt"`). Empty components
/// (leading/trailing/doubled `/`) are dropped, so `"/file.txt"` and
/// `"file.txt"` behave identically — both end up with no directory
/// components, i.e. root.
fn split_path(path: &str) -> (Vec<&str>, &str) {
    let mut parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let file = parts.pop().unwrap_or("");
    (parts, file)
}

/// Walks from the volume's root directory down through `dir_parts`
/// (each one an 8.3 directory name), opening each level in turn, and
/// returns the innermost directory. Every intermediate directory handle
/// opened along the way is closed before returning (`embedded-sdmmc` has
/// a fixed `MAX_DIRS` of simultaneously-open directories, so leaking one
/// per path component would eventually starve every other caller).
///
/// With `create_missing` set, a directory component that doesn't exist
/// yet is created via `make_dir_in_dir` and then opened — used by
/// `write` so the first write into a new subdirectory just works. Reads
/// never pass `create_missing`: a missing intermediate directory is
/// simply a failed read, never something to conjure into existence.
fn open_dir_path(mgr: &mut Mgr, raw_volume: RawVolume, dir_parts: &[&str], create_missing: bool) -> Result<RawDirectory, &'static str> {
    let mut current = mgr.open_root_dir(raw_volume).map_err(|_| "failed to open root directory")?;
    for part in dir_parts {
        let next = match mgr.open_dir(current, *part) {
            Ok(d) => d,
            Err(_) if create_missing => {
                match mgr.make_dir_in_dir(current, *part) {
                    Ok(()) | Err(FsError::DirAlreadyExists) => {}
                    Err(_) => {
                        let _ = mgr.close_dir(current);
                        return Err("failed to create directory");
                    }
                }
                match mgr.open_dir(current, *part) {
                    Ok(d) => d,
                    Err(_) => {
                        let _ = mgr.close_dir(current);
                        return Err("failed to open directory after create");
                    }
                }
            }
            Err(_) => {
                let _ = mgr.close_dir(current);
                return Err("directory not found");
            }
        };
        let _ = mgr.close_dir(current);
        current = next;
    }
    Ok(current)
}

/// Reads a whole file's contents by path (e.g. `"hello.txt"` or
/// `"sub/hello.txt"`). A path with no `/` operates directly in the root
/// directory, exactly as before; each `/`-separated component before the
/// last names a subdirectory to walk into. Missing intermediate
/// directories are a read failure, not something this creates.
pub fn read(path: &str) -> Result<Vec<u8>, &'static str> {
    without_interrupts(|| {
    let _guard = DISK_LOCK.lock();
    let mut mgr = mount();
    let raw_volume = mgr.open_raw_volume(VolumeIdx(0)).map_err(|_| "failed to open volume")?;
    let (dir_parts, filename) = split_path(path);
    if filename.is_empty() {
        let _ = mgr.close_volume(raw_volume);
        return Err("invalid path");
    }

    let dir = match open_dir_path(&mut mgr, raw_volume, &dir_parts, false) {
        Ok(d) => d,
        Err(e) => {
            let _ = mgr.close_volume(raw_volume);
            return Err(e);
        }
    };
    let file = match mgr.open_file_in_dir(dir, filename, Mode::ReadOnly) {
        Ok(f) => f,
        Err(_) => {
            let _ = mgr.close_dir(dir);
            let _ = mgr.close_volume(raw_volume);
            return Err("file not found");
        }
    };

    let mut out = Vec::new();
    let mut chunk = [0u8; SECTOR_SIZE];
    let read_result: Result<(), &'static str> = loop {
        match mgr.read(file, &mut chunk) {
            Ok(0) => break Ok(()),
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => break Err("disk read error"),
        }
    };

    let _ = mgr.close_file(file);
    let _ = mgr.close_dir(dir);
    let _ = mgr.close_volume(raw_volume);
    read_result.map(|()| out)
    })
}

/// Writes (creating or truncating) a whole file's contents by path,
/// e.g. `"hello.txt"` or `"sub/hello.txt"`. A path with no `/` operates
/// directly in the root directory, exactly as before. Any intermediate
/// directory component that doesn't exist yet is created on the fly
/// (see `open_dir_path`), so writing into a brand-new subdirectory just
/// works the first time.
pub fn write(path: &str, data: &[u8]) -> Result<(), &'static str> {
    without_interrupts(|| {
    let _guard = DISK_LOCK.lock();
    let mut mgr = mount();
    let raw_volume = mgr.open_raw_volume(VolumeIdx(0)).map_err(|_| "failed to open volume")?;
    let (dir_parts, filename) = split_path(path);
    if filename.is_empty() {
        let _ = mgr.close_volume(raw_volume);
        return Err("invalid path");
    }

    let dir = match open_dir_path(&mut mgr, raw_volume, &dir_parts, true) {
        Ok(d) => d,
        Err(e) => {
            let _ = mgr.close_volume(raw_volume);
            return Err(e);
        }
    };
    let file = match mgr.open_file_in_dir(dir, filename, Mode::ReadWriteCreateOrTruncate) {
        Ok(f) => f,
        Err(_) => {
            let _ = mgr.close_dir(dir);
            let _ = mgr.close_volume(raw_volume);
            return Err("failed to create file");
        }
    };

    let write_result = mgr.write(file, data).map_err(|_| "disk write error");
    let _ = mgr.flush_file(file);
    let _ = mgr.close_file(file);
    let _ = mgr.close_dir(dir);
    let _ = mgr.close_volume(raw_volume);
    write_result
    })
}

/// Lists entry names in `path` (a subdirectory, e.g. `"sub"`); an empty
/// string or `"/"` means the root directory. Mainly for a boot-time
/// sanity check that the filesystem actually mounted and is readable.
pub fn list_dir(path: &str) -> Result<Vec<String>, &'static str> {
    without_interrupts(|| {
    let _guard = DISK_LOCK.lock();
    let mut mgr = mount();
    let raw_volume = mgr.open_raw_volume(VolumeIdx(0)).map_err(|_| "failed to open volume")?;
    let dir_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let dir = match open_dir_path(&mut mgr, raw_volume, &dir_parts, false) {
        Ok(d) => d,
        Err(e) => {
            let _ = mgr.close_volume(raw_volume);
            return Err(e);
        }
    };

    let mut names = Vec::new();
    let iter_result = mgr
        .iterate_dir(dir, |entry| names.push(format!("{}", entry.name)))
        .map_err(|_| "directory read error");

    let _ = mgr.close_dir(dir);
    let _ = mgr.close_volume(raw_volume);
    iter_result.map(|()| names)
    })
}

/// Lists file names in the root directory — mainly for a boot-time sanity
/// check that the filesystem actually mounted and is readable.
pub fn list_root() -> Result<Vec<String>, &'static str> {
    list_dir("")
}

/// Metadata for a path: whether it exists, whether it's a directory, and
/// (for a file) its size in bytes. `size` is meaningless (left `0`) when
/// `is_dir` is true or `exists` is false.
pub struct Stat {
    pub exists: bool,
    pub is_dir: bool,
    pub size: u64,
}

/// Looks up `path` without opening it for read/write. Walks to its parent
/// directory exactly like `read`/`write` do, then tries `open_dir` on the
/// final component first, falling back to `open_file_in_dir` — trying
/// "is it a directory" before "is it a file" costs nothing extra (both are
/// just directory-entry lookups) and avoids ever opening a file only to
/// discover it was a directory. Neither succeeding means the path simply
/// doesn't exist, not an error.
pub fn stat(path: &str) -> Stat {
    let not_found = Stat { exists: false, is_dir: false, size: 0 };
    without_interrupts(|| {
    let _guard = DISK_LOCK.lock();
    let mut mgr = mount();
    let raw_volume = match mgr.open_raw_volume(VolumeIdx(0)) {
        Ok(v) => v,
        Err(_) => return not_found,
    };
    let (dir_parts, filename) = split_path(path);
    if filename.is_empty() {
        let _ = mgr.close_volume(raw_volume);
        return not_found;
    }

    let dir = match open_dir_path(&mut mgr, raw_volume, &dir_parts, false) {
        Ok(d) => d,
        Err(_) => {
            let _ = mgr.close_volume(raw_volume);
            return not_found;
        }
    };

    let result = match mgr.open_dir(dir, filename) {
        Ok(subdir) => {
            let _ = mgr.close_dir(subdir);
            Stat { exists: true, is_dir: true, size: 0 }
        }
        Err(_) => match mgr.open_file_in_dir(dir, filename, Mode::ReadOnly) {
            Ok(file) => {
                let size = mgr.file_length(file).unwrap_or(0) as u64;
                let _ = mgr.close_file(file);
                Stat { exists: true, is_dir: false, size }
            }
            Err(_) => not_found,
        },
    };

    let _ = mgr.close_dir(dir);
    let _ = mgr.close_volume(raw_volume);
    result
    })
}

/// Deletes the file at `path`. Fails if `path` names a directory (the
/// underlying `delete_file_in_dir` refuses that itself) or doesn't exist.
pub fn unlink(path: &str) -> Result<(), &'static str> {
    without_interrupts(|| {
    let _guard = DISK_LOCK.lock();
    let mut mgr = mount();
    let raw_volume = mgr.open_raw_volume(VolumeIdx(0)).map_err(|_| "failed to open volume")?;
    let (dir_parts, filename) = split_path(path);
    if filename.is_empty() {
        let _ = mgr.close_volume(raw_volume);
        return Err("invalid path");
    }

    let dir = match open_dir_path(&mut mgr, raw_volume, &dir_parts, false) {
        Ok(d) => d,
        Err(e) => {
            let _ = mgr.close_volume(raw_volume);
            return Err(e);
        }
    };

    let delete_result = mgr.delete_file_in_dir(dir, filename).map_err(|_| "failed to delete file");

    let _ = mgr.close_dir(dir);
    let _ = mgr.close_volume(raw_volume);
    delete_result
    })
}

/// Renames/moves `old_path` to `new_path`. `embedded-sdmmc`'s
/// `VolumeManager` (checked against its actual 0.8.2 source: no method
/// named `rename_file` or anything like it exists anywhere in the crate)
/// has no native rename, so this falls back to read-old + write-new +
/// delete-old, reusing this module's own `read`/`write`/`unlink`. That
/// fallback is **not atomic**: it takes and releases `DISK_LOCK` three
/// separate times (once inside each of `read`, `write`, `unlink`), so a
/// crash or power loss between any two of those steps leaves the
/// filesystem in whatever state that step reached — most plausibly, a
/// crash between the write and the delete leaves *both* `old_path` and
/// `new_path` present with identical contents, never neither. Callers
/// that need real move-semantics guarantees should not rely on this
/// being atomic.
pub fn rename(old_path: &str, new_path: &str) -> Result<(), &'static str> {
    let data = read(old_path)?;
    write(new_path, &data)?;
    unlink(old_path)
}

/// Creates the directory at `path` explicitly, as an empty directory with
/// nothing written into it — unlike `write`, which only ever creates
/// directories implicitly as intermediate components of a file path. This
/// treats every `/`-separated component of `path` (including the last) as
/// a directory name and reuses `open_dir_path`'s `create_missing` handling
/// for all of them, then just closes the resulting handle.
pub fn mkdir(path: &str) -> Result<(), &'static str> {
    without_interrupts(|| {
    let _guard = DISK_LOCK.lock();
    let mut mgr = mount();
    let raw_volume = mgr.open_raw_volume(VolumeIdx(0)).map_err(|_| "failed to open volume")?;
    let dir_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if dir_parts.is_empty() {
        let _ = mgr.close_volume(raw_volume);
        return Err("invalid path");
    }

    let result = open_dir_path(&mut mgr, raw_volume, &dir_parts, true);
    match result {
        Ok(dir) => {
            let _ = mgr.close_dir(dir);
            let _ = mgr.close_volume(raw_volume);
            Ok(())
        }
        Err(e) => {
            let _ = mgr.close_volume(raw_volume);
            Err(e)
        }
    }
    })
}

/// Self-contained round-trip check for directory support: creates a
/// subdirectory, writes a known byte string into a file inside it, reads
/// it back, and confirms the bytes match exactly. Exists so the boot
/// sequence can prove subdirectory read/write actually works on real
/// hardware, not just that it compiles — call this from `main.rs`.
pub fn verify_directories() -> Result<(), &'static str> {
    const DIR_NAME: &str = "SUBDIR";
    const FILE_PATH: &str = "SUBDIR/NEST.TXT";
    const CONTENTS: &[u8] = b"nested file contents";

    write(FILE_PATH, CONTENTS)?;
    let read_back = read(FILE_PATH)?;

    if read_back != CONTENTS {
        return Err("verify_directories: round-tripped bytes did not match what was written");
    }

    // Sanity-check that the directory actually shows up as an entry of
    // its parent, not just that the file inside it round-tripped.
    let root_entries = list_dir("")?;
    if !root_entries.iter().any(|name| name.eq_ignore_ascii_case(DIR_NAME)) {
        return Err("verify_directories: subdirectory not visible in root listing");
    }

    Ok(())
}
