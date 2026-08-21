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
use std::time::Duration;

#[allow(dead_code)]
pub(crate) async fn connect_socket(
    addr: &Addr,
    port: u16,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] tcp_user_timeout: Option<
        Duration,
    >,
    keepalive_config: Option<&KeepaliveConfig>,
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
            Ok(Socket::new_unix(socket))
        }
    }
}
