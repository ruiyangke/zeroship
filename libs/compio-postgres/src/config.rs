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
use std::num::NonZeroUsize;
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

/// Whether a TLS client certificate may or must be sent.
///
/// This is libpq's `sslcertmode`. [`Require`](SslCertMode::Require) is not
/// merely a requirement that certificate files exist: a successful connection
/// must prove that the server requested a certificate and the TLS client
/// selected one to send.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SslCertMode {
    /// Never send a client certificate, even when one is configured.
    Disable,
    /// Send a configured client certificate when the server requests one.
    #[default]
    Allow,
    /// Require the server to request, and the client to send, a certificate.
    Require,
}

impl SslCertMode {
    /// The spelling this mode has in a connection string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Require => "require",
        }
    }
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

/// A PostgreSQL frontend/backend wire-protocol version.
///
/// Protocol 3.1 is reserved and was never implemented by PostgreSQL, so the
/// supported set deliberately jumps from 3.0 to 3.2.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ProtocolVersion {
    /// Protocol 3.0, supported by PostgreSQL 7.4 and later.
    V3_0,
    /// Protocol 3.2, supported by PostgreSQL 18 and later.
    V3_2,
}

impl ProtocolVersion {
    /// The spelling libpq uses in a connection string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V3_0 => "3.0",
            Self::V3_2 => "3.2",
        }
    }

    pub(crate) const fn as_wire(self) -> u32 {
        match self {
            Self::V3_0 => 0x0003_0000,
            Self::V3_2 => 0x0003_0002,
        }
    }

    pub(crate) const fn from_wire(version: u32) -> Option<Self> {
        match version {
            0x0003_0000 => Some(Self::V3_0),
            0x0003_0002 => Some(Self::V3_2),
            _ => None,
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
/// `START_REPLICATION`, `CREATE_REPLICATION_SLOT`, ...) are issued via
/// the simple-query path and the connection returns
/// `CopyBothResponse` for `START_REPLICATION`.
///
/// See: https://www.postgresql.org/docs/16/protocol-replication.html
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplicationMode {
    /// Physical replication. Streams the raw WAL - not used by
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
/// * `passfile` - Path to a password file to read the password from when none is set. Ignored if the file is
///     group- or world-accessible, as libpq ignores it. NO default location is searched; see "The environment".
/// * `service` - Name of a `pg_service.conf` section supplying connection parameters. Parameters given explicitly
///     win over the service's, whatever order they appear in. A service that is not defined is an error, and so is
///     naming one without a `servicefile`.
/// * `servicefile` - Path of the `pg_service.conf` a `service` is read from. NOT a libpq parameter: libpq finds this
///     file through the environment and this driver does not read the environment.
/// * `sslkeylogfile` - File to append this session's TLS secrets to, in NSS key-log format, so a capture can be
///     decrypted. DEFEATS THE CONFIDENTIALITY OF THE CONNECTION; for deliberate diagnosis only.
/// * `dbname` - The name of the database to connect to. Defaults to the username.
/// * `options` - Command line options used to configure the server.
/// * `application_name` - Sets the `application_name` parameter on the server.
/// * `fallback_application_name` - The `application_name` to announce when none is set, so a library can name the
///     session without overriding the application's own choice.
/// * `client_encoding` - Accepted only as `UTF8` (or `UNICODE`). The startup packet always announces UTF8 because
///     Rust strings are UTF-8; any other encoding is refused rather than mis-decoded.
/// * `statement_cache_capacity` - Maximum number of implicit raw-SQL prepared
///     statements retained per connection. Defaults to 0 (disabled).
/// * `min_protocol_version` - Oldest PostgreSQL wire protocol the connection
///     may use: `3.0`, `3.2`, or `latest`. Defaults to `3.0`.
/// * `max_protocol_version` - PostgreSQL wire protocol requested at startup:
///     `3.0`, `3.2`, or `latest`. Defaults to `3.2`.
/// * `sslmode` - Controls usage of TLS, with libpq's six values and libpq's meanings: `disable`, `allow`, `prefer`
///     (the default), `require`, `verify-ca`, `verify-full`. See [`SslMode`] for what each one does, and
///     [`SslRootCert`] for the certificate-verification table - in particular, `require` encrypts but does *not*
///     authenticate the server unless `sslrootcert` names trust anchors.
/// * `sslrootcert` - Trust anchors for server-certificate verification: a path to a PEM file, or the keyword `system`
///     for the operating system's store. Unset by default, which means no verification for `require`/`prefer`/`allow`
///     and an error for `verify-ca`/`verify-full`. Point it at your CA (or at the server's own certificate, if
///     self-signed) when the server does not chain to a public root. `system` may only be combined with
///     `sslmode=verify-full`.
/// * `sslcert` - Path to the client certificate chain (PEM) for
///     client-certificate authentication. Unless `sslcertmode=disable`, it
///     must be given together with `sslkey`.
/// * `sslkey` - Path to the private key (PEM) matching `sslcert`. Unless
///     `sslcertmode=disable`, it must be given together with `sslcert`.
/// * `sslcertmode` - Whether a client certificate is disabled, allowed (the
///     default), or required. `require` also verifies that the server requested
///     the certificate and that the client sent it. This crate does not load
///     libpq's default `~/.postgresql` identity, so `require` needs explicit
///     `sslcert` and `sslkey` paths when using the built-in rustls connector.
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
/// * `sslsni` - Whether TLS ClientHello messages for DNS names carry the Server
///     Name Indication extension. Accepts `1` (the default) or `0`; IP server
///     names never produce SNI.
/// * `requirepeer` - On Linux Unix-domain sockets, require the connected server
///     process to be owned by this operating-system user. Other Unix platforms
///     fail loudly until their peer-credential API is implemented. As in
///     libpq, the setting is ignored on TCP connections.
/// * `host` - The host to connect to. On Unix platforms, if the host starts with a `/` character it is treated as the
///     path to the directory containing Unix domain sockets. On Linux, `@` selects the abstract Unix-socket namespace.
///     Otherwise, it is treated as a hostname. Multiple hosts can be specified, separated by commas. By default they
///     are tried in order; `load_balance_hosts=random` randomizes them. Either `host` or `hostaddr` is required by
///     this driver's `connect` method.
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
///     In a comma-separated list, an empty item selects the corresponding `host` item.
///     If this parameter is not specified, the value of `host` will be looked up to find the corresponding IP address,
///     or if host specifies an IP address, that value will be used directly.
///     Using `hostaddr` allows the application to avoid a host name look-up, which might be important in applications
///     with time constraints. However, a host name is required for
///     `verify-full` TLS certificate verification.
///     Specifically:
///         * If `hostaddr` is specified without `host`, the value for `hostaddr` gives the server network address.
///             The connection attempt will fail with `sslmode=verify-full`;
///         * If `host` is specified without `hostaddr`, a host name lookup occurs;
///         * If both `host` and `hostaddr` are specified, the value for `hostaddr` gives the server network address.
///             The value for `host` remains the TLS/SNI identity and the host
///             used for password-file matching.
/// * `port` - The port to connect to. Multiple ports can be specified, separated by commas. The number of ports must be
///     either 1, in which case it will be used for all configured endpoints, or the same as the number of `host` or
///     `hostaddr` entries. Defaults to 5432 if omitted or the empty string.
/// * `connect_timeout` - The time limit in seconds applied to each address tried, covering TLS negotiation, startup,
///     and authentication, and applied once more to each host entry's name resolution. Hostnames can resolve to
///     multiple IP addresses, and the limit restarts for each, as libpq's does. Defaults to no timeout.
/// * `tcp_user_timeout` - The time limit that transmitted data may remain unacknowledged before a connection is
///     forcibly closed, in MILLISECONDS. The one member of this family that is not seconds - `keepalives_idle`,
///     `keepalives_interval` and `connect_timeout` are all seconds.
///     This is ignored for Unix domain socket connections. It is only supported on systems where TCP_USER_TIMEOUT is available
///     and will default to the system default if omitted or set to 0; on other systems, it has no effect.
/// * `keepalives` - Controls the use of TCP keepalive. A value of 0 disables keepalive and nonzero integers enable it.
///     This option is ignored when connecting with Unix sockets. Defaults to on.
/// * `keepalives_idle` - The number of seconds of inactivity after which a keepalive message is sent to the server.
///     This option is ignored when connecting with Unix sockets. By default the operating-system setting is left unchanged.
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
/// # The environment
///
/// libpq takes a default for nearly every parameter above from the environment:
/// `PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE`, `PGPASSWORD`, `PGSSLMODE`, `PGAPPNAME` and the rest.
///
/// **This driver reads NONE of them - it reads no environment variable at all.** It is a library,
/// not a command-line tool: a connection whose target depends on ambient process state is one whose
/// target cannot be read off the call site, and a stray `PGDATABASE` in a service manager's
/// environment would silently redirect every connection the process makes. Everything that decides
/// where this driver connects, and as whom, comes from the `Config` or the connection string.
///
/// That is not only a preference here, it is a workspace rule with a gate behind it: a published
/// library takes resolved options from its caller, and `crates/zeroship-core/tests/config_env_access_gate.rs`
/// rejects a raw environment read anywhere in this crate's sources, against an exemption list that
/// is deliberately empty.
///
/// The consequence is that the file-valued parameters need their paths GIVEN, since there is
/// nothing else to find them with: `passfile` names the password file, and `servicefile` names the
/// `pg_service.conf` that a `service` is read from. An application that wants libpq's search order
/// (`$PGPASSFILE` then `~/.pgpass`; `$PGSERVICEFILE` then `~/.pg_service.conf` then
/// `$PGSYSCONFDIR/pg_service.conf`) performs it and passes the winner - which leaves the decision
/// to read a user's environment with the program that has one.
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
    /// Path to a libpq password file. `None` means no password file is read;
    /// this driver searches no default location.
    pub(crate) passfile: Option<String>,
    /// Name of a `pg_service.conf` section supplying connection parameters.
    pub(crate) service: Option<String>,
    /// Path of the `pg_service.conf` a named service is read from. This crate
    /// never searches for one; see `service.rs`.
    pub(crate) service_file: Option<String>,
    /// Path to write TLS secrets to, in NSS key-log format, for debugging.
    pub(crate) ssl_key_log_file: Option<String>,
    /// Largest single backend message to accept, or `None` for the default.
    pub(crate) max_message_size: Option<usize>,
    pub(crate) dbname: Option<String>,
    pub(crate) options: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) fallback_application_name: Option<String>,
    pub(crate) statement_cache_capacity: usize,
    pub(crate) statement_cache_execution_threshold: NonZeroUsize,
    pub(crate) ssl_mode: SslMode,
    /// Whether the caller selected `sslmode`, as opposed to observing its
    /// compiled default. `sslrootcert=system` may strengthen only the latter.
    pub(crate) ssl_mode_explicit: bool,
    pub(crate) ssl_negotiation: SslNegotiation,
    pub(crate) ssl_root_cert: SslRootCert,
    pub(crate) ssl_cert: Option<String>,
    pub(crate) ssl_key: Option<String>,
    pub(crate) ssl_cert_mode: SslCertMode,
    pub(crate) ssl_password: Option<Vec<u8>>,
    pub(crate) ssl_crl: Option<String>,
    pub(crate) ssl_crl_dir: Option<String>,
    pub(crate) min_protocol_version: ProtocolVersion,
    pub(crate) max_protocol_version: ProtocolVersion,
    pub(crate) ssl_min_protocol_version: SslProtocolVersion,
    pub(crate) ssl_max_protocol_version: Option<SslProtocolVersion>,
    pub(crate) ssl_sni: bool,
    pub(crate) require_peer: Option<String>,
    pub(crate) host: Vec<Host>,
    /// Positional `hostaddr` entries. `None` is an explicitly empty slot,
    /// which tells libpq-compatible endpoint selection to use the matching
    /// `host` entry instead.
    pub(crate) hostaddr: Vec<Option<IpAddr>>,
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
            passfile: None,
            service: None,
            service_file: None,
            ssl_key_log_file: None,
            max_message_size: None,
            dbname: None,
            options: None,
            application_name: None,
            fallback_application_name: None,
            statement_cache_capacity: 0,
            statement_cache_execution_threshold: NonZeroUsize::MIN,
            ssl_mode: SslMode::Prefer,
            ssl_mode_explicit: false,
            ssl_negotiation: SslNegotiation::Postgres,
            ssl_root_cert: SslRootCert::Unset,
            ssl_cert: None,
            ssl_key: None,
            ssl_cert_mode: SslCertMode::Allow,
            ssl_password: None,
            ssl_crl: None,
            ssl_crl_dir: None,
            min_protocol_version: ProtocolVersion::V3_0,
            max_protocol_version: ProtocolVersion::V3_2,
            ssl_min_protocol_version: SslProtocolVersion::TlsV1_2,
            ssl_max_protocol_version: None,
            ssl_sni: true,
            require_peer: None,
            host: vec![],
            hostaddr: vec![],
            port: vec![],
            connect_timeout: None,
            read_timeout: None,
            tcp_user_timeout: None,
            keepalives: true,
            #[cfg(not(target_arch = "wasm32"))]
            keepalive_config: KeepaliveConfig {
                idle: Duration::ZERO,
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
        let user = user.into();
        self.user = (!user.is_empty()).then_some(user);
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
        let password = password.as_ref();
        self.password = (!password.is_empty()).then(|| password.to_vec());
        self
    }

    /// Gets the password to authenticate with, if one has been configured with
    /// the `password` method.
    pub fn get_password(&self) -> Option<&[u8]> {
        self.password.as_deref()
    }

    /// Sets the path of the password file to read a missing password from.
    ///
    /// Consulted only when no password is set. No default location is
    /// searched: this driver reads no environment, so libpq's `$PGPASSFILE`
    /// and `~/.pgpass` are the caller's to resolve.
    pub fn passfile(&mut self, passfile: impl Into<String>) -> &mut Config {
        self.passfile = Some(passfile.into());
        self
    }

    /// Gets the password file path, if one has been configured.
    ///
    /// `None` means no password file is consulted at all.
    pub fn get_passfile(&self) -> Option<&str> {
        self.passfile.as_deref()
    }

    /// Sets the `pg_service.conf` section to take connection parameters from.
    ///
    /// Setting this on an already-built `Config` records the name but does not
    /// read the file; the service is expanded when a connection string naming
    /// it is PARSED, because that is the only point at which "which parameters
    /// did the caller give explicitly" is still known.
    pub fn service(&mut self, service: impl Into<String>) -> &mut Config {
        self.service = Some(service.into());
        self
    }

    /// Gets the service name, if one was given.
    pub fn get_service(&self) -> Option<&str> {
        self.service.as_deref()
    }

    /// Sets the `pg_service.conf` file that [`Config::service`] is read from.
    ///
    /// Required whenever a service is named: this driver does not search
    /// `$PGSERVICEFILE`, `~/.pg_service.conf` or `$PGSYSCONFDIR` for one. An
    /// application that wants libpq's search order performs it and passes the
    /// winner here - see the "The environment" section above for why that
    /// division exists.
    pub fn service_file(&mut self, path: impl Into<String>) -> &mut Config {
        self.service_file = Some(path.into());
        self
    }

    /// Gets the service file path, if one was set.
    pub fn get_service_file(&self) -> Option<&str> {
        self.service_file.as_deref()
    }

    /// Sets a file to write this connection's TLS secrets to, in NSS key-log
    /// format, so a packet capture of it can be decrypted.
    ///
    /// THIS DEFEATS THE CONFIDENTIALITY OF THE CONNECTION. Anyone who can read
    /// the file can decrypt any capture of any session it describes, including
    /// captures taken before the file was read. It exists because diagnosing a
    /// protocol fault otherwise means reproducing it without encryption, and
    /// libpq offers the same switch under the same name. Turn it on
    /// deliberately, for a diagnosis, and delete the file afterwards.
    ///
    /// Has no effect unless the `tls` feature is enabled and the connection
    /// actually negotiates TLS.
    pub fn ssl_key_log_file(&mut self, path: impl Into<String>) -> &mut Config {
        self.ssl_key_log_file = Some(path.into());
        self
    }

    /// Gets the TLS key-log file path, if one was set.
    pub fn get_ssl_key_log_file(&self) -> Option<&str> {
        self.ssl_key_log_file.as_deref()
    }

    /// Sets the largest single backend message accepted for application
    /// traffic after connection setup.
    ///
    /// Startup, authentication, and target-session probing always use the
    /// default ceiling. The data phase defaults to 64 MiB. The cap exists
    /// because a message's length field is read BEFORE its body, so a server
    /// claiming a multi-GB message would otherwise have the driver allocate
    /// for it; rejecting on the header bounds that for nothing.
    ///
    /// Raise it when the rows are genuinely large - PostgreSQL will send a
    /// single value of up to 1 GiB, and at the default such a value cannot be
    /// read at all. Raising it trades that ceiling for the memory a hostile or
    /// broken server could make one connection reserve, so raise it to what
    /// the data needs rather than to the maximum.
    pub fn max_message_size(&mut self, max_message_size: usize) -> &mut Config {
        self.max_message_size = Some(max_message_size);
        self
    }

    /// Gets the configured maximum message size, if one was set. `None` means
    /// the 64 MiB default applies.
    pub fn get_max_message_size(&self) -> Option<usize> {
        self.max_message_size
    }

    /// Fill parameters the caller did not give from the named service.
    ///
    /// `explicit` is every key the connection string itself supplied. Those
    /// win REGARDLESS OF ORDER - measured against libpq, where
    /// `dbname=x service=s` and `service=s dbname=x` both select `x` - which
    /// is why this runs after the string is fully parsed rather than at the
    /// point the `service` key is seen.
    fn apply_service(&mut self, explicit: &[String]) -> Result<(), Error> {
        let Some(service) = self.service.clone() else {
            return Ok(());
        };

        let Some(path) = self.service_file.clone() else {
            return Err(Error::config(Box::new(
                crate::service::ServiceError::NoServiceFile { service },
            )));
        };

        let parameters = crate::service::parameters(&service, std::path::Path::new(&path))
            .map_err(|e| Error::config(Box::new(e)))?;

        self.fill_unset(parameters, explicit)
    }

    /// Apply `parameters` for every key not already given explicitly.
    ///
    /// Split from [`Config::apply_service`] so the precedence rule can be
    /// tested on its own: locating the service file reads process-wide
    /// environment variables, and this half - the part with a rule in it -
    /// does not.
    fn fill_unset(
        &mut self,
        parameters: Vec<(String, String)>,
        explicit: &[String],
    ) -> Result<(), Error> {
        // A section may name one key twice, and libpq takes the FIRST of them.
        // Applying each pair as it arrives would take the last.
        let mut applied: Vec<&str> = Vec::new();

        for (key, value) in &parameters {
            if key == "service" {
                return Err(Error::config(Box::new(NestedService)));
            }
            if let Some(key) = match key.as_str() {
                "requiressl" => Some("requiressl"),
                "servicefile" => Some("servicefile"),
                _ => None,
            } {
                return Err(Error::config(Box::new(InvalidServiceOption(key))));
            }
            let canonical_key = canonical_parameter_key(key);
            if explicit
                .iter()
                .any(|given| canonical_parameter_key(given) == canonical_key)
                || applied
                    .iter()
                    .any(|given| canonical_parameter_key(given) == canonical_key)
            {
                continue;
            }
            self.param(key, value)?;
            applied.push(key);
        }

        Ok(())
    }

    /// Sets the name of the database to connect to.
    ///
    /// Defaults to the user.
    pub fn dbname(&mut self, dbname: impl Into<String>) -> &mut Config {
        let dbname = dbname.into();
        self.dbname = (!dbname.is_empty()).then_some(dbname);
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
    /// [`Config::statement_cache_execution_threshold`] controls which
    /// execution first earns a prepared slot. It defaults to 1, preserving
    /// immediate preparation. Higher values execute through PostgreSQL's
    /// unnamed statement below the threshold. Admission history is a separate
    /// 100-entry LRU per connection and is forgotten when a prepared entry is
    /// evicted, so generated SQL cannot grow the tracker without bound or
    /// return immediately after eviction to displace the new working set.
    ///
    /// The cache is disabled by default. Keep it disabled when connecting
    /// through a transaction-mode connection pooler: persistent named
    /// statements are scoped to a `PostgreSQL` session, while successive
    /// transactions through such a pooler may use different sessions.
    ///
    /// If `PostgreSQL` reports that a cached statement's server-side name is
    /// missing (`26000` from `FetchPreparedStatement`), the stale entry is
    /// evicted and the operation is prepared and run again, once. A changed
    /// result type (`0A000` from the plan-cache routines) receives the same
    /// recovery only when the statement has no parameters. A second failure
    /// propagates.
    ///
    /// Nothing is retried inside a transaction, after `BindComplete`, or for a
    /// matching SQLSTATE raised by application code. A parameterized 0A000 is
    /// also propagated: PostgreSQL can run user-defined domain input checks
    /// before it revalidates the plan, and their nontransactional side effects
    /// cannot be replayed safely. The stale entry is still evicted, so the next
    /// call prepares against the current result shape.
    pub const fn statement_cache_capacity(&mut self, capacity: usize) -> &mut Config {
        self.statement_cache_capacity = capacity;
        self
    }

    /// Gets the per-connection prepared-statement cache capacity.
    #[must_use]
    pub const fn get_statement_cache_capacity(&self) -> usize {
        self.statement_cache_capacity
    }

    /// Sets the execution which first promotes exact SQL into the implicit
    /// prepared-statement cache.
    ///
    /// A threshold of 1 prepares the first execution and is the default. Zero
    /// has no useful meaning here: cache capacity zero already disables the
    /// cache, so the type rejects zero instead of silently aliasing it.
    /// Executions below the threshold use PostgreSQL's unnamed statement and
    /// do not consume a prepared slot. Here an execution is a valid raw-SQL
    /// operation with matching parameter arity; the counter advances before
    /// parameter-value encoding or server completion. A validated
    /// [`Transaction::bind`](crate::Transaction::bind) counts once because
    /// promotion avoids its repeated Parse/Describe work, even if its portal
    /// is never executed. This is programmatic-only because libpq defines no
    /// equivalent connection-string parameter.
    pub const fn statement_cache_execution_threshold(
        &mut self,
        threshold: NonZeroUsize,
    ) -> &mut Config {
        self.statement_cache_execution_threshold = threshold;
        self
    }

    /// Gets the per-connection statement-cache execution threshold.
    ///
    /// Defaults to 1.
    #[must_use]
    pub const fn get_statement_cache_execution_threshold(&self) -> NonZeroUsize {
        self.statement_cache_execution_threshold
    }

    /// Sets the SSL configuration.
    ///
    /// Defaults to `prefer`, except that `sslrootcert=system` strengthens an
    /// otherwise implicit default to `verify-full` as libpq does.
    pub fn ssl_mode(&mut self, ssl_mode: SslMode) -> &mut Config {
        self.ssl_mode = ssl_mode;
        self.ssl_mode_explicit = true;
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
        if !self.ssl_mode_explicit {
            self.ssl_mode = if self.ssl_root_cert == SslRootCert::System {
                SslMode::VerifyFull
            } else {
                SslMode::Prefer
            };
        }
        self
    }

    /// Gets the trust anchors used to verify the server's certificate.
    pub fn get_ssl_root_cert(&self) -> &SslRootCert {
        &self.ssl_root_cert
    }

    /// Sets the path to the client certificate chain (PEM) sent to the server.
    ///
    /// Must be paired with [`Config::ssl_key`] unless
    /// [`SslCertMode::Disable`] is selected.
    pub fn ssl_cert(&mut self, ssl_cert: impl Into<String>) -> &mut Config {
        self.ssl_cert = Some(ssl_cert.into());
        self
    }

    /// Gets the path to the client certificate chain, if one has been set.
    pub fn get_ssl_cert(&self) -> Option<&str> {
        self.ssl_cert.as_deref()
    }

    /// Sets the path to the client private key (PEM) matching [`Config::ssl_cert`].
    ///
    /// Must be paired with [`Config::ssl_cert`] unless
    /// [`SslCertMode::Disable`] is selected. OpenSSL `engine:key` specifiers
    /// accepted by libpq are not supported and are refused before connecting.
    pub fn ssl_key(&mut self, ssl_key: impl Into<String>) -> &mut Config {
        self.ssl_key = Some(ssl_key.into());
        self
    }

    /// Gets the path to the client private key, if one has been set.
    pub fn get_ssl_key(&self) -> Option<&str> {
        self.ssl_key.as_deref()
    }

    /// Sets whether a TLS client certificate may or must be sent.
    ///
    /// The built-in rustls connector requires explicit [`Config::ssl_cert`]
    /// and [`Config::ssl_key`] paths for [`SslCertMode::Require`]; unlike
    /// libpq, this crate does not search `~/.postgresql` for a default identity.
    ///
    /// Defaults to [`SslCertMode::Allow`].
    pub fn ssl_cert_mode(&mut self, ssl_cert_mode: SslCertMode) -> &mut Config {
        self.ssl_cert_mode = ssl_cert_mode;
        self
    }

    /// Gets the client-certificate mode.
    pub fn get_ssl_cert_mode(&self) -> SslCertMode {
        self.ssl_cert_mode
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

    /// Sets the oldest PostgreSQL wire protocol the connection may use.
    ///
    /// Defaults to [`ProtocolVersion::V3_0`], allowing a server with protocol
    /// negotiation support to negotiate the connection down to 3.0.
    pub fn min_protocol_version(&mut self, version: ProtocolVersion) -> &mut Config {
        self.min_protocol_version = version;
        self
    }

    /// Gets the minimum permitted PostgreSQL wire-protocol version.
    pub fn get_min_protocol_version(&self) -> ProtocolVersion {
        self.min_protocol_version
    }

    /// Sets the PostgreSQL wire-protocol version requested at startup.
    ///
    /// Defaults to [`ProtocolVersion::V3_2`], which is a DELIBERATE DIVERGENCE
    /// from libpq: libpq 18 defaults to `3.0` and makes 3.2 opt-in. Measured
    /// 2026-08-25 with `psql -c '\conninfo'`, which reports the negotiated
    /// version - no protocol settings gives `3.0` against an 18.4 server that
    /// would happily speak 3.2, while `max_protocol_version=3.2` gives `3.2`.
    ///
    /// Requesting 3.2 by default is worth the divergence because 3.2 is what
    /// carries the longer cancel key; 3.0's is a fixed 32 bits, which is
    /// brute-forceable, and that is why upstream lengthened it. Nothing is lost
    /// when a peer answers `NegotiateProtocolVersion`: the session continues on
    /// 3.0. `PostgreSQL` versions before 9.3.21 reject the newer minor, and other
    /// peers without negotiation support may do the same; use
    /// `max_protocol_version=3.0` for those peers.
    ///
    /// The fallback was measured in every shape this driver is deployed in,
    /// because a default that breaks a pooler is not a default: direct to
    /// PostgreSQL 16.14 negotiates down to 3.0, and so does PgBouncer in front
    /// of either 16.14 or 18.4 - the pooler does not speak 3.2 and negotiates
    /// the client down rather than refusing.
    pub fn max_protocol_version(&mut self, version: ProtocolVersion) -> &mut Config {
        self.max_protocol_version = version;
        self
    }

    /// Gets the PostgreSQL wire-protocol version requested at startup.
    pub fn get_max_protocol_version(&self) -> ProtocolVersion {
        self.max_protocol_version
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

    /// Sets whether TLS handshakes for DNS names send the Server Name
    /// Indication extension. IP server names never send SNI.
    ///
    /// Defaults to `true`.
    pub fn ssl_sni(&mut self, ssl_sni: bool) -> &mut Config {
        self.ssl_sni = ssl_sni;
        self
    }

    /// Gets whether TLS handshakes for DNS names may send Server Name
    /// Indication.
    pub fn get_ssl_sni(&self) -> bool {
        self.ssl_sni
    }

    /// Requires a Linux Unix-domain server socket's peer process to be owned
    /// by the named operating-system user.
    ///
    /// An empty name disables the check. The setting is ignored for TCP,
    /// matching libpq's address-family-specific behavior. Other Unix platforms
    /// refuse the setting until an equivalent peer-credential check is
    /// implemented.
    pub fn require_peer(&mut self, require_peer: impl Into<String>) -> &mut Config {
        let require_peer = require_peer.into();
        self.require_peer = (!require_peer.is_empty()).then_some(require_peer);
        self
    }

    /// Gets the required Unix-domain peer user, if one has been configured.
    pub fn get_require_peer(&self) -> Option<&str> {
        self.require_peer.as_deref()
    }

    /// Adds a host to the configuration.
    ///
    /// Multiple hosts can be specified by calling this method multiple times.
    /// They are tried in insertion order unless
    /// [`LoadBalanceHosts::Random`] is selected. On Unix systems, a host
    /// starting with a `/` is interpreted as a path to a directory containing
    /// Unix domain sockets. On Linux, a host starting with `@` selects the
    /// abstract Unix-socket namespace. When both hosts and hostaddrs are
    /// present, they must have the same length.
    pub fn host(&mut self, host: impl Into<String>) -> &mut Config {
        let host = host.into();

        #[cfg(unix)]
        {
            if host.starts_with('/') {
                return self.host_path(host);
            }
        }

        #[cfg(target_os = "linux")]
        if let Some(name) = host.strip_prefix('@') {
            return self.host_abstract(name.as_bytes());
        }

        self.host.push(Host::Tcp(host));
        self
    }

    /// Gets the hosts that have been added to the configuration with `host`.
    pub fn get_hosts(&self) -> &[Host] {
        &self.host
    }

    /// Gets the positional hostaddr entries in the configuration.
    ///
    /// An explicitly empty item in a parsed comma-separated list is `None` and
    /// selects the corresponding `host`. Programmatic calls to
    /// [`Config::hostaddr`] always append `Some`.
    pub fn get_hostaddrs(&self) -> &[Option<IpAddr>] {
        &self.hostaddr
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

    #[cfg(target_os = "linux")]
    fn host_abstract(&mut self, name: &[u8]) -> &mut Config {
        let mut path = Vec::with_capacity(name.len() + 1);
        path.push(0);
        path.extend_from_slice(name);
        self.host_path(OsStr::from_bytes(&path))
    }

    /// Adds a hostaddr to the configuration.
    ///
    /// Multiple hostaddrs can be specified by calling this method multiple
    /// times. They are tried in insertion order unless
    /// [`LoadBalanceHosts::Random`] is selected. When both hostaddrs and hosts
    /// are present, they must have the same length.
    pub fn hostaddr(&mut self, hostaddr: IpAddr) -> &mut Config {
        self.hostaddr.push(Some(hostaddr));
        self
    }

    /// Adds a port to the configuration.
    ///
    /// Multiple ports can be specified by calling this method multiple times.
    /// There must either be no ports, in which case the default of 5432 is
    /// used, a single port, in which case it is used for every endpoint, or the
    /// same number of ports as configured host or hostaddr entries.
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
    /// no limit. A zero duration likewise clears the limit.
    pub fn connect_timeout(&mut self, connect_timeout: Duration) -> &mut Config {
        self.connect_timeout = (!connect_timeout.is_zero()).then_some(connect_timeout);
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
        self.tcp_user_timeout = (!tcp_user_timeout.is_zero()).then_some(tcp_user_timeout);
        self
    }

    /// Gets the TCP user timeout, if one has been set with
    /// [`Config::tcp_user_timeout`].
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
    /// This is ignored for Unix domain sockets, or if the `keepalives` option is disabled.
    /// By default the operating-system setting is left unchanged.
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
    /// On Windows, this sets the value of the tcp_keepalive struct's keepaliveinterval field.
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
    /// `START_REPLICATION`, ...) through the dedicated replication client in
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

    /// NO CALLER EVER DELIVERS THE SAME KEY TWICE, so an arm here cannot
    /// "override an earlier occurrence" and a test shaped `key=A key=B` cannot
    /// prove that it does. Measured 2026-08-26 across all three routes:
    ///
    /// * keyword strings collapse duplicates to the LAST value - `host=a host=b`
    ///   yields ONE host, and `application_name=first application_name=second`
    ///   yields `"second"`, so the first value is never applied at all;
    /// * `fill_unset` (service files) skips a key already in `applied` or
    ///   `explicit`, deliberately, because libpq takes the FIRST of a repeated
    ///   service key;
    /// * a URI's query replaces the authority before anything reaches here, so
    ///   `postgres://h:5455/db?port=` is one `port` application, not two.
    ///
    /// The consequence is a live trap: revert an override arm and its
    /// `key=A key=B` test still passes, which reads as a proved regression
    /// guard. Two such tests are marked NON-DISCRIMINATING below.
    ///
    /// THEY WERE NOT WRITTEN THAT WAY. The collapse arrived with
    /// `ParsedParameters::insert`, which removes an existing key before
    /// pushing; before it, the DSN loop called `param` once per OCCURRENCE.
    /// Both tests were measured failing against their own fix reverted, at the
    /// commits that introduced them - so each was a working guard that a later
    /// refactor in the same series silently disarmed, with the suite green the
    /// whole way. That is the durable hazard here: collapsing duplicates is
    /// correct, and it costs coverage somewhere far from the change.
    ///
    /// To exercise an override now, drive the SETTER
    /// (`Config::connect_timeout`) - that path is still reachable and its
    /// tests do discriminate.
    fn param(&mut self, key: &str, value: &str) -> Result<(), Error> {
        // libpq's treatment of `key=` is per-OPTION, not uniform. `port=` uses
        // the compiled default, while the six socket integer options reject an
        // empty value. Enums reject it too; strings generally keep it.
        if value.is_empty() && key == "port" {
            // Empty selects the compiled default. Clearing is observable when
            // it overrides an earlier occurrence in the same string.
            self.port.clear();
            return Ok(());
        }

        const EMPTY_MEANS_UNSET: &[&str] = &["statement_cache_capacity", "max_message_size"];
        if value.is_empty() && EMPTY_MEANS_UNSET.contains(&key) {
            return Ok(());
        }

        match key {
            // An EMPTY credential means UNSET, not "a user whose name is the
            // empty string". libpq resolves `?user=` by falling back to the
            // operating-system user -- measured: psql on
            // `postgres://:pw@127.0.0.1/postgres` fails with
            // `role "root" does not exist`, the OS user, never having tried an
            // empty one. Storing `Some("")` suppressed the same `whoami`
            // fallback in `connect_raw`, so the driver sent an empty user name
            // the server was certain to reject, and an empty password produced
            // "password authentication failed" where libpq says
            // "no password supplied".
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
                // VERBATIM, backslashes included. libpq documents that a space
                // inside an option value must be escaped with a backslash, which
                // reads like something the client unescapes before sending. It
                // does not: the escape travels to the server and `pg_split_opts`
                // splits on it there. MEASURED 2026-08-26 against the review container's libpq,
                // `options=-c search_path=a\ b` reaches the backend as ONE
                // argument whose value contains a space (`invalid value for
                // parameter "search_path": "a b"`), while the same string with a
                // raw space splits into two (`invalid command-line argument for
                // server process: b`). Had libpq unescaped client-side the first
                // case would have split exactly like the second. So unescaping
                // here would corrupt every escaped option.
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
                if !is_decodable_encoding(value) {
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
            // NOT a libpq key; libpq imposes no such ceiling. See
            // `Config::max_message_size`. Zero is refused rather than taken to
            // mean "unlimited": every message carries a header, so a limit of
            // zero would reject the connection's own first frame, and a caller
            // writing it plainly means something else.
            "max_message_size" => {
                let max: usize = value
                    .parse()
                    .map_err(|_| Error::config_parse(Box::new(InvalidValue("max_message_size"))))?;
                if max == 0 {
                    return Err(Error::config_parse(Box::new(InvalidValue(
                        "max_message_size",
                    ))));
                }
                self.max_message_size(max);
            }
            // This driver never uses GSSAPI, so a request to have it OFF is a
            // request for the state it is permanently in, and refusing that
            // rejects a connection string libpq accepts. `prefer` counts as
            // off: it explicitly permits falling back to a non-GSS connection,
            // which is the only thing that happens here.
            //
            // `require` is refused by name because it cannot be satisfied, and
            // an unrecognised value is refused the way libpq refuses it
            // (`invalid gssencmode value: "bogus"`) rather than being waved
            // through as another way of saying off.
            "gssencmode" => match value {
                "disable" | "prefer" => {}
                "require" => {
                    return Err(Error::config_parse(Box::new(UnsupportedOption(
                        "gssencmode",
                    ))));
                }
                _ => return Err(Error::config_parse(Box::new(InvalidValue("gssencmode")))),
            },
            // Delegation only means anything once GSSAPI is in use. Asking for
            // none is satisfied; asking for it is refused rather than accepted
            // and quietly not done.
            "gssdelegation" => match value {
                "0" => {}
                "1" => {
                    return Err(Error::config_parse(Box::new(UnsupportedOption(
                        "gssdelegation",
                    ))));
                }
                _ => {
                    return Err(Error::config_parse(Box::new(InvalidValue("gssdelegation"))));
                }
            },
            // TLS compression is not implemented and modern PostgreSQL removed
            // it, so `0` asks for what happens anyway. libpq accepts ANY value
            // here including nonsense, because the setting is vestigial for it
            // too; this refuses `1` rather than take a request to compress and
            // quietly not compress.
            "sslcompression" => match value {
                "0" => {}
                "1" => {
                    return Err(Error::config_parse(Box::new(UnsupportedOption(
                        "sslcompression",
                    ))));
                }
                _ => {
                    return Err(Error::config_parse(Box::new(InvalidValue(
                        "sslcompression",
                    ))));
                }
            },
            // The pre-`sslmode` spelling. libpq stores `0` as
            // `sslmode=prefer`, rather than treating it as a no-op, so it must
            // override an earlier sslmode in the same string. `1` means
            // `sslmode=require`, and that IS supported - under that name,
            // which is what the error says rather than leaving the caller to
            // guess.
            "requiressl" => match value {
                "0" => {
                    self.ssl_mode(SslMode::Prefer);
                }
                "1" => {
                    return Err(Error::config_parse(Box::new(UnsupportedOption(
                        "requiressl (use sslmode=require)",
                    ))));
                }
                _ => return Err(Error::config_parse(Box::new(InvalidValue("requiressl")))),
            },
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
                if ssl_key_is_engine_specifier(value) {
                    return Err(Error::config_parse(Box::new(UnsupportedOption("sslkey"))));
                }
                self.ssl_key(value);
            }
            "passfile" => {
                if value.is_empty() {
                    return Err(Error::config_parse(Box::new(InvalidValue("passfile"))));
                }
                self.passfile(value);
            }
            "service" => {
                if value.is_empty() {
                    return Err(Error::config_parse(Box::new(InvalidValue("service"))));
                }
                self.service(value);
            }
            // NOT a libpq key - libpq 18.4 rejects it, measured. It exists
            // because libpq finds the service file through the environment and
            // this driver does not, so the path has to arrive some other way,
            // and it has to arrive in the CONNECTION STRING: a service is
            // expanded while the string is parsed, which is the only point at
            // which the set of explicitly given keys is still known.
            "servicefile" => {
                if value.is_empty() {
                    return Err(Error::config_parse(Box::new(InvalidValue("servicefile"))));
                }
                self.service_file(value);
            }
            "sslkeylogfile" => {
                if value.is_empty() {
                    return Err(Error::config_parse(Box::new(InvalidValue("sslkeylogfile"))));
                }
                self.ssl_key_log_file(value);
            }
            "sslcertmode" => {
                let mode = match value {
                    "disable" => SslCertMode::Disable,
                    "allow" => SslCertMode::Allow,
                    "require" => SslCertMode::Require,
                    _ => {
                        return Err(Error::config_parse(Box::new(InvalidValue("sslcertmode"))));
                    }
                };
                self.ssl_cert_mode(mode);
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
            "min_protocol_version" => {
                self.min_protocol_version(parse_protocol_version("min_protocol_version", value)?);
            }
            "max_protocol_version" => {
                self.max_protocol_version(parse_protocol_version("max_protocol_version", value)?);
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
            "sslsni" => {
                let enabled = match value {
                    "0" => false,
                    "1" => true,
                    _ => return Err(Error::config_parse(Box::new(InvalidValue("sslsni")))),
                };
                self.ssl_sni(enabled);
            }
            "requirepeer" => {
                self.require_peer(value);
            }
            // The list-valued keys CLEAR before they append, so a repeated key
            // replaces rather than extending. libpq resolves
            // `host=a host=b` to `b` alone; appending made it a two-host
            // failover list that tries `a` FIRST, so a string assembled as
            // default-then-override kept the default and preferred it. Measured
            // against psql, which fails to resolve `host=127.0.0.1
            // host=nonexistent.invalid` and could only do that by discarding
            // the first.
            //
            // The comma form is untouched and remains the way to ask for
            // several hosts, in both drivers. The builder methods still append;
            // this is a property of PARSING a connection string, where a later
            // key is an override.
            "host" => {
                self.host.clear();
                if !value.is_empty() {
                    for host in value.split(',') {
                        self.host(host);
                    }
                }
            }
            "hostaddr" => {
                self.hostaddr.clear();
                if !value.is_empty() {
                    for hostaddr in value.split(',') {
                        if hostaddr.is_empty() {
                            self.hostaddr.push(None);
                            continue;
                        }
                        let addr = hostaddr
                            .parse()
                            .map_err(|_| Error::config_parse(Box::new(InvalidValue("hostaddr"))))?;
                        self.hostaddr(addr);
                    }
                }
            }
            "port" => {
                self.port.clear();
                for port in value.split(',') {
                    // Port 0 is refused, as libpq refuses it: it means "any port"
                    // to bind(2) and nothing at all to connect(2), so a config
                    // carrying it can never reach a server.
                    let port = if port.is_empty() {
                        5432
                    } else {
                        let port = parse_libpq_integer(port, "port")?;
                        match u16::try_from(port) {
                            Ok(0) | Err(_) => {
                                return Err(Error::config_parse(Box::new(InvalidValue("port"))));
                            }
                            Ok(port) => port,
                        }
                    };
                    self.port(port);
                }
            }
            "connect_timeout" => {
                let timeout = parse_libpq_integer(value, "connect_timeout")?;
                if timeout > 0 {
                    // TAKEN LITERALLY, INCLUDING 1. PostgreSQL 16 and older
                    // documented and implemented a two-second floor, but
                    // PostgreSQL 18 removed it. This crate's parity oracle is
                    // PostgreSQL 18, whose source uses the positive integer as
                    // supplied.
                    self.connect_timeout(Duration::from_secs(timeout as u64));
                } else {
                    self.connect_timeout(Duration::ZERO);
                }
            }
            "tcp_user_timeout" => {
                let timeout = parse_libpq_integer(value, "tcp_user_timeout")?;
                if timeout > 0 {
                    // MILLISECONDS, and it is the only member of this family
                    // that is not seconds: libpq documents `keepalives_idle`,
                    // `keepalives_interval` and `connect_timeout` in seconds
                    // but this one as "the number of milliseconds that
                    // transmitted data may remain unacknowledged". The kernel
                    // option is milliseconds too, and socket2 converts the
                    // `Duration` with `as_millis`.
                    //
                    // Reading it as seconds is a 1000x error that no
                    // accept/reject test can see - the DSN parses, the socket
                    // option is set, and the bound the caller asked for never
                    // fires. `tcp_user_timeout=30000` means thirty seconds;
                    // as seconds it would be eight hours.
                    //
                    // tokio-postgres 0.7.18 reads this as seconds. That is a
                    // DELIBERATE divergence from the crate this one is a port
                    // of, in favour of the parameter's documented meaning.
                    //
                    // The server settles it, so neither implementation has to be
                    // taken on trust: `SELECT name, unit FROM pg_settings` gives
                    // `tcp_user_timeout | ms` and `tcp_keepalives_idle | s`
                    // (checked on 16.15, 2026-08-26). A peer driver is the wrong
                    // oracle for a unit question - it can be confidently wrong in
                    // a way no differential against it will ever surface.
                    self.tcp_user_timeout(Duration::from_millis(timeout as u64));
                } else {
                    // libpq clamps a negative value to zero, and zero restores
                    // the system default. This must also clear an earlier
                    // occurrence of the same key.
                    self.tcp_user_timeout(Duration::ZERO);
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives" => {
                // Signed, as libpq is: it reads this with `strtol` and tests the
                // result against zero, so `keepalives=-1` is non-zero and means
                // ON. Parsing it as unsigned refuses a value the reference
                // implementation accepts, which turns a DSN psql connects with
                // into a config error here.
                let keepalives = parse_libpq_integer(value, "keepalives")?;
                self.keepalives(keepalives != 0);
            }
            // ZERO must reach `KeepaliveConfig`; a NEGATIVE must not get
            // there at all. `keepalives_idle` and `keepalives_interval`
            // guarded their setters with `> 0`, which discarded the zero
            // before `keepalive::TcpKeepalive::from` - the one place that
            // reads a zero as "leave the socket option alone" - could act on
            // it, so `keepalives_idle=0` silently kept this crate's OWN
            // two-hour default. That default is neither what PostgreSQL
            // documents for a zero nor what libpq does with one.
            //
            // WHAT WAS MEASURED, and it is less than an earlier version of
            // this comment claimed. Against PostgreSQL 16 on 2026-08-23,
            // `psql` fails the connection for every one of
            // `keepalives_idle=0`, `keepalives_idle=-1`,
            // `keepalives_interval=0`, `keepalives_interval=-1`,
            // `keepalives_count=0` and `keepalives_count=-1`, each with
            // `setsockopt(TCP_KEEP{IDLE,INTVL,CNT}) failed: Invalid
            // argument`, while `keepalives_idle=1` and `keepalives_count=2`
            // connect. Zero and negative fail IDENTICALLY there, so that
            // probe cannot tell whether libpq clamps a negative to zero and
            // sets it or passes it straight through - and no claim here rests
            // on the answer.
            //
            // So the two values are ruled on separately and for separate
            // reasons. Zero is the documented "use the system default", which
            // this crate honours and libpq does not (the divergence is argued
            // at `keepalive::TcpKeepalive::from`). A negative is not a
            // documented value at all, and both implementations refuse the
            // connection over one; refusing it HERE only makes the refusal
            // earlier and the message better. It must not be folded into zero
            // - that would turn a typo into a silently successful connection
            // running on system defaults, which is worse than either.
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives_idle" => {
                let keepalives_idle = keepalive_seconds(value, "keepalives_idle")?;
                self.keepalives_idle(Duration::from_secs(keepalives_idle));
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives_interval" => {
                let keepalives_interval = keepalive_seconds(value, "keepalives_interval")?;
                self.keepalives_interval(Duration::from_secs(keepalives_interval));
            }
            #[cfg(not(target_arch = "wasm32"))]
            "keepalives_count" => {
                let keepalives_count = parse_nonnegative_libpq_integer(value, "keepalives_count")?;
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
                let mode = if value.eq_ignore_ascii_case("database") {
                    Some(ReplicationMode::Logical)
                } else if ["true", "on", "1", "yes"]
                    .iter()
                    .any(|candidate| value.eq_ignore_ascii_case(candidate))
                {
                    Some(ReplicationMode::Physical)
                } else if ["false", "off", "0", "no"]
                    .iter()
                    .any(|candidate| value.eq_ignore_ascii_case(candidate))
                {
                    None
                } else {
                    return Err(Error::config_parse(Box::new(InvalidValue("replication"))));
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

    /// Reject connection-parameter combinations that contradict each other.
    ///
    /// libpq runs these in `connectOptions2`, before a socket is opened, and so
    /// do we: each rule describes a URL that can never work, so answering at
    /// connect time is the difference between naming the cause and blaming the
    /// server for a handshake that was never going to happen.
    ///
    /// The rules mirror libpq's validation and fail before an impossible
    /// configuration can be mistaken for a server error:
    ///
    /// * A maximum PostgreSQL wire-protocol version below the minimum names an
    ///   empty protocol set the server cannot negotiate into.
    /// * A maximum TLS version below the minimum likewise names an empty
    ///   protocol set. rustls cannot honour it without crossing one of the
    ///   caller's bounds.
    /// * `sslcertmode=require` cannot be combined with `sslmode=disable`,
    ///   because plaintext cannot carry a TLS client certificate.
    /// * `sslrootcert=system` demands `sslmode=verify-full`. Trusting the
    ///   public root program *without* checking the host name accepts any
    ///   certificate any public CA has issued for any name, which is barely
    ///   better than no verification while looking like the strongest setting
    ///   on the page.
    /// * `sslnegotiation=direct` demands a mode with no plaintext fallback. A
    ///   direct handshake sends no `SSLRequest`, so there is no negotiation to
    ///   fall back *from*; a mode that permits plaintext must not drive it.
    pub(crate) fn validate_connection_settings(&self) -> Result<(), Error> {
        self.validate_protocol_version_range()?;
        self.validate_ssl_protocol_version_range()?;

        if self
            .ssl_key
            .as_deref()
            .is_some_and(ssl_key_is_engine_specifier)
        {
            return Err(Error::config(
                "sslkey requests an OpenSSL engine key, which this rustls driver does not support"
                    .into(),
            ));
        }

        if self.ssl_cert_mode == SslCertMode::Require && !self.ssl_mode.permits_tls() {
            return Err(Error::config(
                "sslcertmode=require cannot be satisfied with sslmode=disable because no TLS \
                 client certificate can be sent"
                    .into(),
            ));
        }

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

    pub(crate) fn validate_protocol_version_range(&self) -> Result<(), Error> {
        if self.min_protocol_version > self.max_protocol_version {
            return Err(Error::config(
                format!(
                    "min_protocol_version={} cannot be higher than max_protocol_version={}",
                    self.min_protocol_version.as_str(),
                    self.max_protocol_version.as_str()
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
    /// Uses the startup, authentication, timeout, statement-cache, TLS mode,
    /// `sslsni`, and `sslcertmode` settings. A configured `requirepeer` is
    /// refused because an arbitrary stream exposes neither its address family
    /// nor peer credentials. Transport-address settings such as `host`,
    /// `hostaddr`, `port`, keepalives, and `tcp_user_timeout` are ignored. The
    /// connect timeout starts with TLS negotiation, covers startup and
    /// authentication, and cannot cover the caller's work to open that stream.
    /// The read timeout is installed only after startup succeeds.
    ///
    /// The caller owns the stream, so this entry point cannot open a second
    /// one. `allow` and `prefer` are therefore reduced to the transport they
    /// attempt *first* - plaintext and TLS respectively - with no reconnect if
    /// it fails. Use [`Config::connect`] to get the fallback.
    pub async fn connect_raw<S, T>(
        &self,
        stream: S,
        tls: T,
    ) -> Result<(Client, Connection<S, T::Stream>), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        if self.require_peer.is_some() {
            // A generic stream does not expose an address family or peer
            // credentials. Pretending this is a Unix socket would be unsafe,
            // while ignoring the requested identity check would turn it into
            // false assurance.
            return Err(Error::config(
                "requirepeer cannot be checked by Config::connect_raw; use Config::connect \
                 with a Unix-domain host"
                    .into(),
            ));
        }
        self.validate_connection_settings()?;
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
            // Paths and names, not secrets: `passfile` and `servicefile` say
            // WHERE a credential lives, which is the thing a caller debugging
            // "why is it not using my password" needs to see. The password
            // itself is redacted above however it arrived, including from a
            // file.
            .field("passfile", &self.passfile)
            .field("service", &self.service)
            .field("service_file", &self.service_file)
            .field("dbname", &self.dbname)
            .field("options", &self.options)
            .field("application_name", &self.application_name)
            .field("fallback_application_name", &self.fallback_application_name)
            .field("statement_cache_capacity", &self.statement_cache_capacity)
            .field(
                "statement_cache_execution_threshold",
                &self.statement_cache_execution_threshold,
            )
            .field("ssl_mode", &self.ssl_mode)
            .field("ssl_negotiation", &self.ssl_negotiation)
            .field("ssl_root_cert", &self.ssl_root_cert)
            .field("ssl_cert", &self.ssl_cert)
            .field("ssl_key", &self.ssl_key)
            .field("ssl_cert_mode", &self.ssl_cert_mode)
            .field(
                "ssl_password",
                &self.ssl_password.as_ref().map(|_| Redaction {}),
            )
            .field("ssl_crl", &self.ssl_crl)
            .field("ssl_crl_dir", &self.ssl_crl_dir)
            .field("min_protocol_version", &self.min_protocol_version)
            .field("max_protocol_version", &self.max_protocol_version)
            .field("ssl_min_protocol_version", &self.ssl_min_protocol_version)
            .field("ssl_max_protocol_version", &self.ssl_max_protocol_version)
            .field("ssl_sni", &self.ssl_sni)
            .field("ssl_key_log_file", &self.ssl_key_log_file)
            .field("max_message_size", &self.max_message_size)
            .field("require_peer", &self.require_peer)
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

/// A recognised option asking for something this driver does not implement.
///
/// Distinct from `unknown option`, which says the key is not understood at
/// all: this says the key IS understood and the value cannot be honoured, so
/// the caller learns their setting was rejected rather than mistyped.
#[derive(Debug)]
struct UnsupportedOption(&'static str);

impl fmt::Display for UnsupportedOption {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "option `{}` asks for a feature this driver does not implement",
            self.0
        )
    }
}

impl error::Error for UnsupportedOption {}

#[derive(Debug)]
struct NestedService;

impl fmt::Display for NestedService {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("nested service specifications are not supported: option `service`")
    }
}

impl error::Error for NestedService {}

#[derive(Debug)]
struct InvalidServiceOption(&'static str);

impl fmt::Display for InvalidServiceOption {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "connection option `{}` is not valid in a service file",
            self.0
        )
    }
}

impl error::Error for InvalidServiceOption {}

/// The deprecated `requiressl` spelling occupies libpq's `sslmode` slot.
/// Precedence and repeated-key replacement therefore operate across the two
/// names, not independently for each spelling.
fn canonical_parameter_key(key: &str) -> &str {
    match key {
        "requiressl" => "sslmode",
        _ => key,
    }
}

/// libpq treats any colon as the `engine:key` separator, except for a Windows
/// drive-letter colon. This rustls backend has no OpenSSL ENGINE API, so the
/// recognisable form must not fall through to filesystem loading.
fn ssl_key_is_engine_specifier(value: &str) -> bool {
    match value.find(':') {
        None => false,
        #[cfg(windows)]
        Some(1) => false,
        Some(_) => true,
    }
}

/// The final string value for each connection option, before that value is
/// interpreted.
///
/// libpq's conninfo parser stores strings in option slots and validates those
/// slots only after the whole connection string has been read. Consequently,
/// an invalid earlier value is harmless when a later occurrence replaces it.
/// Keeping the original spelling matters for `requiressl`, whose value parser
/// differs from `sslmode` even though the two names share one precedence slot.
#[derive(Default)]
struct ParsedParameters(Vec<(String, String)>);

impl ParsedParameters {
    fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let canonical_key = canonical_parameter_key(&key);
        if let Some(index) = self
            .0
            .iter()
            .position(|(given, _)| canonical_parameter_key(given) == canonical_key)
        {
            self.0.remove(index);
        }
        self.0.push((key, value.into()));
    }

    fn apply(self, config: &mut Config) -> Result<(), Error> {
        for (key, value) in self.0 {
            config.param(&key, &value)?;
        }
        Ok(())
    }
}

/// Parse libpq's integer grammar: a signed C `int`, surrounded only by C
/// whitespace. Rust's integer parser has the right digit and sign grammar once
/// those six whitespace bytes have been removed.
fn parse_libpq_integer(value: &str, option: &'static str) -> Result<i32, Error> {
    value
        .trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{000b}' | '\u{000c}'))
        .parse::<i32>()
        .map_err(|_| Error::config_parse(Box::new(InvalidValue(option))))
}

fn parse_nonnegative_libpq_integer(value: &str, option: &'static str) -> Result<u32, Error> {
    u32::try_from(parse_libpq_integer(value, option)?)
        .map_err(|_| Error::config_parse(Box::new(InvalidValue(option))))
}

/// Parse a keepalive duration in seconds: zero allowed, negative refused.
///
/// A zero survives because this crate reads it as "use the system default" and
/// drops it in [`crate::keepalive`]'s `TcpKeepalive` conversion, so discarding
/// it here would leave that conversion nothing to drop.
#[cfg(not(target_arch = "wasm32"))]
fn keepalive_seconds(value: &str, option: &'static str) -> Result<u64, Error> {
    parse_nonnegative_libpq_integer(value, option).map(u64::from)
}

/// Whether `value` names the only text encoding this driver can decode.
///
/// ONE definition, called from all three places that have to rule on it: the
/// connection string (`Config::param`), the startup `ParameterStatus` a server
/// sends during the handshake (`connect_raw::read_info`), and a mid-session
/// change (`connection::route_async`). The predicate was written out separately
/// in the first two and MISSING ENTIRELY from the third until 2026-08-23, which
/// is the shape this consolidation exists to prevent: a guard on one door and
/// not its twin.
///
/// Rust strings are UTF-8. Any other encoding would be decoded as something it
/// is not -- and, where the foreign bytes happen to be valid UTF-8, decoded
/// SILENTLY as a different string. `UNICODE` is libpq's accepted alias for
/// UTF8, and the separators are stripped because `utf-8` and `utf_8` name the
/// same encoding.
pub(crate) fn is_decodable_encoding(value: &str) -> bool {
    matches!(
        value.replace(['-', '_'], "").to_ascii_uppercase().as_str(),
        "UTF8" | "UNICODE"
    )
}

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

fn parse_protocol_version(parameter: &'static str, value: &str) -> Result<ProtocolVersion, Error> {
    match value {
        "3.0" => Ok(ProtocolVersion::V3_0),
        "3.2" | "latest" => Ok(ProtocolVersion::V3_2),
        _ => Err(Error::config_parse(Box::new(InvalidValue(parameter)))),
    }
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
        let mut explicit = Vec::new();
        let mut parameters = ParsedParameters::default();

        while let Some((key, value)) = parser.parameter()? {
            parameters.insert(key, value);
            explicit.push(key.to_owned());
        }

        parameters.apply(&mut config)?;

        // A `service` names parameters to fall back on, so it is expanded once
        // the string is fully read and every explicitly given key is known.
        config.apply_service(&explicit)?;

        Ok(config)
    }

    /// What libpq counts as whitespace between keyword=value pairs: C's
    /// `isspace()` in the C locale, applied to BYTES.
    ///
    /// NOT `char::is_whitespace`, which is the Unicode definition and includes
    /// U+00A0, U+2007, the U+2000 block and more. Splitting on those made a
    /// value containing a non-breaking space end early and the rest of it parse
    /// as further keywords -- so `application_name=x\u{a0}user=alice` connected
    /// as `alice` here, while libpq keeps the whole thing as one application
    /// name and connects as the configured user. Measured against psql 16.14.
    ///
    /// NOT `char::is_ascii_whitespace` either: that omits the vertical tab
    /// (0x0B), which C's `isspace()` includes and libpq therefore splits on.
    const fn is_conninfo_space(c: char) -> bool {
        matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
    }

    fn skip_ws(&mut self) {
        self.take_while(Self::is_conninfo_space);
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
            c if Self::is_conninfo_space(c) => false,
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
            if Self::is_conninfo_space(c) {
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

        // An empty value is LEGAL: libpq accepts `dbname=x password=`. It can
        // only arise at the end of the string, because after `=` the parser
        // skips whitespace and takes whatever follows as the value - which is
        // why `user= host=h` asks for a user named `host=h` rather than
        // producing an empty one. Refusing it here rejected the whole DSN.
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
                        format!(
                            "unexpected character at byte {i}: expected a keyword but got `{c}`"
                        )
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
    /// Query parameters and the authority's port list, kept as strings until
    /// every possible override has been seen.
    parameters: ParsedParameters,
    /// Every key the URL itself supplied, so a `service` cannot override one.
    explicit: Vec<String>,
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
            parameters: ParsedParameters::default(),
            explicit: Vec::new(),
        };

        parser.parse_credentials()?;
        parser.parse_host()?;
        parser.parse_path()?;
        parser.parse_params()?;

        std::mem::take(&mut parser.parameters).apply(&mut parser.config)?;

        let explicit = std::mem::take(&mut parser.explicit);
        parser.config.apply_service(&explicit)?;

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

    /// Take the URI authority without treating delimiters inside a leading
    /// bracketed host as structural. libpq does not validate the bracketed
    /// text as IPv6 here; it scans opaquely through the first `]`.
    fn take_uri_authority(&mut self) -> &'a str {
        let bytes = self.s.as_bytes();
        let mut i = 0;
        let mut item_start = true;

        while i < bytes.len() {
            if item_start && bytes[i] == b'[' {
                let Some(close) = self.s[i + 1..].find(']') else {
                    return self.take_all();
                };
                i += close + 2;
                item_start = false;
                continue;
            }

            match bytes[i] {
                b'/' | b'?' => {
                    let (authority, tail) = self.s.split_at(i);
                    self.s = tail;
                    return authority;
                }
                b',' => item_start = true,
                _ => item_start = false,
            }
            i += 1;
        }

        self.take_all()
    }

    fn eat_byte(&mut self) {
        self.s = &self.s[1..];
    }

    fn parse_credentials(&mut self) -> Result<(), Error> {
        // THE `@` SCAN IS BOUNDED BY `/`, because userinfo cannot appear after
        // the path begins. Scanning the whole remainder let ANY later `@` --
        // in a query value, most easily an application_name or a password --
        // be read as the userinfo separator. The result was not a parse error
        // but a redirected connection:
        //
        //   postgres://127.0.0.1:5432/postgres?user=postgres&application_name=c@d
        //     libpq: connects to 127.0.0.1, application_name is "c@d"
        //     was:   host "d", user "127.0.0.1", password
        //            "5432/postgres?user=postgres&application_name=c"
        //
        // So a string carrying `password=` in its query sent that password to a
        // host named by whatever followed the `@`.
        //
        // BOUNDED BY `/` ONLY, NOT BY `?`, and that is measured rather than
        // assumed. With no path at all libpq really does scan past the query:
        // `postgres://127.0.0.1:5432?...&application_name=a@b` fails there with
        // `could not translate host name "b"`. Stopping at `?` as well would be
        // unfaithful in the other direction.
        // Decided by LOOKING before consuming: `take_until` advances, and
        // there is no way to put the input back if the delimiter turns out to
        // be the `/`.
        let creds = match self.s.find(['@', '/']) {
            Some(at) if self.s.as_bytes()[at] == b'@' => {
                let (head, tail) = self.s.split_at(at);
                self.s = tail;
                head
            }
            // A `/` first means the authority ended with no userinfo in it;
            // none at all means the same.
            _ => return Ok(()),
        };
        self.eat_byte();

        // A syntactically absent value stays unset. A raw nonempty value that
        // decodes to empty is still EXPLICIT, however: libpq stores that empty
        // option and therefore does not let a service replace it.
        let mut it = creds.splitn(2, ':');
        let user = it.next().unwrap();
        if !user.is_empty() {
            let user = Self::decode(user)?;
            self.config.user(user);
            self.explicit.push("user".to_owned());
        }

        if let Some(password) = it.next().filter(|password| !password.is_empty()) {
            let password = Self::validated_percent_decode(password)?;
            self.config.password(password);
            self.explicit.push("password".to_owned());
        }

        Ok(())
    }

    fn parse_host(&mut self) -> Result<(), Error> {
        let host = self.take_uri_authority();

        if host.is_empty() {
            return Ok(());
        }
        let mut hosts = Vec::new();
        let mut ports = Vec::new();

        let mut remaining = host;
        loop {
            let comma = if remaining.starts_with('[') {
                remaining.find(']').and_then(|close| {
                    remaining[close + 1..]
                        .find(',')
                        .map(|comma| close + 1 + comma)
                })
            } else {
                remaining.find(',')
            };
            let (chunk, tail) = comma.map_or((remaining, None), |comma| {
                (&remaining[..comma], Some(&remaining[comma + 1..]))
            });

            let (host, port) = if chunk.starts_with('[') {
                let idx = match chunk.find(']') {
                    Some(idx) => idx,
                    None => return Err(Error::config_parse(InvalidValue("host").into())),
                };

                let host = &chunk[1..idx];
                if host.is_empty() {
                    return Err(Error::config_parse(InvalidValue("host").into()));
                }
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

            hosts.push(host);
            // Keep the authority ports as strings until the query has been
            // parsed. A later `?port=` owns the same libpq option slot, so it
            // can replace even an authority value that would be invalid if it
            // survived. The joined value preserves one port per authority
            // host; applying each one separately would make only the last win.
            ports.push(port.unwrap_or(""));

            let Some(tail) = tail else {
                break;
            };
            remaining = tail;
        }

        // libpq stores an authority option only when its assembled buffer is
        // nonempty. This matters for a single empty slot: `postgresql://h/db`
        // leaves `port` unset and `postgresql://:5455/db` leaves `host` unset,
        // so a service may supply the missing value. In a multi-host authority
        // the comma itself makes the buffer nonempty, preserving positional
        // empty slots such as `host=a,b port=,`.
        let hosts = hosts.join(",");
        if !hosts.is_empty() {
            self.host_params(&hosts)?;
            self.explicit.push("host".to_owned());
        }

        let ports = ports.join(",");
        if !ports.is_empty() {
            self.parameters
                .insert("port", Self::decode(&ports)?.into_owned());
            self.explicit.push("port".to_owned());
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
            self.config.dbname(Self::decode(dbname)?);
            self.explicit.push("dbname".to_owned());
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
                Some(key) => Self::decode(key)?,
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

            if value.contains('=') {
                return Err(Error::config_parse(
                    format!("extra key/value separator `=` in URI query parameter: `{key}`").into(),
                ));
            }

            if key == "host" {
                // A query-string `host=` REPLACES the authority's hosts, as
                // every other key in this loop does through `param`. Measured
                // against psql: `postgres://127.0.0.1/db?host=nonexistent`
                // fails to resolve the query host and never falls back to the
                // authority, which it could only do by having discarded it.
                //
                // This one key is routed through `host_params` rather than
                // `param` so a `/`-prefixed value is still recognised as a
                // socket directory, and `host_params` appends because the
                // AUTHORITY needs it to -- so the clear belongs here, once,
                // before the comma-separated value is applied.
                self.config.host.clear();
                self.host_params(value)?;
                self.explicit.push("host".to_owned());
            } else {
                let value = Self::decode(value)?.into_owned();
                if key == "ssl" && value == "true" {
                    self.parameters.insert("sslmode", "require");
                    self.explicit.push("sslmode".to_owned());
                } else {
                    self.explicit.push(key.to_string());
                    self.parameters.insert(key.into_owned(), value);
                }
            }
        }

        Ok(())
    }

    /// Decode and append a URL's complete `host` option.
    ///
    /// libpq percent-decodes the whole option before its comma-list splitter
    /// runs. Consequently `%2C` is a host separator, not a literal comma in a
    /// host name. Decoding each authority item separately reversed that order
    /// and tried to resolve one comma-containing name instead of two hosts.
    ///
    /// A wholly empty decoded option means no host list. A comma still creates
    /// positional empty slots, just as the keyword parser does.
    fn host_params(&mut self, s: &str) -> Result<(), Error> {
        let decoded = Self::validated_percent_decode(s)?;
        if decoded.is_empty() {
            return Ok(());
        }

        for host in decoded.split(|byte| *byte == b',') {
            self.decoded_host_param(host)?;
        }
        Ok(())
    }

    /// Append one already-decoded host without clearing earlier list items.
    fn decoded_host_param(&mut self, decoded: &[u8]) -> Result<(), Error> {
        #[cfg(unix)]
        if decoded.first() == Some(&b'/') {
            self.config.host_path(OsStr::from_bytes(decoded));
            return Ok(());
        }

        #[cfg(target_os = "linux")]
        if decoded.first() == Some(&b'@') {
            self.config.host_abstract(&decoded[1..]);
            return Ok(());
        }

        let decoded = str::from_utf8(decoded).map_err(|e| Error::config_parse(Box::new(e)))?;
        self.config.host(decoded);
        Ok(())
    }

    /// Trim raw ASCII spaces at a component's boundaries and refuse one in its
    /// interior, as libpq's `conninfo_uri_decode` does.
    ///
    /// `PostgreSQL` deliberately accepts boundary padding even though a strict
    /// RFC URI would not. An interior raw space still commonly means the two
    /// DSN syntaxes were mixed, as in
    /// `postgres://host/zeroship read_timeout=5`. libpq rejects that outright;
    /// this crate used to fold the whole tail into the DATABASE NAME and fail
    /// later against the server with `database "zeroship read_timeout=5" does
    /// not exist`, which names the symptom and not the cause. Measured against
    /// the review container's libpq on 2026-08-26: it refuses an interior raw
    /// space in the user, host, database and a query value alike, and accepts
    /// `%20` in each.
    fn trim_raw_boundary_spaces(s: &str) -> Result<&str, Error> {
        let trimmed = s.trim_matches(' ');
        if trimmed.contains(' ') {
            return Err(Error::config_parse(
                format!(
                    "unexpected spaces found in \"{s}\", use percent-encoded \
                     spaces (%20) instead"
                )
                .into(),
            ));
        }
        Ok(trimmed)
    }

    /// Refuse a malformed percent escape, as libpq does
    /// (`invalid percent-encoded token: "%zz"`).
    ///
    /// `percent_encoding::percent_decode` NEVER FAILS: a `%` not followed by
    /// two hex digits is copied through verbatim. So `?password=se%cret`
    /// authenticated with a literal `se%cret` rather than telling the caller
    /// their string was malformed, and `[fe80::1%eth0]` -- a zone id written
    /// without encoding the `%` -- became a host name containing a percent
    /// sign. Silently using a different credential than the one written is the
    /// failure worth refusing.
    fn validate_percent_escapes(s: &str) -> Result<(), Error> {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'%' {
                i += 1;
                continue;
            }
            let escape = bytes.get(i + 1..i + 3);
            if !escape.is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit)) {
                return Err(Error::config_parse(
                    format!("invalid percent-encoded token: \"{s}\"").into(),
                ));
            }
            i += 3;
        }
        Ok(())
    }

    /// Validate one URL component and percent-decode it to BYTES.
    ///
    /// THE ONLY PLACE a component is validated, so a rule added here applies
    /// to every one of them by construction. That matters because the three
    /// callers cannot share a return type: a password need not be UTF-8 and a
    /// Unix socket path is an `OsStr`, so only [`Self::decode`] can go on to
    /// `decode_utf8`. Before this existed the two checks were copied at three
    /// sites, and adding the raw-space rule on 2026-08-26 meant editing all
    /// three by hand - miss one and that component silently accepts what the
    /// others refuse.
    fn validated_percent_decode(s: &str) -> Result<Cow<'_, [u8]>, Error> {
        let encoded = s;
        let s = Self::trim_raw_boundary_spaces(s)?;
        Self::validate_percent_escapes(s)?;
        let decoded = Cow::from(percent_encoding::percent_decode(s.as_bytes()));
        if decoded.contains(&0) {
            return Err(Error::config_parse(
                format!("forbidden value %00 in percent-encoded value: \"{encoded}\"").into(),
            ));
        }
        Ok(decoded)
    }

    fn decode(s: &str) -> Result<Cow<'_, str>, Error> {
        match Self::validated_percent_decode(s)? {
            Cow::Borrowed(bytes) => std::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|e| Error::config_parse(e.into())),
            Cow::Owned(bytes) => String::from_utf8(bytes)
                .map(Cow::Owned)
                .map_err(|e| Error::config_parse(e.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    /// What every parameter means when the caller says NOTHING.
    ///
    /// A wrong default is invisible in the same way a wrong unit is, and for
    /// the same reasons: the parity table rules only on accept-vs-refuse, the
    /// differential oracle compares query results, and every test that sets a
    /// value explicitly is unaffected. It changes behaviour only for callers
    /// who said nothing - which is most of them.
    ///
    /// `ssl_mode` is the one that matters most: defaulting to `Disable` rather
    /// than `Prefer` would silently stop negotiating encryption at all, and
    /// nothing else here would notice.
    ///
    /// Checked against the PostgreSQL 18 documentation on 2026-08-26; all eleven
    /// matched.
    mod parameter_defaults {
        use super::super::{
            ChannelBinding, Config, LoadBalanceHosts, SslCertMode, SslMode, SslNegotiation,
            TargetSessionAttrs,
        };

        #[test]
        fn the_defaults_are_libpqs() {
            let config = Config::new();

            assert_eq!(config.get_ssl_mode(), SslMode::Prefer);
            assert_eq!(config.get_ssl_negotiation(), SslNegotiation::Postgres);
            assert_eq!(config.get_ssl_cert_mode(), SslCertMode::Allow);
            assert!(config.get_ssl_sni());
            assert!(config.get_keepalives());
            assert_eq!(config.get_target_session_attrs(), TargetSessionAttrs::Any);
            assert_eq!(config.get_channel_binding(), ChannelBinding::Prefer);
            assert_eq!(config.get_load_balance_hosts(), LoadBalanceHosts::Disable);
            // An omitted keepalives_idle leaves the operating-system default
            // untouched. Zero is how this Config represents that absence.
            assert_eq!(config.get_keepalives_idle(), std::time::Duration::ZERO);
            // "Zero, negative, or not specified means wait indefinitely."
            assert_eq!(config.get_connect_timeout(), None);
            // Empty means "the port PostgreSQL was built with"; the connect
            // path supplies 5432 rather than the config carrying it.
            assert!(config.get_ports().is_empty());
        }
    }

    /// Every parameter whose value carries a UNIT or a value convention,
    /// pinned against what PostgreSQL documents it to mean.
    ///
    /// This exists because a wrong unit is invisible to every other check we
    /// have. The DSN parses, the setting is applied, and the number is simply
    /// wrong by a factor - so the libpq parity table (which only rules on
    /// accept-vs-refuse) reports it as implemented, and the differential suite
    /// cannot see it at all because that compares QUERY RESULTS and never DSN
    /// interpretation.
    ///
    /// `tcp_user_timeout` was read as seconds here for exactly that reason,
    /// inherited verbatim from tokio-postgres 0.7.18, which still reads it
    /// that way. libpq documents it in MILLISECONDS and it is the only member
    /// of this family that is not seconds.
    mod parameter_units {
        use super::super::Config;
        use std::time::Duration;

        fn parse(fragment: &str) -> Config {
            format!("host=h {fragment}")
                .parse()
                .expect("the fragment parses")
        }

        #[test]
        fn connect_timeout_is_seconds() {
            assert_eq!(
                parse("connect_timeout=10").get_connect_timeout(),
                Some(&Duration::from_secs(10))
            );
        }

        #[test]
        fn tcp_user_timeout_is_milliseconds() {
            assert_eq!(
                parse("tcp_user_timeout=7000").get_tcp_user_timeout(),
                Some(&Duration::from_millis(7000)),
                "libpq documents this one in milliseconds, unlike the rest"
            );
        }

        #[test]
        fn the_keepalive_timings_are_seconds_and_the_count_is_a_count() {
            let config = parse("keepalives_idle=11 keepalives_interval=3 keepalives_count=5");
            assert_eq!(config.keepalive_config.idle, Duration::from_secs(11));
            assert_eq!(
                config.keepalive_config.interval,
                Some(Duration::from_secs(3))
            );
            assert_eq!(config.keepalive_config.retries, Some(5));
        }

        #[test]
        fn socket_integers_use_libpqs_whitespace_and_i32_range() {
            let mut whitespace_refused = Vec::new();
            let mut out_of_range_accepted = Vec::new();
            for key in [
                "port",
                "connect_timeout",
                "tcp_user_timeout",
                "keepalives",
                "keepalives_idle",
                "keepalives_interval",
                "keepalives_count",
            ] {
                if format!("host=h {key}=' 1 '").parse::<Config>().is_err() {
                    whitespace_refused.push(key);
                }
                for value in ["2147483648", "-2147483649"] {
                    if format!("host=h {key}={value}").parse::<Config>().is_ok() {
                        out_of_range_accepted.push(format!("{key}={value}"));
                    }
                }
            }

            assert!(
                whitespace_refused.is_empty() && out_of_range_accepted.is_empty(),
                "C whitespace refused: {}; values outside C int accepted: {}",
                whitespace_refused.join(", "),
                out_of_range_accepted.join(", ")
            );
        }

        /// libpq reads `keepalives` with `strtol` and tests the result against
        /// zero, so a NEGATIVE value means on. Parsing it as unsigned would
        /// refuse a DSN psql accepts.
        #[test]
        fn keepalives_is_nonzero_rather_than_boolean() {
            assert!(parse("keepalives=1").get_keepalives());
            assert!(parse("keepalives=-1").get_keepalives());
            assert!(!parse("keepalives=0").get_keepalives());
        }

        /// Zero means "use the system default" for both timeouts, which is
        /// expressed by leaving the setting unset rather than by passing a
        /// zero down to the socket.
        #[test]
        fn a_zero_timeout_is_left_unset_so_the_default_applies() {
            assert_eq!(parse("connect_timeout=0").get_connect_timeout(), None);
            assert_eq!(parse("tcp_user_timeout=0").get_tcp_user_timeout(), None);
        }

        #[test]
        fn a_programmatic_zero_connect_timeout_is_indefinite() {
            let mut config = Config::new();
            config.connect_timeout(Duration::ZERO);
            assert_eq!(config.get_connect_timeout(), None);
        }

        // NON-DISCRIMINATING NOW, for the same duplicate-collapse reason; it DID
        // fail at d51ae16ff, the commit that added it. See `Config::param`. The
        // programmatic peer, `a_programmatic_zero_connect_timeout_is_indefinite`,
        // still discriminates.
        #[test]
        fn a_later_indefinite_connect_timeout_clears_an_earlier_limit() {
            for value in ["0", "-1"] {
                let config = parse(&format!("connect_timeout=5 connect_timeout={value}"));
                assert_eq!(
                    config.get_connect_timeout(),
                    None,
                    "the later connect_timeout={value} did not override the earlier limit"
                );
            }
        }

        #[test]
        fn a_later_system_tcp_user_timeout_clears_an_earlier_limit() {
            for value in ["0", "-1"] {
                let config = parse(&format!("tcp_user_timeout=5000 tcp_user_timeout={value}"));
                assert_eq!(
                    config.get_tcp_user_timeout(),
                    None,
                    "the later tcp_user_timeout={value} did not override the earlier limit"
                );
            }
        }
    }

    /// What libpq does with `key=` - a keyword whose value is empty.
    ///
    /// Measured against the review container's libpq with the `.invalid` host read-out
    /// (`docs/runbooks/compio-postgres-libpq-parameter-probing.md` describes the
    /// technique). The rule is per-TYPE, not uniform: every numeric option
    /// takes empty as "not given" and uses its default, every enum option
    /// REFUSES it, and string options keep the empty string.
    /// GSSAPI is not implemented, but that is not a reason to refuse a request
    /// to TURN IT OFF.
    ///
    /// This driver never uses GSSAPI encryption, so `gssencmode=disable` asks
    /// for the state it is permanently in, and `prefer` explicitly permits
    /// falling back to a non-GSS connection. Refusing either rejects a
    /// connection string libpq accepts, for asking us to do what we already do
    /// - measured 2026-08-25, libpq connects for `disable`, `prefer` and
    /// `gssdelegation=0` alike.
    ///
    /// `require` and `gssdelegation=1` are different: they ask for something
    /// this driver genuinely will not do, so they stay refused BY NAME. That
    /// distinction is the whole point - accepting them would be the
    /// "accepted and silently ignored" failure `libpq_parameter_parity.rs`
    /// exists to prevent.
    /// An interior raw space in a URL is a MISTAKE, and the commonest one is
    /// mixing the two DSN syntaxes: appending a keyword setting to a URL.
    ///
    /// This crate used to accept it and fold the tail into the database name,
    /// so `postgres://h/zeroship read_timeout=5` connected to a database
    /// literally called `zeroship read_timeout=5` and failed at the SERVER
    /// with `database ... does not exist`, naming the symptom rather than the
    /// cause. That cost real debugging time. libpq refuses it up front and
    /// says what to do instead; these pin the same behaviour.
    mod raw_spaces_in_a_url {
        use crate::Config;

        fn chain(dsn: &str) -> String {
            let error = dsn
                .parse::<Config>()
                .expect_err("a raw space must be refused");
            let mut chain = String::new();
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                chain.push_str(&cause.to_string());
                source = std::error::Error::source(cause);
            }
            chain
        }

        /// EVERY component, and the list is the point rather than the count.
        ///
        /// The PASSWORD and the HOST each had their own decode path, because
        /// neither can be a `str` - a password need not be UTF-8 and a socket
        /// path is an `OsStr`. So a rule added to the ordinary path reached
        /// four components and silently missed those two. This test named only
        /// four until 2026-08-26 and would have passed with the password
        /// unchecked.
        #[test]
        fn an_interior_raw_space_is_refused_in_every_component() {
            for dsn in [
                "postgres://postgres@127.0.0.1:5432/zeroship read_timeout=5",
                "postgres://post gres@127.0.0.1:5432/zeroship",
                "postgres://postgres:pass word@127.0.0.1:5432/zeroship",
                "postgres://postgres@127.0.0 .1:5432/zeroship",
                "postgres://postgres@127.0.0.1:5432/zeroship?application_name=a b",
            ] {
                let chain = chain(dsn);
                assert!(
                    chain.contains("unexpected spaces found") && chain.contains("%20"),
                    "the refusal does not tell the caller how to fix it: {chain}"
                );
            }
        }

        /// THE CONTROL, and it is the whole point: an ENCODED space is a legal
        /// space and must still decode to one. Without this the test above
        /// would be satisfied by refusing every space, encoded or not.
        #[test]
        fn an_encoded_space_still_decodes_to_a_space() {
            let config: Config =
                "postgres://postgres@127.0.0.1:5432/zeroship?application_name=a%20b"
                    .parse()
                    .expect("percent-encoded spaces are legal");
            assert_eq!(config.get_application_name(), Some("a b"));
            assert_eq!(config.get_dbname(), Some("zeroship"));
        }

        /// The KEYWORD syntax is untouched: there a space separates settings,
        /// so this check must not reach it.
        #[test]
        fn the_keyword_syntax_still_takes_spaces_as_separators() {
            let config: Config = "host=h port=5432 dbname=db"
                .parse()
                .expect("spaces separate keyword settings");
            assert_eq!(config.get_dbname(), Some("db"));
            assert_eq!(config.get_ports(), [5432]);
        }
    }

    mod gssapi_parameters {
        use crate::Config;

        fn chain(dsn: &str) -> String {
            let error = dsn.parse::<Config>().expect_err("this must be refused");
            let mut chain = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                chain.push_str(" | ");
                chain.push_str(&cause.to_string());
                source = std::error::Error::source(cause);
            }
            chain
        }

        #[test]
        fn turning_gssapi_off_is_accepted() {
            for dsn in [
                "host=h gssencmode=disable",
                "host=h gssencmode=prefer",
                "host=h gssdelegation=0",
            ] {
                assert!(
                    dsn.parse::<Config>().is_ok(),
                    "libpq accepts this and it asks for what this driver already does: {dsn}"
                );
            }
        }

        /// THE CONTROL. Without it the test above would be satisfied by a
        /// parser that accepted every GSSAPI setting and ignored all of them.
        #[test]
        fn asking_for_gssapi_is_still_refused_by_name() {
            for (dsn, key) in [
                ("host=h gssencmode=require", "gssencmode"),
                ("host=h gssdelegation=1", "gssdelegation"),
                ("host=h gsslib=gssapi", "gsslib"),
                ("host=h krbsrvname=postgres", "krbsrvname"),
            ] {
                let chain = chain(dsn);
                assert!(
                    chain.contains(key),
                    "refused without naming {key}, so the caller cannot tell which \
                     option is unsupported: {chain}"
                );
            }
        }

        /// A value libpq itself rejects must not become acceptable here just
        /// because this driver skips the feature.
        #[test]
        fn an_invalid_gssencmode_value_is_refused() {
            let chain = chain("host=h gssencmode=bogus");
            assert!(
                chain.contains("gssencmode"),
                "an invalid gssencmode value must be refused naming the key: {chain}"
            );
        }
    }

    /// The same rule as [`gssapi_parameters`], applied to the rest of the
    /// refused surface: a value that asks for this driver's own behaviour is
    /// satisfied, and one that asks for a feature it lacks is refused by name.
    ///
    /// Fixing only `gssencmode` would have repeated the mistake that made the
    /// TLS teardown take three rounds - patching the instance in front of me
    /// and leaving the others.
    mod off_values_of_unsupported_features {
        use crate::Config;

        fn chain(dsn: &str) -> String {
            let error = dsn.parse::<Config>().expect_err("this must be refused");
            let mut chain = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                chain.push_str(" | ");
                chain.push_str(&cause.to_string());
                source = std::error::Error::source(cause);
            }
            chain
        }

        #[test]
        fn asking_for_the_feature_to_be_off_is_accepted() {
            for dsn in ["host=h sslcompression=0", "host=h requiressl=0"] {
                assert!(
                    dsn.parse::<Config>().is_ok(),
                    "libpq accepts this and it asks for what this driver already does: {dsn}"
                );
            }
        }

        #[test]
        fn requiressl_zero_overrides_an_earlier_sslmode() {
            let config = "host=h sslmode=require requiressl=0"
                .parse::<Config>()
                .expect("requiressl=0 is the legacy spelling of sslmode=prefer");
            assert_eq!(config.get_ssl_mode(), crate::config::SslMode::Prefer);
        }

        /// THE CONTROL: turning these ON must still be refused, or the test
        /// above is satisfied by a parser that swallows the whole family.
        #[test]
        fn asking_for_the_feature_is_refused_by_name() {
            for (dsn, key) in [
                ("host=h sslcompression=1", "sslcompression"),
                ("host=h requiressl=1", "requiressl"),
            ] {
                let chain = chain(dsn);
                assert!(chain.contains(key), "refused without naming {key}: {chain}");
            }
        }

        /// `requiressl=1` is not unimplementable - it is `sslmode=require`
        /// under the pre-`sslmode` spelling - so the error names the option
        /// that does work instead of only saying no.
        #[test]
        fn the_requiressl_refusal_points_at_sslmode() {
            let chain = chain("host=h requiressl=1");
            assert!(
                chain.contains("sslmode=require"),
                "the refusal does not tell the caller what to use instead: {chain}"
            );
        }
    }

    mod empty_parameter_values {
        use crate::Config;

        /// The whole family reduces to this: a DSN whose LAST parameter has an
        /// empty value. `dbname=x password=` is an ordinary way to say "no
        /// password", and this crate refused the entire string.
        #[test]
        fn a_trailing_empty_value_is_accepted() {
            let config: Config = "dbname=zs password="
                .parse()
                .expect("libpq accepts a trailing empty value");
            assert_eq!(config.get_dbname(), Some("zs"));
            assert_eq!(
                config.get_password(),
                None,
                "an empty password is unset, not a password of length zero"
            );
        }

        #[test]
        fn empty_identity_values_restore_their_defaults() {
            let parsed = "user=alice user='' password=secret password='' dbname=app dbname=''"
                .parse::<Config>()
                .expect("empty identity values are default selections");
            assert_eq!(parsed.get_user(), None);
            assert_eq!(parsed.get_password(), None);
            assert_eq!(parsed.get_dbname(), None);

            let mut built = Config::new();
            built
                .user("alice")
                .user("")
                .password("secret")
                .password([])
                .dbname("app")
                .dbname("");
            assert_eq!(built.get_user(), None);
            assert_eq!(built.get_password(), None);
            assert_eq!(built.get_dbname(), None);
        }

        /// Port is the one integer option whose documented empty value selects
        /// the compiled default.
        #[test]
        fn an_empty_port_value_leaves_the_default() {
            let config: Config = "host=x.invalid port="
                .parse()
                .expect("libpq documents an empty port as the compiled default");
            assert!(config.get_ports().is_empty(), "port= must not set a port");
        }

        // NON-DISCRIMINATING NOW: passes with the `port` arm in `param` reverted,
        // because duplicates collapse to the last value. It DID fail that way at
        // 618825f82, the commit that added it; `ParsedParameters` disarmed it
        // later. See the note on `Config::param`.
        #[test]
        fn a_later_empty_port_restores_the_compiled_default() {
            let config: Config = "host=x.invalid port=5455 port="
                .parse()
                .expect("the last non-null port value wins in libpq");
            assert!(
                config.get_ports().is_empty(),
                "the empty override retained the earlier explicit port"
            );
        }

        #[test]
        fn empty_libpq_integer_values_are_refused_by_name() {
            let mut accepted = Vec::new();
            let mut unnamed = Vec::new();
            for key in [
                "connect_timeout",
                "tcp_user_timeout",
                "keepalives",
                "keepalives_idle",
                "keepalives_interval",
                "keepalives_count",
            ] {
                let dsn = format!("host=x.invalid {key}=");
                match dsn.parse::<Config>() {
                    Ok(_) => accepted.push(key),
                    Err(error) => {
                        let mut names_key = error.to_string().contains(key);
                        let mut source = std::error::Error::source(&error);
                        while let Some(cause) = source {
                            names_key |= cause.to_string().contains(key);
                            source = std::error::Error::source(cause);
                        }
                        if !names_key {
                            unnamed.push(key);
                        }
                    }
                }
            }
            assert!(
                accepted.is_empty(),
                "empty integer values were silently accepted: {}",
                accepted.join(", ")
            );
            assert!(
                unnamed.is_empty(),
                "empty integer values were refused without naming: {}",
                unnamed.join(", ")
            );
        }

        /// And so `port= connect_timeout=` is NOT two empty numerics: `port`
        /// swallows the text after it and is asked to parse `connect_timeout=`
        /// as an integer. libpq says `invalid integer value "connect_timeout="
        /// for connection option "port"`, and refusing it is correct.
        #[test]
        fn a_swallowed_numeric_value_is_still_rejected() {
            assert!(
                "host=x.invalid port= connect_timeout="
                    .parse::<Config>()
                    .is_err(),
                "port was handed the text `connect_timeout=` and must refuse it"
            );
        }

        /// The control for the two above: empty is NOT universally accepted, so
        /// a parser that simply stopped rejecting empty values would pass them
        /// and fail this. libpq answers `invalid sslmode value: ""`.
        #[test]
        fn an_empty_enum_value_is_still_refused() {
            for dsn in [
                "host=x.invalid sslmode=",
                "host=x.invalid channel_binding=",
                "host=x.invalid target_session_attrs=",
            ] {
                assert!(
                    dsn.parse::<Config>().is_err(),
                    "an empty enum value must be refused, as libpq refuses it: {dsn}"
                );
            }
        }

        /// An empty value only ever arises at the END of the string: after
        /// `=`, libpq skips whitespace and takes whatever comes next as the
        /// value, so `user= host=h` asks for a USER NAMED `host=h` and leaves
        /// no host at all. Measured - psql reports `role "host=x.invalid" does
        /// not exist`. It reads like a bug in this crate and is not one.
        #[test]
        fn an_empty_value_mid_string_swallows_the_following_pair() {
            let config: Config = "user= host=x.invalid".parse().expect("this parses");
            assert_eq!(config.get_user(), Some("host=x.invalid"));
            assert!(
                config.get_hosts().is_empty(),
                "the host was consumed as the user's value, exactly as libpq does"
            );
        }
    }

    /// The service-precedence rule, measured against libpq: an explicitly
    /// given key wins REGARDLESS OF ORDER, so these exercise
    /// `Config::fill_unset` directly rather than going through the file
    /// lookup, which reads process-wide environment variables.
    mod service_precedence {
        use crate::Config;

        fn service_params() -> Vec<(String, String)> {
            vec![
                ("host".to_owned(), "service.example".to_owned()),
                ("dbname".to_owned(), "service_db".to_owned()),
                ("user".to_owned(), "service_user".to_owned()),
            ]
        }

        #[test]
        fn a_service_fills_every_key_the_caller_omitted() {
            let mut config = Config::new();
            config
                .fill_unset(service_params(), &[])
                .expect("the service parameters are valid");

            assert_eq!(config.get_dbname(), Some("service_db"));
            assert_eq!(config.get_user(), Some("service_user"));
        }

        /// The control for the test above: with the key named as explicit, the
        /// service value must NOT land, whatever the config currently holds.
        #[test]
        fn an_explicit_key_is_not_overwritten_by_the_service() {
            let mut config = Config::new();
            config.dbname("explicit_db");
            config
                .fill_unset(service_params(), &["dbname".to_owned()])
                .expect("the service parameters are valid");

            assert_eq!(
                config.get_dbname(),
                Some("explicit_db"),
                "the service overrode a parameter the caller gave"
            );
            // The keys that were NOT explicit still come from the service, or
            // this test would pass for a `fill_unset` that does nothing at all.
            assert_eq!(config.get_user(), Some("service_user"));
        }

        #[test]
        fn explicit_requiressl_blocks_the_service_sslmode_alias() {
            let mut config = Config::new();
            config
                .param("requiressl", "0")
                .expect("requiressl=0 is sslmode=prefer");
            config
                .fill_unset(
                    vec![("sslmode".to_owned(), "require".to_owned())],
                    &["requiressl".to_owned()],
                )
                .expect("the service sslmode must be shadowed");

            assert_eq!(
                config.get_ssl_mode(),
                crate::config::SslMode::Prefer,
                "the service overrode an explicit spelling of the same option"
            );
        }

        /// A section may name one key twice, and libpq takes the FIRST. Probed
        /// against 16.14 with two `host` lines: it resolved `first.invalid`.
        /// Applying every pair in order takes the last instead, which is the
        /// opposite, and silently connects somewhere the operator did not.
        #[test]
        fn the_first_of_two_values_for_one_key_wins() {
            let mut config = Config::new();
            config
                .fill_unset(
                    vec![
                        ("dbname".to_owned(), "first_db".to_owned()),
                        ("dbname".to_owned(), "second_db".to_owned()),
                    ],
                    &[],
                )
                .expect("a repeated key is not a parse error");

            assert_eq!(config.get_dbname(), Some("first_db"));
        }

        /// Libpq rejects a service that selects another service instead of
        /// silently dropping the nested selection.
        #[test]
        fn a_service_key_inside_a_service_is_rejected() {
            let mut config = Config::new();
            let error = config
                .fill_unset(vec![("service".to_owned(), "another".to_owned())], &[])
                .expect_err("a nested service specification must be rejected");
            let mut text = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                text.push_str(" | ");
                text.push_str(&cause.to_string());
                source = std::error::Error::source(cause);
            }
            assert!(
                text.contains("nested service") && text.contains("service"),
                "the rejection must explain and name the nested key: {text}"
            );
        }

        /// `requiressl` is a conninfo-parser compatibility alias, not an
        /// entry in libpq's connection-option table. Its service-file parser
        /// therefore rejects it instead of translating it to `sslmode`.
        #[test]
        fn requiressl_is_not_accepted_from_a_service() {
            let mut config = Config::new();
            let error = config
                .fill_unset(vec![("requiressl".to_owned(), "0".to_owned())], &[])
                .expect_err("requiressl is not a valid service-file key");

            assert!(
                error_chain_contains(&error, "requiressl"),
                "the rejection must name the invalid service key: {error:?}"
            );
        }

        /// `servicefile` is this driver's explicit replacement for libpq's
        /// process-environment search. Once a service is being expanded it is
        /// too late for the value to select that service's source file.
        #[test]
        fn servicefile_is_not_accepted_from_a_service() {
            let mut config = Config::new();
            let error = config
                .fill_unset(vec![("servicefile".to_owned(), "/ignored".to_owned())], &[])
                .expect_err("servicefile inside a service would be ignored");

            assert!(
                error_chain_contains(&error, "servicefile"),
                "the rejection must name the ineffective service key: {error:?}"
            );
        }

        #[test]
        fn an_unknown_key_in_a_service_is_rejected_by_name() {
            let mut config = Config::new();
            let error = config
                .fill_unset(vec![("notakey".to_owned(), "1".to_owned())], &[])
                .expect_err("an unknown parameter is not silently ignored");
            let names_the_key = std::iter::successors(std::error::Error::source(&error), |error| {
                std::error::Error::source(*error)
            })
            .any(|cause| cause.to_string().contains("notakey"));
            assert!(
                names_the_key,
                "the error does not name the offending key: {error:?}"
            );
        }

        fn error_chain_contains(error: &(dyn std::error::Error + 'static), needle: &str) -> bool {
            std::iter::successors(Some(error), |error| std::error::Error::source(*error))
                .any(|cause| cause.to_string().contains(needle))
        }
    }

    use crate::config::{
        AuthMethod, AuthMethods, ReplicationMode, RequireAuth, SslCertMode, SslMode,
        SslNegotiation, SslProtocolVersion, SslRootCert, TargetSessionAttrs,
    };
    use crate::{Config, config::Host};

    /// A DSN zero must reach `KeepaliveConfig` so the one conversion that
    /// drops it can.
    ///
    /// `keepalive.rs` says the zero is dropped in `TcpKeepalive::from` because
    /// "this is the single point every entry path passes through". It was not:
    /// the DSN arms guarded their setters with `> 0`, so a zero never became a
    /// value at all and `keepalives_idle` kept this crate's OWN default of two
    /// hours - neither the failure libpq produces nor the system default
    /// PostgreSQL documents. Only `keepalives_count` reached the conversion,
    /// which is why only it was measured.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_zero_keepalive_in_a_dsn_reaches_the_conversion_that_drops_it() {
        let config = "host=h keepalives_idle=0 keepalives_interval=0 keepalives_count=0"
            .parse::<Config>()
            .expect("a zero keepalive is a legal DSN value");
        assert_eq!(
            config.get_keepalives_idle(),
            Duration::ZERO,
            "keepalives_idle=0 was discarded and left this crate's two-hour default in place"
        );
        assert_eq!(
            config.get_keepalives_interval(),
            Some(Duration::ZERO),
            "keepalives_interval=0 was discarded rather than recorded"
        );
        assert_eq!(
            config.get_keepalives_count(),
            Some(0),
            "keepalives_count=0 was discarded rather than recorded"
        );
    }

    /// One variable away: a POSITIVE value must still land, or the assertions
    /// above would be satisfied by a parser that ignored these keys entirely.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_positive_keepalive_in_a_dsn_still_lands() {
        let config = "host=h keepalives_idle=30 keepalives_interval=5 keepalives_count=7"
            .parse::<Config>()
            .expect("a positive keepalive is a legal DSN value");
        assert_eq!(config.get_keepalives_idle(), Duration::from_secs(30));
        assert_eq!(
            config.get_keepalives_interval(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(config.get_keepalives_count(), Some(7));
    }

    /// A NEGATIVE is refused, and must NOT be folded into the zero above.
    ///
    /// Each of the three is asserted on its own DSN, because a single DSN
    /// carrying all three would go red on the first arm that still refused and
    /// say nothing about the other two.
    ///
    /// The reasoning is separate from the zero's, and deliberately does not
    /// rest on what libpq does internally. Measured 2026-08-23 against
    /// PostgreSQL 16, `psql` fails the connection for a negative in any of the
    /// three - and for a ZERO in any of the three, with the same
    /// `setsockopt(...) failed: Invalid argument`. Those two outcomes are
    /// identical, so that measurement cannot show whether libpq clamps or
    /// passes through, and nothing here depends on it. What it does show is
    /// that a negative is a value neither implementation runs with; refusing
    /// it at parse time only moves the failure earlier. Folding it into zero
    /// instead would make a typo'd `keepalives_idle=-1` connect silently on
    /// system defaults, which is worse than either.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_negative_keepalive_in_a_dsn_is_refused() {
        for dsn in [
            "host=h keepalives_idle=-1",
            "host=h keepalives_interval=-1",
            "host=h keepalives_count=-1",
        ] {
            assert!(
                dsn.parse::<Config>().is_err(),
                "a negative keepalive was accepted rather than refused: {dsn}"
            );
        }
    }

    /// A value that is not a number at all is refused too, so the arm above
    /// cannot be satisfied by a parser that refuses every keepalive value.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_non_integer_keepalive_in_a_dsn_is_refused() {
        for dsn in [
            "host=h keepalives_idle=abc",
            "host=h keepalives_interval=abc",
            "host=h keepalives_count=abc",
        ] {
            assert!(
                dsn.parse::<Config>().is_err(),
                "a non-numeric keepalive was accepted: {dsn}"
            );
        }
    }

    fn assert_target_session_attrs_parses(value: &str, expected: TargetSessionAttrs) {
        for dsn in [
            format!("host=h target_session_attrs={value}"),
            format!("postgresql://h/db?target_session_attrs={value}"),
        ] {
            let config = dsn.parse::<Config>().unwrap_or_else(|error| {
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
        assert_target_session_attrs_parses("prefer-standby", TargetSessionAttrs::PreferStandby);
    }

    #[test]
    fn replication_values_are_case_insensitive() {
        for (value, expected) in [
            ("TrUe", Some(ReplicationMode::Physical)),
            ("DaTaBaSe", Some(ReplicationMode::Logical)),
            ("OfF", None),
        ] {
            let config = format!("host=h replication={value}")
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("replication={value} was refused: {error}"));
            assert_eq!(config.get_replication(), expected, "replication={value}");
        }
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
                .validate_connection_settings()
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
            .validate_connection_settings()
            .expect("verify-full is the mode sslrootcert=system exists for");
    }

    #[test]
    fn sslrootcert_system_strengthens_only_the_implicit_sslmode() {
        let implicit = "host=h sslrootcert=system".parse::<Config>().unwrap();
        assert_eq!(implicit.get_ssl_mode(), SslMode::VerifyFull);
        implicit
            .validate_connection_settings()
            .expect("system roots derive verify-full when sslmode is omitted");

        let mut built = Config::new();
        built.ssl_root_cert(SslRootCert::System);
        assert_eq!(built.get_ssl_mode(), SslMode::VerifyFull);

        for dsn in [
            "host=h sslmode=prefer sslrootcert=system",
            "host=h sslrootcert=system sslmode=prefer",
        ] {
            dsn.parse::<Config>()
                .unwrap()
                .validate_connection_settings()
                .expect_err("an explicitly weak sslmode must remain an error");
        }
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
                .validate_connection_settings()
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
                .validate_connection_settings()
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
                Some("127.0.0.1".parse::<IpAddr>().unwrap()),
                Some("127.0.0.2".parse::<IpAddr>().unwrap())
            ],
            config.get_hostaddrs(),
        );

        assert_eq!(1, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn at_prefixed_hosts_use_the_abstract_unix_namespace() {
        use std::os::unix::ffi::OsStrExt as _;

        for dsn in [
            "host=@zeroship",
            "postgresql:///db?host=%40zeroship",
            "postgresql://%40zeroship/db",
        ] {
            let config = dsn
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("{dsn} did not parse: {error}"));
            match config.get_hosts() {
                [Host::Unix(path)] => {
                    assert_eq!(path.as_os_str().as_bytes(), b"\0zeroship", "{dsn}");
                }
                hosts => panic!("{dsn} was not an abstract Unix socket host: {hosts:?}"),
            }
        }
    }

    #[test]
    fn test_invalid_hostaddr_parsing() {
        let s = "user=pass_user dbname=postgres host=host1 hostaddr=127.0.0 port=26257";
        s.parse::<Config>().err().unwrap();
    }

    /// The MAIN password must not reach `Debug`, whichever way it arrived.
    ///
    /// `ssl_password` has been covered since it was added; this one - the
    /// secret every connection carries, and the one most likely to be in a
    /// log line - had nothing. A `Config` is `Debug`-printed in error paths
    /// and by callers tracing their own setup, so a leak here ends up at rest
    /// in whatever collects those logs.
    #[test]
    fn the_password_never_reaches_debug_output() {
        const SECRET: &str = "hunter2-do-not-log";

        // From a keyword DSN, from a URL, and from the builder: three
        // different paths into the same field, and a redaction that covered
        // only one of them would be worth nothing.
        let from_keywords: Config = format!("host=h user=u password={SECRET}")
            .parse()
            .expect("the keyword DSN parses");
        let from_url: Config = format!("postgres://u:{SECRET}@h/db")
            .parse()
            .expect("the URL parses");
        let mut from_builder = Config::new();
        from_builder.host("h").user("u").password(SECRET);

        for (source, config) in [
            ("keyword DSN", from_keywords),
            ("URL", from_url),
            ("builder", from_builder),
        ] {
            assert_eq!(
                config.get_password(),
                Some(SECRET.as_bytes()),
                "{source}: the password did not survive parsing, so the \
                 redaction check below would pass for the wrong reason"
            );

            let debug = format!("{config:?}");
            assert!(
                !debug.contains(SECRET),
                "{source}: Debug leaked the password: {debug}"
            );
            // THE CONTROL. `!contains` is satisfied by an empty string, so
            // prove the output is real and still useful for diagnosis.
            assert!(
                debug.contains("user"),
                "{source}: Debug printed nothing recognisable, so the leak \
                 assertion above proves nothing: {debug}"
            );
        }
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
    fn sslcertmode_sslsni_and_requirepeer_parse_their_closed_sets() {
        assert_eq!(Config::new().get_ssl_cert_mode(), SslCertMode::Allow);
        assert!(Config::new().get_ssl_sni());
        assert_eq!(Config::new().get_require_peer(), None);

        for (value, expected) in [
            ("disable", SslCertMode::Disable),
            ("allow", SslCertMode::Allow),
            ("require", SslCertMode::Require),
        ] {
            let config = format!("host=h sslcertmode={value}")
                .parse::<Config>()
                .unwrap();
            assert_eq!(config.get_ssl_cert_mode(), expected);
            assert_eq!(expected.as_str(), value);
        }
        for value in ["", "prefer", "ALLOW"] {
            assert!(
                format!("host=h sslcertmode='{value}'")
                    .parse::<Config>()
                    .is_err(),
                "sslcertmode={value:?} parsed"
            );
        }

        assert!("host=h sslsni=1".parse::<Config>().unwrap().get_ssl_sni());
        assert!(!"host=h sslsni=0".parse::<Config>().unwrap().get_ssl_sni());
        for value in ["", "2", "true", "10"] {
            assert!(
                format!("host=h sslsni='{value}'")
                    .parse::<Config>()
                    .is_err(),
                "sslsni={value:?} parsed"
            );
        }

        let required = "host=h requirepeer=postgres".parse::<Config>().unwrap();
        assert_eq!(required.get_require_peer(), Some("postgres"));
        let empty = "host=h requirepeer=''".parse::<Config>().unwrap();
        assert_eq!(empty.get_require_peer(), None);
    }

    #[test]
    fn required_client_certificate_cannot_be_combined_with_disabled_tls() {
        let config = "host=h sslmode=disable sslcertmode=require"
            .parse::<Config>()
            .unwrap();
        let error = config
            .validate_connection_settings()
            .expect_err("plaintext cannot send a required TLS client certificate");
        let text = error.to_string()
            + &std::error::Error::source(&error)
                .map(|source| format!(": {source}"))
                .unwrap_or_default();
        assert!(
            text.contains("sslcertmode=require") && text.contains("sslmode=disable"),
            "the contradiction was not named: {text}"
        );
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
        for key in ["ssl_min_protocol_version", "ssl_max_protocol_version"] {
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
                .validate_connection_settings()
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
            "host=h sslrootcert=system"
                .parse::<Config>()
                .unwrap()
                .get_ssl_root_cert(),
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
        for s in ["host=h sslrootcert=", "host=h sslcert=", "host=h sslkey="] {
            s.parse::<Config>()
                .err()
                .unwrap_or_else(|| panic!("{s} parsed, so an empty path reads as absent"));
        }
    }

    #[test]
    fn sslkey_engine_file_path_control() {
        "host=h sslkey=/tmp/client.key"
            .parse::<Config>()
            .expect("a regular sslkey path must still parse")
            .validate_connection_settings()
            .expect("a regular sslkey path must remain valid connection configuration");
    }

    #[test]
    fn sslkey_engine_specifier_is_refused_during_parse() {
        let error = "host=h sslkey=pkcs11:client-key"
            .parse::<Config>()
            .expect_err("an OpenSSL engine key cannot be implemented by rustls");
        let cause = std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            cause.contains("sslkey") && cause.contains("does not implement"),
            "the refusal must name the unsupported sslkey feature: {cause}"
        );
    }

    #[test]
    fn sslkey_engine_specifier_is_refused_for_programmatic_config() {
        let mut config = Config::new();
        config.ssl_key("pkcs11:client-key");
        let error = config
            .validate_connection_settings()
            .expect_err("the programmatic setter must not bypass engine-key refusal");
        let cause = std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            cause.contains("sslkey") && cause.contains("engine"),
            "the refusal must name the unsupported sslkey engine: {cause}"
        );
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
    fn statement_cache_execution_threshold_is_nonzero_and_programmatic() {
        assert_eq!(
            Config::new().get_statement_cache_execution_threshold(),
            NonZeroUsize::MIN
        );

        let mut config = Config::new();
        config.statement_cache_execution_threshold(NonZeroUsize::new(5).unwrap());
        assert_eq!(
            config.get_statement_cache_execution_threshold(),
            NonZeroUsize::new(5).unwrap()
        );
        assert!(NonZeroUsize::new(0).is_none());

        for dsn in [
            "host=h statement_cache_execution_threshold=5",
            "postgresql://h/db?statement_cache_execution_threshold=5",
        ] {
            let error = dsn
                .parse::<Config>()
                .expect_err("the builder-only threshold parsed from a connection string");
            let cause = std::error::Error::source(&error)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                cause.contains("statement_cache_execution_threshold"),
                "{dsn}: rejection did not identify the unsupported key: {cause:?}"
            );
        }
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
            &RequireAuth::Reject(AuthMethods::new(AuthMethod::Password).with(AuthMethod::None))
        );
        assert_eq!(
            "host=h require_auth=!gss,!sspi"
                .parse::<Config>()
                .unwrap()
                .get_require_auth(),
            &RequireAuth::Reject(AuthMethods::new(AuthMethod::Gss).with(AuthMethod::Sspi))
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

    fn error_chain(error: &Error) -> String {
        std::iter::successors(std::error::Error::source(error), |cause| {
            std::error::Error::source(*cause)
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ")
    }

    fn service_file_with_host_and_port() -> tempfile::NamedTempFile {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().expect("create service file");
        writeln!(file, "[uri-defaults]\nhost=service.example\nport=6543")
            .expect("write service file");
        file.flush().expect("flush service file");
        file
    }

    fn service_file_with_user() -> tempfile::NamedTempFile {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().expect("create service file");
        writeln!(file, "[uri-user]\nuser=service-user").expect("write service file");
        file.flush().expect("flush service file");
        file
    }

    #[test]
    fn uri_percent_decoded_nul_is_forbidden() {
        for dsn in [
            "postgresql://user:a%00b@host/db",
            "postgresql://host/d%00b",
            "postgresql://host/db?application_name=a%00b",
        ] {
            let Err(error) = dsn.parse::<Config>() else {
                panic!("percent-decoded NUL parsed in {dsn:?}");
            };
            let chain = error_chain(&error);
            assert!(
                chain.contains("forbidden value %00"),
                "the NUL refusal did not identify the bad encoding in {dsn:?}: {chain}"
            );
        }
    }

    #[test]
    fn uri_boundary_spaces_are_trimmed_before_percent_decoding() {
        let config = "postgresql:// user : secret @ host : 5432 / db ? application_name = app "
            .parse::<Config>()
            .expect("libpq trims raw spaces at component boundaries");

        assert_eq!(config.get_user(), Some("user"));
        assert_eq!(config.get_password(), Some(b"secret".as_slice()));
        assert_eq!(config.get_hosts(), [Host::Tcp("host".to_owned())]);
        assert_eq!(config.get_ports(), [5432]);
        assert_eq!(config.get_dbname(), Some("db"));
        assert_eq!(config.get_application_name(), Some("app"));

        let encoded = "postgresql://host/db?application_name=%20app%20"
            .parse::<Config>()
            .expect("percent-encoded boundary spaces are data, not padding");
        assert_eq!(encoded.get_application_name(), Some(" app "));
    }

    #[test]
    fn an_explicitly_blank_raw_uri_user_blocks_a_service_user() {
        let service_file = service_file_with_user();
        let query = format!(
            "servicefile={}&service=uri-user",
            service_file.path().display()
        );

        let absent = format!("postgresql://@host/db?{query}")
            .parse::<Config>()
            .expect("the service supplies an absent URI user");
        assert_eq!(absent.get_user(), Some("service-user"));

        let blank = format!("postgresql:// @host/db?{query}")
            .parse::<Config>()
            .expect("libpq accepts a raw-space URI user as explicitly blank");
        assert_eq!(
            blank.get_user(),
            None,
            "an explicitly blank URI user must not inherit the service user"
        );
    }

    #[test]
    fn bracketed_uri_hosts_scan_delimiters_after_the_closing_bracket() {
        for (dsn, expected_hosts) in [
            ("postgresql://[a,b]/db", &["a", "b"][..]),
            ("postgresql://[a?b]/db", &["a?b"][..]),
            ("postgresql://[a/b]/db", &["a/b"][..]),
        ] {
            let config = dsn
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("libpq accepts {dsn:?}: {error}"));
            let hosts = config
                .get_hosts()
                .iter()
                .map(|host| match host {
                    Host::Tcp(host) => host.as_str(),
                    #[cfg(unix)]
                    Host::Unix(path) => panic!("{dsn:?} became a Unix host: {path:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(hosts, expected_hosts, "{dsn}");
            assert_eq!(config.get_dbname(), Some("db"), "{dsn}");
        }
    }

    #[test]
    fn uri_authority_port_list_is_decoded_before_parsing_items() {
        "postgresql://first: 5432 ,second: 5433 /db"
            .parse::<Config>()
            .expect_err("libpq rejects spaces internal to the assembled port option");

        let one = "postgresql://first: 5432 /db"
            .parse::<Config>()
            .expect("boundary spaces around one authority port are valid");
        assert_eq!(one.get_ports(), [5432]);
    }

    #[test]
    fn uri_query_rejects_a_second_raw_equals() {
        "postgresql://host/db?application_name=a=b"
            .parse::<Config>()
            .expect_err("libpq rejects an extra raw key/value separator");

        let encoded = "postgresql://host/db?application_name=a%3Db"
            .parse::<Config>()
            .expect("an encoded equals sign is ordinary value data");
        assert_eq!(encoded.get_application_name(), Some("a=b"));
    }

    #[test]
    fn empty_bracketed_uri_host_is_rejected() {
        "postgresql://[]/db"
            .parse::<Config>()
            .expect_err("libpq rejects an empty bracketed IPv6 host");

        let valid = "postgresql://[::1]/db"
            .parse::<Config>()
            .expect("a nonempty bracketed IPv6 host is valid");
        assert_eq!(valid.get_hosts(), [Host::Tcp("::1".to_owned())]);
    }

    #[test]
    fn uri_ssl_true_maps_to_sslmode_require() {
        let config = "postgresql://host/db?ssl=true"
            .parse::<Config>()
            .expect("libpq supports the JDBC ssl=true URI spelling");
        assert_eq!(config.get_ssl_mode(), SslMode::Require);

        for (dsn, expected) in [
            (
                "postgresql://host/db?ssl=true&sslmode=disable",
                SslMode::Disable,
            ),
            (
                "postgresql://host/db?sslmode=disable&ssl=true",
                SslMode::Require,
            ),
        ] {
            let config = dsn
                .parse::<Config>()
                .expect("ssl=true and sslmode share one last-value-wins slot");
            assert_eq!(config.get_ssl_mode(), expected, "{dsn}");
        }

        "postgresql://host/db?ssl=false"
            .parse::<Config>()
            .expect_err("only the documented ssl=true spelling is special");
    }

    #[test]
    fn uri_percent_decoded_host_commas_split_the_host_list() {
        for dsn in [
            "postgresql://first%2Csecond/db",
            "postgresql:///db?host=first%2Csecond",
        ] {
            let config = dsn
                .parse::<Config>()
                .unwrap_or_else(|error| panic!("{dsn:?} did not parse: {error}"));
            assert_eq!(
                config.get_hosts(),
                [
                    Host::Tcp("first".to_owned()),
                    Host::Tcp("second".to_owned()),
                ],
                "libpq decodes the host option before splitting it: {dsn:?}"
            );
        }
    }

    #[test]
    fn empty_uri_query_host_is_an_unset_host_list() {
        let config = "postgresql://authority/db?host=&hostaddr=127.0.0.1,127.0.0.2"
            .parse::<Config>()
            .expect("an empty query host clears the authority host");

        assert!(
            config.get_hosts().is_empty(),
            "host= must be absent, not a one-element empty host list"
        );
        assert_eq!(config.get_hostaddrs().len(), 2);
    }

    #[test]
    fn single_uri_authority_omissions_are_filled_by_the_service() {
        let service_file = service_file_with_host_and_port();
        let query = format!(
            "servicefile={}&service=uri-defaults",
            service_file.path().display()
        );

        for (authority, expected_host, expected_port) in [
            ("authority.example", "authority.example", 6543),
            ("authority.example:", "authority.example", 6543),
            (":5455", "service.example", 5455),
            (":", "service.example", 6543),
        ] {
            let dsn = format!("postgresql://{authority}/db?{query}");
            let config = dsn.parse::<Config>().unwrap_or_else(|error| {
                panic!("service defaults did not complete {dsn:?}: {error:?}")
            });

            assert_eq!(
                config.get_hosts(),
                [Host::Tcp(expected_host.to_owned())],
                "wrong host for authority {authority:?}"
            );
            assert_eq!(
                config.get_ports(),
                [expected_port],
                "wrong port for authority {authority:?}"
            );
        }
    }

    #[test]
    fn multi_host_uri_authority_keeps_its_empty_port_slots() {
        let service_file = service_file_with_host_and_port();
        let dsn = format!(
            "postgresql://first.example,second.example/db?servicefile={}&service=uri-defaults",
            service_file.path().display()
        );
        let config = dsn.parse::<Config>().expect("parse multi-host URI");

        assert_eq!(
            config.get_hosts(),
            [
                Host::Tcp("first.example".to_owned()),
                Host::Tcp("second.example".to_owned()),
            ]
        );
        assert_eq!(
            config.get_ports(),
            [5432, 5432],
            "the multi-host port buffer is nonempty because it contains a comma"
        );
    }

    #[test]
    fn only_the_last_keyword_value_is_semantically_validated() {
        let cases = [
            ("host=h port=abc port=5455", "port"),
            ("host=h sslmode=bogus sslmode=disable", "sslmode"),
            (
                "host=h hostaddr=not-an-address hostaddr=127.0.0.1",
                "hostaddr",
            ),
            ("host=h gssencmode=require gssencmode=disable", "gssencmode"),
            ("host=h requiressl=1 sslmode=disable", "requiressl/sslmode"),
        ];

        for (dsn, parameter) in cases {
            dsn.parse::<Config>().unwrap_or_else(|error| {
                panic!("the later valid {parameter} did not shadow the earlier value: {error:?}")
            });
        }

        let port = "host=h port=abc port=5455".parse::<Config>().unwrap();
        assert_eq!(port.get_ports(), [5455]);

        let hostaddr = "host=h hostaddr=bad hostaddr=127.0.0.1"
            .parse::<Config>()
            .unwrap();
        assert_eq!(
            hostaddr.get_hostaddrs(),
            [Some("127.0.0.1".parse::<IpAddr>().unwrap())]
        );

        let sslmode = "host=h requiressl=1 sslmode=disable"
            .parse::<Config>()
            .unwrap();
        assert_eq!(sslmode.get_ssl_mode(), SslMode::Disable);
    }

    #[test]
    fn only_the_last_uri_query_value_is_semantically_validated() {
        let cases = [
            "postgresql://h/db?port=abc&port=5455",
            "postgresql://h/db?sslmode=bogus&sslmode=disable",
            "postgresql://h/db?hostaddr=bad&hostaddr=127.0.0.1",
            "postgresql://h/db?gssencmode=require&gssencmode=disable",
            "postgresql://h/db?requiressl=1&sslmode=disable",
        ];

        for dsn in cases {
            dsn.parse::<Config>().unwrap_or_else(|error| {
                panic!("a later valid query value did not shadow the earlier value in {dsn:?}: {error:?}")
            });
        }

        let port = cases[0].parse::<Config>().unwrap();
        assert_eq!(port.get_ports(), [5455]);

        let hostaddr = cases[2].parse::<Config>().unwrap();
        assert_eq!(
            hostaddr.get_hostaddrs(),
            [Some("127.0.0.1".parse::<IpAddr>().unwrap())]
        );

        let sslmode = cases[4].parse::<Config>().unwrap();
        assert_eq!(sslmode.get_ssl_mode(), SslMode::Disable);
    }

    #[test]
    fn a_query_port_can_shadow_an_invalid_authority_port() {
        let config = "postgresql://h:not-a-port/db?port=5455"
            .parse::<Config>()
            .expect("the query port is the final value");
        assert_eq!(config.get_ports(), [5455]);
    }

    #[test]
    fn uri_authority_ports_use_libpq_integer_grammar() {
        for port in ["%20+5455%20", "%095455%0A"] {
            let dsn = format!("postgresql://h:{port}/db");
            let config = dsn.parse::<Config>().unwrap_or_else(|error| {
                panic!("the percent-decoded C integer port in {dsn:?} was refused: {error:?}")
            });
            assert_eq!(config.get_ports(), [5455]);
        }
    }

    #[test]
    fn an_unknown_parameter_is_not_shadowed() {
        let error = "host=h unknown_connection_option=bad sslmode=disable"
            .parse::<Config>()
            .expect_err("an unknown name must still be refused");
        let named = std::iter::successors(std::error::Error::source(&error), |cause| {
            std::error::Error::source(*cause)
        })
        .any(|cause| cause.to_string().contains("unknown_connection_option"));
        assert!(
            named,
            "the refusal did not name the unknown parameter: {error:?}"
        );
    }

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
                .unwrap_or_else(|error| {
                    panic!("client_encoding={spelling} did not parse: {error}")
                });
        }

        let refused = "host=h client_encoding=LATIN1"
            .parse::<Config>()
            .expect_err("a non-UTF8 client_encoding must be refused, not ignored");
        // The top-level Display is only "invalid connection string", so the key
        // has to be reachable through the source chain or the caller cannot
        // tell which option they got wrong.
        let named = std::iter::successors(std::error::Error::source(&refused), |error| {
            std::error::Error::source(*error)
        })
        .any(|cause| cause.to_string().contains("client_encoding"));
        assert!(named, "no cause names the offending key: {refused}");
    }
}
