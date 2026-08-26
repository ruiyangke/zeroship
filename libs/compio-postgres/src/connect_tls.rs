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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{ChannelBinding, TlsStream};
    use compio::buf::{IoBuf, IoBufMut};
    use compio::net::{TcpListener, TcpStream};
    use std::future::Future;
    use std::pin::Pin;

    /// A real TCP peer that reads the 8-byte `SSLRequest` and replies with a
    /// fixed script, then closes.
    ///
    /// A real socket rather than a hand-rolled mock: the property under test is
    /// "how many bytes were taken off the wire", and a mock's answer to that is
    /// whatever the mock was written to say.
    async fn scripted_server(server_says: Vec<u8>) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        compio::runtime::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let compio::BufResult(res, _) = sock.read_exact(vec![0u8; 8]).await;
            res.unwrap();
            let compio::BufResult(res, _) = sock.write_all(server_says).await;
            res.unwrap();
            sock.flush().await.unwrap();
        })
        .detach();

        TcpStream::connect(addr).await.unwrap()
    }

    /// A connector that performs no handshake and simply hands the socket back,
    /// so a test can inspect what negotiation left unread on it.
    struct PassthroughTls;

    struct PassthroughStream<S>(S);

    /// Unsplittable on purpose: this fixture exercises the connector
    /// contract, not a run loop, so it takes the serialized path.
    impl<S: AsyncRead + AsyncWrite + Unpin + 'static> crate::buf_stream::SplitStream
        for PassthroughStream<S>
    {
        type ReadHalf = PassthroughStream<S>;
        type WriteHalf = PassthroughStream<S>;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Err(self)
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for PassthroughStream<S> {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            self.0.read(buf).await
        }
    }
    impl<S: AsyncWrite + Unpin> AsyncWrite for PassthroughStream<S> {
        async fn write<B: IoBuf>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            self.0.write(buf).await
        }
        async fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush().await
        }
        async fn shutdown(&mut self) -> std::io::Result<()> {
            self.0.shutdown().await
        }
    }
    impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for PassthroughStream<S> {
        fn channel_binding(&self) -> ChannelBinding {
            ChannelBinding::none()
        }
    }

    impl<S> TlsConnect<S> for PassthroughTls
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        type Stream = PassthroughStream<S>;
        type Error = std::io::Error;
        #[allow(clippy::type_complexity)]
        type Future = Pin<Box<dyn Future<Output = Result<PassthroughStream<S>, std::io::Error>>>>;

        fn connect(self, stream: S) -> Self::Future {
            Box::pin(async move { Ok(PassthroughStream(stream)) })
        }

        fn can_connect(&self, _: ForcePrivateApi) -> bool {
            true
        }
    }

    /// The `SSLRequest` response is read one byte at a time, so bytes a man in
    /// the middle appends to it stay on the socket instead of being swallowed
    /// into a buffer the startup parser would later read as authenticated
    /// server messages.
    ///
    /// This is CVE-2021-23222. libpq closes the hole by *detecting* it after
    /// the handshake ("received unencrypted data after SSL response"); this
    /// driver closes it by never over-reading, which is why the assertion here
    /// is about what is still unread rather than about an error. Replace the
    /// one-byte `read_exact` with any buffered read and this fails: the
    /// injected bytes vanish from the stream.
    #[compio::test]
    async fn ssl_response_read_consumes_exactly_one_byte() {
        const INJECTED: &[u8] = b"INJECTED-BY-A-MITM";

        let mut script = vec![b'S'];
        script.extend_from_slice(INJECTED);

        let negotiated = negotiate_tls(
            scripted_server(script).await,
            Encryption::Tls,
            SslMode::Require,
            SslNegotiation::Postgres,
            PassthroughTls,
            true,
        )
        .await
        .expect("the scripted server accepted TLS");

        let mut negotiated = match negotiated {
            MaybeTlsStream::Tls(s) => s,
            MaybeTlsStream::Raw(_) => panic!("the server answered S; this must be the TLS arm"),
        };

        let rest = vec![0u8; INJECTED.len()];
        let compio::BufResult(read, rest) = negotiated.read(rest).await;
        assert_eq!(read.unwrap(), INJECTED.len());
        assert_eq!(
            rest, INJECTED,
            "negotiation buffered post-response bytes; they must stay on the socket"
        );
    }

    /// The other half: a server that refuses TLS leaves the socket usable, and
    /// the startup packet goes out on it with no reconnect. Only the one
    /// negotiation byte is consumed.
    #[compio::test]
    async fn a_refusal_reuses_the_socket_for_a_mode_that_permits_plaintext() {
        let mut script = vec![b'N'];
        script.extend_from_slice(b"BackendMessages");

        let negotiated = negotiate_tls(
            scripted_server(script).await,
            Encryption::Tls,
            SslMode::Prefer,
            SslNegotiation::Postgres,
            PassthroughTls,
            true,
        )
        .await
        .expect("prefer continues in plaintext when the server answers N");

        let mut raw = match negotiated {
            MaybeTlsStream::Raw(s) => s,
            MaybeTlsStream::Tls(_) => panic!("the server answered N; this cannot be encrypted"),
        };

        let rest = vec![0u8; b"BackendMessages".len()];
        let compio::BufResult(read, rest) = raw.read(rest).await;
        read.unwrap();
        assert_eq!(rest, b"BackendMessages", "the socket must still be usable");
    }

    /// The same refusal under a mode that requires TLS is an error, and there
    /// is no arm in which it is not.
    #[compio::test]
    async fn a_refusal_is_fatal_for_every_mode_that_requires_tls() {
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            let err = negotiate_tls(
                scripted_server(vec![b'N']).await,
                Encryption::Tls,
                mode,
                SslNegotiation::Postgres,
                PassthroughTls,
                true,
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("sslmode={} accepted a plaintext session", mode.as_str()));
            assert!(
                !err.is_tls_handshake(),
                "a refusal is not a handshake failure"
            );
        }
    }

    /// A server error during the SSL exchange is fatal even for the modes that
    /// permit plaintext. libpq refuses to fall back here - and refuses to read
    /// the message - because the server has not authenticated itself yet.
    #[compio::test]
    async fn an_error_response_during_the_ssl_exchange_is_fatal_in_every_mode() {
        for mode in [
            SslMode::Allow,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyFull,
        ] {
            negotiate_tls(
                scripted_server(vec![b'E']).await,
                Encryption::Tls,
                mode,
                SslNegotiation::Postgres,
                PassthroughTls,
                true,
            )
            .await
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "sslmode={} treated an ErrorResponse as a refusal",
                    mode.as_str()
                )
            });
        }
    }

    /// The ordering swap that is the whole difference between `allow` and
    /// `prefer`.
    #[test]
    fn allow_offers_plaintext_first_and_prefer_offers_tls_first() {
        assert_eq!(
            Encryption::first_for(SslMode::Allow),
            Encryption::Plaintext,
            "allow is the plaintext-first mode"
        );
        assert_eq!(
            Encryption::first_for(SslMode::Disable),
            Encryption::Plaintext
        );
        for mode in [
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            assert_eq!(
                Encryption::first_for(mode),
                Encryption::Tls,
                "{} offers TLS first",
                mode.as_str()
            );
        }
    }
}

/// TLS could not be used on this attempt. Continue in plaintext if the mode
/// allows it; otherwise report why, in libpq's words.
///
/// This is the single place the "may I downgrade?" question is asked, and it
/// asks it of [`SslMode::permits_plaintext`] - the set membership, not a list
/// of mode names. `require`, `verify-ca` and `verify-full` cannot reach the
/// `Ok` arm.
fn unavailable<S, T>(stream: S, mode: SslMode, why: &str) -> Result<MaybeTlsStream<S, T>, Error> {
    if mode.permits_plaintext() {
        Ok(MaybeTlsStream::Raw(stream))
    } else {
        Err(Error::tls(
            format!("{why}, but SSL was required by sslmode={}", mode.as_str()).into(),
        ))
    }
}
