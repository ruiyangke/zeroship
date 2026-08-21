// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Adapted to compio: the trait bounds reference `compio::io::{AsyncRead,
// AsyncWrite}` in place of their tokio equivalents. The shape is otherwise
// preserved so external TLS backends (compio-tls native-tls, future
// rustls-based impls) can plug in without touching this file.

//! TLS support.

use crate::Error;
use crate::config::SslCertMode;
use compio::io::{AsyncRead, AsyncWrite};
use std::error;
use std::fmt;
use std::future::Future;
use std::io;

pub(crate) mod private {
    pub struct ForcePrivateApi;
}

/// Channel binding information returned from a TLS handshake.
pub struct ChannelBinding {
    pub(crate) tls_server_end_point: Option<Vec<u8>>,
}

/// What a completed TLS handshake observed about client-certificate use.
///
/// [`SslCertMode::Require`](crate::config::SslCertMode::Require) consumes this
/// after PostgreSQL authentication succeeds. A custom TLS backend that cannot
/// report the observation returns [`Unknown`](ClientCertStatus::Unknown),
/// which is refused under that mode rather than approximated as success.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientCertStatus {
    /// The connection is plaintext, so no TLS certificate could be requested.
    NotApplicable,
    /// The TLS server did not request a client certificate.
    NotRequested,
    /// The server requested a certificate, but the client did not select one.
    NotSent,
    /// The server requested a certificate and the client selected one to send.
    Sent,
    /// The TLS backend cannot report whether it sent a certificate.
    Unknown,
}

impl ChannelBinding {
    /// Creates a `ChannelBinding` containing no information.
    pub fn none() -> ChannelBinding {
        ChannelBinding {
            tls_server_end_point: None,
        }
    }

    /// Creates a `ChannelBinding` containing `tls-server-end-point` channel binding information.
    pub fn tls_server_end_point(tls_server_end_point: Vec<u8>) -> ChannelBinding {
        ChannelBinding {
            tls_server_end_point: Some(tls_server_end_point),
        }
    }
}

/// A constructor of `TlsConnect`ors.
pub trait MakeTlsConnect<S> {
    /// The stream type created by the `TlsConnect` implementation.
    type Stream: TlsStream + Unpin;
    /// The `TlsConnect` implementation created by this type.
    type TlsConnect: TlsConnect<S, Stream = Self::Stream>;
    /// The error type returned by the `TlsConnect` implementation.
    type Error: Into<Box<dyn error::Error + Sync + Send>>;

    /// Creates a new `TlsConnect`or.
    ///
    /// The domain name is provided for certificate verification and SNI.
    fn make_tls_connect(&mut self, domain: &str) -> Result<Self::TlsConnect, Self::Error>;
}

/// An asynchronous function wrapping a stream in a TLS session.
pub trait TlsConnect<S> {
    /// The stream returned by the future.
    type Stream: TlsStream + Unpin;
    /// The error returned by the future.
    type Error: Into<Box<dyn error::Error + Sync + Send>>;
    /// The future returned by the connector.
    type Future: Future<Output = Result<Self::Stream, Self::Error>>;

    /// Returns a future performing a TLS handshake over the stream.
    fn connect(self, stream: S) -> Self::Future;

    /// Reports whether this connector will apply the requested SNI policy.
    ///
    /// The default accepts enabled SNI because [`MakeTlsConnect`] has always
    /// received a domain for certificate verification and SNI. A connector
    /// that can disable SNI must override this method and attest that its
    /// handshake configuration matches `enabled`.
    fn can_honor_sslsni(&self, enabled: bool) -> bool {
        enabled
    }

    /// Reports whether this connector will apply the client-certificate mode.
    ///
    /// The default accepts only libpq's permissive `allow` mode. Connectors
    /// supporting `disable` or `require` must override this method; `require`
    /// is also checked after authentication through
    /// [`TlsStream::client_cert_status`].
    fn can_honor_sslcertmode(&self, mode: SslCertMode) -> bool {
        mode == SslCertMode::Allow
    }

    #[doc(hidden)]
    fn can_connect(&self, _: private::ForcePrivateApi) -> bool {
        true
    }
}

/// A TLS-wrapped connection to a PostgreSQL database.
pub trait TlsStream: AsyncRead + AsyncWrite {
    /// Returns channel binding information for the session.
    fn channel_binding(&self) -> ChannelBinding;

    /// Reports whether the handshake requested and sent a client certificate.
    fn client_cert_status(&self) -> ClientCertStatus {
        ClientCertStatus::Unknown
    }
}

/// A `MakeTlsConnect` and `TlsConnect` implementation which simply returns an error.
///
/// This can be used when `sslmode` is `none` or `prefer`.
#[derive(Debug, Copy, Clone)]
pub struct NoTls;

impl<S> MakeTlsConnect<S> for NoTls {
    type Stream = NoTlsStream;
    type TlsConnect = NoTls;
    type Error = NoTlsError;

    fn make_tls_connect(&mut self, _: &str) -> Result<NoTls, NoTlsError> {
        Ok(NoTls)
    }
}

impl<S> TlsConnect<S> for NoTls {
    type Stream = NoTlsStream;
    type Error = NoTlsError;
    type Future = NoTlsFuture;

    fn connect(self, _: S) -> NoTlsFuture {
        NoTlsFuture(())
    }

    fn can_connect(&self, _: private::ForcePrivateApi) -> bool {
        false
    }
}

/// The future returned by `NoTls`.
pub struct NoTlsFuture(());

impl Future for NoTlsFuture {
    type Output = Result<NoTlsStream, NoTlsError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Ready(Err(NoTlsError(())))
    }
}

/// The TLS "stream" type produced by the `NoTls` connector.
///
/// Since `NoTls` doesn't support TLS, this type is uninhabited.
pub enum NoTlsStream {}

impl AsyncRead for NoTlsStream {
    async fn read<B: compio::buf::IoBufMut>(&mut self, _buf: B) -> compio::BufResult<usize, B> {
        match *self {}
    }
}

impl AsyncWrite for NoTlsStream {
    async fn write<B: compio::buf::IoBuf>(&mut self, _buf: B) -> compio::BufResult<usize, B> {
        match *self {}
    }

    async fn flush(&mut self) -> io::Result<()> {
        match *self {}
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match *self {}
    }
}

impl TlsStream for NoTlsStream {
    fn channel_binding(&self) -> ChannelBinding {
        match *self {}
    }
}

/// The error returned by `NoTls`.
#[derive(Debug)]
pub struct NoTlsError(());

impl fmt::Display for NoTlsError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("no TLS implementation configured")
    }
}

impl error::Error for NoTlsError {}

// Suppress unused-warning: Error is imported so callers can surface TLS
// failures via Error::tls; it's also referenced indirectly from
// connect_tls.rs.
#[allow(dead_code)]
fn _ref_error(_: Error) {}
