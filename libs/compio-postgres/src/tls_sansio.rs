//! Drive a rustls handshake directly on compio I/O, with no poll-based bridge.
//!
//! rustls is SANS-IO: [`ClientConnection`] is a state machine that never
//! touches a socket. It asks for bytes with `read_tls(&mut dyn Read)`, hands
//! bytes back with `write_tls(&mut dyn Write)`, and those are SYNCHRONOUS
//! `std::io` traits over buffers we own. That is a good fit for completion-based
//! I/O rather than an obstacle: we read into an owned buffer, feed the buffer
//! in, drain the reply into another buffer, and write that.
//!
//! # Why this exists rather than `compio-tls`
//!
//! `compio-tls` wraps `futures-rustls` and keeps the session private. Its
//! `TlsStream` is a private enum with no `split`, no `into_inner`, and no
//! accessor for the `ClientConnection` - 0.9.1 and 0.10.0 alike, checked in the
//! vendored source. `tls_rustls.rs` already works around that once, reading
//! channel-binding material off the raw `futures-rustls` stream "before handing
//! the stream to compio-tls, which exposes only the negotiated ALPN".
//!
//! Owning the handshake removes that whole class of question. It also yields
//! `(socket, connection)` as two separate values, which is what lets a TLS
//! connection be split later: the socket halves go one way, the session is
//! shared, and the connection task stops needing a second run-loop. See task
//! #49 - the point of the exercise is that TLS is a transport and must not
//! decide which protocol implementation runs.
//!
//! Nothing here duplicates `futures-rustls`'s poll bridge
//! (`AsyncStream` + `SyncStream`, 524 lines of self-referential pinned
//! futures). The state machine is driven directly.

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use rustls::ClientConnection;

use crate::Error;

/// Bytes requested per socket read while handshaking.
///
/// A TLS record is at most 16 KiB of plaintext plus overhead, so this holds a
/// whole record in the common case without over-allocating for the handshake,
/// which is a handful of records.
const READ_CHUNK: usize = 16 * 1024;

/// Complete a TLS handshake over `socket`, returning both halves of the result.
///
/// On success the connection is past `is_handshaking`, and any application
/// bytes the server sent alongside the final flight are already inside
/// `connection` - readable through its `reader()`. That matters: those bytes
/// have left the socket, so a caller that kept only the socket would lose them.
/// Returning the connection is what makes the pair safe to separate.
pub(crate) async fn handshake<S>(
    mut socket: S,
    mut connection: ClientConnection,
) -> Result<(S, ClientConnection), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        // Flush first, always. rustls will not make progress on a flight it
        // has not been allowed to send, and the server will not answer one it
        // has not received, so reading before writing deadlocks the handshake
        // rather than merely delaying it.
        while connection.wants_write() {
            let mut out = Vec::new();
            connection
                .write_tls(&mut out)
                .map_err(|error| Error::tls(error.into()))?;
            if out.is_empty() {
                break;
            }
            let BufResult(written, _) = socket.write_all(out).await;
            written.map_err(|error| Error::io(error))?;
        }

        if !connection.is_handshaking() {
            return Ok((socket, connection));
        }

        let BufResult(read, buffer) = socket.read(Vec::with_capacity(READ_CHUNK)).await;
        let read = read.map_err(|error| Error::io(error))?;
        if read == 0 {
            // The peer closed mid-handshake. `Error::tls` rather than a bare
            // EOF: a truncated handshake is a TLS-level failure, and rustls
            // treats silent truncation as an attack rather than an ending.
            return Err(Error::tls(
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the peer closed the connection during the TLS handshake",
                )
                .into(),
            ));
        }

        // `read_tls` consumes as much as one record boundary allows, so this
        // loops until the chunk is drained. `process_new_packets` must run
        // between reads, not after them: it is what advances the state machine,
        // and it is where a bad certificate or a protocol violation surfaces.
        let mut pending = &buffer[..read];
        while !pending.is_empty() {
            connection
                .read_tls(&mut pending)
                .map_err(|error| Error::tls(error.into()))?;
            connection
                .process_new_packets()
                .map_err(|error| Error::tls(error.into()))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compio::buf::{IoBuf, IoBufMut};
    use std::sync::Arc;

    /// A peer that accepts every byte and answers every read with EOF.
    ///
    /// Enough to drive the handshake loop through one full iteration: the
    /// ClientHello is flushed, the connection is still handshaking, and the
    /// read that follows finds the peer gone.
    struct SilentPeer {
        written: usize,
    }

    impl AsyncRead for SilentPeer {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for SilentPeer {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.written += buf.buf_len();
            BufResult(Ok(buf.buf_len()), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn client_connection() -> ClientConnection {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("default protocol versions")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("example.invalid").expect("server name"),
        )
        .expect("client connection")
    }

    /// A peer that vanishes mid-handshake is a TLS-level failure, not a quiet
    /// end. rustls treats silent truncation as an attack rather than a close,
    /// and a caller that saw a bare EOF here could not tell "the server hung
    /// up" from "someone cut the connection during key exchange".
    #[compio::test]
    async fn a_peer_that_closes_mid_handshake_is_refused() {
        let mut peer = SilentPeer { written: 0 };
        let error = handshake(&mut peer, client_connection())
            .await
            .err()
            .expect("a truncated handshake must not succeed");

        let rendered = format!("{error}");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; ");
        assert!(
            rendered.contains("TLS") || chain.contains("TLS") || chain.contains("handshake"),
            "the refusal must read as a TLS failure: {rendered} / {chain}"
        );
    }

    /// The control for the test above: the ClientHello really was flushed
    /// before the read happened. Without this, the refusal would also be
    /// produced by a driver that read first and never wrote at all - which
    /// would deadlock against a real server, because the server answers a
    /// flight it has not received with nothing.
    #[compio::test]
    async fn the_client_hello_is_flushed_before_the_first_read() {
        let mut peer = SilentPeer { written: 0 };
        let _ = handshake(&mut peer, client_connection()).await;
        assert!(
            peer.written > 0,
            "no bytes reached the peer, so the handshake read before it wrote"
        );
    }
}
