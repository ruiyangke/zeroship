// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Host resolution + failover. Walks the config.host / config.hostaddr
// list, calls connect_socket -> connect_tls -> connect_raw for each
// host until one succeeds. Returns (Client, Connection).
//
// Differences from the tokio source:
//
// * DNS resolution uses compio's `ToSocketAddrsAsync` - we call
//   `(host, port).to_socket_addrs_async()` instead of
//   `tokio::net::lookup_host`.
// * Host failover is sequential (`for host in hosts`). The source uses
//   `FuturesOrdered` for parallel attempts when `hosts > 1`; that's a
//   nice-to-have future optimisation and documented as a hand-off in
//   PHASE3.md.
// * `target_session_attrs` is checked on the raw stream before `connect_raw`
//   packages it into a `Connection`. `PreferStandby` makes a standby-only
//   pass over the host list followed, if needed, by an any-host pass.

use crate::client::{Addr, Client, SocketConfig};
use crate::config::{Host, LoadBalanceHosts, SslMode, TargetSessionAttrs};
use crate::connect_raw::connect_raw_with_target_session_attrs;
use crate::connect_socket::connect_socket;
use crate::connect_tls::Encryption;
use crate::connection::Connection;
use crate::passfile;
use crate::tls::MakeTlsConnect;
use crate::{Config, Error, Socket};
use compio::net::ToSocketAddrsAsync;
use rand::seq::SliceRandom;
use std::borrow::Cow;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
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

    /// The hostname this endpoint is spelled with in a password file.
    ///
    /// A Unix socket matches as `localhost` rather than as its socket path. A
    /// bare `hostaddr` with no name matches as the ADDRESS - measured against
    /// libpq 16.14, where a file keyed by the IP was used and one keyed by
    /// `localhost` was not.
    fn passfile_host(&self) -> String {
        match &self.target {
            #[cfg(unix)]
            EndpointTarget::Unix(_) => passfile::UNIX_SOCKET_HOST.to_owned(),
            EndpointTarget::Name(host) => host.clone(),
            EndpointTarget::Ip(ip) => self.hostname.clone().unwrap_or_else(|| ip.to_string()),
        }
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
    config.validate_connection_settings()?;

    #[cfg(not(target_os = "linux"))]
    if config
        .get_hosts()
        .iter()
        .any(|host| matches!(host, Host::Tcp(name) if name.starts_with('@')))
    {
        return Err(Error::config(
            "host values beginning with `@` require abstract Unix sockets, which this target does not implement"
                .into(),
        ));
    }

    // A DIVERGENCE FROM libpq, and a deliberate one. `postgres:///db` parses
    // here exactly as it does there, but libpq then connects over a
    // compiled-in default socket directory (overridable by `PGHOST`), while
    // this refuses. Measured: psql accepts `postgres:///postgres?user=postgres`
    // and connects.
    //
    // The reason is this crate's own rule -- a published library takes
    // resolved options from its caller and reads no process configuration
    // (`tests/common/env.rs`, enforced by a workspace source gate). Half of
    // libpq's behaviour here IS process configuration, and implementing the
    // other half alone would mean silently dialling a platform-specific path
    // the caller never named. Refusing says so instead.
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

    indices
        .into_iter()
        .map(|i| -> Result<Endpoint, Error> {
            let host = config.get_hosts().get(i);
            let hostname = match host {
                Some(Host::Tcp(host)) if !host.is_empty() => Some(host.clone()),
                Some(Host::Tcp(_)) => None,
                #[cfg(unix)]
                Some(Host::Unix(_)) => None,
                None => None,
            };
            let target = match config.get_hostaddrs().get(i).copied().flatten() {
                Some(ip) => EndpointTarget::Ip(ip),
                None => match host {
                    Some(Host::Tcp(host)) if !host.is_empty() => {
                        EndpointTarget::Name(host.clone())
                    }
                    Some(Host::Tcp(_)) | None => {
                        return Err(Error::config(
                            format!(
                                "host and hostaddr entry {} are both empty; this driver does not infer libpq's compiled default Unix socket directory",
                                i + 1
                            )
                            .into(),
                        ));
                    }
                    #[cfg(unix)]
                    Some(Host::Unix(path)) => EndpointTarget::Unix(path.clone()),
                },
            };
            let port = config
                .get_ports()
                .get(i)
                .or_else(|| config.get_ports().first())
                .copied()
                .unwrap_or(5432);

            Ok(Endpoint {
                target,
                hostname,
                port,
            })
        })
        .collect()
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
    let target = config.get_target_session_attrs();

    if target == TargetSessionAttrs::PreferStandby {
        let first_pass_error = match connect_pass(
            &endpoints,
            resolver,
            &mut tls,
            config,
            TargetSessionAttrs::Standby,
        )
        .await
        {
            Ok(connected) => return Ok(connected),
            Err(error) => error,
        };

        // `prefer-standby` starts its any-host pass only after the standby
        // search exhausts retryable connection failures and target
        // mismatches. A completed startup/authentication failure is terminal
        // for the whole connection request, just as it is for every other
        // target_session_attrs value.
        if stops_host_walk(&first_pass_error) {
            return Err(first_pass_error);
        }

        return connect_pass(
            &endpoints,
            resolver,
            &mut tls,
            config,
            TargetSessionAttrs::Any,
        )
        .await;
    }

    connect_pass(&endpoints, resolver, &mut tls, config, target).await
}

/// PostgreSQL's one server-side exception to terminal startup errors.
///
/// SQLSTATE 57P03 means that this server is temporarily unable to accept a
/// connection. libpq advances directly to the next CONFIGURED HOST, skipping
/// sibling addresses and alternate encryption methods for the current host.
fn advances_to_next_host(error: &Error) -> bool {
    error.code() == Some(&crate::error::SqlState::CANNOT_CONNECT_NOW)
}

/// Whether an error ends the entire configured-host walk.
///
/// A target-session SQL/result rejection is the exception among database
/// errors: libpq advances to the next configured host for that classification.
/// Failure to communicate during the same probe is globally terminal.
fn stops_host_walk(error: &Error) -> bool {
    if error.is_target_session_attrs() {
        return false;
    }

    error.is_target_session_attrs_fatal()
        || error.is_authentication()
        || error
            .code()
            .is_some_and(|code| code != &crate::error::SqlState::CANNOT_CONNECT_NOW)
}

async fn connect_pass<T, R>(
    endpoints: &[Endpoint],
    resolver: &mut R,
    tls: &mut T,
    config: &Config,
    target_session_attrs: TargetSessionAttrs,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
    R: Resolver,
{
    let mut error = None;
    for endpoint in endpoints {
        // The password file is keyed on host, port, database and user, so it
        // is consulted PER ENDPOINT: failover to a different host or port can
        // legitimately select a different line. libpq does the same.
        let from_passfile = password_from_passfile(config, endpoint)?;
        let config = from_passfile.as_ref().unwrap_or(config);

        match connect_host(endpoint, resolver, tls, config, target_session_attrs).await {
            Ok((client, connection)) => return Ok((client, connection)),
            Err(e) if stops_host_walk(&e) => return Err(e),
            Err(e) => error = Some(e),
        }
    }

    Err(error.expect("endpoints rejects an empty host list"))
}

/// A copy of `config` carrying the password a password file supplies for this
/// endpoint, or `None` to use `config` unchanged.
///
/// `None` covers every case where the file has nothing to say: a password was
/// already configured, no file exists, the file is too permissive, or no line
/// matches. None of those is an error - libpq lets authentication fail on its
/// own terms rather than refusing to connect, and a driver that raised here
/// would reject setups libpq accepts.
///
/// Resolving libpq's default operating-system user can fail; that is returned
/// as an error, just as it is when startup resolves the same default.
fn password_from_passfile(config: &Config, endpoint: &Endpoint) -> Result<Option<Config>, Error> {
    if config.get_password().is_some() {
        return Ok(None);
    }

    // Only an explicitly configured file. libpq would fall back to
    // `$PGPASSFILE` and `~/.pgpass`; resolving those is the caller's job, for
    // the reason `passfile.rs` gives at length.
    let Some(path) = config.get_passfile() else {
        return Ok(None);
    };
    let path = Path::new(path);
    let user = match config.get_user() {
        Some(user) => Cow::Borrowed(user),
        None => Cow::Owned(whoami::username().map_err(|error| Error::io(error.into()))?),
    };
    // libpq defaults the database to the user, and matches the file on the
    // database it will actually connect to.
    let dbname = config.get_dbname().unwrap_or(&user);
    let host = endpoint.passfile_host();
    let port = endpoint.port().to_string();

    let Some(password) = passfile::lookup(
        path,
        passfile::PassfileKey {
            host: &host,
            port: &port,
            dbname,
            user: &user,
        },
    ) else {
        return Ok(None);
    };

    let mut with_password = config.clone();
    with_password.password(password);
    Ok(Some(with_password))
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
    target_session_attrs: TargetSessionAttrs,
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
            connect_once(
                addr,
                endpoint.hostname(),
                endpoint.port(),
                tls,
                config,
                target_session_attrs,
            ),
        )
        .await
        {
            Ok(stream) => return Ok(stream),
            // libpq treats the role as a property of this configured host: a
            // mismatch advances to the next host instead of trying another
            // address returned for this one.
            Err(e)
                if e.is_target_session_attrs()
                    || e.is_target_session_attrs_fatal()
                    || advances_to_next_host(&e)
                    || stops_host_walk(&e) =>
            {
                return Err(e);
            }
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

/// The name passed to a TLS backend for verification and SNI configuration.
/// A hostaddr-only endpoint uses its IP address; `has_hostname` remains a
/// separate policy bit so `verify-full` can still require an actual `host`.
pub(crate) fn tls_server_name(addr: &Addr, hostname: Option<&str>) -> String {
    hostname.map(str::to_owned).unwrap_or_else(|| match addr {
        Addr::Tcp(ip) => ip.to_string(),
        #[cfg(unix)]
        Addr::Unix(_) => String::new(),
    })
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
    target_session_attrs: TargetSessionAttrs,
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
        return connect_leg(
            &addr,
            hostname,
            port,
            tls,
            config,
            target_session_attrs,
            first,
        )
        .await;
    }

    let err = match connect_leg(
        &addr,
        hostname,
        port,
        tls,
        config,
        target_session_attrs,
        first,
    )
    .await
    {
        Ok(connected) => return Ok(connected),
        Err(e) => e,
    };

    // Four failure classes bypass `sslmode=allow`'s alternate TLS leg. A
    // target-session SQL/result rejection advances to the next configured
    // host, while a communication failure during that probe ends the whole
    // request. SQLSTATE 57P03 advances directly to the next configured host.
    // A local authentication failure goes straight to libpq's error_return
    // path. Other startup ErrorResponses do NOT appear here: libpq may retry
    // those under a different encryption method before treating the final
    // result as terminal, and the policy below preserves that behavior.
    if err.is_target_session_attrs()
        || err.is_target_session_attrs_fatal()
        || advances_to_next_host(&err)
        || err.is_authentication()
    {
        return Err(err);
    }

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
        // The connector cannot attest to a TLS parameter this config asks for,
        // so TLS-as-configured is unavailable through it - the same situation
        // as a failed handshake, reached before any bytes are sent.
        //
        // This arm is what the paragraph above says belongs here. The
        // attestation gate escalates `prefer` to chain verification the moment
        // `sslrootcert` is set, and the default `can_honor_server_verification`
        // attests to nothing, so WITHOUT this arm adding `sslrootcert` to a
        // working `prefer` DSN turns it into a hard error - which is precisely
        // the "refuse to downgrade once trust anchors were named" divergence
        // the comment above weighs and declines. It was never decided here; it
        // arrived from `connect_raw` as a side effect.
        //
        // `require` and the `verify-*` modes are unaffected: they have no
        // plaintext leg and fall through to the `_` arm below, so an
        // unattesting connector still fails them. That is what keeps this from
        // downgrading a guarantee anyone actually has.
        (SslMode::Prefer, Encryption::Tls) if err.is_tls_unattested() => Encryption::Plaintext,
        // Plaintext first, and it failed for any reason: dial again, with TLS.
        (SslMode::Allow, Encryption::Plaintext) => Encryption::Tls,
        // Every other mode has one transport in its allowed set, so there is
        // nothing to fall back to. This is libpq's structural guarantee, not a
        // check: `require`/`verify-ca`/`verify-full` never reach this arm with
        // a second option.
        _ => return Err(err),
    };

    connect_leg(
        &addr,
        hostname,
        port,
        tls,
        config,
        target_session_attrs,
        retry,
    )
    .await
}

/// One attempt: a fresh socket, one transport, one startup exchange.
async fn connect_leg<T>(
    addr: &Addr,
    hostname: Option<&str>,
    port: u16,
    tls: &mut T,
    config: &Config,
    target_session_attrs: TargetSessionAttrs,
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
        config.get_require_peer(),
    )
    .await?;

    let server_name = tls_server_name(&addr, hostname);
    let tls = tls
        .make_tls_connect(&server_name)
        .map_err(|e| Error::tls(e.into()))?;
    let has_hostname = hostname.is_some();
    // Taken while the socket is still a `Socket` - `connect_raw` is generic
    // over the stream and the TLS wrapper hides the descriptor.
    let release = socket.release_handle();
    let (mut client, connection, negotiated) = connect_raw_with_target_session_attrs(
        socket,
        tls,
        encryption,
        has_hostname,
        config,
        target_session_attrs,
        release,
    )
    .await?;

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
        require_peer: config.get_require_peer().map(str::to_owned),
        // Recorded, not re-derived at cancel time: see the field's doc.
        //
        // Keyed off what this session NEGOTIATED, not off `sslmode` alone. A
        // plaintext session has no certificate for anyone to verify, and
        // `ServerVerification::select` REFUSES `sslmode=disable` outright
        // ("does not use TLS, so no verification policy applies") - so asking
        // it unconditionally turns every `disable` connection into a TLS error.
        server_verification: if negotiated == crate::connect_tls::Encryption::Plaintext {
            crate::tls::ServerVerification::None
        } else {
            crate::tls::ServerVerification::demanded_by(
                config.get_ssl_mode(),
                config.get_ssl_root_cert(),
            )?
        },
        // `negotiated`, not `encryption`: the argument above is what this leg
        // ATTEMPTED. A cancel has to reproduce what the session got.
        encryption: negotiated,
    });

    Ok((client, connection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoTls;
    use crate::config::SslMode;
    use crate::tls::{MakeTlsConnect, NoTlsStream, TlsConnect};
    use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use compio::net::TcpListener;
    use futures_channel::oneshot;
    use std::collections::HashSet;
    use std::error::Error as _;
    use std::future;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const SSL_REQUEST_CODE: u32 = 80_877_103;

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

    #[test]
    fn passfile_uses_the_default_os_user_and_database() {
        use std::io::Write as _;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;

        let user = whoami::username().expect("look up the operating-system user");
        let mut passfile = tempfile::NamedTempFile::new().expect("create a password file");
        writeln!(passfile, "127.0.0.1:5432:{user}:{user}:from-passfile")
            .expect("write the password file");
        #[cfg(unix)]
        std::fs::set_permissions(passfile.path(), std::fs::Permissions::from_mode(0o600))
            .expect("make the password file private");

        let mut config = Config::new();
        config.passfile(passfile.path().to_string_lossy());
        let endpoint = Endpoint {
            target: EndpointTarget::Ip("127.0.0.1".parse().unwrap()),
            hostname: None,
            port: 5432,
        };

        let with_password = password_from_passfile(&config, &endpoint)
            .expect("look up the effective operating-system user")
            .expect("the effective user and database match the passfile");
        assert_eq!(
            with_password.get_password(),
            Some(b"from-passfile".as_slice())
        );
    }

    #[test]
    fn empty_hostaddr_slots_fall_back_to_the_corresponding_hosts() {
        let config = "host=first.example,second.example hostaddr=,127.0.0.2"
            .parse::<Config>()
            .expect("empty hostaddr slots are positional defaults");
        assert_eq!(
            config.get_hostaddrs(),
            [None, Some("127.0.0.2".parse().unwrap())]
        );

        let endpoints = endpoints(&config).expect("build the two positional endpoints");
        assert_eq!(endpoints.len(), 2);
        assert!(matches!(
            &endpoints[0].target,
            EndpointTarget::Name(host) if host == "first.example"
        ));
        assert_eq!(endpoints[0].hostname(), Some("first.example"));
        assert!(matches!(
            endpoints[1].target,
            EndpointTarget::Ip(ip) if ip == "127.0.0.2".parse::<IpAddr>().unwrap()
        ));
        assert_eq!(endpoints[1].hostname(), Some("second.example"));
    }

    #[test]
    fn empty_host_uses_hostaddr_for_tls_and_passfile_identity() {
        let config = "host='' hostaddr=127.0.0.3"
            .parse::<Config>()
            .expect("an empty host may be paired with hostaddr");
        let endpoint = endpoints(&config)
            .expect("hostaddr supplies the network target")
            .pop()
            .expect("one endpoint");

        assert!(matches!(
            &endpoint.target,
            EndpointTarget::Ip(ip) if *ip == "127.0.0.3".parse::<IpAddr>().unwrap()
        ));
        assert_eq!(endpoint.hostname(), None);
        assert_eq!(endpoint.passfile_host(), "127.0.0.3");
    }

    #[test]
    fn empty_default_host_is_refused_instead_of_resolved_as_an_empty_name() {
        let config = "host=''"
            .parse::<Config>()
            .expect("libpq syntax permits its default host slot");
        let error = match endpoints(&config) {
            Ok(_) => panic!("an empty host was sent to DNS instead of being refused"),
            Err(error) => error,
        };

        let mut text = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            text.push_str(" | ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        assert!(
            text.contains("hostaddr entry 1") && text.contains("compiled default Unix socket"),
            "the intentional default-host refusal was not clear: {text}"
        );
    }

    fn refused_handshake() -> Vec<u8> {
        frame(b'E', b"SERROR\0C57P03\0Mscripted refusal\0\0")
    }

    fn invalid_password_handshake() -> Vec<u8> {
        frame(
            b'E',
            b"SFATAL\0C28P01\0Mscripted authentication failure\0\0",
        )
    }

    fn cleartext_password_challenge() -> Vec<u8> {
        frame(b'R', &3u32.to_be_bytes())
    }

    enum ProbeReply {
        Close,
        CloseThenTls(oneshot::Sender<u32>),
        ErrorThenTls(oneshot::Sender<u32>),
        Stall,
        TransactionReadOnly(bool),
        Recovery(bool),
    }

    fn probe_result(column: &[u8], value: &str, command_complete: &[u8]) -> Vec<u8> {
        let mut row_description = 1u16.to_be_bytes().to_vec();
        row_description.extend_from_slice(column);
        row_description.push(0);
        row_description.extend_from_slice(&0u32.to_be_bytes());
        row_description.extend_from_slice(&0i16.to_be_bytes());
        row_description.extend_from_slice(&25u32.to_be_bytes());
        row_description.extend_from_slice(&(-1i16).to_be_bytes());
        row_description.extend_from_slice(&(-1i32).to_be_bytes());
        row_description.extend_from_slice(&0i16.to_be_bytes());

        let mut data_row = 1u16.to_be_bytes().to_vec();
        data_row.extend_from_slice(&i32::try_from(value.len()).unwrap().to_be_bytes());
        data_row.extend_from_slice(value.as_bytes());

        let mut script = frame(b'T', &row_description);
        script.extend_from_slice(&frame(b'D', &data_row));
        script.extend_from_slice(&frame(b'C', command_complete));
        script.extend_from_slice(&frame(b'Z', b"I"));
        script
    }

    async fn scripted_probe_server(reply: ProbeReply) -> (SocketAddr, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (query_seen, query_observed) = oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
            result.unwrap();

            let compio::BufResult(result, _) = socket.write_all(successful_handshake()).await;
            result.unwrap();
            socket.flush().await.unwrap();

            let compio::BufResult(result, tag) = socket.read_exact(vec![0u8; 1]).await;
            if result.is_err() {
                return;
            }
            assert_eq!(tag, b"Q");
            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            if result.is_err() {
                return;
            }
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, query) = socket.read_exact(vec![0u8; length - 4]).await;
            if result.is_err() {
                return;
            }
            let _ = query_seen.send(query);

            match reply {
                ProbeReply::Close => {}
                ProbeReply::CloseThenTls(opening_seen) => {
                    drop(socket);

                    let (mut socket, _) = listener.accept().await.unwrap();
                    let compio::BufResult(result, opening) = socket.read_exact(vec![0u8; 8]).await;
                    result.unwrap();
                    assert_eq!(u32::from_be_bytes(opening[..4].try_into().unwrap()), 8);
                    let code = u32::from_be_bytes(opening[4..].try_into().unwrap());
                    let _ = opening_seen.send(code);

                    let compio::BufResult(result, _) = socket.write_all(vec![b'S']).await;
                    result.unwrap();
                    socket.flush().await.unwrap();
                }
                ProbeReply::ErrorThenTls(opening_seen) => {
                    let reply = frame(
                        b'E',
                        b"SERROR\0CXX000\0Mscripted target-session probe failure\0\0",
                    );
                    let compio::BufResult(result, _) = socket.write_all(reply).await;
                    result.unwrap();
                    socket.flush().await.unwrap();
                    drop(socket);

                    let (mut socket, _) = listener.accept().await.unwrap();
                    let compio::BufResult(result, opening) = socket.read_exact(vec![0u8; 8]).await;
                    result.unwrap();
                    assert_eq!(u32::from_be_bytes(opening[..4].try_into().unwrap()), 8);
                    let code = u32::from_be_bytes(opening[4..].try_into().unwrap());
                    let _ = opening_seen.send(code);

                    let compio::BufResult(result, _) = socket.write_all(vec![b'S']).await;
                    result.unwrap();
                    socket.flush().await.unwrap();
                }
                ProbeReply::Stall => {
                    let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
                }
                ProbeReply::TransactionReadOnly(read_only) => {
                    let value = if read_only { "on" } else { "off" };
                    let reply = probe_result(b"transaction_read_only", value, b"SHOW\0");
                    let compio::BufResult(result, _) = socket.write_all(reply).await;
                    result.unwrap();
                    socket.flush().await.unwrap();
                    let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
                }
                ProbeReply::Recovery(in_recovery) => {
                    let value = if in_recovery { "t" } else { "f" };
                    let reply = probe_result(b"pg_is_in_recovery", value, b"SELECT 1\0");
                    let compio::BufResult(result, _) = socket.write_all(reply).await;
                    result.unwrap();
                    socket.flush().await.unwrap();
                    let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
                }
            }
        })
        .detach();

        (addr, query_observed)
    }

    /// A primary-only endpoint that accepts exactly the three connections a
    /// correct two-host `prefer-standby` fallback makes. The first two report
    /// that they are not in recovery; the third completes startup for the
    /// `any` pass.
    async fn scripted_prefer_standby_fallback_server()
    -> (SocketAddr, oneshot::Receiver<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (fallback_seen, fallback_observed) = oneshot::channel();

        compio::runtime::spawn(async move {
            let mut first_pass_queries = Vec::with_capacity(2);
            for _ in 0..2 {
                let (mut candidate, _) = listener.accept().await.unwrap();

                let compio::BufResult(result, length) = candidate.read_exact(vec![0u8; 4]).await;
                result.unwrap();
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, _) =
                    candidate.read_exact(vec![0u8; length - 4]).await;
                result.unwrap();

                let compio::BufResult(result, _) =
                    candidate.write_all(successful_handshake()).await;
                result.unwrap();
                candidate.flush().await.unwrap();

                let compio::BufResult(result, tag) = candidate.read_exact(vec![0u8; 1]).await;
                result.unwrap();
                assert_eq!(tag, b"Q");
                let compio::BufResult(result, length) = candidate.read_exact(vec![0u8; 4]).await;
                result.unwrap();
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, query) =
                    candidate.read_exact(vec![0u8; length - 4]).await;
                result.unwrap();
                first_pass_queries.push(query);

                let reply = probe_result(b"pg_is_in_recovery", "f", b"SELECT 1\0");
                let compio::BufResult(result, _) = candidate.write_all(reply).await;
                result.unwrap();
                candidate.flush().await.unwrap();
            }

            let (mut fallback, _) = listener.accept().await.unwrap();
            let compio::BufResult(result, length) = fallback.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, _) = fallback.read_exact(vec![0u8; length - 4]).await;
            result.unwrap();

            let compio::BufResult(result, _) = fallback.write_all(successful_handshake()).await;
            result.unwrap();
            fallback.flush().await.unwrap();
            let _ = fallback_seen.send(first_pass_queries);

            let compio::BufResult(_, _) = fallback.read(vec![0u8; 1]).await;
        })
        .detach();

        (addr, fallback_observed)
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

            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
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

    #[cfg(target_os = "linux")]
    struct TempSocketDir(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl TempSocketDir {
        fn create() -> TempSocketDir {
            static NEXT_DIR: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);

            // Keep this below Linux's 108-byte sockaddr_un limit even when the
            // repository lives under a long worktree path.
            let path = std::path::PathBuf::from("/tmp").join(format!(
                "cpg-peer-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create temporary socket directory");
            TempSocketDir(path)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TempSocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Accept the rejected socket first and prove it receives zero PostgreSQL
    /// bytes, then complete a real startup exchange on the matching attempt.
    #[cfg(target_os = "linux")]
    async fn scripted_requirepeer_server() -> (TempSocketDir, compio::runtime::JoinHandle<()>) {
        let socket_dir = TempSocketDir::create();
        let socket_path = socket_dir.0.join(".s.PGSQL.5432");
        let listener = compio::net::UnixListener::bind(&socket_path)
            .await
            .expect("bind scripted Unix server");
        let server = compio::runtime::spawn(async move {
            let (mut rejected, _) = listener.accept().await.expect("accept rejected peer");
            let compio::BufResult(result, _) = rejected.read(vec![0u8; 1]).await;
            assert_eq!(
                result.expect("read rejected peer"),
                0,
                "requirepeer mismatch sent PostgreSQL bytes before checking SO_PEERCRED"
            );

            let (mut accepted, _) = listener.accept().await.expect("accept matching peer");
            let compio::BufResult(result, length) = accepted.read_exact(vec![0u8; 4]).await;
            result.expect("read matching startup length");
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            assert!(length >= 8, "startup packet is too short: {length}");
            let compio::BufResult(result, _) = accepted.read_exact(vec![0u8; length - 4]).await;
            result.expect("read matching startup packet");
            let compio::BufResult(result, _) = accepted.write_all(successful_handshake()).await;
            result.expect("write matching startup response");
            accepted.flush().await.expect("flush startup response");
        });

        (socket_dir, server)
    }

    /// The pair exercises real Linux peer credentials through the complete
    /// Config connection path. The wrong name is rejected before StartupMessage;
    /// changing only that name to the socket owner's canonical OS name connects.
    #[cfg(target_os = "linux")]
    #[compio::test]
    async fn requirepeer_matches_the_unix_server_process_owner() {
        let (socket_dir, server) = scripted_requirepeer_server().await;
        let actual = nix::unistd::User::from_uid(nix::unistd::Uid::effective())
            .expect("look up current operating-system user")
            .expect("the current user ID has no passwd-database entry")
            .name;
        let wrong = format!("{actual}-definitely-not-the-peer");

        let mut rejected = Config::new();
        rejected
            .user("scripted-user")
            .host_path(&socket_dir.0)
            .port(5432)
            .ssl_mode(SslMode::Disable)
            .require_peer(&wrong);
        let outcome = compio::time::timeout(Duration::from_secs(2), rejected.connect(NoTls))
            .await
            .expect("wrong requirepeer check timed out");
        let error = match outcome {
            Ok(_) => panic!("a wrong Unix peer user connected"),
            Err(error) => error,
        };
        let cause = error.source().map(ToString::to_string).unwrap_or_default();
        assert!(
            cause.contains("requirepeer") && cause.contains(&wrong) && cause.contains(&actual),
            "the mismatch did not name both peer users: {error}: {cause}"
        );

        let mut accepted = Config::new();
        accepted
            .user("scripted-user")
            .host_path(&socket_dir.0)
            .port(5432)
            .ssl_mode(SslMode::Disable)
            .require_peer(&actual);
        let connected = compio::time::timeout(Duration::from_secs(2), accepted.connect(NoTls))
            .await
            .expect("matching requirepeer connection timed out")
            .expect("the Unix server's actual owner must connect");
        drop(connected);

        compio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("scripted requirepeer server timed out")
            .expect("scripted requirepeer server panicked");
    }

    /// libpq gates `requirepeer` on AF_UNIX. A value that is certainly wrong
    /// must therefore remain a no-op on TCP, not become a cross-transport
    /// policy the reference client never promised.
    #[compio::test]
    async fn requirepeer_is_ignored_on_tcp_like_libpq() {
        let (addr, startup_observed) =
            scripted_server_after_startup(Some(successful_handshake())).await;
        let mut config = config_for(addr, Duration::from_secs(2));
        config.require_peer("definitely-not-the-tcp-peer");

        let connected = compio::time::timeout(Duration::from_secs(5), config.connect(NoTls))
            .await
            .expect("TCP requirepeer control timed out")
            .expect("libpq ignores requirepeer on TCP");
        startup_observed
            .await
            .expect("TCP server did not receive StartupMessage");
        drop(connected);
    }

    /// Accept one PostgreSQL SSLRequest, agree to TLS, and leave the
    /// connector's scripted handshake failure to end the address attempt.
    async fn tls_handshake_server_bound(bind: SocketAddr) -> (SocketAddr, oneshot::Receiver<u32>) {
        let listener = TcpListener::bind(bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (opening_seen, opening_observed) = oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let compio::BufResult(result, opening) = socket.read_exact(vec![0u8; 8]).await;
            result.unwrap();
            assert_eq!(u32::from_be_bytes(opening[..4].try_into().unwrap()), 8);
            let code = u32::from_be_bytes(opening[4..].try_into().unwrap());
            let _ = opening_seen.send(code);

            let compio::BufResult(result, _) = socket.write_all(vec![b'S']).await;
            result.unwrap();
            socket.flush().await.unwrap();
        })
        .detach();

        (addr, opening_observed)
    }

    /// Advertises a usable TLS implementation, then fails after the server
    /// accepts the SSLRequest. The stream type is uninhabited because a
    /// successful test handshake would be a fixture bug.
    struct HandshakeFailingTls;

    struct RecordingDomainTls {
        domains: Arc<Mutex<Vec<String>>>,
    }

    impl<S> MakeTlsConnect<S> for RecordingDomainTls {
        type Stream = NoTlsStream;
        type TlsConnect = HandshakeFailingTls;
        type Error = io::Error;

        fn make_tls_connect(&mut self, domain: &str) -> Result<Self::TlsConnect, Self::Error> {
            self.domains.lock().unwrap().push(domain.to_owned());
            Ok(HandshakeFailingTls)
        }
    }

    impl<S> MakeTlsConnect<S> for HandshakeFailingTls {
        type Stream = NoTlsStream;
        type TlsConnect = HandshakeFailingTls;
        type Error = io::Error;

        fn make_tls_connect(&mut self, _domain: &str) -> Result<Self::TlsConnect, Self::Error> {
            Ok(HandshakeFailingTls)
        }
    }

    impl<S> TlsConnect<S> for HandshakeFailingTls {
        type Stream = NoTlsStream;
        type Error = io::Error;
        type Future = future::Ready<Result<NoTlsStream, io::Error>>;

        fn connect(self, _stream: S) -> Self::Future {
            future::ready(Err(io::Error::other("scripted TLS handshake failure")))
        }
    }

    #[compio::test]
    async fn hostaddr_only_tls_uses_the_ip_as_the_connector_server_name() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; 8]).await;
            if result.is_ok() {
                let compio::BufResult(result, _) = socket.write_all(vec![b'S']).await;
                result.unwrap();
                socket.flush().await.unwrap();
            }
        });

        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(addr.ip())
            .port(addr.port())
            .ssl_mode(SslMode::Require)
            .connect_timeout(Duration::from_secs(2));
        let domains = Arc::new(Mutex::new(Vec::new()));
        let outcome = config
            .connect(RecordingDomainTls {
                domains: domains.clone(),
            })
            .await;
        assert!(outcome.is_err(), "the scripted TLS handshake must fail");
        assert_eq!(domains.lock().unwrap().as_slice(), [addr.ip().to_string()]);
        server.await.expect("scripted server panicked");
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

    struct RecordingResolver {
        address: SocketAddr,
        calls: Vec<(String, u16)>,
    }

    impl Resolver for RecordingResolver {
        async fn resolve(&mut self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.calls.push((host.to_owned(), port));
            Ok(vec![self.address])
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
    async fn resolver_receives_endpoint_hostname_and_port() {
        let mut config = Config::new();
        config
            .host("resolver-argument.example")
            .port(6543)
            .ssl_mode(SslMode::Disable);
        let endpoint = endpoints(&config)
            .expect("one hostname is a valid endpoint list")
            .pop()
            .expect("the endpoint list contains the hostname");
        let mut resolver = RecordingResolver {
            address: "127.0.0.9:6543".parse().unwrap(),
            calls: Vec::new(),
        };

        endpoint
            .addresses(&mut resolver, LoadBalanceHosts::Disable)
            .await
            .expect("the recording resolver returned one address");

        assert_eq!(
            resolver.calls,
            vec![("resolver-argument.example".to_owned(), 6543)],
            "Endpoint::addresses must pass its configured hostname and port unchanged"
        );
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
    async fn connection_death_during_target_session_attrs_probe_is_an_error() {
        let (addr, query_observed) = scripted_probe_server(ProbeReply::Close).await;
        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(addr.ip())
            .port(addr.port())
            .ssl_mode(SslMode::Disable)
            .target_session_attrs(TargetSessionAttrs::ReadWrite);

        let connect = compio::runtime::spawn(async move { config.connect(NoTls).await });
        let query = compio::time::timeout(Duration::from_secs(2), query_observed)
            .await
            .expect("the target-session probe was never sent")
            .expect("the connection closed without the target-session probe");
        assert_eq!(query, b"SHOW transaction_read_only\0");

        let result = compio::time::timeout(Duration::from_secs(2), connect)
            .await
            .expect("connection death during the probe hung connect")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert!(
            result.is_err(),
            "a connection that died without answering the probe was returned"
        );
    }

    #[compio::test]
    async fn probe_sql_error_skips_transport_and_address_before_next_host() {
        let (tls_seen, mut tls_observed) = oneshot::channel();
        let (first, first_query_observed) =
            scripted_probe_server(ProbeReply::ErrorThenTls(tls_seen)).await;
        let sibling_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (sibling, mut sibling_startup_observed) =
            scripted_server_bound(sibling_bind, Some(successful_handshake())).await;
        let (second, second_query_observed) =
            scripted_probe_server(ProbeReply::TransactionReadOnly(false)).await;

        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("probe-error.example")
            .host("healthy.example")
            .port(first.port())
            .port(second.port())
            .ssl_mode(SslMode::Allow)
            .target_session_attrs(TargetSessionAttrs::ReadWrite)
            .connect_timeout(Duration::from_secs(2));
        let mut resolver = ProbeRoutingResolver {
            first: vec![first, sibling],
            second: vec![second],
        };

        let connected = compio::time::timeout(
            Duration::from_secs(5),
            connect_with_resolver(HandshakeFailingTls, &config, &mut resolver),
        )
        .await
        .expect("the probe SQL error host walk hung")
        .expect("a probe SQL error did not advance to the healthy configured host");

        assert_eq!(
            first_query_observed
                .await
                .expect("the first host closed without receiving the probe"),
            b"SHOW transaction_read_only\0"
        );
        assert_eq!(
            second_query_observed
                .await
                .expect("the healthy host closed without receiving the probe"),
            b"SHOW transaction_read_only\0"
        );
        assert!(
            tls_observed
                .try_recv()
                .expect("the first-host fixture disappeared")
                .is_none(),
            "a probe SQL error retried another transport on the same address"
        );
        assert!(
            sibling_startup_observed
                .try_recv()
                .expect("the sibling-address fixture disappeared")
                .is_none(),
            "a probe SQL error retried another address for the same configured host"
        );
        drop(connected);
    }

    #[compio::test]
    async fn probe_transport_failure_stops_every_retry_path() {
        let (tls_seen, mut tls_observed) = oneshot::channel();
        let (first, query_observed) =
            scripted_probe_server(ProbeReply::CloseThenTls(tls_seen)).await;
        let sibling_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (sibling, mut sibling_startup_observed) =
            scripted_server_bound(sibling_bind, Some(successful_handshake())).await;
        let (second, mut second_query_observed) =
            scripted_probe_server(ProbeReply::TransactionReadOnly(false)).await;
        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("broken-probe.example")
            .host("healthy.example")
            .port(first.port())
            .port(second.port())
            .ssl_mode(SslMode::Allow)
            .target_session_attrs(TargetSessionAttrs::ReadWrite)
            .connect_timeout(Duration::from_secs(2));
        let mut resolver = ProbeRoutingResolver {
            first: vec![first, sibling],
            second: vec![second],
        };

        let result = compio::time::timeout(
            Duration::from_secs(5),
            connect_with_resolver(HandshakeFailingTls, &config, &mut resolver),
        )
        .await
        .expect("the probe transport failure host walk hung");
        let query = compio::time::timeout(Duration::from_secs(2), query_observed)
            .await
            .expect("the plaintext target-session probe was never sent")
            .expect("the plaintext connection closed before the target-session probe");
        assert_eq!(query, b"SHOW transaction_read_only\0");

        let Err(error) = result else {
            panic!("a probe transport failure advanced to the healthy configured host");
        };
        assert!(
            error.is_target_session_attrs_fatal(),
            "the probe transport failure was not globally terminal: {error:?}"
        );
        assert!(
            tls_observed
                .try_recv()
                .expect("the first-host fixture disappeared")
                .is_none(),
            "a probe transport failure retried another transport"
        );
        assert!(
            sibling_startup_observed
                .try_recv()
                .expect("the sibling-address fixture disappeared")
                .is_none(),
            "a probe transport failure retried another address"
        );
        assert!(
            second_query_observed
                .try_recv()
                .expect("the healthy-host fixture disappeared")
                .is_none(),
            "a probe transport failure retried another configured host"
        );
    }

    #[compio::test]
    async fn connect_timeout_covers_a_stalled_target_session_attrs_probe() {
        let (addr, query_observed) = scripted_probe_server(ProbeReply::Stall).await;
        let mut config = config_for(addr, Duration::from_millis(100));
        config.target_session_attrs(TargetSessionAttrs::ReadWrite);
        let connect = compio::runtime::spawn(async move { config.connect(NoTls).await });

        let query = compio::time::timeout(Duration::from_secs(2), query_observed)
            .await
            .expect("the target-session probe was never sent")
            .expect("the connection closed without the target-session probe");
        assert_eq!(query, b"SHOW transaction_read_only\0");

        let result = compio::time::timeout(Duration::from_secs(2), connect)
            .await
            .expect("the outer watchdog expired because the probe escaped connect_timeout")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let error = match result {
            Ok(_) => panic!("a server that never answered the probe produced a connection"),
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
    async fn connect_timeout_covers_a_stalled_recovery_probe() {
        let (addr, query_observed) = scripted_probe_server(ProbeReply::Stall).await;
        let mut config = config_for(addr, Duration::from_millis(100));
        config.target_session_attrs(TargetSessionAttrs::Primary);
        let connect = compio::runtime::spawn(async move { config.connect(NoTls).await });

        let query = compio::time::timeout(Duration::from_secs(2), query_observed)
            .await
            .expect("the recovery probe was never sent")
            .expect("the connection closed without the recovery probe");
        assert_eq!(query, b"SELECT pg_catalog.pg_is_in_recovery()\0");

        let result = compio::time::timeout(Duration::from_secs(2), connect)
            .await
            .expect("the outer watchdog expired because the recovery probe escaped connect_timeout")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let Err(error) = result else {
            panic!("a server that never answered the recovery probe connected");
        };
        let io = error
            .source()
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("connection timeout must retain its I/O cause");
        assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(io.to_string(), "connection timed out");
    }

    #[compio::test]
    async fn prefer_standby_exhausts_the_host_list_before_the_any_pass() {
        let (addr, fallback_observed) = scripted_prefer_standby_fallback_server().await;
        let mut config = config_for(addr, Duration::from_secs(2));
        config
            .hostaddr(addr.ip())
            .port(addr.port())
            .target_session_attrs(TargetSessionAttrs::PreferStandby);

        let connected = compio::time::timeout(Duration::from_secs(5), config.connect(NoTls))
            .await
            .expect("the two-pass connection walk hung")
            .expect("the any-mode pass did not accept the primary endpoint");
        let first_pass_queries = compio::time::timeout(Duration::from_secs(2), fallback_observed)
            .await
            .expect("the server never observed the any-mode fallback connection")
            .expect("the server closed before reporting the first-pass queries");

        let expected = b"SELECT pg_catalog.pg_is_in_recovery()\0".to_vec();
        assert_eq!(
            first_pass_queries,
            vec![expected.clone(), expected],
            "the any-mode pass began before both configured hosts failed the standby check"
        );
        drop(connected);
    }

    #[compio::test]
    async fn startup_fatal_stops_the_configured_host_walk() {
        let (first, first_seen) =
            scripted_server_after_startup(Some(invalid_password_handshake())).await;
        let (second, mut second_seen) =
            scripted_server_after_startup(Some(successful_handshake())).await;
        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(first.ip())
            .hostaddr(second.ip())
            .port(first.port())
            .port(second.port())
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_secs(2));

        let result = compio::time::timeout(Duration::from_secs(5), config.connect(NoTls))
            .await
            .expect("the fatal-startup connection hung");
        let Err(error) = result else {
            panic!("an authentication FATAL advanced to the healthy second host");
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("28P01")
        );
        first_seen
            .await
            .expect("the first host never received StartupMessage");
        assert!(
            second_seen
                .try_recv()
                .expect("the second-host fixture disappeared")
                .is_none(),
            "an authentication FATAL dialled the second configured host"
        );
    }

    #[compio::test]
    async fn client_authentication_failure_stops_the_configured_host_walk() {
        let (first, first_seen) =
            scripted_server_after_startup(Some(cleartext_password_challenge())).await;
        let (second, mut second_seen) =
            scripted_server_after_startup(Some(successful_handshake())).await;
        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(first.ip())
            .hostaddr(second.ip())
            .port(first.port())
            .port(second.port())
            .ssl_mode(SslMode::Allow)
            .connect_timeout(Duration::from_secs(2));

        let result = compio::time::timeout(Duration::from_secs(5), config.connect(NoTls))
            .await
            .expect("the client-authentication failure hung");
        let Err(error) = result else {
            panic!("a missing password advanced to the healthy second host");
        };
        assert_eq!(error.to_string(), "authentication error");
        first_seen
            .await
            .expect("the first host never received StartupMessage");
        assert!(
            second_seen
                .try_recv()
                .expect("the second-host fixture disappeared")
                .is_none(),
            "a client-side authentication failure dialled the second configured host"
        );
    }

    #[compio::test]
    async fn prefer_standby_does_not_start_pass_two_after_a_startup_fatal() {
        let (primary, mut fallback_observed) = scripted_prefer_standby_fallback_server().await;
        let (fatal, fatal_seen) =
            scripted_server_after_startup(Some(invalid_password_handshake())).await;
        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(primary.ip())
            .hostaddr(primary.ip())
            .hostaddr(fatal.ip())
            .port(primary.port())
            .port(primary.port())
            .port(fatal.port())
            .ssl_mode(SslMode::Disable)
            .target_session_attrs(TargetSessionAttrs::PreferStandby)
            .connect_timeout(Duration::from_secs(2));

        let result = compio::time::timeout(Duration::from_secs(5), config.connect(NoTls))
            .await
            .expect("the prefer-standby connection walk hung");
        let Err(error) = result else {
            panic!("prefer-standby ignored a FATAL and connected during its any-host pass");
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("28P01")
        );
        fatal_seen
            .await
            .expect("the fatal endpoint never received StartupMessage");
        assert!(
            fallback_observed
                .try_recv()
                .expect("the primary-host fixture disappeared")
                .is_none(),
            "prefer-standby began its any-host pass after a FATAL"
        );
    }

    #[compio::test]
    async fn target_mismatch_skips_other_addresses_for_the_same_host() {
        let (first, first_query_observed) =
            scripted_probe_server(ProbeReply::Recovery(false)).await;
        let second_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (second, mut second_startup_observed) =
            scripted_server_bound(second_bind, Some(successful_handshake())).await;
        let mut config = hostname_config_for(first, Duration::from_secs(2));
        config.target_session_attrs(TargetSessionAttrs::Standby);
        let mut resolver = ListResolver(vec![first, second]);

        let Err(error) = connect_with_resolver(NoTls, &config, &mut resolver).await else {
            panic!("a primary satisfied target_session_attrs=standby");
        };
        let io = error
            .source()
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("target mismatch must retain its I/O cause");
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(io.to_string(), "database server is not in recovery");

        let query = first_query_observed
            .await
            .expect("the first address closed without the recovery probe");
        assert_eq!(query, b"SELECT pg_catalog.pg_is_in_recovery()\0");
        assert!(
            second_startup_observed
                .try_recv()
                .expect("the second-address fixture disappeared")
                .is_none(),
            "a target mismatch tried another address for the same host"
        );
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

    struct ProbeRoutingResolver {
        first: Vec<SocketAddr>,
        second: Vec<SocketAddr>,
    }

    impl Resolver for ProbeRoutingResolver {
        async fn resolve(&mut self, host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            match host {
                "probe-error.example" | "broken-probe.example" => Ok(self.first.clone()),
                "healthy.example" => Ok(self.second.clone()),
                _ => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unexpected scripted host {host}"),
                )),
            }
        }
    }

    /// Four entries have 24 possible permutations. If the shuffle is correct
    /// and uniform, the chance that all 64 trials produce the same ordering is
    /// 24^-63, approximately 1.11e-87.
    #[compio::test]
    async fn random_load_balance_shuffles_resolved_addresses() {
        let mut config = Config::new();
        config
            .host("random-addresses.example")
            .ssl_mode(SslMode::Disable);
        let endpoint = endpoints(&config)
            .expect("one hostname is a valid endpoint list")
            .pop()
            .expect("the endpoint list contains the hostname");
        let addresses = vec![
            "127.0.0.1:5432".parse().unwrap(),
            "127.0.0.2:5432".parse().unwrap(),
            "127.0.0.3:5432".parse().unwrap(),
            "127.0.0.4:5432".parse().unwrap(),
        ];
        let mut orders = HashSet::new();

        for _ in 0..64 {
            let mut resolver = ListResolver(addresses.clone());
            let order = endpoint
                .addresses(&mut resolver, LoadBalanceHosts::Random)
                .await
                .expect("the resolver returned four addresses")
                .into_iter()
                .map(|addr| match addr {
                    Addr::Tcp(ip) => ip,
                    #[cfg(unix)]
                    Addr::Unix(path) => {
                        panic!(
                            "a hostname resolver returned a Unix path: {}",
                            path.display()
                        )
                    }
                })
                .collect::<Vec<_>>();
            orders.insert(order);
        }

        assert!(
            orders.len() > 1,
            "LoadBalanceHosts::Random never changed the resolved-address order in 64 trials"
        );
    }

    /// The endpoint shuffle is separate from the address shuffle above and
    /// therefore needs its own observation. It has the same 24^-63 false-fail
    /// probability across four entries and 64 trials.
    #[test]
    fn random_load_balance_shuffles_configured_endpoints() {
        let mut config = Config::new();
        for i in 0..4 {
            config
                .host(format!("random-endpoint-{i}.example"))
                .port(6000 + i);
        }
        config
            .ssl_mode(SslMode::Disable)
            .load_balance_hosts(LoadBalanceHosts::Random);
        let mut orders = HashSet::new();

        for _ in 0..64 {
            let order = endpoints(&config)
                .expect("four hostname/port pairs are valid endpoints")
                .into_iter()
                .map(|endpoint| {
                    (
                        endpoint
                            .hostname()
                            .expect("configured TCP host has a hostname")
                            .to_owned(),
                        endpoint.port(),
                    )
                })
                .collect::<Vec<_>>();
            orders.insert(order);
        }

        assert!(
            orders.len() > 1,
            "LoadBalanceHosts::Random never changed the endpoint order in 64 trials"
        );
    }

    /// A TLS-handshake failure on one resolved address must not stop the
    /// address walk. Both probes report the opening protocol code so this
    /// proves the test traversed TLS legs rather than plaintext sockets.
    #[compio::test]
    async fn tls_failure_advances_to_second_resolved_address() {
        let (first, first_opening) =
            tls_handshake_server_bound("127.0.0.1:0".parse().unwrap()).await;
        let second_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (second, second_opening) = tls_handshake_server_bound(second_bind).await;

        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("scripted.example")
            .port(first.port())
            .ssl_mode(SslMode::Require);
        let mut resolver = ListResolver(vec![first, second]);

        let result = compio::time::timeout(
            Duration::from_secs(5),
            connect_with_resolver(HandshakeFailingTls, &config, &mut resolver),
        )
        .await
        .expect("the TLS address walk hung");
        assert!(result.is_err(), "both scripted TLS handshakes fail");

        async fn opening_code(seen: oneshot::Receiver<u32>) -> u32 {
            compio::time::timeout(Duration::from_secs(2), seen)
                .await
                .expect("the resolved address was never dialled")
                .expect("the TLS probe closed without reporting its opening message")
        }

        assert_eq!(opening_code(first_opening).await, SSL_REQUEST_CODE);
        assert_eq!(
            opening_code(second_opening).await,
            SSL_REQUEST_CODE,
            "the first TLS failure stopped the resolved-address walk"
        );
    }

    /// A transport failure on the first address does not merely make the walk
    /// dial address two: a healthy second PostgreSQL server must be allowed to
    /// win the walk.
    #[compio::test]
    async fn connect_succeeds_via_second_resolved_address() {
        let (first, first_seen) = scripted_server_after_startup(Some(Vec::new())).await;
        let second_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (second, second_seen) =
            scripted_server_bound(second_bind, Some(successful_handshake())).await;
        let config = hostname_config_for(first, Duration::from_secs(5));
        let mut resolver = ListResolver(vec![first, second]);

        let (client, connection) = compio::time::timeout(
            Duration::from_secs(10),
            connect_with_resolver(NoTls, &config, &mut resolver),
        )
        .await
        .expect("the resolved-address walk hung")
        .expect("the healthy second address must complete the connection");

        async fn startup_seen(seen: oneshot::Receiver<()>, address: &str) {
            compio::time::timeout(Duration::from_secs(2), seen)
                .await
                .unwrap_or_else(|_| panic!("the {address} server was never dialled"))
                .unwrap_or_else(|_| {
                    panic!("the {address} server closed without observing startup")
                });
        }

        startup_seen(first_seen, "first").await;
        startup_seen(second_seen, "healthy second").await;
        let socket_config = client
            .cancel_token()
            .socket_config
            .expect("a connected client records its socket address");
        assert!(
            matches!(socket_config.addr, Addr::Tcp(ip) if ip == second.ip()),
            "the returned client must record the healthy second address"
        );
        drop((client, connection));
    }

    #[compio::test]
    async fn cannot_connect_now_skips_other_addresses_for_the_same_host() {
        let (first, first_seen) = scripted_server_after_startup(Some(refused_handshake())).await;
        let second_bind = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let (_second, mut second_seen) =
            scripted_server_bound(second_bind, Some(successful_handshake())).await;
        let mut config = hostname_config_for(first, Duration::from_secs(5));
        config.ssl_mode(SslMode::Allow);
        let mut resolver = ListResolver(vec![first, second_bind]);

        let result = compio::time::timeout(
            Duration::from_secs(10),
            connect_with_resolver(NoTls, &config, &mut resolver),
        )
        .await
        .expect("the 57P03 address walk hung");
        let Err(error) = result else {
            panic!("57P03 advanced to another address for the same host");
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P03")
        );
        first_seen
            .await
            .expect("the first address never received StartupMessage");
        assert!(
            second_seen
                .try_recv()
                .expect("the second-address fixture disappeared")
                .is_none(),
            "57P03 dialled another address for the same configured host"
        );
    }

    #[compio::test]
    async fn cannot_connect_now_advances_to_the_next_configured_host() {
        let (first, first_seen) = scripted_server_after_startup(Some(refused_handshake())).await;
        let (second, second_seen) =
            scripted_server_after_startup(Some(successful_handshake())).await;
        let mut config = Config::new();
        config
            .user("scripted-user")
            .hostaddr(first.ip())
            .hostaddr(second.ip())
            .port(first.port())
            .port(second.port())
            .ssl_mode(SslMode::Allow)
            .connect_timeout(Duration::from_secs(2));

        let connected = compio::time::timeout(Duration::from_secs(5), config.connect(NoTls))
            .await
            .expect("the configured-host failover hung")
            .expect("57P03 must advance to the next configured host");
        first_seen
            .await
            .expect("the first host never received StartupMessage");
        second_seen
            .await
            .expect("the healthy second host never received StartupMessage");
        drop(connected);
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
