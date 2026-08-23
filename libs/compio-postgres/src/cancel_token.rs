// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::config::{SslMode, SslNegotiation};
use crate::tls::TlsConnect;
use crate::{Error, Socket, cancel_query, cancel_query_raw, client::SocketConfig, tls::MakeTlsConnect};
use compio::io::{AsyncRead, AsyncWrite};

/// The capability to request cancellation of in-progress queries on a
/// connection.
#[derive(Clone)]
pub struct CancelToken {
    pub(crate) socket_config: Option<SocketConfig>,
    pub(crate) ssl_mode: SslMode,
    pub(crate) ssl_negotiation: SslNegotiation,
    pub(crate) process_id: i32,
    pub(crate) secret_key: i32,
}

impl CancelToken {
    /// Attempts to cancel the in-progress query on the connection associated
    /// with this `CancelToken`.
    ///
    /// The server provides no information about whether a cancellation attempt was successful or not. An error will
    /// only be returned if the client was unable to connect to the database.
    ///
    /// Cancellation is inherently racy. There is no guarantee that the
    /// cancellation request will reach the server before the query terminates
    /// normally, or that the connection associated with this token is still
    /// active.
    pub async fn cancel_query<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        cancel_query::cancel_query(
            self.socket_config.clone(),
            self.ssl_mode,
            self.ssl_negotiation,
            tls,
            self.process_id,
            self.secret_key,
        )
        .await
    }

    /// Send cancellation and wait until the postmaster closes its dedicated
    /// connection after consuming the packet.
    ///
    /// Pool timeout recovery uses this stronger internal primitive before it
    /// allows the original backend to be reused. The public method preserves
    /// its established fire-and-forget contract.
    pub(crate) async fn cancel_query_confirmed<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        cancel_query::cancel_query_confirmed(
            self.socket_config.clone(),
            self.ssl_mode,
            self.ssl_negotiation,
            tls,
            self.process_id,
            self.secret_key,
        )
        .await
    }

    /// Like `cancel_query`, but uses a stream which is already connected to the server rather than opening a new
    /// connection itself.
    pub async fn cancel_query_raw<S, T>(&self, stream: S, tls: T) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        cancel_query_raw::cancel_query_raw(
            stream,
            self.ssl_mode,
            self.ssl_negotiation,
            tls,
            // `true`, deliberately, and NOT derived from `self.socket_config`.
            //
            // Here `has_hostname` asks whether the CONNECTOR the caller just
            // handed us has a name to validate against. Only the caller knows:
            // this method takes their stream and their `TlsConnect`, and
            // nothing says either has to match the address the original
            // session used. `true` means "attempt validation", which fails
            // closed - a connector with no name refuses the handshake.
            //
            // Deriving it from the token was tried and reverted. It reads as
            // more precise and is a downgrade: `Config::connect_raw` leaves
            // `socket_config` as `None` forever (`config.rs`), so every
            // caller-owned-stream client got `false`, and under the default
            // `sslmode=prefer` that skips TLS and puts the cancel key - a
            // bearer credential, see `cancel_query_raw` - on the wire in
            // cleartext. It did not even fix the Unix case it was written for:
            // `cancel_query_raw` still picks `Encryption::first_for(mode)`, so
            // Unix plus `require` fails either way, just with a different
            // message.
            //
            // Upstream tokio-postgres passes `true` here too.
            true,
            self.process_id,
            self.secret_key,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoTls;
    use crate::client::Addr;
    use crate::tls::{ChannelBinding, TlsStream};
    use compio::buf::{IoBuf, IoBufMut};
    use compio::io::{AsyncReadExt, AsyncWriteExt};
    use compio::net::{TcpListener, TcpStream};
    use futures_util::future::{Either, select};
    use postgres_protocol::message::frontend;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const PROCESS_ID: i32 = 1234;
    const SECRET_KEY: i32 = 5678;
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn cancel_packet() -> Vec<u8> {
        let mut packet = bytes::BytesMut::new();
        frontend::cancel_request(PROCESS_ID, SECRET_KEY, &mut packet);
        packet.to_vec()
    }

    fn ssl_request() -> Vec<u8> {
        let mut packet = bytes::BytesMut::new();
        frontend::ssl_request(&mut packet);
        packet.to_vec()
    }

    async fn read_exact(stream: &mut TcpStream, len: usize) -> Vec<u8> {
        let compio::BufResult(result, bytes) = stream.read_exact(vec![0; len]).await;
        result.expect("read scripted cancel peer bytes");
        bytes
    }

    async fn write_all(stream: &mut TcpStream, bytes: Vec<u8>) {
        let compio::BufResult(result, _) = stream.write_all(bytes).await;
        result.expect("write scripted cancel peer bytes");
        stream.flush().await.expect("flush scripted cancel peer");
    }

    fn network_token(addr: std::net::SocketAddr) -> CancelToken {
        CancelToken {
            socket_config: Some(SocketConfig {
                addr: Addr::Tcp(addr.ip()),
                hostname: Some("localhost".to_string()),
                port: addr.port(),
                connect_timeout: None,
                tcp_user_timeout: None,
                keepalive: None,
                require_peer: None,
                encryption: crate::connect_tls::Encryption::Plaintext,
            }),
            ssl_mode: SslMode::Disable,
            ssl_negotiation: SslNegotiation::Postgres,
            process_id: PROCESS_ID,
            secret_key: SECRET_KEY,
        }
    }

    #[compio::test]
    async fn public_cancel_is_fire_and_forget_but_confirmed_cancel_waits_for_eof() {
        compio::time::timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind scripted cancel peer");
            let addr = listener.local_addr().expect("scripted cancel peer address");
            let token = network_token(addr);

            let (public_seen_tx, public_seen_rx) = futures_channel::oneshot::channel();
            let (release_public_tx, release_public_rx) = futures_channel::oneshot::channel();
            let (confirmed_seen_tx, confirmed_seen_rx) = futures_channel::oneshot::channel();
            let (release_confirmed_tx, release_confirmed_rx) =
                futures_channel::oneshot::channel();

            let peer = compio::runtime::spawn(async move {
                let (mut public, _) = listener.accept().await.expect("accept public cancel");
                assert_eq!(read_exact(&mut public, 16).await, cancel_packet());
                public_seen_tx.send(()).expect("report public cancel packet");
                release_public_rx.await.expect("release public cancel peer");
                drop(public);

                let (mut confirmed, _) = listener.accept().await.expect("accept confirmed cancel");
                assert_eq!(read_exact(&mut confirmed, 16).await, cancel_packet());
                confirmed_seen_tx
                    .send(())
                    .expect("report confirmed cancel packet");
                release_confirmed_rx
                    .await
                    .expect("release confirmed cancel peer");
                drop(confirmed);
            });

            compio::time::timeout(Duration::from_millis(500), token.cancel_query(NoTls))
                .await
                .expect(
                    "public cancellation waited for peer EOF instead of returning after its \
                     flushed packet",
                )
                .expect("public cancellation failed");
            compio::time::timeout(Duration::from_millis(500), public_seen_rx)
                .await
                .expect("scripted peer did not receive public cancel packet")
                .expect("scripted peer dropped public packet report");
            release_public_tx
                .send(())
                .expect("release public cancel connection");

            let confirmed = Box::pin(token.cancel_query_confirmed(NoTls));
            let confirmed_seen = Box::pin(confirmed_seen_rx);
            let mut confirmed = match select(confirmed, confirmed_seen).await {
                Either::Left((result, _)) => {
                    panic!("confirmed cancellation returned before peer EOF: {result:?}")
                }
                Either::Right((seen, confirmed)) => {
                    seen.expect("scripted peer dropped confirmed packet report");
                    confirmed
                }
            };
            assert!(
                compio::time::timeout(Duration::from_millis(100), confirmed.as_mut())
                    .await
                    .is_err(),
                "confirmed cancellation returned before the postmaster-style peer closed"
            );

            release_confirmed_tx
                .send(())
                .expect("release confirmed cancel connection");
            compio::time::timeout(Duration::from_millis(500), confirmed)
                .await
                .expect("confirmed cancellation did not finish after peer EOF")
                .expect("confirmed cancellation failed after peer EOF");
            compio::time::timeout(Duration::from_millis(500), peer)
                .await
                .expect("scripted cancel peer timed out")
                .expect("scripted cancel peer panicked");
        })
        .await
        .expect("cancel completion-contract test exceeded its 5 second deadline");
    }

    #[derive(Clone)]
    struct PassthroughTls {
        connected: Arc<AtomicBool>,
    }

    struct PassthroughStream<S>(S);

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
        type Future = Pin<Box<dyn Future<Output = Result<Self::Stream, Self::Error>>>>;

        fn connect(self, stream: S) -> Self::Future {
            self.connected.store(true, Ordering::Relaxed);
            Box::pin(async move { Ok(PassthroughStream(stream)) })
        }
    }

    #[compio::test]
    async fn raw_cancel_uses_the_callers_tls_identity_when_the_token_has_no_socket_config() {
        compio::time::timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind scripted raw cancel peer");
            let addr = listener
                .local_addr()
                .expect("scripted raw cancel peer address");

            let peer = compio::runtime::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept raw cancel");
                let first = read_exact(&mut stream, 8).await;
                let packet = if first == ssl_request() {
                    write_all(&mut stream, vec![b'S']).await;
                    read_exact(&mut stream, 16).await
                } else {
                    let mut packet = first.clone();
                    packet.extend_from_slice(&read_exact(&mut stream, 8).await);
                    packet
                };
                (first, packet)
            });

            let token = CancelToken {
                socket_config: None,
                ssl_mode: SslMode::Prefer,
                ssl_negotiation: SslNegotiation::Postgres,
                process_id: PROCESS_ID,
                secret_key: SECRET_KEY,
            };
            let stream = TcpStream::connect(addr)
                .await
                .expect("connect caller-owned cancel stream");
            let connected = Arc::new(AtomicBool::new(false));

            token
                .cancel_query_raw(
                    stream,
                    PassthroughTls {
                        connected: Arc::clone(&connected),
                    },
                )
                .await
                .expect("send raw cancellation through caller TLS connector");
            let (first, packet) = compio::time::timeout(Duration::from_millis(500), peer)
                .await
                .expect("scripted raw cancel peer timed out")
                .expect("scripted raw cancel peer panicked");

            assert_eq!(
                first,
                ssl_request(),
                "raw cancellation derived hostname availability from an address-less token and \
                 sent the cancel credential without attempting caller-provided TLS"
            );
            assert_eq!(packet, cancel_packet());
            assert!(
                connected.load(Ordering::Relaxed),
                "raw cancellation did not invoke the caller's TLS connector"
            );
        })
        .await
        .expect("raw cancel TLS-identity test exceeded its 5 second deadline");
    }
}
