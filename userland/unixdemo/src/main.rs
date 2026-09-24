//! Proves `SYS_USOCK_*` end-to-end: a *ring-3* program creating a named,
//! connection-oriented, non-blocking Unix-domain-socket-like endpoint
//! (`kernel/src/unixsocket.rs`), binding+listening under a name, connecting
//! a second socket to that name, accepting the resulting connection, and
//! then exchanging bytes in *both* directions over it.
//!
//! This is deliberately a single process playing both "ends" sequentially
//! — there's no need for two separate spawned programs just to prove the
//! primitive itself works. What it proves that `pipedemo` doesn't: a real
//! connection (an explicit `bind_listen`/`connect`/`accept` handshake,
//! addressed by name rather than handed back as a fd pair) that is
//! bidirectional — writes from either side of the accepted connection are
//! readable from the other, unlike `pipe()`'s single fixed direction.
//!
//! Both the accept and the reads have to retry: `usock_accept` and
//! `usock_read` are non-blocking (return `WOULD_BLOCK`/`u64::MAX - 1`
//! exactly like a pipe or TCP socket read), and nothing else in this
//! single-threaded demo is going to wake anything up mid-syscall — so
//! this spins the same bounded retry loop `connect_blocking`/
//! `resolve_blocking`/`pipedemo` already use elsewhere, rather than
//! assuming state is visible on the very first attempt.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{Writer, WOULD_BLOCK};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const SOCK_NAME: &str = "unixdemo.sock";
const CLIENT_TO_SERVER: &[u8] = b"hello over a unix socket";
const SERVER_TO_CLIENT: &[u8] = b"and hello right back";

/// Bounded retry loop reading exactly `want` bytes from `id` via
/// `usock_read` — the same shape as `pipedemo`'s read loop, adapted for
/// unix-socket ids instead of regular fds.
fn read_exact_blocking(id: u64, want: usize, buf: &mut [u8]) -> Option<usize> {
    let mut got: usize = 0;
    for _ in 0..4000 {
        let n = myos_userlib::usock_read(id, &mut buf[got..]);
        match n {
            WOULD_BLOCK => {
                for _ in 0..200_000 {
                    core::hint::spin_loop();
                }
            }
            0 => break, // EOF
            u64::MAX => return None,
            n => {
                got += n as usize;
                if got >= want {
                    break;
                }
            }
        }
    }
    Some(got)
}

fn report_roundtrip(label: &str, expected: &[u8], got: &[u8]) {
    if got == expected {
        if let Ok(s) = core::str::from_utf8(got) {
            let _ = writeln!(
                Writer,
                "unixdemo: {label} round-trip OK, got {} bytes: {s}",
                got.len()
            );
        } else {
            let _ = writeln!(
                Writer,
                "unixdemo: {label} round-trip OK, got {} bytes (non-utf8)",
                got.len()
            );
        }
    } else {
        let _ = writeln!(
            Writer,
            "unixdemo: {label} MISMATCH expected {} bytes got {} bytes",
            expected.len(),
            got.len()
        );
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // 1. Create both endpoints.
    let listener_id = myos_userlib::usock_create();
    let client_id = myos_userlib::usock_create();
    let _ = writeln!(
        Writer,
        "unixdemo: usock_create() -> listener_id={listener_id} client_id={client_id}"
    );

    // 2. Bind + listen.
    if myos_userlib::usock_bind_listen(listener_id, SOCK_NAME, 4) {
        let _ = writeln!(Writer, "unixdemo: bind_listen({SOCK_NAME:?}) ok");
    } else {
        let _ = writeln!(Writer, "unixdemo: bind_listen({SOCK_NAME:?}) FAILED");
        myos_userlib::exit(1);
    }

    // 3. Connect the client.
    if myos_userlib::usock_connect(client_id, SOCK_NAME) {
        let _ = writeln!(Writer, "unixdemo: connect({SOCK_NAME:?}) ok");
    } else {
        let _ = writeln!(Writer, "unixdemo: connect({SOCK_NAME:?}) FAILED");
        myos_userlib::exit(1);
    }

    // 4. Bounded retry loop accepting the pending connection.
    let mut accepted_id = u64::MAX;
    for _ in 0..4000 {
        match myos_userlib::usock_accept(listener_id) {
            WOULD_BLOCK => {
                for _ in 0..200_000 {
                    core::hint::spin_loop();
                }
            }
            u64::MAX => {
                let _ = writeln!(Writer, "unixdemo: accept() FAILED (not a listening socket)");
                break;
            }
            id => {
                accepted_id = id;
                break;
            }
        }
    }
    if accepted_id == u64::MAX {
        let _ = writeln!(Writer, "unixdemo: accept() never completed");
        myos_userlib::exit(1);
    }
    let _ = writeln!(Writer, "unixdemo: accept() ok accepted_id={accepted_id}");

    // 5. Client writes to the server.
    let written = myos_userlib::usock_write(client_id, CLIENT_TO_SERVER);
    let _ = writeln!(
        Writer,
        "unixdemo: usock_write(client_id, ..) wrote {written} bytes"
    );

    // 6. Server (accepted_id) reads it back.
    let mut buf1 = [0u8; 64];
    match read_exact_blocking(accepted_id, CLIENT_TO_SERVER.len(), &mut buf1) {
        Some(got) => report_roundtrip("client->server", CLIENT_TO_SERVER, &buf1[..got]),
        None => let_none("client->server"),
    }

    // 7. Server writes a reply; client reads it — proving the other
    //    direction works too, unlike pipedemo's one-way pipe.
    let written2 = myos_userlib::usock_write(accepted_id, SERVER_TO_CLIENT);
    let _ = writeln!(
        Writer,
        "unixdemo: usock_write(accepted_id, ..) wrote {written2} bytes"
    );

    let mut buf2 = [0u8; 64];
    match read_exact_blocking(client_id, SERVER_TO_CLIENT.len(), &mut buf2) {
        Some(got) => report_roundtrip("server->client", SERVER_TO_CLIENT, &buf2[..got]),
        None => let_none("server->client"),
    }

    myos_userlib::exit(0);
}

fn let_none(label: &str) {
    let _ = writeln!(Writer, "unixdemo: {label} read FAILED (not a connected socket)");
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
