// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Opens a fresh socket on the SAME transport the session negotiated, then
// routes through the shared cancel-packet writer.
//
// It REPLAYS that transport - `SocketConfig::encryption`, recorded by
// `connect.rs` from the stream `negotiate_tls` actually produced - rather than
// re-running the `sslmode` decision. Re-deriving it is wrong in both
// directions, because `connect.rs` has a retry and this has none:
//
//   * `allow` offers plaintext first and re-dials with TLS when the server
//     refuses, so an `hostssl`-only server yields an ENCRYPTED session. A
//     re-derived cancel opens in plaintext and puts the backend PID and secret
//     key - a bearer credential, see `cancel_query_raw` - on the wire in the
//     clear. The postmaster dispatches `CancelRequest` before HBA, so it is
//     accepted and nothing ever reports the downgrade.
//   * `prefer` retries a failed handshake in plaintext, so the session is
//     UNENCRYPTED. A re-derived cancel attempts TLS, hits the same handshake
//     failure, and - having no second leg - cannot cancel that session at all.
//
// Replaying removes both. It still adds no retry: there is nothing to retry
// for, because the transport is no longer a guess.

use crate::cancel_token::CancelKey;
use crate::client::SocketConfig;
use crate::config::{SslCertMode, SslMode, SslNegotiation};
use crate::connect::{tls_server_name, with_connect_timeout};
use crate::encryption::Encryption;
use crate::tls::{MakeTlsConnect, TlsConnect, TlsPolicyIdentity};
use crate::{Error, Socket, cancel_query_raw, connect_socket};
use std::io;

pub(crate) fn validate_cancel_tls_policy<S, T>(
    tls: &T,
    encryption: Encryption,
    ssl_sni: bool,
    ssl_cert_mode: SslCertMode,
    server_verification: crate::tls::ServerVerification,
    expected_policy_identity: Option<&TlsPolicyIdentity>,
) -> Result<(), Error>
where
    T: TlsConnect<S>,
{
    if encryption == Encryption::Plaintext {
        return Ok(());
    }

    if !tls.can_honor_sslsni(ssl_sni) {
        return Err(Error::tls_unattested(
            format!(
                "the TLS connector supplied to cancel does not attest to sslsni={}",
                u8::from(ssl_sni)
            )
            .into(),
        ));
    }
    if !tls.can_honor_sslcertmode(ssl_cert_mode) {
        return Err(Error::tls_unattested(
            format!(
                "the TLS connector supplied to cancel does not attest to sslcertmode={}",
                ssl_cert_mode.as_str()
            )
            .into(),
        ));
    }
    if !tls.can_honor_server_verification(server_verification) {
        return Err(Error::tls_unattested(
            "the TLS connector supplied to cancel does not attest to the server verification \
             this session was established with"
                .into(),
        ));
    }

    let expected_policy_identity = expected_policy_identity.ok_or_else(|| {
        Error::tls_unattested(
            "the original TLS connector did not provide a cancellation policy identity".into(),
        )
    })?;
    let actual_policy_identity = tls.cancel_policy_identity().ok_or_else(|| {
        Error::tls_unattested(
            "the TLS connector supplied to cancel does not provide a cancellation policy identity"
                .into(),
        )
    })?;
    if !expected_policy_identity.same_as(actual_policy_identity) {
        return Err(Error::tls_unattested(
            "the TLS connector supplied to cancel does not match the original session's TLS policy identity"
                .into(),
        ));
    }

    Ok(())
}

fn validate_cancel_tls_connector<T>(
    tls: &T,
    config: &SocketConfig,
    expected_policy_identity: Option<&TlsPolicyIdentity>,
) -> Result<(), Error>
where
    T: TlsConnect<Socket>,
{
    validate_cancel_tls_policy::<Socket, _>(
        tls,
        config.encryption,
        config.ssl_sni,
        config.ssl_cert_mode,
        config.server_verification,
        expected_policy_identity,
    )
}

pub(crate) async fn cancel_query<T>(
    config: Option<SocketConfig>,
    ssl_mode: SslMode,
    ssl_negotiation: SslNegotiation,
    mut tls: T,
    process_id: i32,
    secret_key: CancelKey,
    expected_policy_identity: Option<TlsPolicyIdentity>,
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

    // `with_connect_timeout` fabricates its own error after dropping the timed
    // future, so keep delivery state outside it and attach that state to
    // whichever error emerges.
    let delivery = cancel_query_raw::CancelDeliveryTracker::default();
    let attempt_delivery = delivery.clone();
    with_connect_timeout(config.connect_timeout, async move {
        let encryption = config.encryption;
        let server_name = tls_server_name(&config.addr, config.hostname.as_deref());
        let tls = tls
            .make_tls_connect(&server_name)
            .map_err(|e| Error::tls(e.into()))?;
        // The cancel key is a bearer credential, and its connector comes from
        // the caller at cancel time. Hold that new connection to every TLS
        // disclosure and verification policy the session recorded before the
        // connector can emit a ClientHello.
        validate_cancel_tls_connector(&tls, &config, expected_policy_identity.as_ref())?;
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
            &attempt_delivery,
        )
        .await
    })
    .await
    .map_err(|error| error.with_cancel_delivery(delivery.delivery()))
}

/// Send `CancelRequest` and wait for the postmaster to consume its connection.
///
/// This pool-only form keeps its EOF wait outside `connect_timeout`. Public
/// cancellation uses the same server-close barrier, but its configured timeout
/// covers the complete attempt. Pool recovery instead supplies a separate,
/// whole-recovery grace period around this function and the original session's
/// `ReadyForQuery` barrier.
pub(crate) async fn cancel_query_confirmed<T>(
    config: Option<SocketConfig>,
    ssl_mode: SslMode,
    ssl_negotiation: SslNegotiation,
    mut tls: T,
    process_id: i32,
    secret_key: CancelKey,
    expected_policy_identity: Option<TlsPolicyIdentity>,
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
        let encryption = config.encryption;
        let server_name = tls_server_name(&config.addr, config.hostname.as_deref());
        let tls = tls
            .make_tls_connect(&server_name)
            .map_err(|e| Error::tls(e.into()))?;
        validate_cancel_tls_connector(&tls, &config, expected_policy_identity.as_ref())?;
        let has_hostname = config.hostname.is_some();

        let socket = connect_socket::connect_socket(
            &config.addr,
            config.port,
            config.tcp_user_timeout,
            config.keepalive.as_ref(),
            config.require_peer.as_deref(),
        )
        .await?;

        cancel_query_raw::send_cancel_request_with_exact_encryption(
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
    use crate::NoTls;
    use crate::client::Addr;
    use crate::tls::{ChannelBinding, TlsConnect, TlsPolicyIdentity, TlsStream};
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
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            encryption: Encryption::Plaintext,
            ssl_sni: true,
            ssl_cert_mode: crate::config::SslCertMode::Allow,
            server_verification: crate::tls::ServerVerification::None,
        };
        let cancel = Box::pin(cancel_query_confirmed(
            Some(config),
            SslMode::Disable,
            SslNegotiation::Postgres,
            NoTls,
            PROCESS_ID,
            SECRET_KEY.into(),
            None,
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

    fn ssl_request() -> Vec<u8> {
        let mut packet = bytes::BytesMut::new();
        frontend::ssl_request(&mut packet);
        packet.to_vec()
    }

    /// The `FATAL` an `hostssl`-only `pg_hba.conf` returns to a plaintext
    /// startup: it arrives after the startup packet, so `sslmode=allow` sees it
    /// as a failure of its first leg and re-dials with TLS.
    fn hba_refusal() -> Vec<u8> {
        let mut body = Vec::new();
        for (field, value) in [
            (b'S', "FATAL"),
            (b'V', "FATAL"),
            (b'C', "28000"),
            (b'M', "no pg_hba.conf entry for host, SSL off"),
        ] {
            body.push(field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);

        let mut response = vec![b'E'];
        response.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        response.extend_from_slice(&body);
        response
    }

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

    /// Consume one startup packet (`int32` length, then the body).
    async fn read_startup<S>(stream: &mut S) -> Vec<u8>
    where
        S: AsyncRead + Unpin,
    {
        let length = read_exact(stream, 4).await;
        let length = u32::from_be_bytes(length.try_into().expect("startup packet length"));
        assert!(length >= 8, "startup packet is too short: {length}");
        read_exact(stream, length as usize - 4).await
    }

    /// What the first bytes of a cancel connection were, and the full packet
    /// when the client sent one.
    async fn observe_cancel_connection<S>(stream: &mut S) -> (Vec<u8>, Option<Vec<u8>>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let opening = read_exact(stream, 8).await;
        if opening == ssl_request() {
            write_all(stream, vec![b'S']).await;
            // Not `read_exact`'s panicking helper: a client whose handshake
            // fails sends nothing more and closes, and that is an outcome to
            // report rather than a reason to blow up the scripted peer.
            let compio::BufResult(result, packet) = stream.read_exact(vec![0; 16]).await;
            (opening, result.ok().map(|_| packet))
        } else {
            let mut packet = opening.clone();
            packet.extend_from_slice(&read_exact(stream, 8).await);
            (opening, Some(packet))
        }
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
            assert_eq!(&startup[..4], &[0, 3, 0, 2]);
            write_all(&mut session, startup_response()).await;

            let (mut cancel, _) = listener.accept().await.expect("accept cancel connection");
            assert_eq!(read_exact(&mut cancel, 16).await, cancel_packet());
        });

        let connected = Arc::new(AtomicBool::new(false));
        let tls = PassthroughTls::new(connected.clone());
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
        policy_identity: TlsPolicyIdentity,
    }

    impl PassthroughTls {
        fn new(connected: Arc<AtomicBool>) -> Self {
            Self {
                connected,
                policy_identity: TlsPolicyIdentity::new(),
            }
        }
    }

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

        fn cancel_policy_identity(&self) -> Option<&TlsPolicyIdentity> {
            Some(&self.policy_identity)
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
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            // The only value `require` can record: `connect.rs` never offers
            // it a plaintext leg, and a server refusal is fatal there.
            encryption: Encryption::Tls,
            ssl_sni: true,
            ssl_cert_mode: crate::config::SslCertMode::Allow,
            server_verification: crate::tls::ServerVerification::None,
        };
        let connected = Arc::new(AtomicBool::new(false));
        let tls = PassthroughTls::new(connected.clone());
        let policy_identity = tls.policy_identity.clone();

        compio::time::timeout(
            Duration::from_secs(2),
            cancel_query(
                Some(config),
                SslMode::Require,
                SslNegotiation::Postgres,
                tls,
                PROCESS_ID,
                SECRET_KEY.into(),
                Some(policy_identity),
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

    /// The control for the two tests below: the SAME end-to-end shape, with
    /// `sslmode` the single variable. `require` has one leg, so what
    /// `connect.rs` records and what the mode would have re-derived agree, and
    /// the cancel opens with an `SSLRequest` under either rule. It must stay
    /// green while the `allow` and `prefer` cases move.
    #[compio::test]
    async fn require_records_tls_and_its_cancel_opens_encrypted() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted TLS-only server");
        let addr = listener.local_addr().expect("scripted server address");

        let server = compio::runtime::spawn(async move {
            let (mut session, _) = listener.accept().await.expect("accept session");
            assert_eq!(read_exact(&mut session, 8).await, ssl_request());
            write_all(&mut session, vec![b'S']).await;
            read_startup(&mut session).await;
            write_all(&mut session, startup_response()).await;

            let (mut cancel, _) = listener.accept().await.expect("accept cancel connection");
            let observed = observe_cancel_connection(&mut cancel).await;
            drop(cancel);
            (observed, session)
        });

        let tls = PassthroughTls::new(Arc::new(AtomicBool::new(false)));
        let dsn = format!(
            "host=localhost hostaddr=127.0.0.1 port={} user=postgres sslmode=require",
            addr.port()
        );
        let (client, _connection) =
            compio::time::timeout(Duration::from_secs(2), crate::connect(&dsn, tls.clone()))
                .await
                .expect("require connect timed out")
                .expect("require connect failed");

        compio::time::timeout(
            Duration::from_secs(2),
            client.cancel_token().cancel_query(tls),
        )
        .await
        .expect("require cancel timed out")
        .expect("require cancel failed");

        let ((opening, packet), _session) = compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted TLS-only server timed out")
            .expect("scripted TLS-only server panicked");

        assert_eq!(
            opening,
            ssl_request(),
            "a require session's cancel opened in plaintext"
        );
        assert_eq!(packet, Some(cancel_packet()));
    }

    /// A recorded TLS transport is a decision, not a fresh preference. If the
    /// cancel peer refuses TLS, the bearer PID and secret must not be sent on
    /// that socket in plaintext even when the session's original mode was
    /// `prefer`.
    #[compio::test]
    async fn tls_cancel_refusal_does_not_downgrade_the_bearer_key() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted cancel server");
        let addr = listener.local_addr().expect("scripted server address");
        let server = compio::runtime::spawn(async move {
            let (mut cancel, _) = listener.accept().await.expect("accept cancel connection");
            assert_eq!(read_exact(&mut cancel, 8).await, ssl_request());
            write_all(&mut cancel, vec![b'N']).await;

            let compio::BufResult(result, bytes) = cancel.read(vec![0; 16]).await;
            match result {
                Ok(0) => None,
                Ok(read) => Some(bytes[..read].to_vec()),
                Err(error) => panic!("read after SSL refusal failed: {error}"),
            }
        });

        let config = SocketConfig {
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            encryption: Encryption::Tls,
            ssl_sni: true,
            ssl_cert_mode: crate::config::SslCertMode::Allow,
            server_verification: crate::tls::ServerVerification::None,
        };
        let tls = PassthroughTls::new(Arc::new(AtomicBool::new(false)));
        let policy_identity = tls.policy_identity.clone();
        let result = compio::time::timeout(
            Duration::from_secs(2),
            cancel_query(
                Some(config),
                SslMode::Prefer,
                SslNegotiation::Postgres,
                tls,
                PROCESS_ID,
                SECRET_KEY.into(),
                Some(policy_identity),
            ),
        )
        .await
        .expect("prefer cancel timed out");

        let observed = compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted cancel server timed out")
            .expect("scripted cancel server panicked");
        assert_eq!(
            observed, None,
            "the cancel PID and secret were sent in plaintext after TLS was refused"
        );
        result.expect_err("a recorded TLS cancel must fail when TLS is refused");
    }

    /// A connector that always fails its handshake, which is what `prefer`
    /// retries in plaintext.
    #[derive(Clone)]
    struct FailingTls;

    impl<S> MakeTlsConnect<S> for FailingTls
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        type Stream = PassthroughStream<S>;
        type TlsConnect = FailingTls;
        type Error = std::io::Error;

        fn make_tls_connect(&mut self, _: &str) -> Result<Self::TlsConnect, Self::Error> {
            Ok(self.clone())
        }
    }

    impl<S> TlsConnect<S> for FailingTls
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        type Stream = PassthroughStream<S>;
        type Error = std::io::Error;
        type Future = Pin<Box<dyn Future<Output = Result<PassthroughStream<S>, std::io::Error>>>>;

        fn connect(self, _: S) -> Self::Future {
            Box::pin(async move { Err(std::io::Error::other("scripted TLS handshake failure")) })
        }
    }

    /// `sslmode=allow` offers plaintext first and re-dials with TLS when the
    /// server refuses it, so an `hostssl`-only server yields an ENCRYPTED
    /// session. The cancel key is a bearer credential; re-deriving the cancel
    /// transport from the mode alone sends it in the clear on that session,
    /// and the postmaster - which dispatches `CancelRequest` before HBA -
    /// accepts it, so nothing observable ever reports the downgrade.
    #[compio::test]
    async fn allow_cancel_uses_the_transport_the_session_actually_negotiated() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted hostssl server");
        let addr = listener.local_addr().expect("scripted server address");

        let server = compio::runtime::spawn(async move {
            // Leg 1: the plaintext attempt `allow` makes first, refused the
            // way an `hostssl`-only pg_hba.conf refuses it.
            let (mut plaintext, _) = listener.accept().await.expect("accept plaintext leg");
            read_startup(&mut plaintext).await;
            write_all(&mut plaintext, hba_refusal()).await;
            drop(plaintext);

            // Leg 2: the TLS retry, which is the session that exists.
            let (mut session, _) = listener.accept().await.expect("accept TLS leg");
            assert_eq!(read_exact(&mut session, 8).await, ssl_request());
            write_all(&mut session, vec![b'S']).await;
            read_startup(&mut session).await;
            write_all(&mut session, startup_response()).await;

            let (mut cancel, _) = listener.accept().await.expect("accept cancel connection");
            let observed = observe_cancel_connection(&mut cancel).await;
            drop(cancel);
            (observed, session)
        });

        let tls = PassthroughTls::new(Arc::new(AtomicBool::new(false)));
        let dsn = format!(
            "host=localhost hostaddr=127.0.0.1 port={} user=postgres sslmode=allow",
            addr.port()
        );
        let (client, _connection) =
            compio::time::timeout(Duration::from_secs(2), crate::connect(&dsn, tls.clone()))
                .await
                .expect("allow connect timed out")
                .expect("allow must retry with TLS when plaintext is refused");

        compio::time::timeout(
            Duration::from_secs(2),
            client.cancel_token().cancel_query(tls),
        )
        .await
        .expect("allow cancel timed out")
        .expect("allow cancel failed");

        let ((opening, packet), _session) = compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted hostssl server timed out")
            .expect("scripted hostssl server panicked");

        assert_eq!(
            opening,
            ssl_request(),
            "the session negotiated TLS but its cancel request opened in plaintext, putting the \
             backend PID and secret key on the wire unencrypted"
        );
        assert_eq!(packet, Some(cancel_packet()));
    }

    /// The mirror case. `prefer` retries a failed handshake in plaintext, so
    /// the session is UNENCRYPTED; re-deriving the cancel transport from the
    /// mode makes the cancel attempt TLS, hit the same handshake failure, and
    /// - having no second leg - fail outright. The session cannot be cancelled
    /// at all.
    #[compio::test]
    async fn prefer_cancel_reaches_a_session_that_fell_back_to_plaintext() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted TLS-offering server");
        let addr = listener.local_addr().expect("scripted server address");

        let server = compio::runtime::spawn(async move {
            // Leg 1: `prefer` offers TLS first. Accept the request so the
            // client reaches the handshake its connector then fails.
            let (mut attempted, _) = listener.accept().await.expect("accept TLS leg");
            assert_eq!(read_exact(&mut attempted, 8).await, ssl_request());
            write_all(&mut attempted, vec![b'S']).await;
            drop(attempted);

            // Leg 2: the plaintext retry, which is the session that exists.
            let (mut session, _) = listener.accept().await.expect("accept plaintext leg");
            read_startup(&mut session).await;
            write_all(&mut session, startup_response()).await;

            let (mut cancel, _) = listener.accept().await.expect("accept cancel connection");
            let observed = observe_cancel_connection(&mut cancel).await;
            drop(cancel);
            (observed, session)
        });

        let dsn = format!(
            "host=localhost hostaddr=127.0.0.1 port={} user=postgres sslmode=prefer",
            addr.port()
        );
        let (client, _connection) =
            compio::time::timeout(Duration::from_secs(2), crate::connect(&dsn, FailingTls))
                .await
                .expect("prefer connect timed out")
                .expect("prefer must fall back to plaintext after a handshake failure");

        let cancelled = compio::time::timeout(
            Duration::from_secs(2),
            client.cancel_token().cancel_query(FailingTls),
        )
        .await
        .expect("prefer cancel timed out");

        let ((opening, packet), _session) = compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted TLS-offering server timed out")
            .expect("scripted TLS-offering server panicked");

        cancelled.expect(
            "the session runs in plaintext, but its cancel re-derived TLS from sslmode and died \
             on the same handshake failure the session had already fallen back from",
        );
        assert_ne!(
            opening,
            ssl_request(),
            "the cancel offered TLS to a session that had already fallen back to plaintext"
        );
        assert_eq!(packet, Some(cancel_packet()));
    }

    /// A cancel must not carry the backend PID and secret key over a connector
    /// that attests to less verification than the SESSION was established with.
    ///
    /// Those two values are a bearer credential: anything holding them can
    /// cancel this session's queries. `CancelToken::cancel_query` takes the
    /// connector as an ARGUMENT, so it need not be the one that connected -
    /// nothing stops a caller passing one that verifies nothing, and before
    /// this check nothing did. `connect_raw` gates its own connector this way;
    /// the cancel path, which carries the credential, did not.
    ///
    /// `PassthroughTls` is exactly that connector: it completes a handshake and
    /// attests to `ServerVerification::None` via the trait default.
    #[compio::test]
    async fn a_cancel_refuses_a_connector_that_attests_less_than_the_session() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().expect("scripted server address");

        let config = SocketConfig {
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            encryption: Encryption::Tls,
            ssl_sni: true,
            ssl_cert_mode: crate::config::SslCertMode::Allow,
            // What `sslmode=verify-full` plus a root cert demands.
            server_verification: crate::tls::ServerVerification::ChainAndHostname,
        };

        // Bounded: without the gate this proceeds to dial a scripted server that
        // never answers, so the failure mode of removing it is a HANG. The
        // watchdog turns that into a clean, fast failure.
        let tls = PassthroughTls::new(Arc::new(AtomicBool::new(false)));
        let policy_identity = tls.policy_identity.clone();
        let error = compio::time::timeout(
            Duration::from_secs(5),
            cancel_query(
                Some(config),
                SslMode::VerifyFull,
                SslNegotiation::Postgres,
                tls,
                PROCESS_ID,
                SECRET_KEY.into(),
                Some(policy_identity),
            ),
        )
        .await
        .expect("the gate must refuse immediately, not dial the server")
        .expect_err("a cancel must not send the key through an unattesting connector");
        let chain = std::iter::successors(std::error::Error::source(&error), |e| {
            std::error::Error::source(*e)
        })
        .fold(format!("{error}"), |acc, e| format!("{acc}: {e}"));
        assert!(
            chain.contains("does not attest"),
            "the refusal must name the attestation gap: {chain}"
        );
    }

    async fn assert_cancel_tls_policy_refused(
        ssl_sni: bool,
        ssl_cert_mode: crate::config::SslCertMode,
        expected: &str,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().expect("scripted server address");
        let config = SocketConfig {
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            encryption: Encryption::Tls,
            ssl_sni,
            ssl_cert_mode,
            server_verification: crate::tls::ServerVerification::None,
        };

        let tls = PassthroughTls::new(Arc::new(AtomicBool::new(false)));
        let policy_identity = tls.policy_identity.clone();
        let cancel = Box::pin(cancel_query(
            Some(config),
            SslMode::Require,
            SslNegotiation::Postgres,
            tls,
            PROCESS_ID,
            SECRET_KEY.into(),
            Some(policy_identity),
        ));
        let accept = Box::pin(listener.accept());
        let error = match select(cancel, accept).await {
            Either::Left((result, _)) => {
                result.expect_err("the connector must be refused before a socket is dialed")
            }
            Either::Right(_) => {
                panic!("the cancel dialed before enforcing the session's {expected} policy")
            }
        };
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .fold(format!("{error}"), |chain, error| {
            format!("{chain}: {error}")
        });
        assert!(
            chain.contains(expected),
            "the refusal must name {expected}: {chain}"
        );
    }

    #[compio::test]
    async fn a_cancel_tls_policy_refuses_sni_disclosure() {
        assert_cancel_tls_policy_refused(false, crate::config::SslCertMode::Allow, "sslsni=0")
            .await;
    }

    #[compio::test]
    async fn a_cancel_tls_policy_refuses_client_certificate_disclosure() {
        assert_cancel_tls_policy_refused(
            true,
            crate::config::SslCertMode::Disable,
            "sslcertmode=disable",
        )
        .await;
    }

    #[compio::test]
    async fn a_cancel_refuses_a_different_tls_policy_lineage_before_dialing() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().expect("scripted server address");
        let config = SocketConfig {
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            encryption: Encryption::Tls,
            ssl_sni: true,
            ssl_cert_mode: crate::config::SslCertMode::Allow,
            server_verification: crate::tls::ServerVerification::None,
        };
        let original_policy_identity = TlsPolicyIdentity::new();
        let replacement = PassthroughTls::new(Arc::new(AtomicBool::new(false)));

        let cancel = Box::pin(cancel_query(
            Some(config),
            SslMode::Require,
            SslNegotiation::Postgres,
            replacement,
            PROCESS_ID,
            SECRET_KEY.into(),
            Some(original_policy_identity),
        ));
        let accept = Box::pin(listener.accept());
        let error = match select(cancel, accept).await {
            Either::Left((result, _)) => {
                result.expect_err("a different TLS policy lineage must be refused before dialing")
            }
            Either::Right(_) => panic!("the mismatched TLS policy connector dialed the server"),
        };
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .fold(format!("{error}"), |chain, error| {
            format!("{chain}: {error}")
        });
        assert!(
            chain.contains("TLS policy identity"),
            "the refusal must name the TLS policy identity mismatch: {chain}"
        );
    }

    /// THE CONTROL, one variable: the same connector against a session that
    /// demanded NO verification. It must still be allowed, or the gate would
    /// simply ban `PassthroughTls` and every plain `require` cancel with it.
    #[compio::test]
    async fn a_cancel_tls_policy_allows_matching_defaults() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().expect("scripted server address");
        let server = compio::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept cancel connection");
            assert_eq!(read_exact(&mut stream, 8).await, ssl_request());
            write_all(&mut stream, vec![b'S']).await;
            assert_eq!(read_exact(&mut stream, 16).await, cancel_packet());
        });

        let config = SocketConfig {
            addr: Addr::tcp(addr),
            hostname: Some("localhost".to_string()),
            port: addr.port(),
            connect_timeout: None,
            tcp_user_timeout: None,
            keepalive: None,
            require_peer: None,
            encryption: Encryption::Tls,
            ssl_sni: true,
            ssl_cert_mode: crate::config::SslCertMode::Allow,
            server_verification: crate::tls::ServerVerification::None,
        };

        let tls = PassthroughTls::new(Arc::new(AtomicBool::new(false)));
        let policy_identity = tls.policy_identity.clone();
        compio::time::timeout(
            Duration::from_secs(2),
            cancel_query(
                Some(config),
                SslMode::Require,
                SslNegotiation::Postgres,
                tls,
                PROCESS_ID,
                SECRET_KEY.into(),
                Some(policy_identity),
            ),
        )
        .await
        .expect("cancel timed out")
        .expect("a session that demanded no verification must still be cancellable");

        compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted server timed out")
            .expect("scripted server panicked");
    }
}
