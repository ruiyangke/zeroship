//! Shared rustls connector construction for WebSocket and `node:tls`.

#![cfg(feature = "runtime_tls")]

use std::io;
use std::sync::Arc;

use base64::Engine;
use compio_tls::TlsConnector;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

#[derive(Debug, Clone)]
pub struct TlsConnectorOptions {
    pub reject_unauthorized: bool,
    pub ca_pem: Option<String>,
}

impl Default for TlsConnectorOptions {
    fn default() -> Self {
        Self {
            reject_unauthorized: true,
            ca_pem: None,
        }
    }
}

pub fn build_tls_connector(opts: &TlsConnectorOptions) -> io::Result<TlsConnector> {
    if !opts.reject_unauthorized {
        let cfg = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth();
        return Ok(TlsConnector::from(Arc::new(cfg)));
    }

    let mut root_store = RootCertStore::empty();
    if let Some(ca_pem) = opts.ca_pem.as_deref() {
        let certs = parse_ca_certs(ca_pem)?;
        let added = root_store.add_parsable_certificates(certs);
        if added.0 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ca did not contain any parseable certificates",
            ));
        }
    } else {
        let rustls_native_certs::CertificateResult { certs, errors: _errors, .. } =
            rustls_native_certs::load_native_certs();
        let _ = root_store.add_parsable_certificates(certs);
    }

    if root_store.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no root CA certificates found",
        ));
    }

    let cfg = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(cfg)))
}

fn parse_ca_certs(input: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    let mut certs = Vec::new();
    let mut rest = input;
    while let Some(begin) = rest.find("-----BEGIN CERTIFICATE-----") {
        let after_begin = &rest[begin + "-----BEGIN CERTIFICATE-----".len()..];
        let Some(end) = after_begin.find("-----END CERTIFICATE-----") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unterminated PEM certificate",
            ));
        };
        let body = &after_begin[..end];
        let b64: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        certs.push(CertificateDer::from(der));
        rest = &after_begin[end + "-----END CERTIFICATE-----".len()..];
    }

    if !certs.is_empty() {
        return Ok(certs);
    }

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "ca must contain PEM-encoded certificates",
    ))
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
