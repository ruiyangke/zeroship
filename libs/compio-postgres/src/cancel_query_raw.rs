// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The control flow is identical to tokio-postgres; only the trait names
// change. compio's `AsyncWriteExt::write_all` takes an owned buffer and
// returns the buffer; we throw it away with `.0`. Shutdown is a plain
// `AsyncWrite` method.

use crate::Error;
use crate::config::{SslMode, SslNegotiation};
use crate::connect_tls;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::TlsConnect;
use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use postgres_protocol::message::frontend;
use std::io;

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
    // The stream belongs to the caller, so there is no second socket to dial:
    // `allow` and `prefer` get the transport they attempt first, and no
    // fallback. What is lost is the retry.
    //
    // A CancelRequest carries NO query text, but it does carry the backend's
    // process id and secret key, and that pair is a BEARER CREDENTIAL: anyone
    // who reads it off the wire can cancel that backend's queries at will.
    // Sending it unencrypted is a real exposure, not a free trade.
    //
    // Two facts, kept apart because they point in opposite directions.
    // MEASURED against PostgreSQL 16: the server accepts a plaintext cancel
    // even when it is `hostssl`-only, because the postmaster dispatches a
    // CancelRequest before startup, authentication and HBA. Cancelling a
    // `pg_sleep(30)` that way returned SQLSTATE 57014 to the TLS session that
    // had issued it. So the cancel is DELIVERED.
    //
    // That is not permission to send it in the clear. libpq's plaintext cancel
    // path - `PQcancel` / `PQrequestCancel` - is DEPRECATED, and PostgreSQL's
    // own documentation gives this exact reason: it does not send the request
    // encrypted even when the original connection required encryption. The
    // current `PQcancelCreate` / `PQcancelBlocking` path reuses the original
    // connection's `sslmode`. An earlier version of this comment cited libpq as
    // precedent for plaintext; it was citing the interface upstream retired.
    //
    // `cancel_query::cancel_query` owns its socket and so applies the
    // address-aware rule this function has no address to apply. It does NOT
    // add a retry either - neither entry point has one.
    cancel_query_with_encryption(
        stream,
        connect_tls::Encryption::first_for(mode),
        mode,
        negotiation,
        tls,
        has_hostname,
        process_id,
        secret_key,
    )
    .await
}

pub(crate) async fn cancel_query_with_encryption<S, T>(
    stream: S,
    encryption: connect_tls::Encryption,
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
    send_cancel_request_with_encryption(
        stream,
        encryption,
        mode,
        negotiation,
        tls,
        has_hostname,
        process_id,
        secret_key,
    )
    .await
    .map(drop)
}

/// Send and flush a cancel packet, returning its half-closed connection.
///
/// Returning the stream lets pool recovery wait for the postmaster's EOF
/// outside `connect_timeout`. That setting remains clock (4), not a socket read
/// deadline; the pool's separately bounded recovery grace owns the EOF wait.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_cancel_request_with_encryption<S, T>(
    stream: S,
    encryption: connect_tls::Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    process_id: i32,
    secret_key: i32,
) -> Result<MaybeTlsStream<S, T::Stream>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let mut stream =
        connect_tls::negotiate_tls(stream, encryption, mode, negotiation, tls, has_hostname)
            .await?;

    let mut buf = BytesMut::new();
    frontend::cancel_request(process_id, secret_key, &mut buf);

    // compio's write_all is owned-buffer. Throw away the buffer.
    let compio::BufResult(res, _) = stream.write_all(buf.to_vec()).await;
    res.map_err(Error::io)?;
    stream.flush().await.map_err(Error::io)?;
    stream.shutdown().await.map_err(Error::io)?;

    Ok(stream)
}

/// Wait until the postmaster closes a connection whose cancel packet was sent.
///
/// `CancelRequest` has no protocol response. EOF is nevertheless a delivery
/// barrier: `PostgreSQL` closes this dedicated connection only after consuming
/// the startup packet. Without it, a delayed cancel could arrive after the
/// main session's `Sync` and cancel that backend's next query.
pub(crate) async fn wait_for_server_close<S, T>(
    mut stream: MaybeTlsStream<S, T>,
) -> Result<(), Error>
where
    S: AsyncRead + Unpin,
    T: AsyncRead + Unpin,
{
    let tls = matches!(&stream, MaybeTlsStream::Tls(_));
    let compio::BufResult(result, _) = stream.read(vec![0; 1]).await;
    match result {
        Ok(0) => Ok(()),
        Ok(_) => Err(Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "PostgreSQL sent data on a CancelRequest connection",
        ))),
        // PostgreSQL closes the cancel connection without a TLS close_notify.
        // rustls reports that confirmed TCP EOF as UnexpectedEof; accepting it
        // here is specific to this one-way startup packet, not a general
        // relaxation of TLS truncation.
        Err(error) if tls && error.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(error) => Err(Error::io(error)),
    }
}
