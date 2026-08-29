// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Compio doesn't use pin-projection (async-fn traits carry no `poll_*`
// methods), so the wrapper is a plain enum; no pin_project_lite needed.

use crate::buf_stream::SplitStream;
use crate::encryption::Encryption;
use crate::tls::{ChannelBinding, ClientCertStatus, TlsStream};
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use std::io;

/// A stream that may or may not be TLS-wrapped.
pub enum MaybeTlsStream<S, T> {
    /// Plain underlying transport (TCP or Unix socket).
    Raw(S),
    /// TLS-wrapped stream produced by a [`TlsConnect`](crate::tls::TlsConnect).
    Tls(T),
}

impl<S, T> MaybeTlsStream<S, T> {
    /// Which transport this stream ACTUALLY ended up on.
    ///
    /// Not the same question as "which transport was attempted": a server that
    /// answers `N` to `SSLRequest` leaves a `Tls` attempt running in plaintext
    /// on the same socket. Anything that has to reproduce a session's transport
    /// later - a `CancelRequest` in particular - must ask this, not re-run the
    /// `sslmode` decision, because the two disagree exactly where it matters.
    pub(crate) fn negotiated_encryption(&self) -> Encryption {
        match self {
            MaybeTlsStream::Raw(_) => Encryption::Plaintext,
            MaybeTlsStream::Tls(_) => Encryption::Tls,
        }
    }
}

impl<S, T> AsyncRead for MaybeTlsStream<S, T>
where
    S: AsyncRead + Unpin,
    T: AsyncRead + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            MaybeTlsStream::Raw(s) => s.read(buf).await,
            MaybeTlsStream::Tls(s) => s.read(buf).await,
        }
    }
}

impl<S, T> AsyncWrite for MaybeTlsStream<S, T>
where
    S: AsyncWrite + Unpin,
    T: AsyncWrite + Unpin,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            MaybeTlsStream::Raw(s) => s.write(buf).await,
            MaybeTlsStream::Tls(s) => s.write(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            MaybeTlsStream::Raw(s) => s.flush().await,
            MaybeTlsStream::Tls(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            MaybeTlsStream::Raw(s) => s.shutdown().await,
            MaybeTlsStream::Tls(s) => s.shutdown().await,
        }
    }
}

impl<S, T> TlsStream for MaybeTlsStream<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsStream + Unpin,
{
    fn channel_binding(&self) -> ChannelBinding {
        match self {
            MaybeTlsStream::Raw(_) => ChannelBinding::none(),
            MaybeTlsStream::Tls(s) => s.channel_binding(),
        }
    }

    fn client_cert_status(&self) -> ClientCertStatus {
        match self {
            MaybeTlsStream::Raw(_) => ClientCertStatus::NotApplicable,
            MaybeTlsStream::Tls(s) => s.client_cert_status(),
        }
    }

    fn configure_release(
        &self,
        private: crate::tls::private::ForcePrivateApi,
        release: crate::tls::private::ReleaseConfig<'_>,
    ) {
        if let MaybeTlsStream::Tls(stream) = self {
            stream.configure_release(private, release);
        }
    }
}

/// Read half of a split [`MaybeTlsStream`].
///
/// An enum rather than `S::ReadHalf`, because the two transports produce
/// different half types: plaintext yields the socket's own read half, TLS
/// yields a decrypting one that shares the session with its write half.
pub enum MaybeTlsReadHalf<S: SplitStream, T: SplitStream> {
    /// Read half of the plain transport.
    Raw(S::ReadHalf),
    /// Read half of the TLS transport.
    Tls(T::ReadHalf),
}

/// Write half of a split [`MaybeTlsStream`].
pub enum MaybeTlsWriteHalf<S: SplitStream, T: SplitStream> {
    /// Write half of the plain transport.
    Raw(S::WriteHalf),
    /// Write half of the TLS transport.
    Tls(T::WriteHalf),
}

impl<S, T> AsyncRead for MaybeTlsReadHalf<S, T>
where
    S: SplitStream,
    T: SplitStream,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            MaybeTlsReadHalf::Raw(s) => s.read(buf).await,
            MaybeTlsReadHalf::Tls(s) => s.read(buf).await,
        }
    }
}

impl<S, T> AsyncWrite for MaybeTlsWriteHalf<S, T>
where
    S: SplitStream,
    T: SplitStream,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            MaybeTlsWriteHalf::Raw(s) => s.write(buf).await,
            MaybeTlsWriteHalf::Tls(s) => s.write(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            MaybeTlsWriteHalf::Raw(s) => s.flush().await,
            MaybeTlsWriteHalf::Tls(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            MaybeTlsWriteHalf::Raw(s) => s.shutdown().await,
            MaybeTlsWriteHalf::Tls(s) => s.shutdown().await,
        }
    }
}

impl<S, T> SplitStream for MaybeTlsStream<S, T>
where
    S: SplitStream,
    T: SplitStream,
{
    type ReadHalf = MaybeTlsReadHalf<S, T>;
    type WriteHalf = MaybeTlsWriteHalf<S, T>;

    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        // Both transports split, so both reach the multiplexed loop. A
        // connector whose stream genuinely cannot be torn in two answers
        // `Err` and is handed back for the serialized loop.
        match self {
            MaybeTlsStream::Raw(s) => match s.try_into_split() {
                Ok((read, write)) => {
                    Ok((MaybeTlsReadHalf::Raw(read), MaybeTlsWriteHalf::Raw(write)))
                }
                Err(s) => Err(MaybeTlsStream::Raw(s)),
            },
            MaybeTlsStream::Tls(s) => match s.try_into_split() {
                Ok((read, write)) => {
                    Ok((MaybeTlsReadHalf::Tls(read), MaybeTlsWriteHalf::Tls(write)))
                }
                Err(s) => Err(MaybeTlsStream::Tls(s)),
            },
        }
    }
}
