//! A TCP connection wrapped as `embedded_io_async::{Read, Write}` —
//! the trait `embedded-tls` wants its transport to implement. This is
//! the same connect/send/recv pattern `http.rs` used directly against
//! smoltcp before TLS existed, factored out so both plain HTTP and the
//! TLS layer share it.
//!
//! Every wait is `net_tick().await`, same as everywhere else in this
//! kernel: control goes back to the executor, and the socket is only
//! re-checked when a NIC IRQ or the PIT tick actually gives a reason to.

use crate::net::stack::{self, net_tick};
use alloc::vec::Vec;
use embedded_io::{ErrorKind, ErrorType};
use embedded_io_async::{Read, Write};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, IpEndpoint};

pub struct TcpStream {
    handle: SocketHandle,
}

impl TcpStream {
    pub async fn connect(ip: IpAddress, port: u16) -> Result<Self, &'static str> {
        let handle = stack::with_stack(|s| {
            let socket = stack::new_tcp_socket();
            s.sockets.add(socket)
        });

        stack::with_stack(|s| {
            let (sockets, iface) = (&mut s.sockets, &mut s.iface);
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            socket
                .connect(iface.context(), IpEndpoint::new(ip, port), 49152)
                .map_err(|_| "tcp connect failed")
        })?;

        loop {
            let state =
                stack::with_stack(|s| s.sockets.get::<tcp::Socket>(handle).state());
            match state {
                tcp::State::Established => return Ok(TcpStream { handle }),
                tcp::State::Closed | tcp::State::TimeWait => {
                    return Err("connection closed before it was established")
                }
                _ => net_tick().await,
            }
        }
    }

    /// Reads everything left to read (until the peer closes its side),
    /// for callers that don't need TLS and just want the whole response
    /// — the same pattern `http.rs`'s plain-HTTP path uses.
    pub async fn read_to_end(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match Read::read(self, &mut buf).await {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        stack::with_stack(|s| {
            s.sockets.remove(self.handle);
        });
    }
}

impl ErrorType for TcpStream {
    type Error = ErrorKind;
}

impl Read for TcpStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, ErrorKind> {
        loop {
            let (n, may_recv) = stack::with_stack(|s| {
                let socket = s.sockets.get_mut::<tcp::Socket>(self.handle);
                let n = socket.recv_slice(buf).unwrap_or(0);
                (n, socket.may_recv())
            });
            if n > 0 {
                return Ok(n);
            }
            if !may_recv {
                return Ok(0); // peer closed its side: EOF
            }
            net_tick().await;
        }
    }
}

impl Write for TcpStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, ErrorKind> {
        loop {
            let n = stack::with_stack(|s| {
                let socket = s.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_send() {
                    socket.send_slice(buf).unwrap_or(0)
                } else if !socket.may_send() {
                    usize::MAX // sentinel: connection gone
                } else {
                    0
                }
            });
            match n {
                usize::MAX => return Err(ErrorKind::ConnectionAborted),
                0 => net_tick().await,
                n => return Ok(n),
            }
        }
    }

    async fn flush(&mut self) -> Result<(), ErrorKind> {
        // smoltcp sends as soon as data is queued and the window allows
        // it (driven by `net_poll_loop`'s `stack::poll()`), so there's
        // no separate buffered-in-userspace state here to force out.
        Ok(())
    }
}
