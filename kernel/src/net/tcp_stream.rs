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

/// Bounds on `connect`'s and `read_to_end`'s own wait loops — this
/// kernel's established rule (see `disk/ata.rs`'s `POLL_LIMIT`, or
/// `myos_userlib::wait`'s own retry cap) that nothing here waits on an
/// external event forever. A real, reproduced hang motivated this one:
/// a second sequential TLS/TCP fetch on the same host (`net/http.rs`
/// fetching a linked stylesheet right after the page body) sat waiting
/// indefinitely — plausibly the peer or an intermediate connection-reuse
/// path never actually closing/establishing cleanly. Whatever the exact
/// cause, an unbounded `loop { ...; net_tick().await }` had no way to
/// ever give up, so a caller (`net::http::render_page`) that needed to
/// abandon a slow/stuck stylesheet and move on had no way to.
const CONNECT_TICK_LIMIT: u32 = 200_000;
const READ_TICK_LIMIT: u32 = 200_000;

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

        for _ in 0..CONNECT_TICK_LIMIT {
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
        stack::with_stack(|s| s.sockets.remove(handle));
        Err("tcp connect timed out")
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
        // This is the innermost of the two loops `CONNECT_TICK_LIMIT`'s
        // doc comment describes — the one actually reproduced hanging:
        // a peer that stops sending without ever closing its side
        // (`may_recv` staying true) spins here forever with nothing to
        // ever break it out. Bounded the same way.
        for _ in 0..READ_TICK_LIMIT {
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
        Err(ErrorKind::TimedOut)
    }
}

impl Write for TcpStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, ErrorKind> {
        for _ in 0..READ_TICK_LIMIT {
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
        Err(ErrorKind::TimedOut)
    }

    async fn flush(&mut self) -> Result<(), ErrorKind> {
        // smoltcp sends as soon as data is queued and the window allows
        // it (driven by `net_poll_loop`'s `stack::poll()`), so there's
        // no separate buffered-in-userspace state here to force out.
        Ok(())
    }
}
