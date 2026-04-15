use std::sync::Arc;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use compio_tls::TlsStream;

/// A unified stream that is either a plain TcpStream or a TLS-wrapped one.
/// Provides `read` and `write_all` operations that mirror what thread.rs uses,
/// so the HTTP worker loop can call them without knowing whether TLS is active.
pub enum MaybeStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl MaybeStream {
    /// Read into `buf`, returning the buffer and result (mirrors compio BufResult pattern).
    pub async fn read(&mut self, buf: Vec<u8>) -> compio::BufResult<usize, Vec<u8>> {
        match self {
            MaybeStream::Plain(s) => s.read(buf).await,
            MaybeStream::Tls(s) => s.read(buf).await,
        }
    }

    /// Write all bytes from `buf`.
    pub async fn write_all(&mut self, buf: Vec<u8>) -> compio::BufResult<(), Vec<u8>> {
        match self {
            MaybeStream::Plain(s) => s.write_all(buf).await,
            MaybeStream::Tls(s) => s.write_all(buf).await,
        }
    }
}

/// Build a rustls `ClientConfig` that accepts any server certificate.
///
/// For production use you would want real certificate verification.
/// `--insecure` / development mode is the intended use-case here.
fn insecure_client_config() -> Arc<rustls::ClientConfig> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    Arc::new(config)
}

/// Perform a TLS handshake on `stream` and return a `MaybeStream::Tls`.
pub async fn wrap_tls(
    stream: TcpStream,
    host: &str,
) -> Result<MaybeStream, String> {
    let tls_config = insecure_client_config();
    let connector = compio_tls::TlsConnector::from(tls_config);
    connector
        .connect(host, stream)
        .await
        .map(|s| MaybeStream::Tls(Box::new(s)))
        .map_err(|e| format!("TLS handshake failed: {e}"))
}

/// Connect and optionally wrap in TLS based on the URL scheme.
pub async fn connect(host: &str, port: u16, tls: bool) -> Result<MaybeStream, String> {
    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect(addr.as_str())
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    if tls {
        wrap_tls(stream, host).await
    } else {
        Ok(MaybeStream::Plain(stream))
    }
}

// ---------------------------------------------------------------------------
// Certificate verifier that accepts everything (development / benchmarking).
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer,
        _intermediates: &[rustls::pki_types::CertificateDer],
        _server_name: &rustls::pki_types::ServerName,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
