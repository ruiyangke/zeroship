// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Swapped the tokio poll-based AsyncRead/AsyncWrite surface for compio's
// async-fn-based IoBuf/IoBufMut shape; see `compio::io` for the trait
// signatures we implement below.

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::TcpStream;
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
