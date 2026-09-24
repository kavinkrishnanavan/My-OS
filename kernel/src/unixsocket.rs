//! In-kernel Unix domain sockets: connection-oriented, named (bound to an
//! arbitrary string "address" rather than a real filesystem path — nothing
//! is written to FAT32), and unlike `net::stack`'s TCP sockets, smoltcp and
//! the NIC are never involved — a connected pair is just two byte streams
//! shuffled between two entries of the in-kernel `SOCKETS` table.
//!
//! Same hard constraint as `pipe.rs`: this kernel's interrupt gates disable
//! IF for the whole syscall handler, so nothing here may loop waiting on
//! another thread. `accept`/`connect`/`read`/`write` all return immediately,
//! with `UnixSocketError::WouldBlock` standing in for "try again later".
//!
//! `connect` and `accept` compose across two steps because a syscall can't
//! block for the other side to show up: `connect` never itself creates the
//! connected pair, since the listener's owning thread might not call
//! `accept` until later (or never). It only enqueues a `PendingConnection`
//! (pre-built ring buffers, no peer yet) onto the listener's backlog and
//! immediately rewrites `id`'s own state to `Connected`, wired to those same
//! buffers. Whenever `accept` is next called on the listener, it just pops
//! the queue and hands out a fresh id pointing at the same pair — no
//! separate "handshake complete" signal is needed because the buffers
//! already exist and are already shared.
//!
//! `bind_and_listen`'s `backlog` is enforced by capping the pending-queue
//! length; a `connect()` that finds it full is indistinguishable here from
//! "nobody's listening" (this kernel's error enum stays deliberately small,
//! same convention as `pipe.rs`/`fs.rs`), so it also reports
//! `AddressNotFound` rather than a dedicated "backlog full" variant.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

const SOCKET_BUF_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixSocketId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSocketError {
    WouldBlock,
    BrokenPipe,
    AddressInUse,
    AddressNotFound,
    NotListening,
}

/// One direction of a connected pair's byte stream, shared (via `Arc`)
/// between the two `SocketState::Connected` entries that make up a pair.
/// The two ends can close independently, so a single "alive" bit isn't
/// enough: `writer_open` governs whether the *reader* sees `WouldBlock`
/// (writer still around) or real EOF (`Ok(0)`, writer gone); `reader_open`
/// governs whether the *writer* sees success/`WouldBlock` or `BrokenPipe`
/// (nobody left to ever read this).
struct Channel {
    buf: Mutex<VecDeque<u8>>,
    writer_open: core::sync::atomic::AtomicBool,
    reader_open: core::sync::atomic::AtomicBool,
}

impl Channel {
    fn new() -> Arc<Channel> {
        Arc::new(Channel {
            buf: Mutex::new(VecDeque::with_capacity(SOCKET_BUF_CAPACITY)),
            writer_open: core::sync::atomic::AtomicBool::new(true),
            reader_open: core::sync::atomic::AtomicBool::new(true),
        })
    }
}

/// A connect() that hasn't been accept()ed yet: a fully-built pair of
/// channels with no owning `Connected` socket on the server side. `accept`
/// pops one of these and wraps it in a fresh id.
struct PendingConnection {
    /// Channel this (future) server-end reads from (i.e. client -> server).
    recv: Arc<Channel>,
    /// Channel this (future) server-end writes to (i.e. server -> client).
    send: Arc<Channel>,
}

struct ListenerState {
    backlog: usize,
    pending: VecDeque<PendingConnection>,
}

enum SocketState {
    /// Freshly created via `create()`, not yet bound or connected.
    Unbound,
    /// Bound via `bind_and_listen`; owns a slot in `LISTENERS` keyed by
    /// the same name.
    Listening { name: String },
    /// A connected end, one way or the other (client or server side —
    /// both look identical once connected). `recv` is drained by `read`,
    /// `send` is appended to by `write`.
    Connected { recv: Arc<Channel>, send: Arc<Channel> },
}

static SOCKETS: Mutex<BTreeMap<u64, SocketState>> = Mutex::new(BTreeMap::new());
static LISTENERS: Mutex<BTreeMap<String, ListenerState>> = Mutex::new(BTreeMap::new());
static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);

pub fn create() -> UnixSocketId {
    let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    SOCKETS.lock().insert(id, SocketState::Unbound);
    UnixSocketId(id)
}

pub fn bind_and_listen(id: UnixSocketId, name: &str, backlog: usize) -> Result<(), UnixSocketError> {
    let mut listeners = LISTENERS.lock();
    if listeners.contains_key(name) {
        return Err(UnixSocketError::AddressInUse);
    }

    let mut sockets = SOCKETS.lock();
    let state = match sockets.get_mut(&id.0) {
        Some(state) => state,
        None => return Err(UnixSocketError::AddressNotFound),
    };

    listeners.insert(
        String::from(name),
        ListenerState {
            backlog,
            pending: VecDeque::new(),
        },
    );
    *state = SocketState::Listening { name: String::from(name) };
    Ok(())
}

pub fn accept(id: UnixSocketId) -> Result<UnixSocketId, UnixSocketError> {
    let name = {
        let sockets = SOCKETS.lock();
        match sockets.get(&id.0) {
            Some(SocketState::Listening { name }) => name.clone(),
            Some(_) => return Err(UnixSocketError::NotListening),
            None => return Err(UnixSocketError::NotListening),
        }
    };

    let mut listeners = LISTENERS.lock();
    let listener = match listeners.get_mut(&name) {
        Some(l) => l,
        None => return Err(UnixSocketError::NotListening),
    };

    let pending = match listener.pending.pop_front() {
        Some(p) => p,
        None => return Err(UnixSocketError::WouldBlock),
    };
    drop(listeners);

    let new_id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    SOCKETS.lock().insert(
        new_id,
        SocketState::Connected {
            recv: pending.recv,
            send: pending.send,
        },
    );
    Ok(UnixSocketId(new_id))
}

pub fn connect(id: UnixSocketId, name: &str) -> Result<(), UnixSocketError> {
    let mut listeners = LISTENERS.lock();
    let listener = match listeners.get_mut(name) {
        Some(l) => l,
        None => return Err(UnixSocketError::AddressNotFound),
    };

    if listener.pending.len() >= listener.backlog {
        // Simplification per this kernel's small error enum: a full
        // backlog is reported the same as "nobody's listening".
        return Err(UnixSocketError::AddressNotFound);
    }

    // client -> server and server -> client channels.
    let c2s = Channel::new();
    let s2c = Channel::new();

    listener.pending.push_back(PendingConnection {
        recv: c2s.clone(),
        send: s2c.clone(),
    });
    drop(listeners);

    let mut sockets = SOCKETS.lock();
    let state = match sockets.get_mut(&id.0) {
        Some(state) => state,
        None => return Err(UnixSocketError::AddressNotFound),
    };
    // Client reads what the server sends (s2c) and writes into c2s.
    *state = SocketState::Connected { recv: s2c, send: c2s };
    Ok(())
}

pub fn read(id: UnixSocketId, buf: &mut [u8]) -> Result<usize, UnixSocketError> {
    let sockets = SOCKETS.lock();
    let (recv, _send) = match sockets.get(&id.0) {
        Some(SocketState::Connected { recv, send }) => (recv.clone(), send.clone()),
        _ => return Err(UnixSocketError::BrokenPipe),
    };
    drop(sockets);

    let mut chan = recv.buf.lock();
    if chan.is_empty() {
        if !recv.writer_open.load(Ordering::Acquire) {
            return Ok(0);
        }
        return Err(UnixSocketError::WouldBlock);
    }

    let n = core::cmp::min(buf.len(), chan.len());
    for slot in buf.iter_mut().take(n) {
        *slot = chan.pop_front().expect("checked non-empty above");
    }
    Ok(n)
}

pub fn write(id: UnixSocketId, buf: &[u8]) -> Result<usize, UnixSocketError> {
    let sockets = SOCKETS.lock();
    let send = match sockets.get(&id.0) {
        Some(SocketState::Connected { send, .. }) => send.clone(),
        _ => return Err(UnixSocketError::BrokenPipe),
    };
    drop(sockets);

    if !send.reader_open.load(Ordering::Acquire) {
        return Err(UnixSocketError::BrokenPipe);
    }

    let mut chan = send.buf.lock();
    let space = SOCKET_BUF_CAPACITY.saturating_sub(chan.len());
    if space == 0 {
        return Err(UnixSocketError::WouldBlock);
    }

    let n = core::cmp::min(space, buf.len());
    chan.extend(buf[..n].iter().copied());
    Ok(n)
}

pub fn close(id: UnixSocketId) {
    let mut sockets = SOCKETS.lock();
    let state = match sockets.remove(&id.0) {
        Some(state) => state,
        None => return,
    };

    match state {
        SocketState::Listening { name } => {
            LISTENERS.lock().remove(&name);
        }
        SocketState::Connected { recv, send } => {
            // This end is `recv`'s reader: nobody will ever read it again,
            // so the peer's next write to it must see BrokenPipe.
            recv.reader_open.store(false, Ordering::Release);
            // This end is `send`'s writer: mark it closed so the peer's
            // next read sees EOF once it drains what's already buffered.
            send.writer_open.store(false, Ordering::Release);
        }
        SocketState::Unbound => {}
    }
}
