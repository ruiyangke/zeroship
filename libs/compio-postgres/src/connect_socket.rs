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
