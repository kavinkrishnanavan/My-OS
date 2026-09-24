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

pub fn on_irq() {
    let mut port: Port<u8> = Port::new(DATA_PORT);
    let scancode = unsafe { port.read() };

    if scancode & 0x80 != 0 {
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
