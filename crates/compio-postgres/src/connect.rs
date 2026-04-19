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
//   (the `SHOW transaction_read_only` query) is deferred to Phase 4:
//   it requires `Client::simple_query_raw` which lands with the full
//   query surface. We return an error if a non-default value is set,
//   to surface the limitation rather than silently ignore it.

use crate::client::{Addr, Client, SocketConfig};
use crate::config::{Host, LoadBalanceHosts, TargetSessionAttrs};
use crate::connect_raw::connect_raw;
use crate::connect_socket::connect_socket;
use crate::connection::Connection;
use crate::tls::MakeTlsConnect;
use crate::{Config, Error, Socket};
use compio::net::ToSocketAddrsAsync;
use rand::seq::SliceRandom;
use std::{cmp, io};

pub async fn connect<T>(
    mut tls: T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
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

    // Either one of config.host or config.hostaddr is empty, or their
    // lengths are equal.
    let num_hosts = cmp::max(config.get_hosts().len(), config.get_hostaddrs().len());

    if config.get_ports().len() > 1 && config.get_ports().len() != num_hosts {
        return Err(Error::config("invalid number of ports".into()));
    }

    let mut indices = (0..num_hosts).collect::<Vec<_>>();
    if config.get_load_balance_hosts() == LoadBalanceHosts::Random {
        indices.shuffle(&mut rand::rng());
    }

    let mut error = None;
    for i in indices {
        let host = config.get_hosts().get(i);
        let hostaddr = config.get_hostaddrs().get(i);
        let port = config
            .get_ports()
            .get(i)
            .or_else(|| config.get_ports().first())
            .copied()
            .unwrap_or(5432);

        // `host` is the TLS validation hostname.
        let hostname = match host {
            Some(Host::Tcp(host)) => Some(host.clone()),
            #[cfg(unix)]
            Some(Host::Unix(_)) => None,
            None => None,
        };

        // Prefer `hostaddr` (numeric IP) when present; else fall back
        // to `host` (which may be a hostname or Unix socket path).
        let addr = match hostaddr {
            Some(ipaddr) => Host::Tcp(ipaddr.to_string()),
            None => host.cloned().unwrap(),
        };

        match connect_host(addr, hostname, port, &mut tls, config).await {
            Ok((client, connection)) => return Ok((client, connection)),
            Err(e) => error = Some(e),
        }
    }

    Err(error.unwrap())
}

async fn connect_host<T>(
    host: Host,
    hostname: Option<String>,
    port: u16,
    tls: &mut T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    match host {
        Host::Tcp(host) => {
            // Resolve DNS via compio.
            let mut addrs = (&*host, port)
                .to_socket_addrs_async()
                .await
                .map_err(Error::connect)?
                .collect::<Vec<_>>();

            if config.get_load_balance_hosts() == LoadBalanceHosts::Random {
                addrs.shuffle(&mut rand::rng());
            }

            let mut last_err = None;
            for addr in addrs {
                match connect_once(Addr::Tcp(addr.ip()), hostname.as_deref(), port, tls, config)
                    .await
                {
                    Ok(stream) => return Ok(stream),
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                };
            }

            Err(last_err.unwrap_or_else(|| {
                Error::connect(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not resolve any addresses",
                ))
            }))
        }
        #[cfg(unix)]
        Host::Unix(path) => {
            connect_once(Addr::Unix(path), hostname.as_deref(), port, tls, config).await
        }
    }
}

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
    let socket = connect_socket(
        &addr,
        port,
        config.get_connect_timeout().copied(),
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
    let (mut client, connection) = connect_raw(socket, tls, has_hostname, config).await?;

    // TargetSessionAttrs post-connect probe. The source interleaves a
    // `simple_query_raw("SHOW transaction_read_only")` with
    // `connection.poll_unpin` — fail the probe if the connection dies.
    // Our `Connection::run` consumes `self`, so the in-place interleave
    // of the source cannot be expressed directly. Implementing this
    // properly requires either: (a) a `poll_one_step` method on
    // Connection; or (b) spawning the connection and re-joining it
    // after the probe. Neither fits cleanly into Phase 4's scope, so we
    // surface the limitation rather than silently ignore a non-default
    // value. Phase 5 will revisit alongside the Transaction port.
    if config.get_target_session_attrs() != TargetSessionAttrs::Any {
        return Err(Error::config(
            "target_session_attrs is not yet supported; Phase 5 will implement the probe".into(),
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
