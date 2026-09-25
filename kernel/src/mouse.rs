//! PS/2 mouse driver: drives the i8042 controller's auxiliary port through
//! its enable/reset sequence, then decodes the 3-byte (or 4-byte,
//! IntelliMouse wheel) packets it streams one byte per IRQ12 into
//! high-level `MouseEvent`s a syscall handler can drain later.
//!
//! Shares both i8042 I/O ports with `keyboard.rs` (data at `0x60`,
//! command/status at `0x64`) — the controller multiplexes the keyboard
//! and the AUX (mouse) device onto the same pair of ports and tells them
//! apart by which IRQ line and which internal buffer the byte came from,
//! not by anything encoded in the byte itself. `init()` is responsible
//! for the one-time controller-side setup (AUX enable, IRQ12 unmask) and
//! the device-side setup (defaults, enable reporting, optional wheel
//! detection); `on_irq` only ever does the second half of that, one data
//! byte at a time.
//!
//! Same hard constraint as `keyboard.rs`: `on_irq` runs with interrupts
//! off, so it may never block or spin waiting for more bytes — it just
//! buffers whatever single byte arrived and, once a full packet has
//! accumulated, decodes it and returns. `init()` runs once at boot,
//! outside any interrupt gate, so its bounded polling waits (below) are
//! fine.

use spin::Mutex;
use x86_64::instructions::port::Port;

const DATA_PORT: u16 = 0x60;
const COMMAND_STATUS_PORT: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;

const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_WRITE_TO_AUX: u8 = 0xD4;

const CONFIG_AUX_IRQ_ENABLE: u8 = 1 << 1;

const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_REPORTING: u8 = 0xF4;
const MOUSE_SET_SAMPLE_RATE: u8 = 0xF3;
const MOUSE_GET_DEVICE_ID: u8 = 0xF2;
const MOUSE_ACK: u8 = 0xFA;

/// Upper bound on how many times we re-read the controller's status port
/// (or the data port, waiting on a specific reply byte) before giving up
/// on a single step of the init sequence. Mirrors `disk/ata.rs`'s
/// `POLL_LIMIT`: every iteration here is a real port I/O round trip
/// (a VM exit under QEMU, an actual bus cycle on real hardware), so this
/// is deliberately generous rather than tuned tight — but it must exist
/// at all, because a machine with no PS/2 mouse wired up (or a QEMU
/// invocation with the mouse device left out) will otherwise never send
/// the byte we're waiting for, and an unbounded `loop {}` here would
/// hang boot forever instead of just booting without a mouse.
const POLL_LIMIT: u32 = 100_000;

const EVENT_BUFFER_CAPACITY: usize = 64;

/// One decoded mouse event, ready for a caller (e.g. the syscall layer
/// or the page renderer's event loop) to act on.
///
/// `Move.dy` uses screen/framebuffer convention, not PS/2's: positive
/// means the cursor moved *down* the screen. PS/2 packets report Y with
/// "up" as positive, so `on_irq` negates it before this event is built —
/// callers should NOT flip the sign again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    /// Relative movement since the last event. `dx` positive = right,
    /// `dy` positive = down (see the type-level doc comment above for
    /// why `dy` is flipped from the raw PS/2 sign convention).
    Move { dx: i32, dy: i32 },
    LeftDown,
    LeftUp,
    ScrollUp,
    ScrollDown,
}

struct EventBuffer {
    buf: [Option<MouseEvent>; EVENT_BUFFER_CAPACITY],
    head: usize,
    tail: usize,
    len: usize,
}

impl EventBuffer {
    const fn new() -> Self {
        EventBuffer {
            buf: [None; EVENT_BUFFER_CAPACITY],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn push(&mut self, event: MouseEvent) {
        if self.len == EVENT_BUFFER_CAPACITY {
            self.head = (self.head + 1) % EVENT_BUFFER_CAPACITY;
            self.len -= 1;
        }
        self.buf[self.tail] = Some(event);
        self.tail = (self.tail + 1) % EVENT_BUFFER_CAPACITY;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<MouseEvent> {
        if self.len == 0 {
            return None;
        }
        let event = self.buf[self.head].take();
        self.head = (self.head + 1) % EVENT_BUFFER_CAPACITY;
        self.len -= 1;
        event
    }
}

static EVENT_BUFFER: Mutex<EventBuffer> = Mutex::new(EventBuffer::new());

/// In-progress packet bytes, filled in one byte at a time by `on_irq`
/// (IRQ12 fires once per byte, not once per 3-or-4-byte packet). Behind
/// the same lock discipline as `EVENT_BUFFER` even though in practice
/// only `on_irq` (always the same interrupt context) touches it — kept
/// as a `Mutex` rather than a bare `static mut` for the same reason the
/// rest of this kernel avoids `static mut` entirely.
struct PacketAssembly {
    bytes: [u8; 4],
    count: usize,
}

static PACKET: Mutex<PacketAssembly> = Mutex::new(PacketAssembly { bytes: [0; 4], count: 0 });

/// Whether `init()`'s IntelliMouse wheel-detection sequence succeeded —
/// determines whether `on_irq` assembles 3-byte or 4-byte packets.
static WHEEL_MODE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);


/// Left-button state as of the last decoded packet, so `on_irq` can emit
/// `LeftDown`/`LeftUp` only on transitions rather than once per packet
/// while the button is held down.
static LEFT_BUTTON_DOWN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn read_data() -> u8 {
    let mut port: Port<u8> = Port::new(DATA_PORT);
    unsafe { port.read() }
}

fn write_data(byte: u8) {
    let mut port: Port<u8> = Port::new(DATA_PORT);
    unsafe { port.write(byte) };
}

fn write_command(byte: u8) {
    let mut port: Port<u8> = Port::new(COMMAND_STATUS_PORT);
    unsafe { port.write(byte) };
}

fn read_status() -> u8 {
    let mut port: Port<u8> = Port::new(COMMAND_STATUS_PORT);
    unsafe { port.read() }
}

/// Waits (bounded) until the controller's output buffer has a byte
/// ready, then reads and returns it. Used both for controller replies
/// (e.g. the configuration byte) and for the mouse's own replies once
/// they've been routed through to the data port — the status bit means
/// "a byte is waiting at 0x60", regardless of which device it came from.
fn wait_and_read_data() -> Option<u8> {
    for _ in 0..POLL_LIMIT {
        if read_status() & STATUS_OUTPUT_FULL != 0 {
            return Some(read_data());
        }
        core::hint::spin_loop();
    }
    None
}

/// Waits (bounded) until the controller's input buffer is empty — i.e.
/// it's ready to accept another command/data byte — before returning.
/// Every write to 0x64/0x60 during init should be preceded by this, or
/// the controller can silently drop the byte if it's still processing
/// the previous one.
fn wait_input_ready() -> bool {
    for _ in 0..POLL_LIMIT {
        if read_status() & STATUS_INPUT_FULL == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Sends one command byte to the mouse device itself (as opposed to the
/// controller): prefixes it with `0xD4` on the command port, writes the
/// command byte on the data port, then waits for the mouse's `0xFA` ACK.
/// Returns `false` (soft failure) if any step doesn't complete within
/// `POLL_LIMIT` iterations or the reply isn't ACK.
fn send_mouse_command(command: u8) -> bool {
    if !wait_input_ready() {
        return false;
    }
    write_command(CMD_WRITE_TO_AUX);
    if !wait_input_ready() {
        return false;
    }
    write_data(command);
    matches!(wait_and_read_data(), Some(MOUSE_ACK))
}

/// Initializes the PS/2 mouse: enables the controller's auxiliary port
/// and IRQ12 generation, resets the mouse to defaults, turns on data
/// reporting, and (best-effort) negotiates the IntelliMouse extension for
/// a scroll-wheel byte in each packet.
///
/// Returns `true` if a mouse responded and reporting was successfully
/// enabled, `false` otherwise (no mouse present, or the controller/device
/// didn't ACK within the bounded wait anywhere along the way). A `false`
/// return is not fatal — the caller should simply not unmask/handle
/// IRQ12, and boot continues with no mouse support.
/// Wraps `init_locked` in `without_interrupts` for its entire duration —
/// a real bug, not a theoretical one: `init()` runs after
/// `interrupts::init()` has already enabled interrupts, and every step
/// here is polled I/O sharing `0x60`/`0x64` with `keyboard.rs`'s IRQ1
/// handler. Without this, a keyboard interrupt landing between one of
/// our own `write_command`s and the matching `wait_and_read_data()`
/// would have `keyboard::on_irq` read (and consume) the very byte we
/// were waiting for — `on_irq` has no way to know it wasn't a scancode,
/// and our own poll would then time out waiting for a reply that already
/// got eaten. Reproduced in practice: `init()` failed at the very first
/// controller-config-byte read on every boot before this fix, even
/// though QEMU's default PC machine always has a working i8042
/// controller regardless of whether a mouse is attached.
pub fn init() -> bool {
    x86_64::instructions::interrupts::without_interrupts(init_locked)
}

fn init_locked() -> bool {
    // Step 1: tell the controller to enable the auxiliary device. No
    // data byte follows this one — it's a bare controller command.
    if !wait_input_ready() {
        crate::serial_println!("mouse: controller not ready for AUX-enable command");
        return false;
    }
    write_command(CMD_ENABLE_AUX);

    // Step 2: read the controller configuration byte, set the AUX-IRQ
    // enable bit, write it back. Bit 1 of this byte is "enable IRQ12 /
    // enable the second PS/2 port's interrupt" per the standard i8042
    // controller configuration byte layout (bit 0 is the analogous
    // enable for IRQ1/the keyboard port, which `keyboard.rs`'s side of
    // init already relies on being set).
    if !wait_input_ready() {
        crate::serial_println!("mouse: controller not ready for read-config command");
        return false;
    }
    write_command(CMD_READ_CONFIG);
    let config = match wait_and_read_data() {
        Some(byte) => byte,
        None => {
            crate::serial_println!("mouse: controller did not return configuration byte");
            return false;
        }
    };
    let new_config = config | CONFIG_AUX_IRQ_ENABLE;
    if !wait_input_ready() {
        crate::serial_println!("mouse: controller not ready for write-config command");
        return false;
    }
    write_command(CMD_WRITE_CONFIG);
    if !wait_input_ready() {
        crate::serial_println!("mouse: controller not ready for configuration byte");
        return false;
    }
    write_data(new_config);

    // Step 3-4: reset the mouse to its power-on defaults.
    if !send_mouse_command(MOUSE_SET_DEFAULTS) {
        crate::serial_println!("mouse: no ACK for Set Defaults — assuming no mouse present");
        return false;
    }

    // Step 5 (attempted before enabling reporting): the IntelliMouse
    // wheel-detection "magic sequence" — three sample-rate-set commands
    // with specific values, each needing its own ACK, followed by a Get
    // Device ID. A real device that supports the extension recognizes
    // this exact sequence as a mode-switch request and starts reporting
    // ID 3 (instead of the default 0) and 4-byte packets. This is purely
    // best-effort: any failure along the way just leaves us in plain
    // 3-byte mode, which every PS/2 mouse supports.
    let mut wheel = false;
    let magic_sequence_ok = send_mouse_command(MOUSE_SET_SAMPLE_RATE)
        && send_mouse_command(200)
        && send_mouse_command(MOUSE_SET_SAMPLE_RATE)
        && send_mouse_command(100)
        && send_mouse_command(MOUSE_SET_SAMPLE_RATE)
        && send_mouse_command(80);
    if magic_sequence_ok && send_mouse_command(MOUSE_GET_DEVICE_ID) {
        if let Some(id) = wait_and_read_data() {
            wheel = id == 3;
        }
    }
    WHEEL_MODE.store(wheel, core::sync::atomic::Ordering::Relaxed);
    if wheel {
        crate::serial_println!("mouse: IntelliMouse wheel support detected");
    } else {
        crate::serial_println!("mouse: no wheel support (or detection failed) — using 3-byte packets");
    }

    // The wheel-detection magic sequence above (best-effort or not)
    // leaves the device's actual sample rate at whichever value its last
    // step happened to send (80 samples/sec) — an artifact of that
    // handshake, not a deliberate choice, and a real bottleneck: at
    // 80Hz the mouse can only ever report ~80 position updates a
    // second no matter how fast it's physically moved, which reads as a
    // sluggish, low-refresh-rate cursor even once IRQ delivery itself is
    // working. Explicitly set it back up to the PS/2 maximum (200) now
    // that detection is done. Best-effort like everything else here — a
    // failure just leaves the mouse at whatever rate it already had.
    if !send_mouse_command(MOUSE_SET_SAMPLE_RATE) || !send_mouse_command(200) {
        crate::serial_println!("mouse: could not raise sample rate to 200/sec — leaving it as-is");
    }

    // Step 6: turn on data reporting. From here on the mouse sends a
    // packet on every movement/button change, one byte per IRQ12.
    if !send_mouse_command(MOUSE_ENABLE_REPORTING) {
        crate::serial_println!("mouse: no ACK for Enable Data Reporting");
        return false;
    }

    PACKET.lock().count = 0;
    LEFT_BUTTON_DOWN.store(false, core::sync::atomic::Ordering::Relaxed);
    crate::serial_println!("mouse: initialized");
    true
}

/// Sign-extends a PS/2 9-bit movement value: `byte` is the low 8 bits,
/// `sign_bit_set` is bit 4 (X) or bit 5 (Y) of packet byte 0 — a
/// separate sign bit, not `byte`'s own MSB. When set, the true value is
/// `byte - 256` (i.e. `byte` interpreted as the low byte of a negative
/// two's-complement number one bit wider than a plain `i8`), not simply
/// reinterpreting `byte` as `i8`.
fn sign_extend_9bit(byte: u8, sign_bit_set: bool) -> i32 {
    let value = byte as i32;
    if sign_bit_set {
        value - 256
    } else {
        value
    }
}

/// Decodes one complete, already-buffered packet (3 or 4 bytes,
/// depending on `WHEEL_MODE`) into zero or more `MouseEvent`s pushed onto
/// `EVENT_BUFFER`.
fn decode_packet(bytes: &[u8]) {
    let byte0 = bytes[0];

    // Bit 3 is documented to always be 1 in a valid byte-0 position; a
    // packet that fails this check means we've lost byte alignment with
    // the device's stream (e.g. an IRQ was coalesced/missed a byte) —
    // drop it rather than decode garbage. The caller (`on_irq`) is
    // responsible for resynchronizing the *next* stream of bytes; here
    // we just refuse to act on a packet that doesn't look sane.
    if byte0 & 0x08 == 0 {
        return;
    }

    let x_overflow = byte0 & 0x40 != 0;
    let y_overflow = byte0 & 0x80 != 0;
    let x_sign = byte0 & 0x10 != 0;
    let y_sign = byte0 & 0x20 != 0;

    let dx = if x_overflow { 0 } else { sign_extend_9bit(bytes[1], x_sign) };
    // Negate: PS/2 reports "up" as a positive Y delta, but this driver's
    // public `MouseEvent::Move` uses screen convention (positive = down)
    // — see the type's doc comment.
    let dy_raw = if y_overflow { 0 } else { sign_extend_9bit(bytes[2], y_sign) };
    let dy = -dy_raw;

    if dx != 0 || dy != 0 {
        EVENT_BUFFER.lock().push(MouseEvent::Move { dx, dy });
    }

    let left_now = byte0 & 0x01 != 0;
    let left_before = LEFT_BUTTON_DOWN.swap(left_now, core::sync::atomic::Ordering::Relaxed);
    if left_now && !left_before {
        EVENT_BUFFER.lock().push(MouseEvent::LeftDown);
    } else if !left_now && left_before {
        EVENT_BUFFER.lock().push(MouseEvent::LeftUp);
    }

    if bytes.len() == 4 {
        // Low byte is the meaningful part; treat it as a signed 8-bit
        // value and only look at its sign for a single scroll-tick per
        // packet (magnitude beyond +-1 notch isn't something typical
        // hardware sends anyway, and we don't need finer-grained scroll
        // than "one tick" for the page renderer's purposes).
        let wheel = bytes[3] as i8;
        if wheel > 0 {
            EVENT_BUFFER.lock().push(MouseEvent::ScrollUp);
        } else if wheel < 0 {
            EVENT_BUFFER.lock().push(MouseEvent::ScrollDown);
        }
    }
}

/// Called from the IRQ12 handler once per byte that arrives on the data
/// port. Buffers bytes into a packet (3 or 4 bytes depending on whether
/// `init()` detected wheel support) and, once a complete packet has been
/// assembled, decodes it into `MouseEvent`s. Never blocks — reads
/// exactly one byte and returns, same constraint as `keyboard::on_irq`.
pub fn on_irq() {
    let byte = read_data();
    let wheel_mode = WHEEL_MODE.load(core::sync::atomic::Ordering::Relaxed);
    let packet_len = if wheel_mode { 4 } else { 3 };

    let mut packet = PACKET.lock();

    // Resynchronization: if we're expecting the first byte of a new
    // packet and it doesn't have bit 3 set, this stream is misaligned
    // (e.g. we started listening mid-packet, or a byte was dropped) —
    // discard bytes until one looks like a valid packet start instead of
    // building a packet on the wrong offset forever.
    if packet.count == 0 && byte & 0x08 == 0 {
        return;
    }

    let index = packet.count;
    packet.bytes[index] = byte;
    packet.count += 1;

    if packet.count == packet_len {
        let bytes = packet.bytes;
        let len = packet.count;
        packet.count = 0;
        drop(packet);
        decode_packet(&bytes[..len]);
    }
}

/// Non-blocking pop of the next buffered event, or `None` if there isn't
/// one yet. Wrapped in `without_interrupts` for the same reason
/// `mouse::init`'s own doc comment explains in detail: this runs in
/// ordinary task context with interrupts enabled, and `on_irq` (which
/// also locks `EVENT_BUFFER`, from inside the ISR) can fire at any
/// point — including mid-critical-section here. Without this, IRQ12
/// landing between this lock's acquire and release would have `on_irq`
/// spin forever waiting for a lock whose holder can never run again
/// (resuming it requires this very ISR to return first) — a same-core
/// deadlock that freezes the whole kernel, not just mouse input.
/// Reproduced in practice: real, frequent mouse motion would move the
/// cursor briefly and then the whole system would stop dead once the
/// race actually landed, exactly as expected from this class of bug.
pub fn poll_event() -> Option<MouseEvent> {
    x86_64::instructions::interrupts::without_interrupts(|| EVENT_BUFFER.lock().pop())
}
