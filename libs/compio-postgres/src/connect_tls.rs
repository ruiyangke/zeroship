// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The SSL negotiation dance translates cleanly between tokio's byte-slice
// Ext traits and compio's owned-buffer Ext traits; the control flow is
// identical. We only swap `tokio::io::AsyncReadExt::read_exact(&mut [u8])`
// for compio's `AsyncReadExt::read_exact(buf: T: IoBufMut)`.

use crate::Error;
use crate::config::{SslMode, SslNegotiation};
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::TlsConnect;
use crate::tls::private::ForcePrivateApi;
use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use postgres_protocol::message::frontend;

/// Negotiate TLS over an open socket, returning a stream ready for the
/// Postgres startup message.
///
/// `SslMode::Disable` returns the raw stream unchanged. `SslMode::Prefer`
/// falls back to plain TCP when the server refuses TLS or when the TLS
/// connector cannot actually perform a handshake (e.g., `NoTls`).
/// `SslMode::Require` returns an error in those cases.
#[allow(dead_code)]
pub async fn connect_tls<S, T>(
    mut stream: S,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
) -> Result<MaybeTlsStream<S, T::Stream>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    match mode {
        SslMode::Disable => return Ok(MaybeTlsStream::Raw(stream)),
        SslMode::Prefer if !tls.can_connect(ForcePrivateApi) => {
            return Ok(MaybeTlsStream::Raw(stream));
        }
        SslMode::Prefer if negotiation == SslNegotiation::Direct => {
            return Err(Error::tls(
                "weak sslmode \"prefer\" may not be used with sslnegotiation=direct (use \"require\")"
                    .into(),
            ));
        }
        SslMode::Prefer | SslMode::Require => {}
    }

    if negotiation == SslNegotiation::Postgres {
        let mut buf = BytesMut::new();
        frontend::ssl_request(&mut buf);
        // AsyncWriteExt::write_all consumes the buffer and returns it; we
        // don't need the buffer back, so discard via destructuring.
        let compio::BufResult(res, _) = stream.write_all(buf.to_vec()).await;
        res.map_err(Error::io)?;

        // One-byte negotiation response: 'S' = accepted, 'N' = rejected.
        let resp = vec![0u8; 1];
        let compio::BufResult(res, resp) = stream.read_exact(resp).await;
        res.map_err(Error::io)?;

        if resp[0] != b'S' {
            if SslMode::Require == mode {
                return Err(Error::tls("server does not support TLS".into()));
            } else {
                return Ok(MaybeTlsStream::Raw(stream));
            }
        }
    }

    if !has_hostname {
        return Err(Error::tls("no hostname provided for TLS handshake".into()));
    }

    let stream = tls
        .connect(stream)
        .await
        .map_err(|e| Error::tls(e.into()))?;

    Ok(MaybeTlsStream::Tls(stream))
}
