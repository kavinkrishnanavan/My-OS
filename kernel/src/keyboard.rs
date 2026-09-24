//! PS/2 keyboard driver: translates IBM PC/AT Scancode Set 1 bytes read
//! from the i8042 controller's data port into ASCII, and buffers the
//! result in a small ring so `SYS_READ_KEY` can pop keystrokes later.
//!
//! `on_irq` is called directly from the IRQ1 handler and `pop_key` from a
//! syscall handler — same constraint as everywhere else in this kernel
//! that borders an interrupt gate: IF is off for the whole handler, so
//! neither function may loop or wait for anything. `on_irq` always reads
//! exactly one scancode and returns; `pop_key` either has a byte ready or
//! it doesn't.

use spin::Mutex;
use x86_64::instructions::port::Port;

const DATA_PORT: u16 = 0x60;
const KEY_BUFFER_CAPACITY: usize = 256;

/// Scancode Set 1 make codes (key press) -> ASCII, indexed by scancode.
/// `0` means "no mapping". Release codes are the make code with the high
/// bit set (0x80+) and are never looked up here; unmapped/extended codes
/// (function keys, arrows, modifiers, the 0xE0 prefix itself) are left 0
/// and silently dropped.
const SET1_TO_ASCII: [u8; 128] = {
    let mut table = [0u8; 128];
    table[0x01] = 27; // Escape
    table[0x02] = b'1';
    table[0x03] = b'2';
    table[0x04] = b'3';
    table[0x05] = b'4';
    table[0x06] = b'5';
    table[0x07] = b'6';
    table[0x08] = b'7';
    table[0x09] = b'8';
    table[0x0A] = b'9';
    table[0x0B] = b'0';
    table[0x0C] = b'-';
    table[0x0D] = b'=';
    table[0x0E] = 8; // Backspace
    table[0x0F] = b'\t';
    table[0x10] = b'q';
    table[0x11] = b'w';
    table[0x12] = b'e';
    table[0x13] = b'r';
    table[0x14] = b't';
    table[0x15] = b'y';
    table[0x16] = b'u';
    table[0x17] = b'i';
    table[0x18] = b'o';
    table[0x19] = b'p';
    table[0x1A] = b'[';
    table[0x1B] = b']';
    table[0x1C] = b'\n'; // Enter
    table[0x1E] = b'a';
    table[0x1F] = b's';
    table[0x20] = b'd';
    table[0x21] = b'f';
    table[0x22] = b'g';
    table[0x23] = b'h';
    table[0x24] = b'j';
    table[0x25] = b'k';
    table[0x26] = b'l';
    table[0x27] = b';';
    table[0x28] = b'\'';
    table[0x29] = b'`';
    table[0x2B] = b'\\';
    table[0x2C] = b'z';
    table[0x2D] = b'x';
    table[0x2E] = b'c';
    table[0x2F] = b'v';
    table[0x30] = b'b';
    table[0x31] = b'n';
    table[0x32] = b'm';
    table[0x33] = b',';
    table[0x34] = b'.';
    table[0x35] = b'/';
    table[0x37] = b'*';
    table[0x39] = b' '; // Space
    table
};

/// Non-ASCII key codes `pop_key` can return, for keys `SET1_TO_ASCII`
/// has no mapping for at all (extended/`0xE0`-prefixed scancodes).
/// Chosen well above ASCII's 0-127 range so a caller can always tell
/// "real typed character" from "navigation key" by comparing against
/// 128, with no ambiguity against any Latin-1-supplement byte either
/// (only reachable via keys this driver doesn't decode from raw bytes
/// anyway — those come from `dead-key`/IME composition this driver
/// doesn't implement).
pub const KEY_UP: u8 = 200;
pub const KEY_DOWN: u8 = 201;
pub const KEY_PAGE_UP: u8 = 202;
pub const KEY_PAGE_DOWN: u8 = 203;

struct KeyBuffer {
    buf: [u8; KEY_BUFFER_CAPACITY],
    head: usize,
    tail: usize,
    len: usize,
}

impl KeyBuffer {
    const fn new() -> Self {
        KeyBuffer {
            buf: [0u8; KEY_BUFFER_CAPACITY],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.len == KEY_BUFFER_CAPACITY {
            self.head = (self.head + 1) % KEY_BUFFER_CAPACITY;
            self.len -= 1;
        }
        self.buf[self.tail] = byte;
        self.tail = (self.tail + 1) % KEY_BUFFER_CAPACITY;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.buf[self.head];
        self.head = (self.head + 1) % KEY_BUFFER_CAPACITY;
        self.len -= 1;
        Some(byte)
    }
}

static KEY_BUFFER: Mutex<KeyBuffer> = Mutex::new(KeyBuffer::new());

/// Set by the previous call to `on_irq` when it read the `0xE0` prefix
/// byte — Set 1 sends extended keys (arrows, Page Up/Down, the right-side
/// Ctrl/Alt, etc.) as two bytes, `0xE0` followed by a code that overlaps
/// the *non-extended* table's own values (e.g. plain `0x48` is `SET1_TO_ASCII`'s
/// unmapped numpad-8, but `E0 48` is the Up arrow) — so this has to persist
/// across the two separate `on_irq` calls that make up one extended key
/// event. `AtomicBool` rather than plain state since it's touched only
/// from `on_irq` (always the same, single, interrupt context) — no real
/// concurrency, just a convenient `Sync` static.
static PENDING_EXTENDED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn on_irq() {
    use core::sync::atomic::Ordering;
    let mut port: Port<u8> = Port::new(DATA_PORT);
    let scancode = unsafe { port.read() };

    if scancode == 0xE0 {
        PENDING_EXTENDED.store(true, Ordering::Relaxed);
        return;
    }
    let extended = PENDING_EXTENDED.swap(false, Ordering::Relaxed);

    if scancode & 0x80 != 0 {
        return; // release code — never produces a key
    }

    if extended {
        let code = match scancode {
            0x48 => KEY_UP,
            0x50 => KEY_DOWN,
            0x49 => KEY_PAGE_UP,
            0x51 => KEY_PAGE_DOWN,
            _ => return, // other extended keys (right Ctrl/Alt, etc.) — not supported
        };
        KEY_BUFFER.lock().push(code);
        return;
    }

    let ascii = SET1_TO_ASCII[scancode as usize];
    if ascii != 0 {
        KEY_BUFFER.lock().push(ascii);
    }
}

pub fn pop_key() -> Option<u8> {
    KEY_BUFFER.lock().pop()
}
