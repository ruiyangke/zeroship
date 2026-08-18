//! A rustls-backed [`MakeTlsConnect`] for compio-postgres.
//!
//! This is the implementation the [`tls`](crate) feature's name promises: the
//! seam in [`crate::tls`] has always accepted an external backend, and this is
//! one. Nothing in `tls.rs`, `connect_tls.rs` or `maybe_tls_stream.rs` changed
//! to accommodate it.
//!
//! # Trust anchors
//!
//! Verification is always on. There is no "accept any certificate" switch and
//! no `danger_accept_invalid_certs`, because a connection that is encrypted but
//! unauthenticated buys nothing against an attacker who can reach the socket -
//! and every deployment shape we have to serve is reachable without one:
//!
//! * a public or managed Postgres whose CA is in the OS store -> the default,
//!   [`SslRootCert::System`];
//! * a private CA, or a self-signed server certificate -> `sslrootcert=<path>`,
//!   which makes that file the *entire* set of trusted roots.
//!
//! # Channel binding
//!
//! [`RustlsStream`] implements `tls-server-end-point` (RFC 5929 section 4.1), so
//! `channel_binding=require` and SCRAM-SHA-256-PLUS work over a rustls
//! connection. See `tls_server_end_point` for the certificates that are *not*
//! covered - it reports no binding rather than a wrong one.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use compio::io::compat::AsyncStream;
use compio::io::{AsyncRead, AsyncWrite};
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

use crate::Error;
use crate::config::{Config, SslRootCert};
use crate::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};

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
        let mut roots = rustls::RootCertStore::empty();
        match config.get_ssl_root_cert() {
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
        let builder = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::tls(Box::new(e)))?
        .with_root_certificates(roots);

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
