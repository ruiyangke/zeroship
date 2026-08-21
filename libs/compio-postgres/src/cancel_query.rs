// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Opens a fresh socket, picks its transport with the same ADDRESS-AWARE rule
// the connection path uses, then routes through the shared cancel-packet
// writer.
//
// Address-aware is all it shares. It does NOT carry over the connect path's
// RETRY, and that has a consequence worth knowing: a session that ended up in
// plaintext because a `prefer` TLS handshake failed (`connect.rs`, the
// `is_tls_handshake` arm) cannot be cancelled at all. This recomputes TLS,
// hits the same handshake failure, and has no second leg to fall back to.
// Deliberate - the retry belongs to a connection that has a caller waiting on
// it - but it means cancellation is best-effort on exactly those sessions.

use crate::client::SocketConfig;
use crate::config::{SslMode, SslNegotiation};
use crate::connect::{first_encryption_for_addr, with_connect_timeout};
use crate::tls::MakeTlsConnect;
use crate::{Error, Socket, cancel_query_raw, connect_socket};
use std::io;

pub(crate) async fn cancel_query<T>(
    config: Option<SocketConfig>,
    ssl_mode: SslMode,
    ssl_negotiation: SslNegotiation,
    mut tls: T,
    process_id: i32,
    secret_key: i32,
) -> Result<(), Error>
where
    T: MakeTlsConnect<Socket>,
{
    let config = match config {
        Some(config) => config,
        None => {
            return Err(Error::connect(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown host",
            )));
        }
    };

    with_connect_timeout(config.connect_timeout, async move {
        let encryption = first_encryption_for_addr(&config.addr, ssl_mode);
        let tls = tls
            .make_tls_connect(config.hostname.as_deref().unwrap_or(""))
            .map_err(|e| Error::tls(e.into()))?;
        let has_hostname = config.hostname.is_some();

        let socket = connect_socket::connect_socket(
            &config.addr,
            config.port,
            config.tcp_user_timeout,
            config.keepalive.as_ref(),
            config.require_peer.as_deref(),
        )
        .await?;

        cancel_query_raw::cancel_query_with_encryption(
            socket,
            encryption,
            ssl_mode,
            ssl_negotiation,
            tls,
            has_hostname,
            process_id,
            secret_key,
        )
        .await
    })
    .await
}

/// Send `CancelRequest` and wait for the postmaster to consume its connection.
///
/// The public cancellation API intentionally remains fire-and-forget. Pool
/// timeout recovery needs the stronger server-close barrier so a late cancel
/// cannot race with reuse of the original backend.
pub(crate) async fn cancel_query_confirmed<T>(
    config: Option<SocketConfig>,
    ssl_mode: SslMode,
    ssl_negotiation: SslNegotiation,
    mut tls: T,
    process_id: i32,
    secret_key: i32,
) -> Result<(), Error>
where
    T: MakeTlsConnect<Socket>,
{
    let config = match config {
        Some(config) => config,
        None => {
            return Err(Error::connect(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown host",
            )));
        }
    };

    let stream = with_connect_timeout(config.connect_timeout, async move {
        let encryption = first_encryption_for_addr(&config.addr, ssl_mode);
        let tls = tls
            .make_tls_connect(config.hostname.as_deref().unwrap_or(""))
            .map_err(|e| Error::tls(e.into()))?;
        let has_hostname = config.hostname.is_some();

        let socket = connect_socket::connect_socket(
            &config.addr,
            config.port,
            config.tcp_user_timeout,
            config.keepalive.as_ref(),
            config.require_peer.as_deref(),
        )
        .await?;

        cancel_query_raw::send_cancel_request_with_encryption(
            socket,
            encryption,
            ssl_mode,
            ssl_negotiation,
            tls,
            has_hostname,
            process_id,
            secret_key,
        )
        .await
    })
    .await?;

    // Deliberately outside `with_connect_timeout`: waiting for postmaster EOF
    // is timeout-recovery synchronization, not connection setup and not a
    // socket read deadline. The caller bounds the whole recovery operation.
    cancel_query_raw::wait_for_server_close(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Addr;
    use crate::tls::{ChannelBinding, TlsConnect, TlsStream};
    use crate::NoTls;
    use compio::buf::{IoBuf, IoBufMut};
    use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use compio::net::TcpListener;
    #[cfg(unix)]
    use compio::net::UnixListener;
    use futures_util::future::{Either, select};
    use postgres_protocol::message::frontend;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const PROCESS_ID: i32 = 1234;
    const SECRET_KEY: i32 = 5678;

    fn cancel_packet() -> Vec<u8> {
        let mut packet = bytes::BytesMut::new();
        frontend::cancel_request(PROCESS_ID, SECRET_KEY, &mut packet);
        packet.to_vec()
    }

    #[compio::test]
    async fn confirmed_cancel_waits_for_postmaster_connection_close() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted cancel server");
        let addr = listener.local_addr().expect("scripted server address");
        let (packet_seen_tx, packet_seen_rx) = futures_channel::oneshot::channel();
        let (allow_close_tx, allow_close_rx) = futures_channel::oneshot::channel();

        let server = compio::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept cancel connection");
            assert_eq!(read_exact(&mut stream, 16).await, cancel_packet());
            packet_seen_tx.send(()).expect("report cancel packet");
            allow_close_rx.await.expect("allow cancel connection close");
            drop(stream);
        });

        let config = SocketConfig {
            addr: Addr::Tcp(addr.ip()),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
        };
        let cancel = Box::pin(cancel_query_confirmed(
            Some(config),
            SslMode::Disable,
            SslNegotiation::Postgres,
            NoTls,
            PROCESS_ID,
            SECRET_KEY,
        ));
        let packet_seen = Box::pin(packet_seen_rx);
        let cancel = match select(cancel, packet_seen).await {
            Either::Left((result, _)) => {
                panic!(
                    "confirmed cancellation returned before server EOF: {:?}",
                    result
                )
            }
            Either::Right((packet_seen, cancel)) => {
                packet_seen.expect("server did not read cancel packet");
                cancel
            }
        };

        allow_close_tx
            .send(())
            .expect("release scripted cancel connection");
        compio::time::timeout(Duration::from_secs(2), cancel)
            .await
            .expect("confirmed cancellation did not observe server EOF")
            .expect("confirmed cancellation failed");
        compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted cancel server timed out")
            .expect("scripted cancel server panicked");
    }

    #[cfg(unix)]
    fn startup_response() -> Vec<u8> {
        let mut response = Vec::new();

        response.push(b'R');
        response.extend_from_slice(&8u32.to_be_bytes());
        response.extend_from_slice(&0i32.to_be_bytes());

        response.push(b'K');
        response.extend_from_slice(&12u32.to_be_bytes());
        response.extend_from_slice(&PROCESS_ID.to_be_bytes());
        response.extend_from_slice(&SECRET_KEY.to_be_bytes());

        response.push(b'Z');
        response.extend_from_slice(&5u32.to_be_bytes());
        response.push(b'I');

        response
    }

    async fn read_exact<S>(stream: &mut S, len: usize) -> Vec<u8>
    where
        S: AsyncRead + Unpin,
    {
        let compio::BufResult(result, bytes) = stream.read_exact(vec![0; len]).await;
        result.expect("scripted server read");
        bytes
    }

    async fn write_all<S>(stream: &mut S, bytes: Vec<u8>)
    where
        S: AsyncWrite + Unpin,
    {
        let compio::BufResult(result, _) = stream.write_all(bytes).await;
        result.expect("scripted server write");
        stream.flush().await.expect("scripted server flush");
    }

    #[cfg(unix)]
    struct TempSocketDir(std::path::PathBuf);

    #[cfg(unix)]
    impl TempSocketDir {
        fn create() -> TempSocketDir {
            static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

            // `/tmp` literally, NOT `std::env::temp_dir()`. Linux caps a
            // `sockaddr_un` path at 108 bytes including the `.s.PGSQL.5432`
            // suffix, and `temp_dir()` returns `$TMPDIR` when it is set. A long
            // `TMPDIR` - this repo's own agent scratchpad root is 79 characters
            // - overruns that and fails `bind` with ENAMETOOLONG on a machine
            // where nothing is actually wrong. The short name below keeps the
            // whole path well inside the limit.
            let path = std::path::PathBuf::from("/tmp").join(format!(
                "cpg-cancel-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create temporary socket directory");
            TempSocketDir(path)
        }
    }

    #[cfg(unix)]
    impl Drop for TempSocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[compio::test]
    async fn unix_socket_cancel_uses_plaintext_when_sslmode_requires_tls() {
        let socket_dir = TempSocketDir::create();
        let socket_path = socket_dir.0.join(".s.PGSQL.5432");
        let listener = UnixListener::bind(&socket_path)
            .await
            .expect("bind scripted Unix server");
        let server = compio::runtime::spawn(async move {
            let (mut session, _) = listener.accept().await.expect("accept session connection");
            let length = read_exact(&mut session, 4).await;
            let length = u32::from_be_bytes(length.try_into().expect("startup packet length"));
            assert!(length >= 8, "startup packet is too short: {length}");
            let startup = read_exact(&mut session, length as usize - 4).await;
            assert_eq!(&startup[..4], &[0, 3, 0, 0]);
            write_all(&mut session, startup_response()).await;

            let (mut cancel, _) = listener.accept().await.expect("accept cancel connection");
            assert_eq!(read_exact(&mut cancel, 16).await, cancel_packet());
        });

        let connected = Arc::new(AtomicBool::new(false));
        let tls = PassthroughTls {
            connected: connected.clone(),
        };
        let dsn = format!(
            "host={} port=5432 user=postgres sslmode=require",
            socket_dir.0.display()
        );
        let (client, _connection) =
            compio::time::timeout(Duration::from_secs(2), crate::connect(&dsn, tls.clone()))
                .await
                .expect("Unix connect timed out")
                .expect("Unix connect must ignore sslmode and use plaintext");

        compio::time::timeout(
            Duration::from_secs(2),
            client.cancel_token().cancel_query(tls),
        )
        .await
        .expect("Unix cancel timed out")
        .expect("Unix cancel must ignore sslmode and use plaintext");

        compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted Unix server timed out")
            .expect("scripted Unix server task panicked");
        assert!(
            !connected.load(Ordering::Relaxed),
            "Unix cancel attempted a TLS handshake"
        );
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

    impl<S> TlsStream for PassthroughStream<S>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        fn channel_binding(&self) -> ChannelBinding {
            ChannelBinding::none()
        }
    }

    impl<S> MakeTlsConnect<S> for PassthroughTls
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        type Stream = PassthroughStream<S>;
        type TlsConnect = PassthroughTls;
        type Error = std::io::Error;

        fn make_tls_connect(&mut self, _: &str) -> Result<Self::TlsConnect, Self::Error> {
            Ok(self.clone())
        }
    }

    impl<S> TlsConnect<S> for PassthroughTls
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        type Stream = PassthroughStream<S>;
        type Error = std::io::Error;
        type Future = Pin<Box<dyn Future<Output = Result<PassthroughStream<S>, std::io::Error>>>>;

        fn connect(self, stream: S) -> Self::Future {
            self.connected.store(true, Ordering::Relaxed);
            Box::pin(async move { Ok(PassthroughStream(stream)) })
        }
    }

    #[compio::test]
    async fn tcp_require_cancel_negotiates_tls_before_sending_request() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted TCP server");
        let addr = listener.local_addr().expect("scripted server address");
        let server = compio::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept cancel connection");

            let mut ssl_request = bytes::BytesMut::new();
            frontend::ssl_request(&mut ssl_request);
            assert_eq!(read_exact(&mut stream, 8).await, ssl_request.to_vec());
            write_all(&mut stream, vec![b'S']).await;

            assert_eq!(read_exact(&mut stream, 16).await, cancel_packet());
        });

        let config = SocketConfig {
            addr: Addr::Tcp(addr.ip()),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
        };
        let connected = Arc::new(AtomicBool::new(false));

        compio::time::timeout(
            Duration::from_secs(2),
            cancel_query(
                Some(config),
                SslMode::Require,
                SslNegotiation::Postgres,
                PassthroughTls {
                    connected: connected.clone(),
                },
                PROCESS_ID,
                SECRET_KEY,
            ),
        )
        .await
        .expect("TCP cancel timed out")
        .expect("TCP require cancel must negotiate TLS");

        compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted TCP server timed out")
            .expect("scripted TCP server task panicked");
        assert!(
            connected.load(Ordering::Relaxed),
            "TCP require cancel did not use the TLS connector"
        );
    }
}
