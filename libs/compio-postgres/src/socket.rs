// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Swapped the tokio poll-based AsyncRead/AsyncWrite surface for compio's
// async-fn-based IoBuf/IoBufMut shape; see `compio::io` for the trait
// signatures we implement below.

use crate::buf_stream::SplitStream;
use crate::release::ConnectionRelease;
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

    /// Borrow the connected descriptor so socket-option assertions can read
    /// back what `connect_socket` applied. Test-only: nothing in the driver
    /// needs the raw fd, and exposing one would invite a second owner of a
    /// descriptor whose lifetime [`ConnectionRelease`] deliberately controls.
    #[cfg(all(test, unix))]
    pub(crate) fn borrowed_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        match &self.0 {
            Inner::Tcp(s) => s.as_fd(),
            Inner::Unix(s) => s.as_fd(),
        }
    }

    /// A handle that shuts this socket down when it is dropped.
    ///
    /// Taken before the socket is handed to the connection task and stored on
    /// the client half, so the session ends when the client does rather than
    /// when the process does. See [`crate::release`] for why that cannot be
    /// left to the connection task's own teardown.
    pub(crate) fn release_handle(&self) -> Option<ConnectionRelease> {
        match &self.0 {
            Inner::Tcp(s) => ConnectionRelease::dup_of(s),
            #[cfg(unix)]
            Inner::Unix(s) => ConnectionRelease::dup_of(s),
        }
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
/// Unix stream produced by compio's `into_split`. `into_split` does NOT
/// `dup(2)`: it `clone()`s a refcounted shared fd (compio `SharedFd`), so
/// both halves reference the SAME underlying fd. Concurrency relies on the
/// kernel allowing concurrent read and write SQEs on one socket, not on two
/// distinct fds. The fd is closed only when BOTH halves drop.
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
        // A plain TCP/Unix socket always splits: compio `into_split`
        // `clone()`s a refcounted shared fd (one fd, shared by both halves —
        // NOT a `dup`). Concurrent read/write submissions are safe because the
        // kernel permits concurrent read+write SQEs on a single socket; the fd
        // closes only when BOTH halves drop.
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

/// A bare compio `TcpStream` splits the same way [`Socket`] does.
///
/// No connect path uses it - `Socket` is what they build - but a `TlsConnect`
/// is generic over its transport, and this crate's own TLS tests hand one a
/// `TcpStream` directly. Without this they could not name a splittable
/// transport at all.
impl SplitStream for TcpStream {
    type ReadHalf = OwnedReadHalf<TcpStream>;
    type WriteHalf = OwnedWriteHalf<TcpStream>;

    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        Ok(self.into_split())
    }
}
