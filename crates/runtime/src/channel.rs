//! Single-threaded channel primitives for compio runtime.
//!
//! These replace `tokio::sync::oneshot` and `tokio::sync::mpsc` with simple
//! `Rc<Cell>` / `Rc<RefCell>` based types. This works because compio is
//! single-threaded — no Send/Sync needed.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;

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
// StreamBuffer — waker-based stream for non-blocking chunk forwarding
// ---------------------------------------------------------------------------

struct StreamInner {
    chunks: VecDeque<Vec<u8>>,
    done: bool,
    waker: Option<Waker>,
}

/// Writer half of a shared stream buffer.
#[derive(Clone)]
pub struct StreamWriter {
    inner: Rc<RefCell<StreamInner>>,
}

impl StreamWriter {
    /// Push a chunk into the buffer and wake the reader.
    pub fn push(&self, data: Vec<u8>) {
        let mut inner = self.inner.borrow_mut();
        inner.chunks.push_back(data);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }

    /// Signal that no more chunks will be written. Wakes the reader.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.done = true;
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

/// Reader half of a shared stream buffer.
pub struct StreamReader {
    inner: Rc<RefCell<StreamInner>>,
}

impl StreamReader {
    /// Drain all available chunks from the buffer.
    pub fn drain(&self) -> Vec<Vec<u8>> {
        self.inner.borrow_mut().chunks.drain(..).collect()
    }

    /// Returns true if the writer has signalled completion.
    pub fn is_done(&self) -> bool {
        self.inner.borrow().done
    }

    /// Register a waker to be notified when data arrives or the stream closes.
    pub fn register_waker(&self, waker: &Waker) {
        self.inner.borrow_mut().waker = Some(waker.clone());
    }

    /// Returns true if there are chunks available to read.
    pub fn has_data(&self) -> bool {
        !self.inner.borrow().chunks.is_empty()
    }

    /// Wait until data is available or the stream is done.
    /// Uses waker-based notification — no polling.
    pub fn wait_for_data(&self) -> WaitForData<'_> {
        WaitForData { reader: self }
    }
}

/// Future that resolves when the StreamReader has data or is done.
pub struct WaitForData<'a> {
    reader: &'a StreamReader,
}

impl<'a> std::future::Future for WaitForData<'a> {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        let inner = self.reader.inner.borrow();
        if !inner.chunks.is_empty() || inner.done {
            std::task::Poll::Ready(())
        } else {
            drop(inner);
            self.reader.register_waker(cx.waker());
            std::task::Poll::Pending
        }
    }
}

/// Create a new stream buffer pair.
pub fn stream_buffer() -> (StreamWriter, StreamReader) {
    let inner = Rc::new(RefCell::new(StreamInner {
        chunks: VecDeque::new(),
        done: false,
        waker: None,
    }));
    (
        StreamWriter { inner: inner.clone() },
        StreamReader { inner },
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
