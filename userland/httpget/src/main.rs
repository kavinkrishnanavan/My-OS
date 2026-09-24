//! A tiny ring-3 HTTP client.
//!
//! The kernel already has its own boot-time fetcher, but that keeps the
//! browser experiment trapped in ring 0. This program proves the next,
//! more Ladybird-shaped boundary: userland can open a TCP socket fd,
//! write an HTTP request, read the response, and print it without direct
//! access to the NIC driver or smoltcp internals.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{close, connect_blocking, read, resolve_blocking, write_fd, Writer, WOULD_BLOCK};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

// A hostname, not a hardcoded IP, as the default — the earlier version
// of this demo hardcoded example.com's IP directly and that address
// later went dead (IANA re-delegated it, and the stale literal left this
// program spinning forever with no way to tell "unreachable" from "still
// connecting"; see connect_blocking's own doc comment for the retry-cap
// fix that came out of chasing that). Resolving a real hostname instead
// means this exercises SYS_RESOLVE too, and stays correct even if
// whatever this domain points to changes again later.
const DEFAULT_HOST: &str = "example.com";
const DEFAULT_PATH: &str = "/";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let arg = myos_userlib::arg_string().unwrap_or_default();
    let mut parts = arg.split_whitespace();
    let host = parts.next().unwrap_or(DEFAULT_HOST);
    let path = parts.next().unwrap_or(DEFAULT_PATH);

    let ip = if let Some(ip) = parse_ipv4(host) {
        ip
    } else {
        let _ = writeln!(Writer, "httpget: resolving {host}");
        match resolve_blocking(host) {
            Some(ip) => ip,
            None => {
                let _ = writeln!(Writer, "httpget: DNS resolution failed");
                myos_userlib::exit(1);
            }
        }
    };

    let _ = writeln!(Writer, "httpget: connecting to {host}{path}");
    let fd = connect_blocking(ip, 80);
    if fd == u64::MAX {
        let _ = writeln!(Writer, "httpget: connect failed");
        myos_userlib::exit(1);
    }

    let request = HttpRequest { host, path };
    let mut writer = SocketWriter { fd, ok: true };
    let _ = write!(
        writer,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: myos-userland/0.1\r\n\r\n",
        request.path, request.host
    );
    if !writer.ok {
        let _ = writeln!(Writer, "httpget: request write failed");
        close(fd);
        myos_userlib::exit(1);
    }

    let mut total = 0usize;
    let mut buf = [0u8; 256];
    loop {
        let n = read(fd, &mut buf);
        if n == WOULD_BLOCK {
            for _ in 0..200_000 {
                core::hint::spin_loop();
            }
            continue;
        }
        if n == 0 {
            break;
        }
        if n == u64::MAX {
            let _ = writeln!(Writer, "\nhttpget: read failed");
            close(fd);
            myos_userlib::exit(1);
        }
        let n = n as usize;
        total += n;
        write_fd(myos_userlib::STDOUT, &buf[..n]);
    }

    close(fd);
    let _ = writeln!(Writer, "\nhttpget: received {total} bytes");
    myos_userlib::exit(0);
}

struct HttpRequest<'a> {
    host: &'a str,
    path: &'a str,
}

struct SocketWriter {
    fd: u64,
    ok: bool,
}

impl core::fmt::Write for SocketWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut rest = s.as_bytes();
        while !rest.is_empty() {
            let n = write_fd(self.fd, rest);
            if n == WOULD_BLOCK {
                for _ in 0..200_000 {
                    core::hint::spin_loop();
                }
                continue;
            }
            if n == 0 || n == u64::MAX {
                self.ok = false;
                return Err(core::fmt::Error);
            }
            rest = &rest[n as usize..];
        }
        Ok(())
    }
}

fn parse_ipv4(s: &str) -> Option<u32> {
    let mut out = 0u32;
    let mut count = 0usize;
    for part in s.split('.') {
        let octet: u8 = part.parse().ok()?;
        out = (out << 8) | octet as u32;
        count += 1;
    }
    (count == 4).then_some(out)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
