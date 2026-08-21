// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Host resolution + failover. Walks the config.host / config.hostaddr
// list, calls connect_socket -> connect_tls -> connect_raw for each
// host until one succeeds. Returns (Client, Connection).
//
// Differences from the tokio source:
//
// * DNS resolution uses compio's `ToSocketAddrsAsync` — we call
//   `(host, port).to_socket_addrs_async()` instead of
//   `tokio::net::lookup_host`.
// * Host failover is sequential (`for host in hosts`). The source uses
//   `FuturesOrdered` for parallel attempts when `hosts > 1`; that's a
//   nice-to-have future optimisation and documented as a hand-off in
//   PHASE3.md.
// * `target_session_attrs=ReadWrite`/`ReadOnly` post-handshake probe
//   (the `SHOW transaction_read_only` query) is still deferred: it
//   requires `Client::simple_query_raw` from the full query surface.
//   We return an error if a non-default value is set, rather than
//   silently ignoring it.

use crate::client::{Addr, Client, SocketConfig};
use crate::config::{Host, LoadBalanceHosts, SslMode, TargetSessionAttrs};
use crate::connect_raw::connect_raw;
use crate::connect_socket::connect_socket;
use crate::connect_tls::Encryption;
use crate::connection::Connection;
use crate::tls::MakeTlsConnect;
use crate::{Config, Error, Socket};
use compio::net::ToSocketAddrsAsync;
use rand::seq::SliceRandom;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;
use std::{cmp, io};

/// One configured host entry, separated into the address to reach and the
/// optional name TLS validates.
pub(crate) struct Endpoint {
    target: EndpointTarget,
    hostname: Option<String>,
    port: u16,
}

enum EndpointTarget {
    Name(String),
    Ip(IpAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

/// The name-resolution seam used by the endpoint walk.
pub(crate) trait Resolver {
    async fn resolve(&mut self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

pub(crate) struct SystemResolver;

impl Resolver for SystemResolver {
    async fn resolve(&mut self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok((host, port)
            .to_socket_addrs_async()
            .await?
            .collect::<Vec<_>>())
    }
}

impl Endpoint {
    pub(crate) fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Resolve this host entry to the addresses it denotes. Numeric
    /// `hostaddr` values and Unix sockets bypass name resolution entirely.
    pub(crate) async fn addresses<R>(
        &self,
        resolver: &mut R,
        load_balance_hosts: LoadBalanceHosts,
    ) -> Result<Vec<Addr>, Error>
    where
        R: Resolver,
    {
        match &self.target {
            EndpointTarget::Name(host) => {
                let mut addrs = resolver
                    .resolve(host, self.port)
                    .await
                    .map_err(Error::connect)?;

                if load_balance_hosts == LoadBalanceHosts::Random {
                    addrs.shuffle(&mut rand::rng());
                }

                if addrs.is_empty() {
                    return Err(Error::connect(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "could not resolve any addresses",
                    )));
                }

                Ok(addrs.into_iter().map(|addr| Addr::Tcp(addr.ip())).collect())
            }
            EndpointTarget::Ip(ip) => Ok(vec![Addr::Tcp(*ip)]),
            #[cfg(unix)]
            EndpointTarget::Unix(path) => Ok(vec![Addr::Unix(path.clone())]),
        }
    }
}

/// Validate and enumerate the configured host entries once for every
/// connection path.
pub(crate) fn endpoints(config: &Config) -> Result<Vec<Endpoint>, Error> {
    config.validate_tls_settings()?;

    if config.get_hosts().is_empty() && config.get_hostaddrs().is_empty() {
        return Err(Error::config("both host and hostaddr are missing".into()));
    }

    if !config.get_hosts().is_empty()
        && !config.get_hostaddrs().is_empty()
        && config.get_hosts().len() != config.get_hostaddrs().len()
    {
        let msg = format!(
            "number of hosts ({}) is different from number of hostaddrs ({})",
            config.get_hosts().len(),
            config.get_hostaddrs().len(),
        );
        return Err(Error::config(msg.into()));
    }

    let num_hosts = cmp::max(config.get_hosts().len(), config.get_hostaddrs().len());
    if config.get_ports().len() > 1 && config.get_ports().len() != num_hosts {
        return Err(Error::config("invalid number of ports".into()));
    }

    let mut indices = (0..num_hosts).collect::<Vec<_>>();
    if config.get_load_balance_hosts() == LoadBalanceHosts::Random {
        indices.shuffle(&mut rand::rng());
    }

    Ok(indices
        .into_iter()
        .map(|i| {
            let host = config.get_hosts().get(i);
            let hostname = match host {
                Some(Host::Tcp(host)) => Some(host.clone()),
                #[cfg(unix)]
                Some(Host::Unix(_)) => None,
                None => None,
            };
            let target = match config.get_hostaddrs().get(i) {
                Some(ip) => EndpointTarget::Ip(*ip),
                None => match host.expect("one of host / hostaddr is present at this index") {
                    Host::Tcp(host) => EndpointTarget::Name(host.clone()),
                    #[cfg(unix)]
                    Host::Unix(path) => EndpointTarget::Unix(path.clone()),
                },
            };
            let port = config
                .get_ports()
                .get(i)
                .or_else(|| config.get_ports().first())
                .copied()
                .unwrap_or(5432);

            Endpoint {
                target,
                hostname,
                port,
            }
        })
        .collect())
}

pub(crate) async fn with_connect_timeout<T, F>(
    timeout: Option<Duration>,
    connect: F,
) -> Result<T, Error>
where
    F: Future<Output = Result<T, Error>>,
{
    match timeout {
        Some(timeout) => match compio::time::timeout(timeout, connect).await {
            Ok(result) => result,
            Err(_) => Err(Error::connect(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            ))),
        },
        None => connect.await,
    }
}

pub async fn connect<T>(
    tls: T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    let mut resolver = SystemResolver;
    connect_with_resolver(tls, config, &mut resolver).await
}

async fn connect_with_resolver<T, R>(
    mut tls: T,
    config: &Config,
    resolver: &mut R,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
    R: Resolver,
{
    let endpoints = endpoints(config)?;

    let mut error = None;
    for endpoint in endpoints {
        match connect_host(&endpoint, resolver, &mut tls, config).await {
            Ok((client, connection)) => return Ok((client, connection)),
            Err(e) => error = Some(e),
        }
    }

    Err(error.expect("endpoints rejects an empty host list"))
}

/// One configured host entry: resolve it, then try each address it denotes
/// until one connects.
///
/// `connect_timeout` is applied once to resolution and then AFRESH to each
/// address, so a host entry's total budget scales with the addresses it names.
async fn connect_host<T, R>(
    endpoint: &Endpoint,
    resolver: &mut R,
    tls: &mut T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
    R: Resolver,
{
    let timeout = config.get_connect_timeout().copied();

    // libpq leaves name resolution OUTSIDE the deadline: `pg_getaddrinfo_all`
    // blocks inside `PQconnectPoll`, and `connectDBComplete`'s `finish_time`
    // only governs the socket waits around it. We diverge deliberately and
    // give resolution its own budget, because an unresponsive resolver
    // otherwise hangs a connect that asked for a time limit.
    //
    // This bounds THE CALLER'S WAIT, not the resolver's work. compio resolves
    // via `spawn_blocking` around the blocking `to_socket_addrs`
    // (compio-net-0.11.1 src/resolve/unix.rs), so timing out here drops our
    // future but cannot cancel an in-flight `getaddrinfo`; that thread stays
    // occupied until the C resolver returns on its own. Repeated timeouts
    // against a black-holed nameserver therefore tie up blocking-pool threads.
    // Returning control to the caller is still the right trade, but do not
    // read this budget as a bound on resources.
    let addrs = with_connect_timeout(
        timeout,
        endpoint.addresses(resolver, config.get_load_balance_hosts()),
    )
    .await?;

    let mut last_err = None;
    for addr in addrs {
        // The deadline restarts for EVERY address, matching libpq
        // (`fe-connect.c`, `connectDBComplete`), which recomputes
        // `finish_time` whenever `whichhost` OR `whichaddr` moves. One budget
        // shared across the walk would let the first blackholed A record
        // consume it all and strand the healthy addresses behind it.
        match with_connect_timeout(
            timeout,
            connect_once(addr, endpoint.hostname(), endpoint.port(), tls, config),
        )
        .await
        {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("Endpoint::addresses rejects an empty address list"))
}

/// The first transport to use for a newly opened socket.
///
/// Unix-domain sockets have no TLS transport regardless of `sslmode`. TCP
/// sockets use the mode's normal ordering and may be retried by their caller.
pub(crate) fn first_encryption_for_addr(addr: &Addr, mode: SslMode) -> Encryption {
    match addr {
        Addr::Tcp(_) => Encryption::first_for(mode),
        #[cfg(unix)]
        Addr::Unix(_) => Encryption::Plaintext,
    }
}

/// One address, with libpq's transport ordering and its reconnect.
///
/// Everything about `allow` and `prefer` that is not just "try TLS" lives here,
/// because the fallback is not a branch inside an attempt - it is a *second
/// attempt on a new socket*. A handshake that fails after the server answered
/// `S` has consumed the stream: TLS records were exchanged on it and there is
/// no way back to a clean startup packet. libpq's answer is
/// `need_new_connection = true`, which drops the socket and re-enters address
/// resolution; ours is calling [`connect_leg`] again, which opens a new one.
///
/// The two orderings, and what each retries:
///
/// * `prefer` - TLS, then plaintext, **retrying only a handshake failure**. A
///   startup or authentication failure is final: retrying a rejected password
///   in the clear would put it on the wire unencrypted, which is a worse
///   outcome than the error.
/// * `allow` - plaintext, then TLS, retrying any failure of the first leg.
///   This is the `hostssl`-only server: the plaintext attempt is refused
///   during startup and the TLS retry is the one that connects. Retrying in
///   the *stronger* direction carries none of the hazard above.
///
/// A server that answers `N` to `SSLRequest` needs no reconnect at all and
/// does not get one; that case is handled inside `negotiate_tls` on the
/// original socket.
async fn connect_once<T>(
    addr: Addr,
    hostname: Option<&str>,
    port: u16,
    tls: &mut T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    let first = first_encryption_for_addr(&addr, config.get_ssl_mode());

    // libpq: "sslmode is ignored for Unix domain socket communication."
    // A local socket has no network to eavesdrop on and no host name to put in
    // a certificate, so every mode - including verify-full - is plaintext.
    #[cfg(unix)]
    if matches!(addr, Addr::Unix(_)) {
        return connect_leg(&addr, hostname, port, tls, config, first).await;
    }

    let err = match connect_leg(&addr, hostname, port, tls, config, first).await {
        Ok(connected) => return Ok(connected),
        Err(e) => e,
    };

    let retry = match (config.get_ssl_mode(), first) {
        // TLS first, and the handshake is what failed: dial again, in the clear.
        //
        // KNOWN SHARP EDGE, and it is libpq's. "The handshake failed" includes
        // "the certificate did not verify": libpq's retry site is the single
        // `pollres == PGRES_POLLING_FAILED` arm after `pqsecure_open_client`,
        // which reports one status for a protocol failure and a verification
        // failure alike, and rustls gives us one error for both too. So
        // `sslmode=prefer sslrootcert=<ca>` against a server presenting a bad
        // certificate does not fail - it silently downgrades to plaintext.
        //
        // We reproduce it because this is a driver and `prefer` is libpq's
        // word, not ours. The mode's own documentation says it "makes no sense
        // from a security point of view"; a deployment that cares names a
        // stronger mode, and `verify-ca`/`verify-full` cannot reach this arm at
        // all. The alternative - refusing to downgrade once trust anchors were
        // named, on the grounds that naming them is a statement of intent - is
        // a deliberate divergence from libpq rather than a bug fix, and if it
        // is ever wanted it belongs here, in this one arm.
        (SslMode::Prefer, Encryption::Tls) if err.is_tls_handshake() => Encryption::Plaintext,
        // Plaintext first, and it failed for any reason: dial again, with TLS.
        (SslMode::Allow, Encryption::Plaintext) => Encryption::Tls,
        // Every other mode has one transport in its allowed set, so there is
        // nothing to fall back to. This is libpq's structural guarantee, not a
        // check: `require`/`verify-ca`/`verify-full` never reach this arm with
        // a second option.
        _ => return Err(err),
    };

    connect_leg(&addr, hostname, port, tls, config, retry).await
}

/// One attempt: a fresh socket, one transport, one startup exchange.
async fn connect_leg<T>(
    addr: &Addr,
    hostname: Option<&str>,
    port: u16,
    tls: &mut T,
    config: &Config,
    encryption: Encryption,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    let addr = addr.clone();
    let socket = connect_socket(
        &addr,
        port,
        config.get_tcp_user_timeout().copied(),
        if config.get_keepalives() {
            Some(&config.keepalive_config)
        } else {
            None
        },
    )
    .await?;

    let tls = tls
        .make_tls_connect(hostname.unwrap_or(""))
        .map_err(|e| Error::tls(e.into()))?;
    let has_hostname = hostname.is_some();
    // Taken while the socket is still a `Socket` - `connect_raw` is generic
    // over the stream and the TLS wrapper hides the descriptor.
    let release = socket.release_handle();
    let (mut client, connection) =
        connect_raw(socket, tls, encryption, has_hostname, config, release).await?;

    // TargetSessionAttrs post-connect probe. The source interleaves a
    // `simple_query_raw("SHOW transaction_read_only")` with
    // `connection.poll_unpin` — fail the probe if the connection dies.
    // Our `Connection::run` consumes `self`, so the in-place interleave
    // of the source cannot be expressed directly. Implementing this
    // properly requires either: (a) a `poll_one_step` method on
    // Connection; or (b) spawning the connection and re-joining it
    // after the probe. For now we surface the limitation rather than
    // silently ignoring a non-default value. This should be revisited
    // alongside the transaction port.
    if config.get_target_session_attrs() != TargetSessionAttrs::Any {
        return Err(Error::config(
            "target_session_attrs is not yet supported; the post-connect probe is still missing"
                .into(),
        ));
    }

    client.set_socket_config(SocketConfig {
        addr,
        hostname: hostname.map(|s| s.to_string()),
        port,
        connect_timeout: config.get_connect_timeout().copied(),
        tcp_user_timeout: config.get_tcp_user_timeout().copied(),
        keepalive: if config.get_keepalives() {
            Some(config.keepalive_config.clone())
        } else {
            None
        },
    });

    Ok((client, connection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoTls;
    use crate::config::SslMode;
    use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use compio::net::TcpListener;
    use futures_channel::oneshot;
    use std::error::Error as _;
    use std::future;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + body.len());
        frame.push(tag);
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn successful_handshake() -> Vec<u8> {
        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&frame(b'K', &[0; 8]));
        script.extend_from_slice(&frame(b'Z', b"I"));
        script
    }

    async fn scripted_server_after_startup(
        server_says: Option<Vec<u8>>,
    ) -> (SocketAddr, oneshot::Receiver<()>) {
        scripted_server_bound("127.0.0.1:0".parse().unwrap(), server_says).await
    }

    /// The same scripted server, on a caller-chosen bind address.
    ///
    /// The per-address deadline test needs TWO stalled endpoints sharing ONE
    /// port, because `Endpoint::addresses` discards the resolved port
    /// (`Addr::Tcp(addr.ip())`) and `connect_once` dials `endpoint.port()`.
    /// Two loopback IPs on the same port is the only shape that expresses it.
    async fn scripted_server_bound(
        bind: SocketAddr,
        server_says: Option<Vec<u8>>,
    ) -> (SocketAddr, oneshot::Receiver<()>) {
        let listener = TcpListener::bind(bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (startup_seen, startup_observed) = oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            assert!(length >= 4, "startup packet length must include its header");

            let compio::BufResult(result, _) =
                socket.read_exact(vec![0u8; length - 4]).await;
            result.unwrap();
            let _ = startup_seen.send(());

            if let Some(server_says) = server_says {
                let compio::BufResult(result, _) = socket.write_all(server_says).await;
                result.unwrap();
                socket.flush().await.unwrap();
            } else {
                // The client is now past TCP connect and startup write. Wait for
                // it to close the timed-out attempt without sending a byte.
                let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
            }
        })
        .detach();

        (addr, startup_observed)
    }

    fn config_for(addr: SocketAddr, connect_timeout: Duration) -> Config {
        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(addr.ip())
            .port(addr.port())
            .ssl_mode(SslMode::Disable)
            .connect_timeout(connect_timeout);
        config
    }

    struct PendingResolver;

    impl Resolver for PendingResolver {
        async fn resolve(&mut self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            future::pending().await
        }
    }

    struct EmptyResolver;

    impl Resolver for EmptyResolver {
        async fn resolve(&mut self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(Vec::new())
        }
    }

    struct StaticResolver(SocketAddr);

    impl Resolver for StaticResolver {
        async fn resolve(&mut self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(vec![self.0])
        }
    }

    fn hostname_config_for(addr: SocketAddr, connect_timeout: Duration) -> Config {
        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("scripted.example")
            .port(addr.port())
            .ssl_mode(SslMode::Disable)
            .connect_timeout(connect_timeout);
        config
    }

    #[compio::test]
    async fn empty_dns_result_is_a_connect_error() {
        let mut config = Config::new();
        config.host("empty.example").ssl_mode(SslMode::Disable);
        let endpoint = endpoints(&config)
            .expect("one hostname is a valid endpoint list")
            .pop()
            .expect("the endpoint list contains the hostname");
        let mut resolver = EmptyResolver;

        let error = match endpoint
            .addresses(&mut resolver, LoadBalanceHosts::Disable)
            .await
        {
            Ok(_) => panic!("an empty DNS result named an address to connect to"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "error connecting to server");
        let io = error
            .source()
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("the connect error must retain its I/O cause");
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(io.to_string(), "could not resolve any addresses");
    }

    #[compio::test]
    async fn connect_timeout_covers_dns_resolution() {
        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("slow.example")
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_millis(50));
        let mut resolver = PendingResolver;

        let result = compio::time::timeout(
            Duration::from_secs(1),
            connect_with_resolver(NoTls, &config, &mut resolver),
        )
        .await
        .expect("outer watchdog expired because DNS resolution escaped connect_timeout");
        let error = match result {
            Ok(_) => panic!("a resolver that never answered produced a connection"),
            Err(error) => error,
        };

        let io = error
            .source()
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("connection timeout must retain its I/O cause");
        assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(io.to_string(), "connection timed out");
    }

    #[compio::test]
    async fn connect_timeout_covers_a_server_stalled_during_handshake() {
        let (addr, startup_observed) = scripted_server_after_startup(None).await;
        let config = config_for(addr, Duration::from_secs(1));
        let connect = compio::runtime::spawn(async move { config.connect(NoTls).await });

        compio::time::timeout(Duration::from_secs(5), startup_observed)
            .await
            .expect("client did not send startup before the test watchdog")
            .expect("server closed before observing the complete startup packet");

        let result = compio::time::timeout(Duration::from_secs(5), connect)
            .await
            .expect("outer watchdog expired because connect_timeout covered only TCP")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let error = match result {
            Ok(_) => panic!("a silent server completed the PostgreSQL handshake"),
            Err(error) => error,
        };

        let io = error
            .source()
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("connection timeout must retain its I/O cause");
        assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(io.to_string(), "connection timed out");
    }

    #[compio::test]
    async fn connect_timeout_allows_dns_and_handshake_inside_deadline() {
        let (addr, startup_observed) =
            scripted_server_after_startup(Some(successful_handshake())).await;
        let config = hostname_config_for(addr, Duration::from_secs(5));
        let mut resolver = StaticResolver(addr);

        let connected = compio::time::timeout(
            Duration::from_secs(10),
            connect_with_resolver(NoTls, &config, &mut resolver),
        )
        .await
        .expect("outer watchdog expired during scripted DNS and local handshake")
        .expect("DNS and a handshake inside connect_timeout must succeed");
        startup_observed
            .await
            .expect("server did not observe the complete startup packet");
        drop(connected);
    }

    struct ListResolver(Vec<SocketAddr>);

    impl Resolver for ListResolver {
        async fn resolve(&mut self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.clone())
        }
    }

    /// `connect_timeout` restarts for EVERY resolved address, not once per
    /// configured host entry.
    ///
    /// libpq restarts its deadline whenever `whichhost` OR `whichaddr` moves
    /// (`fe-connect.c`, `connectDBComplete`):
    ///
    /// ```text
    /// if (flag != PGRES_POLLING_OK && timeout > 0 &&
    ///     (conn->whichhost != last_whichhost ||
    ///      conn->whichaddr != last_whichaddr))
    ///     finish_time = time(NULL) + timeout;
    /// ```
    ///
    /// So a name resolving to N addresses gets N budgets, not one. Wrapping the
    /// whole per-host walk in a single budget instead means the FIRST address
    /// that stalls consumes all of it and the remaining addresses are never
    /// dialled -- a hostname whose first A record is a blackhole becomes
    /// unreachable even when its second one is healthy.
    ///
    /// Asserted on whether the second address was ever DIALLED rather than on
    /// elapsed wall-clock, so the test states the behaviour instead of timing
    /// noise.
    #[compio::test]
    async fn connect_timeout_restarts_for_each_resolved_address() {
        let (first, first_seen) = scripted_server_after_startup(None).await;
        // Same port, second loopback IP: see `scripted_server_bound`.
        let second_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (second, second_seen) = scripted_server_bound(second_bind, None).await;

        let config = hostname_config_for(first, Duration::from_millis(150));
        let mut resolver = ListResolver(vec![first, second]);

        let result = compio::time::timeout(
            Duration::from_secs(10),
            connect_with_resolver(NoTls, &config, &mut resolver),
        )
        .await
        .expect("the outer watchdog expired, so some leg had no deadline at all");
        assert!(
            result.is_err(),
            "both addresses stall forever, so this cannot connect"
        );

        // Bounded, because an address that was never dialled leaves its server
        // parked in `accept()` still holding the sender -- a bare `.await` here
        // would hang forever instead of failing.
        async fn dialled(seen: oneshot::Receiver<()>) -> bool {
            compio::time::timeout(Duration::from_secs(2), seen)
                .await
                .is_ok_and(|received| received.is_ok())
        }

        assert!(
            dialled(first_seen).await,
            "the first address was never dialled"
        );
        assert!(
            dialled(second_seen).await,
            "the second address was never dialled: one budget was shared across the whole \
             host walk instead of restarting per address"
        );
    }
}
