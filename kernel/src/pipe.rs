//! Fixed-capacity in-kernel pipes: a Unix-pipe-style byte ring buffer that
//! one thread writes into and another reads out of, used to back the
//! pipe-family syscalls. Every pipe lives in the global `PIPES` table,
//! keyed by an opaque `PipeId`, with `readers`/`writers` refcounts so the
//! buffer can be freed once both ends are closed.
//!
//! `read`/`write` here are called directly from syscall handlers, and this
//! kernel's interrupt gates disable IF for the whole handler — there is no
//! way to park a handler and resume it later, and no other thread can run
//! to make progress while one is blocked inside a gate. So neither
//! function may ever loop waiting on the other end; both return
//! immediately, with `PipeError::WouldBlock` standing in for "try the
//! syscall again later" (the caller, e.g. via a retry-on-EAGAIN loop at
//! ring 3 or a scheduler yield, is what actually waits).

use alloc::collections::{BTreeMap, VecDeque};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

const PIPE_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PipeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeError {
    WouldBlock,
    BrokenPipe,
}

struct PipeState {
    buf: VecDeque<u8>,
    readers: u32,
    writers: u32,
}

impl PipeState {
    fn new() -> Self {
        PipeState {
            buf: VecDeque::with_capacity(PIPE_CAPACITY),
            readers: 1,
            writers: 1,
        }
    }
}

static PIPES: Mutex<BTreeMap<u64, PipeState>> = Mutex::new(BTreeMap::new());
static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(1);

pub fn create() -> PipeId {
    let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
    PIPES.lock().insert(id, PipeState::new());
    PipeId(id)
}

pub fn read(id: PipeId, buf: &mut [u8]) -> Result<usize, PipeError> {
    let mut pipes = PIPES.lock();
    let state = match pipes.get_mut(&id.0) {
        Some(state) => state,
        None => return Ok(0),
    };

    if state.buf.is_empty() {
        if state.writers == 0 {
            return Ok(0);
        }
        return Err(PipeError::WouldBlock);
    }

    let n = core::cmp::min(buf.len(), state.buf.len());
    for slot in buf.iter_mut().take(n) {
        *slot = state.buf.pop_front().expect("checked non-empty above");
    }
    Ok(n)
}

pub fn write(id: PipeId, buf: &[u8]) -> Result<usize, PipeError> {
    let mut pipes = PIPES.lock();
    let state = match pipes.get_mut(&id.0) {
        Some(state) => state,
        None => return Err(PipeError::BrokenPipe),
    };

    if state.readers == 0 {
        return Err(PipeError::BrokenPipe);
    }

    let space = PIPE_CAPACITY.saturating_sub(state.buf.len());
    if space == 0 {
        return Err(PipeError::WouldBlock);
    }

    let n = core::cmp::min(space, buf.len());
    state.buf.extend(buf[..n].iter().copied());
    Ok(n)
}

pub fn close_read_end(id: PipeId) {
    let mut pipes = PIPES.lock();
    if let Some(state) = pipes.get_mut(&id.0) {
        state.readers = state.readers.saturating_sub(1);
        if state.readers == 0 && state.writers == 0 {
            pipes.remove(&id.0);
        }
    }
}

pub fn close_write_end(id: PipeId) {
    let mut pipes = PIPES.lock();
    if let Some(state) = pipes.get_mut(&id.0) {
        state.writers = state.writers.saturating_sub(1);
        if state.readers == 0 && state.writers == 0 {
            pipes.remove(&id.0);
        }
    }
}
