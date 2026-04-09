//! Single-threaded channel primitives for compio runtime.
//!
//! These replace `tokio::sync::oneshot` and `tokio::sync::mpsc` with simple
//! `Rc<Cell>` / `Rc<RefCell>` based types. This works because compio is
//! single-threaded — no Send/Sync needed.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// ResultSlot — replaces tokio::sync::oneshot
// ---------------------------------------------------------------------------

/// Single-value slot for passing a result from pump to connection handler.
/// Works because compio is single-threaded — no Send/Sync needed.
pub struct ResultSender<T>(Rc<Cell<Option<T>>>);

impl<T> ResultSender<T> {
    /// Store a value. The receiver will see it on next `try_recv()`.
    pub fn send(self, value: T) {
        self.0.set(Some(value));
    }
}

pub struct ResultReceiver<T>(Rc<Cell<Option<T>>>);

impl<T> ResultReceiver<T> {
    /// Take the value if the sender has stored one.
    pub fn try_recv(&self) -> Option<T> {
        self.0.take()
    }
}

/// Create a new oneshot-like slot pair.
pub fn result_slot<T>() -> (ResultSender<T>, ResultReceiver<T>) {
    let slot = Rc::new(Cell::new(None));
    (ResultSender(slot.clone()), ResultReceiver(slot))
}

// ---------------------------------------------------------------------------
// StreamBuffer — replaces tokio::sync::mpsc for streaming body chunks
// ---------------------------------------------------------------------------

/// Writer half of a shared stream buffer.
pub struct StreamWriter {
    chunks: Rc<RefCell<VecDeque<Vec<u8>>>>,
    done: Rc<Cell<bool>>,
}

impl StreamWriter {
    /// Push a chunk into the buffer.
    pub fn push(&self, data: Vec<u8>) {
        self.chunks.borrow_mut().push_back(data);
    }

    /// Signal that no more chunks will be written.
    pub fn close(&self) {
        self.done.set(true);
    }
}

/// Reader half of a shared stream buffer.
pub struct StreamReader {
    chunks: Rc<RefCell<VecDeque<Vec<u8>>>>,
    done: Rc<Cell<bool>>,
}

impl StreamReader {
    /// Drain all available chunks from the buffer.
    pub fn drain(&self) -> Vec<Vec<u8>> {
        self.chunks.borrow_mut().drain(..).collect()
    }

    /// Returns true if the writer has signalled completion.
    pub fn is_done(&self) -> bool {
        self.done.get()
    }
}

/// Create a new stream buffer pair.
pub fn stream_buffer() -> (StreamWriter, StreamReader) {
    let chunks = Rc::new(RefCell::new(VecDeque::new()));
    let done = Rc::new(Cell::new(false));
    (
        StreamWriter { chunks: chunks.clone(), done: done.clone() },
        StreamReader { chunks, done },
    )
}

// ---------------------------------------------------------------------------
// CancelFlag — replaces tokio_util::sync::CancellationToken
// ---------------------------------------------------------------------------

/// Simple boolean flag for cancellation. Single-threaded only.
#[derive(Clone)]
pub struct CancelFlag(Rc<Cell<bool>>);

impl CancelFlag {
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    pub fn cancel(&self) {
        self.0.set(true);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.get()
    }
}
