// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The SSL negotiation dance translates cleanly between tokio's byte-slice
// Ext traits and compio's owned-buffer Ext traits; the control flow is
// identical. We only swap `tokio::io::AsyncReadExt::read_exact(&mut [u8])`
// for compio's `AsyncReadExt::read_exact(buf: T: IoBufMut)`.
//
// The `sslmode` policy that used to live here does NOT any more. This file
// performs ONE attempt on ONE socket; choosing which transport to attempt, and
// what to do when an attempt fails, is `connect.rs`'s job, because the answer
// for `allow` and `prefer` is "open a different socket" and a socket is not
// something this function owns.

use crate::Error;
use crate::config::{SslMode, SslNegotiation};
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::TlsConnect;
use crate::tls::private::ForcePrivateApi;
use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use postgres_protocol::message::frontend;

/// Which transport a single connection attempt should use.
///
/// libpq's `current_enc_method`. It is a decision, not a preference: by the
/// time it reaches [`negotiate_tls`] the mode has already been consulted.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Encryption {
    /// Send the startup packet in the clear, with no `SSLRequest` at all.
    Plaintext,
    /// Ask for TLS and hand the startup packet to the encrypted stream.
    Tls,
}

impl Encryption {
    /// The transport this mode offers first.
    ///
    /// libpq's `select_next_encryption_method`, whose whole content is this
    /// ordering swap: `allow` offers plaintext first, every other mode offers
    /// TLS first (and `disable` has only plaintext to offer).
    pub(crate) const fn first_for(mode: SslMode) -> Encryption {
        match mode {
            SslMode::Disable | SslMode::Allow => Encryption::Plaintext,
            SslMode::Prefer | SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
                Encryption::Tls
            }
        }
    }
}

/// Negotiate one attempt over an open socket, returning a stream ready for the
/// Postgres startup message.
///
/// `mode` is read for exactly one decision - what a server's `N` (refusal)
/// means - and nothing else. Every other use of the mode has already happened
/// in the caller.
///
/// # Errors
///
/// A TLS *handshake* failure is [`Error::tls_handshake`], which the caller can
/// distinguish with [`Error::is_tls_handshake`]. That distinction is
/// load-bearing: it is the only failure `prefer` retries in plaintext. A
/// startup or authentication failure must NOT be retried in the clear, or a
/// mistyped password would be re-sent unencrypted on the second attempt.
pub(crate) async fn negotiate_tls<S, T>(
    mut stream: S,
    encryption: Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
) -> Result<MaybeTlsStream<S, T::Stream>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    if encryption == Encryption::Plaintext {
        return Ok(MaybeTlsStream::Raw(stream));
    }

    // No connector compiled in (`NoTls`), or no name to put in the handshake.
    // libpq treats both as "this transport is unavailable": a mode that permits
    // plaintext uses plaintext, a mode that does not gets an error. Answering
    // here rather than after `SSLRequest` also keeps the wire quiet - there is
    // no point asking the server for something we cannot complete.
    if !tls.can_connect(ForcePrivateApi) {
        return unavailable(stream, mode, "no TLS connector is configured");
    }
    if !has_hostname {
        return unavailable(stream, mode, "no hostname provided for TLS handshake");
    }

    if negotiation == SslNegotiation::Postgres {
        let mut buf = BytesMut::new();
        frontend::ssl_request(&mut buf);
        // AsyncWriteExt::write_all consumes the buffer and returns it; we
        // don't need the buffer back, so discard via destructuring.
        let compio::BufResult(res, _) = stream.write_all(buf.to_vec()).await;
        res.map_err(Error::io)?;

        // Exactly one byte, never more. That is not an optimisation: reading
        // ahead here would buffer bytes received BEFORE the handshake, which by
        // definition arrived unencrypted and may have been injected by a man in
        // the middle. libpq guards the same hole with an explicit "received
        // unencrypted data after SSL response" check after the handshake
        // (CVE-2021-23222); `read_exact` of a one-byte buffer makes the
        // over-read impossible instead of detectable.
        let resp = vec![0u8; 1];
        let compio::BufResult(res, resp) = stream.read_exact(resp).await;
        res.map_err(Error::io)?;

        match resp[0] {
            // Accepted.
            b'S' => {}
            // Refused. The socket is still in a known state - one byte
            // consumed, nothing else sent - so a mode that permits plaintext
            // continues the startup on THIS connection. libpq does the same
            // (`ENCRYPTION_NEGOTIATION_FAILED` returning `CONNECTION_MADE`);
            // no reconnect is needed and none is done.
            b'N' => {
                return unavailable(stream, mode, "server does not support SSL");
            }
            // A server error during the SSL exchange is fatal in every mode,
            // including the ones that permit plaintext. libpq deliberately does
            // not even read the message: the server has not authenticated
            // itself yet, so its bytes are not to be trusted or repeated.
            b'E' => {
                return Err(Error::tls(
                    "server sent an error response during SSL exchange".into(),
                ));
            }
            other => {
                return Err(Error::tls(
                    format!("unexpected response to SSLRequest: {:?}", other as char).into(),
                ));
            }
        }
    }

    let stream = tls
        .connect(stream)
        .await
        .map_err(|e| Error::tls_handshake(e.into()))?;

    Ok(MaybeTlsStream::Tls(stream))
}

/// TLS could not be used on this attempt. Continue in plaintext if the mode
/// allows it; otherwise report why, in libpq's words.
///
/// This is the single place the "may I downgrade?" question is asked, and it
/// asks it of [`SslMode::permits_plaintext`] - the set membership, not a list
/// of mode names. `require`, `verify-ca` and `verify-full` cannot reach the
/// `Ok` arm.
fn unavailable<S, T>(
    stream: S,
    mode: SslMode,
    why: &str,
) -> Result<MaybeTlsStream<S, T>, Error> {
    if mode.permits_plaintext() {
        Ok(MaybeTlsStream::Raw(stream))
    } else {
        Err(Error::tls(
            format!("{why}, but SSL was required by sslmode={}", mode.as_str()).into(),
        ))
    }
}
