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
use std::future::Future;
use std::io;
use std::time::Duration;

#[allow(dead_code)]
pub(crate) async fn connect_socket(
    addr: &Addr,
    port: u16,
    connect_timeout: Option<Duration>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] tcp_user_timeout: Option<
        Duration,
    >,
    keepalive_config: Option<&KeepaliveConfig>,
) -> Result<Socket, Error> {
    match addr {
        Addr::Tcp(ip) => {
            // compio's TcpStream::connect takes any `ToSocketAddrsAsync` — a
            // `(IpAddr, u16)` tuple is directly supported.
            let stream =
                connect_with_timeout(TcpStream::connect((*ip, port)), connect_timeout).await?;

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
            let socket = connect_with_timeout(UnixStream::connect(path), connect_timeout).await?;
            Ok(Socket::new_unix(socket))
        }
    }
}

async fn connect_with_timeout<F, T>(connect: F, timeout: Option<Duration>) -> Result<T, Error>
where
    F: Future<Output = io::Result<T>>,
{
    match timeout {
        Some(timeout) => match compio::time::timeout(timeout, connect).await {
            Ok(Ok(socket)) => Ok(socket),
            Ok(Err(e)) => Err(Error::connect(e)),
            Err(_) => Err(Error::connect(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            ))),
        },
        None => match connect.await {
            Ok(socket) => Ok(socket),
            Err(e) => Err(Error::connect(e)),
        },
    }
}
