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
use crate::encryption::Encryption;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::private::ForcePrivateApi;
use crate::tls::{POSTGRESQL_ALPN_PROTOCOL, TlsConnect, TlsStream};
use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use postgres_protocol::message::frontend;

/// Negotiate one attempt over an open socket, returning a stream ready for the
/// Postgres startup message.
///
/// `mode` drives both what a server's `N` (refusal) means and whether
/// `verify-full` without a hostname is refused before any bytes are written.
/// The caller supplies `has_hostname` as a separate bit; it does not fold
/// hostname presence into `mode`.
///
/// # Errors
///
/// A TLS *handshake* failure is [`Error::tls_handshake`], which the caller can
/// distinguish with [`Error::is_tls_handshake`]. That distinction is
/// load-bearing: it is the only failure `prefer` retries in plaintext. A
/// startup or authentication failure must NOT be retried in the clear, or a
/// mistyped password would be re-sent unencrypted on the second attempt.
pub(crate) async fn negotiate_tls<S, T>(
    stream: S,
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
    negotiate_tls_inner(
        stream,
        encryption,
        mode,
        negotiation,
        tls,
        has_hostname,
        mode == SslMode::Prefer,
    )
    .await
}

/// Negotiate a transport that was already selected by an earlier connection.
/// Unlike [`negotiate_tls`], an unavailable TLS transport is never converted
/// to plaintext: callers use this to replay an actually encrypted session.
pub(crate) async fn negotiate_tls_exact<S, T>(
    stream: S,
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
    negotiate_tls_inner(
        stream,
        encryption,
        mode,
        negotiation,
        tls,
        has_hostname,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn negotiate_tls_inner<S, T>(
    mut stream: S,
    encryption: Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    permit_plaintext: bool,
) -> Result<MaybeTlsStream<S, T::Stream>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    if encryption == Encryption::Plaintext {
        return Ok(MaybeTlsStream::Raw(stream));
    }

    // No connector compiled in (`NoTls`), or no name for verify-full to
    // authenticate. libpq permits hostaddr-only TLS for every other mode.
    // Answering impossible cases here rather than after `SSLRequest` also
    // keeps the wire quiet.
    if !tls.can_connect(ForcePrivateApi) {
        return unavailable(
            stream,
            mode,
            permit_plaintext,
            "no TLS connector is configured",
        );
    }
    if !has_hostname && mode == SslMode::VerifyFull {
        return unavailable(
            stream,
            mode,
            permit_plaintext,
            "no hostname provided for sslmode=verify-full",
        );
    }

    if negotiation == SslNegotiation::Postgres {
        let mut buf = BytesMut::new();
        frontend::ssl_request(&mut buf);
        // AsyncWriteExt::write_all consumes the buffer and returns it; we
        // don't need the buffer back, so discard via destructuring.
        let compio::BufResult(res, _) = stream.write_all(buf.to_vec()).await;
        res.map_err(Error::io)?;
        stream.flush().await.map_err(Error::io)?;

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
            // consumed, nothing else sent. `prefer` can continue the startup
            // on THIS connection because TLS was its first attempt. `allow`
            // reaches this point only after its plaintext attempt failed, so
            // another plaintext startup would repeat an exhausted method.
            b'N' => {
                return unavailable(
                    stream,
                    mode,
                    permit_plaintext,
                    "server does not support SSL",
                );
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

    // Direct TLS omits PostgreSQL's SSLRequest discriminator, so ALPN is the
    // protocol-confusion defense. libpq requires the server to select the one
    // registered PostgreSQL protocol and rejects both an absent selection and
    // any other value before it sends the startup packet.
    if negotiation == SslNegotiation::Direct {
        match stream.negotiated_alpn_protocol() {
            Some(protocol) if protocol == POSTGRESQL_ALPN_PROTOCOL => {}
            None => {
                return Err(Error::tls_handshake(
                    "direct SSL connection was established without ALPN protocol negotiation \
                     extension"
                        .into(),
                ));
            }
            Some(_) => {
                return Err(Error::tls_handshake(
                    "SSL connection was established with unexpected ALPN protocol".into(),
                ));
            }
        }
    }

    Ok(MaybeTlsStream::Tls(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{ChannelBinding, POSTGRESQL_ALPN_PROTOCOL, TlsStream};
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
    struct PassthroughTls {
        negotiated_alpn_protocol: Option<&'static [u8]>,
    }

    struct PassthroughStream<S> {
        inner: S,
        negotiated_alpn_protocol: Option<&'static [u8]>,
    }

    /// A transport whose writes do not become readable by its peer until
    /// `flush`, matching the contract `AsyncWrite` permits.
    struct FlushRequiredStream {
        response: &'static [u8],
        pending: Vec<u8>,
        flushed: Vec<u8>,
    }

    impl AsyncRead for FlushRequiredStream {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            if self.flushed.is_empty() {
                return compio::BufResult(
                    Err(std::io::Error::other(
                        "SSLRequest response was read before the request was flushed",
                    )),
                    buf,
                );
            }
            self.response.read(buf).await
        }
    }

    impl AsyncWrite for FlushRequiredStream {
        async fn write<B: IoBuf>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            self.pending.extend_from_slice(buf.as_init());
            let written = buf.buf_len();
            compio::BufResult(Ok(written), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.flushed.append(&mut self.pending);
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

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
            self.inner.read(buf).await
        }
    }
    impl<S: AsyncWrite + Unpin> AsyncWrite for PassthroughStream<S> {
        async fn write<B: IoBuf>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            self.inner.write(buf).await
        }
        async fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush().await
        }
        async fn shutdown(&mut self) -> std::io::Result<()> {
            self.inner.shutdown().await
        }
    }
    impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for PassthroughStream<S> {
        fn channel_binding(&self) -> ChannelBinding {
            ChannelBinding::none()
        }

        fn negotiated_alpn_protocol(&self) -> Option<&[u8]> {
            self.negotiated_alpn_protocol
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
            Box::pin(async move {
                Ok(PassthroughStream {
                    inner: stream,
                    negotiated_alpn_protocol: self.negotiated_alpn_protocol,
                })
            })
        }

        fn can_connect(&self, _: ForcePrivateApi) -> bool {
            true
        }
    }

    #[compio::test]
    async fn ssl_request_is_flushed_before_reading_the_response() {
        let negotiated = negotiate_tls(
            FlushRequiredStream {
                response: b"S",
                pending: Vec::new(),
                flushed: Vec::new(),
            },
            Encryption::Tls,
            SslMode::Require,
            SslNegotiation::Postgres,
            PassthroughTls {
                negotiated_alpn_protocol: None,
            },
            true,
        )
        .await
        .expect("a flushed SSLRequest should receive its scripted response");

        let stream = match negotiated {
            MaybeTlsStream::Tls(stream) => stream,
            MaybeTlsStream::Raw(_) => panic!("the scripted peer accepted TLS"),
        };
        assert_eq!(
            stream.inner.flushed,
            [0, 0, 0, 8, 4, 210, 22, 47],
            "the transport did not receive the complete SSLRequest before the response read"
        );
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
            PassthroughTls {
                negotiated_alpn_protocol: None,
            },
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

    #[compio::test]
    async fn hostaddr_only_require_still_negotiates_tls() {
        let negotiated = negotiate_tls(
            scripted_server(vec![b'S']).await,
            Encryption::Tls,
            SslMode::Require,
            SslNegotiation::Postgres,
            PassthroughTls {
                negotiated_alpn_protocol: None,
            },
            false,
        )
        .await
        .expect("libpq permits TLS without a host unless verify-full needs it");

        assert!(matches!(negotiated, MaybeTlsStream::Tls(_)));
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
            PassthroughTls {
                negotiated_alpn_protocol: None,
            },
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

    /// `allow` has already tried plaintext by the time its TLS leg sends an
    /// `SSLRequest`. A refusal therefore exhausts the mode; returning the raw
    /// second socket would send a second plaintext StartupMessage, which
    /// libpq's encryption-method state machine never does.
    #[compio::test]
    async fn allow_tls_refusal_does_not_send_a_second_plaintext_startup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let compio::BufResult(result, request) = socket.read_exact(vec![0u8; 8]).await;
            result.unwrap();
            assert_eq!(u32::from_be_bytes(request[..4].try_into().unwrap()), 8);
            assert_eq!(
                u32::from_be_bytes(request[4..].try_into().unwrap()),
                80_877_103
            );

            let compio::BufResult(result, _) = socket.write_all(vec![b'N']).await;
            result.unwrap();
            socket.flush().await.unwrap();

            let compio::BufResult(result, bytes) = socket.read(vec![0u8; 1]).await;
            match result {
                Ok(0) => None,
                Ok(read) => Some(bytes[..read].to_vec()),
                Err(error) => panic!("read after SSL refusal failed: {error}"),
            }
        });

        let result = negotiate_tls(
            TcpStream::connect(addr).await.unwrap(),
            Encryption::Tls,
            SslMode::Allow,
            SslNegotiation::Postgres,
            PassthroughTls {
                negotiated_alpn_protocol: None,
            },
            true,
        )
        .await;

        // If the pre-fix implementation hands this socket back as plaintext,
        // drive the returned stream exactly as the startup path would. This
        // makes the regression assert the bytes on the peer, not just an enum.
        let result = match result {
            Ok(MaybeTlsStream::Raw(mut stream)) => {
                let mut startup = BytesMut::new();
                frontend::startup_message([("user", "postgres")], &mut startup).unwrap();
                let compio::BufResult(write, _) = stream.write_all(startup.to_vec()).await;
                write.unwrap();
                stream.flush().await.unwrap();
                drop(stream);
                Ok(())
            }
            Ok(MaybeTlsStream::Tls(_)) => panic!("the server refused TLS"),
            Err(error) => Err(error),
        };

        let observed = server.await.expect("scripted server task");
        assert_eq!(
            observed, None,
            "a second plaintext StartupMessage reached the server after its SSL refusal"
        );
        assert!(
            result.is_err(),
            "sslmode=allow repeated plaintext after its first plaintext leg failed"
        );
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
                PassthroughTls {
                    negotiated_alpn_protocol: None,
                },
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
                PassthroughTls {
                    negotiated_alpn_protocol: None,
                },
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

    async fn direct_socket() -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        compio::runtime::spawn(async move {
            let _ = listener.accept().await.unwrap();
        })
        .detach();

        TcpStream::connect(addr).await.unwrap()
    }

    #[compio::test]
    async fn direct_requires_postgresql_alpn() {
        for (selected, expected) in [
            (None, "without ALPN protocol negotiation extension"),
            (Some(b"h2".as_slice()), "unexpected ALPN protocol"),
        ] {
            let error = negotiate_tls(
                direct_socket().await,
                Encryption::Tls,
                SslMode::Require,
                SslNegotiation::Direct,
                PassthroughTls {
                    negotiated_alpn_protocol: selected,
                },
                true,
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("direct TLS accepted selected ALPN {selected:?}"));

            assert!(error.is_tls_handshake(), "ALPN is part of the handshake");
            let source = std::error::Error::source(&error)
                .expect("ALPN refusal carries the libpq-compatible reason")
                .to_string();
            assert!(
                source.contains(expected),
                "wrong error for selected ALPN {selected:?}: {source}"
            );
        }

        let negotiated = negotiate_tls(
            direct_socket().await,
            Encryption::Tls,
            SslMode::Require,
            SslNegotiation::Direct,
            PassthroughTls {
                negotiated_alpn_protocol: Some(POSTGRESQL_ALPN_PROTOCOL),
            },
            true,
        )
        .await
        .expect("direct TLS accepts the registered PostgreSQL ALPN protocol");
        assert!(matches!(negotiated, MaybeTlsStream::Tls(_)));
    }
}

/// TLS could not be used on this attempt. Continue in plaintext only when TLS
/// was the mode's first offer; otherwise report why.
///
/// `prefer` is the only TLS-first mode with a plaintext method remaining.
/// `allow` also permits plaintext, but it offers that method first and reaches
/// this function's TLS failure paths only on its second and final leg.
fn unavailable<S, T>(
    stream: S,
    mode: SslMode,
    permit_plaintext: bool,
    why: &str,
) -> Result<MaybeTlsStream<S, T>, Error> {
    if permit_plaintext {
        Ok(MaybeTlsStream::Raw(stream))
    } else {
        Err(Error::tls(
            format!("{why}, but SSL was required by sslmode={}", mode.as_str()).into(),
        ))
    }
}
