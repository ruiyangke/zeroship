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
//! trust anchors. `ServerVerification::select` is the *only* place that function is
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
use zeroize::{Zeroize, Zeroizing};

use crate::Error;
use crate::config::{Config, SslCertMode, SslMode, SslProtocolVersion, SslRootCert};
use crate::tls::{
    ChannelBinding, ClientCertStatus, MakeTlsConnect, POSTGRESQL_ALPN_PROTOCOL, ServerVerification,
    TlsConnect, TlsPolicyIdentity, TlsStream,
};
use crate::tls_sansio::{self, SharedSession, TlsReadHalf, TlsStreamCore, TlsWriteHalf, share};

const TLS12_ONLY: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];
const TLS13_ONLY: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];
const TLS12_AND_TLS13: &[&SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

// ---------------------------------------------------------------------------
// Verification policy - the one decision, and the one place it is made
// ---------------------------------------------------------------------------
//
// The policy type and its selection live in `crate::tls` (see
// [`ServerVerification`]), not here, because `connect_raw` has to evaluate the
// same function to hold a NON-rustls connector to the same promise. Read
// `ServerVerification::select`'s arms against [`SslRootCert`]'s table.

/// Writes TLS secrets to a named file in NSS key-log format, so a capture of
/// the connection can be decrypted.
///
/// rustls ships [`rustls::KeyLogFile`], but it takes its path from the
/// `SSLKEYLOGFILE` environment variable and reads it once at construction.
/// libpq's `sslkeylogfile` names the path as a CONNECTION PARAMETER, so
/// different connections in one process can log to different files - or, much
/// more importantly, so that only the connection that asked for it logs at
/// all. That is not something an environment variable can express, hence this.
///
/// Behaviour matches libpq 18, measured rather than assumed: the file is
/// created 0600, secrets are APPENDED (a second connection to the same path
/// adds to it rather than replacing it), and a path that cannot be opened is a
/// WARNING that still lets the connection proceed. That last one is a
/// deliberate choice of libpq's and worth keeping: failing the connection
/// because a debugging aid could not be written would turn a diagnostic switch
/// into an outage.
#[derive(Debug)]
struct KeyLogToFile {
    /// `None` once opening has failed, so a broken path warns once rather than
    /// on every secret of every handshake.
    file: std::sync::Mutex<Option<std::fs::File>>,
}

impl KeyLogToFile {
    fn new(path: &str) -> Self {
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Anyone who can read this file can decrypt the session, so it is
            // created as tightly as libpq creates it. This sets the mode only
            // when the file is CREATED; an existing file keeps its own, which
            // is the same corner libpq has.
            options.mode(0o600);
        }

        let file = match options.open(path) {
            Ok(file) => Some(file),
            Err(error) => {
                log::warn!("could not open SSL key logging file {path:?}: {error}");
                None
            }
        };

        Self {
            file: std::sync::Mutex::new(file),
        }
    }
}

impl rustls::KeyLog for KeyLogToFile {
    fn log(&self, label: &str, client_random: &[u8], secret: &[u8]) {
        // Both traits are in play: `fmt::Write` builds the hex line in memory,
        // `io::Write` puts it on disk.
        use std::fmt::Write as _;
        use std::io::Write as _;

        let mut guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(file) = guard.as_mut() else {
            return;
        };

        let mut line =
            String::with_capacity(label.len() + 2 * client_random.len() + 2 * secret.len() + 3);
        line.push_str(label);
        line.push(' ');
        for byte in client_random {
            let _ = write!(line, "{byte:02x}");
        }
        line.push(' ');
        for byte in secret {
            let _ = write!(line, "{byte:02x}");
        }
        line.push('\n');

        let write_error = file.write_all(line.as_bytes()).err();
        if write_error.is_some() {
            // Stop trying: a file that has started failing will keep failing,
            // and a warning per secret per handshake is its own problem.
            *guard = None;
        }
        drop(guard);
        if let Some(error) = write_error {
            log::warn!("could not write to SSL key logging file: {error}");
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
        .map(|(verifier, _)| verifier)
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
) -> Result<(Arc<dyn ServerCertVerifier>, ServerVerification), Error> {
    let algorithms = provider.signature_verification_algorithms;
    // "Configured" is asked of the store, not of `SslRootCert`, and the two
    // cannot disagree: `from_config` loads nothing for `Unset`, and errors
    // rather than returning an empty store for `System` or `File`. Asking the
    // store is the safer of the two identical questions, because a store with
    // no anchors could not verify anything even if a path had been named.
    //
    // The policy is returned as well as applied, because the connector has to
    // repeat it to `connect_raw`: that is what turns "this verifier does X"
    // into an attestation `connect_raw` can hold every connector to.
    let policy = ServerVerification::select(mode, !roots.is_empty())?;
    if policy == ServerVerification::None {
        return Ok((Arc::new(AcceptAnyServerCert { algorithms }), policy));
    }

    let builder =
        || WebPkiServerVerifier::builder_with_provider(roots.clone(), Arc::new(provider.clone()));

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
    let file_issuers = distinct_crl_issuers(&file_crls)
        .map_err(|reason| Error::tls(format!("sslcrl: {reason}").into()))?;
    ensure_crls_started(&file_crls)
        .map_err(|reason| Error::tls(format!("sslcrl: {reason}").into()))?;

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
        builder()
            .build()
            .map_err(|error| Error::tls(Box::new(error)))?
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

    let verifier: Arc<dyn ServerCertVerifier> = match policy {
        ServerVerification::Chain => Arc::new(ChainOnlyServerCert { verifier }),
        ServerVerification::ChainAndHostname => verifier,
        ServerVerification::None => {
            unreachable!("ServerVerification::None returns before WebPki verifier construction")
        }
    };
    Ok((verifier, policy))
}

/// [`ServerVerification::Chain`]: the chain is checked against the configured trust
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

/// [`ServerVerification::None`]: no chain, no host name, no expiry.
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

fn read_private_key_file(key_path: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
    read_private_key_file_with_metadata(key_path, std::fs::File::metadata)
}

fn read_private_key_file_with_metadata(
    key_path: &str,
    inspect: impl FnOnce(&std::fs::File) -> io::Result<std::fs::Metadata>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(key_path).map_err(|error| {
        Error::tls(format!("sslkey={key_path}: cannot read PEM: {error}").into())
    })?;
    let metadata = inspect(&file).map_err(|error| {
        Error::tls(format!("sslkey={key_path}: cannot inspect private key file: {error}").into())
    })?;
    if !metadata.file_type().is_file() {
        return Err(Error::tls(
            format!("sslkey={key_path}: private key is not a regular file").into(),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        // libpq permits root-owned system keys to be group-readable, but a
        // key with any other owner must have no group or world access. The
        // file handle and its metadata stay together so a path replacement
        // cannot make us parse a different, unchecked key.
        let forbidden = if metadata.uid() == 0 { 0o037 } else { 0o077 };
        if metadata.mode() & forbidden != 0 {
            return Err(Error::tls(
                format!(
                    "sslkey={key_path}: private key has group or world access; use permissions \
                     0600 or less, or 0640 or less for a root-owned key"
                )
                .into(),
            ));
        }
    }

    let mut pem = Zeroizing::new(Vec::new());
    file.read_to_end(&mut pem).map_err(|error| {
        Error::tls(format!("sslkey={key_path}: cannot read PEM: {error}").into())
    })?;
    Ok(pem)
}

/// Load a private key without letting the presence of `sslpassword` change the
/// meaning of an ordinary plaintext key.
fn private_key_from_config(
    key_path: &str,
    password: Option<&[u8]>,
) -> Result<PrivateKeyDer<'static>, Error> {
    let pem = read_private_key_file(key_path)?;
    let unencrypted_error = match PrivateKeyDer::from_pem_slice(&pem) {
        Ok(key) => return Ok(key),
        Err(error) => error,
    };

    // rustls-pki-types intentionally ignores ENCRYPTED PRIVATE KEY sections.
    // Parse only that one additional label here; malformed plaintext keys must
    // retain the existing rustls PEM error instead of being misreported as a
    // bad passphrase.
    let pem_text = match std::str::from_utf8(&pem) {
        Ok(pem) => pem,
        Err(_) => {
            return Err(Error::tls(
                format!("sslkey={key_path}: cannot read PEM: {unencrypted_error}").into(),
            ));
        }
    };
    let (label, encrypted_document) = match SecretDocument::from_pem(pem_text) {
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

    Ok(
        match (
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
        },
    )
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
        // Below 128, so the value is in `0..=127` and this cast is exact.
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
fn openssl_crl_issuer(crl: &CertificateRevocationListDer<'_>) -> Result<(Vec<u8>, i64), String> {
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
    let entries =
        std::fs::read_dir(path).map_err(|error| format!("cannot read directory: {error}"))?;
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
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
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
///
/// Clone this maker before passing it to a connection if that session may need
/// cancellation. The clone retains the opaque TLS policy identity that a
/// cancel connection must present. Constructing a second maker, even from the
/// same visible settings, intentionally creates a different policy lineage and
/// is refused before the cancel connection sends bytes.
#[derive(Clone, Debug)]
pub struct MakeRustlsConnect {
    config: Arc<ClientConfig>,
    policy_identity: TlsPolicyIdentity,
    ssl_cert_mode: SslCertMode,
    /// What the verifier inside `config` really checks. Reported to
    /// `connect_raw`, which refuses the connection when the connection string
    /// asked for more. [`MakeRustlsConnect::new`] cannot inspect a
    /// caller-supplied [`ClientConfig`], so it claims
    /// [`ServerVerification::None`] - the only honest answer.
    server_verification: ServerVerification,
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
    ///
    /// `verification` is what YOU promise the supplied [`ClientConfig`]
    /// actually checks, and the driver takes it at its word.
    ///
    /// It has to be told, because rustls does not expose a `ClientConfig`'s
    /// verifier and the policy therefore cannot be read back out. Before this
    /// argument existed the constructor assumed
    /// [`ServerVerification::None`] - the only honest guess, but a permanent
    /// one: a connector built from a genuinely verifying config could never
    /// satisfy `sslmode=verify-ca` / `verify-full`, or any `sslmode` naming
    /// `sslrootcert`, and the only way out was to use a different constructor.
    /// The in-crate tests had to assign the private field directly, which is
    /// the tell that the capability was missing rather than withheld.
    ///
    /// ATTESTING MORE THAN THE CONFIG PERFORMS IS THE ONE THING THAT MATTERS
    /// HERE. Claim [`ServerVerification::ChainAndHostname`] for a config whose
    /// verifier accepts anything and `verify-full` will report success over a
    /// session nothing authenticated - a failure that looks exactly like a
    /// verified connection. This is the same trust any
    /// [`TlsConnect`](crate::tls::TlsConnect) implementation is given when it
    /// overrides `can_honor_server_verification`; the argument only makes the
    /// promise explicit at the call site instead of silently denying it.
    ///
    /// [`MakeRustlsConnect::from_config`] needs no such argument: it builds the
    /// verifier itself from `sslmode` and `sslrootcert`, so it knows.
    pub fn new(config: Arc<ClientConfig>, verification: ServerVerification) -> MakeRustlsConnect {
        let config = if config.alpn_protocols.is_empty() {
            let mut config = (*config).clone();
            config.alpn_protocols = vec![POSTGRESQL_ALPN_PROTOCOL.to_vec()];
            Arc::new(config)
        } else {
            config
        };
        MakeRustlsConnect {
            config,
            policy_identity: TlsPolicyIdentity::new(),
            ssl_cert_mode: SslCertMode::Allow,
            server_verification: verification,
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
        let (verifier, server_verification) =
            verifier_for(config.get_ssl_mode(), roots, crls, &provider)?;

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

        // Installed only when the connection asked for it, so a process that
        // never sets `sslkeylogfile` cannot be made to leak secrets by an
        // environment variable it did not choose.
        if let Some(path) = config.get_ssl_key_log_file() {
            client_config.key_log = Arc::new(KeyLogToFile::new(path));
        }

        if crl_directory_reload.is_some() || config.get_ssl_cert_mode() == SslCertMode::Require {
            // Resumption can skip both server-certificate verification and a
            // fresh CertificateRequest. rustls also keys tickets to verifier
            // and client-resolver identity today, but disabling them makes
            // both policies explicit invariants rather than incidental
            // properties of its session-cache implementation.
            client_config.resumption = rustls::client::Resumption::disabled();
        }
        // Leaving resumption ENABLED for every other configuration is safe for
        // a reason that lives on the SERVER, not here, so it is worth writing
        // down: PostgreSQL hands out no resumable session at all. Measured
        // 2026-08-23 against the `tls_live_setup.sh` servers with
        // `openssl s_client -starttls postgres -sess_out`, on both a TLS 1.2
        // and a TLS 1.3 server -- the Session-ID comes back empty, no session
        // ticket arrives, and `-sess_out` writes NO file, so there is nothing a
        // client could present to resume. A resumed handshake, which is the
        // thing that would skip `verify_server_cert`, therefore cannot occur.
        //
        // This is a property of the peer, so it can change. If PostgreSQL ever
        // enables session tickets, the two arms above stop being the only ones
        // that need `Resumption::disabled()` and this whole decision has to be
        // re-taken. Re-run the probe above rather than assuming either way.

        let mut connector = MakeRustlsConnect::new(Arc::new(client_config), server_verification);
        connector.ssl_cert_mode = config.get_ssl_cert_mode();
        connector.crl_directory_reload = crl_directory_reload;
        Ok(connector)
    }
}

impl<S> MakeTlsConnect<S> for MakeRustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + crate::buf_stream::SplitStream + 'static,
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
            policy_identity: self.policy_identity.clone(),
            domain: domain.to_string(),
            ssl_cert_mode: self.ssl_cert_mode,
            server_verification: self.server_verification,
        })
    }
}

/// A single rustls handshake, produced by [`MakeRustlsConnect`].
pub struct RustlsConnect {
    config: Arc<ClientConfig>,
    policy_identity: TlsPolicyIdentity,
    domain: String,
    ssl_cert_mode: SslCertMode,
    server_verification: ServerVerification,
}

impl std::fmt::Debug for RustlsConnect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RustlsConnect")
            .field("config", &"<redacted>")
            .field("policy_identity", &self.policy_identity)
            .field("domain", &self.domain)
            .field("ssl_cert_mode", &self.ssl_cert_mode)
            .field("server_verification", &self.server_verification)
            .finish()
    }
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + crate::buf_stream::SplitStream + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<RustlsStream<S>, io::Error>>>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let server_name = ServerName::try_from(self.domain.clone())
                .map_err(|e| io::Error::other(format!("invalid TLS hostname: {e}")))?;
            let (config, client_cert_observation) = if self.ssl_cert_mode == SslCertMode::Require {
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
            // The handshake is driven against the socket directly rather than
            // through a poll-based adapter, so the socket is still ours
            // afterwards. That is the whole point: an adapter that keeps the
            // socket cannot hand back halves, and a stream that cannot be
            // split gets the serialized run loop. See `tls_sansio`.
            let connection = rustls::ClientConnection::new(config, server_name)
                .map_err(|error| io::Error::other(format!("TLS session setup failed: {error}")))?;
            let (stream, connection) = tls_sansio::handshake(stream, connection).await?;

            let tls_server_end_point = connection
                .peer_certificates()
                .and_then(<[CertificateDer<'_>]>::first)
                .and_then(tls_server_end_point);
            let negotiated_alpn_protocol = connection.alpn_protocol().map(<[u8]>::to_vec);
            let session: SharedSession = share(connection);
            let client_cert_status = client_cert_observation
                .as_deref()
                .map_or(ClientCertStatus::Unknown, ClientCertObservation::status);

            Ok(RustlsStream {
                inner: TlsStreamCore::new(stream, session),
                tls_server_end_point,
                client_cert_status,
                negotiated_alpn_protocol,
            })
        })
    }

    fn can_honor_sslsni(&self, enabled: bool) -> bool {
        self.config.enable_sni == enabled
    }

    fn can_honor_sslcertmode(&self, mode: SslCertMode) -> bool {
        self.ssl_cert_mode == mode
    }

    fn can_honor_server_verification(&self, verification: ServerVerification) -> bool {
        self.server_verification == verification
    }

    fn cancel_policy_identity(&self) -> Option<&TlsPolicyIdentity> {
        Some(&self.policy_identity)
    }
}

/// A TLS-wrapped connection produced by [`RustlsConnect`].
pub struct RustlsStream<S> {
    inner: TlsStreamCore<S>,
    tls_server_end_point: Option<Vec<u8>>,
    client_cert_status: ClientCertStatus,
    negotiated_alpn_protocol: Option<Vec<u8>>,
}

impl<S> std::fmt::Debug for RustlsStream<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tls_server_end_point = self.tls_server_end_point.as_ref().map(|_| "<redacted>");

        formatter
            .debug_struct("RustlsStream")
            .field("tls_server_end_point", &tls_server_end_point)
            .field("client_cert_status", &self.client_cert_status)
            .field("negotiated_alpn_protocol", &self.negotiated_alpn_protocol)
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + 'static> AsyncRead for RustlsStream<S> {
    async fn read<B: compio::buf::IoBufMut>(&mut self, buf: B) -> compio::BufResult<usize, B> {
        self.inner.read(buf).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + 'static> AsyncWrite for RustlsStream<S> {
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

/// A TLS stream splits when its SOCKET does.
///
/// The rustls session cannot be duplicated, so it is not: both halves share
/// the one session and reach it only through synchronous helpers that never
/// hold a mutex guard across an `await`. What gets split is the socket, which
/// is the same operation the plaintext path already performs. `tls_sansio`
/// has the full argument, including the one asymmetry - ciphertext produced by
/// the read path leaves with the next write.
impl<S> crate::buf_stream::SplitStream for RustlsStream<S>
where
    S: crate::buf_stream::SplitStream,
{
    type ReadHalf = TlsReadHalf<S::ReadHalf>;
    type WriteHalf = TlsWriteHalf<S::WriteHalf>;

    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        let RustlsStream {
            inner,
            tls_server_end_point,
            client_cert_status,
            negotiated_alpn_protocol,
        } = self;
        match inner.try_into_split() {
            Ok((read, write)) => Ok((read, write)),
            // The socket refused, so rebuild the stream exactly as it was.
            Err(inner) => Err(RustlsStream {
                inner,
                tls_server_end_point,
                client_cert_status,
                negotiated_alpn_protocol,
            }),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + 'static> TlsStream for RustlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        match &self.tls_server_end_point {
            Some(hash) => ChannelBinding::tls_server_end_point(hash.clone()),
            None => ChannelBinding::none(),
        }
    }

    fn negotiated_alpn_protocol(&self) -> Option<&[u8]> {
        self.negotiated_alpn_protocol.as_deref()
    }

    fn client_cert_status(&self) -> ClientCertStatus {
        self.client_cert_status
    }

    fn configure_release(
        &self,
        _: crate::tls::private::ForcePrivateApi,
        mut release: crate::tls::private::ReleaseConfig<'_>,
    ) {
        release.set_tls_session(self.inner.session());
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
/// Returns `None` when the signature algorithm names no hash we can reproduce.
/// `None` propagates to `ChannelBinding::none()`, so
/// `channel_binding=require` fails loudly instead of authenticating against a
/// hash the server did not compute. Ed25519 is the common case: its signature
/// algorithm does not identify a standalone digest.
fn tls_server_end_point(cert: &CertificateDer<'_>) -> Option<Vec<u8>> {
    use sha2::Digest as _;

    #[derive(Clone, Copy)]
    enum Digest {
        Sha224,
        Sha256,
        Sha384,
        Sha512,
        Sha512_224,
        Sha512_256,
    }

    impl Digest {
        fn for_hash_oid(oid: &str) -> Option<Digest> {
            match oid {
                // SHA-1 is upgraded by RFC 5929 section 4.1.
                "1.3.14.3.2.26" => Some(Digest::Sha256),
                "2.16.840.1.101.3.4.2.4" => Some(Digest::Sha224),
                "2.16.840.1.101.3.4.2.1" => Some(Digest::Sha256),
                "2.16.840.1.101.3.4.2.2" => Some(Digest::Sha384),
                "2.16.840.1.101.3.4.2.3" => Some(Digest::Sha512),
                "2.16.840.1.101.3.4.2.5" => Some(Digest::Sha512_224),
                "2.16.840.1.101.3.4.2.6" => Some(Digest::Sha512_256),
                _ => None,
            }
        }

        fn hash(self, der: &[u8]) -> Vec<u8> {
            match self {
                Digest::Sha224 => sha2::Sha224::digest(der).to_vec(),
                Digest::Sha256 => sha2::Sha256::digest(der).to_vec(),
                Digest::Sha384 => sha2::Sha384::digest(der).to_vec(),
                Digest::Sha512 => sha2::Sha512::digest(der).to_vec(),
                Digest::Sha512_224 => sha2::Sha512_224::digest(der).to_vec(),
                Digest::Sha512_256 => sha2::Sha512_256::digest(der).to_vec(),
            }
        }
    }

    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    let signature_oid = parsed.signature_algorithm.algorithm.to_id_string();
    let digest = if signature_oid == "1.2.840.113549.1.1.10" {
        // RSASSA-PSS carries its digest in AlgorithmIdentifier parameters. This
        // is the equivalent of libpq's X509_get_signature_info path; treating
        // the outer OID as the whole algorithm loses the binding.
        match x509_parser::signature_algorithm::SignatureAlgorithm::try_from(
            &parsed.signature_algorithm,
        )
        .ok()?
        {
            x509_parser::signature_algorithm::SignatureAlgorithm::RSASSA_PSS(parameters) => {
                Digest::for_hash_oid(&parameters.hash_algorithm_oid().to_id_string())?
            }
            _ => return None,
        }
    } else {
        // Dotted OIDs from RFC 8017 (RSA), RFC 5758 (ECDSA/DSA), and RFC 5754
        // (SHA-2). MD5 and SHA-1 signatures map to SHA-256 per RFC 5929.
        match signature_oid.as_str() {
            // md5WithRSAEncryption, sha1WithRSAEncryption, id-dsa-with-sha1,
            // ecdsa-with-SHA1, sha256WithRSAEncryption, ecdsa-with-SHA256,
            // dsa-with-sha256.
            "1.2.840.113549.1.1.4"
            | "1.2.840.113549.1.1.5"
            | "1.2.840.10040.4.3"
            | "1.2.840.10045.4.1"
            | "1.2.840.113549.1.1.11"
            | "1.2.840.10045.4.3.2"
            | "2.16.840.1.101.3.4.3.2" => Digest::Sha256,
            // sha224WithRSAEncryption, ecdsa-with-SHA224, dsa-with-sha224.
            "1.2.840.113549.1.1.14" | "1.2.840.10045.4.3.1" | "2.16.840.1.101.3.4.3.1" => {
                Digest::Sha224
            }
            // sha384WithRSAEncryption, ecdsa-with-SHA384, dsa-with-sha384.
            "1.2.840.113549.1.1.12" | "1.2.840.10045.4.3.3" | "2.16.840.1.101.3.4.3.3" => {
                Digest::Sha384
            }
            // sha512WithRSAEncryption, ecdsa-with-SHA512, dsa-with-sha512.
            "1.2.840.113549.1.1.13" | "1.2.840.10045.4.3.4" | "2.16.840.1.101.3.4.3.4" => {
                Digest::Sha512
            }
            "1.2.840.113549.1.1.15" => Digest::Sha512_224,
            "1.2.840.113549.1.1.16" => Digest::Sha512_256,
            _ => return None,
        }
    };

    Some(digest.hash(cert.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf_stream::SplitStream;
    use crate::maybe_tls_stream::MaybeTlsStream;
    use crate::tls_sansio::handshaken_pair;
    use compio::buf::{BufResult, IoBuf, IoBufMut};
    use compio::net::{TcpListener, TcpStream};
    use futures_channel::oneshot;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use sha2::Digest;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::Write as _;
    use std::sync::Mutex;

    /// The CA that signed [`SERVER_LOCALHOST`], and nothing else.
    const CA: &str = include_str!("../tests/data/verifier_ca.pem");
    /// A server certificate whose only subject-alternative name is `localhost`.
    const SERVER_LOCALHOST: &str = include_str!("../tests/data/verifier_server_localhost.pem");

    struct ScriptedTlsSocket {
        reads: VecDeque<Vec<u8>>,
    }

    struct DiscardTlsWriteHalf;

    async fn read_scripted<B: IoBufMut>(
        reads: &mut VecDeque<Vec<u8>>,
        buf: B,
    ) -> BufResult<usize, B> {
        let Some(chunk) = reads.pop_front() else {
            let mut empty: &[u8] = &[];
            return AsyncRead::read(&mut empty, buf).await;
        };
        let mut source: &[u8] = &chunk;
        let result = AsyncRead::read(&mut source, buf).await;
        let consumed = chunk.len() - source.len();
        if consumed < chunk.len() {
            reads.push_front(chunk[consumed..].to_vec());
        }
        result
    }

    impl AsyncRead for ScriptedTlsSocket {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            read_scripted(&mut self.reads, buf).await
        }
    }

    impl AsyncWrite for ScriptedTlsSocket {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = buf.buf_len();
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWrite for DiscardTlsWriteHalf {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = buf.buf_len();
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SplitStream for ScriptedTlsSocket {
        type ReadHalf = Self;
        type WriteHalf = DiscardTlsWriteHalf;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Ok((self, DiscardTlsWriteHalf))
        }
    }

    fn scripted_tls_stream(
        client: rustls::ClientConnection,
        reads: VecDeque<Vec<u8>>,
    ) -> MaybeTlsStream<ScriptedTlsSocket, RustlsStream<ScriptedTlsSocket>> {
        MaybeTlsStream::Tls(RustlsStream {
            inner: TlsStreamCore::new(ScriptedTlsSocket { reads }, share(client)),
            tls_server_end_point: None,
            client_cert_status: ClientCertStatus::NotApplicable,
            negotiated_alpn_protocol: None,
        })
    }

    #[compio::test]
    async fn tls_split_reassembles_a_record_across_socket_reads() {
        const PAYLOAD: &[u8] = b"one record in two reads";
        let (client, mut server) = handshaken_pair();

        server
            .writer()
            .write_all(PAYLOAD)
            .expect("queue the TLS record");
        let mut first = Vec::new();
        while server.wants_write() {
            server
                .write_tls(&mut first)
                .expect("serialize the TLS record");
        }
        assert!(
            first.len() > 3,
            "the TLS record must extend beyond its scripted first read"
        );
        let second = first.split_off(3);

        let stream = scripted_tls_stream(client, VecDeque::from([first, second]));
        let Ok((mut read, _write)) = stream.try_into_split() else {
            panic!("the scripted TLS stream refused to split");
        };
        let BufResult(result, buf) = read.read(vec![0u8; PAYLOAD.len()]).await;
        let n = result.expect("decrypt the TLS record split across two socket reads");
        assert_eq!(n, PAYLOAD.len());
        assert_eq!(&buf[..n], PAYLOAD);
    }

    #[compio::test]
    async fn tls_split_reports_close_notify_and_socket_eof_as_eof() {
        let (close_client, mut close_server) = handshaken_pair();
        close_server.send_close_notify();
        let mut close_wire = Vec::new();
        while close_server.wants_write() {
            close_server
                .write_tls(&mut close_wire)
                .expect("serialize close_notify");
        }
        assert!(
            !close_wire.is_empty(),
            "the close_notify fixture produced no ciphertext"
        );

        let close_stream = scripted_tls_stream(close_client, VecDeque::from([close_wire]));
        let Ok((mut close_read, _write)) = close_stream.try_into_split() else {
            panic!("the close_notify TLS stream refused to split");
        };
        let BufResult(close_result, _) = close_read.read(vec![0u8; 1]).await;
        assert_eq!(
            close_result.expect("read the peer's close_notify"),
            0,
            "close_notify must end the split TLS reader"
        );

        let (eof_client, _eof_server) = handshaken_pair();
        let eof_stream = scripted_tls_stream(eof_client, VecDeque::new());
        let Ok((mut eof_read, _write)) = eof_stream.try_into_split() else {
            panic!("the socket-EOF TLS stream refused to split");
        };
        let BufResult(eof_result, _) = eof_read.read(vec![0u8; 1]).await;
        assert_eq!(
            eof_result.expect("read the closed socket"),
            0,
            "socket EOF must end the split TLS reader"
        );
    }

    #[compio::test]
    async fn tls_split_preserves_ciphertext_already_read_from_socket() {
        const BEFORE: &[u8] = b"before split";
        let trailing = vec![b'x'; 4096];
        let (client, mut server) = handshaken_pair();

        server
            .writer()
            .write_all(BEFORE)
            .expect("queue the first TLS record");
        let mut wire = Vec::new();
        while server.wants_write() {
            server
                .write_tls(&mut wire)
                .expect("serialize the first TLS record");
        }
        server
            .writer()
            .write_all(&trailing)
            .expect("queue the trailing TLS record");
        while server.wants_write() {
            server
                .write_tls(&mut wire)
                .expect("serialize the trailing TLS record");
        }
        assert!(
            wire.len() > 4096 && wire.len() < 16 * 1024,
            "the fixture must cross rustls' 4096-byte input window in one socket read"
        );

        let socket = ScriptedTlsSocket {
            reads: VecDeque::from([wire]),
        };
        let rustls = RustlsStream {
            inner: TlsStreamCore::new(socket, share(client)),
            tls_server_end_point: None,
            client_cert_status: ClientCertStatus::NotApplicable,
            negotiated_alpn_protocol: None,
        };
        let mut stream: MaybeTlsStream<ScriptedTlsSocket, _> = MaybeTlsStream::Tls(rustls);

        let BufResult(first, first_buf) = stream.read(vec![0u8; BEFORE.len()]).await;
        let first = first.expect("decrypt the first TLS record");
        assert_eq!(&first_buf[..first], BEFORE, "the split fixture is invalid");

        let (mut read, _write) = match stream.try_into_split() {
            Ok(halves) => halves,
            Err(_) => panic!("the scripted TLS stream refused to split"),
        };
        let BufResult(second, second_buf) = read.read(vec![0u8; trailing.len()]).await;
        let second = second.expect("decrypt the trailing TLS record after the split");
        assert_eq!(
            second,
            trailing.len(),
            "the TLS split discarded ciphertext already read from the socket"
        );
        assert_eq!(&second_buf[..second], trailing);
    }

    thread_local! {
        static KEY_LOG_PROBE: RefCell<Option<Arc<KeyLogToFile>>> = const { RefCell::new(None) };
        static KEY_LOG_MUTEX_LOCKED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    struct KeyLogWarningProbe;

    impl log::Log for KeyLogWarningProbe {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, _record: &log::Record<'_>) {
            KEY_LOG_PROBE.with(|slot| {
                if let Some(key_log) = slot.borrow().as_ref() {
                    KEY_LOG_MUTEX_LOCKED
                        .with(|locked| locked.set(Some(key_log.file.try_lock().is_err())));
                }
            });
        }

        fn flush(&self) {}
    }

    static KEY_LOG_WARNING_PROBE: KeyLogWarningProbe = KeyLogWarningProbe;
    static INSTALL_KEY_LOG_WARNING_PROBE: std::sync::Once = std::sync::Once::new();
    static KEY_LOG_WARNING_PROBE_INSTALLED: AtomicBool = AtomicBool::new(false);

    fn install_key_log_warning_probe() {
        INSTALL_KEY_LOG_WARNING_PROBE.call_once(|| {
            KEY_LOG_WARNING_PROBE_INSTALLED.store(
                log::set_logger(&KEY_LOG_WARNING_PROBE).is_ok(),
                Ordering::Relaxed,
            );
            log::set_max_level(log::LevelFilter::Trace);
        });
        assert!(
            KEY_LOG_WARNING_PROBE_INSTALLED.load(Ordering::Relaxed),
            "the unit-test process already installed a different logger"
        );
    }

    #[test]
    fn a_key_log_write_warning_runs_after_the_file_mutex_is_released() {
        install_key_log_warning_probe();
        let read_only = std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("open a read-only file for the failing-write fixture");
        let key_log = Arc::new(KeyLogToFile {
            file: Mutex::new(Some(read_only)),
        });
        KEY_LOG_PROBE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&key_log)));
        KEY_LOG_MUTEX_LOCKED.with(|locked| locked.set(None));

        rustls::KeyLog::log(&*key_log, "CLIENT_TRAFFIC_SECRET_0", &[0; 32], &[1; 32]);

        let observed = KEY_LOG_MUTEX_LOCKED.with(Cell::get);
        KEY_LOG_PROBE.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "the caller logger ran while the SSL key-log file mutex was locked"
        );
        assert!(
            key_log.file.lock().expect("lock key-log file").is_none(),
            "the fixture write did not fail, so the warning path never ran"
        );
    }

    fn tls_error_source(error: &Error) -> String {
        std::error::Error::source(error)
            .expect("a TLS error must retain its specific source")
            .to_string()
    }

    #[test]
    fn sslkey_open_error_names_the_missing_temporary_path() {
        let directory = tempfile::tempdir().expect("create an isolated sslkey directory");
        let path = directory.path().join("missing.key");
        assert!(
            !path.exists(),
            "the missing-file fixture unexpectedly exists"
        );
        let open_error = std::fs::File::open(&path)
            .expect_err("the fixture path must independently fail to open");
        let key_path = path.to_str().expect("temporary paths are valid UTF-8");

        let error = read_private_key_file(key_path)
            .expect_err("a missing sslkey cannot yield private key bytes");

        assert_eq!(
            tls_error_source(&error),
            format!("sslkey={key_path}: cannot read PEM: {open_error}"),
            "the open failure must be attributed to the exact sslkey path"
        );
    }

    #[test]
    fn sslkey_metadata_error_names_the_opened_temporary_path() {
        let key = tempfile::NamedTempFile::new().expect("create a temporary regular sslkey");
        let key_path = key
            .path()
            .to_str()
            .expect("temporary paths are valid UTF-8");

        let error = read_private_key_file_with_metadata(key_path, |file| {
            assert!(
                file.metadata()
                    .expect("inspect the real fixture descriptor")
                    .is_file(),
                "the metadata-error fixture must first open a regular file"
            );
            Err(io::Error::other("forced metadata failure"))
        })
        .expect_err("an sslkey whose metadata cannot be inspected must be refused");

        assert_eq!(
            tls_error_source(&error),
            format!("sslkey={key_path}: cannot inspect private key file: forced metadata failure"),
            "the metadata failure must be attributed to the exact opened sslkey path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sslkey_directory_is_named_as_not_a_regular_file() {
        let directory = tempfile::tempdir().expect("create a temporary sslkey directory");
        assert!(
            directory
                .path()
                .metadata()
                .expect("inspect the directory fixture")
                .is_dir(),
            "the non-regular sslkey fixture must be a directory"
        );
        std::fs::File::open(directory.path())
            .expect("Unix must open the directory so the regular-file check decides");
        let key_path = directory
            .path()
            .to_str()
            .expect("temporary paths are valid UTF-8");

        let error = read_private_key_file(key_path)
            .expect_err("a directory cannot supply private key bytes");

        assert_eq!(
            tls_error_source(&error),
            format!("sslkey={key_path}: private key is not a regular file"),
            "the regular-file refusal must name the exact sslkey directory"
        );
    }

    #[cfg(unix)]
    fn generated_sslkey(mode: u32) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate a private key fixture");
        let mut key = tempfile::NamedTempFile::new().expect("create a private key fixture");
        key.write_all(issued.signing_key.serialize_pem().as_bytes())
            .expect("write the private key fixture");
        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(mode))
            .expect("set private key fixture permissions");
        key
    }

    #[cfg(unix)]
    #[test]
    fn sslkey_permissions_allow_a_private_file() {
        let key = generated_sslkey(0o600);
        private_key_from_config(key.path().to_str().unwrap(), None)
            .expect("a 0600 private key must load");
    }

    #[cfg(unix)]
    #[test]
    fn sslkey_permissions_reject_group_or_world_access() {
        let key = generated_sslkey(0o644);
        let error = private_key_from_config(key.path().to_str().unwrap(), None)
            .expect_err("a 0644 private key exposes its client identity");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .fold(format!("{error}"), |chain, error| {
            format!("{chain}: {error}")
        });
        assert!(
            chain.contains("sslkey="),
            "the error must name sslkey: {chain}"
        );
        assert!(
            chain.contains("group or world access"),
            "the error must name the unsafe permissions: {chain}"
        );
    }

    /// Write an embedded PEM to a 0600 temp file. `read_private_key_file`
    /// refuses group- or world-readable keys, so a fixture committed at 0644
    /// would fail on permissions before reaching the branch under test.
    #[cfg(unix)]
    fn sslkey_fixture(pem: &str) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let mut key = tempfile::NamedTempFile::new().expect("create a private key fixture");
        key.write_all(pem.as_bytes())
            .expect("write the private key fixture");
        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(0o600))
            .expect("set private key fixture permissions");
        key
    }

    #[cfg(unix)]
    fn sslkey_error_chain(error: &Error) -> String {
        std::iter::successors(std::error::Error::source(error), |error| {
            std::error::Error::source(*error)
        })
        .fold(format!("{error}"), |chain, error| {
            format!("{chain}: {error}")
        })
    }

    /// An encrypted key with no `sslpassword` must say so. This message is the
    /// only guidance a creator gets after forgetting the setting, and nothing
    /// executed it: the whole encrypted-key error surface was unreached, while
    /// only the successful decrypt is covered by the live TLS suite.
    #[cfg(unix)]
    #[test]
    fn encrypted_sslkey_without_a_password_names_sslpassword() {
        let key = sslkey_fixture(include_str!("../tests/data/encrypted_pkcs8_key.pem"));
        let error = private_key_from_config(key.path().to_str().unwrap(), None)
            .expect_err("an encrypted key cannot load without a passphrase");
        let chain = sslkey_error_chain(&error);
        assert!(
            chain.contains("encrypted private key requires a non-empty sslpassword"),
            "the error must name the missing sslpassword: {chain}"
        );
    }

    /// An empty `sslpassword` is the same case as none. libpq treats an empty
    /// passphrase as absent, and the `filter` that implements this would be a
    /// no-op if removed, which no other test would notice.
    #[cfg(unix)]
    #[test]
    fn encrypted_sslkey_with_an_empty_password_names_sslpassword() {
        let key = sslkey_fixture(include_str!("../tests/data/encrypted_pkcs8_key.pem"));
        let error = private_key_from_config(key.path().to_str().unwrap(), Some(b""))
            .expect_err("an empty passphrase cannot decrypt an encrypted key");
        let chain = sslkey_error_chain(&error);
        assert!(
            chain.contains("encrypted private key requires a non-empty sslpassword"),
            "an empty sslpassword must be treated as absent: {chain}"
        );
    }

    /// A PEM that is not a private key at all must keep the rustls PEM error.
    /// The comment above `private_key_from_config` states this outright -
    /// "malformed plaintext keys must retain the existing rustls PEM error
    /// instead of being misreported as a bad passphrase" - and nothing checked
    /// it. Misreporting would send someone hunting for a wrong sslpassword when
    /// the real fault is the file they pointed sslkey at.
    #[cfg(unix)]
    #[test]
    fn a_certificate_given_as_sslkey_is_not_reported_as_a_passphrase_problem() {
        let key = sslkey_fixture(include_str!("../tests/data/sha256_cert.pem"));
        let error = private_key_from_config(key.path().to_str().unwrap(), None)
            .expect_err("a certificate is not a private key");
        let chain = sslkey_error_chain(&error);
        assert!(
            chain.contains("cannot read PEM"),
            "the error must report a PEM problem: {chain}"
        );
        assert!(
            !chain.contains("sslpassword"),
            "a non-key PEM must not be blamed on the passphrase: {chain}"
        );
    }

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
        .expect("build accept-any test verifier")
        .0;
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("safe protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = alpn_protocols;
        config
    }

    #[test]
    fn rustls_connect_debug_redacts_config() {
        const CONFIG_SECRET: &str = "rustls-connect-config-material-4a873f2c";

        let secret = CONFIG_SECRET.as_bytes().to_vec();
        let exposed = format!("{secret:?}");
        let config = Arc::new(client_config(vec![secret]));
        assert!(
            format!("{config:?}").contains(&exposed),
            "test sentinel is not exposed by raw ClientConfig Debug"
        );
        let connect = RustlsConnect {
            config,
            policy_identity: TlsPolicyIdentity::new(),
            domain: "debug.example".to_owned(),
            ssl_cert_mode: SslCertMode::Allow,
            server_verification: ServerVerification::None,
        };

        let debug = format!("{connect:?}");

        assert!(
            debug.contains("RustlsConnect"),
            "rustls connector Debug did not name its type: {debug}"
        );
        assert!(
            debug.contains("config: \"<redacted>\""),
            "rustls connector Debug did not mark the config redaction: {debug}"
        );
        assert!(
            !debug.contains(&exposed) && !debug.contains(CONFIG_SECRET),
            "rustls connector Debug leaked its client config: {debug}"
        );
    }

    #[test]
    fn rustls_stream_debug_redacts_tls_server_end_point() {
        const BINDING_SECRET: &str = "rustls-stream-binding-material-b1357eca";

        let secret = BINDING_SECRET.as_bytes().to_vec();
        let exposed = format!("{secret:?}");
        let (client, _server) = handshaken_pair();
        let stream = RustlsStream {
            inner: TlsStreamCore::new((), share(client)),
            tls_server_end_point: Some(secret),
            client_cert_status: ClientCertStatus::NotApplicable,
            negotiated_alpn_protocol: None,
        };

        let debug = format!("{stream:?}");

        assert!(
            debug.contains("RustlsStream"),
            "rustls stream Debug did not name its type: {debug}"
        );
        assert!(
            debug.contains("tls_server_end_point: Some(\"<redacted>\")"),
            "rustls stream Debug did not mark the channel binding redaction: {debug}"
        );
        assert!(
            !debug.contains(&exposed) && !debug.contains(BINDING_SECRET),
            "rustls stream Debug leaked TLS channel binding material: {debug}"
        );
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
        let mut make = MakeRustlsConnect::new(Arc::new(client_config), ServerVerification::None);
        let connector = <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(
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
        assert_eq!(offered, Some(vec![POSTGRESQL_ALPN_PROTOCOL.to_vec()]));
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
        let (verifier, _) = verifier_for(mode, roots, ConfiguredCrls::default(), &provider())
            .map_err(|e| e.to_string())?;
        let cert = CertificateDer::from_pem_slice(SERVER_LOCALHOST.as_bytes()).unwrap();
        verifier
            .verify_server_cert(&cert, &[], &ServerName::try_from(name).unwrap(), &[], AT)
            .map(|_| ())
            .map_err(|e| e.to_string())
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
        verify_as(
            SslMode::Require,
            Arc::new(RootCertStore::empty()),
            "wrong.example",
        )
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
            verify_as(mode, Arc::new(RootCertStore::empty()), "localhost").unwrap_or_else(|e| {
                panic!("sslmode={} unverified must accept: {e}", mode.as_str())
            });
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

    fn endpoint_hash_fixture(pem: &str) -> (CertificateDer<'static>, Vec<u8>) {
        let cert = CertificateDer::from_pem_slice(pem.as_bytes()).expect("parse certificate PEM");
        let hash = tls_server_end_point(&cert).expect("certificate signature names a digest");
        (cert, hash)
    }

    /// The control for the two libpq digest cases below. It must remain green
    /// when their new selection arms are removed, proving the shared filter
    /// still runs a known working certificate.
    #[test]
    fn end_point_hash_libpq_digest_sha256_control() {
        let (cert, hash) = endpoint_hash_fixture(include_str!("../tests/data/sha256_cert.pem"));
        assert_eq!(hash, sha2::Sha256::digest(cert.as_ref()).to_vec());
    }

    #[test]
    fn end_point_hash_libpq_digest_rsa_pss_sha256() {
        let (cert, hash) =
            endpoint_hash_fixture(include_str!("../tests/data/rsa_pss_sha256_cert.pem"));
        assert_eq!(
            hash,
            sha2::Sha256::digest(cert.as_ref()).to_vec(),
            "RSASSA-PSS must take SHA-256 from its AlgorithmIdentifier parameters"
        );
    }

    #[test]
    fn end_point_hash_libpq_digest_rsa_sha224() {
        let (cert, hash) = endpoint_hash_fixture(include_str!("../tests/data/rsa_sha224_cert.pem"));
        assert_eq!(hash, sha2::Sha224::digest(cert.as_ref()).to_vec());
    }

    /// SHA-512 is the widest digest the signature table selects, and until this
    /// test nothing chose it: `Digest::Sha512`, and the `sha2::Sha512` arm of
    /// `Digest::hash`, had zero executed coverage regions. A server presenting a
    /// SHA-512-signed certificate would have computed its SCRAM-PLUS channel
    /// binding through code no test had run.
    #[test]
    fn end_point_hash_libpq_digest_rsa_sha512() {
        let (cert, hash) = endpoint_hash_fixture(include_str!("../tests/data/sha512_cert.pem"));
        assert_eq!(
            hash.len(),
            64,
            "sha512WithRSAEncryption must select SHA-512"
        );
        assert_eq!(hash, sha2::Sha512::digest(cert.as_ref()).to_vec());
    }

    /// RSASSA-PSS reads its digest from AlgorithmIdentifier parameters through
    /// `Digest::for_hash_oid`, a SECOND and separate table from the signature
    /// OIDs above. Only its SHA-256 arm had ever run, so every other hash a PSS
    /// certificate can name went through an unexecuted branch.
    ///
    /// `sha384_cert.pem` does NOT cover this: it is signed with
    /// sha384WithRSAEncryption, which the signature table resolves directly and
    /// which never consults `for_hash_oid` at all.
    #[test]
    fn end_point_hash_libpq_digest_rsa_pss_sha384() {
        let (cert, hash) =
            endpoint_hash_fixture(include_str!("../tests/data/rsa_pss_sha384_cert.pem"));
        assert_eq!(
            hash.len(),
            48,
            "PSS parameters naming SHA-384 must select it"
        );
        assert_eq!(
            hash,
            sha2::Sha384::digest(cert.as_ref()).to_vec(),
            "RSASSA-PSS must take SHA-384 from its AlgorithmIdentifier parameters"
        );
    }

    /// The RFC 5929 upgrade must also apply on the RSASSA-PSS path, which
    /// reads its digest from AlgorithmIdentifier parameters through a SECOND
    /// table, `Digest::for_hash_oid`. Its SHA-1 arm had zero executed regions,
    /// so a PSS certificate naming SHA-1 went through an unexecuted branch to
    /// reach a security-relevant decision.
    ///
    /// Both digests here are 32 bytes, so the length proves nothing; the
    /// assertion compares the VALUE against SHA-256 of the DER.
    #[test]
    fn end_point_hash_upgrades_a_pss_sha1_signature_to_sha256() {
        let (cert, hash) =
            endpoint_hash_fixture(include_str!("../tests/data/rsa_pss_sha1_cert.pem"));
        assert_eq!(
            hash,
            sha2::Sha256::digest(cert.as_ref()).to_vec(),
            "PSS parameters naming SHA-1 must bind with SHA-256"
        );
    }

    /// `sha512-256WithRSAEncryption` selects the truncated SHA-512/256, which
    /// is NOT SHA-256 despite the matching 32-byte width. Both the table entry
    /// and the `Digest::hash` arm were unexecuted, so nothing had ever proved
    /// this picks the truncated variant rather than the one it looks like.
    #[test]
    fn end_point_hash_libpq_digest_rsa_sha512_256() {
        let (cert, hash) = endpoint_hash_fixture(include_str!("../tests/data/sha512_256_cert.pem"));
        assert_eq!(hash, sha2::Sha512_256::digest(cert.as_ref()).to_vec());
        assert_ne!(
            hash,
            sha2::Sha256::digest(cert.as_ref()).to_vec(),
            "SHA-512/256 must not be confused with SHA-256; both are 32 bytes"
        );
    }

    /// RFC 5929 section 4.1 upgrades MD5 and SHA-1 signatures to SHA-256 for
    /// `tls-server-end-point`, so a SHA-1-signed certificate must bind with a
    /// 32-byte SHA-256 hash and never a 20-byte SHA-1 one. Binding the weaker
    /// digest would let a peer that can forge SHA-1 forge the channel binding
    /// SCRAM-PLUS rests on.
    ///
    /// **Coverage cannot see this gap.** `sha1WithRSAEncryption` shares its
    /// match arm with `sha256WithRSAEncryption`, which is already covered, so
    /// the arm reports as executed either way. Only removing the SHA-1 OID from
    /// that arm distinguishes the two, which is exactly the mutation that
    /// proves this test.
    #[test]
    fn end_point_hash_upgrades_a_sha1_signature_to_sha256() {
        let (cert, hash) = endpoint_hash_fixture(include_str!("../tests/data/sha1_cert.pem"));
        assert_eq!(
            hash.len(),
            32,
            "a SHA-1 signature must bind with SHA-256, not its own digest"
        );
        assert_eq!(hash, sha2::Sha256::digest(cert.as_ref()).to_vec());
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
    fn crl_issuer_bmp_string_decodes_big_endian_two_byte_units() {
        // BMPString stores each Unicode scalar as one two-byte, big-endian unit.
        let encoded = [0x00, 0x61, 0x26, 0x03];
        let value = Any::from_tag_and_data(Tag::BmpString, &encoded);

        let canonical = openssl_canonical_string(&value)
            .expect("a BMPString containing valid Unicode scalars must decode")
            .expect("BMPString is an OpenSSL-canonical string type");

        assert_eq!(
            canonical,
            "a☃".as_bytes(),
            "two-byte big-endian BMPString units must become the scalar's UTF-8 bytes"
        );
    }

    #[test]
    fn crl_issuer_bmp_string_rejects_an_odd_byte_count() {
        // BMPString requires exactly two bytes per character, so no trailing byte is valid.
        let encoded = [0x00, 0x61, 0x00];
        let value = Any::from_tag_and_data(Tag::BmpString, &encoded);

        let error = openssl_canonical_string(&value)
            .expect_err("an odd-length BMPString cannot contain complete two-byte units");

        assert_eq!(error, "odd-length BMPString in CRL issuer");
    }

    #[test]
    fn crl_issuer_bmp_string_rejects_a_non_scalar_unit() {
        // BMPString units are decoded independently; UTF-16 surrogate 0xd800 is not a scalar.
        let encoded = [0xd8, 0x00];
        let value = Any::from_tag_and_data(Tag::BmpString, &encoded);

        let error = openssl_canonical_string(&value)
            .expect_err("a BMPString surrogate unit is not a Unicode scalar value");

        assert_eq!(error, "invalid BMPString scalar in CRL issuer");
    }

    #[test]
    fn crl_issuer_universal_string_decodes_big_endian_four_byte_units() {
        // UniversalString stores each Unicode scalar as one four-byte, big-endian unit.
        let encoded = [0x00, 0x00, 0x00, 0x61, 0x00, 0x01, 0xf6, 0x42];
        let value = Any::from_tag_and_data(Tag::UniversalString, &encoded);

        let canonical = openssl_canonical_string(&value)
            .expect("a UniversalString containing valid Unicode scalars must decode")
            .expect("UniversalString is an OpenSSL-canonical string type");

        assert_eq!(
            canonical,
            "a🙂".as_bytes(),
            "four-byte big-endian UniversalString units must become the scalar's UTF-8 bytes"
        );
    }

    #[test]
    fn crl_issuer_universal_string_rejects_a_partial_unit() {
        // UniversalString requires exactly four bytes per character, so a fifth byte is invalid.
        let encoded = [0x00, 0x00, 0x00, 0x61, 0x00];
        let value = Any::from_tag_and_data(Tag::UniversalString, &encoded);

        let error = openssl_canonical_string(&value)
            .expect_err("a misaligned UniversalString ends with a partial four-byte unit");

        assert_eq!(error, "misaligned UniversalString in CRL issuer");
    }

    #[test]
    fn crl_issuer_universal_string_rejects_a_non_scalar_unit() {
        // UniversalString 0x00110000 is above Unicode's largest scalar, U+10ffff.
        let encoded = [0x00, 0x11, 0x00, 0x00];
        let value = Any::from_tag_and_data(Tag::UniversalString, &encoded);

        let error = openssl_canonical_string(&value)
            .expect_err("a UniversalString unit above U+10ffff is not a Unicode scalar value");

        assert_eq!(error, "invalid UniversalString scalar in CRL issuer");
    }

    #[test]
    fn crl_issuer_teletex_string_maps_each_byte_to_the_same_value_character() {
        // TeletexString byte 0xe9 maps to U+00e9, whose UTF-8 encoding is the two bytes c3 a9.
        let encoded = [0x61, 0xe9];
        let value = Any::from_tag_and_data(Tag::TeletexString, &encoded);

        let canonical = openssl_canonical_string(&value)
            .expect("every TeletexString byte denotes a same-value Unicode character")
            .expect("TeletexString is an OpenSSL-canonical string type");

        assert_eq!(
            canonical,
            "aé".as_bytes(),
            "same-value TeletexString characters must then be emitted as UTF-8 bytes"
        );
    }

    #[test]
    fn der_tlv_uses_big_endian_long_form_lengths_above_127_bytes() {
        // DER uses 0x80 | width followed by the content length in width big-endian bytes.
        for (contents_len, expected_length) in
            [(128, &[0x81, 0x80][..]), (256, &[0x82, 0x01, 0x00][..])]
        {
            let contents = vec![0x5a; contents_len];
            let encoded = der_tlv(0x04, &contents);
            let contents_start = 1 + expected_length.len();

            assert_eq!(encoded[0], 0x04, "the TLV tag must be preserved");
            assert_eq!(
                &encoded[1..contents_start],
                expected_length,
                "a {contents_len}-byte value needs the minimal big-endian DER long-form length"
            );
            assert_eq!(
                &encoded[contents_start..],
                contents,
                "the long-form length must not alter the TLV contents"
            );
        }
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
        let invalid_crl = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/invalid_crl.pem");
        let config = format!("host=h sslmode=verify-full sslrootcert={root} sslcrl={invalid_crl}")
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
        let verified =
            format!("host=h sslmode=verify-full sslrootcert={root} sslcrldir={directory}")
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
        let connector = <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(
            &mut make,
            "localhost",
        )
        .expect("rebuild the per-connection verifier");
        assert!(
            !Arc::ptr_eq(&make.config, &connector.config),
            "the pool's initial CRL snapshot was reused"
        );
    }

    /// A caller-supplied `ClientConfig` can now serve the verifying modes,
    /// because the caller says what it verifies.
    ///
    /// Before the `verification` argument existed this was IMPOSSIBLE, not
    /// merely awkward: `new` hard-coded `ServerVerification::None`, the field
    /// is private, and `can_honor_server_verification` compares for equality -
    /// so a connector built from a genuinely verifying config was refused under
    /// `verify-ca` / `verify-full` and under any `sslmode` naming
    /// `sslrootcert`, with no way for the caller to say otherwise. The
    /// in-crate tests had to assign the private field directly, which is
    /// exactly the tell.
    ///
    /// The three cases below are the discriminating set: the level the caller
    /// declares is honoured, a DIFFERENT level is not, and the old default is
    /// still reachable by declaring it.
    /// Build the connector the way a connection does, so the level is checked
    /// after it has crossed `make_tls_connect` rather than only in the maker.
    fn connector_declaring(verification: ServerVerification) -> RustlsConnect {
        let mut make = MakeRustlsConnect::new(Arc::new(client_config(vec![])), verification);
        <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(&mut make, "localhost")
            .expect("make rustls connector")
    }

    #[test]
    fn a_declared_verification_level_is_what_the_connector_attests() {
        let declared = connector_declaring(ServerVerification::ChainAndHostname);
        assert!(
            TlsConnect::<TcpStream>::can_honor_server_verification(
                &declared,
                ServerVerification::ChainAndHostname
            ),
            "a connector told it verifies chain and host name must attest to it"
        );

        // Same construction, one variable changed: a level it was NOT told it
        // performs. Attestation is exact, so this must be refused - otherwise
        // the argument would be decorative and `verify-full` could ride on a
        // connector that only checks the chain.
        assert!(
            !TlsConnect::<TcpStream>::can_honor_server_verification(
                &declared,
                ServerVerification::Chain
            ),
            "attestation must be exact, not 'at least as strong'"
        );

        // The old hard-coded behaviour is still expressible, and still refuses
        // the verifying modes. This is the control for the pair above: it must
        // stay green whatever the argument does.
        let unverified = connector_declaring(ServerVerification::None);
        assert!(
            !TlsConnect::<TcpStream>::can_honor_server_verification(
                &unverified,
                ServerVerification::ChainAndHostname
            ),
            "declaring None must not satisfy a mode that demands verification"
        );
    }
}
