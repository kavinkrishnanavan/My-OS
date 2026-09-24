# MyOS

A bare-metal x86_64 kernel that boots on real hardware or QEMU, brings up
its own NIC driver and TCP/IP stack, fetches a web page, and draws it on
screen — no host OS involved.

## Architecture

```
bootloader (BIOS/UEFI, via the `bootloader` crate)
  -> _start / kernel_main
  -> paging + heap + GDT + IDT + PIT setup
  -> PCI scan -> RTL8139 driver (interrupt-driven RX/TX)
  -> smoltcp (DHCP -> DNS -> TCP)
  -> TLS 1.3 (embedded-tls, for https://) -> async executor running the fetch task
  -> HTML-to-text extraction -> bitmap-font renderer -> framebuffer
```

The design rule threaded through all of it: **nothing ever busy-waits for
the network.** The CPU executes `hlt` whenever there's no ready task, and
only wakes back up on a real interrupt — a packet arriving on the NIC, or
the periodic PIT timer tick that drives smoltcp's own retransmit/lease
timers. Interrupt handlers do the minimum (ack the hardware, wake a
`Waker`) and return immediately; all the actual packet/socket handling
happens afterward, cooperatively, in the async executor. See
`kernel/src/task/executor.rs` and `kernel/src/net/stack.rs` for where
that's implemented.

| Layer | File |
|---|---|
| Serial console (COM1) | `kernel/src/serial.rs` |
| Heap / physical memory | `kernel/src/allocator.rs`, `kernel/src/memory.rs` |
| GDT / TSS | `kernel/src/gdt.rs` |
| IDT + PIC (naked-fn interrupt trampolines) | `kernel/src/interrupts.rs` |
| PIT timer | `kernel/src/task/time.rs` |
| PCI bus scan | `kernel/src/pci.rs` |
| RTL8139 NIC driver | `kernel/src/net/rtl8139.rs` |
| smoltcp device glue | `kernel/src/net/mod.rs` |
| DHCP/DNS/TCP stack wiring | `kernel/src/net/stack.rs` |
| Async executor | `kernel/src/task/executor.rs` |
| URL parsing + HTTP GET + chunked-transfer decode + page render | `kernel/src/net/http.rs` |
| TCP socket as `embedded_io_async::{Read, Write}` | `kernel/src/net/tcp_stream.rs` |
| TLS 1.3 client (`https://`) | `kernel/src/net/tls.rs` |
| RDRAND-backed RNG for the TLS handshake | `kernel/src/rng.rs` |
| FPU/SSE enable (`CR0`/`CR4`) for `fxsave`/`fxrstor` | `kernel/src/fpu.rs` |
| HTML-to-text extraction | `kernel/src/html.rs` |
| Framebuffer / bitmap-font drawing | `kernel/src/gfx.rs` |

`kernel/` is a `#![no_std]` binary built for the bare `x86_64-unknown-none`
target on **stable** Rust — the interrupt handlers are hand-written
naked-function trampolines specifically to avoid needing the nightly-only
`extern "x86-interrupt"` ABI. `builder/` is an ordinary host binary that
uses the `bootloader` crate to stitch the compiled kernel into bootable
BIOS/UEFI disk images; *that* step needs a nightly toolchain (the
bootloader's own boot-stage binaries require `-Z build-std`), which is
why the run instructions below use `rustup run nightly` for it.

## Build & run

```sh
# one-time setup
rustup toolchain install nightly --component llvm-tools
rustup target add x86_64-unknown-none          # stable toolchain
rustup target add --toolchain nightly x86_64-unknown-none

# build the kernel (stable)
cargo build -p myos-kernel --release

# build the userland programs (kernel/src/elf.rs loads them at boot —
# see main.rs's spawn_userland_demos) — same target, each its own
# directory since each needs its own linker script (link.ld). All four
# depend on userland/libmyos (shared syscall wrappers/allocator), which
# cargo builds automatically as part of each — no separate step for it.
# spawner also proves SYS_SPAWN/SYS_WAIT/SYS_GETARG: it calls
# myos_userlib::spawn_with_arg("hello.elf", "hi from spawner") and then
# wait() on the result, so a real ring-3 program — not just main.rs at
# boot — launches another one with an argument, blocks until it exits,
# and gets back the exit status hello passed to exit(); hello prints its
# own arg_string() either way, so boot-spawned (empty arg) and
# spawner-spawned (real arg) instances are distinguishable in the serial
# log. spawner also proves SYS_KILL: after the hello/wait round trip, it
# spawns counter.elf, kills it almost immediately, and waits on it again
# — wait() unblocks with the u64::MAX "killed" sentinel instead of
# hanging forever waiting for an exit the victim will now never make
# itself. It finishes by proving SYS_UPTIME_MS/sleep_ms (sleeping 500ms
# and printing the real PIT-clock elapsed time around the call) and
# SYS_MEMINFO (printing the kernel's live heap/frame counters — built by
# three parallel agents at once: kernel/src/fs.rs's directory support,
# kernel/src/{interrupts,allocator,memory}.rs's SYS_MEMINFO, and this
# userland wrapper/demo, on a fixed ABI spec so none of them needed to
# wait on each other or touch a shared file). httpget
# proves SYS_CONNECT/SYS_CONNECT_STATUS and
# SYS_RESOLVE/SYS_RESOLVE_STATUS: it takes "host [path]" as its spawn
# argument (example.com / by default), resolves host via a real DNS
# query if it isn't already a dotted-quad IP, opens its own TCP socket
# fd, and does a real HTTP GET, printing the response — the same TCP/IP
# stack the kernel's own boot-time HTTPS fetch uses, now reachable from
# ring 3 instead of only from kernel code. pipedemo proves SYS_PIPE: a
# real Unix-style in-kernel pipe (kernel/src/pipe.rs — a refcounted ring
# buffer, non-blocking read/write since a syscall handler can never park
# itself) — it opens a pipe, writes a message into the write end, closes
# it, and reads the bytes back out the read end, round-tripping through
# the kernel's own buffer rather than anything the two ends share
# directly. Built by two parallel agents at once (kernel/src/pipe.rs;
# the userland wrapper + this demo crate), same fixed-ABI-spec pattern as
# SYS_MEMINFO above — integration (interrupts.rs dispatch, thread.rs fd
# wiring) done by hand afterward since that's the one shared-file surface
# multiple agents can't safely touch at once in a repo with no git
# merge. Also flushed out a real, pre-existing latent bug while
# integrating: net::rtl8139::init()'s alloc_dma_region() needs physically
# *contiguous* frames, but the frame allocator was serving frames out of
# its freed-list (page-table frames reclaimed from a just-killed process)
# ahead of its bump cursor — spawner.elf killing counter.elf during boot
# could populate that list right as NIC init ran, handing back an
# unrelated recycled address mid-loop and breaking contiguity
# deterministically once a fourth boot-time ELF spawn (this demo) shifted
# the timing enough to hit it every run. Fixed in kernel/src/allocator.rs
# with a bump-cursor-only allocation path reserved for DMA.
#
# keydemo/rtcdemo prove SYS_READ_KEY and SYS_RTC_NOW: kernel/src/keyboard.rs
# is a real IRQ1-driven PS/2 driver (Scancode Set 1 -> ASCII into a
# non-blocking ring buffer — unlike every syscall added before it, this one
# needed a real IDT/PIC interrupt handler, not just a dispatch entry).
# kernel/src/rtc.rs reads the CMOS real-time clock (handling the
# update-in-progress race, BCD-vs-binary and 12h/24h modes) — rtcdemo's
# boot-time output has matched the real host date/time. Built by four
# parallel agents across two independent features at once (keyboard
# driver, RTC driver, the libmyos wrappers, both demo crates), same
# fixed-ABI-spec-then-hand-integrate pattern as SYS_PIPE above.
(cd userland/hello && cargo build --release)
(cd userland/counter && cargo build --release)
(cd userland/spawner && cargo build --release)
(cd userland/httpget && cargo build --release)
(cd userland/pipedemo && cargo build --release)
(cd userland/keydemo && cargo build --release)
(cd userland/rtcdemo && cargo build --release)

# package it into bootable images (nightly, for the bootloader crate's own build-std step)
# — this also creates dist/myos-data.img on first run: an MBR + FAT32
# partition (kernel/src/fs.rs mounts it via the ATA driver in
# kernel/src/disk/ata.rs), seeded with a hello.txt and the four userland
# binaries above (as hello.elf/counter.elf/spawner.elf/httpget.elf) the
# kernel reads back at boot.
rustup run nightly cargo run -p builder --release

# boot it — two drives: the boot image, then the data disk
qemu-system-x86_64 \
  -drive format=raw,file=dist/myos-bios.img \
  -drive format=raw,file=dist/myos-data.img \
  -netdev user,id=n0 -device rtl8139,netdev=n0 \
  -serial stdio
```

(On this machine, QEMU lives at `C:\msys64\mingw64\bin\qemu-system-x86_64.exe`
rather than being on `PATH`.)

You should see boot/driver/DHCP/DNS log lines over serial, and the
fetched page (defaults to `https://example.com/` — see the
`net::http::fetch(...)` call in `kernel/src/main.rs`; edit that string to
change the URL, `http://` or `https://` both work) drawn on the QEMU
display window. Running headless (`-display none`), you can still grab
what was drawn via the QEMU monitor's `screendump` command (e.g. over
`-monitor telnet:127.0.0.1:5555,server,nowait`), which is how the
screenshot below was taken — a `.ppm` file, convertible with
`ffmpeg -i shot.ppm shot.png` or similar.

## Status

Boot-tested in QEMU (`qemu-system-x86_64`, BIOS image, `-netdev user`
with an emulated RTL8139) end to end: PCI scan finds the NIC, DHCP gets a
lease from QEMU's SLIRP gateway, DNS resolves a real hostname, a TCP
connection is opened and an HTTP/1.1 GET actually round-trips to the
real internet (through QEMU's NAT) and the response prints over serial.

Three real bugs turned up during that first boot and are fixed in the
current code — worth knowing about if you're reading this as a naked-
trampoline / no_std-async reference:

- **Stale `SS` selector after loading a new GDT** (`gdt.rs`): only `CS`
  was reloaded after `lgdt`; `SS` kept the bootloader's old selector
  value, which now indexed into an unrelated (system-type) descriptor in
  *our* GDT. `iretq` validates `SS` on every return from an interrupt,
  so the very first timer tick double-faulted. Fix: load `SS`/`DS`/`ES`
  with a real data-segment selector (and null `FS`/`GS`) right after
  `lgdt`.
- **Self-deadlock in `unmask_irq`** (`interrupts.rs`): it held the
  `PICS` spinlock while interrupts were enabled; the IRQ it had just
  unmasked could fire before the function returned, and that ISR's own
  EOI tried to re-lock the same mutex — a single-CPU deadlock with
  interrupts hardware-disabled for the ISR's duration, so nothing could
  ever break it. Fixed by wrapping the critical section in
  `without_interrupts`.
- **Single-waiter `AtomicWaker` shared by two tasks** (`net/stack.rs`):
  both `net_poll_loop` and the HTTP fetch task awaited the same
  `net_tick()`, but `AtomicWaker` only remembers the *most recent*
  registration — the first task's waker was silently dropped, so it
  never got resumed and `stack::poll()` stopped being called after the
  first round. DHCP happened to complete before this bit; DNS resolution
  hung forever afterward. Fixed with a small multi-waiter `WakerSet`.
- Also corrected the EOI logic for the NIC's IRQ (9-12, all on the
  *secondary* 8259): the handler was always EOI'ing with a vector on the
  primary PIC, which never actually notified the secondary — harmless in
  this QEMU config apparently, but wrong per spec and worth fixing before
  it bites on real hardware.

Adding TLS surfaced three more, all in `interrupts.rs`'s naked-function
trampolines, none of which showed up until real crypto code (with its
XMM usage and deeper call stacks) started running:

- **No FPU/SSE state saved across interrupts.** x86_64's baseline ABI
  always has SSE2, so the compiler is free to use `xmm` registers in
  ordinary code — and AES-GCM/P-256/SHA-2 (all pulled in by TLS) do. The
  trampolines only saved GPRs; an interrupt landing mid-crypto-op could
  silently corrupt whatever was in `xmm0-15`. Fixed by adding
  `fxsave`/`fxrstor` around the handler call — which then surfaced two
  more bugs getting *that* right:
  - `fxsave`/`fxrstor` need `CR4.OSFXSR` set, which nothing had done.
    Added `fpu::init()`, run before interrupts are ever enabled.
  - `fxsave`/`fxrstor` also need a 16-byte-aligned address, and hand
    -deriving the right amount of padding from "the CPU aligns RSP
    before pushing the interrupt frame" turned out to not match what
    was observed at runtime. Replaced the fragile arithmetic with an
    unconditionally-correct sequence: stash the pre-alignment `rsp`,
    force-align with `and rsp, -16`, and jump straight back to the
    stashed value afterward — correct regardless of what alignment the
    interrupted code actually had.
- **Stashed that pointer in a caller-saved register (`rax`).** The
  Rust handler being called is free to clobber `rax` — it's volatile in
  the SysV ABI — which destroyed the saved restore pointer and sent
  `iretq` off to a near-null `rsp` on return. Moved it to `rbx`
  (callee-saved; the ABI guarantees the handler preserves it).

Plus one non-interrupt bug: **`TlsConnection::write()` only buffers**
— it doesn't send anything until `flush()` is called, since several
small writes can be coalesced into one TLS record. The first attempt
called `write()` and went straight to reading a response that was never
going to arrive, because the request had never actually left the
buffer. Fixed by calling `.flush().await` after the request write.

## Security notes

- **No certificate validation.** `net/tls.rs` uses `embedded-tls`'s
  `UnsecureProvider` — the crate's own docs note that real certificate
  verification (`webpki`) only works with `std` today, so this is the
  only option available to a `no_std` kernel with this crate. The
  session is encrypted (passive eavesdropping sees only ciphertext) but
  **not authenticated** — an active attacker on the network path can
  MITM it undetected. Treat it like `curl -k`, not a browser padlock.
- **TLS handshake randomness depends on the CPU.** `rng.rs` prefers
  `RDRAND` (a real hardware entropy source); QEMU's default CPU model
  doesn't expose it, so under plain `qemu-system-x86_64` you'll see
  `tls: WARNING — no RDRAND on this CPU` and the handshake falls back to
  a non-cryptographic xorshift PRNG. Boot with `-cpu host` (or real
  hardware from the last ~decade) to get real hardware randomness
  instead — the code logs which path was used on every connection so
  this is never silent.

## Rendering a page

`net::http::fetch` now does more than log the response: it splits the
raw HTTP response into headers/body, decodes `Transfer-Encoding: chunked`
if present (`http.rs::dechunk`), runs the body through a minimal
HTML-to-text extractor (`html.rs` — no DOM, no CSS, just enough tag
tracking to insert line breaks at block elements and decode common
entities), and flows the resulting text onto the bootloader's linear
framebuffer with a real anti-aliased bitmap font
(`noto-sans-mono-bitmap`, alpha-blended per pixel in `gfx.rs`). Headings
render larger; there's no layout beyond top-to-bottom flowed text yet
(no scrolling either — a page longer than one screen just gets cut off).

## Roadmap — and what's *not* happening

The natural next steps toward "more of the web renders":

1. Real certificate validation (a root CA trust store + X.509 chain
   verification) — closes the MITM gap noted above. `embedded-tls`'s
   `webpki` feature does this but currently needs `std`.
2. Clickable links (needs keyboard/mouse input — PS/2 driver — plus
   tracking each block's source `<a href>`).
3. Basic `<img>` support for simple formats (BMP is nearly trivial;
   PNG needs an inflate/DEFLATE decoder).
4. Multiple open pages / a simple window manager, if this grows beyond
   "one page fills the screen."

**YouTube (or any video-streaming site) is out of scope, not just
unbuilt.** Playing a YouTube video needs, at minimum: HTTPS, a
JavaScript engine (the page is unusable without it), DASH/HLS manifest
parsing, and a hardware or software H.264/VP9 + AAC decoder — each one
individually comparable in size to this entire kernel, and typically
gated behind Widevine DRM besides. That's "build a real browser engine
and a media pipeline," not an incremental step from here.

The `boot/boot.asm` + `boot.bin` files in this directory are leftovers
from an earlier hand-rolled real-mode bootloader prototype that never
actually loaded a kernel; they're unused now that `builder/` produces
real bootable images, and can be deleted.
