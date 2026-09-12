use crate::{Error, Result, config::TlsConfig};
use compio::{
    BufResult,
    buf::{IoBuf, IoBufMut},
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use compio_tls::{TlsConnector, TlsStream};
use std::sync::Arc;

pub(crate) enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

pub(crate) async fn encrypt(
    stream: TcpStream,
    host: &str,
    config: &TlsConfig,
) -> Result<Transport> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(path) = &config.ca_file {
        let pem = std::fs::read(path)?;
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            roots
                .add(cert?)
                .map_err(|_| Error::Config("invalid TLS CA certificate".into()))?;
        }
    } else {
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    }
    if roots.is_empty() {
        return Err(Error::Config("TLS trust store is empty".into()));
    }
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Config("TLS protocol configuration failed".into()))?
    .with_root_certificates(roots);
    let client = match (&config.cert_file, &config.key_file) {
        (Some(cert), Some(key)) => {
            let pem = std::fs::read(cert)?;
            let certs =
                rustls_pemfile::certs(&mut pem.as_slice()).collect::<std::io::Result<Vec<_>>>()?;
            let pem = std::fs::read(key)?;
            let key = rustls_pemfile::private_key(&mut pem.as_slice())?
                .ok_or_else(|| Error::Config("TLS private key is missing".into()))?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|_| Error::Config("invalid TLS client identity".into()))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(Error::Config(
                "TLS certificate and key must be supplied together".into(),
            ));
        }
    };
    let connector = TlsConnector::from(Arc::new(client));
    let tls = connector
        .connect(config.server_name.as_deref().unwrap_or(host), stream)
        .await?;
    Ok(Transport::Tls(Box::new(tls)))
}

impl AsyncRead for Transport {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Plain(s) => s.read(buf).await,
            Self::Tls(s) => s.read(buf).await,
        }
    }
}

impl AsyncWrite for Transport {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Plain(s) => s.write(buf).await,
            Self::Tls(s) => s.write(buf).await,
        }
    }
    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush().await,
            Self::Tls(s) => s.flush().await,
        }
    }
    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.shutdown().await,
            Self::Tls(s) => s.shutdown().await,
        }
    }
}
