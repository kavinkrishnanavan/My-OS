//! MyOS: a bare-metal x86_64 kernel that boots, brings up its own NIC
//! driver and TCP/IP stack, and fetches a web page — no host OS involved.
//!
//! Architecture, end to end:
//!   `bootloader` (BIOS/UEFI) -> `_start` -> paging/heap/GDT/IDT setup
//!   -> RTL8139 driver (interrupt-driven) -> smoltcp (DHCP/DNS/TCP)
//!   -> async executor running the HTTP fetch task
//!
//! Nothing after `executor.run()` ever busy-waits for the network: the
//! CPU halts (`hlt`) whenever there's no ready task and only wakes back
//! up on an actual interrupt (packet arrived, timer tick). See
//! `task/executor.rs` and `net/stack.rs` for where that's implemented.

#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod css;
mod disk;
mod dom;
mod elf;
mod fpu;
mod fs;
mod gdt;
mod gfx;
mod img;
mod interrupts;
mod keyboard;
mod layout;
mod memory;
mod mmap;
mod mouse;
mod net;
mod pci;
mod pipe;
mod rng;
mod rtc;
mod serial;
mod task;
mod tsc;
mod unixsocket;

use bootloader_api::config::{BootloaderConfig, Mapping};
use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;
use task::{executor::Executor, Task};

/// Requests the bootloader map *all* physical memory at a known offset
/// (`BootInfo::physical_memory_offset`) instead of just our own kernel
/// image. The RTL8139 driver depends on this: it hands the NIC physical
/// DMA addresses and reads/writes those same buffers back through
/// `offset + phys`, with no per-buffer page-table work — see memory.rs.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    unsafe { serial::init() };
    serial_println!("MyOS booting...");

    let physical_memory_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("bootloader must map physical memory (see builder/src/main.rs BootConfig)");

    let mut mapper = unsafe { memory::init(physical_memory_offset) };
    let mut frame_allocator = unsafe { allocator::BootInfoFrameAllocator::init(&boot_info.memory_regions) };
    allocator::init_heap(&mut mapper, &mut frame_allocator).expect("heap init failed");
    memory::set_frame_allocator(frame_allocator);
    memory::set_mapper(mapper); // kept around for later on-demand mappings — see task::thread's ring-3 support
    serial_println!("memory: heap online");

    gdt::init();
    fpu::init(); // before interrupts::init(): the first tick's trampoline already uses fxsave
    interrupts::init();
    task::time::init();
    task::thread::init();
    tsc::init();
    serial_println!("cpu: GDT/IDT/PIT/FPU online, interrupts enabled");

    // Unlike the keyboard (always unmasked unconditionally — see
    // interrupts.rs), a PS/2 mouse needs its own enable handshake before
    // it sends anything, and might not even be present (some QEMU
    // configs, some real hardware) — only unmask IRQ12 if that handshake
    // actually succeeded, mirroring how the NIC driver owns unmasking
    // its own (PCI-scanned) line.
    if mouse::init() {
        interrupts::unmask_irq(12);
        serial_println!("mouse: PS/2 mouse online");
    } else {
        serial_println!("mouse: no PS/2 mouse detected");
    }

    // Temporary: prove preemption is real (not just cooperative
    // yielding) by spawning kernel threads that print on their own,
    // uncoordinated schedule and watching their output interleave.
    spawn_preemption_demo_threads();
    // Temporary: prove ring-3 execution + the syscall gate back into the
    // kernel actually work (Milestone 2) — see its own doc comment.
    spawn_usermode_demo();
    // Temporary: prove isolated address spaces actually isolate
    // (Milestone 3) — see its own doc comment.
    spawn_isolation_demo();

    match boot_info.framebuffer.take() {
        Some(fb) => {
            let info = fb.info();
            *gfx::SCREEN.lock() = Some(gfx::Framebuffer::new(fb.into_buffer(), info));
            serial_println!(
                "gfx: framebuffer {}x{} ({:?}, {} bytes/px)",
                info.width,
                info.height,
                info.pixel_format,
                info.bytes_per_pixel
            );
        }
        None => serial_println!("gfx: no framebuffer from bootloader — page rendering disabled"),
    }

    // Temporary: exercise the new disk+filesystem stack end to end
    // before anything else touches it — mount, read the file the
    // builder seeded on the host, then write+read one back to confirm
    // both directions actually round-trip through real disk I/O.
    verify_filesystem();
    // Temporary: prove the ELF loader works, with two genuinely
    // different real programs running concurrently — see its own doc
    // comment on spawn_userland_demos.
    spawn_userland_demos();

    interrupts::log_pci_bus();

    match net::rtl8139::init() {
        Some(_mac) => serial_println!("net: rtl8139 driver online"),
        None => {
            serial_println!("net: no RTL8139 found on the PCI bus — is QEMU running with -device rtl8139,netdev=n0 -netdev user,id=n0 ?");
            halt_forever();
        }
    }

    net::stack::init();

    let mut executor = Executor::new();
    executor.spawn(Task::new(net_poll_loop()));
    executor.spawn(Task::new(spawn_userland_network_demo_when_online()));
    executor.spawn(Task::new(net::http::fetch("https://en.wikipedia.org/wiki/PNG")));
    executor.run();
}

/// Temporary verification for `task::thread`: three kernel threads, each
/// just busy-spinning a counter and printing its own tag — no shared
/// state, no cooperation. If the scheduler's preemption is real,
/// their serial output interleaves; if something's actually still
/// cooperative (or broken), one tag would run to completion (it never
/// does — these loop forever) before another appears at all.
fn spawn_preemption_demo_threads() {
    extern "C" fn thread_a() -> ! {
        preemption_demo_loop("thread-a")
    }
    extern "C" fn thread_b() -> ! {
        preemption_demo_loop("thread-b")
    }
    extern "C" fn thread_c() -> ! {
        preemption_demo_loop("thread-c")
    }
    fn preemption_demo_loop(tag: &str) -> ! {
        let mut n: u64 = 0;
        loop {
            serial_println!("{tag}: {n}");
            n += 1;
            for _ in 0..3_000_000 {
                core::hint::spin_loop();
            }
        }
    }

    task::thread::spawn(thread_a);
    task::thread::spawn(thread_b);
    task::thread::spawn(thread_c);
}

/// Temporary verification for ring-3 execution (`task::thread`'s
/// Milestone 2 support): a tiny, hand-verified position-independent
/// payload — only short relative jumps and immediate loads, no absolute
/// addressing, so it stays correct once copied to a different address
/// than where the compiler actually placed it — that runs in ring 3 and
/// can only affect the world through the `int 0x80` syscall gate
/// (`interrupts.rs`). Its only "output" is a steady stream of `U`
/// characters over serial: proof ring-3 code ran at all, and that it
/// only got the kernel to do anything on its behalf via the syscall the
/// kernel explicitly handles for it, never by calling kernel code
/// directly (which ring-3 code has no access to — it can't even see
/// where the kernel's own code lives).
fn spawn_usermode_demo() {
    #[unsafe(naked)]
    extern "C" fn user_payload() -> ! {
        core::arch::naked_asm!(
            "2:",
            "mov dil, 85", // 'U'
            "mov rax, 1",  // SYS_WRITE_BYTE — see interrupts.rs's syscall_handler
            "int 0x80",
            "mov rcx, 3000000", // just a pacing delay, no meaning beyond that
            "3:",
            "dec rcx",
            "jnz 3b",
            "jmp 2b",
        )
    }

    let payload = unsafe {
        core::slice::from_raw_parts(user_payload as *const () as *const u8, task::thread::USER_PAYLOAD_COPY_LEN)
    };
    task::thread::spawn_user(payload).expect("failed to spawn the ring-3 demo thread");
}

/// Temporary verification for isolated address spaces (`task::thread`'s
/// Milestone 3 support): two ring-3 threads, each in its *own* address
/// space via `spawn_isolated_user`, both using the exact same virtual
/// addresses for their code/stack (`task::thread::USER_CODE_ADDR`/
/// `USER_STACK_ADDR` — not configurable per call, deliberately, to make
/// this exact collision-or-not test possible) but running different code
/// there. If they were accidentally sharing a table, one payload's bytes
/// would overwrite the other's at that shared address and only one
/// letter (or a fault) would ever appear; real isolation means both `A`s
/// and `B`s keep showing up, indefinitely, from what's actually different
/// physical memory underneath the same-looking virtual address.
fn spawn_isolation_demo() {
    #[unsafe(naked)]
    extern "C" fn payload_a() -> ! {
        core::arch::naked_asm!(
            "2:",
            "mov dil, 65", // 'A'
            "mov rax, 1",
            "int 0x80",
            "mov rcx, 3000000",
            "3:",
            "dec rcx",
            "jnz 3b",
            "jmp 2b",
        )
    }
    #[unsafe(naked)]
    extern "C" fn payload_b() -> ! {
        core::arch::naked_asm!(
            "2:",
            "mov dil, 66", // 'B'
            "mov rax, 1",
            "int 0x80",
            "mov rcx, 3000000",
            "3:",
            "dec rcx",
            "jnz 3b",
            "jmp 2b",
        )
    }

    for entry in [payload_a as *const (), payload_b as *const ()] {
        let payload = unsafe { core::slice::from_raw_parts(entry as *const u8, task::thread::USER_PAYLOAD_COPY_LEN) };
        task::thread::spawn_isolated_user(payload).expect("failed to spawn an isolated ring-3 demo thread");
    }
}

/// Temporary verification for the ELF loader (`elf::load`, via
/// `task::thread::spawn_elf`): reads `HELLO.ELF` and `COUNTER.ELF` — two
/// real, *different* separately-compiled programs (`userland/hello`,
/// `userland/counter`, embedded onto the data disk by
/// `builder/src/main.rs`), not bytes this kernel wrote itself — off the
/// filesystem `fs.rs` (Milestone 1) already proved works, and runs both
/// concurrently, each in its own address space (Milestone 3). Two
/// distinct real programs at once, not two copies of one hand-written
/// demo (Milestone 3's own A/B test), is the point: `hello`'s and
/// `counter`'s own syscall-driven output interleaving in the serial log
/// is proof the kernel hosts genuinely different workloads, not just
/// that ELF loading works once.
fn spawn_userland_demos() {
    // "SPAWNER.ELF" (userland/spawner) itself calls SYS_SPAWN on
    // "hello.elf" once it runs — proof a *ring-3* program can launch
    // another one, not just this function deciding everything at boot.
    // That makes two independent `hello` instances running concurrently
    // by the time both have started: this one, spawned here the normal
    // way, and whichever one spawner's own SYS_SPAWN call produces.
    for name in [
        "HELLO.ELF",
        "COUNTER.ELF",
        "SPAWNER.ELF",
        "PIPEDEMO.ELF",
        "KEYDEMO.ELF",
        "RTCDEMO.ELF",
        "TSCDEMO.ELF",
        "FSDEMO.ELF",
        "MMAPDEMO.ELF",
        "UNIXDEMO.ELF",
    ] {
        match fs::read(name) {
            Ok(bytes) => {
                if let Err(e) = task::thread::spawn_elf(&bytes, &[]) {
                    serial_println!("elf: failed to spawn {name}: {e}");
                }
            }
            Err(e) => serial_println!("elf: failed to read {name}: {e}"),
        }
    }
}

/// Starts the first userland network client once DHCP has actually
/// completed. This is deliberately spawned from the async executor
/// instead of `spawn_userland_demos`: the early boot demos run before
/// the NIC/DHCP stack exists, while `HTTPGET.ELF` needs the OS socket
/// syscalls to have a live interface behind them.
async fn spawn_userland_network_demo_when_online() {
    while !net::stack::has_ip() {
        net::stack::net_tick().await;
    }

    match fs::read("HTTPGET.ELF") {
        Ok(bytes) => {
            // A hostname, not a hardcoded IP (see userland/httpget's own
            // doc comment: a hardcoded example.com address used here
            // before went dead once IANA re-delegated it, hanging this
            // demo forever with no way to tell "still connecting" apart
            // from "target unreachable" — SYS_RESOLVE plus
            // connect_blocking's retry cap fixes both problems at once).
            if let Err(e) = task::thread::spawn_elf(&bytes, b"example.com /") {
                serial_println!("elf: failed to spawn HTTPGET.ELF: {e}");
            }
        }
        Err(e) => serial_println!("elf: failed to read HTTPGET.ELF: {e}"),
    }
}

fn verify_filesystem() {
    match fs::list_root() {
        Ok(names) => serial_println!("fs: root directory: {:?}", names),
        Err(e) => {
            serial_println!("fs: failed to list root directory: {e}");
            return;
        }
    }

    match fs::read("hello.txt") {
        Ok(bytes) => serial_println!("fs: hello.txt: {:?}", core::str::from_utf8(&bytes).unwrap_or("<not utf8>")),
        Err(e) => serial_println!("fs: failed to read hello.txt: {e}"),
    }

    // embedded-sdmmc only supports classic 8.3 short filenames (no VFAT
    // long-filename support), hence "kernel.txt" rather than something
    // longer that would trip `FilenameError(NameTooLong)`.
    match fs::write("kernel.txt", b"written by the kernel itself\n") {
        Ok(()) => serial_println!("fs: wrote kernel.txt"),
        Err(e) => serial_println!("fs: failed to write kernel.txt: {e}"),
    }

    match fs::read("kernel.txt") {
        Ok(bytes) => serial_println!(
            "fs: read back kernel.txt: {:?}",
            core::str::from_utf8(&bytes).unwrap_or("<not utf8>")
        ),
        Err(e) => serial_println!("fs: failed to read back kernel.txt: {e}"),
    }

    match fs::verify_directories() {
        Ok(()) => serial_println!("fs: directory support verified"),
        Err(e) => serial_println!("fs: directory verification failed: {e}"),
    }
}

/// Drives the smoltcp interface forward. Never spins: it awaits
/// `net_tick()` between iterations, which only resumes it when an
/// interrupt (NIC RX or PIT) actually gives it a reason to re-check.
async fn net_poll_loop() {
    loop {
        net::stack::poll();
        net::stack::net_tick().await;
    }
}

fn halt_forever() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("KERNEL PANIC: {}", info);
    halt_forever()
}
