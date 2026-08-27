// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// tokio -> compio migration notes:
//   - `tokio::net::{TcpStream, UnixStream}` -> `compio::net::{TcpStream, UnixStream}`
//   - `tokio::time::timeout`                -> `compio::time::timeout`
//   - `socket2::SockRef` for keepalive and TCP_USER_TIMEOUT works unchanged
//     because compio exposes `AsRawFd` on its sockets.
//
// We accept the same `Addr` input the upstream source does, but build it
// through `connect.rs`, which owns DNS resolution. That keeps the
// "dial + tune the socket" responsibility isolated in one file,
// matching the upstream layering.

use crate::client::Addr;
use crate::keepalive::KeepaliveConfig;
use crate::{Error, Socket};
use compio::net::TcpStream;
#[cfg(unix)]
use compio::net::UnixStream;
use socket2::{SockRef, TcpKeepalive};
#[cfg(unix)]
use std::io;
use std::time::Duration;

#[allow(dead_code)]
pub(crate) async fn connect_socket(
    addr: &Addr,
    port: u16,
    tcp_user_timeout: Option<Duration>,
    keepalive_config: Option<&KeepaliveConfig>,
    #[cfg_attr(not(unix), allow(unused_variables))] require_peer: Option<&str>,
) -> Result<Socket, Error> {
    match addr {
        Addr::Tcp(ip) => {
            // TCP_USER_TIMEOUT is applied inside the dial, because it only does
            // its job if it is already on the socket when the SYN goes out.
            let stream = dial_tcp(*ip, port, tcp_user_timeout).await?;

            stream.set_nodelay(true).map_err(Error::connect)?;

            // socket2 borrows the raw fd via AsFd - no ownership change,
            // so the TcpStream remains usable afterwards.
            let sock_ref = SockRef::from(&stream);

            // The keepalive options, unlike TCP_USER_TIMEOUT, act only on an
            // ESTABLISHED connection, so setting them after the dial is
            // correct. libpq sets them earlier only because it happens to hold
            // the socket earlier, not because the ordering matters for them.
            if let Some(keepalive_config) = keepalive_config {
                keepalive_config
                    .check_expressible()
                    .map_err(|message| Error::config(message.into()))?;
                sock_ref
                    .set_tcp_keepalive(&TcpKeepalive::from(keepalive_config))
                    .map_err(Error::connect)?;
            }

            Ok(Socket::new_tcp(stream))
        }
        #[cfg(unix)]
        Addr::Unix(dir) => {
            let path = dir.join(format!(".s.PGSQL.{port}"));
            let socket = UnixStream::connect(path).await.map_err(Error::connect)?;
            if let Some(require_peer) = require_peer {
                check_require_peer(&socket, require_peer)
                    .await
                    .map_err(Error::connect)?;
            }
            Ok(Socket::new_unix(socket))
        }
    }
}

/// Dial a TCP address with `TCP_USER_TIMEOUT` applied BEFORE `connect(2)`.
///
/// The ordering is load-bearing, not cosmetic. Linux consults
/// `icsk_user_timeout` while the socket is still in SYN_SENT, so the option
/// bounds the handshake only if it is already set when the SYN goes out;
/// applied afterwards it can bound nothing but an already-established
/// connection. libpq depends on exactly that: `setTCPUserTimeout` runs at
/// `fe-connect.c:3427`, inside the `addr_cur->family != AF_UNIX` block, and
/// `connect()` is not reached until `fe-connect.c:3481`.
///
/// Measured on Linux 6.12.90 against a blackholed address, one variable apart:
/// with the option set beforehand `connect` fails at 3.01s for a 3000ms
/// setting; on the same socket with the option unset it was still retrying at
/// 19.63s. Setting it afterwards, as this function's caller used to, left a
/// configured `tcp_user_timeout` inert during the one phase it was asked to
/// bound - and when the dial never completed, the option was never set at all.
///
/// `compio::net::TcpStream::connect` only hands back a socket that is already
/// connected, and compio-net 0.11.1's `SocketOpts` (`src/opts.rs`) carries no
/// `TCP_USER_TIMEOUT` field, so neither entry point can set it early enough.
/// Submitting the connect here is what makes the fd reachable before the
/// handshake starts; it mirrors what `compio_net::Socket::connect_async` does
/// internally, which is not reachable from outside that crate (`mod socket` is
/// private).
#[cfg(target_os = "linux")]
async fn dial_tcp(
    ip: std::net::IpAddr,
    port: u16,
    tcp_user_timeout: Option<Duration>,
) -> Result<TcpStream, Error> {
    use compio::buf::{BufResult, IntoInner};
    use compio::driver::{ToSharedFd, op::Connect};
    use socket2::{Domain, Protocol, SockAddr, Type};

    // Without the option there is nothing to order, so take compio's own path.
    let Some(tcp_user_timeout) = tcp_user_timeout else {
        return TcpStream::connect((ip, port)).await.map_err(Error::connect);
    };

    let target = std::net::SocketAddr::new(ip, port);
    let socket = socket2::Socket::new(
        Domain::for_address(target),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(Error::connect)?;
    // compio creates its own sockets non-blocking under the poll driver and
    // relies on it (`compio-driver` `src/sys/poll/op.rs`); io_uring is
    // indifferent. Setting it keeps this socket correct under either.
    socket.set_nonblocking(true).map_err(Error::connect)?;
    SockRef::from(&socket)
        .set_tcp_user_timeout(Some(tcp_user_timeout))
        .map_err(Error::connect)?;

    let attacher = compio::runtime::Attacher::new(socket).map_err(Error::connect)?;
    let BufResult(result, op) = compio::runtime::submit(Connect::new(
        attacher.to_shared_fd(),
        SockAddr::from(target),
    ))
    .await;
    // The submitted op holds its own clone of the shared fd, so it has to be
    // dropped before the socket below can be reclaimed.
    drop(op);
    result.map_err(Error::connect)?;

    let socket = attacher.into_inner().try_unwrap().map_err(|_| {
        Error::connect(io::Error::other(
            "the dialled socket was still shared after its connect completed",
        ))
    })?;
    // Attaching a second time is a no-op on this path: `Proactor::attach`
    // documents "io-uring & polling: it will do nothing but return Ok(())".
    TcpStream::from_std(std::net::TcpStream::from(socket)).map_err(Error::connect)
}

/// libpq's `setTCPUserTimeout` is guarded by `#ifdef TCP_USER_TIMEOUT`, so on a
/// platform without the option the dial is not bounded there either. Matching
/// that is the point: the parameter is refused or honoured, never emulated.
#[cfg(not(target_os = "linux"))]
async fn dial_tcp(
    ip: std::net::IpAddr,
    port: u16,
    _tcp_user_timeout: Option<Duration>,
) -> Result<TcpStream, Error> {
    TcpStream::connect((ip, port)).await.map_err(Error::connect)
}

/// Authenticate a Unix socket before any PostgreSQL bytes cross it.
///
/// libpq compares the configured name with the canonical passwd-database name
/// for the peer UID, rather than resolving the requested name to a UID. That
/// distinction makes aliases for the same numeric UID fail instead of quietly
/// weakening an exact-name policy.
#[cfg(all(unix, target_os = "linux"))]
async fn check_require_peer(stream: &UnixStream, required: &str) -> io::Result<()> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    use nix::unistd::{Uid, User};

    let credentials = getsockopt(stream, PeerCredentials).map_err(|error| {
        io::Error::other(format!(
            "requirepeer: could not get peer credentials: {error}"
        ))
    })?;
    let uid = credentials.uid();

    // NSS may consult LDAP or another blocking provider. Keep that work off
    // the compio thread; as with DNS, a timed-out lookup cannot stop the
    // underlying blocking call, but it must not stall unrelated I/O.
    let lookup = compio::runtime::spawn_blocking(move || User::from_uid(Uid::from_raw(uid)))
        .await
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    let actual = lookup
        .map_err(|error| {
            io::Error::other(format!(
                "requirepeer: could not look up peer user ID {uid}: {error}"
            ))
        })?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("requirepeer: no operating-system user has peer user ID {uid}"),
            )
        })?
        .name;

    if actual != required {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "requirepeer specifies \"{required}\", but actual peer user name is \"{actual}\""
            ),
        ));
    }

    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
async fn check_require_peer(_stream: &UnixStream, _required: &str) -> io::Result<()> {
    // Linux SO_PEERCRED is the implemented trust primitive. A platform on
    // which we cannot obtain equivalent kernel credentials must refuse the
    // requested check, never treat it as advisory.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "requirepeer is not supported on this platform",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use compio::net::TcpListener;
    use std::net::{IpAddr, Ipv4Addr};

    /// Dial a listener this test owns, so the assertions are about the socket
    /// options and nothing else. The listener is returned so the connection
    /// stays open for the duration of the `getsockopt` calls.
    async fn dial(
        keepalive: Option<&KeepaliveConfig>,
        tcp_user_timeout: Option<Duration>,
    ) -> (Socket, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let socket = connect_socket(
            &Addr::Tcp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            addr.port(),
            tcp_user_timeout,
            keepalive,
            None,
        )
        .await
        .expect("connect to the loopback listener");
        (socket, listener)
    }

    /// The keepalive values a caller configures must reach the kernel, not just
    /// the `TcpKeepalive` builder. `set_tcp_keepalive` reports `Ok` for a
    /// config it applied partially or not at all, so reading the options back
    /// off the connected descriptor is the only check that distinguishes
    /// "configured" from "in effect".
    #[compio::test]
    async fn configured_keepalive_values_reach_the_kernel() {
        let config = KeepaliveConfig {
            idle: Duration::from_secs(11),
            interval: Some(Duration::from_secs(3)),
            retries: Some(5),
        };
        let (socket, _listener) = dial(Some(&config), Some(Duration::from_secs(7))).await;

        let fd = socket.borrowed_fd();
        let sock_ref = SockRef::from(&fd);
        assert!(
            sock_ref.keepalive().expect("read SO_KEEPALIVE"),
            "a configured keepalive left SO_KEEPALIVE off"
        );
        assert_eq!(
            sock_ref.tcp_keepalive_time().expect("read TCP_KEEPIDLE"),
            Duration::from_secs(11),
            "keepalive idle did not reach the kernel"
        );
        assert_eq!(
            sock_ref
                .tcp_keepalive_interval()
                .expect("read TCP_KEEPINTVL"),
            Duration::from_secs(3),
            "keepalive interval did not reach the kernel"
        );
        assert_eq!(
            sock_ref.tcp_keepalive_retries().expect("read TCP_KEEPCNT"),
            5,
            "keepalive retry count did not reach the kernel"
        );
        assert_eq!(
            sock_ref.tcp_user_timeout().expect("read TCP_USER_TIMEOUT"),
            Some(Duration::from_secs(7)),
            "TCP_USER_TIMEOUT did not reach the kernel"
        );
    }

    /// The one-variable control: the SAME dial with no keepalive config must
    /// leave SO_KEEPALIVE off. Without it, the assertions above would still
    /// pass on a kernel whose defaults happened to match, and would pass on a
    /// `connect_socket` that enabled keepalives unconditionally.
    #[compio::test]
    async fn no_keepalive_config_leaves_the_socket_default() {
        let (socket, _listener) = dial(None, None).await;

        let fd = socket.borrowed_fd();
        let sock_ref = SockRef::from(&fd);
        assert!(
            !sock_ref.keepalive().expect("read SO_KEEPALIVE"),
            "an unconfigured socket had SO_KEEPALIVE on"
        );
        assert_eq!(
            sock_ref.tcp_user_timeout().expect("read TCP_USER_TIMEOUT"),
            None,
            "an unconfigured socket had TCP_USER_TIMEOUT set"
        );
    }

    /// "A value of zero uses the system default" is what PostgreSQL documents
    /// for `keepalives_idle`, `keepalives_interval` AND `keepalives_count`.
    /// Linux rejects a zero in any of the three matching socket options with
    /// EINVAL, so passing one through turns "use the default" into a connection
    /// that cannot be opened at all. `keepalives_count=0` reaches this straight
    /// from a DSN; the other two reach it through `Config`'s setters.
    ///
    /// The assertion is deliberately about SO_KEEPALIVE still being ON rather
    /// than only about the connect succeeding: skipping a zero must leave the
    /// system's own timings in force, not silently disable keepalives.
    #[compio::test]
    async fn a_zero_value_leaves_that_socket_option_at_the_system_default() {
        let cases = [
            (
                "count",
                KeepaliveConfig {
                    idle: Duration::from_secs(11),
                    interval: Some(Duration::from_secs(3)),
                    retries: Some(0),
                },
            ),
            (
                "idle",
                KeepaliveConfig {
                    idle: Duration::ZERO,
                    interval: Some(Duration::from_secs(3)),
                    retries: Some(5),
                },
            ),
            (
                "interval",
                KeepaliveConfig {
                    idle: Duration::from_secs(11),
                    interval: Some(Duration::ZERO),
                    retries: Some(5),
                },
            ),
        ];

        for (zeroed, config) in cases {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback listener");
            let addr = listener.local_addr().expect("listener address");
            let socket = connect_socket(
                &Addr::Tcp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                addr.port(),
                None,
                Some(&config),
                None,
            )
            .await
            .unwrap_or_else(|error| {
                panic!("a zero keepalives_{zeroed} failed the connection: {error:?}")
            });

            let fd = socket.borrowed_fd();
            let sock_ref = SockRef::from(&fd);
            assert!(
                sock_ref.keepalive().expect("read SO_KEEPALIVE"),
                "a zero keepalives_{zeroed} switched keepalives off entirely"
            );
            // The two values that were NOT zeroed must still reach the kernel:
            // "skip the zero" must not degrade into "skip the whole config".
            if zeroed != "idle" {
                assert_eq!(
                    sock_ref.tcp_keepalive_time().expect("read TCP_KEEPIDLE"),
                    Duration::from_secs(11),
                    "a zero keepalives_{zeroed} discarded the idle value beside it"
                );
            }
            if zeroed != "count" {
                assert_eq!(
                    sock_ref.tcp_keepalive_retries().expect("read TCP_KEEPCNT"),
                    5,
                    "a zero keepalives_{zeroed} discarded the retry count beside it"
                );
            }
            drop(listener);
        }
    }

    /// `tcp_user_timeout` has to bound the DIAL, not just an established
    /// connection. Linux reads `icsk_user_timeout` while the socket is in
    /// SYN_SENT, so the option only does that job if it is already set when the
    /// SYN goes out; applied after `connect(2)` returns it cannot bound a
    /// handshake that never finishes, and the tests above cannot see the
    /// difference because they all dial a listener that answers immediately.
    ///
    /// THE CONTROL LEG IS WHAT MAKES THE ASSERTION MEAN ANYTHING. It dials the
    /// same address with no `tcp_user_timeout` and requires that it is STILL
    /// hanging, proving the address really does swallow SYNs here. Without it a
    /// host that answers this address with a RST would let the bounded leg
    /// return quickly for entirely the wrong reason, and the test would pass
    /// against the very defect it exists to catch.
    #[compio::test]
    async fn tcp_user_timeout_bounds_a_dial_that_never_completes() {
        // RFC 5737 reserves 192.0.2.0/24 for documentation, so no host answers
        // and the SYN is dropped rather than refused.
        const BLACKHOLE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        const PORT: u16 = 5432;
        // Comfortably under the kernel's own SYN budget, which is six retries
        // by default (net.ipv4.tcp_syn_retries) and runs past two minutes.
        const STILL_HANGING: Duration = Duration::from_secs(4);
        const BOUND: Duration = Duration::from_secs(2);

        let control = compio::time::timeout(
            STILL_HANGING,
            connect_socket(&Addr::Tcp(BLACKHOLE), PORT, None, None, None),
        )
        .await;
        assert!(
            control.is_err(),
            "fixture unusable: dialling {BLACKHOLE} settled within \
             {STILL_HANGING:?}, so it does not blackhole on this host and the \
             bounded leg below would prove nothing"
        );

        let started = std::time::Instant::now();
        let bounded = compio::time::timeout(
            STILL_HANGING * 2,
            connect_socket(&Addr::Tcp(BLACKHOLE), PORT, Some(BOUND), None, None),
        )
        .await;
        let elapsed = started.elapsed();

        bounded
            .expect("tcp_user_timeout did not bound the dial at all")
            .expect_err("a blackholed address reported a successful connection");
        assert!(
            elapsed < STILL_HANGING,
            "tcp_user_timeout={BOUND:?} took {elapsed:?}, past the \
             {STILL_HANGING:?} that the SAME dial survives with no timeout set, \
             so the option is not reaching the socket before the SYN"
        );
    }

    /// A DSN's values must reach the kernel IN LIBPQ'S UNITS.
    ///
    /// The tests above build a `KeepaliveConfig` by hand, so they prove
    /// `connect_socket` applies what it is given and nothing about what the
    /// connection string turns into. That is where a unit is lost: libpq
    /// documents `keepalives_idle` and `keepalives_interval` in SECONDS but
    /// `tcp_user_timeout` in MILLISECONDS, and reading the whole family as
    /// seconds is a mistake no accept/reject test can see - the DSN parses,
    /// the option is set, and the value is a thousand times too large.
    ///
    /// The numbers below are chosen so the two units cannot be confused: a
    /// `tcp_user_timeout` of 7000 is seven seconds, and would be just under two
    /// hours if it were read as seconds.
    #[compio::test]
    async fn dsn_values_reach_the_kernel_in_libpq_units() {
        let config: crate::Config = "host=127.0.0.1 keepalives=1 keepalives_idle=11 \
             keepalives_interval=3 keepalives_count=5 tcp_user_timeout=7000"
            .parse()
            .expect("the DSN parses");

        // Exactly what `connect.rs` hands to `connect_socket`.
        let keepalive = config
            .get_keepalives()
            .then(|| config.keepalive_config.clone());
        let (socket, _listener) =
            dial(keepalive.as_ref(), config.get_tcp_user_timeout().copied()).await;

        let fd = socket.borrowed_fd();
        let sock_ref = SockRef::from(&fd);
        assert_eq!(
            sock_ref.tcp_keepalive_time().expect("read TCP_KEEPIDLE"),
            Duration::from_secs(11),
            "keepalives_idle is documented in seconds"
        );
        assert_eq!(
            sock_ref
                .tcp_keepalive_interval()
                .expect("read TCP_KEEPINTVL"),
            Duration::from_secs(3),
            "keepalives_interval is documented in seconds"
        );
        assert_eq!(
            sock_ref.tcp_keepalive_retries().expect("read TCP_KEEPCNT"),
            5,
            "keepalives_count is a plain count"
        );
        assert_eq!(
            sock_ref.tcp_user_timeout().expect("read TCP_USER_TIMEOUT"),
            Some(Duration::from_millis(7000)),
            "tcp_user_timeout is documented in MILLISECONDS, so 7000 means \
             seven seconds; reading it as seconds sets a timeout 1000x too \
             long and silently stops it ever firing"
        );
    }

    /// A keepalive that is not a whole number of seconds is not expressible:
    /// `TCP_KEEPIDLE` and `TCP_KEEPINTVL` take whole seconds, and socket2
    /// converts a `Duration` with `as_secs()`, which TRUNCATES (socket2 0.6.3,
    /// `src/sys/unix.rs:1294`).
    ///
    /// That truncation fails in two ways, and both are covered below. Under one
    /// second it reaches the kernel as 0, which Linux refuses with EINVAL,
    /// surfacing as an opaque failed connection rather than a statement about
    /// the value the caller chose. At or above one second it is ACCEPTED at the
    /// truncated value, which is worse: 1500ms becomes 1s and 59_999ms becomes
    /// 59s with nothing reported, so the caller cannot find out. `Duration::ZERO`
    /// is the "leave it unset" sentinel and is handled separately.
    #[compio::test]
    async fn a_keepalive_that_is_not_whole_seconds_is_refused_by_name() {
        for (label, config) in [
            (
                "keepalives_idle",
                KeepaliveConfig {
                    idle: Duration::from_millis(500),
                    interval: None,
                    retries: None,
                },
            ),
            (
                "keepalives_interval",
                KeepaliveConfig {
                    idle: Duration::from_secs(10),
                    interval: Some(Duration::from_millis(500)),
                    retries: None,
                },
            ),
            // A FRACTIONAL value above one second is the same defect: 1500ms
            // truncates to 1s and 59_999ms to 59s, so the caller silently gets
            // a different keepalive from the one they asked for. Keying the
            // guard to `as_secs() == 0` catches only the values that reach the
            // kernel as 0; the quantity the rule is about is the sub-second
            // remainder.
            (
                "keepalives_idle",
                KeepaliveConfig {
                    idle: Duration::from_millis(1500),
                    interval: None,
                    retries: None,
                },
            ),
            (
                "keepalives_interval",
                KeepaliveConfig {
                    idle: Duration::from_secs(10),
                    interval: Some(Duration::from_millis(59_999)),
                    retries: None,
                },
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback listener");
            let addr = listener.local_addr().expect("listener address");
            let error = connect_socket(
                &Addr::Tcp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                addr.port(),
                None,
                Some(&config),
                None,
            )
            .await
            .expect_err("a keepalive that is not whole seconds was accepted");
            let chain = {
                let mut text = error.to_string();
                let mut source = std::error::Error::source(&error);
                while let Some(inner) = source {
                    text.push_str(&format!(": {inner}"));
                    source = std::error::Error::source(inner);
                }
                text
            };
            assert!(
                chain.contains(label),
                "the refusal did not name {label}: {chain}"
            );
            assert!(
                !chain.contains("Invalid argument"),
                "{label} still surfaced as a raw EINVAL: {chain}"
            );
        }
    }
}
