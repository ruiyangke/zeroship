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

    /// Forwarded, NOT defaulted. The trait's default answers `None`, which
    /// `connect_tls` reads as "this backend cannot observe ALPN" and refuses in
    /// `sslnegotiation=direct`. A wrapped session that DID negotiate
    /// `postgresql` would report the opposite of the truth.
    ///
    /// Today `connect_tls` asks the inner stream before wrapping it, so the
    /// omission was invisible; that ordering is not a property anyone declared.
    fn negotiated_alpn_protocol(&self) -> Option<&[u8]> {
        match self {
            MaybeTlsStream::Raw(_) => None,
            MaybeTlsStream::Tls(s) => s.negotiated_alpn_protocol(),
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

impl<S: SplitStream, T: SplitStream> std::fmt::Debug for MaybeTlsReadHalf<S, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raw(_) => formatter.debug_tuple("Raw").finish_non_exhaustive(),
            Self::Tls(_) => formatter.debug_tuple("Tls").finish_non_exhaustive(),
        }
    }
}

impl<S: SplitStream, T: SplitStream> std::fmt::Debug for MaybeTlsWriteHalf<S, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raw(_) => formatter.debug_tuple("Raw").finish_non_exhaustive(),
            Self::Tls(_) => formatter.debug_tuple("Tls").finish_non_exhaustive(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct ProbeStream;

    struct ProbeReadHalf;

    #[derive(Default)]
    struct ProbeWriteHalf {
        flushes: usize,
        shutdowns: usize,
    }

    impl AsyncRead for ProbeReadHalf {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for ProbeWriteHalf {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let n = buf.buf_len();
            BufResult(Ok(n), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            self.shutdowns += 1;
            Ok(())
        }
    }

    impl SplitStream for ProbeStream {
        type ReadHalf = ProbeReadHalf;
        type WriteHalf = ProbeWriteHalf;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Ok((ProbeReadHalf, ProbeWriteHalf::default()))
        }
    }

    #[compio::test]
    async fn maybe_tls_write_half_delegates_tls_flush() {
        let stream: MaybeTlsStream<ProbeStream, ProbeStream> = MaybeTlsStream::Tls(ProbeStream);
        let Ok((_read, mut write)) = stream.try_into_split() else {
            panic!("the TLS probe stream refused to split");
        };

        write.flush().await.expect("flush the TLS half");

        let MaybeTlsWriteHalf::Tls(probe) = write else {
            panic!("the TLS write half changed variants");
        };
        assert_eq!(probe.flushes, 1, "the TLS transport was not flushed");
    }

    #[compio::test]
    async fn maybe_tls_write_half_delegates_tls_shutdown() {
        let stream: MaybeTlsStream<ProbeStream, ProbeStream> = MaybeTlsStream::Tls(ProbeStream);
        let Ok((_read, mut write)) = stream.try_into_split() else {
            panic!("the TLS probe stream refused to split");
        };

        write.shutdown().await.expect("shut down the TLS half");

        let MaybeTlsWriteHalf::Tls(probe) = write else {
            panic!("the TLS write half changed variants");
        };
        assert_eq!(probe.shutdowns, 1, "the TLS transport was not shut down");
    }

    /// A session that negotiated ALPN, to prove the wrapper forwards rather
    /// than answering from `TlsStream`'s default.
    struct AlpnStream;

    impl AsyncRead for AlpnStream {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for AlpnStream {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let n = buf.buf_len();
            BufResult(Ok(n), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl crate::tls::TlsStream for AlpnStream {
        fn channel_binding(&self) -> crate::tls::ChannelBinding {
            crate::tls::ChannelBinding::none()
        }

        fn negotiated_alpn_protocol(&self) -> Option<&[u8]> {
            Some(b"postgresql")
        }
    }

    /// The default answer is `None`, which `connect_tls` reads as "this backend
    /// cannot observe ALPN" and refuses under `sslnegotiation=direct`. Not
    /// forwarding would therefore report the opposite of the truth for a
    /// session that DID select `postgresql`.
    #[test]
    fn the_wrapper_forwards_the_negotiated_alpn_protocol() {
        use crate::tls::TlsStream as _;

        let tls: MaybeTlsStream<AlpnStream, AlpnStream> = MaybeTlsStream::Tls(AlpnStream);
        assert_eq!(
            tls.negotiated_alpn_protocol(),
            Some(&b"postgresql"[..]),
            "the wrapper answered from the trait default instead of the session"
        );

        let raw: MaybeTlsStream<AlpnStream, AlpnStream> = MaybeTlsStream::Raw(AlpnStream);
        assert_eq!(
            raw.negotiated_alpn_protocol(),
            None,
            "the Raw arm consulted the inner stream; only the Tls arm may"
        );
    }
}
