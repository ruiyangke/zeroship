//! Buffered compio I/O for PostgreSQL connections.
//!
//! Wraps compio's TcpStream with an 8KB userspace read buffer to amortize
//! io_uring submissions. Without buffering, parsing one Postgres message
//! (1-byte tag + 4-byte length + payload) would be 3 separate io_uring SQEs.

use bytes::BytesMut;
use compio::buf::BufResult;
use compio::io::AsyncWriteExt;
use compio::net::TcpStream;

use crate::{Error, Result};

const READ_BUF_CAPACITY: usize = 8192;

/// Maximum single-message size the driver will accept. PostgreSQL's own
/// limit is 1 GB (`PG_LARGE_SEND_MAX`), but for a platform where apps
/// execute structured CRUD queries (not bulk COPY), 64 MB is generous.
/// A malformed or malicious server sending a message header with a
/// multi-GB length field will be rejected here instead of OOM-ing the
/// worker.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

pub(crate) enum StreamInner {
    Tcp(TcpStream),
    #[cfg(feature = "tls")]
    Tls(compio_tls::TlsStream<TcpStream>),
}

/// Buffered read/write stream over a compio TcpStream (or TlsStream).
pub(crate) struct BufStream {
    inner: StreamInner,
    read_buf: BytesMut,
    write_buf: BytesMut,
}

impl BufStream {
    /// Wrap a TCP stream.
    pub fn tcp(stream: TcpStream) -> Self {
        Self {
            inner: StreamInner::Tcp(stream),
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
            write_buf: BytesMut::with_capacity(1024),
        }
    }

    /// Wrap a TLS stream.
    #[cfg(feature = "tls")]
    pub fn tls(stream: compio_tls::TlsStream<TcpStream>) -> Self {
        Self {
            inner: StreamInner::Tls(stream),
            read_buf: BytesMut::with_capacity(READ_BUF_CAPACITY),
            write_buf: BytesMut::with_capacity(1024),
        }
    }

    /// Ensure the read buffer has at least `min_bytes` available.
    /// Reads from the socket if needed. Returns error on EOF or I/O failure.
    ///
    /// Rejects requests larger than `MAX_MESSAGE_SIZE` to prevent a
    /// malformed/malicious server from OOM-ing the process with a
    /// crafted 4-byte length field (e.g., 0xFFFFFFFF = 4 GB).
    pub async fn fill(&mut self, min_bytes: usize) -> Result<()> {
        if min_bytes > MAX_MESSAGE_SIZE {
            return Err(Error::Protocol(format!(
                "message too large: {min_bytes} bytes (max {MAX_MESSAGE_SIZE})"
            )));
        }
        while self.read_buf.len() < min_bytes {
            let capacity = READ_BUF_CAPACITY.max(min_bytes - self.read_buf.len());
            let buf = vec![0u8; capacity];
            let (n, buf) = self.read_raw(buf).await?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::new(
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

    /// Read exactly one byte from the stream (used for SSL negotiation response).
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    pub async fn read_byte(&mut self) -> Result<u8> {
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

    /// Flush the write buffer to the socket.
    pub async fn flush(&mut self) -> Result<()> {
        if self.write_buf.is_empty() {
            return Ok(());
        }
        let data = self.write_buf.split().freeze().to_vec();
        self.write_all_raw(data).await?;
        Ok(())
    }

    /// Low-level read into owned buffer, returning (bytes_read, buffer).
    async fn read_raw(&mut self, buf: Vec<u8>) -> Result<(usize, Vec<u8>)> {
        use compio::io::AsyncRead;
        let BufResult(result, returned) = match &mut self.inner {
            StreamInner::Tcp(s) => s.read(buf).await,
            #[cfg(feature = "tls")]
            StreamInner::Tls(s) => s.read(buf).await,
        };
        let n = result?;
        Ok((n, returned))
    }

    /// Low-level write all bytes to the socket.
    async fn write_all_raw(&mut self, data: Vec<u8>) -> Result<()> {
        let BufResult(result, _) = match &mut self.inner {
            StreamInner::Tcp(s) => s.write_all(data).await,
            #[cfg(feature = "tls")]
            StreamInner::Tls(s) => s.write_all(data).await,
        };
        result?;
        Ok(())
    }

    /// Extract the underlying TCP stream (for TLS upgrade).
    /// Only works if the stream is currently plain TCP.
    #[cfg(feature = "tls")]
    pub fn into_tcp(self) -> std::result::Result<TcpStream, Self> {
        match self.inner {
            StreamInner::Tcp(s) => Ok(s),
            _ => Err(self),
        }
    }
}
