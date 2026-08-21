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

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use compio::io::compat::AsyncStream;
use compio::io::{AsyncRead, AsyncWrite};
use pkcs8::der::pem::PemLabel;
use pkcs8::{EncryptedPrivateKeyInfo, SecretDocument};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{ResolvesClientCert, VerifierBuilderError, WebPkiServerVerifier};
use rustls::crypto::{
    CryptoProvider, KeyProvider, WebPkiSupportedAlgorithms, verify_tls12_signature,
    verify_tls13_signature,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{
    CertificateDer, CertificateRevocationListDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
    SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::sign::{CertifiedKey, SigningKey};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureAlgorithm,
    SignatureScheme, SupportedProtocolVersion,
};
use sha1::Digest as _;
use x509_parser::asn1_rs::{Any, Class, Tag, ToDer};
use zeroize::Zeroize;

use crate::Error;
use crate::config::{Config, SslCertMode, SslMode, SslProtocolVersion, SslRootCert};
use crate::tls::{
    ChannelBinding, ClientCertStatus, MakeTlsConnect, TlsConnect, TlsStream,
};

/// `PostgreSQL`'s registered ALPN protocol identifier.
const POSTGRESQL_ALPN_PROTOCOL: &[u8] = b"postgresql";

const TLS12_ONLY: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];
const TLS13_ONLY: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];
const TLS12_AND_TLS13: &[&SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

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

#[derive(Default)]
struct ConfiguredCrls {
    file: Vec<CertificateRevocationListDer<'static>>,
    directory: Option<ConfiguredCrlDirectory>,
}

struct ConfiguredCrlDirectory {
    path: String,
    loaded: Result<Vec<CertificateRevocationListDer<'static>>, String>,
}

#[derive(Debug)]
struct CrlDirectoryReload {
    mode: SslMode,
    roots: Arc<RootCertStore>,
    file: Vec<CertificateRevocationListDer<'static>>,
    directory_path: String,
    provider: Arc<CryptoProvider>,
}

impl CrlDirectoryReload {
    fn verifier(&self) -> Result<Arc<dyn ServerCertVerifier>, Error> {
        verifier_for(
            self.mode,
            self.roots.clone(),
            ConfiguredCrls {
                file: self.file.clone(),
                directory: Some(ConfiguredCrlDirectory {
                    path: self.directory_path.clone(),
                    loaded: crls_from_hashed_directory(Path::new(&self.directory_path)),
                }),
            },
            &self.provider,
        )
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
    crls: ConfiguredCrls,
    provider: &CryptoProvider,
) -> Result<Arc<dyn ServerCertVerifier>, Error> {
    let algorithms = provider.signature_verification_algorithms;
    // "Configured" is asked of the store, not of `SslRootCert`, and the two
    // cannot disagree: `from_config` loads nothing for `Unset`, and errors
    // rather than returning an empty store for `System` or `File`. Asking the
    // store is the safer of the two identical questions, because a store with
    // no anchors could not verify anything even if a path had been named.
    let policy = VerifyPolicy::select(mode, !roots.is_empty())?;
    if policy == VerifyPolicy::AcceptAny {
        return Ok(Arc::new(AcceptAnyServerCert { algorithms }));
    }

    let builder = || {
        WebPkiServerVerifier::builder_with_provider(roots.clone(), Arc::new(provider.clone()))
    };

    // Preserve sslcrl's libpq behaviour: a file OpenSSL cannot load is
    // ignored. Validate it alone before combining it with the directory so an
    // invalid file cannot make a valid directory fail open.
    let mut file_crls = crls.file;
    if !file_crls.is_empty() {
        match builder().with_crls(file_crls.clone()).build() {
            Ok(_) => {}
            Err(VerifierBuilderError::InvalidCrl(_)) => file_crls.clear(),
            Err(error) => return Err(Error::tls(Box::new(error))),
        }
    }
    let file_issuers = distinct_crl_issuers(&file_crls).map_err(|reason| {
        Error::tls(format!("sslcrl: {reason}").into())
    })?;
    ensure_crls_started(&file_crls).map_err(|reason| {
        Error::tls(format!("sslcrl: {reason}").into())
    })?;

    let (all_crls, directory_path) = match crls.directory {
        Some(directory) => {
            let directory_crls = directory.loaded.map_err(|reason| {
                Error::tls(format!("sslcrldir={}: {reason}", directory.path).into())
            })?;
            if file_crls.is_empty() && directory_crls.is_empty() {
                return Err(Error::tls(
                    format!(
                        "sslcrldir={}: directory contains no usable OpenSSL-hashed PEM CRLs",
                        directory.path
                    )
                    .into(),
                ));
            }
            let directory_issuers = distinct_crl_issuers(&directory_crls).map_err(|reason| {
                Error::tls(format!("sslcrldir={}: {reason}", directory.path).into())
            })?;
            if !file_issuers.is_disjoint(&directory_issuers) {
                return Err(Error::tls(
                    format!(
                        "sslcrl and sslcrldir={} contain CRLs for the same issuer; rustls cannot \
                         reproduce OpenSSL's current-CRL selection without risking stale \
                         revocation data",
                        directory.path
                    )
                    .into(),
                ));
            }
            file_crls.extend(directory_crls);
            (file_crls, Some(directory.path))
        }
        None => (file_crls, None),
    };

    let verifier = if all_crls.is_empty() {
        builder().build().map_err(|error| Error::tls(Box::new(error)))?
    } else {
        match builder()
            .with_crls(all_crls)
            .enforce_revocation_expiration()
            .build()
        {
            Ok(verifier) => verifier,
            Err(VerifierBuilderError::InvalidCrl(error)) if directory_path.is_some() => {
                return Err(Error::tls(
                    format!(
                        "sslcrldir={}: rustls rejected a CRL: {error:?}",
                        directory_path.expect("the guard proved the path is present")
                    )
                    .into(),
                ));
            }
            Err(error) => return Err(Error::tls(Box::new(error))),
        }
    };

    Ok(match policy {
        VerifyPolicy::Chain => Arc::new(ChainOnlyServerCert { verifier }),
        VerifyPolicy::ChainAndHostname => verifier,
        VerifyPolicy::AcceptAny => unreachable!("handled above"),
    })
}

/// [`VerifyPolicy::Chain`]: the chain is checked against the configured trust
/// anchors, the host name is not.
///
/// The inner webpki verifier checks the chain and CRLs before the host name.
/// Only its host-name error is suppressed; every chain, expiry, purpose, and
/// revocation error is returned unchanged. A custom verifier is needed because
/// [`WebPkiServerVerifier`] has no knob to disable only the name check. The
/// combined wrong-name/revoked-certificate case in `tests/tls_live.rs` guards
/// that load-bearing order against a rustls change.
#[derive(Debug)]
struct ChainOnlyServerCert {
    verifier: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for ChainOnlyServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.verifier.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            Err(error) => Err(error),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier.supported_verify_schemes()
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

/// Import a private key into aws-lc, then erase rustls' owned DER copy.
#[derive(Debug)]
struct ZeroizingAwsLcKeyProvider;

impl KeyProvider for ZeroizingAwsLcKeyProvider {
    fn load_private_key(
        &self,
        mut key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn rustls::sign::SigningKey>, rustls::Error> {
        let result = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key_der);
        key_der.zeroize();
        result
    }

    fn fips(&self) -> bool {
        rustls::crypto::aws_lc_rs::default_provider()
            .key_provider
            .fips()
    }
}

static ZEROIZING_AWS_LC_KEY_PROVIDER: ZeroizingAwsLcKeyProvider = ZeroizingAwsLcKeyProvider;

/// Load a private key without letting the presence of `sslpassword` change the
/// meaning of an ordinary plaintext key.
fn private_key_from_config(
    key_path: &str,
    password: Option<&[u8]>,
) -> Result<PrivateKeyDer<'static>, Error> {
    let unencrypted_error = match PrivateKeyDer::from_pem_file(key_path) {
        Ok(key) => return Ok(key),
        Err(error) => error,
    };

    // rustls-pki-types intentionally ignores ENCRYPTED PRIVATE KEY sections.
    // Parse only that one additional label here; malformed plaintext keys must
    // retain the existing rustls PEM error instead of being misreported as a
    // bad passphrase.
    let pem = match std::fs::read_to_string(key_path) {
        Ok(pem) => pem,
        Err(_) => {
            return Err(Error::tls(
                format!("sslkey={key_path}: cannot read PEM: {unencrypted_error}").into(),
            ));
        }
    };
    let (label, encrypted_document) = match SecretDocument::from_pem(&pem) {
        Ok(document) => document,
        Err(_) => {
            return Err(Error::tls(
                format!("sslkey={key_path}: cannot read PEM: {unencrypted_error}").into(),
            ));
        }
    };
    if EncryptedPrivateKeyInfo::validate_pem_label(label).is_err() {
        return Err(Error::tls(
            format!("sslkey={key_path}: cannot read PEM: {unencrypted_error}").into(),
        ));
    }

    let password = password
        .filter(|password| !password.is_empty())
        .ok_or_else(|| {
            Error::tls(
                format!(
                    "sslkey={key_path}: encrypted private key requires a non-empty sslpassword"
                )
                .into(),
            )
        })?;
    let encrypted =
        EncryptedPrivateKeyInfo::try_from(encrypted_document.as_bytes()).map_err(|error| {
            Error::tls(format!("sslkey={key_path}: invalid encrypted PKCS#8 key: {error}").into())
        })?;
    let cleartext = encrypted.decrypt(password).map_err(|error| {
        Error::tls(
            format!(
                "sslkey={key_path}: cannot decrypt encrypted PKCS#8 key with sslpassword: {error}"
            )
            .into(),
        )
    })?;

    Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        cleartext.as_bytes().to_vec(),
    )))
}

fn protocol_versions_from_config(
    config: &Config,
) -> Result<&'static [&'static SupportedProtocolVersion], Error> {
    config.validate_ssl_protocol_version_range()?;

    Ok(match (
        config.get_ssl_min_protocol_version(),
        config.get_ssl_max_protocol_version(),
    ) {
        (SslProtocolVersion::TlsV1_2, Some(SslProtocolVersion::TlsV1_2)) => TLS12_ONLY,
        (SslProtocolVersion::TlsV1_2, None | Some(SslProtocolVersion::TlsV1_3)) => {
            TLS12_AND_TLS13
        }
        (SslProtocolVersion::TlsV1_3, None | Some(SslProtocolVersion::TlsV1_3)) => TLS13_ONLY,
        (SslProtocolVersion::TlsV1_3, Some(SslProtocolVersion::TlsV1_2)) => {
            unreachable!("the inverted range was rejected above")
        }
    })
}

fn crls_from_pem_file(path: &Path) -> Option<Vec<CertificateRevocationListDer<'static>>> {
    let crls = CertificateRevocationListDer::pem_file_iter(path)
        .and_then(|crls| crls.collect::<Result<Vec<_>, _>>())
        .ok()?;
    (!crls.is_empty()).then_some(crls)
}

fn der_tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(contents.len() + 8);
    encoded.push(tag);
    if contents.len() < 128 {
        encoded.push(contents.len() as u8);
    } else {
        let width = (usize::BITS - contents.len().leading_zeros()).div_ceil(8) as usize;
        encoded.push(0x80 | width as u8);
        for shift in (0..width).rev() {
            encoded.push((contents.len() >> (shift * 8)) as u8);
        }
    }
    encoded.extend_from_slice(contents);
    encoded
}

fn openssl_canonical_string(value: &Any<'_>) -> Result<Option<Vec<u8>>, String> {
    if value.class() != Class::Universal || value.header.constructed() {
        return Ok(None);
    }

    let utf8 = match value.tag() {
        Tag::Utf8String => std::str::from_utf8(value.as_bytes())
            .map_err(|error| format!("invalid UTF8String in CRL issuer: {error}"))?
            .as_bytes()
            .to_vec(),
        Tag::PrintableString | Tag::Ia5String | Tag::VisibleString => value.as_bytes().to_vec(),
        Tag::TeletexString => value
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>()
            .into_bytes(),
        Tag::BmpString => {
            if value.as_bytes().len() % 2 != 0 {
                return Err("odd-length BMPString in CRL issuer".to_string());
            }
            value
                .as_bytes()
                .chunks_exact(2)
                .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]) as u32)
                .map(|scalar| {
                    char::from_u32(scalar)
                        .ok_or_else(|| "invalid BMPString scalar in CRL issuer".to_string())
                })
                .collect::<Result<String, _>>()?
                .into_bytes()
        }
        Tag::UniversalString => {
            if value.as_bytes().len() % 4 != 0 {
                return Err("misaligned UniversalString in CRL issuer".to_string());
            }
            value
                .as_bytes()
                .chunks_exact(4)
                .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .map(|scalar| {
                    char::from_u32(scalar)
                        .ok_or_else(|| "invalid UniversalString scalar in CRL issuer".to_string())
                })
                .collect::<Result<String, _>>()?
                .into_bytes()
        }
        _ => return Ok(None),
    };

    let is_space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let Some(first) = utf8.iter().position(|byte| !is_space(*byte)) else {
        return Ok(Some(Vec::new()));
    };
    let last = utf8
        .iter()
        .rposition(|byte| !is_space(*byte))
        .expect("the first non-space byte proved one exists");
    let mut canonical = Vec::with_capacity(last - first + 1);
    let mut previous_was_space = false;
    for byte in &utf8[first..=last] {
        if is_space(*byte) {
            if !previous_was_space {
                canonical.push(b' ');
                previous_was_space = true;
            }
        } else {
            canonical.push(byte.to_ascii_lowercase());
            previous_was_space = false;
        }
    }
    Ok(Some(canonical))
}

/// OpenSSL hashes a canonicalised X509_NAME, not the issuer's original DER.
/// Reproducing those bytes is what makes a rehashed filename meaningful here.
fn openssl_crl_issuer(
    crl: &CertificateRevocationListDer<'_>,
) -> Result<(Vec<u8>, i64), String> {
    let (remaining, parsed) = x509_parser::parse_x509_crl(crl.as_ref())
        .map_err(|error| format!("cannot parse CRL issuer: {error}"))?;
    if !remaining.is_empty() {
        return Err("trailing data after CRL".to_string());
    }

    let mut canonical_name = Vec::new();
    for rdn in parsed.issuer().iter_rdn() {
        let mut attributes = Vec::new();
        for attribute in rdn.iter() {
            let mut contents = attribute
                .attr_type()
                .to_der_vec()
                .map_err(|error| format!("cannot encode CRL issuer OID: {error}"))?;
            let value = match openssl_canonical_string(attribute.attr_value())? {
                Some(utf8) => der_tlv(Tag::Utf8String.0 as u8, &utf8),
                None => attribute
                    .attr_value()
                    .to_der_vec()
                    .map_err(|error| format!("cannot encode CRL issuer value: {error}"))?,
            };
            contents.extend(value);
            attributes.push(der_tlv(0x30, &contents));
        }
        attributes.sort();
        let contents = attributes.into_iter().flatten().collect::<Vec<_>>();
        canonical_name.extend(der_tlv(0x31, &contents));
    }

    Ok((canonical_name, parsed.last_update().timestamp()))
}

fn distinct_crl_issuers(
    crls: &[CertificateRevocationListDer<'_>],
) -> Result<BTreeSet<Vec<u8>>, String> {
    let mut issuers = BTreeSet::new();
    for crl in crls {
        let (issuer, _) = openssl_crl_issuer(crl)?;
        if !issuers.insert(issuer) {
            return Err(
                "multiple CRLs have the same issuer; rustls would use the first and could miss a \
                 newer revocation"
                    .to_string(),
            );
        }
    }
    Ok(issuers)
}

fn ensure_crls_started(crls: &[CertificateRevocationListDer<'_>]) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("cannot check CRL thisUpdate against system time: {error}"))?;
    let now = i64::try_from(now.as_secs())
        .map_err(|_| "system time cannot be represented as an X.509 timestamp".to_string())?;
    for crl in crls {
        let (_, this_update) = openssl_crl_issuer(crl)?;
        if this_update > now {
            return Err("CRL thisUpdate is in the future".to_string());
        }
    }
    Ok(())
}

fn openssl_crl_issuer_hash(crl: &CertificateRevocationListDer<'_>) -> Result<String, String> {
    ensure_crls_started(std::slice::from_ref(crl))?;
    let (canonical_name, _) = openssl_crl_issuer(crl)?;

    let digest = sha1::Sha1::digest(&canonical_name);
    Ok(format!(
        "{:08x}",
        u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
    ))
}

/// Parse only lookup names OpenSSL generates, so an unhashed source `.crl`
/// beside them cannot accidentally become active policy.
fn hashed_crl_name(name: &OsStr) -> Option<(&str, usize)> {
    let name = name.to_str()?;
    let hash = name.get(..8)?;
    let suffix = name.get(8..)?;
    if !hash
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let digits = suffix.strip_prefix(".r")?;
    let index = digits.parse::<usize>().ok()?;
    if index.to_string() != digits {
        return None;
    }
    Some((hash, index))
}

/// Read one snapshot of an OpenSSL-style hashed CRL directory.
fn crls_from_hashed_directory(
    path: &Path,
) -> Result<Vec<CertificateRevocationListDer<'static>>, String> {
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("cannot read directory: {error}"))?;
    let mut by_issuer = BTreeMap::<String, BTreeMap<usize, PathBuf>>::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read directory entry: {error}"))?;
        let name = entry.file_name();
        let Some((hash, index)) = hashed_crl_name(&name) else {
            continue;
        };
        by_issuer
            .entry(hash.to_string())
            .or_default()
            .insert(index, entry.path());
    }

    let mut loaded = Vec::new();
    for (hash, entries) in &by_issuer {
        let mut expected = 0;
        while let Some(entry_path) = entries.get(&expected) {
            // OpenSSL advances through a hash collision only while each
            // preceding entry exists. A gap ends that issuer's lookup.
            let Some(mut crls) = crls_from_pem_file(entry_path) else {
                return Err(format!(
                    "cannot load hashed PEM CRL entry {}",
                    entry_path.display()
                ));
            };
            if crls.len() != 1 {
                return Err(format!(
                    "hashed CRL entry {} must contain exactly one PEM CRL",
                    entry_path.display()
                ));
            }
            for crl in &crls {
                let actual_hash = openssl_crl_issuer_hash(crl)?;
                if actual_hash != *hash {
                    return Err(format!(
                        "hashed CRL entry {} has issuer hash {actual_hash}, not {hash}",
                        entry_path.display()
                    ));
                }
            }
            loaded.append(&mut crls);
            expected += 1;
        }
    }
    Ok(loaded)
}

/// Read libpq's optional CRL sources without turning either into a trust anchor.
fn crls_from_config(config: &Config) -> ConfiguredCrls {
    let file = config
        .get_ssl_crl()
        .filter(|path| !path.is_empty())
        .and_then(|path| crls_from_pem_file(Path::new(path)))
        .unwrap_or_default();
    let directory = config
        .get_ssl_crl_dir()
        .filter(|path| !path.is_empty())
        .map(|path| ConfiguredCrlDirectory {
            path: path.to_string(),
            loaded: crls_from_hashed_directory(Path::new(path)),
        });

    ConfiguredCrls { file, directory }
}

/// Per-handshake evidence for `sslcertmode=require`.
///
/// This cannot live on [`MakeRustlsConnect`]: pools may perform concurrent
/// handshakes, and one connection's certificate request must never satisfy
/// another connection's requirement.
#[derive(Debug, Default)]
struct ClientCertObservation {
    requested: AtomicBool,
    sent: AtomicBool,
}

impl ClientCertObservation {
    fn status(&self) -> ClientCertStatus {
        if self.sent.load(Ordering::Relaxed) {
            ClientCertStatus::Sent
        } else if self.requested.load(Ordering::Relaxed) {
            ClientCertStatus::NotSent
        } else {
            ClientCertStatus::NotRequested
        }
    }
}

/// Records the server's CertificateRequest and wraps the selected key so the
/// later signature-scheme decision is observable too.
#[derive(Debug)]
struct ObservingClientCertResolver {
    inner: Arc<dyn ResolvesClientCert>,
    observation: Arc<ClientCertObservation>,
}

impl ResolvesClientCert for ObservingClientCertResolver {
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        self.observation.requested.store(true, Ordering::Relaxed);
        let selected = self.inner.resolve(root_hint_subjects, sigschemes)?;
        Some(Arc::new(CertifiedKey {
            cert: selected.cert.clone(),
            key: Arc::new(ObservingSigningKey {
                inner: selected.key.clone(),
                observation: self.observation.clone(),
            }),
            ocsp: selected.ocsp.clone(),
        }))
    }

    fn only_raw_public_keys(&self) -> bool {
        self.inner.only_raw_public_keys()
    }

    fn has_certs(&self) -> bool {
        self.inner.has_certs()
    }
}

/// Marks a certificate as sent only when rustls finds a signature scheme it
/// can actually use. Resolver success alone is insufficient: rustls sends an
/// empty Certificate message when `choose_scheme` returns `None`.
#[derive(Debug)]
struct ObservingSigningKey {
    inner: Arc<dyn SigningKey>,
    observation: Arc<ClientCertObservation>,
}

impl SigningKey for ObservingSigningKey {
    fn choose_scheme(
        &self,
        offered: &[SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        let signer = self.inner.choose_scheme(offered)?;
        self.observation.sent.store(true, Ordering::Relaxed);
        Some(signer)
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        self.inner.public_key()
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.inner.algorithm()
    }
}

/// A [`MakeTlsConnect`] that performs handshakes with rustls.
///
/// Build one from a [`Config`] with [`MakeRustlsConnect::from_config`] (which
/// reads the protocol bounds, SNI policy, `sslrootcert`, both CRL sources, and
/// the client-certificate mode and identity), or hand it a fully built
/// [`ClientConfig`] with [`MakeRustlsConnect::new`] when the TLS decisions are
/// made in code rather than in the connection string. The connection path
/// refuses a connector whose SNI or certificate mode does not match its
/// [`Config`].
#[derive(Clone, Debug)]
pub struct MakeRustlsConnect {
    config: Arc<ClientConfig>,
    ssl_cert_mode: SslCertMode,
    /// A CRL directory is mutable verification policy. libpq observes it for
    /// each new connection, so retain only the public verification inputs
    /// needed to rebuild the verifier instead of freezing the first directory
    /// snapshot or extending the lifetime of connection passwords.
    crl_directory_reload: Option<Arc<CrlDirectoryReload>>,
}

impl MakeRustlsConnect {
    /// Wrap an existing rustls client configuration.
    ///
    /// `PostgreSQL` 17 requires the `postgresql` ALPN protocol for direct TLS,
    /// and libpq offers it for traditional `PostgreSQL` TLS negotiation too. To
    /// match that behaviour, an empty [`ClientConfig::alpn_protocols`] list is
    /// replaced with a list containing only `postgresql`.
    ///
    /// A nonempty list is caller-owned configuration and is preserved exactly.
    /// In particular, this method does not append `postgresql`; a caller that
    /// supplies an incompatible list can cause `PostgreSQL` to reject the TLS
    /// handshake, including when `sslnegotiation=direct` is used.
    pub fn new(config: Arc<ClientConfig>) -> MakeRustlsConnect {
        let config = if config.alpn_protocols.is_empty() {
            let mut config = (*config).clone();
            config.alpn_protocols = vec![POSTGRESQL_ALPN_PROTOCOL.to_vec()];
            Arc::new(config)
        } else {
            config
        };
        MakeRustlsConnect {
            config,
            ssl_cert_mode: SslCertMode::Allow,
            crl_directory_reload: None,
        }
    }

    /// Build a connector from the TLS parameters of a connection string.
    ///
    /// Reads the TLS protocol bounds, SNI policy, `sslrootcert` / `sslcrl` /
    /// `sslcrldir` (server authentication), and `sslcertmode` plus the
    /// `sslcert` / `sslkey` / `sslpassword` client-authentication settings.
    /// Except when certificate use is disabled, the certificate and key must
    /// be given together; naming one without the other is an error rather than
    /// a silently ignored half-configuration.
    pub fn from_config(config: &Config) -> Result<MakeRustlsConnect, Error> {
        let protocol_versions = protocol_versions_from_config(config)?;
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
        let mut provider = rustls::crypto::aws_lc_rs::default_provider();
        provider.key_provider = &ZEROIZING_AWS_LC_KEY_PROVIDER;
        let provider = Arc::new(provider);
        let roots = Arc::new(roots);
        let crls = crls_from_config(config);
        let crl_directory_reload = crls.directory.as_ref().map(|directory| {
            Arc::new(CrlDirectoryReload {
                mode: config.get_ssl_mode(),
                roots: roots.clone(),
                file: crls.file.clone(),
                directory_path: directory.path.clone(),
                provider: provider.clone(),
            })
        });
        let verifier = verifier_for(config.get_ssl_mode(), roots, crls, &provider)?;

        // `dangerous()` is rustls saying "you are about to choose the
        // verification policy yourself", and that is precisely what a
        // PostgreSQL driver has to do: two of libpq's six modes are weaker than
        // rustls' default and cannot be expressed any other way. The policy
        // came from `verifier_for` one line up, which is the single audited
        // site; nothing else in this crate calls `dangerous()`.
        let builder = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(protocol_versions)
            .map_err(|e| Error::tls(Box::new(e)))?
            .dangerous()
            .with_custom_certificate_verifier(verifier);

        let mut client_config = match config.get_ssl_cert_mode() {
            // Do not even open the named files. Besides needless failure, that
            // could prompt/decrypt a private key for a mode whose promise is
            // that the identity never leaves the client.
            SslCertMode::Disable => builder.with_no_client_auth(),
            SslCertMode::Allow | SslCertMode::Require => {
                match (config.get_ssl_cert(), config.get_ssl_key()) {
                    (Some(cert_path), Some(key_path)) => {
                        let certs = CertificateDer::pem_file_iter(cert_path)
                            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                            .map_err(|e| {
                                Error::tls(
                                    format!("sslcert={cert_path}: cannot read PEM: {e}").into(),
                                )
                            })?;
                        let key = private_key_from_config(key_path, config.get_ssl_password())?;
                        builder
                            .with_client_auth_cert(certs, key)
                            .map_err(|e| Error::tls(Box::new(e)))?
                    }
                    (None, None) if config.get_ssl_cert_mode() == SslCertMode::Require => {
                        return Err(Error::tls(
                            "sslcertmode=require needs sslcert and sslkey because \
                             compio-postgres does not read libpq's default client-certificate \
                             files"
                                .into(),
                        ));
                    }
                    (None, None) => builder.with_no_client_auth(),
                    (Some(_), None) => {
                        return Err(Error::tls(
                            "sslcert was given without sslkey; client-certificate \
                             authentication needs both"
                                .into(),
                        ));
                    }
                    (None, Some(_)) => {
                        return Err(Error::tls(
                            "sslkey was given without sslcert; client-certificate \
                             authentication needs both"
                                .into(),
                        ));
                    }
                }
            }
        };

        // This field controls emission of the SNI extension without changing
        // the ServerName that rustls still uses for certificate verification.
        client_config.enable_sni = config.get_ssl_sni();

        if crl_directory_reload.is_some() || config.get_ssl_cert_mode() == SslCertMode::Require {
            // Resumption can skip both server-certificate verification and a
            // fresh CertificateRequest. rustls also keys tickets to verifier
            // and client-resolver identity today, but disabling them makes
            // both policies explicit invariants rather than incidental
            // properties of its session-cache implementation.
            client_config.resumption = rustls::client::Resumption::disabled();
        }

        let mut connector = MakeRustlsConnect::new(Arc::new(client_config));
        connector.ssl_cert_mode = config.get_ssl_cert_mode();
        connector.crl_directory_reload = crl_directory_reload;
        Ok(connector)
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
        let config = match &self.crl_directory_reload {
            Some(reload) => {
                let verifier = reload.verifier().map_err(io::Error::other)?;
                let mut config = (*self.config).clone();
                config.dangerous().set_certificate_verifier(verifier);
                Arc::new(config)
            }
            None => self.config.clone(),
        };
        Ok(RustlsConnect {
            config,
            domain: domain.to_string(),
            ssl_cert_mode: self.ssl_cert_mode,
        })
    }
}

/// A single rustls handshake, produced by [`MakeRustlsConnect`].
pub struct RustlsConnect {
    config: Arc<ClientConfig>,
    domain: String,
    ssl_cert_mode: SslCertMode,
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
            let (config, client_cert_observation) =
                if self.ssl_cert_mode == SslCertMode::Require {
                    let observation = Arc::new(ClientCertObservation::default());
                    let mut config = (*self.config).clone();
                    config.client_auth_cert_resolver = Arc::new(ObservingClientCertResolver {
                        inner: config.client_auth_cert_resolver.clone(),
                        observation: observation.clone(),
                    });
                    (Arc::new(config), Some(observation))
                } else {
                    (self.config, None)
                };
            let connector = futures_rustls::TlsConnector::from(config);
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
            let client_cert_status = client_cert_observation
                .as_deref()
                .map_or(ClientCertStatus::Unknown, ClientCertObservation::status);

            Ok(RustlsStream {
                inner: compio_tls::TlsStream::from(tls),
                tls_server_end_point,
                client_cert_status,
            })
        })
    }

    fn can_honor_sslsni(&self, enabled: bool) -> bool {
        self.config.enable_sni == enabled
    }

    fn can_honor_sslcertmode(&self, mode: SslCertMode) -> bool {
        self.ssl_cert_mode == mode
    }
}

/// A TLS-wrapped connection produced by [`RustlsConnect`].
pub struct RustlsStream<S> {
    inner: compio_tls::TlsStream<S>,
    tls_server_end_point: Option<Vec<u8>>,
    client_cert_status: ClientCertStatus,
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

    fn client_cert_status(&self) -> ClientCertStatus {
        self.client_cert_status
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
    use compio::net::{TcpListener, TcpStream};
    use futures_channel::oneshot;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use sha2::Digest;
    use std::sync::Mutex;

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

    type OfferedAlpn = Option<Vec<Vec<u8>>>;

    #[derive(Debug)]
    struct AlpnRecorder {
        sender: Mutex<Option<oneshot::Sender<OfferedAlpn>>>,
    }

    impl ResolvesServerCert for AlpnRecorder {
        fn resolve(
            &self,
            client_hello: ClientHello<'_>,
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            let offered = client_hello
                .alpn()
                .map(|protocols| protocols.map(<[u8]>::to_vec).collect());
            self.sender
                .lock()
                .expect("lock ALPN recorder")
                .take()
                .expect("record exactly one ClientHello")
                .send(offered)
                .expect("deliver offered ALPN protocols");

            // Capturing the real ClientHello is the whole server's job. No
            // certificate is needed after that point, so end the handshake.
            None
        }
    }

    fn client_config(alpn_protocols: Vec<Vec<u8>>) -> ClientConfig {
        let provider = Arc::new(provider());
        let verifier = verifier_for(
            SslMode::Require,
            Arc::new(RootCertStore::empty()),
            ConfiguredCrls::default(),
            &provider,
        )
        .expect("build accept-any test verifier");
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("safe protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = alpn_protocols;
        config
    }

    /// Start a real rustls server, connect the driver's rustls client to it,
    /// and return the protocol list parsed from the serialized `ClientHello`.
    async fn alpn_offered_on_wire(client_config: ClientConfig) -> OfferedAlpn {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ALPN recorder");
        let address = listener.local_addr().expect("ALPN recorder address");
        let (sender, receiver) = oneshot::channel();
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(provider()))
            .with_safe_default_protocol_versions()
            .expect("safe protocol versions")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(AlpnRecorder {
                sender: Mutex::new(Some(sender)),
            }));
        let acceptor = compio_tls::TlsAcceptor::from(Arc::new(server_config));
        let server = compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept rustls client");
            assert!(
                acceptor.accept(stream).await.is_err(),
                "capture-only rustls server must end after ClientHello"
            );
        });

        let stream = TcpStream::connect(address)
            .await
            .expect("connect to rustls server");
        let mut make = MakeRustlsConnect::new(Arc::new(client_config));
        let connector =
            <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(
                &mut make,
                "localhost",
            )
            .expect("make rustls connector");
        assert!(
            connector.connect(stream).await.is_err(),
            "capture-only rustls server must end the client handshake"
        );
        server.await.expect("rustls server task");
        receiver.await.expect("receive offered ALPN protocols")
    }

    #[compio::test]
    async fn empty_client_alpn_offers_postgresql_on_wire() {
        let offered = alpn_offered_on_wire(client_config(Vec::new())).await;
        assert_eq!(
            offered,
            Some(vec![POSTGRESQL_ALPN_PROTOCOL.to_vec()])
        );
    }

    #[compio::test]
    async fn caller_alpn_is_preserved_on_wire() {
        let caller_protocols = vec![b"caller/one".to_vec(), b"caller/two".to_vec()];
        let offered = alpn_offered_on_wire(client_config(caller_protocols.clone())).await;
        assert_eq!(offered, Some(caller_protocols));
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
        let verifier = verifier_for(mode, roots, ConfiguredCrls::default(), &provider())
            .map_err(|e| e.to_string())?;
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
    /// that is what the live SCRAM-PLUS handshake in `tests/tls_live.rs`
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

    #[test]
    fn only_openssl_hashed_crl_names_are_directory_entries() {
        for (name, expected) in [
            ("0123abcd.r0", Some(("0123abcd", 0))),
            ("0123abcd.r12", Some(("0123abcd", 12))),
            ("0123ABCD.r0", None),
            ("0123abcd.r01", None),
            ("0123abcd.crl", None),
            ("server.crl", None),
        ] {
            assert_eq!(hashed_crl_name(OsStr::new(name)), expected, "{name}");
        }
    }

    #[test]
    fn consecutive_hash_entries_distinguish_collisions_from_stale_crls() {
        let pem = include_str!("../tests/data/stale_guard_crl.pem");
        let crl = CertificateRevocationListDer::from_pem_slice(pem.as_bytes())
            .expect("parse the stale-CRL guard fixture");
        let hash = openssl_crl_issuer_hash(&crl).expect("hash the fixture issuer");
        let directory = tempfile::tempdir().expect("create an isolated CRL directory");
        for suffix in [0, 1] {
            std::fs::write(directory.path().join(format!("{hash}.r{suffix}")), pem)
                .expect("write a regular hashed CRL fixture");
        }

        let loaded = crls_from_hashed_directory(directory.path())
            .expect("r1 can represent a real 32-bit issuer-hash collision");
        assert_eq!(loaded.len(), 2);
        let error = distinct_crl_issuers(&loaded)
            .expect_err("the same issuer in r0 and r1 risks selecting a stale CRL");
        assert!(error.contains("same issuer"), "unexpected refusal: {error}");
    }

    #[test]
    fn sslcrl_and_sslcrldir_cannot_select_different_crls_for_one_issuer() {
        let pem = include_str!("../tests/data/stale_guard_crl.pem");
        let crl = CertificateRevocationListDer::from_pem_slice(pem.as_bytes())
            .expect("parse the stale-CRL guard fixture");
        let error = verifier_for(
            SslMode::VerifyFull,
            roots_with(CA),
            ConfiguredCrls {
                file: vec![crl.clone()],
                directory: Some(ConfiguredCrlDirectory {
                    path: "/isolated/crl-directory".to_string(),
                    loaded: Ok(vec![crl]),
                }),
            },
            &provider(),
        )
        .expect_err("rustls first-match selection could miss the newer revocation policy");
        let cause = std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            cause.contains("sslcrl")
                && cause.contains("sslcrldir=")
                && cause.contains("same issuer"),
            "the refusal must name both conflicting settings: {cause}"
        );
    }

    /// libpq treats sslcrl as optional even when the caller named a path: an
    /// absent file or invalid CRL DER leaves revocation checking disabled.
    #[test]
    fn missing_or_invalid_sslcrl_is_ignored_like_libpq() {
        let path = "/definitely/missing/compio-postgres.crl";
        let config = format!("host=h sslcrl={path}").parse::<Config>().unwrap();
        assert!(
            crls_from_config(&config).file.is_empty(),
            "{path} unexpectedly produced a CRL"
        );

        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/verifier_ca.pem");
        let invalid_crl = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/invalid_crl.pem"
        );
        let config = format!(
            "host=h sslmode=verify-full sslrootcert={root} sslcrl={invalid_crl}"
        )
        .parse::<Config>()
        .unwrap();
        MakeRustlsConnect::from_config(&config)
            .expect("libpq ignores a CRL file whose DER cannot be parsed");
    }

    /// A hash directory activates revocation checking only for a verified
    /// connection. Once active, an unusable directory must be a refusal rather
    /// than silently turning the check back off.
    #[test]
    fn unusable_sslcrldir_is_ignored_without_verification_and_named_with_it() {
        let directory = "/definitely/missing/compio-postgres-crls";
        let unverified = format!("host=h sslmode=require sslcrldir={directory}")
            .parse::<Config>()
            .unwrap();
        MakeRustlsConnect::from_config(&unverified)
            .expect("libpq does not consult CRLs when no trust anchors are configured");

        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/verifier_ca.pem");
        let verified = format!(
            "host=h sslmode=verify-full sslrootcert={root} sslcrldir={directory}"
        )
        .parse::<Config>()
        .unwrap();
        let error = MakeRustlsConnect::from_config(&verified)
            .expect_err("an unusable hash directory must not disable revocation checking");
        let cause = std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            cause.contains("sslcrldir=") && cause.contains(directory),
            "the refusal must name the unusable setting: {cause}"
        );
    }

    #[test]
    fn sslcrldir_rebuilds_the_verifier_for_each_new_connection() {
        let config = "host=h sslmode=require sslcrldir=/definitely/missing/reloaded-crls"
            .parse::<Config>()
            .unwrap();
        let mut make = MakeRustlsConnect::from_config(&config)
            .expect("an unverified connection does not consult the missing directory");
        let connector =
            <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(
                &mut make,
                "localhost",
            )
            .expect("rebuild the per-connection verifier");
        assert!(
            !Arc::ptr_eq(&make.config, &connector.config),
            "the pool's initial CRL snapshot was reused"
        );
    }
}
