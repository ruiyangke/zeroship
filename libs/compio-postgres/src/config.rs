// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Connection configuration.

#![allow(clippy::doc_overindented_list_items)]

use crate::Socket;
use crate::connect::{connect, with_connect_timeout};
use crate::connect_raw::connect_raw;
use crate::connect_tls::Encryption;
#[cfg(not(target_arch = "wasm32"))]
use crate::keepalive::KeepaliveConfig;
use crate::tls::MakeTlsConnect;
use crate::tls::TlsConnect;
use crate::{Client, Connection, Error};
use compio::io::{AsyncRead, AsyncWrite};
use std::borrow::Cow;
#[cfg(unix)]
use std::ffi::OsStr;
use std::net::IpAddr;
use std::ops::Deref;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::str;
use std::str::FromStr;
use std::time::Duration;
use std::{error, fmt, iter, mem};

/// Properties required of a session.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetSessionAttrs {
    /// No special properties are required.
    Any,
    /// The session must accept read-write transactions by default.
    ReadWrite,
    /// The session must not accept read-write transactions by default.
    ReadOnly,
    /// The server must not be in recovery.
    Primary,
    /// The server must be in recovery.
    Standby,
    /// Prefer a server in recovery, falling back to any server only after a
    /// complete first pass over the configured hosts.
    PreferStandby,
}

/// TLS configuration: libpq's `sslmode`, all six values, with libpq's meanings.
///
/// The definitions below are the ones in the PostgreSQL documentation for the
/// `sslmode` connection parameter (<https://www.postgresql.org/docs/current/libpq-connect.html>),
/// and the fallback mechanics are the ones in libpq's own connection state
/// machine (`src/interfaces/libpq/fe-connect.c`).
///
/// Two axes vary independently, and conflating them is the classic bug:
///
/// * **Is TLS mandatory?** [`Require`](SslMode::Require),
///   [`VerifyCa`](SslMode::VerifyCa) and [`VerifyFull`](SslMode::VerifyFull)
///   have no plaintext fallback at all - not as a check at the failure site,
///   but because plaintext is never in the set of transports they may use.
///   [`Disable`](SslMode::Disable), [`Allow`](SslMode::Allow) and
///   [`Prefer`](SslMode::Prefer) permit plaintext.
/// * **Is the server's certificate checked?** That is *not* implied by the
///   first axis. See [`SslRootCert`] for the table; the short version is that
///   `require` encrypts without authenticating unless trust anchors are named.
///
/// `allow` and `prefer` differ only in which transport is tried *first*.
/// libpq's `select_next_encryption_method` is literally an ordering swap:
/// `allow` offers plaintext then TLS, everything else offers TLS then (if
/// permitted) plaintext.
///
/// `sslmode` is ignored for Unix-domain-socket connections, as in libpq.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SslMode {
    /// Only try a non-TLS connection.
    Disable,
    /// First try a non-TLS connection; if that fails, try a TLS connection.
    ///
    /// The mirror image of [`Prefer`](SslMode::Prefer). This is the mode for a
    /// server that *might* insist on TLS (`hostssl` in `pg_hba.conf`): the
    /// plaintext attempt is rejected during startup, and the retry is the one
    /// that gets in. TLS is therefore only ever reached here when plaintext
    /// has already failed.
    Allow,
    /// First try a TLS connection; if that fails, try a non-TLS connection.
    ///
    /// libpq's default, and this driver's. The documentation's own verdict on
    /// it is worth repeating: it "makes no sense from a security point of
    /// view", because a man in the middle need only answer `N` to the
    /// `SSLRequest` to get a plaintext session. It is the default for
    /// compatibility, not because it is a good choice.
    Prefer,
    /// Only try a TLS connection. If trust anchors are configured, verify the
    /// certificate exactly as [`VerifyCa`](SslMode::VerifyCa) would; otherwise
    /// perform no certificate verification at all.
    ///
    /// Encryption without authentication: it stops passive eavesdropping and
    /// nothing else.
    Require,
    /// Only try a TLS connection, and verify that the server certificate
    /// chains to a configured trust anchor. The host name is **not** checked.
    ///
    /// Requires trust anchors; see [`SslRootCert`].
    VerifyCa,
    /// Only try a TLS connection, verify the chain, **and** verify that the
    /// host name asked for matches the certificate.
    ///
    /// Requires trust anchors; see [`SslRootCert`].
    VerifyFull,
}

impl SslMode {
    /// Whether a plaintext session is an acceptable outcome for this mode.
    ///
    /// This is libpq's `ENC_PLAINTEXT` membership test in
    /// `init_allowed_encryption_methods`, and it is what makes a silent
    /// downgrade structurally impossible for the three strong modes: they
    /// never have plaintext in the set to fall back to.
    pub const fn permits_plaintext(self) -> bool {
        matches!(self, Self::Disable | Self::Allow | Self::Prefer)
    }

    /// Whether TLS may be attempted for this mode (libpq's `ENC_SSL`).
    pub const fn permits_tls(self) -> bool {
        !matches!(self, Self::Disable)
    }

    /// The spelling this mode has in a connection string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }
}

/// TLS negotiation configuration
///
/// See more information at
/// https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNECT-SSLNEGOTIATION
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SslNegotiation {
    /// Use PostgreSQL SslRequest for Ssl negotiation
    #[default]
    Postgres,
    /// Start Ssl handshake without negotiation, only works for PostgreSQL 17+
    Direct,
}

/// A TLS protocol version this driver's rustls backend can negotiate.
///
/// libpq also recognises `TLSv1` and `TLSv1.1`. rustls deliberately does not
/// implement either, so connection strings that request them are rejected
/// rather than being widened to include a newer version.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SslProtocolVersion {
    /// TLS 1.2 (`TLSv1.2`).
    TlsV1_2,
    /// TLS 1.3 (`TLSv1.3`).
    TlsV1_3,
}

impl SslProtocolVersion {
    /// The spelling libpq uses in a connection string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TlsV1_2 => "TLSv1.2",
            Self::TlsV1_3 => "TLSv1.3",
        }
    }
}

/// Where the trust anchors for server-certificate verification come from.
///
/// This is the `sslrootcert` connection parameter, and together with
/// [`SslMode`] it decides what - if anything - is checked about the server's
/// certificate. libpq computes that from two facts: whether trust anchors are
/// available at all (`have_rootcert` in `initialize_SSL`), and whether the
/// mode is exactly `verify-full` (the only mode whose
/// `pq_verify_peer_name_matches_certificate` does any work).
///
/// | `sslmode` | no anchors ([`Unset`](SslRootCert::Unset)) | anchors configured |
/// | --- | --- | --- |
/// | `disable` | no TLS | no TLS |
/// | `allow`, `prefer` | TLS unverified, or plaintext | chain checked when TLS is used |
/// | `require` | **encrypted, unverified** | chain checked (i.e. `verify-ca`) |
/// | `verify-ca` | error: no trust anchors | chain checked |
/// | `verify-full` | error: no trust anchors | chain **and** host name checked |
///
/// The one asymmetry worth memorising: `require` does not authenticate the
/// server unless you give it something to authenticate against. That is
/// libpq's behaviour, and this driver is a driver.
///
/// A deployment whose Postgres presents a private-CA or self-signed certificate
/// names the signing CA with `sslrootcert=<path>` in the connection string, or
/// by setting it on a [`Config`] handed to
/// [`Pool::connect_with_config`](crate::Pool::connect_with_config). The
/// URL-taking constructors reach it only through the connection string.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SslRootCert {
    /// No trust anchors named - the default.
    ///
    /// libpq's equivalent is "`~/.postgresql/root.crt` does not exist", which
    /// is the usual state of a machine. This driver has no home-directory
    /// default (a published library does not read the user's dotfiles), so
    /// "unset" is spelled by leaving `sslrootcert` out.
    #[default]
    Unset,
    /// Use the operating system's certificate store (`sslrootcert=system`).
    ///
    /// libpq restricts this keyword to `sslmode=verify-full`: naming the
    /// public root program as your trust anchor while skipping the host-name
    /// check would trust every certificate any public CA has ever issued, for
    /// any name. Weaker modes are rejected, with the same error libpq raises.
    System,
    /// Trust exactly the certificates in this PEM file, and nothing else.
    ///
    /// This is the private-CA and self-signed-server path: point it at the CA
    /// certificate (or at the server's own certificate, if self-signed).
    File(String),
}

impl SslRootCert {
    /// Whether any trust anchors are configured - libpq's `have_rootcert`.
    pub const fn is_configured(&self) -> bool {
        !matches!(self, Self::Unset)
    }
}

/// Channel binding configuration.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChannelBinding {
    /// Do not use channel binding.
    Disable,
    /// Attempt to use channel binding but allow sessions without.
    Prefer,
    /// Require the use of channel binding.
    Require,
}

/// An authentication method recognized by PostgreSQL 16's `require_auth`
/// connection parameter.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthMethod {
    /// Cleartext password authentication (`password`).
    Password,
    /// PostgreSQL's MD5 challenge-response authentication (`md5`).
    Md5,
    /// GSSAPI authentication (`gss`). The driver cannot perform it, so DSNs
    /// may name it only in a negative policy such as `require_auth=!gss`.
    Gss,
    /// Windows SSPI authentication (`sspi`). The driver cannot perform it, so
    /// DSNs may name it only in a negative policy such as `require_auth=!sspi`.
    Sspi,
    /// SCRAM-SHA-256, with or without channel binding (`scram-sha-256`).
    ScramSha256,
    /// No explicit PostgreSQL-protocol authentication challenge (`none`).
    None,
}

impl AuthMethod {
    /// Returns this method's PostgreSQL connection-string spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Md5 => "md5",
            Self::Gss => "gss",
            Self::Sspi => "sspi",
            Self::ScramSha256 => "scram-sha-256",
            Self::None => "none",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "password" => Some(Self::Password),
            "md5" => Some(Self::Md5),
            "gss" => Some(Self::Gss),
            "sspi" => Some(Self::Sspi),
            "scram-sha-256" => Some(Self::ScramSha256),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// A non-empty collection of authentication methods for a [`RequireAuth`]
/// policy.
///
/// Use [`AuthMethods::new`] for the first method and [`AuthMethods::with`] to
/// add alternatives. Adding the same method twice is idempotent, so the typed
/// builder cannot create the duplicate or empty lists rejected by the
/// connection-string grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMethods(Vec<AuthMethod>);

impl AuthMethods {
    /// Creates a collection containing one authentication method.
    pub fn new(method: AuthMethod) -> Self {
        Self(vec![method])
    }

    /// Adds another acceptable or rejected method to this collection.
    pub fn with(mut self, method: AuthMethod) -> Self {
        if !self.contains(method) {
            self.0.push(method);
        }
        self
    }

    /// Returns whether this collection contains `method`.
    pub fn contains(&self, method: AuthMethod) -> bool {
        self.0.contains(&method)
    }
}

/// Policy restricting which authentication methods a server may request.
///
/// This is PostgreSQL 16's `require_auth` connection parameter. A positive
/// list is represented by [`RequireAuth::Require`], a fully-negated list by
/// [`RequireAuth::Reject`], and an omitted or empty parameter by
/// [`RequireAuth::Any`]. Including [`AuthMethod::None`] in a required list
/// permits the server to skip an explicit authentication exchange; rejecting
/// it requires the server to complete one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RequireAuth {
    /// Accept any supported method, including no authentication challenge.
    #[default]
    Any,
    /// Require exactly one of the listed methods.
    Require(AuthMethods),
    /// Reject the listed methods and accept any other supported method.
    Reject(AuthMethods),
}

impl RequireAuth {
    fn parse(value: &str) -> Result<Self, InvalidRequireAuth> {
        if value.is_empty() {
            return Ok(Self::Any);
        }

        let mut methods = Vec::new();
        let mut expected_negated = None;

        for part in value.split(',') {
            let (negated, method_name) = match part.strip_prefix('!') {
                Some(method) => (true, method),
                None => (false, part),
            };

            match expected_negated {
                None => expected_negated = Some(negated),
                Some(false) if negated => {
                    return Err(InvalidRequireAuth(format!(
                        "negative method {part:?} cannot be mixed with non-negative methods"
                    )));
                }
                Some(true) if !negated => {
                    return Err(InvalidRequireAuth(format!(
                        "method {part:?} cannot be mixed with negative methods"
                    )));
                }
                Some(_) => {}
            }

            let method = AuthMethod::parse(method_name).ok_or_else(|| {
                InvalidRequireAuth(format!("unknown authentication method {method_name:?}"))
            })?;
            if !negated && matches!(method, AuthMethod::Gss | AuthMethod::Sspi) {
                return Err(InvalidRequireAuth(format!(
                    "authentication method {method_name:?} cannot be required because it is not \
                     supported by this driver"
                )));
            }
            if methods.contains(&method) {
                return Err(InvalidRequireAuth(format!(
                    "method {part:?} is specified more than once"
                )));
            }
            methods.push(method);
        }

        let methods = AuthMethods(methods);
        if expected_negated == Some(true) {
            Ok(Self::Reject(methods))
        } else {
            Ok(Self::Require(methods))
        }
    }

    pub(crate) fn allows(&self, method: AuthMethod) -> bool {
        match self {
            Self::Any => true,
            Self::Require(methods) => methods.contains(method),
            Self::Reject(methods) => !methods.contains(method),
        }
    }
}

impl fmt::Display for RequireAuth {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => fmt.write_str(""),
            Self::Require(methods) | Self::Reject(methods) => {
                let negated = matches!(self, Self::Reject(_));
                for (index, method) in methods.0.iter().enumerate() {
                    if index != 0 {
                        fmt.write_str(",")?;
                    }
                    if negated {
                        fmt.write_str("!")?;
                    }
                    fmt.write_str(method.as_str())?;
                }
                Ok(())
            }
        }
    }
}

/// Load balancing configuration.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadBalanceHosts {
    /// Make connection attempts to hosts in the order provided.
    Disable,
    /// Make connection attempts to hosts in a random order.
    Random,
}

/// Replication mode for the connection.
///
/// Setting this configures the `replication` startup parameter, which
/// enables the Postgres streaming-replication protocol. Once the
/// connection enters replication mode the regular `query` / `execute`
/// surface is **not used**; replication commands (`IDENTIFY_SYSTEM`,
/// `START_REPLICATION`, `CREATE_REPLICATION_SLOT`, …) are issued via
/// the simple-query path and the connection returns
/// `CopyBothResponse` for `START_REPLICATION`.
///
/// See: https://www.postgresql.org/docs/16/protocol-replication.html
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplicationMode {
    /// Physical replication. Streams the raw WAL — not used by
    /// zeroship; documented for completeness because the startup
    /// parameter value is just `replication=true`.
    Physical,
    /// Logical replication via a logical-decoding output plugin
    /// (we use `pgoutput`). Startup parameter is
    /// `replication=database`.
    Logical,
}

/// A host specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// A TCP hostname.
    Tcp(String),
    /// A path to a directory containing the server's Unix socket.
    ///
    /// This variant is only available on Unix platforms.
    #[cfg(unix)]
    Unix(PathBuf),
}

/// Connection configuration.
///
/// Configuration can be parsed from libpq-style connection strings. These strings come in two formats:
///
/// # Key-Value
///
/// This format consists of space-separated key-value pairs. Values which are either the empty string or contain
/// whitespace should be wrapped in `'`. `'` and `\` characters should be backslash-escaped.
///
/// ## Keys
///
/// * `user` - The username to authenticate with. Defaults to the user executing this process.
/// * `password` - The password to authenticate with.
/// * `dbname` - The name of the database to connect to. Defaults to the username.
/// * `options` - Command line options used to configure the server.
/// * `application_name` - Sets the `application_name` parameter on the server.
/// * `fallback_application_name` - The `application_name` to announce when none is set, so a library can name the
///     session without overriding the application's own choice.
/// * `client_encoding` - Accepted only as `UTF8` (or `UNICODE`). The startup packet always announces UTF8 because
///     Rust strings are UTF-8; any other encoding is refused rather than mis-decoded.
/// * `statement_cache_capacity` - Maximum number of implicit raw-SQL prepared
///     statements retained per connection. Defaults to 0 (disabled).
/// * `sslmode` - Controls usage of TLS, with libpq's six values and libpq's meanings: `disable`, `allow`, `prefer`
///     (the default), `require`, `verify-ca`, `verify-full`. See [`SslMode`] for what each one does, and
///     [`SslRootCert`] for the certificate-verification table - in particular, `require` encrypts but does *not*
///     authenticate the server unless `sslrootcert` names trust anchors.
/// * `sslrootcert` - Trust anchors for server-certificate verification: a path to a PEM file, or the keyword `system`
///     for the operating system's store. Unset by default, which means no verification for `require`/`prefer`/`allow`
///     and an error for `verify-ca`/`verify-full`. Point it at your CA (or at the server's own certificate, if
///     self-signed) when the server does not chain to a public root. `system` may only be combined with
///     `sslmode=verify-full`.
/// * `sslcert` - Path to the client certificate chain (PEM) for client-certificate authentication. Must be given
///     together with `sslkey`.
/// * `sslkey` - Path to the private key (PEM) matching `sslcert`.
/// * `sslpassword` - Passphrase for an encrypted PKCS#8 `sslkey`. A blank value
///     behaves as absent, and a value is ignored when `sslkey` is unencrypted.
/// * `sslcrl` - Path to a PEM certificate revocation list used while verifying
///     the server certificate. As in libpq, a missing or unreadable file has no
///     effect.
/// * `sslcrldir` - Path to a directory prepared with `openssl rehash` or
///     `c_rehash`. Its hashed PEM CRL entries are used together with `sslcrl`.
/// * `ssl_min_protocol_version` - Minimum TLS version: `TLSv1.2` (the default)
///     or `TLSv1.3`. libpq's `TLSv1` and `TLSv1.1` values are rejected because
///     rustls cannot honour them.
/// * `ssl_max_protocol_version` - Maximum TLS version: `TLSv1.2` or `TLSv1.3`.
///     Unset by default. A maximum below the minimum is rejected before
///     connecting.
/// * `host` - The host to connect to. On Unix platforms, if the host starts with a `/` character it is treated as the
///     path to the directory containing Unix domain sockets. Otherwise, it is treated as a hostname. Multiple hosts
///     can be specified, separated by commas. Each host will be tried in turn when connecting. Required if connecting
///     with the `connect` method.
/// * `sslnegotiation` - TLS negotiation method. If set to `direct`, the client
///     will perform direct TLS handshake, this only works for PostgreSQL 17 and
///     newer.
///     PostgreSQL requires the `postgresql` ALPN protocol for direct TLS. This
///     crate's `MakeRustlsConnect` adds it when the supplied rustls
///     `ClientConfig` has an empty ALPN list. A nonempty caller-supplied list is
///     preserved unchanged and must include `postgresql` to support direct TLS.
///     If set to `postgres`, the default value, it follows original postgres
///     wire protocol to perform the negotiation.
/// * `hostaddr` - Numeric IP address of host to connect to. This should be in the standard IPv4 address format,
///     e.g., 172.28.40.9. If your machine supports IPv6, you can also use those addresses.
///     If this parameter is not specified, the value of `host` will be looked up to find the corresponding IP address,
///     or if host specifies an IP address, that value will be used directly.
///     Using `hostaddr` allows the application to avoid a host name look-up, which might be important in applications
///     with time constraints. However, a host name is required for TLS certificate verification.
///     Specifically:
///         * If `hostaddr` is specified without `host`, the value for `hostaddr` gives the server network address.
///             The connection attempt will fail if the authentication method requires a host name;
///         * If `host` is specified without `hostaddr`, a host name lookup occurs;
///         * If both `host` and `hostaddr` are specified, the value for `hostaddr` gives the server network address.
///             The value for `host` is ignored unless the authentication method requires it,
///             in which case it will be used as the host name.
/// * `port` - The port to connect to. Multiple ports can be specified, separated by commas. The number of ports must be
///     either 1, in which case it will be used for all hosts, or the same as the number of hosts. Defaults to 5432 if
///     omitted or the empty string.
/// * `connect_timeout` - The time limit in seconds applied to each address tried, covering TLS negotiation, startup,
///     and authentication, and applied once more to each host entry's name resolution. Hostnames can resolve to
///     multiple IP addresses, and the limit restarts for each, as libpq's does. Defaults to no timeout.
/// * `tcp_user_timeout` - The time limit that transmitted data may remain unacknowledged before a connection is forcibly closed.
///     This is ignored for Unix domain socket connections. It is only supported on systems where TCP_USER_TIMEOUT is available
///     and will default to the system default if omitted or set to 0; on other systems, it has no effect.
/// * `keepalives` - Controls the use of TCP keepalive. A value of 0 disables keepalive and nonzero integers enable it.
///     This option is ignored when connecting with Unix sockets. Defaults to on.
/// * `keepalives_idle` - The number of seconds of inactivity after which a keepalive message is sent to the server.
///     This option is ignored when connecting with Unix sockets. Defaults to 2 hours.
/// * `keepalives_interval` - The time interval between TCP keepalive probes.
///     This option is ignored when connecting with Unix sockets.
/// * `keepalives_count` - The maximum number of TCP keepalive probes that will be sent before dropping a connection.
///     This option is ignored when connecting with Unix sockets.
/// * `target_session_attrs` - Specifies requirements of the session. `read-write` requires
///     `transaction_read_only` to be `off`, while `read-only` requires it to be `on`. `primary` requires a server
///     that is not in recovery, `standby` requires one that is, and `prefer-standby` retries the host list in `any`
///     mode only if the first pass finds no standby. Defaults to `any`.
/// * `channel_binding` - Controls usage of channel binding in the authentication process. If set to `disable`, channel
///     binding will not be used. If set to `prefer`, channel binding will be used if available, but not used otherwise.
///     If set to `require`, the authentication process will fail if channel binding is not used. Defaults to `prefer`.
/// * `require_auth` - A comma-separated allowlist of authentication methods, or a list in which every method is
///     prefixed by `!` to reject those methods. The driver supports positive requirements for `password`, `md5`,
///     `scram-sha-256`, and `none`. It recognizes PostgreSQL 16's `gss` and `sspi` names only in negative lists,
///     because it cannot perform those authentication methods. An omitted or empty value accepts any supported
///     method and permits the server to skip authentication.
/// * `load_balance_hosts` - Controls the order in which the client tries to connect to the available hosts and
///     addresses. Once a connection attempt is successful no other hosts and addresses will be tried. This parameter
///     is typically used in combination with multiple host names or a DNS record that returns multiple IPs. If set to
///     `disable`, hosts and addresses will be tried in the order provided. If set to `random`, hosts will be tried
///     in a random order, and the IP addresses resolved from a hostname will also be tried in a random order. Defaults
///     to `disable`.
///
/// ## Examples
///
/// ```not_rust
/// host=localhost user=postgres connect_timeout=10 keepalives=0
/// ```
///
/// ```not_rust
/// host=/var/run/postgresql,localhost port=1234 user=postgres password='password with spaces'
/// ```
///
/// ```not_rust
/// host=host1,host2,host3 port=1234,,5678 hostaddr=127.0.0.1,127.0.0.2,127.0.0.3 user=postgres target_session_attrs=read-write
/// ```
///
/// ```not_rust
/// host=host1,host2,host3 port=1234,,5678 user=postgres target_session_attrs=read-write
/// ```
///
/// # Url
///
/// This format resembles a URL with a scheme of either `postgres://` or `postgresql://`. All components are optional,
/// and the format accepts query parameters for all of the key-value pairs described in the section above. Multiple
/// host/port pairs can be comma-separated. Unix socket paths in the host section of the URL should be percent-encoded,
/// as the path component of the URL specifies the database name.
///
/// ## Examples
///
/// ```not_rust
/// postgresql://user@localhost
/// ```
///
/// ```not_rust
/// postgresql://user:password@%2Fvar%2Frun%2Fpostgresql/mydb?connect_timeout=10
/// ```
///
/// ```not_rust
/// postgresql://user@host1:1234,host2,host3:5678?target_session_attrs=read-write
/// ```
///
/// ```not_rust
/// postgresql:///mydb?user=user&host=/var/run/postgresql
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub(crate) user: Option<String>,
    pub(crate) password: Option<Vec<u8>>,
    pub(crate) dbname: Option<String>,
    pub(crate) options: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) fallback_application_name: Option<String>,
    pub(crate) statement_cache_capacity: usize,
    pub(crate) ssl_mode: SslMode,
    pub(crate) ssl_negotiation: SslNegotiation,
    pub(crate) ssl_root_cert: SslRootCert,
    pub(crate) ssl_cert: Option<String>,
    pub(crate) ssl_key: Option<String>,
    pub(crate) ssl_password: Option<Vec<u8>>,
    pub(crate) ssl_crl: Option<String>,
    pub(crate) ssl_crl_dir: Option<String>,
    pub(crate) ssl_min_protocol_version: SslProtocolVersion,
    pub(crate) ssl_max_protocol_version: Option<SslProtocolVersion>,
    pub(crate) host: Vec<Host>,
    pub(crate) hostaddr: Vec<IpAddr>,
    pub(crate) port: Vec<u16>,
    pub(crate) connect_timeout: Option<Duration>,
    /// Programmatic-only post-startup socket-read policy. It is deliberately
    /// absent from the libpq connection-string parser.
    pub(crate) read_timeout: Option<Duration>,
    pub(crate) tcp_user_timeout: Option<Duration>,
    pub(crate) keepalives: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) keepalive_config: KeepaliveConfig,
    pub(crate) target_session_attrs: TargetSessionAttrs,
    pub(crate) channel_binding: ChannelBinding,
    pub(crate) require_auth: RequireAuth,
    pub(crate) load_balance_hosts: LoadBalanceHosts,
    pub(crate) replication: Option<ReplicationMode>,
}

impl Default for Config {
    fn default() -> Config {
        Config::new()
    }
}

impl Config {
    /// Creates a new configuration.
    pub fn new() -> Config {
        Config {
            user: None,
            password: None,
            dbname: None,
            options: None,
            application_name: None,
            fallback_application_name: None,
            statement_cache_capacity: 0,
            ssl_mode: SslMode::Prefer,
            ssl_negotiation: SslNegotiation::Postgres,
            ssl_root_cert: SslRootCert::Unset,
            ssl_cert: None,
            ssl_key: None,
            ssl_password: None,
            ssl_crl: None,
            ssl_crl_dir: None,
            ssl_min_protocol_version: SslProtocolVersion::TlsV1_2,
            ssl_max_protocol_version: None,
            host: vec![],
            hostaddr: vec![],
            port: vec![],
            connect_timeout: None,
            read_timeout: None,
            tcp_user_timeout: None,
            keepalives: true,
            #[cfg(not(target_arch = "wasm32"))]
            keepalive_config: KeepaliveConfig {
                idle: Duration::from_secs(2 * 60 * 60),
                interval: None,
                retries: None,
            },
            target_session_attrs: TargetSessionAttrs::Any,
            channel_binding: ChannelBinding::Prefer,
            require_auth: RequireAuth::Any,
            load_balance_hosts: LoadBalanceHosts::Disable,
            replication: None,
        }
    }

    /// Sets the user to authenticate with.
    ///
    /// Defaults to the user executing this process.
    pub fn user(&mut self, user: impl Into<String>) -> &mut Config {
        self.user = Some(user.into());
        self
    }

    /// Gets the user to authenticate with, if one has been configured with
    /// the `user` method.
    pub fn get_user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Sets the password to authenticate with.
    pub fn password<T>(&mut self, password: T) -> &mut Config
    where
        T: AsRef<[u8]>,
    {
        self.password = Some(password.as_ref().to_vec());
        self
    }

    /// Gets the password to authenticate with, if one has been configured with
    /// the `password` method.
    pub fn get_password(&self) -> Option<&[u8]> {
        self.password.as_deref()
    }

    /// Sets the name of the database to connect to.
    ///
    /// Defaults to the user.
    pub fn dbname(&mut self, dbname: impl Into<String>) -> &mut Config {
        self.dbname = Some(dbname.into());
        self
    }

    /// Gets the name of the database to connect to, if one has been configured
    /// with the `dbname` method.
    pub fn get_dbname(&self) -> Option<&str> {
        self.dbname.as_deref()
    }

    /// Sets command line options used to configure the server.
    pub fn options(&mut self, options: impl Into<String>) -> &mut Config {
        self.options = Some(options.into());
        self
    }

    /// Gets the command line options used to configure the server, if the
    /// options have been set with the `options` method.
    pub fn get_options(&self) -> Option<&str> {
        self.options.as_deref()
    }

    /// Sets the value of the `application_name` runtime parameter.
    pub fn application_name(&mut self, application_name: impl Into<String>) -> &mut Config {
        self.application_name = Some(application_name.into());
        self
    }

    /// Gets the value of the `application_name` runtime parameter, if it has
    /// been set with the `application_name` method.
    pub fn get_application_name(&self) -> Option<&str> {
        self.application_name.as_deref()
    }

    /// Sets the session name to announce when `application_name` is unset.
    ///
    /// libpq's purpose for this key is that a library can name the session
    /// without overriding a name the application itself chose, so it never
    /// displaces `application_name`.
    pub fn fallback_application_name(
        &mut self,
        fallback_application_name: impl Into<String>,
    ) -> &mut Config {
        self.fallback_application_name = Some(fallback_application_name.into());
        self
    }

    /// Gets the value of the `fallback_application_name` runtime parameter, if
    /// it has been set with the `fallback_application_name` method.
    pub fn get_fallback_application_name(&self) -> Option<&str> {
        self.fallback_application_name.as_deref()
    }

    /// The session name the startup packet announces: `application_name` when
    /// set, otherwise `fallback_application_name`. Resolved in one place so the
    /// two getters can keep mirroring their setters.
    pub(crate) fn resolved_application_name(&self) -> Option<&str> {
        self.get_application_name()
            .or_else(|| self.get_fallback_application_name())
    }

    /// Sets the maximum number of implicit raw-SQL prepared statements cached
    /// on each connection.
    ///
    /// Entries are keyed by the exact SQL text and evicted least-recently used.
    /// Explicit [`Client::prepare`](crate::Client::prepare) calls remain
    /// caller-owned and are not cached. [`Uncached`](crate::Uncached) bypasses
    /// an enabled cache for one operation.
    ///
    /// The cache is disabled by default. Keep it disabled when connecting
    /// through a transaction-mode connection pooler: persistent named
    /// statements are scoped to a `PostgreSQL` session, while successive
    /// transactions through such a pooler may use different sessions.
    ///
    /// If `PostgreSQL` reports that a cached statement's result type changed
    /// (`0A000` from its plan-cache routines) or that its server-side name is
    /// missing (`26000`), the stale entry is evicted and the operation is
    /// prepared and run again, once. The caller sees the result, not the
    /// error. A second consecutive failure propagates.
    ///
    /// Two limits are worth knowing before enabling this. Inside a transaction
    /// nothing is retried, because `0A000` has already aborted it, so the error
    /// propagates and the transaction still needs recovery. And `26000` is
    /// matched on its SQLSTATE alone, which cannot prove the statement did no
    /// work: a function that performs non-transactional work and then raises
    /// `26000` would be run twice. No row reaches the caller before the retry
    /// decision, so a retry never duplicates rows -- only such side effects.
    pub const fn statement_cache_capacity(&mut self, capacity: usize) -> &mut Config {
        self.statement_cache_capacity = capacity;
        self
    }

    /// Gets the per-connection prepared-statement cache capacity.
    #[must_use]
    pub const fn get_statement_cache_capacity(&self) -> usize {
        self.statement_cache_capacity
    }

    /// Sets the SSL configuration.
    ///
    /// Defaults to `prefer`.
    pub fn ssl_mode(&mut self, ssl_mode: SslMode) -> &mut Config {
        self.ssl_mode = ssl_mode;
        self
    }

    /// Gets the SSL configuration.
    pub fn get_ssl_mode(&self) -> SslMode {
        self.ssl_mode
    }

    /// Sets the SSL negotiation method.
    ///
    /// Defaults to `postgres`.
    pub fn ssl_negotiation(&mut self, ssl_negotiation: SslNegotiation) -> &mut Config {
        self.ssl_negotiation = ssl_negotiation;
        self
    }

    /// Gets the SSL negotiation method.
    pub fn get_ssl_negotiation(&self) -> SslNegotiation {
        self.ssl_negotiation
    }

    /// Sets the trust anchors used to verify the server's certificate.
    ///
    /// Defaults to [`SslRootCert::Unset`].
    pub fn ssl_root_cert(&mut self, ssl_root_cert: SslRootCert) -> &mut Config {
        self.ssl_root_cert = ssl_root_cert;
        self
    }

    /// Gets the trust anchors used to verify the server's certificate.
    pub fn get_ssl_root_cert(&self) -> &SslRootCert {
        &self.ssl_root_cert
    }

    /// Sets the path to the client certificate chain (PEM) sent to the server.
    ///
    /// Must be paired with [`Config::ssl_key`].
    pub fn ssl_cert(&mut self, ssl_cert: impl Into<String>) -> &mut Config {
        self.ssl_cert = Some(ssl_cert.into());
        self
    }

    /// Gets the path to the client certificate chain, if one has been set.
    pub fn get_ssl_cert(&self) -> Option<&str> {
        self.ssl_cert.as_deref()
    }

    /// Sets the path to the client private key (PEM) matching [`Config::ssl_cert`].
    pub fn ssl_key(&mut self, ssl_key: impl Into<String>) -> &mut Config {
        self.ssl_key = Some(ssl_key.into());
        self
    }

    /// Gets the path to the client private key, if one has been set.
    pub fn get_ssl_key(&self) -> Option<&str> {
        self.ssl_key.as_deref()
    }

    /// Sets the passphrase used to decrypt an encrypted PKCS#8 client key.
    ///
    /// A blank passphrase behaves as though none was supplied. The value is
    /// ignored when [`Config::ssl_key`] names an unencrypted key.
    pub fn ssl_password<T>(&mut self, ssl_password: T) -> &mut Config
    where
        T: AsRef<[u8]>,
    {
        self.ssl_password = Some(ssl_password.as_ref().to_vec());
        self
    }

    /// Gets the client private-key passphrase, if one has been set.
    pub fn get_ssl_password(&self) -> Option<&[u8]> {
        self.ssl_password.as_deref()
    }

    /// Sets the PEM certificate revocation list used for server verification.
    ///
    /// A blank path behaves as though none was supplied. libpq also ignores a
    /// CRL file that does not exist or cannot be loaded.
    pub fn ssl_crl(&mut self, ssl_crl: impl Into<String>) -> &mut Config {
        self.ssl_crl = Some(ssl_crl.into());
        self
    }

    /// Gets the certificate revocation list path, if one has been set.
    pub fn get_ssl_crl(&self) -> Option<&str> {
        self.ssl_crl.as_deref()
    }

    /// Sets the directory of OpenSSL-hashed PEM certificate revocation lists.
    ///
    /// A blank path behaves as though none was supplied. The directory may be
    /// used together with [`Config::ssl_crl`], but two CRLs for the same issuer
    /// are refused because rustls cannot reproduce OpenSSL's newest-CRL
    /// selection safely.
    pub fn ssl_crl_dir(&mut self, ssl_crl_dir: impl Into<String>) -> &mut Config {
        self.ssl_crl_dir = Some(ssl_crl_dir.into());
        self
    }

    /// Gets the hashed certificate revocation list directory, if one was set.
    pub fn get_ssl_crl_dir(&self) -> Option<&str> {
        self.ssl_crl_dir.as_deref()
    }

    /// Sets the minimum TLS protocol version.
    ///
    /// Defaults to [`SslProtocolVersion::TlsV1_2`].
    pub fn ssl_min_protocol_version(&mut self, version: SslProtocolVersion) -> &mut Config {
        self.ssl_min_protocol_version = version;
        self
    }

    /// Gets the minimum TLS protocol version.
    pub fn get_ssl_min_protocol_version(&self) -> SslProtocolVersion {
        self.ssl_min_protocol_version
    }

    /// Sets the maximum TLS protocol version.
    ///
    /// By default no maximum below rustls' own highest supported version is
    /// imposed.
    pub fn ssl_max_protocol_version(&mut self, version: SslProtocolVersion) -> &mut Config {
        self.ssl_max_protocol_version = Some(version);
        self
    }

    /// Gets the maximum TLS protocol version, if one has been set.
    pub fn get_ssl_max_protocol_version(&self) -> Option<SslProtocolVersion> {
        self.ssl_max_protocol_version
    }

    /// Adds a host to the configuration.
    ///
    /// Multiple hosts can be specified by calling this method multiple times, and each will be tried in order. On Unix
    /// systems, a host starting with a `/` is interpreted as a path to a directory containing Unix domain sockets.
    /// There must be either no hosts, or the same number of hosts as hostaddrs.
    pub fn host(&mut self, host: impl Into<String>) -> &mut Config {
        let host = host.into();

        #[cfg(unix)]
        {
            if host.starts_with('/') {
                return self.host_path(host);
            }
        }

        self.host.push(Host::Tcp(host));
        self
    }

    /// Gets the hosts that have been added to the configuration with `host`.
    pub fn get_hosts(&self) -> &[Host] {
        &self.host
    }

    /// Gets the hostaddrs that have been added to the configuration with `hostaddr`.
    pub fn get_hostaddrs(&self) -> &[IpAddr] {
        self.hostaddr.deref()
    }

    /// Adds a Unix socket host to the configuration.
    ///
    /// Unlike `host`, this method allows non-UTF8 paths.
    #[cfg(unix)]
    pub fn host_path<T>(&mut self, host: T) -> &mut Config
    where
        T: AsRef<Path>,
    {
        self.host.push(Host::Unix(host.as_ref().to_path_buf()));
        self
    }

    /// Adds a hostaddr to the configuration.
    ///
    /// Multiple hostaddrs can be specified by calling this method multiple times, and each will be tried in order.
    /// There must be either no hostaddrs, or the same number of hostaddrs as hosts.
    pub fn hostaddr(&mut self, hostaddr: IpAddr) -> &mut Config {
        self.hostaddr.push(hostaddr);
        self
    }

    /// Adds a port to the configuration.
    ///
    /// Multiple ports can be specified by calling this method multiple times. There must either be no ports, in which
    /// case the default of 5432 is used, a single port, in which it is used for all hosts, or the same number of ports
    /// as hosts.
    pub fn port(&mut self, port: u16) -> &mut Config {
        self.port.push(port);
        self
    }

    /// Gets the ports that have been added to the configuration with `port`.
    pub fn get_ports(&self) -> &[u16] {
        &self.port
    }

    /// Sets the timeout applied to each address tried, covering TLS
    /// negotiation, startup, and authentication.
    ///
    /// Hostnames can resolve to multiple IP addresses, and this timeout
    /// restarts for each one, as libpq's does. It is also applied once to each
    /// host entry's name resolution, which libpq leaves unbounded. Defaults to
    /// no limit.
    pub fn connect_timeout(&mut self, connect_timeout: Duration) -> &mut Config {
        self.connect_timeout = Some(connect_timeout);
        self
    }

    /// Gets the connection timeout, if one has been set with the
    /// `connect_timeout` method.
    pub fn get_connect_timeout(&self) -> Option<&Duration> {
        self.connect_timeout.as_ref()
    }

    /// Sets the maximum period in which a successfully flushed protocol
    /// response may make no socket-read progress. Defaults to no limit.
    ///
    /// This is clock (3), the post-startup socket-read inactivity deadline.
    /// It is armed only while the connection owes a protocol response, resets
    /// after each successful underlying read, and retires the connection on
    /// expiry because a cancelled, possibly partial read cannot be resumed.
    /// It is programmatic connection policy, not a libpq parameter, so the
    /// connection-string parser intentionally does not accept a spelling for
    /// it.
    ///
    /// Five clocks bound a query and none substitutes for another. This is
    /// clock (3); the others are
    /// [`crate::PoolConfig::command_timeout`] (1: a whole pooled command plus
    /// `CancelRequest` recovery), the server's own `statement_timeout`
    /// (2: PostgreSQL-side execution, a GUC with no client knob here, set it
    /// with `options=-c statement_timeout=...`),
    /// [`Config::connect_timeout`] (4: resolution, TCP/TLS setup, startup and
    /// authentication), and [`crate::PoolConfig::acquire_timeout`]
    /// (5: waiting for a pooled connection).
    ///
    /// None of them is `tcp_user_timeout`, which bounds how long transmitted
    /// TCP data may remain unacknowledged rather than silence from a peer that
    /// is connected and healthy.
    pub fn read_timeout(&mut self, read_timeout: Duration) -> &mut Config {
        self.read_timeout = Some(read_timeout);
        self
    }

    /// Gets the socket-read inactivity deadline, if one was configured with
    /// [`Config::read_timeout`].
    pub fn get_read_timeout(&self) -> Option<&Duration> {
        self.read_timeout.as_ref()
    }

    /// Sets the TCP user timeout.
    ///
    /// This is ignored for Unix domain socket connections. It is only supported on systems where
    /// TCP_USER_TIMEOUT is available and will default to the system default if omitted or set to 0;
    /// on other systems, it has no effect.
    pub fn tcp_user_timeout(&mut self, tcp_user_timeout: Duration) -> &mut Config {
        self.tcp_user_timeout = Some(tcp_user_timeout);
        self
    }

    /// Gets the TCP user timeout, if one has been set with the
    /// `user_timeout` method.
    pub fn get_tcp_user_timeout(&self) -> Option<&Duration> {
        self.tcp_user_timeout.as_ref()
    }

    /// Controls the use of TCP keepalive.
    ///
    /// This is ignored for Unix domain socket connections. Defaults to `true`.
    pub fn keepalives(&mut self, keepalives: bool) -> &mut Config {
        self.keepalives = keepalives;
        self
    }

    /// Reports whether TCP keepalives will be used.
    pub fn get_keepalives(&self) -> bool {
        self.keepalives
    }

    /// Sets the amount of idle time before a keepalive packet is sent on the connection.
    ///
    /// This is ignored for Unix domain sockets, or if the `keepalives` option is disabled. Defaults to 2 hours.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn keepalives_idle(&mut self, keepalives_idle: Duration) -> &mut Config {
        self.keepalive_config.idle = keepalives_idle;
        self
    }

    /// Gets the configured amount of idle time before a keepalive packet will
    /// be sent on the connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn get_keepalives_idle(&self) -> Duration {
        self.keepalive_config.idle
    }

    /// Sets the time interval between TCP keepalive probes.
    /// On Windows, this sets the value of the tcp_keepalive struct’s keepaliveinterval field.
    ///
    /// This is ignored for Unix domain sockets, or if the `keepalives` option is disabled.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn keepalives_interval(&mut self, keepalives_interval: Duration) -> &mut Config {
        self.keepalive_config.interval = Some(keepalives_interval);
        self
    }

    /// Gets the time interval between TCP keepalive probes.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn get_keepalives_interval(&self) -> Option<Duration> {
        self.keepalive_config.interval
    }

    /// Sets the maximum number of TCP keepalive probes that will be sent before dropping a connection.
    ///
    /// This is ignored for Unix domain sockets, or if the `keepalives` option is disabled.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn keepalives_count(&mut self, keepalives_count: u32) -> &mut Config {
        self.keepalive_config.retries = Some(keepalives_count);
        self
    }

    /// Gets the maximum number of TCP keepalive probes that will be sent before dropping a connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn get_keepalives_count(&self) -> Option<u32> {
        self.keepalive_config.retries
    }

    /// Sets the requirements of the session.
    ///
    /// This can be used to select a server by transaction writability or
    /// recovery role. Defaults to `Any`.
    pub fn target_session_attrs(
        &mut self,
        target_session_attrs: TargetSessionAttrs,
    ) -> &mut Config {
        self.target_session_attrs = target_session_attrs;
        self
    }

    /// Gets the requirements of the session.
    pub fn get_target_session_attrs(&self) -> TargetSessionAttrs {
        self.target_session_attrs
    }

    /// Sets the channel binding behavior.
    ///
    /// Defaults to `prefer`.
    pub fn channel_binding(&mut self, channel_binding: ChannelBinding) -> &mut Config {
        self.channel_binding = channel_binding;
        self
    }

    /// Gets the channel binding behavior.
    pub fn get_channel_binding(&self) -> ChannelBinding {
        self.channel_binding
    }

    /// Restricts which authentication methods the server may use.
    ///
    /// Defaults to [`RequireAuth::Any`].
    pub fn require_auth(&mut self, require_auth: RequireAuth) -> &mut Config {
        self.require_auth = require_auth;
        self
    }

    /// Gets the authentication-method policy.
    pub fn get_require_auth(&self) -> &RequireAuth {
        &self.require_auth
    }

    /// Sets the host load balancing behavior.
    ///
    /// Defaults to `disable`.
    pub fn load_balance_hosts(&mut self, load_balance_hosts: LoadBalanceHosts) -> &mut Config {
        self.load_balance_hosts = load_balance_hosts;
        self
    }

    /// Gets the host load balancing behavior.
    pub fn get_load_balance_hosts(&self) -> LoadBalanceHosts {
        self.load_balance_hosts
    }

    /// Enable replication mode on this connection.
    ///
    /// The startup handshake includes `replication=<value>`, putting the
    /// server in walsender mode. After authentication the connection
    /// accepts replication commands (`IDENTIFY_SYSTEM`,
    /// `START_REPLICATION`, …) through the dedicated replication client in
    /// [`crate::replication`] - `identify_system` and
    /// `start_logical_replication`, not a `Client` method. Regular query
    /// pipelining is not supported on a replication connection.
    pub fn replication(&mut self, mode: ReplicationMode) -> &mut Config {
        self.replication = Some(mode);
        self
    }

    /// Gets the replication mode, if one has been set with the
    /// [`Config::replication`] method.
    pub fn get_replication(&self) -> Option<ReplicationMode> {
        self.replication
    }

    fn param(&mut self, key: &str, value: &str) -> Result<(), Error> {
        match key {
            "user" => {
                self.user(value);
            }
            "password" => {
                self.password(value);
            }
            "dbname" => {
                self.dbname(value);
            }
            "options" => {
                self.options(value);
            }
            "fallback_application_name" => {
                self.fallback_application_name(value);
            }
            "client_encoding" => {
                // The startup packet always announces UTF8 because Rust strings
                // are UTF-8. Naming UTF8 is therefore a no-op, and naming any
                // other encoding is a request this driver cannot honour --
                // refuse it rather than decode the server's bytes as something
                // they are not.
                let normalized = value.replace(['-', '_'], "").to_ascii_uppercase();
                if !matches!(normalized.as_str(), "UTF8" | "UNICODE") {
                    return Err(Error::config_parse(Box::new(InvalidValue(
                        "client_encoding",
                    ))));
                }
            }
            "application_name" => {
                self.application_name(value);
            }
            "statement_cache_capacity" => {
                let capacity = value.parse().map_err(|_| {
                    Error::config_parse(Box::new(InvalidValue("statement_cache_capacity")))
                })?;
                self.statement_cache_capacity(capacity);
            }
            "sslmode" => {
                let mode = match value {
                    "disable" => SslMode::Disable,
                    "allow" => SslMode::Allow,
                    "prefer" => SslMode::Prefer,
                    "require" => SslMode::Require,
                    "verify-ca" => SslMode::VerifyCa,
                    "verify-full" => SslMode::VerifyFull,
                    _ => return Err(Error::config_parse(Box::new(InvalidValue("sslmode")))),
                };
                self.ssl_mode(mode);
            }
            "sslnegotiation" => {
                let mode = match value {
                    "postgres" => SslNegotiation::Postgres,
                    "direct" => SslNegotiation::Direct,
                    _ => {
                        return Err(Error::config_parse(Box::new(InvalidValue(
                            "sslnegotiation",
                        ))));
                    }
                };
                self.ssl_negotiation(mode);
            }
            "sslrootcert" => {
                // libpq 16+ spells the OS trust store `system`; anything else
                // is a path. A path literally named "system" is therefore
                // unreachable, which is the same corner libpq has.
                let root = match value {
                    "system" => SslRootCert::System,
                    "" => return Err(Error::config_parse(Box::new(InvalidValue("sslrootcert")))),
                    path => SslRootCert::File(path.to_string()),
                };
                self.ssl_root_cert(root);
            }
            "sslcert" => {
                if value.is_empty() {
                    return Err(Error::config_parse(Box::new(InvalidValue("sslcert"))));
                }
                self.ssl_cert(value);
            }
            "sslkey" => {
                if value.is_empty() {
                    return Err(Error::config_parse(Box::new(InvalidValue("sslkey"))));
                }
                self.ssl_key(value);
            }
            "sslpassword" => {
                self.ssl_password(value);
            }
            "sslcrl" => {
                self.ssl_crl(value);
            }
            "sslcrldir" => {
                self.ssl_crl_dir(value);
            }
            "ssl_min_protocol_version" => {
                let version = parse_ssl_protocol_version("ssl_min_protocol_version", value)?;
                self.ssl_min_protocol_version(version);
            }
            "ssl_max_protocol_version" => {
                // An empty maximum is libpq's spelling for leaving the TLS
                // backend's own upper bound in force.
                self.ssl_max_protocol_version = if value.is_empty() {
                    None
                } else {
                    Some(parse_ssl_protocol_version(
                        "ssl_max_protocol_version",
                        value,
                    )?)
                };
            }
            "host" => {
                for host in value.split(',') {
                    self.host(host);
                }
            }
            "hostaddr" => {
                for hostaddr in value.split(',') {
                    let addr = hostaddr
                        .parse()
                        .map_err(|_| Error::config_parse(Box::new(InvalidValue("hostaddr"))))?;
                    self.hostaddr(addr);
                }
            }
            "port" => {
                for port in value.split(',') {
                    let port = if port.is_empty() {
                        5432
                    } else {
                        port.parse()
                            .map_err(|_| Error::config_parse(Box::new(InvalidValue("port"))))?
                    };
                    self.port(port);
                }
            }
            "connect_timeout" => {
                let timeout = value
                    .parse::<i64>()
                    .map_err(|_| Error::config_parse(Box::new(InvalidValue("connect_timeout"))))?;
                if timeout > 0 {
                    self.connect_timeout(Duration::from_secs(timeout as u64));
                }
            }
            "tcp_user_timeout" => {
                let timeout = value
                    .parse::<i64>()
                    .map_err(|_| Error::config_parse(Box::new(InvalidValue("tcp_user_timeout"))))?;
                if timeout > 0 {
                    self.tcp_user_timeout(Duration::from_secs(timeout as u64));
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives" => {
                let keepalives = value
                    .parse::<u64>()
                    .map_err(|_| Error::config_parse(Box::new(InvalidValue("keepalives"))))?;
                self.keepalives(keepalives != 0);
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives_idle" => {
                let keepalives_idle = value
                    .parse::<i64>()
                    .map_err(|_| Error::config_parse(Box::new(InvalidValue("keepalives_idle"))))?;
                if keepalives_idle > 0 {
                    self.keepalives_idle(Duration::from_secs(keepalives_idle as u64));
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives_interval" => {
                let keepalives_interval = value.parse::<i64>().map_err(|_| {
                    Error::config_parse(Box::new(InvalidValue("keepalives_interval")))
                })?;
                if keepalives_interval > 0 {
                    self.keepalives_interval(Duration::from_secs(keepalives_interval as u64));
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives_count" => {
                let keepalives_count = value.parse::<u32>().map_err(|_| {
                    Error::config_parse(Box::new(InvalidValue("keepalives_count")))
                })?;
                self.keepalives_count(keepalives_count);
            }
            "target_session_attrs" => {
                let target_session_attrs = match value {
                    "any" => TargetSessionAttrs::Any,
                    "read-write" => TargetSessionAttrs::ReadWrite,
                    "read-only" => TargetSessionAttrs::ReadOnly,
                    "primary" => TargetSessionAttrs::Primary,
                    "standby" => TargetSessionAttrs::Standby,
                    "prefer-standby" => TargetSessionAttrs::PreferStandby,
                    _ => {
                        return Err(Error::config_parse(Box::new(InvalidValue(
                            "target_session_attrs",
                        ))));
                    }
                };
                self.target_session_attrs(target_session_attrs);
            }
            "channel_binding" => {
                let channel_binding = match value {
                    "disable" => ChannelBinding::Disable,
                    "prefer" => ChannelBinding::Prefer,
                    "require" => ChannelBinding::Require,
                    _ => {
                        return Err(Error::config_parse(Box::new(InvalidValue(
                            "channel_binding",
                        ))));
                    }
                };
                self.channel_binding(channel_binding);
            }
            "require_auth" => {
                let require_auth = RequireAuth::parse(value)
                    .map_err(|error| Error::config_parse(Box::new(error)))?;
                self.require_auth(require_auth);
            }
            "load_balance_hosts" => {
                let load_balance_hosts = match value {
                    "disable" => LoadBalanceHosts::Disable,
                    "random" => LoadBalanceHosts::Random,
                    _ => {
                        return Err(Error::config_parse(Box::new(InvalidValue(
                            "load_balance_hosts",
                        ))));
                    }
                };
                self.load_balance_hosts(load_balance_hosts);
            }
            "replication" => {
                // libpq accepts `database`, `true`, `on`, `1`, `yes`,
                // or `false`/`off`/`0`/`no`. We accept the three the
                // streaming-replication protocol RFC actually uses
                // (`database` / `true` / `false`) plus a permissive
                // off-equivalent set, matching libpq:
                // https://www.postgresql.org/docs/16/libpq-connect.html#LIBPQ-CONNECT-REPLICATION
                let mode = match value {
                    "database" => Some(ReplicationMode::Logical),
                    "true" | "on" | "1" | "yes" => Some(ReplicationMode::Physical),
                    "false" | "off" | "0" | "no" => None,
                    _ => {
                        return Err(Error::config_parse(Box::new(InvalidValue("replication"))));
                    }
                };
                self.replication = mode;
            }
            key => {
                return Err(Error::config_parse(Box::new(UnknownOption(
                    key.to_string(),
                ))));
            }
        }

        Ok(())
    }

    /// Reject TLS parameter combinations that contradict each other.
    ///
    /// libpq runs these in `connectOptions2`, before a socket is opened, and so
    /// do we: each rule describes a URL that can never work, so answering at
    /// connect time is the difference between naming the cause and blaming the
    /// server for a handshake that was never going to happen.
    ///
    /// The latter two rules are libpq's, including the wording:
    ///
    /// * A maximum TLS version below the minimum names an empty protocol set.
    ///   rustls cannot honour it without crossing one of the caller's bounds.
    /// * `sslrootcert=system` demands `sslmode=verify-full`. Trusting the
    ///   public root program *without* checking the host name accepts any
    ///   certificate any public CA has issued for any name, which is barely
    ///   better than no verification while looking like the strongest setting
    ///   on the page.
    /// * `sslnegotiation=direct` demands a mode with no plaintext fallback. A
    ///   direct handshake sends no `SSLRequest`, so there is no negotiation to
    ///   fall back *from*; a mode that permits plaintext must not drive it.
    pub(crate) fn validate_tls_settings(&self) -> Result<(), Error> {
        self.validate_ssl_protocol_version_range()?;

        if self.ssl_root_cert == SslRootCert::System && self.ssl_mode != SslMode::VerifyFull {
            return Err(Error::config(
                format!(
                    "weak sslmode \"{}\" may not be used with sslrootcert=system (use \
                     \"verify-full\")",
                    self.ssl_mode.as_str()
                )
                .into(),
            ));
        }

        if self.ssl_negotiation == SslNegotiation::Direct && self.ssl_mode.permits_plaintext() {
            return Err(Error::config(
                format!(
                    "weak sslmode \"{}\" may not be used with sslnegotiation=direct (use \
                     \"require\", \"verify-ca\", or \"verify-full\")",
                    self.ssl_mode.as_str()
                )
                .into(),
            ));
        }

        Ok(())
    }

    /// Reject a range rustls could only answer by enabling a version outside
    /// the caller's bounds.
    pub(crate) fn validate_ssl_protocol_version_range(&self) -> Result<(), Error> {
        if let Some(maximum) = self.ssl_max_protocol_version {
            if self.ssl_min_protocol_version > maximum {
                return Err(Error::config(
                    format!(
                        "ssl_min_protocol_version={} cannot be higher than \
                         ssl_max_protocol_version={}",
                        self.ssl_min_protocol_version.as_str(),
                        maximum.as_str()
                    )
                    .into(),
                ));
            }
        }

        Ok(())
    }

    /// Opens a connection to a PostgreSQL database.
    pub async fn connect<T>(&self, tls: T) -> Result<(Client, Connection<Socket, T::Stream>), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        connect(tls, self).await
    }

    /// Connects to a PostgreSQL database over an arbitrary stream.
    ///
    /// Uses `user`, `password`, `dbname`, `options`, `application_name`,
    /// `connect_timeout`, `read_timeout`, `statement_cache_capacity`, and
    /// `require_auth`; all other settings are ignored. The connect timeout
    /// starts with TLS negotiation on the supplied stream and covers startup
    /// and authentication. It cannot cover the caller's work to open that
    /// stream. The read timeout is installed only after startup succeeds.
    ///
    /// One exception, and it is the reason this is not simply "sslmode is
    /// ignored": the caller owns the stream, so this entry point cannot open a
    /// second one. `allow` and `prefer` are therefore reduced to the transport
    /// they attempt *first* - plaintext and TLS respectively - with no
    /// reconnect if it fails. Use [`Config::connect`] to get the fallback.
    pub async fn connect_raw<S, T>(
        &self,
        stream: S,
        tls: T,
    ) -> Result<(Client, Connection<S, T::Stream>), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        self.validate_tls_settings()?;
        // No release handle: the stream is the caller's, `S` is unconstrained,
        // and a stream that is not a socket has no descriptor to shut down.
        // Such a connection keeps the pre-existing behaviour - it is released
        // when its connection task is next polled.
        with_connect_timeout(
            self.get_connect_timeout().copied(),
            connect_raw(
                stream,
                tls,
                Encryption::first_for(self.ssl_mode),
                true,
                self,
                None,
            ),
        )
        .await
    }
}

impl FromStr for Config {
    type Err = Error;

    fn from_str(s: &str) -> Result<Config, Error> {
        match UrlParser::parse(s)? {
            Some(config) => Ok(config),
            None => Parser::parse(s),
        }
    }
}

// Omit passwords from debug output.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Redaction {}
        impl fmt::Debug for Redaction {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "_")
            }
        }

        let mut config_dbg = &mut f.debug_struct("Config");
        config_dbg = config_dbg
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| Redaction {}))
            .field("dbname", &self.dbname)
            .field("options", &self.options)
            .field("application_name", &self.application_name)
            .field(
                "fallback_application_name",
                &self.fallback_application_name,
            )
            .field("statement_cache_capacity", &self.statement_cache_capacity)
            .field("ssl_mode", &self.ssl_mode)
            .field("ssl_negotiation", &self.ssl_negotiation)
            .field("ssl_root_cert", &self.ssl_root_cert)
            .field("ssl_cert", &self.ssl_cert)
            .field("ssl_key", &self.ssl_key)
            .field(
                "ssl_password",
                &self.ssl_password.as_ref().map(|_| Redaction {}),
            )
            .field("ssl_crl", &self.ssl_crl)
            .field("ssl_crl_dir", &self.ssl_crl_dir)
            .field("ssl_min_protocol_version", &self.ssl_min_protocol_version)
            .field("ssl_max_protocol_version", &self.ssl_max_protocol_version)
            .field("host", &self.host)
            .field("hostaddr", &self.hostaddr)
            .field("port", &self.port)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("tcp_user_timeout", &self.tcp_user_timeout)
            .field("keepalives", &self.keepalives);

        #[cfg(not(target_arch = "wasm32"))]
        {
            config_dbg = config_dbg
                .field("keepalives_idle", &self.keepalive_config.idle)
                .field("keepalives_interval", &self.keepalive_config.interval)
                .field("keepalives_count", &self.keepalive_config.retries);
        }

        config_dbg
            .field("target_session_attrs", &self.target_session_attrs)
            .field("channel_binding", &self.channel_binding)
            .field("require_auth", &self.require_auth)
            .field("load_balance_hosts", &self.load_balance_hosts)
            .finish()
    }
}

#[derive(Debug)]
struct UnknownOption(String);

impl fmt::Display for UnknownOption {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "unknown option `{}`", self.0)
    }
}

impl error::Error for UnknownOption {}

#[derive(Debug)]
struct InvalidValue(&'static str);

impl fmt::Display for InvalidValue {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "invalid value for option `{}`", self.0)
    }
}

impl error::Error for InvalidValue {}

fn parse_ssl_protocol_version(
    parameter: &'static str,
    value: &str,
) -> Result<SslProtocolVersion, Error> {
    if value.eq_ignore_ascii_case("TLSv1.2") {
        return Ok(SslProtocolVersion::TlsV1_2);
    }
    if value.eq_ignore_ascii_case("TLSv1.3") {
        return Ok(SslProtocolVersion::TlsV1_3);
    }
    if value.is_empty() {
        return Err(Error::config_parse(
            format!(
                "option `{parameter}` leaves the minimum TLS version unbounded, but rustls cannot \
                 negotiate the TLS 1.0 or TLS 1.1 versions that would include"
            )
            .into(),
        ));
    }
    if value.eq_ignore_ascii_case("TLSv1") || value.eq_ignore_ascii_case("TLSv1.1") {
        return Err(Error::config_parse(
            format!(
                "option `{parameter}` requests {value}, but rustls cannot negotiate TLS 1.0 or \
                 TLS 1.1; use TLSv1.2 or TLSv1.3"
            )
            .into(),
        ));
    }

    Err(Error::config_parse(Box::new(InvalidValue(parameter))))
}

#[derive(Debug)]
struct InvalidRequireAuth(String);

impl fmt::Display for InvalidRequireAuth {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "invalid value for option `require_auth`: {}", self.0)
    }
}

impl error::Error for InvalidRequireAuth {}

struct Parser<'a> {
    s: &'a str,
    it: iter::Peekable<str::CharIndices<'a>>,
}

impl<'a> Parser<'a> {
    fn parse(s: &'a str) -> Result<Config, Error> {
        let mut parser = Parser {
            s,
            it: s.char_indices().peekable(),
        };

        let mut config = Config::new();

        while let Some((key, value)) = parser.parameter()? {
            config.param(key, &value)?;
        }

        Ok(config)
    }

    fn skip_ws(&mut self) {
        self.take_while(char::is_whitespace);
    }

    fn take_while<F>(&mut self, f: F) -> &'a str
    where
        F: Fn(char) -> bool,
    {
        let start = match self.it.peek() {
            Some(&(i, _)) => i,
            None => return "",
        };

        loop {
            match self.it.peek() {
                Some(&(_, c)) if f(c) => {
                    self.it.next();
                }
                Some(&(i, _)) => return &self.s[start..i],
                None => return &self.s[start..],
            }
        }
    }

    fn eat(&mut self, target: char) -> Result<(), Error> {
        match self.it.next() {
            Some((_, c)) if c == target => Ok(()),
            Some((i, c)) => {
                let m =
                    format!("unexpected character at byte {i}: expected `{target}` but got `{c}`");
                Err(Error::config_parse(m.into()))
            }
            None => Err(Error::config_parse("unexpected EOF".into())),
        }
    }

    fn eat_if(&mut self, target: char) -> bool {
        match self.it.peek() {
            Some(&(_, c)) if c == target => {
                self.it.next();
                true
            }
            _ => false,
        }
    }

    fn keyword(&mut self) -> Option<&'a str> {
        let s = self.take_while(|c| match c {
            c if c.is_whitespace() => false,
            '=' => false,
            _ => true,
        });

        if s.is_empty() { None } else { Some(s) }
    }

    fn value(&mut self) -> Result<String, Error> {
        let value = if self.eat_if('\'') {
            let value = self.quoted_value()?;
            self.eat('\'')?;
            value
        } else {
            self.simple_value()?
        };

        Ok(value)
    }

    fn simple_value(&mut self) -> Result<String, Error> {
        let mut value = String::new();

        while let Some(&(_, c)) = self.it.peek() {
            if c.is_whitespace() {
                break;
            }

            self.it.next();
            if c == '\\' {
                if let Some((_, c2)) = self.it.next() {
                    value.push(c2);
                }
            } else {
                value.push(c);
            }
        }

        if value.is_empty() {
            return Err(Error::config_parse("unexpected EOF".into()));
        }

        Ok(value)
    }

    fn quoted_value(&mut self) -> Result<String, Error> {
        let mut value = String::new();

        while let Some(&(_, c)) = self.it.peek() {
            if c == '\'' {
                return Ok(value);
            }

            self.it.next();
            if c == '\\' {
                if let Some((_, c2)) = self.it.next() {
                    value.push(c2);
                }
            } else {
                value.push(c);
            }
        }

        Err(Error::config_parse(
            "unterminated quoted connection parameter value".into(),
        ))
    }

    fn parameter(&mut self) -> Result<Option<(&'a str, String)>, Error> {
        self.skip_ws();
        let keyword = match self.keyword() {
            Some(keyword) => keyword,
            // An empty keyword means one of two very different things, and the
            // parse loop above reads `Ok(None)` as "input exhausted" for both.
            // Conflating them makes the parser stop at the first malformed
            // token and report SUCCESS, silently discarding every setting
            // after it - so `host=h =typo sslmode=require` yields a config that
            // no longer asks for TLS. Only a genuinely exhausted input is a
            // clean stop; anything else left in the string is a parse error.
            //
            // tokio-postgres 0.7.18 conflates them too (`parameter()` at its
            // config.rs:963 is otherwise identical), so this is a libpq
            // divergence inherited from upstream rather than one introduced
            // here. libpq rejects the empty option name.
            None => {
                return match self.it.peek() {
                    None => Ok(None),
                    Some(&(i, c)) => Err(Error::config_parse(
                        format!("unexpected character at byte {i}: expected a keyword but got `{c}`")
                            .into(),
                    )),
                };
            }
        };
        self.skip_ws();
        self.eat('=')?;
        self.skip_ws();
        // libpq treats a require_auth value that ends with `=` (optionally
        // followed by whitespace) as the empty, unrestricted policy. The
        // inherited parser rejects unquoted empty values for every other
        // option, so keep this compatibility exception narrowly scoped.
        let value = if keyword == "require_auth" && self.it.peek().is_none() {
            String::new()
        } else {
            self.value()?
        };

        Ok(Some((keyword, value)))
    }
}

// This is a pretty sloppy "URL" parser, but it matches the behavior of libpq, where things really aren't very strict
struct UrlParser<'a> {
    s: &'a str,
    config: Config,
}

impl<'a> UrlParser<'a> {
    fn parse(s: &'a str) -> Result<Option<Config>, Error> {
        let s = match Self::remove_url_prefix(s) {
            Some(s) => s,
            None => return Ok(None),
        };

        let mut parser = UrlParser {
            s,
            config: Config::new(),
        };

        parser.parse_credentials()?;
        parser.parse_host()?;
        parser.parse_path()?;
        parser.parse_params()?;

        Ok(Some(parser.config))
    }

    fn remove_url_prefix(s: &str) -> Option<&str> {
        for prefix in &["postgres://", "postgresql://"] {
            if let Some(stripped) = s.strip_prefix(prefix) {
                return Some(stripped);
            }
        }

        None
    }

    fn take_until(&mut self, end: &[char]) -> Option<&'a str> {
        match self.s.find(end) {
            Some(pos) => {
                let (head, tail) = self.s.split_at(pos);
                self.s = tail;
                Some(head)
            }
            None => None,
        }
    }

    fn take_all(&mut self) -> &'a str {
        mem::take(&mut self.s)
    }

    fn eat_byte(&mut self) {
        self.s = &self.s[1..];
    }

    fn parse_credentials(&mut self) -> Result<(), Error> {
        let creds = match self.take_until(&['@']) {
            Some(creds) => creds,
            None => return Ok(()),
        };
        self.eat_byte();

        let mut it = creds.splitn(2, ':');
        let user = self.decode(it.next().unwrap())?;
        self.config.user(user);

        if let Some(password) = it.next() {
            let password = Cow::from(percent_encoding::percent_decode(password.as_bytes()));
            self.config.password(password);
        }

        Ok(())
    }

    fn parse_host(&mut self) -> Result<(), Error> {
        let host = match self.take_until(&['/', '?']) {
            Some(host) => host,
            None => self.take_all(),
        };

        if host.is_empty() {
            return Ok(());
        }

        for chunk in host.split(',') {
            let (host, port) = if chunk.starts_with('[') {
                let idx = match chunk.find(']') {
                    Some(idx) => idx,
                    None => return Err(Error::config_parse(InvalidValue("host").into())),
                };

                let host = &chunk[1..idx];
                let remaining = &chunk[idx + 1..];
                let port = if let Some(port) = remaining.strip_prefix(':') {
                    Some(port)
                } else if remaining.is_empty() {
                    None
                } else {
                    return Err(Error::config_parse(InvalidValue("host").into()));
                };

                (host, port)
            } else {
                let mut it = chunk.splitn(2, ':');
                (it.next().unwrap(), it.next())
            };

            self.host_param(host)?;
            let port = self.decode(port.unwrap_or("5432"))?;
            self.config.param("port", &port)?;
        }

        Ok(())
    }

    fn parse_path(&mut self) -> Result<(), Error> {
        if !self.s.starts_with('/') {
            return Ok(());
        }
        self.eat_byte();

        let dbname = match self.take_until(&['?']) {
            Some(dbname) => dbname,
            None => self.take_all(),
        };

        if !dbname.is_empty() {
            self.config.dbname(self.decode(dbname)?);
        }

        Ok(())
    }

    fn parse_params(&mut self) -> Result<(), Error> {
        if !self.s.starts_with('?') {
            return Ok(());
        }
        self.eat_byte();

        while !self.s.is_empty() {
            let key = match self.take_until(&['=']) {
                Some(key) => self.decode(key)?,
                None => return Err(Error::config_parse("unterminated parameter".into())),
            };
            self.eat_byte();

            let value = match self.take_until(&['&']) {
                Some(value) => {
                    self.eat_byte();
                    value
                }
                None => self.take_all(),
            };

            if key == "host" {
                self.host_param(value)?;
            } else {
                let value = self.decode(value)?;
                self.config.param(&key, &value)?;
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    fn host_param(&mut self, s: &str) -> Result<(), Error> {
        let decoded = Cow::from(percent_encoding::percent_decode(s.as_bytes()));
        if decoded.first() == Some(&b'/') {
            self.config.host_path(OsStr::from_bytes(&decoded));
        } else {
            let decoded = str::from_utf8(&decoded).map_err(|e| Error::config_parse(Box::new(e)))?;
            self.config.host(decoded);
        }

        Ok(())
    }

    #[cfg(not(unix))]
    fn host_param(&mut self, s: &str) -> Result<(), Error> {
        let s = self.decode(s)?;
        self.config.param("host", &s)
    }

    fn decode(&self, s: &'a str) -> Result<Cow<'a, str>, Error> {
        percent_encoding::percent_decode(s.as_bytes())
            .decode_utf8()
            .map_err(|e| Error::config_parse(e.into()))
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use crate::config::{
        AuthMethod, AuthMethods, RequireAuth, SslMode, SslNegotiation, SslProtocolVersion,
        SslRootCert, TargetSessionAttrs,
    };
    use crate::{Config, config::Host};

    fn assert_target_session_attrs_parses(value: &str, expected: TargetSessionAttrs) {
        for dsn in [
            format!("host=h target_session_attrs={value}"),
            format!("postgresql://h/db?target_session_attrs={value}"),
        ] {
            let config = dsn
                .parse::<Config>()
                .unwrap_or_else(|error| {
                    panic!("target_session_attrs={value} did not parse: {error}")
                });
            assert_eq!(config.get_target_session_attrs(), expected, "{dsn}");
        }
    }

    #[test]
    fn target_session_attrs_primary_parses() {
        assert_target_session_attrs_parses("primary", TargetSessionAttrs::Primary);
    }

    #[test]
    fn target_session_attrs_standby_parses() {
        assert_target_session_attrs_parses("standby", TargetSessionAttrs::Standby);
    }

    #[test]
    fn target_session_attrs_prefer_standby_parses() {
        assert_target_session_attrs_parses(
            "prefer-standby",
            TargetSessionAttrs::PreferStandby,
        );
    }

    /// All six libpq spellings parse, to the six distinct modes.
    ///
    /// `allow`, `verify-ca` and `verify-full` used to be parse errors, which is
    /// the failure this pins: a driver that rejects `verify-full` sends every
    /// deployment that wanted the strongest setting looking for a weaker one
    /// that parses.
    #[test]
    fn all_six_sslmodes_parse() {
        for (text, expected) in [
            ("disable", SslMode::Disable),
            ("allow", SslMode::Allow),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
            ("verify-ca", SslMode::VerifyCa),
            ("verify-full", SslMode::VerifyFull),
        ] {
            let config = format!("host=h sslmode={text}").parse::<Config>().unwrap();
            assert_eq!(config.get_ssl_mode(), expected, "sslmode={text}");
            assert_eq!(expected.as_str(), text, "as_str must round-trip {text}");
        }

        // The default is libpq's default and stays that way.
        assert_eq!(
            "host=h".parse::<Config>().unwrap().get_ssl_mode(),
            SslMode::Prefer
        );

        // The set is still closed: neighbouring spellings are errors, not
        // silent downgrades to the default.
        for text in ["verify_full", "verifyfull", "VERIFY-FULL", "yes", ""] {
            format!("host=h sslmode={text}")
                .parse::<Config>()
                .err()
                .unwrap_or_else(|| panic!("sslmode={text} parsed"));
        }
    }

    /// The plaintext-permitting set is libpq's `ENC_PLAINTEXT` membership, and
    /// it is what makes a silent downgrade impossible for the strong modes.
    #[test]
    fn only_the_three_weak_modes_permit_plaintext() {
        for mode in [SslMode::Disable, SslMode::Allow, SslMode::Prefer] {
            assert!(mode.permits_plaintext(), "{}", mode.as_str());
        }
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert!(!mode.permits_plaintext(), "{}", mode.as_str());
            assert!(mode.permits_tls(), "{}", mode.as_str());
        }
        assert!(!SslMode::Disable.permits_tls());
    }

    /// libpq rejects `sslrootcert=system` under anything weaker than
    /// `verify-full`, because trusting the public root program without
    /// checking the host name accepts any certificate any public CA has issued
    /// for any name.
    #[test]
    fn sslrootcert_system_requires_verify_full() {
        for mode in ["disable", "allow", "prefer", "require", "verify-ca"] {
            let config = format!("host=h sslmode={mode} sslrootcert=system")
                .parse::<Config>()
                .unwrap();
            let err = config
                .validate_tls_settings()
                .expect_err("weaker than verify-full with sslrootcert=system must be rejected");
            let text = format!("{:?}", std::error::Error::source(&err));
            assert!(
                text.contains("sslrootcert=system") && text.contains(mode),
                "the error must name the mode and the parameter: {text}"
            );
        }

        "host=h sslmode=verify-full sslrootcert=system"
            .parse::<Config>()
            .unwrap()
            .validate_tls_settings()
            .expect("verify-full is the mode sslrootcert=system exists for");
    }

    /// A direct TLS handshake sends no `SSLRequest`, so there is no negotiation
    /// to fall back from; libpq refuses to pair it with a mode that permits
    /// plaintext.
    #[test]
    fn direct_negotiation_requires_a_mode_with_no_plaintext_fallback() {
        for mode in ["disable", "allow", "prefer"] {
            format!("host=h sslmode={mode} sslnegotiation=direct")
                .parse::<Config>()
                .unwrap()
                .validate_tls_settings()
                .expect_err("a mode permitting plaintext must not drive a TLS-only handshake");
        }
        for mode in ["require", "verify-ca", "verify-full"] {
            let url = if mode == "require" {
                format!("host=h sslmode={mode} sslnegotiation=direct")
            } else {
                format!("host=h sslmode={mode} sslnegotiation=direct sslrootcert=/ca.pem")
            };
            url.parse::<Config>()
                .unwrap()
                .validate_tls_settings()
                .unwrap_or_else(|e| panic!("sslmode={mode} may use direct negotiation: {e}"));
        }
        assert_eq!(
            "host=h sslnegotiation=direct sslmode=require"
                .parse::<Config>()
                .unwrap()
                .get_ssl_negotiation(),
            SslNegotiation::Direct
        );
    }

    #[test]
    fn test_simple_parsing() {
        let s = "user=pass_user dbname=postgres host=host1,host2 hostaddr=127.0.0.1,127.0.0.2 port=26257";
        let config = s.parse::<Config>().unwrap();
        assert_eq!(Some("pass_user"), config.get_user());
        assert_eq!(Some("postgres"), config.get_dbname());
        assert_eq!(
            [
                Host::Tcp("host1".to_string()),
                Host::Tcp("host2".to_string())
            ],
            config.get_hosts(),
        );

        assert_eq!(
            [
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "127.0.0.2".parse::<IpAddr>().unwrap()
            ],
            config.get_hostaddrs(),
        );

        assert_eq!(1, 1);
    }

    #[test]
    fn test_invalid_hostaddr_parsing() {
        let s = "user=pass_user dbname=postgres host=host1 hostaddr=127.0.0 port=26257";
        s.parse::<Config>().err().unwrap();
    }

    #[test]
    fn tls_file_and_secret_parameters_are_recognised() {
        let config = "host=h sslrootcert=/etc/ca.pem sslcert=/etc/c.pem sslkey=/etc/k.pem \
                      sslpassword=key-secret sslcrl=/etc/root.crl sslcrldir=/etc/crls"
            .parse::<Config>()
            .unwrap();
        assert_eq!(
            config.get_ssl_root_cert(),
            &SslRootCert::File("/etc/ca.pem".to_string())
        );
        assert_eq!(config.get_ssl_cert(), Some("/etc/c.pem"));
        assert_eq!(config.get_ssl_key(), Some("/etc/k.pem"));
        assert_eq!(config.get_ssl_password(), Some(b"key-secret".as_slice()));
        assert_eq!(config.get_ssl_crl(), Some("/etc/root.crl"));
        assert_eq!(config.get_ssl_crl_dir(), Some("/etc/crls"));

        let debug = format!("{config:?}");
        assert!(
            !debug.contains("key-secret"),
            "Debug leaked sslpassword: {debug}"
        );

        let mut built = Config::new();
        built
            .ssl_password(b"built-secret")
            .ssl_crl("/tmp/built.crl")
            .ssl_crl_dir("/tmp/built-crls");
        assert_eq!(built.get_ssl_password(), Some(b"built-secret".as_slice()));
        assert_eq!(built.get_ssl_crl(), Some("/tmp/built.crl"));
        assert_eq!(built.get_ssl_crl_dir(), Some("/tmp/built-crls"));
        assert!(!format!("{built:?}").contains("built-secret"));
    }

    #[test]
    fn tls_protocol_bounds_parse_with_libpq_defaults_and_spelling() {
        let default = Config::new();
        assert_eq!(
            default.get_ssl_min_protocol_version(),
            SslProtocolVersion::TlsV1_2
        );
        assert_eq!(default.get_ssl_max_protocol_version(), None);

        let config = "host=h ssl_min_protocol_version=tlsV1.3 \
                      ssl_max_protocol_version=TLSV1.3"
            .parse::<Config>()
            .unwrap();
        assert_eq!(
            config.get_ssl_min_protocol_version(),
            SslProtocolVersion::TlsV1_3
        );
        assert_eq!(
            config.get_ssl_max_protocol_version(),
            Some(SslProtocolVersion::TlsV1_3)
        );

        let unbounded = "host=h ssl_max_protocol_version=''"
            .parse::<Config>()
            .unwrap();
        assert_eq!(unbounded.get_ssl_max_protocol_version(), None);

        let error = "host=h ssl_min_protocol_version=''"
            .parse::<Config>()
            .expect_err("rustls cannot honour an explicitly unbounded minimum");
        let cause = std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            cause.contains("ssl_min_protocol_version") && cause.contains("rustls"),
            "the refusal must name the unbounded setting: {cause}"
        );
    }

    /// TLS 1.0 and 1.1 are valid libpq values, so accepting and approximating
    /// either one would be more dangerous than treating it as a typo.
    #[test]
    fn rustls_unsupported_protocol_versions_are_refused_by_key() {
        for key in [
            "ssl_min_protocol_version",
            "ssl_max_protocol_version",
        ] {
            for version in ["TLSv1", "TLSv1.1"] {
                let error = format!("host=h {key}={version}")
                    .parse::<Config>()
                    .expect_err("rustls cannot honour TLS 1.0 or TLS 1.1");
                let cause = std::error::Error::source(&error)
                    .map(ToString::to_string)
                    .unwrap_or_default();
                assert!(
                    cause.contains(key) && cause.contains("rustls"),
                    "the refusal must name the unsupported setting: {cause}"
                );
            }
        }
    }

    #[test]
    fn inverted_tls_protocol_range_is_refused_before_connecting() {
        for dsn in [
            "host=h ssl_min_protocol_version=TLSv1.3 ssl_max_protocol_version=TLSv1.2",
            "host=h ssl_max_protocol_version=TLSv1.2 ssl_min_protocol_version=TLSv1.3",
        ] {
            let error = dsn
                .parse::<Config>()
                .unwrap()
                .validate_tls_settings()
                .expect_err("a maximum below the minimum cannot be approximated");
            let cause = std::error::Error::source(&error)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                cause.contains("ssl_min_protocol_version")
                    && cause.contains("ssl_max_protocol_version"),
                "the refusal must name both conflicting bounds: {cause}"
            );
        }
    }

    /// `system` is the one `sslrootcert` value that is a keyword rather than a
    /// path, so this pins that a URL asking for the OS store is parsed as the
    /// OS store and not as a file named "system".
    ///
    /// The default is [`SslRootCert::Unset`], NOT `System`. That is libpq's
    /// shape - its default is a home-directory file that usually does not
    /// exist - and it is what gives `require` its documented meaning: nothing
    /// to verify against, so nothing verified. Defaulting to the OS store
    /// instead would quietly turn `require` into `verify-ca` and would make
    /// every default connection string illegal under the
    /// `sslrootcert=system` rule above.
    #[test]
    fn sslrootcert_system_is_a_keyword_and_the_default_is_unset() {
        assert_eq!(
            "host=h sslrootcert=system".parse::<Config>().unwrap().get_ssl_root_cert(),
            &SslRootCert::System
        );
        assert_eq!(
            "host=h".parse::<Config>().unwrap().get_ssl_root_cert(),
            &SslRootCert::Unset
        );
        assert!(!SslRootCert::Unset.is_configured());
        assert!(SslRootCert::System.is_configured());
        assert!(SslRootCert::File("/ca.pem".into()).is_configured());
    }

    /// The recognised-key set stays closed: a plausible misspelling remains an
    /// error after adding the adjacent libpq parameters.
    #[test]
    fn unknown_ssl_parameters_are_still_rejected() {
        for s in [
            "host=h sslrootcrt=/etc/ca.pem",
            "host=h sslpassphrase=secret",
        ] {
            let err = s.parse::<Config>().err().unwrap_or_else(|| {
                panic!("{s} parsed, so an unrecognised parameter is being ignored")
            });
            let cause = std::error::Error::source(&err)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                cause.contains("unknown option"),
                "{s}: expected an unknown-option error, got {cause:?}"
            );
        }
    }

    /// An empty value is a misconfiguration - almost always an unset shell
    /// variable that expanded to nothing - and must not read as "not set".
    #[test]
    fn empty_tls_file_parameters_are_rejected() {
        for s in [
            "host=h sslrootcert=",
            "host=h sslcert=",
            "host=h sslkey=",
        ] {
            s.parse::<Config>()
                .err()
                .unwrap_or_else(|| panic!("{s} parsed, so an empty path reads as absent"));
        }
    }

    /// libpq treats blank sslpassword, sslcrl, and sslcrldir values as absent
    /// at TLS setup time. Preserve the parsed values so callers can still
    /// distinguish an explicitly blank setting from one that was never
    /// supplied.
    #[test]
    fn empty_optional_tls_paths_parse_but_have_no_tls_effect() {
        let config = "host=h sslpassword='' sslcrl='' sslcrldir=''"
            .parse::<Config>()
            .unwrap();
        assert_eq!(config.get_ssl_password(), Some(b"".as_slice()));
        assert_eq!(config.get_ssl_crl(), Some(""));
        assert_eq!(config.get_ssl_crl_dir(), Some(""));
    }

    #[test]
    fn statement_cache_capacity_is_opt_in_and_parses_in_both_dsn_forms() {
        assert_eq!(Config::new().get_statement_cache_capacity(), 0);
        assert_eq!(
            "host=h statement_cache_capacity=7"
                .parse::<Config>()
                .unwrap()
                .get_statement_cache_capacity(),
            7
        );
        assert_eq!(
            "postgresql://h/db?statement_cache_capacity=9"
                .parse::<Config>()
                .unwrap()
                .get_statement_cache_capacity(),
            9
        );

        "host=h statement_cache_capacity=-1"
            .parse::<Config>()
            .expect_err("a cache capacity cannot be negative");
    }

    #[test]
    fn require_auth_parses_the_postgresql_16_policy_shape() {
        assert_eq!(Config::new().get_require_auth(), &RequireAuth::Any);
        assert_eq!(
            "host=h require_auth=''"
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Any
        );
        assert_eq!(
            "postgresql://h/db?require_auth="
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Any
        );
        assert_eq!(
            "host=h require_auth=   "
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Any
        );

        let all_methods = AuthMethods::new(AuthMethod::Password)
            .with(AuthMethod::Md5)
            .with(AuthMethod::ScramSha256)
            .with(AuthMethod::None);
        assert_eq!(
            "host=h require_auth=password,md5,scram-sha-256,none"
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Require(all_methods)
        );
        assert_eq!(
            "postgresql://h/db?require_auth=!password,!none"
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Reject(
                AuthMethods::new(AuthMethod::Password).with(AuthMethod::None)
            )
        );
        assert_eq!(
            "host=h require_auth=!gss,!sspi"
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Reject(
                AuthMethods::new(AuthMethod::Gss).with(AuthMethod::Sspi)
            )
        );

        let mut built = Config::new();
        built.require_auth(RequireAuth::Require(AuthMethods::new(
            AuthMethod::ScramSha256,
        )));
        assert_eq!(
            built.get_require_auth(),
            &RequireAuth::Require(AuthMethods::new(AuthMethod::ScramSha256))
        );

        let unique = AuthMethods::new(AuthMethod::ScramSha256)
            .with(AuthMethod::None)
            .with(AuthMethod::ScramSha256);
        assert_eq!(
            RequireAuth::Require(unique).to_string(),
            "scram-sha-256,none"
        );
    }

    #[test]
    fn unsupported_positive_require_auth_methods_are_rejected_during_dsn_parsing() {
        for (value, unsupported) in [
            ("gss", "gss"),
            ("sspi", "sspi"),
            ("gss,scram-sha-256", "gss"),
            ("password,sspi", "sspi"),
        ] {
            for dsn in [
                format!("host=h require_auth='{value}'"),
                format!("postgresql://h/db?require_auth={value}"),
            ] {
                let Some(error) = dsn.parse::<Config>().err() else {
                    panic!("unsupported require_auth={value:?} parsed from {dsn:?}");
                };
                let cause = std::error::Error::source(&error)
                    .map(ToString::to_string)
                    .unwrap_or_default();
                assert_eq!(
                    cause,
                    format!(
                        "invalid value for option `require_auth`: authentication method \
                         {unsupported:?} cannot be required because it is not supported by this \
                         driver"
                    ),
                    "require_auth={value:?} produced an unclear error"
                );
            }
        }
    }

    #[test]
    fn malformed_require_auth_lists_are_rejected_during_dsn_parsing() {
        for value in [
            "bogus",
            "password,password",
            "!md5,!md5",
            "none,none",
            "!none,!none",
            "password,!md5",
            "!password,md5",
            "none,!password",
            "password,",
            ",password",
            "password,,md5",
            "!",
            "!!md5",
            "SCRAM-SHA-256",
            "scram-sha-256-plus",
            "oauth",
            " password",
            "password ",
            "password, md5",
        ] {
            let dsn = format!("host=h require_auth='{value}'");
            let error = dsn
                .parse::<Config>()
                .err()
                .unwrap_or_else(|| panic!("malformed require_auth={value:?} parsed"));
            let cause = std::error::Error::source(&error)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                cause.contains("require_auth"),
                "require_auth={value:?} produced an unclear error: {cause:?}"
            );
        }
    }
}

#[cfg(test)]
mod dsn_parse_tests {
    use super::*;

    /// A keyword/value string that cannot be parsed must be REFUSED, not
    /// truncated at the bad token.
    ///
    /// `parameter()` returns `Ok(None)` both when the input is exhausted and
    /// when what remains does not start with a keyword, and the parse loop
    /// reads `None` as end-of-input either way. So everything after the first
    /// malformed token is silently dropped - including a later `sslmode`.
    /// Here the caller asked for `require` and, before the fix, got the
    /// `Prefer` default, which will fall back to an unencrypted connection
    /// against a server that refuses TLS. A connection string that asked for
    /// encryption must never quietly stop asking for it.
    ///
    /// libpq rejects the empty option name rather than treating it as the end
    /// of the string. tokio-postgres 0.7.18 has this same hole (its
    /// `parameter()` at config.rs:963 is identical), so this is a divergence
    /// from libpq that the port inherited rather than one it introduced.
    #[test]
    fn a_malformed_keyword_does_not_silently_drop_the_settings_after_it() {
        let parsed = "host=127.0.0.1 =typo sslmode=require".parse::<Config>();

        // Asserted as "not Ok(sslmode=Prefer)" rather than merely "is Err", so
        // the test names the consequence: the string asked for `require` and
        // the silent-truncation bug answers with the default.
        match parsed {
            Ok(config) => panic!(
                "a malformed keyword parsed as valid, and sslmode silently became {:?} \
                 instead of the requested Require",
                config.get_ssl_mode()
            ),
            Err(_) => {}
        }
    }

    /// The control for the test above: trailing whitespace is genuinely the
    /// end of the input and must still parse. Without this, "reject when the
    /// keyword is empty" could be satisfied by rejecting every string whose
    /// parse loop ends, which would break every well-formed DSN.
    #[test]
    fn trailing_whitespace_is_still_a_complete_connection_string() {
        let config = "host=127.0.0.1 sslmode=require   "
            .parse::<Config>()
            .expect("trailing whitespace made a valid connection string fail");
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
    }

    /// libpq spells the third keepalive knob `keepalives_count`, so a connection
    /// string copied from the PostgreSQL documentation uses that name. We carried
    /// tokio-postgres's `keepalives_retries` instead, and unknown keys are refused,
    /// so such a string was rejected outright rather than tuning the probe count.
    #[test]
    fn the_libpq_spelling_of_the_keepalive_probe_count_is_accepted() {
        for dsn in [
            "host=h keepalives=1 keepalives_count=9",
            "postgresql://h/db?keepalives=1&keepalives_count=9",
        ] {
            let config = dsn
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("{dsn:?} did not parse: {error}"));
            assert_eq!(config.get_keepalives_count(), Some(9), "for {dsn:?}");
        }

        // The control, differing only in the key: the old spelling is gone rather
        // than aliased, so it now fails the same way any unknown key does.
        "host=h keepalives=1 keepalives_retries=9"
            .parse::<Config>()
            .expect_err("keepalives_retries survived as an alias");
    }

    /// libpq falls back to `fallback_application_name` when `application_name`
    /// is unset, so a connection string carrying one was rejected outright as
    /// an unknown key. The getters mirror their setters; which one actually
    /// reaches the server is resolved once, where the startup packet is built,
    /// and is pinned live by `fallback_application_name_names_the_session`.
    #[test]
    fn fallback_application_name_parses_alongside_the_primary() {
        let fallback = "host=h fallback_application_name=faller"
            .parse::<Config>()
            .expect("fallback_application_name did not parse");
        assert_eq!(fallback.get_application_name(), None);
        assert_eq!(fallback.get_fallback_application_name(), Some("faller"));

        // One variable apart: naming both keeps them distinct rather than
        // letting the later key overwrite the earlier.
        let both = "host=h application_name=primary fallback_application_name=faller"
            .parse::<Config>()
            .expect("the pair did not parse");
        assert_eq!(both.get_application_name(), Some("primary"));
        assert_eq!(both.get_fallback_application_name(), Some("faller"));
    }

    /// `client_encoding` is one of the most common libpq keys, and was refused.
    /// The startup packet always announces UTF8 because Rust strings are UTF-8,
    /// so naming UTF8 is accepted and any other encoding is refused rather than
    /// silently decoded as something it is not.
    #[test]
    fn client_encoding_accepts_utf8_and_refuses_an_encoding_we_cannot_decode() {
        for spelling in ["UTF8", "utf8", "UNICODE", "utf-8"] {
            format!("host=h client_encoding={spelling}")
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("client_encoding={spelling} did not parse: {error}"));
        }

        let refused = "host=h client_encoding=LATIN1"
            .parse::<Config>()
            .expect_err("a non-UTF8 client_encoding must be refused, not ignored");
        // The top-level Display is only "invalid connection string", so the key
        // has to be reachable through the source chain or the caller cannot
        // tell which option they got wrong.
        let named = std::iter::successors(
            std::error::Error::source(&refused),
            |error| std::error::Error::source(*error),
        )
        .any(|cause| cause.to_string().contains("client_encoding"));
        assert!(named, "no cause names the offending key: {refused}");
    }
}
