//! Buffered compio I/O for PostgreSQL connections.
//!
//! Wraps a compio stream (TCP, Unix, or a TLS-wrapped variant) with an 8KB
//! userspace read buffer so one Postgres message (1-byte tag + 4-byte
//! length + payload) doesn't translate into 3 separate io_uring SQEs.
//!
//! Two hardening properties are load-bearing and must survive any rewrite:
//! the length cap (which stops a malformed server from OOM-ing the driver;
//! 64 MiB by default, see `Config::max_message_size`) and the zero-copy `BytesMut::split` flush path.

use crate::Error;
use bytes::BytesMut;
use compio::buf::{BufResult, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use std::cell::{Cell, RefCell};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

/// Read-side framing surface used by `codec::read_backend`.
///
/// Abstracts over the buffered read state so the decoder can run against
/// either a whole [`BufStream`] (serialized path) or an owned
/// [`BufReadHalf`] (multiplexed path). Every method operates only on the
/// userspace read buffer + the read side of the socket; it never touches
/// the write side, which is the property that makes a carried
/// `read_backend` future safe to hold across writes on a split stream.
pub(crate) trait ReadFramer {
    /// Ensure at least `min_bytes` are buffered, reading the socket if
    /// needed. Errors on EOF or oversize frame.
    async fn fill(&mut self, min_bytes: usize) -> Result<(), Error>;
    /// The buffered, not-yet-consumed read bytes.
    fn buf(&mut self) -> &mut BytesMut;
    /// Peek a big-endian u32 at `offset` without consuming.
    fn peek_u32_be(&self, offset: usize) -> Option<u32>;
    /// Reject a frame whose declared length exceeds `DEFAULT_MAX_MESSAGE_SIZE`.
    fn validate_length(&self, length: u32) -> Result<(), Error>;
}

/// Write-side framing surface used by `codec::write_frontend`.
///
/// Abstracts over the buffered write state so the encoder can run against
/// either a whole [`BufStream`] or an owned [`BufWriteHalf`]. Only the
/// write buffer is exposed; flushing is the caller's responsibility.
pub(crate) trait WriteFramer {
    /// Mutable access to the write buffer for in-place encoding.
    fn write_buf_mut(&mut self) -> &mut BytesMut;
}

/// Starting size of the read buffer. It GROWS to hold the largest single
/// message the connection has seen and is never shrunk back.
///
/// MEASURED 2026-08-25: one `SELECT repeat('x', 50MB)` takes the test
/// process from 6 MB to 56 MB of RSS, and it stays there after the row is
/// dropped and 50 further small queries run. tokio-postgres, measured the
/// same way in the same process, is identical (+50.1 MB, retained) - so this
/// is the shape a buffered driver has, not a divergence.
///
/// It is left alone deliberately. Shrinking after each message would trade a
/// reallocation on every large row for memory a workload with recurring large
/// rows is about to need again. What bounds it instead is connection
/// LIFETIME: `PoolConfig::max_lifetime` (30 minutes by default) rotates the
/// entry, and the allocation goes with it. A pool sized for peak memory
/// should therefore reckon on `max_size * largest expected message`, not on
/// the 8 KB below - and `Config::max_message_size` is the ceiling on that
/// term.
const READ_BUF_CAPACITY: usize = 8192;

/// Per-`fill()` syscall chunk size. We do not allocate `min_bytes`-sized
/// buffers up front (a 64 MiB frame arriving in 16 KB chunks would cause
/// ~260 GB of heap churn). Instead, each read pulls at most this many
/// bytes and the outer `while` loop issues as many reads as needed to
/// satisfy `min_bytes`.
const READ_CHUNK: usize = 16 * 1024;

/// Default maximum single-message size the driver accepts.
///
/// PostgreSQL's own limit is 1 GB (`PG_LARGE_SEND_MAX`), and this default sits
/// well below it on purpose: a length field is read BEFORE the body it
/// describes, so a malformed or malicious server claiming a multi-GB message
/// would otherwise have this driver allocate for it. Rejecting on the header
/// costs nothing and bounds that.
///
/// It is a DEFAULT rather than a ceiling, because a legal value the server
/// will happily send - a `bytea` or `text` column up to 1 GB - would
/// otherwise be permanently unreadable by this driver with no way to say
/// otherwise. `Config::max_message_size` raises it for callers who know their
/// rows are large and their server is not hostile.
pub(crate) const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Shared controller for one physical connection's socket-read inactivity
/// clock.
///
/// Plain sockets keep an owned read submitted even while the connection is
/// idle so LISTEN/NOTIFY remains prompt. A timer created unconditionally in
/// that task would therefore retire every healthy idle pool entry. The main
/// protocol loop registers only responses whose frontend bytes finished
/// flushing. Waking the reader lets an already-submitted idle read acquire a
/// deadline without cancelling and resubmitting it.
#[derive(Clone)]
pub(crate) struct ReadDeadline {
    inner: Rc<ReadDeadlineInner>,
}

struct ReadDeadlineInner {
    timeout: Duration,
    /// Successfully-flushed protocol phases for which the server still owes
    /// bytes. This is a count, not a boolean: pipelined requests must keep the
    /// clock active until every corresponding `ReadyForQuery` arrives.
    obligations: Cell<usize>,
    /// Deadline for the one currently submitted read. `None` while the
    /// connection is disarmed or while it is processing already-read bytes.
    deadline: Cell<Option<Instant>>,
    reader_waker: RefCell<Option<Waker>>,
}

impl ReadDeadline {
    fn new(timeout: Duration) -> Self {
        Self {
            inner: Rc::new(ReadDeadlineInner {
                timeout,
                obligations: Cell::new(0),
                deadline: Cell::new(None),
                reader_waker: RefCell::new(None),
            }),
        }
    }

    /// Register a response phase after its frontend bytes finish flushing.
    /// Only the zero-to-one transition starts the budget; pipelining another
    /// request cannot buy more time for a read already in flight.
    pub(crate) fn begin_response(&self) {
        let obligations = self.inner.obligations.get();
        self.inner.obligations.set(
            obligations
                .checked_add(1)
                .expect("read obligation overflow"),
        );
        if obligations == 0 {
            self.inner.deadline.set(self.next_deadline());
            self.wake_reader();
        }
    }

    /// Complete one response phase. The one-to-zero transition disarms an
    /// already-submitted read without cancelling it; it becomes an ordinary
    /// idle notification read.
    pub(crate) fn finish_response(&self) {
        let obligations = self.inner.obligations.get();
        debug_assert!(obligations > 0, "finished an unregistered read obligation");
        if obligations == 0 {
            return;
        }
        self.inner.obligations.set(obligations - 1);
        if obligations == 1 {
            self.inner.deadline.set(None);
            self.wake_reader();
        }
    }

    /// Start a fresh budget for an actual underlying read. Time spent parsing
    /// buffered frames or waiting for response-channel capacity is not a
    /// stalled socket read and must not consume this clock.
    fn begin_read(&self) {
        if self.inner.obligations.get() > 0 {
            self.inner.deadline.set(self.next_deadline());
        } else {
            self.inner.deadline.set(None);
        }
    }

    fn finish_read(&self) {
        self.inner.deadline.set(None);
    }

    fn current(&self) -> Option<Instant> {
        self.inner.deadline.get()
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.inner.timeout
    }

    fn next_deadline(&self) -> Option<Instant> {
        // A programmatic Duration can exceed Instant's representable range.
        // Treat that as an effectively unbounded policy instead of panicking
        // the connection task while it arms a read.
        Instant::now().checked_add(self.inner.timeout)
    }

    fn register_reader(&self, waker: &Waker) {
        // A valid custom RawWaker may run caller code from its clone vtable, so
        // clone before taking the mutable borrow. Re-check after the clone:
        // that caller code may itself have changed the registration.
        if self
            .inner
            .reader_waker
            .borrow()
            .as_ref()
            .is_some_and(|saved| saved.will_wake(waker))
        {
            return;
        }
        let cloned = waker.clone();

        // Carry the REPLACED waker out and destroy it with no borrow held.
        // Assigning through the `RefMut` drops the previous `Waker` while
        // `reader_waker` is still borrowed, and a `Waker` is arbitrary caller
        // code: a destructor that re-enters and touches this same cell - which
        // `wake_reader` and `clear_reader` both do - meets `BorrowMutError`.
        //
        // `wake_reader` below already takes the waker OUT before waking, for
        // exactly this reason. The drop is the same hazard reached through the
        // destructor instead of the wake, and it was the half this file missed.
        let replaced = {
            let mut slot = self.inner.reader_waker.borrow_mut();
            if slot.as_ref().is_some_and(|saved| saved.will_wake(&cloned)) {
                None
            } else {
                slot.replace(cloned)
            }
        };
        drop(replaced);
    }

    fn clear_reader(&self) {
        // Bound, not discarded inline. `borrow_mut().take();` drops the taken
        // `Waker` while the `RefMut` temporary is still alive - MEASURED with a
        // standalone probe on edition 2024, where the discarded-inline form
        // reports the cell as still borrowed inside the destructor and the
        // bound form does not. A `Waker` is arbitrary caller code, and a
        // destructor re-entering this cell meets `BorrowMutError`. Same rule as
        // `register_reader` above and `wake_reader` below.
        let cleared = self.inner.reader_waker.borrow_mut().take();
        drop(cleared);
    }

    fn wake_reader(&self) {
        // End the RefCell borrow before invoking an arbitrary waker. A custom
        // waker may poll immediately and register itself again.
        let waker = self.inner.reader_waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct ReaderRegistration<'a>(&'a ReadDeadline);

impl Drop for ReaderRegistration<'_> {
    fn drop(&mut self) {
        // Also runs when Connection teardown cancels a parked read task. Do not
        // leave its task waker retained by the shared controller.
        self.0.clear_reader();
    }
}

/// Drive one owned-buffer read while allowing the protocol loop to arm or
/// disarm its inactivity deadline around an already-submitted idle read.
async fn read_with_deadline<R, B>(
    reader: &mut R,
    buffer: B,
    deadline: Option<&ReadDeadline>,
) -> Result<BufResult<usize, B>, Error>
where
    R: AsyncRead + Unpin,
    B: IoBufMut,
{
    if let Some(deadline) = deadline {
        deadline.begin_read();
    }
    let _registration = deadline.map(ReaderRegistration);
    let mut buffer = buffer;

    loop {
        let BufResult(result, returned) = if let Some(deadline) = deadline {
            let mut read = std::pin::pin!(reader.read(buffer));
            let mut timer: Option<Pin<Box<dyn Future<Output = ()>>>> = None;
            let mut timer_for = None;

            poll_fn(|cx| {
                // Bytes win a same-poll race with the clock, matching
                // compio::time::timeout and treating real socket progress as progress.
                if let Poll::Ready(result) = read.as_mut().poll(cx) {
                    return Poll::Ready(Ok(result));
                }

                // The protocol loop may add an obligation to a read that began while
                // idle, or finish the last one at ReadyForQuery / CopyInResponse.
                // Retain the submitted read in both cases: cancelling it merely to
                // change clocks could discard bytes and desynchronise a connection
                // that was otherwise healthy.
                deadline.register_reader(cx.waker());
                let current = deadline.current();
                if current != timer_for {
                    timer = current.map(|at| {
                        Box::pin(compio::time::sleep_until(at)) as Pin<Box<dyn Future<Output = ()>>>
                    });
                    timer_for = current;
                }

                if let Some(timer) = timer.as_mut()
                    && timer.as_mut().poll(cx).is_ready()
                {
                    // This is the sole intentional cancellation of an in-flight read.
                    // A partial completion cannot be resumed, so the caller receives a
                    // terminal error and Connection::run retires the protocol session.
                    return Poll::Ready(Err(Error::read_timeout(deadline.timeout())));
                }

                Poll::Pending
            })
            .await?
        } else {
            reader.read(buffer).await
        };

        if result
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::Interrupted)
        {
            // Interrupted promises that this operation made no progress. Keep
            // the same owned buffer and, when armed, the same absolute read
            // deadline; reporting it would retire a still-aligned session.
            buffer = returned;
            continue;
        }
        if let Some(deadline) = deadline {
            deadline.finish_read();
        }
        return Ok(BufResult(result, returned));
    }
}

async fn read_raw_from<R>(
    reader: &mut R,
    buf: Vec<u8>,
    read_deadline: Option<&ReadDeadline>,
) -> Result<(usize, Vec<u8>), Error>
where
    R: AsyncRead + Unpin,
{
    let BufResult(result, returned) = read_with_deadline(reader, buf, read_deadline).await?;
    let n = result.map_err(Error::io)?;
    Ok((n, returned))
}

async fn fill_read_buffer<R>(
    reader: &mut R,
    read_buf: &mut BytesMut,
    read_scratch: &mut Vec<u8>,
    read_deadline: Option<&ReadDeadline>,
    max_message_size: usize,
    min_bytes: usize,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin,
{
    if min_bytes > max_message_size {
        return Err(Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message too large: {min_bytes} bytes (max {max_message_size})"),
        )));
    }
    while read_buf.len() < min_bytes {
        if read_scratch.is_empty() {
            *read_scratch = vec![0u8; READ_CHUNK];
        }
        let buf = std::mem::take(read_scratch);
        let (n, buf) = read_raw_from(reader, buf, read_deadline).await?;
        if n == 0 {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed by server",
            )));
        }
        read_buf.extend_from_slice(&buf[..n]);
        *read_scratch = buf;
    }
    Ok(())
}

fn peek_u32_be_from(read_buf: &BytesMut, offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = read_buf.get(offset..end)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn validate_length_against(length: u32, max_message_size: usize) -> Result<(), Error> {
    let total = 1u64 + u64::from(length);
    if total > max_message_size as u64 {
        return Err(Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message too large: {total} bytes (max {max_message_size})"),
        )));
    }
    Ok(())
}

async fn flush_retry_interrupted<W>(writer: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    loop {
        match writer.flush().await {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

async fn flush_write_buffer<W>(writer: &mut W, write_buf: &mut BytesMut) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    if write_buf.is_empty() {
        return Ok(());
    }
    let data = write_buf.split();
    let BufResult(result, _) = writer.write_all(data).await;
    result.map_err(Error::io)?;
    flush_retry_interrupted(writer).await.map_err(Error::io)?;
    Ok(())
}

/// Buffered read/write stream over any compio `AsyncRead + AsyncWrite`.
///
/// The stream is generic so the same wrapper works for plain sockets
/// (`Socket`) and TLS-wrapped variants (`MaybeTlsStream<Socket, _>`).
pub(crate) struct BufStream<S> {
    inner: S,
    read_buf: BytesMut,
    read_scratch: Vec<u8>,
    write_buf: BytesMut,
    read_deadline: Option<ReadDeadline>,
    max_message_size: usize,
}

impl<S> BufStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap a compio stream.
    pub fn new(stream: S) -> Self {
        Self {
            inner: stream,
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
            read_scratch: vec![0u8; READ_CHUNK],
            write_buf: BytesMut::with_capacity(1024),
            read_deadline: None,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }

    /// Raise or lower the largest single backend message this stream accepts.
    ///
    /// See [`DEFAULT_MAX_MESSAGE_SIZE`] for why there is a cap at all.
    pub(crate) fn set_max_message_size(&mut self, max: usize) {
        self.max_message_size = max;
    }

    /// Install a post-startup read deadline. Handshake reads remain owned by
    /// `connect_timeout`; callers invoke this only after startup completes.
    pub(crate) fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_deadline = timeout.map(ReadDeadline::new);
    }

    pub(crate) fn read_deadline(&self) -> Option<ReadDeadline> {
        self.read_deadline.clone()
    }

    pub(crate) fn begin_read_response(&self) {
        if let Some(deadline) = &self.read_deadline {
            deadline.begin_response();
        }
    }

    pub(crate) fn finish_read_response(&self) {
        if let Some(deadline) = &self.read_deadline {
            deadline.finish_response();
        }
    }

    /// Borrow the underlying stream mutably (used for TLS upgrade).
    #[allow(dead_code)]
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Ensure the read buffer has at least `min_bytes` available.
    /// Reads from the socket if needed. Returns error on EOF or I/O failure.
    ///
    /// Rejects requests larger than `DEFAULT_MAX_MESSAGE_SIZE` to prevent a
    /// malformed/malicious server from OOM-ing the process with a
    /// crafted 4-byte length field (e.g., 0xFFFFFFFF = 4 GB).
    pub async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
        fill_read_buffer(
            &mut self.inner,
            &mut self.read_buf,
            &mut self.read_scratch,
            self.read_deadline.as_ref(),
            self.max_message_size,
            min_bytes,
        )
        .await
    }

    /// Access the read buffer for `Message::parse()` to consume from.
    pub fn buf(&mut self) -> &mut BytesMut {
        &mut self.read_buf
    }

    /// Peek a big-endian `u32` at `offset` in the read buffer without consuming.
    /// Returns `None` if fewer than 4 bytes are available starting at `offset`.
    pub fn peek_u32_be(&self, offset: usize) -> Option<u32> {
        peek_u32_be_from(&self.read_buf, offset)
    }

    /// Reject a framed message whose declared length (from the 4-byte length
    /// field, which itself counts its own 4 bytes but not the 1-byte tag)
    /// would exceed `DEFAULT_MAX_MESSAGE_SIZE`. O(1) - called before we buffer the
    /// payload, so a malicious server can't coerce us to allocate up to
    /// 64 MiB per connection.
    pub fn validate_length(&self, length: u32) -> Result<(), Error> {
        validate_length_against(length, self.max_message_size)
    }

    /// Append data to the write buffer (no I/O until flush).
    #[allow(dead_code)]
    pub fn write(&mut self, data: &[u8]) {
        self.write_buf.extend_from_slice(data);
    }

    /// Mutable access to the write buffer - used by the codec layer to
    /// encode directly into the buffer, which is why there is no
    /// `write_bytes` helper: every caller encodes in place instead.
    pub fn write_buf_mut(&mut self) -> &mut BytesMut {
        &mut self.write_buf
    }

    /// Flush the write buffer to the socket.
    pub async fn flush(&mut self) -> Result<(), Error> {
        flush_write_buffer(&mut self.inner, &mut self.write_buf).await
    }
}

// The serialized path drives `read_backend` / `write_frontend` over the
// whole `BufStream`; the trait impls just forward to the inherent methods
// above so a single decoder/encoder works on both the unsplit stream and
// the split halves.
impl<S> ReadFramer for BufStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
        BufStream::fill(self, min_bytes).await
    }
    fn buf(&mut self) -> &mut BytesMut {
        BufStream::buf(self)
    }
    fn peek_u32_be(&self, offset: usize) -> Option<u32> {
        BufStream::peek_u32_be(self, offset)
    }
    fn validate_length(&self, length: u32) -> Result<(), Error> {
        BufStream::validate_length(self, length)
    }
}

impl<S> WriteFramer for BufStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn write_buf_mut(&mut self) -> &mut BytesMut {
        BufStream::write_buf_mut(self)
    }
}

/// A stream that can be torn into independently-owned read and write
/// halves, each pollable without aliasing the other.
///
/// Implemented for the plain socket (`Socket` -> compio `into_split`,
/// which `clone()`s a refcounted shared fd into two owned halves - one fd,
/// shared, NOT a `dup`) and for BOTH variants of `MaybeTlsStream`. The
/// plain-socket halves run concurrent `io_uring` submissions safely because
/// the kernel allows concurrent read+write SQEs on a single socket fd, and
/// that fd closes only when BOTH halves drop.
///
/// **TLS splits too, and why that is sound is worth stating.** rustls does
/// keep one session behind both directions, so the halves do not each get a
/// copy of it - they share it (`Arc<Mutex<..>>`) and reach it only through
/// synchronous helpers that never hold a mutex guard across an `await` (see
/// `tls_sansio`). What they own separately is the socket, which is the same
/// already-safe split as the plaintext case.
///
/// This paragraph used to claim the opposite, and the claim cost every TLS
/// connection the multiplexed loop: a transport was deciding which protocol
/// implementation ran, so `LISTEN` delivered nothing between queries over
/// TLS while working perfectly in plaintext.
///
/// Returning `Err(self)` remains a legitimate implementation, and the only
/// one available to a stream that genuinely cannot be torn in two: the
/// caller falls back to the serialized loop, which is correct but reads only
/// while a request is outstanding.
// `pub` because `TlsConnect::Stream` is bounded by it, so an out-of-crate
// TLS connector must be able to name and implement it. Re-exported at the
// crate root.
pub trait SplitStream: Sized {
    /// Owned read half (read side of the socket).
    type ReadHalf: AsyncRead + Unpin;
    /// Owned write half (write side of the socket).
    type WriteHalf: AsyncWrite + Unpin;
    /// Split into owned halves, or return `self` unchanged if this stream
    /// cannot be split.
    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self>;
}

/// Owned read half of a split [`BufStream`].
///
/// Carries the userspace `read_buf` (so any bytes already buffered-but-
/// unparsed at split time are not lost) plus the owned read side of the
/// socket. Implements [`ReadFramer`] so `codec::read_backend` runs
/// against it identically to the unsplit `BufStream`.
pub(crate) struct BufReadHalf<R> {
    inner: R,
    read_buf: BytesMut,
    read_scratch: Vec<u8>,
    read_deadline: Option<ReadDeadline>,
    max_message_size: usize,
}

impl<R> ReadFramer for BufReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
        fill_read_buffer(
            &mut self.inner,
            &mut self.read_buf,
            &mut self.read_scratch,
            self.read_deadline.as_ref(),
            self.max_message_size,
            min_bytes,
        )
        .await
    }

    fn buf(&mut self) -> &mut BytesMut {
        &mut self.read_buf
    }

    fn peek_u32_be(&self, offset: usize) -> Option<u32> {
        peek_u32_be_from(&self.read_buf, offset)
    }

    fn validate_length(&self, length: u32) -> Result<(), Error> {
        validate_length_against(length, self.max_message_size)
    }
}

/// Owned write half of a split [`BufStream`].
///
/// Carries the userspace `write_buf` plus the owned write side of the
/// socket. Implements [`WriteFramer`] (encode into the buffer) and
/// exposes `flush` / `shutdown` matching `BufStream`'s semantics.
pub(crate) struct BufWriteHalf<W> {
    inner: W,
    write_buf: BytesMut,
}

impl<W> BufWriteHalf<W>
where
    W: AsyncWrite + Unpin,
{
    /// Flush the write buffer to the socket. Mirrors [`BufStream::flush`].
    pub async fn flush(&mut self) -> Result<(), Error> {
        flush_write_buffer(&mut self.inner, &mut self.write_buf).await
    }

    /// Best-effort socket shutdown (TCP FIN). Mirrors the shutdown call in
    /// `BufStream::get_mut().shutdown()` on the serialized terminate path.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}

impl<W> WriteFramer for BufWriteHalf<W>
where
    W: AsyncWrite + Unpin,
{
    fn write_buf_mut(&mut self) -> &mut BytesMut {
        &mut self.write_buf
    }
}

impl<S> BufStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + SplitStream,
{
    /// Tear the buffered stream into owned read/write halves for the
    /// multiplexed connection loop.
    ///
    /// The userspace `read_buf` travels with the read half and the
    /// `write_buf` with the write half, so no buffered bytes are lost
    /// across the split. The split decision depends solely on whether the
    /// inner stream is splittable. If the inner stream cannot be split, the
    /// `BufStream` is reconstructed and returned in `Err` so the caller can
    /// keep using the serialized loop.
    ///
    /// A non-empty `write_buf` does NOT force the serialized fallback - those
    /// bytes are simply carried onto the new write half and flushed with the
    /// next frame by [`BufWriteHalf::flush`] (which prepends them). In
    /// practice the split is taken at an idle point right after the handshake,
    /// where `write_buf` is empty anyway.
    #[allow(clippy::type_complexity)]
    pub fn try_into_split(
        self,
    ) -> Result<
        (
            BufReadHalf<<S as SplitStream>::ReadHalf>,
            BufWriteHalf<<S as SplitStream>::WriteHalf>,
        ),
        Self,
    > {
        // Reconstructing on the unsplittable path needs the buffers back,
        // so destructure rather than move `self` into the split call.
        let BufStream {
            inner,
            read_buf,
            read_scratch,
            write_buf,
            read_deadline,
            max_message_size,
        } = self;
        match inner.try_into_split() {
            Ok((r, w)) => Ok((
                BufReadHalf {
                    inner: r,
                    read_buf,
                    read_scratch,
                    read_deadline,
                    max_message_size,
                },
                BufWriteHalf {
                    inner: w,
                    write_buf,
                },
            )),
            Err(inner) => Err(BufStream {
                inner,
                read_buf,
                read_scratch,
                write_buf,
                read_deadline,
                max_message_size,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compio::buf::{BufResult, IoBuf};
    use std::collections::VecDeque;

    struct ReadySplitIo;

    impl AsyncRead for ReadySplitIo {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(1), buf)
        }
    }

    impl AsyncWrite for ReadySplitIo {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SplitStream for ReadySplitIo {
        type ReadHalf = Self;
        type WriteHalf = Self;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Ok((Self, Self))
        }
    }

    struct UnsplitIo;

    struct OneByteReader {
        bytes: VecDeque<u8>,
        reads: usize,
    }

    struct ReadMustNotBePolled;

    struct ScratchProbeReader {
        reads: usize,
        scratch_was_reused: bool,
    }

    struct InterruptOnceReader {
        reads: usize,
    }

    struct InterruptThenObserveDeadline {
        reads: usize,
        deadline: ReadDeadline,
        sentinel: Instant,
        observed: Rc<Cell<Option<Instant>>>,
    }

    struct InterruptOnceWriter {
        flushes: usize,
    }

    #[derive(Default)]
    struct WriteProbe {
        writes: usize,
        flushes: usize,
        shutdowns: usize,
    }

    fn test_read_half<R>(inner: R) -> BufReadHalf<R>
    where
        R: AsyncRead + Unpin,
    {
        BufReadHalf {
            inner,
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
            read_scratch: vec![0u8; READ_CHUNK],
            read_deadline: None,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }

    impl AsyncRead for OneByteReader {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
            self.reads += 1;
            match self.bytes.pop_front() {
                Some(byte) => {
                    assert!(
                        buf.buf_len() > 0,
                        "the split reader received an empty scratch buffer"
                    );
                    buf.as_mut_slice()[0] = byte;
                    BufResult(Ok(1), buf)
                }
                None => BufResult(Ok(0), buf),
            }
        }
    }

    impl AsyncRead for ReadMustNotBePolled {
        async fn read<B: IoBufMut>(&mut self, _buf: B) -> BufResult<usize, B> {
            panic!("the split reader was polled past its configured message-size limit")
        }
    }

    impl AsyncRead for ScratchProbeReader {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
            const MARKER: u8 = 0xa5;

            assert_eq!(
                buf.buf_len(),
                READ_CHUNK,
                "the split reader received the wrong scratch-buffer size"
            );
            let byte = match self.reads {
                0 => {
                    buf.as_mut_slice()[READ_CHUNK - 1] = MARKER;
                    b'a'
                }
                1 => {
                    self.scratch_was_reused = buf.as_init()[READ_CHUNK - 1] == MARKER;
                    b'b'
                }
                _ => return BufResult(Ok(0), buf),
            };
            self.reads += 1;
            buf.as_mut_slice()[0] = byte;
            BufResult(Ok(1), buf)
        }
    }

    impl AsyncRead for InterruptOnceReader {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.reads += 1;
            if self.reads == 1 {
                BufResult(
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "scripted interrupted read",
                    )),
                    buf,
                )
            } else {
                BufResult(Ok(1), buf)
            }
        }
    }

    impl AsyncRead for InterruptThenObserveDeadline {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.reads += 1;
            if self.reads == 1 {
                self.deadline.inner.deadline.set(Some(self.sentinel));
                BufResult(
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "scripted interrupted read",
                    )),
                    buf,
                )
            } else {
                self.observed.set(self.deadline.current());
                BufResult(Ok(1), buf)
            }
        }
    }

    impl AsyncRead for InterruptOnceWriter {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for InterruptOnceWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = buf.buf_len();
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.flushes == 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "scripted interrupted flush",
                ))
            } else {
                Ok(())
            }
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncRead for WriteProbe {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for WriteProbe {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.writes += 1;
            let len = buf.buf_len();
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            self.shutdowns += 1;
            Ok(())
        }
    }

    impl AsyncRead for UnsplitIo {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for UnsplitIo {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SplitStream for UnsplitIo {
        type ReadHalf = ReadySplitIo;
        type WriteHalf = ReadySplitIo;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Err(self)
        }
    }

    #[compio::test]
    async fn ready_socket_bytes_win_a_same_poll_deadline_race() {
        compio::time::timeout(Duration::from_secs(1), async {
            let deadline = ReadDeadline::new(Duration::ZERO);
            deadline.begin_response();
            let mut reader = ReadySplitIo;

            let BufResult(result, _) =
                read_with_deadline(&mut reader, vec![0u8; 1], Some(&deadline))
                    .await
                    .expect("a ready read lost to its simultaneous deadline");
            assert_eq!(result.expect("the ready read failed"), 1);
        })
        .await
        .expect("same-poll read/deadline test exceeded its watchdog");
    }

    #[compio::test]
    async fn one_interrupted_read_is_retried_on_idle_and_timed_paths() {
        for timed in [false, true] {
            let deadline = ReadDeadline::new(Duration::from_secs(1));
            if timed {
                deadline.begin_response();
            }
            let mut reader = InterruptOnceReader { reads: 0 };

            let BufResult(result, _) =
                read_with_deadline(&mut reader, vec![0u8; 1], timed.then_some(&deadline))
                    .await
                    .expect("the read-deadline layer failed");
            assert_eq!(
                result.map_err(|error| error.kind()),
                Ok(1),
                "one interrupted read was treated as terminal"
            );
            assert_eq!(reader.reads, 2, "the interrupted read was not retried");
        }
    }

    #[compio::test]
    async fn retrying_an_interrupted_read_keeps_the_original_deadline() {
        let deadline = ReadDeadline::new(Duration::from_secs(60));
        deadline.begin_response();
        let sentinel = Instant::now();
        let observed = Rc::new(Cell::new(None));
        let mut reader = InterruptThenObserveDeadline {
            reads: 0,
            deadline: deadline.clone(),
            sentinel,
            observed: Rc::clone(&observed),
        };

        let BufResult(result, _) = read_with_deadline(&mut reader, vec![0u8; 1], Some(&deadline))
            .await
            .expect("the read-deadline layer failed");
        assert_eq!(result.expect("the retried read failed"), 1);
        assert_eq!(
            observed.get(),
            Some(sentinel),
            "retrying an interrupted read restarted its deadline"
        );
    }

    #[compio::test]
    async fn one_interrupted_flush_is_retried_on_serialized_and_split_paths() {
        let mut stream = BufStream::new(InterruptOnceWriter { flushes: 0 });
        stream.write(b"serialized request");
        assert!(
            stream.flush().await.is_ok(),
            "one interrupted flush was treated as terminal"
        );
        assert_eq!(stream.inner.flushes, 2, "serialized flush was not retried");

        let mut write_half = BufWriteHalf {
            inner: InterruptOnceWriter { flushes: 0 },
            write_buf: BytesMut::from(&b"split request"[..]),
        };
        assert!(
            write_half.flush().await.is_ok(),
            "one interrupted flush was treated as terminal"
        );
        assert_eq!(write_half.inner.flushes, 2, "split flush was not retried");
    }

    #[compio::test]
    async fn empty_flush_skips_transport_on_serialized_and_split_paths() {
        let mut stream = BufStream::new(WriteProbe::default());
        stream.flush().await.expect("flush the serialized stream");
        assert_eq!(stream.inner.writes, 0, "serialized empty flush wrote");
        assert_eq!(stream.inner.flushes, 0, "serialized empty flush flushed");

        let mut write_half = BufWriteHalf {
            inner: WriteProbe::default(),
            write_buf: BytesMut::new(),
        };
        write_half.flush().await.expect("flush the split stream");
        assert_eq!(write_half.inner.writes, 0, "split empty flush wrote");
        assert_eq!(write_half.inner.flushes, 0, "split empty flush flushed");
    }

    #[compio::test]
    async fn successful_flush_drains_serialized_and_split_write_buffers() {
        let mut stream = BufStream::new(WriteProbe::default());
        stream.write(b"serialized request");
        stream.flush().await.expect("flush the serialized stream");
        assert!(
            stream.write_buf.is_empty(),
            "serialized buffer was retained"
        );
        assert_eq!(
            stream.inner.writes, 1,
            "serialized bytes were not written once"
        );
        assert_eq!(
            stream.inner.flushes, 1,
            "serialized transport was not flushed once"
        );

        let mut write_half = BufWriteHalf {
            inner: WriteProbe::default(),
            write_buf: BytesMut::from(&b"split request"[..]),
        };
        write_half.flush().await.expect("flush the split stream");
        assert!(write_half.write_buf.is_empty(), "split buffer was retained");
        assert_eq!(
            write_half.inner.writes, 1,
            "split bytes were not written once"
        );
        assert_eq!(
            write_half.inner.flushes, 1,
            "split transport was not flushed once"
        );
    }

    #[compio::test]
    async fn split_shutdown_delegates_to_transport() {
        let mut write_half = BufWriteHalf {
            inner: WriteProbe::default(),
            write_buf: BytesMut::new(),
        };

        write_half
            .shutdown()
            .await
            .expect("shut down the split stream");

        assert_eq!(
            write_half.inner.shutdowns, 1,
            "split shutdown was not delegated"
        );
    }

    #[test]
    fn splitting_preserves_every_buffer_and_unsplittable_fallback_does_too() {
        let mut splittable = BufStream::new(ReadySplitIo);
        splittable
            .read_buf
            .extend_from_slice(b"buffered server bytes");
        splittable
            .write_buf
            .extend_from_slice(b"buffered client bytes");
        splittable.set_read_timeout(Some(Duration::from_secs(1)));

        let (read, write) = match splittable.try_into_split() {
            Ok(halves) => halves,
            Err(_) => panic!("splittable fixture took the fallback path"),
        };
        assert_eq!(&read.read_buf[..], b"buffered server bytes");
        assert_eq!(&write.write_buf[..], b"buffered client bytes");
        assert!(read.read_deadline.is_some());

        let mut unsplittable = BufStream::new(UnsplitIo);
        unsplittable
            .read_buf
            .extend_from_slice(b"fallback server bytes");
        unsplittable
            .write_buf
            .extend_from_slice(b"fallback client bytes");
        unsplittable.set_read_timeout(Some(Duration::from_secs(1)));

        let rebuilt = match unsplittable.try_into_split() {
            Ok(_) => panic!("unsplittable fixture unexpectedly split"),
            Err(stream) => stream,
        };
        assert_eq!(&rebuilt.read_buf[..], b"fallback server bytes");
        assert_eq!(&rebuilt.write_buf[..], b"fallback client bytes");
        assert!(rebuilt.read_deadline.is_some());
    }

    #[compio::test]
    async fn split_fill_accumulates_partial_reads() {
        compio::time::timeout(Duration::from_secs(1), async {
            let mut read = test_read_half(OneByteReader {
                bytes: VecDeque::from([b'a', b'b', b'c']),
                reads: 0,
            });

            read.fill(3)
                .await
                .expect("the split read half failed to fill from partial reads");

            assert_eq!(read.inner.reads, 3, "fill returned after a partial read");
            assert_eq!(&read.read_buf[..], b"abc");
        })
        .await
        .expect("split partial-read fill test exceeded its watchdog");
    }

    #[compio::test]
    async fn split_fill_enforces_configured_max_message_size_before_reading() {
        compio::time::timeout(Duration::from_secs(1), async {
            let mut read = test_read_half(ReadMustNotBePolled);
            read.max_message_size = 8;

            let error = read
                .fill(9)
                .await
                .expect_err("the split read half accepted a fill above its configured limit");
            let source = error
                .into_source()
                .expect("the split fill limit error must preserve its I/O cause");
            let io = source
                .downcast_ref::<std::io::Error>()
                .expect("the split fill limit must be an I/O error");
            assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(io.to_string(), "message too large: 9 bytes (max 8)");
        })
        .await
        .expect("split fill-limit test exceeded its watchdog");
    }

    #[test]
    fn split_length_validation_enforces_configured_max_message_size() {
        let mut read = test_read_half(ReadMustNotBePolled);
        read.max_message_size = 8;

        read.validate_length(7)
            .expect("a split frame exactly at the configured limit was rejected");
        let error = read
            .validate_length(8)
            .expect_err("a split frame above the configured limit was accepted");
        let source = error
            .into_source()
            .expect("the split frame limit error must preserve its I/O cause");
        let io = source
            .downcast_ref::<std::io::Error>()
            .expect("the split frame limit must be an I/O error");
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(io.to_string(), "message too large: 9 bytes (max 8)");
    }

    #[compio::test]
    async fn split_fill_reuses_its_read_scratch_allocation() {
        compio::time::timeout(Duration::from_secs(1), async {
            let mut read = test_read_half(ScratchProbeReader {
                reads: 0,
                scratch_was_reused: false,
            });

            read.fill(1)
                .await
                .expect("the first split scratch-probe read failed");
            assert_eq!(&read.read_buf[..], b"a");
            read.read_buf.clear();
            read.fill(1)
                .await
                .expect("the second split scratch-probe read failed");

            assert_eq!(&read.read_buf[..], b"b");
            assert_eq!(read.inner.reads, 2);
            assert!(
                read.inner.scratch_was_reused,
                "the split read half replaced its scratch allocation between fills"
            );
        })
        .await
        .expect("split scratch-reuse test exceeded its watchdog");
    }

    #[test]
    fn pipelined_obligations_keep_the_first_budget_until_the_last_response() {
        let deadline = ReadDeadline::new(Duration::from_secs(60));

        deadline.begin_response();
        assert_eq!(deadline.inner.obligations.get(), 1);
        assert!(deadline.current().is_some());

        // Use a deterministic marker rather than relying on clock resolution:
        // resetting the budget cannot reproduce this exact instant.
        let first_budget = Instant::now();
        deadline.inner.deadline.set(Some(first_budget));

        deadline.begin_response();
        assert_eq!(deadline.inner.obligations.get(), 2);
        assert_eq!(
            deadline.current(),
            Some(first_budget),
            "a pipelined response extended the existing read budget"
        );

        deadline.finish_response();
        assert_eq!(deadline.inner.obligations.get(), 1);
        assert_eq!(
            deadline.current(),
            Some(first_budget),
            "the first pipelined completion disarmed the remaining obligation"
        );

        deadline.finish_response();
        assert_eq!(deadline.inner.obligations.get(), 0);
        assert_eq!(deadline.current(), None);
    }

    #[test]
    fn unrepresentable_read_timeout_is_effectively_unbounded() {
        let deadline = ReadDeadline::new(Duration::MAX);

        deadline.begin_response();
        assert_eq!(deadline.inner.obligations.get(), 1);
        assert_eq!(deadline.current(), None);

        // Cover the actual-underlying-read arm as well as response setup.
        deadline.begin_read();
        assert_eq!(deadline.current(), None);
    }

    #[test]
    fn dropping_a_reader_registration_releases_its_waker() {
        let deadline = ReadDeadline::new(Duration::from_secs(60));
        let waker = Waker::noop();

        {
            let _registration = ReaderRegistration(&deadline);
            deadline.register_reader(waker);
            assert!(deadline.inner.reader_waker.borrow().is_some());
        }

        assert!(
            deadline.inner.reader_waker.borrow().is_none(),
            "a cancelled parked read left its task waker retained"
        );
    }

    thread_local! {
        /// The deadline whose reader slot the probe below interrogates.
        static READER_PROBE: RefCell<Option<ReadDeadline>> = const { RefCell::new(None) };
        /// `Some(true)` means the slot was STILL borrowed while a replaced
        /// waker was destroyed. `None` means the destructor never ran, which
        /// must not read as success.
        static READER_DROP_BORROWED: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
        static READER_CLONE_BORROWED: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
    }

    struct ReaderDropProbe;

    impl std::task::Wake for ReaderDropProbe {
        fn wake(self: std::sync::Arc<Self>) {}
        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }

    impl Drop for ReaderDropProbe {
        fn drop(&mut self) {
            READER_PROBE.with(|deadline| {
                if let Some(deadline) = deadline.borrow().as_ref() {
                    READER_DROP_BORROWED.with(|flag| {
                        flag.set(Some(deadline.inner.reader_waker.try_borrow_mut().is_err()));
                    });
                }
            });
        }
    }

    #[allow(unsafe_code)]
    mod reader_clone_probe {
        use super::*;

        fn raw_waker() -> std::task::RawWaker {
            unsafe fn clone(_: *const ()) -> std::task::RawWaker {
                READER_PROBE.with(|deadline| {
                    if let Some(deadline) = deadline.borrow().as_ref() {
                        READER_CLONE_BORROWED.with(|flag| {
                            flag.set(Some(deadline.inner.reader_waker.try_borrow_mut().is_err()));
                        });
                    }
                });
                raw_waker()
            }

            unsafe fn wake(_: *const ()) {}
            unsafe fn wake_by_ref(_: *const ()) {}
            unsafe fn drop(_: *const ()) {}

            static VTABLE: std::task::RawWakerVTable =
                std::task::RawWakerVTable::new(clone, wake, wake_by_ref, drop);
            std::task::RawWaker::new(std::ptr::null(), &VTABLE)
        }

        pub(super) fn waker() -> Waker {
            // The vtable owns no data, all four operations preserve that
            // invariant, and its static functions are safe on any thread.
            unsafe { Waker::from_raw(raw_waker()) }
        }
    }

    #[test]
    fn cloning_the_reader_waker_does_not_borrow_its_slot() {
        let deadline = ReadDeadline::new(std::time::Duration::from_secs(1));
        deadline.register_reader(Waker::noop());
        READER_PROBE.with(|cell| *cell.borrow_mut() = Some(deadline.clone()));
        READER_CLONE_BORROWED.with(|flag| flag.set(None));

        let probe = reader_clone_probe::waker();
        deadline.register_reader(&probe);

        let observed = READER_CLONE_BORROWED.with(std::cell::Cell::get);
        READER_PROBE.with(|cell| *cell.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "the caller waker's clone vtable ran while its reader slot was borrowed"
        );
    }

    /// Replacing the reader waker must not DESTROY the old one inside the
    /// borrow.
    ///
    /// `register_reader` assigned through the `RefMut`, which drops the
    /// previous `Waker` while `reader_waker` is still borrowed. A `Waker` is
    /// arbitrary caller code, and a destructor that re-enters this cell - which
    /// `wake_reader` and `clear_reader` both touch - meets `BorrowMutError`.
    ///
    /// `wake_reader` in this same file already takes the waker OUT before
    /// waking, with the reason spelled out. This is that hazard reached through
    /// the destructor rather than the wake, and it was the half that was missed.
    ///
    /// The assertion is on the BORROW rather than on a panic, so it states the
    /// invariant instead of one caller's way of tripping over it. `None` fails
    /// deliberately: a destructor that never ran proves nothing.
    #[test]
    fn replacing_the_reader_waker_does_not_destroy_it_inside_the_borrow() {
        let deadline = ReadDeadline::new(std::time::Duration::from_secs(1));
        let probe = Waker::from(std::sync::Arc::new(ReaderDropProbe));
        deadline.register_reader(&probe);

        READER_PROBE.with(|cell| *cell.borrow_mut() = Some(deadline.clone()));
        READER_DROP_BORROWED.with(|flag| flag.set(None));
        // Hand the slot the last `Arc`, so the replacement below runs the
        // destructor rather than merely decrementing a count.
        drop(probe);

        let other = Waker::from(std::sync::Arc::new(ReaderDropProbe));
        deadline.register_reader(&other);

        let observed = READER_DROP_BORROWED.with(std::cell::Cell::get);
        READER_PROBE.with(|cell| *cell.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "the replaced reader waker was destroyed while its slot was still \
             borrowed (None means the destructor never ran at all)"
        );
    }

    /// `clear_reader` must not destroy the taken waker inside the borrow
    /// either.
    ///
    /// `borrow_mut().take();` discards the taken `Waker` as an unbound
    /// temporary, and it is destroyed while the `RefMut` is still alive.
    /// MEASURED with a standalone probe on edition 2024: the discarded-inline
    /// form reports the cell as still borrowed inside the destructor, and
    /// binding it first does not. Same invariant as `register_reader`, reached
    /// through the other function that empties this slot.
    #[test]
    fn clearing_the_reader_waker_does_not_destroy_it_inside_the_borrow() {
        let deadline = ReadDeadline::new(std::time::Duration::from_secs(1));
        let probe = Waker::from(std::sync::Arc::new(ReaderDropProbe));
        deadline.register_reader(&probe);

        READER_PROBE.with(|cell| *cell.borrow_mut() = Some(deadline.clone()));
        READER_DROP_BORROWED.with(|flag| flag.set(None));
        // The slot owns the last `Arc`, so clearing runs the destructor.
        drop(probe);

        deadline.clear_reader();

        let observed = READER_DROP_BORROWED.with(std::cell::Cell::get);
        READER_PROBE.with(|cell| *cell.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "the cleared reader waker was destroyed while its slot was still \
             borrowed (None means the destructor never ran at all)"
        );
    }
}
