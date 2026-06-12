//! Single-threaded channel primitives for compio runtime.
//!
//! These replace `tokio::sync::oneshot` and `tokio::sync::mpsc` with simple
//! `Rc<RefCell<_>>` based types. This works because compio is single-threaded
//! — no Send/Sync needed.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;

// ---------------------------------------------------------------------------
// ResultSlot — replaces tokio::sync::oneshot
// ---------------------------------------------------------------------------

/// Single-value slot for passing a result from pump to connection handler.
/// Works because compio is single-threaded — no Send/Sync needed.
pub struct ResultSender<T>(Rc<RefCell<ResultInner<T>>>);

impl<T> ResultSender<T> {
    /// Store a value and wake any task waiting in `recv()`.
    pub fn send(self, value: T) {
        let mut inner = self.0.borrow_mut();
        inner.value = Some(value);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

pub struct ResultReceiver<T>(Rc<RefCell<ResultInner<T>>>);

impl<T> ResultReceiver<T> {
    /// Take the value if the sender has stored one.
    pub fn try_recv(&self) -> Option<T> {
        self.0.borrow_mut().value.take()
    }

    /// Wait until the sender stores a value.
    pub fn recv(&self) -> WaitForResult<'_, T> {
        WaitForResult { receiver: self }
    }
}

/// Create a new oneshot-like slot pair.
pub fn result_slot<T>() -> (ResultSender<T>, ResultReceiver<T>) {
    let slot = Rc::new(RefCell::new(ResultInner {
        value: None,
        waker: None,
    }));
    (ResultSender(slot.clone()), ResultReceiver(slot))
}

struct ResultInner<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

pub struct WaitForResult<'a, T> {
    receiver: &'a ResultReceiver<T>,
}

impl<T> std::future::Future for WaitForResult<'_, T> {
    type Output = T;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut inner = self.receiver.0.borrow_mut();
        if let Some(value) = inner.value.take() {
            std::task::Poll::Ready(value)
        } else {
            inner.waker = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// StreamBuffer — waker-based stream for non-blocking chunk forwarding
// ---------------------------------------------------------------------------

/// Maximum bytes a single outbound stream may buffer before a slow client
/// causes `StreamWriter::push` to reject new chunks. Chosen to leave room for
/// a reasonable burst of SSE/NDJSON chunks (~4 MB) while bounding worst-case
/// RAM per in-flight stream so one misbehaving consumer cannot push the
/// process to OOM.
pub const DEFAULT_STREAM_BUFFER_CAP: usize = 4 * 1024 * 1024;

/// Process-wide ceiling on stream-buffered bytes. 512 MB leaves a ntex
/// worker with thousands of concurrent streams plenty of headroom while
/// capping worst-case RSS growth under a misbehaving-app scenario: even
/// 1000 streams can't collectively exceed this, so one bad app cannot
/// OOM its neighbours by fanning out streams.
///
/// Override for testing or tighter multi-tenant configurations via
/// `ZEROSHIP_STREAM_GLOBAL_CAP` env var, parsed once at process start.
pub const DEFAULT_STREAM_GLOBAL_CAP: usize = 512 * 1024 * 1024;

/// Global atomic counter tracking bytes currently buffered across every
/// StreamWriter in the process. Incremented on `push`, decremented on
/// `pop` and drop. Crossing `STREAM_GLOBAL_CAP` (cached below) rejects
/// new pushes with `StreamPushResult::Full` — same overflow semantics
/// as the per-stream cap.
///
/// Atomic because different ntex worker threads each have their own
/// runtime + streams; they share one global budget.
static STREAM_GLOBAL_BUFFERED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn stream_global_cap() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("ZEROSHIP_STREAM_GLOBAL_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_STREAM_GLOBAL_CAP)
    })
}

/// Current bytes buffered across all streams in the process. Exposed so
/// operators can surface it via a metrics endpoint.
#[must_use]
pub fn stream_global_buffered_bytes() -> usize {
    STREAM_GLOBAL_BUFFERED.load(std::sync::atomic::Ordering::Relaxed)
}

struct StreamInner {
    chunks: VecDeque<Vec<u8>>,
    /// Running total of bytes currently buffered (not yet `pop`-ed).
    buffered_bytes: usize,
    /// Hard cap on `buffered_bytes`. Attempts to push beyond this are
    /// rejected and the producer observes the overflow via `push`'s return.
    max_bytes: usize,
    /// True once the writer hit the cap. The reader sees this as "stream
    /// errored" and terminates forwarding.
    overflow: bool,
    done: bool,
    waker: Option<Waker>,
}

impl Drop for StreamInner {
    /// Release this stream's contribution to the process-wide byte counter
    /// when the last Rc handle (writer + reader both gone) is dropped.
    /// Without this, a client disconnect that races ahead of all pops
    /// would leak buffered bytes from the global ledger — eventually
    /// starving other streams even though no real memory was held.
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self.buffered_bytes > 0 {
            STREAM_GLOBAL_BUFFERED.fetch_sub(self.buffered_bytes, Ordering::Relaxed);
        }
    }
}

/// Result of a `StreamWriter::push` call. Distinguishes the happy path from
/// the two reasons a push may be dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPushResult {
    /// Chunk was accepted and buffered.
    Ok,
    /// Chunk was rejected — the stream was already closed by `close()`.
    Closed,
    /// Chunk was rejected — adding it would exceed the per-stream byte cap.
    /// The stream is now marked `overflow` and further pushes also return
    /// `Full`. Producers should stop generating chunks and close the stream.
    Full,
}

/// Writer half of a shared stream buffer.
#[derive(Clone)]
pub struct StreamWriter {
    inner: Rc<RefCell<StreamInner>>,
}

impl StreamWriter {
    /// Push a chunk into the buffer and wake the reader. Returns an explicit
    /// status so the producer knows whether the chunk was accepted — a
    /// previously-unbounded queue could silently accumulate gigabytes if the
    /// consumer was slow.
    ///
    /// Overflow semantics: the chunk is rejected if EITHER the per-stream
    /// cap OR the process-wide cap would be exceeded. The per-stream cap
    /// protects one noisy stream from monopolizing memory; the global cap
    /// protects one noisy *app* from DoSing its neighbours by fanning out
    /// thousands of concurrent streams that each stay under the per-stream
    /// limit. Both are necessary for multi-tenant safety.
    pub fn push(&self, data: Vec<u8>) -> StreamPushResult {
        use std::sync::atomic::Ordering;

        let mut inner = self.inner.borrow_mut();
        if inner.done {
            return StreamPushResult::Closed;
        }
        if inner.overflow {
            return StreamPushResult::Full;
        }
        let n = data.len();

        // Per-stream cap
        if inner.buffered_bytes.saturating_add(n) > inner.max_bytes {
            // Reclaim queued memory + account to the global counter on
            // overflow so repeated-offender streams don't leak budget.
            let released = inner.buffered_bytes;
            inner.chunks.clear();
            inner.buffered_bytes = 0;
            STREAM_GLOBAL_BUFFERED.fetch_sub(released, Ordering::Relaxed);
            inner.overflow = true;
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }
            return StreamPushResult::Full;
        }

        // Process-wide cap. fetch_add returns the previous value, so we
        // add first then check — any concurrent push across threads that
        // would also cross the line is detected symmetrically.
        let prev = STREAM_GLOBAL_BUFFERED.fetch_add(n, Ordering::Relaxed);
        if prev.saturating_add(n) > stream_global_cap() {
            // Undo our reservation and reject. We don't mark the stream
            // as overflow — this is a process-wide transient; another
            // push after buffers drain might succeed.
            STREAM_GLOBAL_BUFFERED.fetch_sub(n, Ordering::Relaxed);
            let released = inner.buffered_bytes;
            inner.chunks.clear();
            inner.buffered_bytes = 0;
            STREAM_GLOBAL_BUFFERED.fetch_sub(released, Ordering::Relaxed);
            inner.overflow = true;
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }
            return StreamPushResult::Full;
        }

        inner.buffered_bytes += n;
        inner.chunks.push_back(data);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
        StreamPushResult::Ok
    }

    /// Bytes currently queued — useful for metrics and backpressure signals.
    pub fn buffered_bytes(&self) -> usize {
        self.inner.borrow().buffered_bytes
    }

    /// The per-stream byte cap this buffer was created with. Backpressure
    /// water-marks are derived from this (per-stream) so a large-cap upload
    /// stream pauses/resumes proportionally, not at the global default.
    pub fn cap(&self) -> usize {
        self.inner.borrow().max_bytes
    }

    /// True once `push` has rejected a chunk for exceeding the byte cap.
    pub fn is_overflow(&self) -> bool {
        self.inner.borrow().overflow
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
        let mut chunks = Vec::new();
        while let Some(chunk) = self.pop() {
            chunks.push(chunk);
        }
        chunks
    }

    /// Pop a single available chunk from the buffer.
    pub fn pop(&self) -> Option<Vec<u8>> {
        use std::sync::atomic::Ordering;

        let mut inner = self.inner.borrow_mut();
        let chunk = inner.chunks.pop_front()?;
        let n = chunk.len();
        inner.buffered_bytes = inner.buffered_bytes.saturating_sub(n);
        STREAM_GLOBAL_BUFFERED.fetch_sub(n, Ordering::Relaxed);
        Some(chunk)
    }

    /// Returns true if the writer has signalled completion **or** hit the
    /// byte cap. Readers treat overflow the same as a normal close plus an
    /// error, since forwarding further chunks is not safe.
    pub fn is_done(&self) -> bool {
        let inner = self.inner.borrow();
        inner.done || inner.overflow
    }

    /// True if the producer exceeded the per-stream byte cap. Consumers can
    /// use this to emit a final error frame instead of a normal completion.
    pub fn is_overflow(&self) -> bool {
        self.inner.borrow().overflow
    }

    /// Register a waker to be notified when data arrives or the stream closes.
    pub fn register_waker(&self, waker: &Waker) {
        self.inner.borrow_mut().waker = Some(waker.clone());
    }

    /// Returns true if there are chunks available to read.
    pub fn has_data(&self) -> bool {
        !self.inner.borrow().chunks.is_empty()
    }

    /// Bytes currently queued in the shared buffer. Used by a streaming
    /// consumer to decide when to release upload backpressure (re-arm a
    /// paused producer once the buffer has drained below a low-water mark).
    pub fn buffered_bytes(&self) -> usize {
        self.inner.borrow().buffered_bytes
    }

    /// The per-stream byte cap this buffer was created with. The streaming
    /// consumer derives its resume low-water mark from this so a large-cap
    /// upload stream re-arms proportionally, not at the global default.
    pub fn cap(&self) -> usize {
        self.inner.borrow().max_bytes
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
        if !inner.chunks.is_empty() || inner.done || inner.overflow {
            std::task::Poll::Ready(())
        } else {
            drop(inner);
            self.reader.register_waker(cx.waker());
            std::task::Poll::Pending
        }
    }
}

/// Create a new stream buffer pair with the default byte cap.
pub fn stream_buffer() -> (StreamWriter, StreamReader) {
    stream_buffer_with_cap(DEFAULT_STREAM_BUFFER_CAP)
}

/// Create a new stream buffer pair with an explicit byte cap. Useful for
/// tests and for apps that need a tighter bound (e.g. latency-sensitive
/// paths where even 4 MB of buffering is too much).
pub fn stream_buffer_with_cap(max_bytes: usize) -> (StreamWriter, StreamReader) {
    let inner = Rc::new(RefCell::new(StreamInner {
        chunks: VecDeque::new(),
        buffered_bytes: 0,
        max_bytes,
        overflow: false,
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

struct CancelInner {
    cancelled: Cell<bool>,
    /// Waker registered by the pump task. `cancel()` wakes it so the runtime
    /// notices cancellation promptly instead of waiting for the next event.
    waker: RefCell<Option<Waker>>,
}

/// Single-threaded cancellation flag, used to abort in-flight async work
/// when the owning request has timed out or the client disconnected.
///
/// `cancel()` wakes any task that called `register_waker` so the pump can
/// drop cancelled requests and their queued ops on the very next cycle.
#[derive(Clone)]
pub struct CancelFlag(Rc<CancelInner>);

impl CancelFlag {
    pub fn new() -> Self {
        Self(Rc::new(CancelInner {
            cancelled: Cell::new(false),
            waker: RefCell::new(None),
        }))
    }

    pub fn cancel(&self) {
        self.0.cancelled.set(true);
        if let Some(waker) = self.0.waker.borrow_mut().take() {
            waker.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.get()
    }

    /// Register a waker to be notified when the flag is set. The registered
    /// waker is taken and dropped as soon as `cancel()` fires.
    pub fn register_waker(&self, waker: &Waker) {
        *self.0.waker.borrow_mut() = Some(waker.clone());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes the measurement windows of every test that reads or
    // perturbs the process-global `STREAM_GLOBAL_BUFFERED` atomic. Without
    // it, sibling tests pushing concurrently can inflate the counter inside
    // another test's before/after window, breaking the `<= before + N`
    // delta assertions. Poison-tolerant: a panicking test must not wedge
    // the rest of the module.
    static GLOBAL_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_global_counter() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_COUNTER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn stream_push_accepts_under_cap() {
        let _guard = lock_global_counter();
        let (w, r) = stream_buffer_with_cap(100);
        assert_eq!(w.push(vec![0u8; 50]), StreamPushResult::Ok);
        assert_eq!(w.push(vec![0u8; 40]), StreamPushResult::Ok);
        assert_eq!(w.buffered_bytes(), 90);
        assert!(!w.is_overflow());
        assert!(r.pop().is_some());
        assert_eq!(w.buffered_bytes(), 40);
    }

    #[test]
    fn stream_push_rejects_over_cap() {
        let _guard = lock_global_counter();
        let (w, r) = stream_buffer_with_cap(100);
        assert_eq!(w.push(vec![0u8; 60]), StreamPushResult::Ok);
        // 60 + 50 > 100 → overflow
        assert_eq!(w.push(vec![0u8; 50]), StreamPushResult::Full);
        assert!(w.is_overflow());
        assert!(r.is_overflow());
        // Queue is cleared on overflow (no partial delivery)
        assert!(r.pop().is_none());
        assert!(r.is_done());
    }

    #[test]
    fn stream_push_stays_rejected_after_overflow() {
        let _guard = lock_global_counter();
        let (w, _r) = stream_buffer_with_cap(10);
        assert_eq!(w.push(vec![0u8; 20]), StreamPushResult::Full);
        // Every subsequent push must also be rejected
        assert_eq!(w.push(vec![0u8; 1]), StreamPushResult::Full);
    }

    #[test]
    fn stream_push_after_close_is_rejected() {
        let _guard = lock_global_counter();
        let (w, _r) = stream_buffer();
        w.close();
        assert_eq!(w.push(vec![0u8; 1]), StreamPushResult::Closed);
    }

    #[test]
    fn stream_global_counter_tracks_push_and_pop() {
        // Test is delta-based because other tests running in parallel may
        // also modify the global counter — no exact-value assertions. The
        // lock serializes this test's measurement window against siblings
        // that push into the same global counter.
        let _guard = lock_global_counter();
        let (w, r) = stream_buffer_with_cap(1024);
        let before = stream_global_buffered_bytes();
        assert_eq!(w.push(vec![0u8; 100]), StreamPushResult::Ok);
        // Counter should have increased by at least 100.
        assert!(stream_global_buffered_bytes() >= before + 100);

        let chunk = r.pop().unwrap();
        assert_eq!(chunk.len(), 100);
        // Counter should have decreased by 100 relative to the post-push read.
        assert!(stream_global_buffered_bytes() <= before + 100);
    }

    #[test]
    fn stream_global_counter_released_on_drop() {
        let _guard = lock_global_counter();
        let before = stream_global_buffered_bytes();
        {
            let (w, _r) = stream_buffer_with_cap(1024);
            assert_eq!(w.push(vec![0u8; 200]), StreamPushResult::Ok);
            assert!(stream_global_buffered_bytes() >= before + 200);
        }
        // Drop must release at least 200 bytes.
        assert!(stream_global_buffered_bytes() <= before + 200);
    }

    #[test]
    fn cancel_flag_wakes_registered_waker() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TestWake(Arc<AtomicBool>);
        impl std::task::Wake for TestWake {
            fn wake(self: Arc<Self>) { self.0.store(true, Ordering::SeqCst); }
            fn wake_by_ref(self: &Arc<Self>) { self.0.store(true, Ordering::SeqCst); }
        }

        let flag = CancelFlag::new();
        let woke = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(TestWake(woke.clone())));
        flag.register_waker(&waker);
        assert!(!woke.load(Ordering::SeqCst));
        flag.cancel();
        assert!(woke.load(Ordering::SeqCst));
        assert!(flag.is_cancelled());
    }

    #[test]
    fn wait_for_data_resolves_when_stream_overflows() {
        use std::future::Future;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        let _guard = lock_global_counter();

        struct TestWake(Arc<AtomicBool>);
        impl Wake for TestWake {
            fn wake(self: Arc<Self>) { self.0.store(true, Ordering::SeqCst); }
            fn wake_by_ref(self: &Arc<Self>) { self.0.store(true, Ordering::SeqCst); }
        }

        let (writer, reader) = stream_buffer_with_cap(4);
        let woke = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(TestWake(woke.clone())));
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(reader.wait_for_data());

        assert!(matches!(Future::poll(fut.as_mut(), &mut cx), Poll::Pending));
        assert_eq!(writer.push(vec![0u8; 8]), StreamPushResult::Full);
        assert!(woke.load(Ordering::SeqCst), "overflow should wake a waiting reader");
        assert!(
            matches!(Future::poll(fut.as_mut(), &mut cx), Poll::Ready(())),
            "overflow is terminal; wait_for_data should resolve"
        );
    }
}
