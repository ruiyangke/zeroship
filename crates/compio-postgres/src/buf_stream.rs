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
