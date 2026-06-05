//! Buffered compio I/O for PostgreSQL connections.
//!
//! Wraps a compio stream (TCP, Unix, or a TLS-wrapped variant) with an 8KB
//! userspace read buffer so one Postgres message (1-byte tag + 4-byte
//! length + payload) doesn't translate into 3 separate io_uring SQEs.
//!
//! Ported from the hardened `crates/pg/src/stream.rs`. Preserves the 64 MB
//! length cap (to stop a malformed server from OOM-ing the driver) and the
//! zero-copy `BytesMut::split` flush path.

use crate::Error;
use bytes::BytesMut;
use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

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
    /// Reject a frame whose declared length exceeds `MAX_MESSAGE_SIZE`.
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

const READ_BUF_CAPACITY: usize = 8192;

/// Per-`fill()` syscall chunk size. We do not allocate `min_bytes`-sized
/// buffers up front (a 64 MB frame arriving in 16 KB chunks would cause
/// ~260 GB of heap churn). Instead, each read pulls at most this many
/// bytes and the outer `while` loop issues as many reads as needed to
/// satisfy `min_bytes`.
const READ_CHUNK: usize = 16 * 1024;

/// Maximum single-message size the driver will accept. PostgreSQL's own
/// limit is 1 GB (`PG_LARGE_SEND_MAX`), but for a platform where apps
/// execute structured CRUD queries (not bulk COPY), 64 MB is generous.
/// A malformed or malicious server sending a message header with a
/// multi-GB length field will be rejected here instead of OOM-ing the
/// worker.
pub(crate) const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Buffered read/write stream over any compio `AsyncRead + AsyncWrite`.
///
/// The stream is generic so the same wrapper works for plain sockets
/// (`Socket`) and TLS-wrapped variants (`MaybeTlsStream<Socket, _>`).
pub(crate) struct BufStream<S> {
    inner: S,
    read_buf: BytesMut,
    write_buf: BytesMut,
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
            write_buf: BytesMut::with_capacity(1024),
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
    /// Rejects requests larger than `MAX_MESSAGE_SIZE` to prevent a
    /// malformed/malicious server from OOM-ing the process with a
    /// crafted 4-byte length field (e.g., 0xFFFFFFFF = 4 GB).
    pub async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
        if min_bytes > MAX_MESSAGE_SIZE {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message too large: {min_bytes} bytes (max {MAX_MESSAGE_SIZE})"),
            )));
        }
        while self.read_buf.len() < min_bytes {
            // Always allocate a fixed, small chunk — NEVER `min_bytes`-sized.
            // A 64 MB frame drip-fed in 16 KB chunks must not translate to
            // 4096 × 64 MB heap allocations. `extend_from_slice` amortises
            // the `read_buf` growth; `READ_CHUNK` keeps each syscall bounded.
            let buf = vec![0u8; READ_CHUNK];
            let (n, buf) = self.read_raw(buf).await?;
            if n == 0 {
                return Err(Error::io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed by server",
                )));
            }
            self.read_buf.extend_from_slice(&buf[..n]);
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
    /// would exceed `MAX_MESSAGE_SIZE`. O(1) — called before we buffer the
    /// payload, so a malicious server can't coerce us to allocate up to
    /// 64 MB per connection.
    pub fn validate_length(&self, length: u32) -> Result<(), Error> {
        let total = 1u64 + u64::from(length);
        if total > MAX_MESSAGE_SIZE as u64 {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message too large: {total} bytes (max {MAX_MESSAGE_SIZE})"),
            )));
        }
        Ok(())
    }

    /// Read exactly one byte from the stream (used for SSL negotiation response).
    pub async fn read_byte(&mut self) -> Result<u8, Error> {
        self.fill(1).await?;
        Ok(self.read_buf.split_to(1)[0])
    }

    /// Append data to the write buffer (no I/O until flush).
    #[allow(dead_code)]
    pub fn write(&mut self, data: &[u8]) {
        self.write_buf.extend_from_slice(data);
    }

    /// Append a BytesMut to the write buffer (no I/O until flush).
    pub fn write_bytes(&mut self, data: &BytesMut) {
        self.write_buf.extend_from_slice(data);
    }

    /// Mutable access to the write buffer — used by the codec layer to
    /// encode directly into the buffer (avoids a copy vs. building a
    /// temporary BytesMut and calling `write_bytes`).
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
        let BufResult(result, returned) = self.inner.read(buf).await;
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
/// Implemented for the plain socket (`Socket` → compio `into_split`,
/// which dups the fd into two owned halves) and for
/// `MaybeTlsStream::Raw`. The TLS variant deliberately does **not**
/// split — rustls keeps shared session state behind the read and write
/// directions, so two owned halves cannot safely run concurrent
/// io_uring submissions against it. `try_into_split` therefore returns
/// the stream back unchanged for any unsplittable case, letting the
/// caller fall back to the serialized loop.
// `pub` (not `pub(crate)`) so it can appear in the bounds of the public
// `Connection::run` without tripping `private_bounds`. The enclosing
// `mod buf_stream` is private, so this is not actually part of the crate's
// external API.
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
}

impl<R> BufReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn read_raw(&mut self, buf: Vec<u8>) -> Result<(usize, Vec<u8>), Error> {
        let BufResult(result, returned) = self.inner.read(buf).await;
        let n = result.map_err(Error::io)?;
        Ok((n, returned))
    }
}

impl<R> ReadFramer for BufReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
        if min_bytes > MAX_MESSAGE_SIZE {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message too large: {min_bytes} bytes (max {MAX_MESSAGE_SIZE})"),
            )));
        }
        while self.read_buf.len() < min_bytes {
            let buf = vec![0u8; READ_CHUNK];
            let (n, buf) = self.read_raw(buf).await?;
            if n == 0 {
                return Err(Error::io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed by server",
                )));
            }
            self.read_buf.extend_from_slice(&buf[..n]);
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
        if total > MAX_MESSAGE_SIZE as u64 {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message too large: {total} bytes (max {MAX_MESSAGE_SIZE})"),
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
    /// across the split. If the inner stream cannot be split (TLS), the
    /// `BufStream` is reconstructed and returned in `Err` so the caller
    /// can keep using the serialized loop. A non-empty `write_buf` at
    /// split time also forces the serialized fallback: those bytes would
    /// otherwise need re-homing onto the new write half, and in practice
    /// the split is always taken at an idle point with an empty buffer.
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
            write_buf,
        } = self;
        match inner.try_into_split() {
            Ok((r, w)) => Ok((
                BufReadHalf {
                    inner: r,
                    read_buf,
                },
                BufWriteHalf {
                    inner: w,
                    write_buf,
                },
            )),
            Err(inner) => Err(BufStream {
                inner,
                read_buf,
                write_buf,
            }),
        }
    }
}
