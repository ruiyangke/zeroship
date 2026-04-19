// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Phase 4 port: the control flow is identical to tokio-postgres; only the
// trait names change. compio's `AsyncWriteExt::write_all` takes an owned
// buffer and returns the buffer; we throw it away with `.0`. Shutdown is a
// plain `AsyncWrite` method.

use crate::Error;
use crate::config::{SslMode, SslNegotiation};
use crate::connect_tls;
use crate::tls::TlsConnect;
use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use postgres_protocol::message::frontend;

pub async fn cancel_query_raw<S, T>(
    stream: S,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    process_id: i32,
    secret_key: i32,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let mut stream = connect_tls::connect_tls(stream, mode, negotiation, tls, has_hostname).await?;

    let mut buf = BytesMut::new();
    frontend::cancel_request(process_id, secret_key, &mut buf);

    // compio's write_all is owned-buffer. Throw away the buffer.
    let compio::BufResult(res, _) = stream.write_all(buf.to_vec()).await;
    res.map_err(Error::io)?;
    stream.flush().await.map_err(Error::io)?;
    stream.shutdown().await.map_err(Error::io)?;

    Ok(())
}
