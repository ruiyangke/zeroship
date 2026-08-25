//! Buffered compio I/O for PostgreSQL connections.
//!
//! Wraps a compio stream (TCP, Unix, or a TLS-wrapped variant) with an 8KB
//! userspace read buffer so one Postgres message (1-byte tag + 4-byte
//! length + payload) doesn't translate into 3 separate io_uring SQEs.
//!
//! Two hardening properties are load-bearing and must survive any rewrite:
//! the length cap (which stops a malformed server from OOM-ing the driver;
//! 64 MB by default, see `Config::max_message_size`) and the zero-copy `BytesMut::split` flush path.

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
/// buffers up front (a 64 MB frame arriving in 16 KB chunks would cause
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
        self.inner
            .obligations
            .set(obligations.checked_add(1).expect("read obligation overflow"));
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
        let mut slot = self.inner.reader_waker.borrow_mut();
        if !slot.as_ref().is_some_and(|saved| saved.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }

    fn clear_reader(&self) {
        self.inner.reader_waker.borrow_mut().take();
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
    let Some(deadline) = deadline else {
        return Ok(reader.read(buffer).await);
    };

    deadline.begin_read();
    let _registration = ReaderRegistration(deadline);
    let mut read = std::pin::pin!(reader.read(buffer));
    let mut timer: Option<Pin<Box<dyn Future<Output = ()>>>> = None;
    let mut timer_for = None;

    poll_fn(|cx| {
        // Bytes win a same-poll race with the clock, matching
        // compio::time::timeout and treating real socket progress as progress.
        if let Poll::Ready(result) = read.as_mut().poll(cx) {
            deadline.finish_read();
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
    .await
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

    /// Consume the buffer, returning the underlying stream. Any unflushed
    /// writes and unparsed reads are discarded.
    #[allow(dead_code)]
    pub fn into_inner(self) -> S {
        self.inner
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
        if min_bytes > self.max_message_size {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "message too large: {min_bytes} bytes (max {})",
                    self.max_message_size
                ),
            )));
        }
        while self.read_buf.len() < min_bytes {
            if self.read_scratch.is_empty() {
                self.read_scratch = vec![0u8; READ_CHUNK];
            }
            let buf = std::mem::take(&mut self.read_scratch);
            let (n, buf) = self.read_raw(buf).await?;
            if n == 0 {
                return Err(Error::io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed by server",
                )));
            }
            self.read_buf.extend_from_slice(&buf[..n]);
            self.read_scratch = buf;
        }
        Ok(())
    }

    /// Access the read buffer for `Message::parse()` to consume from.
    pub fn buf(&mut self) -> &mut BytesMut {
        &mut self.read_buf
    }

    /// Peek a big-endian `u32` at `offset` in the read buffer without consuming.
    /// Returns `None` if fewer than 4 bytes are available starting at `offset`.
    pub fn peek_u32_be(&self, offset: usize) -> Option<u32> {
        let end = offset.checked_add(4)?;
        let slice = self.read_buf.get(offset..end)?;
        Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    /// Reject a framed message whose declared length (from the 4-byte length
    /// field, which itself counts its own 4 bytes but not the 1-byte tag)
    /// would exceed `DEFAULT_MAX_MESSAGE_SIZE`. O(1) - called before we buffer the
    /// payload, so a malicious server can't coerce us to allocate up to
    /// 64 MB per connection.
    pub fn validate_length(&self, length: u32) -> Result<(), Error> {
        let total = 1u64 + u64::from(length);
        if total > self.max_message_size as u64 {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "message too large: {total} bytes (max {})",
                    self.max_message_size
                ),
            )));
        }
        Ok(())
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
        if self.write_buf.is_empty() {
            return Ok(());
        }
        let data = self.write_buf.split();
        self.write_all_raw(data).await?;
        self.inner.flush().await.map_err(Error::io)?;
        Ok(())
    }

    /// Low-level read into owned buffer, returning (bytes_read, buffer).
    async fn read_raw(&mut self, buf: Vec<u8>) -> Result<(usize, Vec<u8>), Error> {
        let BufResult(result, returned) =
            read_with_deadline(&mut self.inner, buf, self.read_deadline.as_ref()).await?;
        let n = result.map_err(Error::io)?;
        Ok((n, returned))
    }

    /// Low-level write all bytes to the socket.
    async fn write_all_raw(&mut self, data: BytesMut) -> Result<(), Error> {
        let BufResult(result, _) = self.inner.write_all(data).await;
        result.map_err(Error::io)?;
        Ok(())
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
/// copy of it - they share it (`Rc<RefCell<..>>`) and reach it only through
/// synchronous helpers that never hold a borrow across an `await` (see
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

impl<R> BufReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn read_raw(&mut self, buf: Vec<u8>) -> Result<(usize, Vec<u8>), Error> {
        let BufResult(result, returned) =
            read_with_deadline(&mut self.inner, buf, self.read_deadline.as_ref()).await?;
        let n = result.map_err(Error::io)?;
        Ok((n, returned))
    }
}

impl<R> ReadFramer for BufReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
        if min_bytes > self.max_message_size {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "message too large: {min_bytes} bytes (max {})",
                    self.max_message_size
                ),
            )));
        }
        while self.read_buf.len() < min_bytes {
            if self.read_scratch.is_empty() {
                self.read_scratch = vec![0u8; READ_CHUNK];
            }
            let buf = std::mem::take(&mut self.read_scratch);
            let (n, buf) = self.read_raw(buf).await?;
            if n == 0 {
                return Err(Error::io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed by server",
                )));
            }
            self.read_buf.extend_from_slice(&buf[..n]);
            self.read_scratch = buf;
        }
        Ok(())
    }

    fn buf(&mut self) -> &mut BytesMut {
        &mut self.read_buf
    }

    fn peek_u32_be(&self, offset: usize) -> Option<u32> {
        let end = offset.checked_add(4)?;
        let slice = self.read_buf.get(offset..end)?;
        Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn validate_length(&self, length: u32) -> Result<(), Error> {
        let total = 1u64 + u64::from(length);
        if total > self.max_message_size as u64 {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "message too large: {total} bytes (max {})",
                    self.max_message_size
                ),
            )));
        }
        Ok(())
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
        if self.write_buf.is_empty() {
            return Ok(());
        }
        let data = self.write_buf.split();
        let BufResult(result, _) = self.inner.write_all(data).await;
        result.map_err(Error::io)?;
        self.inner.flush().await.map_err(Error::io)?;
        Ok(())
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
    /// across the split. The split decision depends SOLELY on whether the
    /// inner stream is splittable: a plain socket always splits, TLS never
    /// does. If the inner stream cannot be split (TLS), the `BufStream` is
    /// reconstructed and returned in `Err` so the caller can keep using the
    /// serialized loop.
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

    #[test]
    fn splitting_preserves_every_buffer_and_unsplittable_fallback_does_too() {
        let mut splittable = BufStream::new(ReadySplitIo);
        splittable.read_buf.extend_from_slice(b"buffered server bytes");
        splittable.write_buf.extend_from_slice(b"buffered client bytes");
        splittable.set_read_timeout(Some(Duration::from_secs(1)));

        let (read, write) = match splittable.try_into_split() {
            Ok(halves) => halves,
            Err(_) => panic!("splittable fixture took the fallback path"),
        };
        assert_eq!(&read.read_buf[..], b"buffered server bytes");
        assert_eq!(&write.write_buf[..], b"buffered client bytes");
        assert!(read.read_deadline.is_some());

        let mut unsplittable = BufStream::new(UnsplitIo);
        unsplittable.read_buf.extend_from_slice(b"fallback server bytes");
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
}
