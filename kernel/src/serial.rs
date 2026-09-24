//! 16550 UART driver on COM1 (I/O port 0x3F8).
//!
//! This is our console: QEMU's `-serial stdio` (or `-chardev` to a file/
//! socket) puts everything we write here on the host terminal, so it's how
//! we prove network fetches actually happened without needing a screen.

use core::fmt;
use spin::Mutex;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

pub struct SerialPort {
    data: Port<u8>,
    int_en: Port<u8>,
    fifo_ctrl: Port<u8>,
    line_ctrl: Port<u8>,
    modem_ctrl: Port<u8>,
    line_status: Port<u8>,
}

impl SerialPort {
    const fn new(base: u16) -> Self {
        SerialPort {
            data: Port::new(base),
            int_en: Port::new(base + 1),
            fifo_ctrl: Port::new(base + 2),
            line_ctrl: Port::new(base + 3),
            modem_ctrl: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    /// # Safety
    /// Must only be called once, before any other port access on COM1.
    unsafe fn init(&mut self) {
        self.int_en.write(0x00); // disable interrupts, we poll this port
        self.line_ctrl.write(0x80); // enable DLAB to set baud rate divisor
        self.data.write(0x03); // divisor low byte -> 38400 baud
        self.int_en.write(0x00); // divisor high byte
        self.line_ctrl.write(0x03); // 8 bits, no parity, one stop bit
        self.fifo_ctrl.write(0xC7); // enable + clear FIFOs, 14-byte threshold
        self.modem_ctrl.write(0x0B); // RTS/DSR set, enable IRQ line (unused)
    }

    fn line_status(&mut self) -> u8 {
        unsafe { self.line_status.read() }
    }

    fn write_byte(&mut self, byte: u8) {
        while self.line_status() & 0x20 == 0 {
            core::hint::spin_loop();
        }
        unsafe { self.data.write(byte) }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            match byte {
                b'\n' => {
                    self.write_byte(b'\r');
                    self.write_byte(b'\n');
                }
                byte => self.write_byte(byte),
            }
        }
        Ok(())
    }
}

pub static SERIAL1: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1));

/// # Safety
/// Must be called exactly once, early in `_start`, before any `serial_print!`.
pub unsafe fn init() {
    SERIAL1.lock().init();
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    use x86_64::instructions::interrupts;

    // Avoid deadlocking if an interrupt handler also wants to print while
    // we hold the lock.
    interrupts::without_interrupts(|| {
        SERIAL1.lock().write_fmt(args).expect("serial write failed");
    });
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! serial_println {
    () => { $crate::serial_print!("\n") };
    // `$($arg:tt)*` is forwarded to `format_args!` untouched (as opposed
    // to going through `concat!` first) specifically so implicit named
    // captures like `serial_println!("{host}")` still work — `concat!`
    // produces a fresh string literal whose identifiers `format_args!`
    // can no longer resolve against the call site's local variables.
    ($($arg:tt)*) => {{
        $crate::serial_print!($($arg)*);
        $crate::serial_print!("\n");
    }};
}
