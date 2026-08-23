// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// tokio → compio migration notes:
//   - `tokio::net::{TcpStream, UnixStream}` → `compio::net::{TcpStream, UnixStream}`
//   - `tokio::time::timeout`                → `compio::time::timeout`
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
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] tcp_user_timeout: Option<
        Duration,
    >,
    keepalive_config: Option<&KeepaliveConfig>,
    #[cfg_attr(not(unix), allow(unused_variables))] require_peer: Option<&str>,
) -> Result<Socket, Error> {
    match addr {
        Addr::Tcp(ip) => {
            // compio's TcpStream::connect takes any `ToSocketAddrsAsync` — a
            // `(IpAddr, u16)` tuple is directly supported.
            let stream = TcpStream::connect((*ip, port))
                .await
                .map_err(Error::connect)?;

            stream.set_nodelay(true).map_err(Error::connect)?;

            // socket2 borrows the raw fd via AsFd — no ownership change,
            // so the TcpStream remains usable afterwards.
            let sock_ref = SockRef::from(&stream);

            #[cfg(target_os = "linux")]
            if let Some(tcp_user_timeout) = tcp_user_timeout {
                sock_ref
                    .set_tcp_user_timeout(Some(tcp_user_timeout))
                    .map_err(Error::connect)?;
            }

            if let Some(keepalive_config) = keepalive_config {
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
            sock_ref.keepalive_time().expect("read TCP_KEEPIDLE"),
            Duration::from_secs(11),
            "keepalive idle did not reach the kernel"
        );
        assert_eq!(
            sock_ref.keepalive_interval().expect("read TCP_KEEPINTVL"),
            Duration::from_secs(3),
            "keepalive interval did not reach the kernel"
        );
        assert_eq!(
            sock_ref.keepalive_retries().expect("read TCP_KEEPCNT"),
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
                    sock_ref.keepalive_time().expect("read TCP_KEEPIDLE"),
                    Duration::from_secs(11),
                    "a zero keepalives_{zeroed} discarded the idle value beside it"
                );
            }
            if zeroed != "count" {
                assert_eq!(
                    sock_ref.keepalive_retries().expect("read TCP_KEEPCNT"),
                    5,
                    "a zero keepalives_{zeroed} discarded the retry count beside it"
                );
            }
            drop(listener);
        }
    }
}
