//! A rustls-backed [`MakeTlsConnect`] for compio-postgres.
//!
//! This is the implementation the [`tls`](crate) feature's name promises: the
//! seam in [`crate::tls`] has always accepted an external backend, and this is
//! one. Nothing in `tls.rs`, `connect_tls.rs` or `maybe_tls_stream.rs` changed
//! to accommodate it.
//!
//! # Trust anchors, and the three verification policies
//!
//! What is checked about the server's certificate is a function of two inputs
//! and nothing else: the [`SslMode`], and whether [`SslRootCert`] names any
//! trust anchors. `VerifyPolicy::select` is the *only* place that function is
//! evaluated, and `verifier_for` is the only place a policy becomes a rustls
//! [`ServerCertVerifier`]. Both are small on purpose: a mistake in either
//! silently disables verification for every connection this driver makes, and
//! the failure mode is a green test suite.
//!
//! Two of the three policies are weaker than rustls' default, which rustls
//! deliberately makes awkward to reach - it is behind
//! `ClientConfig::dangerous()`. They exist because libpq's `require` (and
//! `prefer`/`allow` when TLS happens) encrypts without authenticating unless a
//! root CA is configured; see [`SslRootCert`] for the table. This is a driver,
//! and a driver implements the protocol's modes.
//!
//! The pairing that catches a no-op verifier is `verify-ca` and `verify-full`
//! against the *same* host-name-mismatched server: they must go opposite ways.
//! `tests/tls_live.rs` runs that live, and
//! `the_three_policies_discriminate` runs it offline against committed
//! certificates, so the proof is in CI and not only on a machine with Docker.
//!
//! # Channel binding
//!
//! [`RustlsStream`] implements `tls-server-end-point` (RFC 5929 section 4.1), so
//! `channel_binding=require` and SCRAM-SHA-256-PLUS work over a rustls
//! connection. See `tls_server_end_point` for the certificates that are *not*
//! covered - it reports no binding rather than a wrong one.
//!
//! Channel binding is available at every mode that establishes TLS, including
//! `require` with no verification. libpq has no `sslmode` gate on it either:
//! the binding is derived from the certificate the server presented whenever
//! `ssl_in_use`, and `require` is precisely the mode where it earns its keep,
//! because SCRAM-PLUS is then the only thing authenticating the server at all.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use compio::io::compat::AsyncStream;
use compio::io::{AsyncRead, AsyncWrite};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{WebPkiServerVerifier, verify_server_cert_signed_by_trust_anchor};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::Error;
use crate::config::{Config, SslMode, SslRootCert};
use crate::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};

// ---------------------------------------------------------------------------
// Verification policy - the one decision, and the one place it is made
// ---------------------------------------------------------------------------

/// What a connection checks about the certificate the server presents.
///
/// Three policies, because libpq has three: no verification, chain only, and
/// chain plus host name. Six `sslmode` values map onto them, so the mapping is
/// many-to-one and writing it as one match is what keeps it reviewable.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum VerifyPolicy {
    /// Accept whatever the server sends. Encryption only.
    ///
    /// libpq's `require`, `prefer` and `allow` when no root CA is configured.
    AcceptAny,
    /// The certificate must chain to a configured trust anchor. The host name
    /// is not looked at. libpq's `verify-ca`, and the weaker modes once a root
    /// CA is configured.
    Chain,
    /// Chain, and the host name must match. libpq's `verify-full`; rustls'
    /// default behaviour.
    ChainAndHostname,
}

impl VerifyPolicy {
    /// THE selection. Every certificate check this driver performs or skips is
    /// decided by this function, from these two arguments.
    ///
    /// Read the arms against [`SslRootCert`]'s table. The load-bearing lines:
    /// `VerifyFull` is the only arm that yields
    /// [`ChainAndHostname`](VerifyPolicy::ChainAndHostname), and the two
    /// `Verify*` arms are the only ones that can fail - the weaker modes
    /// degrade to [`AcceptAny`](VerifyPolicy::AcceptAny) rather than erroring,
    /// which is exactly what makes `require` "encrypted, unverified".
    fn select(mode: SslMode, roots_configured: bool) -> Result<VerifyPolicy, Error> {
        match mode {
            SslMode::VerifyFull if roots_configured => Ok(VerifyPolicy::ChainAndHostname),
            SslMode::VerifyCa if roots_configured => Ok(VerifyPolicy::Chain),
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
                    Ok(VerifyPolicy::Chain)
                } else {
                    Ok(VerifyPolicy::AcceptAny)
                }
            }
            // `disable` never builds a connector; `Transport::resolve` and
            // `MakeRustlsConnect::from_config` both stop before here. Refusing
            // is better than inventing a policy for a mode that has none.
            SslMode::Disable => Err(Error::tls(
                "sslmode=disable does not use TLS, so no verification policy applies".into(),
            )),
        }
    }
}

/// Turn a policy into the rustls verifier that implements it.
///
/// Every mode goes through this function, including `verify-full` - which
/// could have used the safe `with_root_certificates` builder instead. It does
/// not, deliberately: one code path means the offline test in this file can
/// drive `verify-full`'s real verifier through the real selection, so a
/// mutation that swaps it for a permissive one has nowhere to hide. A
/// `verify-full` that quietly bypassed this function would be untested by
/// construction.
fn verifier_for(
    mode: SslMode,
    roots: Arc<RootCertStore>,
    provider: &CryptoProvider,
) -> Result<Arc<dyn ServerCertVerifier>, Error> {
    let algorithms = provider.signature_verification_algorithms;
    // "Configured" is asked of the store, not of `SslRootCert`, and the two
    // cannot disagree: `from_config` loads nothing for `Unset`, and errors
    // rather than returning an empty store for `System` or `File`. Asking the
    // store is the safer of the two identical questions, because a store with
    // no anchors could not verify anything even if a path had been named.
    Ok(match VerifyPolicy::select(mode, !roots.is_empty())? {
        VerifyPolicy::AcceptAny => Arc::new(AcceptAnyServerCert { algorithms }),
        VerifyPolicy::Chain => Arc::new(ChainOnlyServerCert { roots, algorithms }),
        VerifyPolicy::ChainAndHostname => {
            WebPkiServerVerifier::builder_with_provider(roots, Arc::new(provider.clone()))
                .build()
                .map_err(|e| Error::tls(Box::new(e)))?
        }
    })
}

/// [`VerifyPolicy::Chain`]: the chain is checked against the configured trust
/// anchors, the host name is not.
///
/// This is not "the default verifier with the name check removed" - it is
/// rustls' own chain-verification entry point,
/// [`verify_server_cert_signed_by_trust_anchor`] (a public, non-`danger`
/// function), with `verify_server_name` simply not called. No path building is
/// reimplemented here. A custom verifier is needed only because
/// [`WebPkiServerVerifier`] has no knob to disable the name check.
#[derive(Debug)]
struct ChainOnlyServerCert {
    roots: Arc<RootCertStore>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ChainOnlyServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// [`VerifyPolicy::AcceptAny`]: no chain, no host name, no expiry.
///
/// The session is encrypted against a passive eavesdropper and against nobody
/// else: anyone able to answer on the socket can present a self-signed
/// certificate for any name and be believed. That is what `sslmode=require`
/// means in libpq, and the honest way to offer it is to say so here rather
/// than to leave a "verified" claim standing on a connection that is not.
///
/// The handshake *signature* is still verified - `verify_tls12_signature` and
/// `verify_tls13_signature` below do the real check - so the peer must at
/// least hold the private key for the certificate it sent. Skipping that would
/// not weaken authentication further (there is none), it would break TLS.
#[derive(Debug)]
struct AcceptAnyServerCert {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// A [`MakeTlsConnect`] that performs handshakes with rustls.
///
/// Build one from a [`Config`] with [`MakeRustlsConnect::from_config`] (which
/// reads `sslrootcert` / `sslcert` / `sslkey`), or hand it a fully built
/// [`ClientConfig`] with [`MakeRustlsConnect::new`] when the trust decisions
/// are made in code rather than in the connection string.
#[derive(Clone, Debug)]
pub struct MakeRustlsConnect {
    config: Arc<ClientConfig>,
}

impl MakeRustlsConnect {
    /// Wrap an existing rustls client configuration.
    pub fn new(config: Arc<ClientConfig>) -> MakeRustlsConnect {
        MakeRustlsConnect { config }
    }

    /// Build a connector from the TLS parameters of a connection string.
    ///
    /// Reads `sslrootcert` (trust anchors) and the `sslcert` / `sslkey` pair
    /// (client-certificate authentication). The two client-auth parameters must
    /// be given together; naming one without the other is an error rather than
    /// a silently ignored half-configuration.
    pub fn from_config(config: &Config) -> Result<MakeRustlsConnect, Error> {
        let mut roots = RootCertStore::empty();
        match config.get_ssl_root_cert() {
            // No trust anchors named. `verifier_for` turns that into either
            // "accept anything" or an error, depending on the mode; loading
            // nothing here is what makes `roots.is_empty()` mean
            // "sslrootcert is unset" at that decision.
            SslRootCert::Unset => {}
            SslRootCert::System => {
                let found = rustls_native_certs::load_native_certs();
                for cert in found.certs {
                    // Ignore individual unparsable system certificates: a
                    // single bad entry in the OS store must not take out the
                    // whole store. An empty result is caught below.
                    let _ = roots.add(cert);
                }
                if roots.is_empty() {
                    return Err(Error::tls(
                        format!(
                            "sslrootcert=system: the operating system certificate store yielded \
                             no usable certificates ({} read error(s)). Name a CA file with \
                             sslrootcert=<path> instead.",
                            found.errors.len()
                        )
                        .into(),
                    ));
                }
            }
            SslRootCert::File(path) => {
                let certs = CertificateDer::pem_file_iter(path)
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| {
                        Error::tls(format!("sslrootcert={path}: cannot read PEM: {e}").into())
                    })?;
                if certs.is_empty() {
                    return Err(Error::tls(
                        format!("sslrootcert={path}: file contains no CERTIFICATE blocks").into(),
                    ));
                }
                for cert in certs {
                    roots.add(cert).map_err(|e| {
                        Error::tls(format!("sslrootcert={path}: rejected certificate: {e}").into())
                    })?;
                }
            }
        }

        // Pin the crypto provider explicitly. `ClientConfig::builder()` reads a
        // process-global default and panics when none was installed, which
        // would make this driver's behaviour depend on whether some unrelated
        // main() happened to call `install_default` first.
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = verifier_for(config.get_ssl_mode(), Arc::new(roots), &provider)?;

        // `dangerous()` is rustls saying "you are about to choose the
        // verification policy yourself", and that is precisely what a
        // PostgreSQL driver has to do: two of libpq's six modes are weaker than
        // rustls' default and cannot be expressed any other way. The policy
        // came from `verifier_for` one line up, which is the single audited
        // site; nothing else in this crate calls `dangerous()`.
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::tls(Box::new(e)))?
            .dangerous()
            .with_custom_certificate_verifier(verifier);

        let client_config = match (config.get_ssl_cert(), config.get_ssl_key()) {
            (Some(cert_path), Some(key_path)) => {
                let certs = CertificateDer::pem_file_iter(cert_path)
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| {
                        Error::tls(format!("sslcert={cert_path}: cannot read PEM: {e}").into())
                    })?;
                let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| {
                    Error::tls(format!("sslkey={key_path}: cannot read PEM: {e}").into())
                })?;
                builder
                    .with_client_auth_cert(certs, key)
                    .map_err(|e| Error::tls(Box::new(e)))?
            }
            (None, None) => builder.with_no_client_auth(),
            (Some(_), None) => {
                return Err(Error::tls(
                    "sslcert was given without sslkey; client-certificate authentication needs \
                     both"
                        .into(),
                ));
            }
            (None, Some(_)) => {
                return Err(Error::tls(
                    "sslkey was given without sslcert; client-certificate authentication needs \
                     both"
                        .into(),
                ));
            }
        };

        Ok(MakeRustlsConnect::new(Arc::new(client_config)))
    }
}

impl<S> MakeTlsConnect<S> for MakeRustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, io::Error> {
        // The domain is validated in `connect`, not here. `connect.rs` calls
        // this with `""` for a Unix-socket host and lets `connect_tls` decide
        // what an absent hostname means for the current `sslmode`; failing
        // early here would turn a `prefer` Unix-socket connection - which
        // legitimately falls back to plaintext - into a hard error.
        Ok(RustlsConnect {
            config: self.config.clone(),
            domain: domain.to_string(),
        })
    }
}

/// A single rustls handshake, produced by [`MakeRustlsConnect`].
pub struct RustlsConnect {
    config: Arc<ClientConfig>,
    domain: String,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<RustlsStream<S>, io::Error>>>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let server_name = ServerName::try_from(self.domain.clone())
                .map_err(|e| io::Error::other(format!("invalid TLS hostname: {e}")))?;
            let connector = futures_rustls::TlsConnector::from(self.config);
            let tls = connector
                .connect(server_name, AsyncStream::new(stream))
                .await?;

            // Read the channel-binding material off the rustls session before
            // handing the stream to compio-tls, which exposes only the
            // negotiated ALPN.
            let tls_server_end_point = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(<[CertificateDer<'_>]>::first)
                .and_then(tls_server_end_point);

            Ok(RustlsStream {
                inner: compio_tls::TlsStream::from(tls),
                tls_server_end_point,
            })
        })
    }
}

/// A TLS-wrapped connection produced by [`RustlsConnect`].
pub struct RustlsStream<S> {
    inner: compio_tls::TlsStream<S>,
    tls_server_end_point: Option<Vec<u8>>,
}

impl<S: AsyncRead + AsyncWrite + 'static> AsyncRead for RustlsStream<S> {
    async fn read<B: compio::buf::IoBufMut>(&mut self, buf: B) -> compio::BufResult<usize, B> {
        self.inner.read(buf).await
    }
}

impl<S: AsyncRead + AsyncWrite + 'static> AsyncWrite for RustlsStream<S> {
    async fn write<B: compio::buf::IoBuf>(&mut self, buf: B) -> compio::BufResult<usize, B> {
        self.inner.write(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

impl<S: AsyncRead + AsyncWrite + 'static> TlsStream for RustlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        match &self.tls_server_end_point {
            Some(hash) => ChannelBinding::tls_server_end_point(hash.clone()),
            None => ChannelBinding::none(),
        }
    }
}

/// Compute `tls-server-end-point` for a server certificate (RFC 5929 section 4.1).
///
/// The binding is a hash of the DER certificate, and the hash is the one named
/// by the certificate's own `signatureAlgorithm`, except that MD5 and SHA-1 are
/// upgraded to SHA-256. This mirrors the server side, `be-secure-openssl.c`'s
/// `be_tls_get_certificate_hash`, which reads the same OID and applies the same
/// MD5/SHA-1 upgrade.
///
/// Returns `None` when the signature algorithm names no hash we can reproduce -
/// RSASSA-PSS (whose hash lives in the algorithm parameters, not the OID) and
/// Ed25519 are the realistic cases. `None` propagates to
/// `ChannelBinding::none()`, so `channel_binding=require` fails loudly instead
/// of authenticating against a hash the server did not compute. PostgreSQL
/// refuses those certificates for channel binding too, so the two sides agree
/// that the connection has no binding.
fn tls_server_end_point(cert: &CertificateDer<'_>) -> Option<Vec<u8>> {
    use sha2::Digest;

    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    // Dotted OIDs, checked against RFC 8017 A.2.4 (RSA PKCS#1 v1.5), RFC 5758
    // (ECDSA, DSA) and RFC 5754 (SHA-2). MD5/SHA-1 signatures map to SHA-256
    // per RFC 5929 4.1.
    let der = cert.as_ref();
    match parsed.signature_algorithm.algorithm.to_id_string().as_str() {
        // md5WithRSAEncryption, sha1WithRSAEncryption, sha256WithRSAEncryption
        "1.2.840.113549.1.1.4" | "1.2.840.113549.1.1.5" | "1.2.840.113549.1.1.11"
        // id-dsa-with-sha1, ecdsa-with-SHA1, ecdsa-with-SHA256
        | "1.2.840.10040.4.3" | "1.2.840.10045.4.1" | "1.2.840.10045.4.3.2"
        // dsa-with-sha256
        | "2.16.840.1.101.3.4.3.2" => Some(sha2::Sha256::digest(der).to_vec()),
        // sha384WithRSAEncryption, ecdsa-with-SHA384
        "1.2.840.113549.1.1.12" | "1.2.840.10045.4.3.3" => {
            Some(sha2::Sha384::digest(der).to_vec())
        }
        // sha512WithRSAEncryption, ecdsa-with-SHA512
        "1.2.840.113549.1.1.13" | "1.2.840.10045.4.3.4" => {
            Some(sha2::Sha512::digest(der).to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    /// The CA that signed [`SERVER_LOCALHOST`], and nothing else.
    const CA: &str = include_str!("../tests/data/verifier_ca.pem");
    /// A server certificate whose only subject-alternative name is `localhost`.
    const SERVER_LOCALHOST: &str = include_str!("../tests/data/verifier_server_localhost.pem");

    /// 2030-01-01T00:00:00Z, comfortably inside both fixtures' validity
    /// (2026-08-19 to 2126-07-26).
    ///
    /// A fixed instant, not `UnixTime::now()`, so these tests cannot start
    /// failing on a Tuesday because a certificate expired. If the fixtures are
    /// ever regenerated, keep this constant inside the new validity window.
    const AT: UnixTime = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_893_456_000));

    fn provider() -> CryptoProvider {
        rustls::crypto::aws_lc_rs::default_provider()
    }

    fn roots_with(pem: &str) -> Arc<RootCertStore> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(pem.as_bytes()).unwrap())
            .unwrap();
        Arc::new(roots)
    }

    /// Run the verifier a given `sslmode` really gets against the fixture
    /// server certificate, presented under `name`.
    ///
    /// Goes through `verifier_for`, never around it. That is the whole point:
    /// a test that constructed the verifiers directly would still pass if the
    /// selection were rewired to hand `verify-full` a permissive one.
    fn verify_as(
        mode: SslMode,
        roots: Arc<RootCertStore>,
        name: &'static str,
    ) -> Result<(), String> {
        let verifier = verifier_for(mode, roots, &provider()).map_err(|e| e.to_string())?;
        let cert = CertificateDer::from_pem_slice(SERVER_LOCALHOST.as_bytes()).unwrap();
        verifier
            .verify_server_cert(&cert, &[], &ServerName::try_from(name).unwrap(), &[], AT)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The mode-to-policy table, asserted arm by arm.
    ///
    /// This is one half of the mutation proof. Swapping the `VerifyFull` arm of
    /// `VerifyPolicy::select` for a weaker policy fails here; it is the
    /// cheapest place such a change can be caught, and it needs no
    /// certificates.
    ///
    /// It does NOT prove the policies *do* anything - a `Chain` variant wired
    /// to a no-op verifier would still satisfy every assertion below. That is
    /// what `the_three_policies_discriminate` is for.
    #[test]
    fn policy_selection_follows_mode_and_whether_roots_are_configured() {
        use SslMode::*;
        use VerifyPolicy::*;

        // No trust anchors: the three weak modes encrypt without
        // authenticating, and the two verifying modes refuse to run at all.
        for mode in [Require, Prefer, Allow] {
            assert_eq!(
                VerifyPolicy::select(mode, false).unwrap(),
                AcceptAny,
                "sslmode={} with no sslrootcert must not claim to verify",
                mode.as_str()
            );
        }
        for mode in [VerifyCa, VerifyFull] {
            VerifyPolicy::select(mode, false)
                .expect_err("a verifying mode with no trust anchors must be an error");
        }

        // Trust anchors configured: everything checks the chain, and exactly
        // one mode also checks the host name.
        for mode in [Require, Prefer, Allow, VerifyCa] {
            assert_eq!(
                VerifyPolicy::select(mode, true).unwrap(),
                Chain,
                "sslmode={} with sslrootcert must check the chain and NOT the host name",
                mode.as_str()
            );
        }
        assert_eq!(
            VerifyPolicy::select(VerifyFull, true).unwrap(),
            ChainAndHostname
        );

        VerifyPolicy::select(Disable, true).expect_err("disable has no verification policy");
    }

    /// The three policies, driven through `verifier_for` against real
    /// certificates, and shown to reach *different* verdicts.
    ///
    /// The discriminating pair is the last two assertions: the SAME
    /// certificate, presented under the SAME wrong name, accepted by
    /// `verify-ca` and rejected by `verify-full`. One variable differs. A
    /// verifier that did nothing would pass both, and a verifier that checked
    /// the name in both would fail the `verify-ca` case, so neither degenerate
    /// implementation survives.
    ///
    /// This is the other half of the mutation proof, and the half that catches
    /// a swap of the verifier *object* rather than of the selection.
    #[test]
    fn the_three_policies_discriminate() {
        let ca = roots_with(CA);

        // Baseline: the certificate really is valid for `localhost` under the
        // strictest policy. Without this, every rejection below could be
        // explained by a broken fixture rather than by a working check.
        verify_as(SslMode::VerifyFull, ca.clone(), "localhost")
            .expect("verify-full accepts the right name signed by the configured CA");

        // Chain checking is real: the same certificate, the same right name,
        // but the CA is not among the trust anchors.
        let foreign = roots_with(SERVER_LOCALHOST);
        verify_as(SslMode::VerifyCa, foreign.clone(), "localhost")
            .expect_err("verify-ca must reject a chain that does not reach a configured anchor");
        verify_as(SslMode::VerifyFull, foreign.clone(), "localhost")
            .expect_err("verify-full must reject a chain that does not reach a configured anchor");

        // `require` with no anchors accepts what both of those rejected. This
        // is libpq's "encrypted, unverified", and it is deliberate.
        verify_as(SslMode::Require, Arc::new(RootCertStore::empty()), "wrong.example")
            .expect("sslmode=require without sslrootcert performs no verification");

        // THE PAIR. Same certificate, same wrong name, same trust anchors;
        // only the mode differs, and the verdicts must differ with it.
        verify_as(SslMode::VerifyCa, ca.clone(), "wrong.example")
            .expect("verify-ca must NOT check the host name");
        verify_as(SslMode::VerifyFull, ca, "wrong.example")
            .expect_err("verify-full must reject a host name the certificate does not cover");
    }

    /// `require` and `prefer` are upgraded to chain checking by the presence of
    /// trust anchors, exactly as libpq's `have_rootcert` does it.
    ///
    /// The brief this work started from asserted that `require` never verifies.
    /// It does, whenever it has something to verify against, and this is the
    /// test that pins the corrected reading.
    #[test]
    fn a_configured_ca_upgrades_require_and_prefer_to_chain_checking() {
        let foreign = roots_with(SERVER_LOCALHOST);
        for mode in [SslMode::Require, SslMode::Prefer, SslMode::Allow] {
            verify_as(mode, Arc::new(RootCertStore::empty()), "localhost")
                .unwrap_or_else(|e| panic!("sslmode={} unverified must accept: {e}", mode.as_str()));
            verify_as(mode, foreign.clone(), "localhost").unwrap_err();
        }
    }

    /// The three-branch hash selection, driven by real certificates rather
    /// than by hand-written OID bytes.
    ///
    /// This checks only that the selection *discriminates*: a SHA-256-signed
    /// certificate hashes to 32 bytes with SHA-256, a SHA-384-signed one to 48
    /// with SHA-384. It does NOT check the value against a PostgreSQL server -
    /// that is what the live SCRAM-PLUS handshake in `tests/tls_rustls.rs`
    /// does.
    #[test]
    fn end_point_hash_follows_the_signature_algorithm() {
        for (pem, expected_len) in [
            (include_str!("../tests/data/sha256_cert.pem"), 32),
            (include_str!("../tests/data/sha384_cert.pem"), 48),
        ] {
            let cert = CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
            let hash = tls_server_end_point(&cert).expect("hash for a SHA-2 RSA certificate");
            assert_eq!(hash.len(), expected_len, "wrong digest width for {pem:.60}");

            let mut expected = if expected_len == 32 {
                sha2::Sha256::digest(cert.as_ref()).to_vec()
            } else {
                sha2::Sha384::digest(cert.as_ref()).to_vec()
            };
            assert_eq!(hash, std::mem::take(&mut expected));
        }
    }

    /// An Ed25519 certificate names no hash in its signature OID. We must
    /// report "no binding" rather than defaulting to SHA-256, because the
    /// server would not compute one either.
    #[test]
    fn end_point_hash_absent_for_ed25519() {
        let pem = include_str!("../tests/data/ed25519_cert.pem");
        let cert = CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
        assert!(tls_server_end_point(&cert).is_none());
    }
}
