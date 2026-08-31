// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Adapted to compio: the trait bounds reference `compio::io::{AsyncRead,
// AsyncWrite}` in place of their tokio equivalents. The shape is otherwise
// preserved so external TLS backends (compio-tls native-tls, future
// rustls-based impls) can plug in without touching this file.

//! TLS support.

use crate::Error;
use crate::config::{SslCertMode, SslMode, SslRootCert};
use compio::io::{AsyncRead, AsyncWrite};
use std::error;
use std::fmt;
use std::future::Future;
use std::io;
use std::sync::Arc;

/// `PostgreSQL`'s registered ALPN protocol identifier.
pub(crate) const POSTGRESQL_ALPN_PROTOCOL: &[u8] = b"postgresql";

pub(crate) mod private {
    #[derive(Debug)]
    pub struct ForcePrivateApi;

    // Read only by the TLS connectors, so the field is genuinely dead when the
    // `tls` feature is off. Targeted rather than a crate-wide allow.
    #[derive(Debug)]
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    pub struct ReleaseConfig<'a>(&'a mut crate::release::ConnectionRelease);

    impl<'a> ReleaseConfig<'a> {
        pub(crate) fn new(release: &'a mut crate::release::ConnectionRelease) -> Self {
            Self(release)
        }

        #[cfg(feature = "tls")]
        pub(crate) fn set_tls_session(&mut self, session: crate::tls_sansio::SharedSession) {
            self.0.set_tls_session(session);
        }
    }
}

/// What a connection string asks a connector to check about the certificate
/// the server presents.
///
/// Three levels, because libpq has three: no verification, chain only, and
/// chain plus host name. Six `sslmode` values and the presence or absence of
/// `sslrootcert` map onto them, so the mapping is many-to-one and writing it as
/// one match is what keeps it reviewable.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerVerification {
    /// Accept whatever the server sends. Encryption only.
    ///
    /// libpq's `require`, `prefer` and `allow` when no root CA is configured.
    None,
    /// The certificate must chain to a configured trust anchor. The host name
    /// is not looked at. libpq's `verify-ca`, and the weaker modes once a root
    /// CA is configured.
    Chain,
    /// Chain, and the host name must match. libpq's `verify-full`; rustls'
    /// default behaviour.
    ChainAndHostname,
}

impl ServerVerification {
    /// THE selection. Every certificate check this driver performs or skips is
    /// decided by this function, from these two arguments.
    ///
    /// The load-bearing lines: `VerifyFull` is the only arm that yields
    /// [`ChainAndHostname`](ServerVerification::ChainAndHostname), and the two
    /// `Verify*` arms are the only ones that can fail - the weaker modes
    /// degrade to [`None`](ServerVerification::None) rather than erroring,
    /// which is exactly what makes `require` "encrypted, unverified".
    pub(crate) fn select(
        mode: SslMode,
        roots_configured: bool,
    ) -> Result<ServerVerification, Error> {
        match mode {
            SslMode::VerifyFull if roots_configured => Ok(ServerVerification::ChainAndHostname),
            SslMode::VerifyCa if roots_configured => Ok(ServerVerification::Chain),
            SslMode::VerifyCa | SslMode::VerifyFull => Err(Error::tls(
                format!(
                    "sslmode={} verifies the server certificate, so it needs trust anchors: set \
                     sslrootcert to the CA that signed it (or to sslrootcert=system for the \
                     operating system store, which requires sslmode=verify-full)",
                    mode.as_str()
                )
                .into(),
            )),
            SslMode::Require | SslMode::Prefer | SslMode::Allow => {
                if roots_configured {
                    Ok(ServerVerification::Chain)
                } else {
                    Ok(ServerVerification::None)
                }
            }
            // `disable` never reaches a connector: `Encryption::first_for`
            // gives it plaintext and no retry can promote it, and
            // `Transport::resolve` / `MakeRustlsConnect::from_config` both stop
            // before here. Refusing is better than inventing a policy for a
            // mode that has none.
            SslMode::Disable => Err(Error::tls(
                "sslmode=disable does not use TLS, so no verification policy applies".into(),
            )),
        }
    }

    /// The verification a [`Config`](crate::Config)'s TLS settings demand.
    pub(crate) fn demanded_by(
        mode: SslMode,
        root_cert: &SslRootCert,
    ) -> Result<ServerVerification, Error> {
        // "Configured" is asked of `sslrootcert`, and it cannot disagree with
        // the trust store a connector actually built: `MakeRustlsConnect` loads
        // nothing for `Unset`, and errors rather than returning an empty store
        // for `System` or `File`.
        ServerVerification::select(mode, *root_cert != SslRootCert::Unset)
    }
}

/// Channel binding information returned from a TLS handshake.
pub struct ChannelBinding {
    pub(crate) tls_server_end_point: Option<Vec<u8>>,
}

impl fmt::Debug for ChannelBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tls_server_end_point = self.tls_server_end_point.as_ref().map(|_| "<redacted>");

        formatter
            .debug_struct("ChannelBinding")
            .field("tls_server_end_point", &tls_server_end_point)
            .finish()
    }
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

/// Opaque identity for one cancellation-sensitive TLS policy.
///
/// A cancel request opens a second connection and sends the original
/// connection's bearer cancel key through it. The coarse connector
/// attestations cover SNI, client-certificate mode, and server-verification
/// level, but cannot prove that two connectors use the same trust anchors,
/// CRLs, client identity, protocol bounds, or other backend-specific policy.
///
/// Mint this once when constructing a TLS policy and retain it in every clone
/// of that policy. Do not mint a replacement for each handshake, and do not
/// reuse it after changing any cancellation-sensitive policy. A cancellation
/// connector without the original identity is refused before it can connect.
/// The identity is not a secret and reveals no policy contents.
#[derive(Clone)]
pub struct TlsPolicyIdentity(Arc<()>);

impl TlsPolicyIdentity {
    /// Creates a fresh identity for one TLS policy lineage.
    pub fn new() -> Self {
        Self(Arc::new(()))
    }

    pub(crate) fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Default for TlsPolicyIdentity {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TlsPolicyIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TlsPolicyIdentity(..)")
    }
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
    ///
    /// [`SplitStream`](crate::SplitStream) is required, not optional: it is
    /// how the connection task learns whether this transport can run the
    /// multiplexed loop. A stream that cannot be torn in two answers
    /// `Err(self)` and gets the serialized loop.
    type Stream: TlsStream + crate::buf_stream::SplitStream + Unpin;
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

    /// Reports whether this connector authenticates the server the way the
    /// connection string asked.
    ///
    /// This is the attestation that matters most, because the setting it
    /// covers - `sslmode` together with `sslrootcert` - is the only one that
    /// promises the peer is who it claims to be. The default answers "yes"
    /// only to [`ServerVerification::None`], the level that promises nothing:
    /// a connector built without reading those settings cannot have applied
    /// them, and accepting `verify-full` on its behalf would report an
    /// authenticated session that nothing authenticated.
    ///
    /// A connector that reads a [`Config`](crate::Config)'s TLS settings must
    /// override this and compare against the level it really built.
    fn can_honor_server_verification(&self, verification: ServerVerification) -> bool {
        verification == ServerVerification::None
    }

    /// Identifies the complete TLS policy this connector will use.
    ///
    /// Cancellation requires the connector for its second connection to
    /// return the same identity recorded from the original connection. The
    /// default refuses TLS cancellation because a connector that did not opt
    /// into this contract cannot prove exact policy continuity.
    fn cancel_policy_identity(&self) -> Option<&TlsPolicyIdentity> {
        None
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

    /// Returns the application protocol selected by the TLS handshake.
    ///
    /// `sslnegotiation=direct` requires the peer to select `postgresql` via
    /// ALPN. The default reports no selection, so a custom backend that cannot
    /// observe ALPN is refused in direct mode rather than silently skipping
    /// that protocol-confusion check.
    fn negotiated_alpn_protocol(&self) -> Option<&[u8]> {
        None
    }

    /// Reports whether the handshake requested and sent a client certificate.
    fn client_cert_status(&self) -> ClientCertStatus {
        ClientCertStatus::Unknown
    }

    /// Lets this crate's `rustls` stream attach its live session to the
    /// synchronous socket-release guard.
    #[doc(hidden)]
    fn configure_release(&self, _: private::ForcePrivateApi, _: private::ReleaseConfig<'_>) {}
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
#[derive(Debug)]
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
#[derive(Debug)]
pub enum NoTlsStream {}

/// Uninhabited, so this is unreachable. It exists to satisfy the
/// [`TlsConnect::Stream`] bound.
impl crate::buf_stream::SplitStream for NoTlsStream {
    type ReadHalf = NoTlsStream;
    type WriteHalf = NoTlsStream;

    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        match self {}
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_binding_debug_redacts_tls_server_end_point() {
        let secret = b"channel-binding-material".to_vec();
        let exposed = format!("{secret:?}");
        let debug = format!("{:?}", ChannelBinding::tls_server_end_point(secret));

        assert!(
            debug.contains("<redacted>"),
            "channel binding debug did not mark the redaction: {debug}"
        );
        assert!(
            !debug.contains(&exposed),
            "channel binding debug leaked TLS binding material: {debug}"
        );
    }

    /// The mode-to-policy table, asserted arm by arm.
    ///
    /// This is one half of the mutation proof. Swapping the `VerifyFull` arm of
    /// `ServerVerification::select` for a weaker policy fails here; it is the
    /// cheapest place such a change can be caught, and it needs no
    /// certificates.
    ///
    /// It does NOT prove the policies *do* anything - a `Chain` variant wired
    /// to a no-op verifier would still satisfy every assertion below. That is
    /// what `tls_rustls`'s `the_three_policies_discriminate` is for.
    ///
    /// It lives here rather than beside the rustls connector because the table
    /// now also governs connectors that are not rustls: `connect_raw` evaluates
    /// it to decide what the supplied connector has to attest to, and that
    /// happens whether or not the `tls` feature is on.
    #[test]
    fn policy_selection_follows_mode_and_whether_roots_are_configured() {
        use ServerVerification::*;
        use SslMode::*;

        // No trust anchors: the three weak modes encrypt without
        // authenticating, and the two verifying modes refuse to run at all.
        for mode in [Require, Prefer, Allow] {
            assert_eq!(
                ServerVerification::select(mode, false).unwrap(),
                None,
                "sslmode={} with no sslrootcert must not claim to verify",
                mode.as_str()
            );
        }
        for mode in [VerifyCa, VerifyFull] {
            ServerVerification::select(mode, false)
                .expect_err("a verifying mode with no trust anchors must be an error");
        }

        // Trust anchors configured: everything checks the chain, and exactly
        // one mode also checks the host name.
        for mode in [Require, Prefer, Allow, VerifyCa] {
            assert_eq!(
                ServerVerification::select(mode, true).unwrap(),
                Chain,
                "sslmode={} with sslrootcert must check the chain and NOT the host name",
                mode.as_str()
            );
        }
        assert_eq!(
            ServerVerification::select(VerifyFull, true).unwrap(),
            ChainAndHostname
        );

        ServerVerification::select(Disable, true).expect_err("disable has no verification policy");
    }

    /// `sslrootcert` is read for the demand exactly as the trust store is read
    /// for the promise, so the two questions cannot answer differently.
    #[test]
    fn naming_trust_anchors_raises_the_demand_on_every_mode_that_uses_tls() {
        for mode in [SslMode::Require, SslMode::Prefer, SslMode::Allow] {
            assert_eq!(
                ServerVerification::demanded_by(mode, &SslRootCert::Unset).unwrap(),
                ServerVerification::None
            );
            assert_eq!(
                ServerVerification::demanded_by(mode, &SslRootCert::File("ca.pem".into())).unwrap(),
                ServerVerification::Chain,
                "sslmode={} with sslrootcert asks for chain checking",
                mode.as_str()
            );
        }
        assert_eq!(
            ServerVerification::demanded_by(SslMode::VerifyFull, &SslRootCert::System).unwrap(),
            ServerVerification::ChainAndHostname
        );
    }

    /// The default attestation, which is what every third-party connector gets
    /// until it overrides the method. It must claim nothing beyond the level
    /// that promises nothing.
    #[test]
    fn the_default_attestation_claims_only_the_unverified_level() {
        struct Unattested;

        impl<S> TlsConnect<S> for Unattested {
            type Stream = NoTlsStream;
            type Error = io::Error;
            type Future = std::future::Ready<Result<NoTlsStream, io::Error>>;

            fn connect(self, _: S) -> Self::Future {
                std::future::ready(Err(io::Error::other("not a real handshake")))
            }
        }

        let connector = Unattested;
        assert!(
            TlsConnect::<()>::can_honor_server_verification(&connector, ServerVerification::None),
            "a connector must still serve the modes that promise no verification"
        );
        for level in [
            ServerVerification::Chain,
            ServerVerification::ChainAndHostname,
        ] {
            assert!(
                !TlsConnect::<()>::can_honor_server_verification(&connector, level),
                "{level:?} must not be claimed by a connector that never read sslrootcert"
            );
        }
    }
}
