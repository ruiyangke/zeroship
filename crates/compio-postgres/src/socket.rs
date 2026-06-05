// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Swapped the tokio poll-based AsyncRead/AsyncWrite surface for compio's
// async-fn-based IoBuf/IoBufMut shape; see `compio::io` for the trait
// signatures we implement below.

use crate::buf_stream::SplitStream;
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::{OwnedReadHalf, OwnedWriteHalf, TcpStream};
#[cfg(unix)]
use compio::net::UnixStream;

#[derive(Debug)]
enum Inner {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

/// The standard stream type used by the crate.
///
/// Wraps compio's TCP or Unix socket so the rest of the driver can remain
/// generic over the transport. TLS is layered on top via
/// [`crate::maybe_tls_stream::MaybeTlsStream`].
#[derive(Debug)]
pub struct Socket(Inner);

impl Socket {
    pub(crate) fn new_tcp(stream: TcpStream) -> Socket {
        Socket(Inner::Tcp(stream))
    }

    #[cfg(unix)]
    pub(crate) fn new_unix(stream: UnixStream) -> Socket {
        Socket(Inner::Unix(stream))
    }
}

impl AsyncRead for Socket {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match &mut self.0 {
            Inner::Tcp(s) => s.read(buf).await,
            #[cfg(unix)]
            Inner::Unix(s) => s.read(buf).await,
        }
    }
}

impl AsyncWrite for Socket {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match &mut self.0 {
            Inner::Tcp(s) => s.write(buf).await,
            #[cfg(unix)]
            Inner::Unix(s) => s.write(buf).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            Inner::Tcp(s) => s.flush().await,
            #[cfg(unix)]
            Inner::Unix(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            Inner::Tcp(s) => s.shutdown().await,
            #[cfg(unix)]
            Inner::Unix(s) => s.shutdown().await,
        }
    }
}

/// Owned read half of a split [`Socket`] — the read side of a TCP or
/// Unix stream produced by compio's `into_split` (the fd is dup'd so the
/// two halves can run independent io_uring submissions).
#[derive(Debug)]
pub enum SocketReadHalf {
    Tcp(OwnedReadHalf<TcpStream>),
    #[cfg(unix)]
    Unix(OwnedReadHalf<UnixStream>),
}

/// Owned write half of a split [`Socket`].
#[derive(Debug)]
pub enum SocketWriteHalf {
    Tcp(OwnedWriteHalf<TcpStream>),
    #[cfg(unix)]
    Unix(OwnedWriteHalf<UnixStream>),
}

impl AsyncRead for SocketReadHalf {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            SocketReadHalf::Tcp(s) => s.read(buf).await,
            #[cfg(unix)]
            SocketReadHalf::Unix(s) => s.read(buf).await,
        }
    }
}

impl AsyncWrite for SocketWriteHalf {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            SocketWriteHalf::Tcp(s) => s.write(buf).await,
            #[cfg(unix)]
            SocketWriteHalf::Unix(s) => s.write(buf).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            SocketWriteHalf::Tcp(s) => s.flush().await,
            #[cfg(unix)]
            SocketWriteHalf::Unix(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            SocketWriteHalf::Tcp(s) => s.shutdown().await,
            #[cfg(unix)]
            SocketWriteHalf::Unix(s) => s.shutdown().await,
        }
    }
}

impl SplitStream for Socket {
    type ReadHalf = SocketReadHalf;
    type WriteHalf = SocketWriteHalf;

    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        // A plain TCP/Unix socket always splits: compio dups the fd so
        // the read and write halves own independent submission slots.
        match self.0 {
            Inner::Tcp(s) => {
                let (r, w) = s.into_split();
                Ok((SocketReadHalf::Tcp(r), SocketWriteHalf::Tcp(w)))
            }
            #[cfg(unix)]
            Inner::Unix(s) => {
                let (r, w) = s.into_split();
                Ok((SocketReadHalf::Unix(r), SocketWriteHalf::Unix(w)))
            }
        }
    }
}
