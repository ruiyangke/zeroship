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
use std::{cmp, io};

pub async fn connect<T>(
    mut tls: T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
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
/// does not get one; that case is handled inside [`negotiate_tls`] on the
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
    // libpq: "sslmode is ignored for Unix domain socket communication."
    // A local socket has no network to eavesdrop on and no host name to put in
    // a certificate, so every mode - including verify-full - is plaintext.
    #[cfg(unix)]
    if matches!(addr, Addr::Unix(_)) {
        return connect_leg(&addr, hostname, port, tls, config, Encryption::Plaintext).await;
    }

    let first = Encryption::first_for(config.get_ssl_mode());
    let err = match connect_leg(&addr, hostname, port, tls, config, first).await {
        Ok(connected) => return Ok(connected),
        Err(e) => e,
    };

    let retry = match (config.get_ssl_mode(), first) {
        // TLS first, and the handshake is what failed: dial again, in the clear.
        //
        // KNOWN SHARP EDGE, and it is libpq's. "The handshake failed" includes
        // "the certificate did not verify", because at this point the two are
        // the same event - `pqsecure_open_client` reports one status for both,
        // and so does rustls. So `sslmode=prefer sslrootcert=<ca>` against a
        // server presenting a bad certificate does not fail: it silently
        // downgrades to plaintext. That was raised on pgsql-hackers in 2016 and
        // libpq still behaves this way.
        //
        // We reproduce it because this is a driver and `prefer` is libpq's
        // word, not ours. The mode's own documentation says it "makes no sense
        // from a security point of view"; a deployment that cares names a
        // stronger mode, and `verify-ca`/`verify-full` cannot reach this arm at
        // all. (Npgsql took the other road and made the unverified outcome an
        // explicit opt-in. If that is wanted here it is a deliberate
        // divergence, and it belongs in one place: this arm.)
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
    let (mut client, connection) =
        connect_raw(socket, tls, encryption, has_hostname, config).await?;

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
