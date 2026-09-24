pub mod http;
pub mod rtl8139;
pub mod stack;
pub mod tcp_stream;
pub mod tls;

use alloc::vec::Vec;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

/// Bridges our interrupt-driven RTL8139 driver to smoltcp's polling
/// `Device` trait. smoltcp itself is not async — it's a plain state
/// machine you call `poll()` on — so the non-blocking behaviour lives one
/// layer up, in `net::stack::poll_task`, which only calls `poll()` when
/// the executor schedules it (on IRQ wakeup or a bounded timer), never in
/// a tight loop.
pub struct Rtl8139Device;

impl Device for Rtl8139Device {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if !rtl8139::rx_ready() {
            return None;
        }
        let mut buf = alloc::vec![0u8; 1600];
        let len = rtl8139::recv(&mut buf)?;
        buf.truncate(len);
        Some((RxToken { buf }, TxToken))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct RxToken {
    buf: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.buf)
    }
}

pub struct TxToken;

impl phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = alloc::vec![0u8; len];
        let result = f(&mut buf);
        rtl8139::send(&buf);
        result
    }
}
