//! The hard failure that replaced this crate's skip announcer.
//!
//! NO `skip` FUNCTION AND NO `ZEROSHIP-TEST-SKIPPED` MARKER, and their absence
//! is the change. Both were here, copied from `crates/test-support`, and
//! `require_pg` announced through them when Postgres could not be reached - a
//! skip, which cargo counts as a pass. This crate is the PostgreSQL driver;
//! there is no test in it that means anything without a server, so there is
//! nothing left for an announcer to announce. Every path that used to skip now
//! calls [`postgres_unreachable`], which panics.
//!
//! `compio-postgres` is a standalone, publishable driver with no zeroship
//! dependency, so this helper is local rather than shared.

pub mod env;

/// Longest identifier `PostgreSQL` stores (`NAMEDATALEN - 1`).
const MAX_POSTGRES_IDENTIFIER_LEN: usize = 63;

/// Build an unquoted `PostgreSQL` identifier private to this test process.
///
/// The readable prefix is normalised to `[a-z0-9_]`, while the PID makes two
/// concurrent test binaries choose different server-side namespaces. The hash
/// covers the original logical name and the PID, so truncating a long readable
/// prefix cannot merge two logical names. The result is ASCII, begins with a
/// legal unquoted-identifier character, and never exceeds `PostgreSQL`'s
/// 63-byte identifier limit.
pub fn test_object_name(logical: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let pid = std::process::id();
    let mut hasher = DefaultHasher::new();
    logical.hash(&mut hasher);
    pid.hash(&mut hasher);
    let digest = hasher.finish();

    let mut readable: String = logical
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if readable.is_empty() {
        readable.push_str("object");
    } else if readable.as_bytes()[0].is_ascii_digit() {
        readable.insert(0, '_');
    }

    let suffix = format!("_{pid}_{digest:016x}");
    let readable_budget = MAX_POSTGRES_IDENTIFIER_LEN - suffix.len();
    readable.truncate(readable_budget);
    format!("{readable}{suffix}")
}

/// Hide the password in a `postgres://user:pass@host/db` DSN.
///
/// The whole point of the message below is that it prints the address that was
/// actually dialled, and the default DSN carries a password. Printing it into a
/// CI log to save a developer one guess is a bad trade, and `PG_TEST_URL` can
/// carry a real credential.
///
/// Only the userinfo between `://` and the LAST `@` before the first `/` of the
/// authority is touched, so a password containing `@` cannot leak a tail: the
/// scan for the separator runs to the end of the authority, not to the first
/// match. A DSN with no userinfo, or with a user and no password, is returned
/// with the same shape it came in.
pub fn redact_dsn(dsn: &str) -> String {
    let Some(scheme_end) = dsn.find("://") else {
        return dsn.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = dsn[authority_start..]
        .find(['/', '?'])
        .map_or(dsn.len(), |offset| authority_start + offset);
    let authority = &dsn[authority_start..authority_end];

    let Some(at) = authority.rfind('@') else {
        return dsn.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return dsn.to_string();
    };

    format!(
        "{}{}:***{}",
        &dsn[..authority_start],
        &userinfo[..colon],
        &dsn[authority_start + at..]
    )
}

/// Renders an error and every `source()` beneath it, joined with `": "`.
///
/// `compio_postgres::Error`'s own `Display` is a one-word kind - `Kind::Db`
/// prints the literal string `"db error"` (`src/error/mod.rs`, the `Display`
/// impl) - and everything that identifies the failure lives in the `DbError`
/// hanging off `source()`. Formatting the outer error alone therefore renders
/// every server-sent refusal, whatever it was, as `db error`.
///
/// The `SQLSTATE` is appended rather than taken from `DbError`'s `Display`,
/// which prints only `severity: message` and drops the code.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = String::new();
    let mut link = Some(error);
    while let Some(current) = link {
        if !rendered.is_empty() {
            rendered.push_str(": ");
        }
        rendered.push_str(&current.to_string());
        if let Some(db) = current.downcast_ref::<compio_postgres::error::DbError>() {
            rendered.push_str(&format!(" (SQLSTATE {})", db.code().code()));
        }
        link = current.source();
    }
    rendered
}

/// Whether the server answered. A `DbError` anywhere in the chain - this
/// driver's, or the `tokio-postgres` oracle's, whose connect failures the
/// differential suites also route through [`postgres_unreachable`] - is a
/// message `PostgreSQL` composed and sent, so something was listening, read
/// the startup packet far enough to reply, and refused on purpose.
pub fn server_answered(error: &(dyn std::error::Error + 'static)) -> bool {
    chain_contains(error, |link| {
        link.is::<compio_postgres::error::DbError>() || link.is::<tokio_postgres::error::DbError>()
    })
}

/// The kinds of every `io::Error` in the chain.
///
/// The classification below is taken on these rather than on either driver's
/// own error kind, because the driver kinds do not separate the cases. This
/// driver files a frame too large to be `PostgreSQL`'s under its
/// communication kind (`buf_stream::validate_length_against` builds it with
/// `Error::io`) and a startup-protocol violation under its connect kind
/// (`connect_raw::protocol_error` builds it with `Error::connect`), so a peer
/// that answered with the wrong protocol shares a kind with a refused dial.
/// The `io::ErrorKind` under it does not: both drivers mark bytes they cannot
/// accept `InvalidData`, and a socket that never produced a reply with the
/// socket-level kinds. `InvalidInput` is the one ambiguous kind; see
/// [`refused_as_invalid`].
fn io_kinds(error: &(dyn std::error::Error + 'static)) -> Vec<std::io::ErrorKind> {
    let mut kinds = Vec::new();
    let mut link = Some(error);
    while let Some(current) = link {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            kinds.push(io.kind());
        }
        link = current.source();
    }
    kinds
}

/// Whether the attempt ran out of time: an `io::Error` of kind `TimedOut`
/// anywhere in the chain. The driver reports an expired connect or pool bound
/// that way, and it says nothing about whether anything answered - a server
/// that drops packets and a server too slow to finish the handshake inside the
/// bound both end here.
fn timed_out(error: &(dyn std::error::Error + 'static)) -> bool {
    io_kinds(error).contains(&std::io::ErrorKind::TimedOut)
}

/// Whether something at the address answered with bytes the driver refused:
/// a frame too large or malformed to be `PostgreSQL`'s, or a TLS handshake
/// that failed on what the peer sent. Both drivers mark bytes they cannot
/// accept `InvalidData`, so a reply arrived and the address is served.
fn peer_answered_unintelligibly(error: &(dyn std::error::Error + 'static)) -> bool {
    io_kinds(error).contains(&std::io::ErrorKind::InvalidData)
}

/// Whether the attempt was refused as invalid, without saying by whom.
///
/// `InvalidInput` names both sides. `postgres-protocol` reports a message tag
/// the protocol does not define with it, which means a peer answered; std
/// reports `EINVAL` and a refused argument with it - an over-long Unix socket
/// path, a name that resolved to no address, a link-local address without a
/// zone - which means the client side refused before anything answered.
/// Neither is a missing server, and the report does not claim either.
fn refused_as_invalid(error: &(dyn std::error::Error + 'static)) -> bool {
    io_kinds(error).contains(&std::io::ErrorKind::InvalidInput)
}

/// Whether the address never produced a reply: nothing accepted the
/// connection, there was no route or no Unix socket at that path, or the peer
/// reset or closed the connection before answering.
///
/// A reset or an end of stream records no stage. A peer that accepted TLS -
/// answered `S` - and then closed mid-handshake lands here too. The only thing
/// in the chain that marks the handshake stage is this driver's own error
/// kind, which reaches it only as display text ("error performing TLS
/// handshake"), so that case is reported as a missing server although
/// something answered.
fn nothing_answered(error: &(dyn std::error::Error + 'static)) -> bool {
    use std::io::ErrorKind::{
        AddrNotAvailable, BrokenPipe, ConnectionAborted, ConnectionRefused, ConnectionReset,
        HostUnreachable, NetworkDown, NetworkUnreachable, NotConnected, NotFound, UnexpectedEof,
    };
    io_kinds(error).iter().any(|kind| {
        matches!(
            kind,
            ConnectionRefused
                | ConnectionReset
                | ConnectionAborted
                | NotConnected
                | BrokenPipe
                | UnexpectedEof
                | NotFound
                | AddrNotAvailable
                | HostUnreachable
                | NetworkUnreachable
                | NetworkDown
        )
    })
}

fn chain_contains(
    error: &(dyn std::error::Error + 'static),
    matches: impl Fn(&(dyn std::error::Error + 'static)) -> bool,
) -> bool {
    let mut link = Some(error);
    while let Some(current) = link {
        if matches(current) {
            return true;
        }
        link = current.source();
    }
    false
}

/// The endpoint behavior relevant to tests that observe session provenance.
///
/// PgBouncer exposes `SHOW CONFIG` only through its `pgbouncer` administration
/// database, and the `pool_mode=transaction` row is positive evidence that the
/// endpoint provides transaction pooling. PostgreSQL either rejects that
/// database or rejects the PgBouncer-only command. Any refusal, query error,
/// or ambiguous response therefore retains the stricter direct assertions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestTransport {
    Direct,
    TransactionPooler,
}

pub async fn test_transport<T>(endpoint: &str, tls: T) -> TestTransport
where
    T: compio_postgres::tls::MakeTlsConnect<compio_postgres::Socket>,
    T::Stream: compio::io::AsyncRead
        + compio::io::AsyncWrite
        + Unpin
        + compio_postgres::SplitStream
        + 'static,
    <T::Stream as compio_postgres::SplitStream>::ReadHalf: 'static,
{
    use compio_postgres::SimpleQueryMessage;

    let mut config: compio_postgres::Config = endpoint
        .parse()
        .expect("the transport-probe endpoint parses");
    config.dbname("pgbouncer");

    let Ok((client, connection)) = config.connect(tls).await else {
        return TestTransport::Direct;
    };
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let is_transaction_pooler =
        client
            .simple_query("SHOW CONFIG")
            .await
            .ok()
            .is_some_and(|messages| {
                messages.into_iter().any(|message| match message {
                    SimpleQueryMessage::Row(row) => {
                        row.get(0) == Some("pool_mode") && row.get(1) == Some("transaction")
                    }
                    _ => false,
                })
            });

    if is_transaction_pooler {
        TestTransport::TransactionPooler
    } else {
        TestTransport::Direct
    }
}

impl TestTransport {
    /// Assert the backend identity returned by a query separate from the mode
    /// probe, so the pooled inequality is an independently falsifiable claim.
    pub fn assert_backend_pid(self, announced_pid: i32, backend_pid: i32, context: &str) {
        match self {
            Self::Direct => assert_eq!(
                backend_pid, announced_pid,
                "{context}: a direct connection changed physical backend"
            ),
            Self::TransactionPooler => assert_ne!(
                backend_pid, announced_pid,
                "{context}: a transaction pooler exposed its synthetic frontend PID as a \
                 PostgreSQL backend PID"
            ),
        }
    }
}

/// Fail the calling test because the connection this test needs was not made.
///
/// A missing database is a failed run, never a skip: cargo counts a skip as a
/// pass, so there is no environment variable that turns this into one.
///
/// The message answers WHICH backend, WHERE it was dialled (with the password
/// removed - see [`redact_dsn`]), WHAT the failure was ([`error_chain`]) and
/// WHAT TO DO, and the last answer depends on who stopped the attempt. See
/// [`connection_failure_report`].
#[track_caller]
pub fn postgres_unreachable(dsn: &str, error: &(dyn std::error::Error + 'static)) -> ! {
    panic!("{}", connection_failure_report(dsn, error))
}

/// Why a connection attempt failed, and the remedy for THAT failure.
///
/// The causes are told apart by what the error chain carries, because each has
/// a different remedy and prescribing the wrong one sends a developer to fix a
/// server that is fine:
///
/// * `PostgreSQL` replied ([`server_answered`]): the server is up and objected,
///   and its SQLSTATE says to what. Provisioning cannot help.
/// * A bound expired ([`timed_out`]): nothing says whether a server answered,
///   so the report asks for the check that tells a missing server from a
///   loaded machine instead of prescribing either remedy.
/// * Something answered, unintelligibly ([`peer_answered_unintelligibly`]):
///   the address is served, by something the driver cannot talk to or behind
///   TLS material it does not trust. Provisioning cannot help.
/// * Refused as invalid ([`refused_as_invalid`]): a peer sent a message the
///   protocol does not define, or the client side refused an argument; the
///   kind cannot say which, and the report does not guess. Neither is a
///   missing server.
/// * Nothing answered ([`nothing_answered`]): no reply came from the address
///   dialled, so provisioning is the remedy.
/// * None of those: the attempt stopped on the client side - a configuration
///   the driver will not use, a TLS setting its connector cannot honour, an
///   authentication requirement, or a name that does not resolve. The DSN or
///   the code that built the `Config` is what needs fixing.
///
/// The remedies name where the DSN came from and what stands its server up,
/// which differ under `suite-over-tls`: that mode reads the TLS fixture's
/// descriptor instead of `PG_TEST_URL`.
#[allow(
    clippy::too_many_lines,
    reason = "one message per cause, kept side by side so their remedies can be compared"
)]
pub fn connection_failure_report(dsn: &str, error: &(dyn std::error::Error + 'static)) -> String {
    let dialled = redact_dsn(dsn);
    let cause = error_chain(error);

    if server_answered(error) {
        return format!(
            "PostgreSQL refused the connection this test requires.\n\
             \n\
             \x20 backend: PostgreSQL\n\
             \x20 dialled: {dialled}\n\
             \x20 server:  {cause}\n\
             \n\
             The server ANSWERED, so it is running and reachable and this is\n\
             not a provisioning problem. The SQLSTATE above says what it\n\
             objected to. `53300` is the `max_connections` ceiling: something\n\
             in this process is holding connections open across tests - see\n\
             `libs/compio-postgres/src/release.rs` - and raising the ceiling\n\
             would hide that rather than fix it.\n\
             \n\
             There is no environment variable that makes this a skip. A\n\
             database this suite cannot use is a failed run, not a green one."
        );
    }

    if timed_out(error) {
        return format!(
            "The connection this test requires ran out of time.\n\
             \n\
             \x20 backend: PostgreSQL\n\
             \x20 dialled: {dialled}\n\
             \x20 error:   {cause}\n\
             \n\
             A timeout does not say whether anything answered: a server that\n\
             is down behind a port that drops packets, and a server that is up\n\
             but did not finish the handshake inside the bound named above,\n\
             both end here. If {HEALTH_CHECK} shows the server\n\
             up, the server is fine and the bound expired on a machine too\n\
             loaded to meet it.\n\
             \n\
             There is no environment variable that makes this a skip. A database\n\
             this suite cannot reach in time is a failed run, not a green one."
        );
    }

    if peer_answered_unintelligibly(error) {
        return format!(
            "Something answered at the address this test dialled, but not in a\n\
             way the driver accepts.\n\
             \n\
             \x20 backend: PostgreSQL\n\
             \x20 dialled: {dialled}\n\
             \x20 error:   {cause}\n\
             \n\
             A reply arrived, so the address is served and provisioning will not\n\
             help. The driver refused what the reply carried: a frame too large\n\
             or malformed to be PostgreSQL's, an unknown message tag, or a TLS\n\
             handshake that failed on the peer's bytes. Check what listens there\n\
             - another service on this port speaks another protocol - and, for a\n\
             TLS failure, whether the server's certificate is one this checkout\n\
             trusts: libs/compio-postgres/tests/tls_live_setup.sh run from\n\
             another checkout regenerates the CA its servers share.\n\
             \n\
             There is no environment variable that makes this a skip. A database\n\
             this suite cannot use is a failed run, not a green one."
        );
    }

    if refused_as_invalid(error) {
        return format!(
            "The connection this test requires was refused as invalid.\n\
             \n\
             \x20 backend: PostgreSQL\n\
             \x20 dialled: {dialled}\n\
             \x20 error:   {cause}\n\
             \n\
             The error's kind does not say which side was invalid: something at\n\
             the address may have answered with a message the protocol does not\n\
             define, or the client side may have refused an argument of this\n\
             attempt - an over-long Unix socket path, a name that resolved to no\n\
             address, a link-local address without a zone. The error names\n\
             which. Neither is a missing server, so provisioning will not help:\n\
             check what listens at the address, or fix the DSN ({DSN_SOURCE}).\n\
             \n\
             There is no environment variable that makes this a skip. A database\n\
             this suite cannot use is a failed run, not a green one."
        );
    }

    if nothing_answered(error) {
        return format!(
            "PostgreSQL is unreachable, and this test requires it.\n\
             \n\
             \x20 backend: PostgreSQL\n\
             \x20 dialled: {dialled}\n\
             \x20 error:   {cause}\n\
             \n\
             Nothing replied at that address, so provision it and re-run:\n\
             \x20 {PROVISION_COMMAND}\n\
             \n\
             {PROVISION_NOTE}\n\
             \n\
             There is no environment variable that makes this a skip. A database\n\
             this suite cannot reach is a failed run, not a green one."
        );
    }

    format!(
        "The connection this test requires was stopped on the client side.\n\
         \n\
         \x20 backend: PostgreSQL\n\
         \x20 dialled: {dialled}\n\
         \x20 error:   {cause}\n\
         \n\
         Nothing in the error says a server is missing, so provisioning one\n\
         will not help. The error names what stopped the attempt: a\n\
         configuration the driver will not use, a TLS setting its connector\n\
         cannot honour, an authentication requirement the server did not meet,\n\
         or a host name that does not resolve. Fix the DSN ({DSN_SOURCE})\n\
         or the code that built the configuration.\n\
         \n\
         There is no environment variable that makes this a skip. A database\n\
         this suite cannot use is a failed run, not a green one."
    )
}

/// Where the DSN a failing test dialled came from.
#[cfg(not(feature = "suite-over-tls"))]
const DSN_SOURCE: &str =
    "PG_TEST_URL, or `DEFAULT_TEST_URL` in libs/compio-postgres/tests/common/env.rs";
#[cfg(feature = "suite-over-tls")]
const DSN_SOURCE: &str = "libs/compio-postgres/tests/data/live/tls_live.conf, which \
                          suite-over-tls reads in place of PG_TEST_URL";

/// The command that stands the dialled server up.
#[cfg(not(feature = "suite-over-tls"))]
const PROVISION_COMMAND: &str = "tests/provision_test_backends.sh";
#[cfg(feature = "suite-over-tls")]
const PROVISION_COMMAND: &str = "libs/compio-postgres/tests/tls_live_setup.sh";

/// What that command does, and what to know before running it.
#[cfg(not(feature = "suite-over-tls"))]
const PROVISION_NOTE: &str = "That brings up the `postgres` service from\n\
                              deploy/compose/docker-compose.yml and waits for it to be healthy.\n\
                              Point the tests somewhere else with PG_TEST_URL.";
#[cfg(feature = "suite-over-tls")]
const PROVISION_NOTE: &str = "That brings up the TLS servers and writes tls_live.conf, which\n\
                              suite-over-tls reads in place of PG_TEST_URL. Read its header\n\
                              first: it regenerates a CA that other checkouts' TLS fixtures\n\
                              share, so running it from a second checkout breaks the first.";

/// How to tell whether the dialled server is up.
#[cfg(not(feature = "suite-over-tls"))]
const HEALTH_CHECK: &str = "`tests/provision_test_backends.sh --check`";
#[cfg(feature = "suite-over-tls")]
const HEALTH_CHECK: &str = "`docker ps` (the servers tls_live_setup.sh starts)";

/// `PG_TEST_URL`, or `env::DEFAULT_TEST_URL` when it is unset.
///
/// Every test target that honours `PG_TEST_URL` resolves its server through
/// [`test_url`] or [`plaintext_url`], both of which read this, so the
/// coordinates cannot drift apart between targets.
///
/// Absent is NOT "do not run" - see `TestEnvKey::PgTestUrl`. A target that
/// cannot reach this server must fail, not skip.
#[cfg(not(feature = "suite-over-tls"))]
fn plaintext_test_url() -> String {
    env::get(env::TestEnvKey::PgTestUrl).unwrap_or_else(|| env::DEFAULT_TEST_URL.to_string())
}

#[cfg(not(feature = "suite-over-tls"))]
pub fn test_url() -> String {
    suite_test_url(plaintext_test_url())
}

/// Under `--features suite-over-tls` the whole suite runs against the
/// encrypted server instead, and `PG_TEST_URL` is deliberately ignored.
///
/// The point of the mode is to run the EXISTING tests over TLS, so the DSN
/// has to name a server this crate's own setup script configured for both
/// jobs - certificates AND logical decoding / prepared transactions. Honouring
/// `PG_TEST_URL` here would silently run the mode against a plaintext server
/// and report the transports as identical without having tested one of them.
#[cfg(feature = "suite-over-tls")]
pub fn test_url() -> String {
    let descriptor = tls_descriptor();
    let ca = descriptor_field(&descriptor, "ca");
    // URL form, NOT the descriptor's key=value form. Callers append their own
    // parameters (`schema_scoped_url` adds `options=-c search_path=...`) and
    // they choose `?` or `&` by looking for a `?`. A key=value DSN has no `?`,
    // so every one of those appends landed INSIDE the last value: the
    // sslrootcert path became `/path/ca.crt?options=-c%20search_path%3D...`
    // and 20 tests failed with "cannot read PEM: No such file or directory".
    let base = descriptor_field(&descriptor, "tls_url");
    let field = |key: &str| {
        base.split_whitespace()
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("the TLS descriptor's tls_url has no `{key}`"))
            .to_string()
    };
    suite_test_url(format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=verify-full&sslrootcert={ca}",
        field("user"),
        field("password"),
        field("host"),
        field("port"),
        field("dbname"),
    ))
}

#[cfg(not(feature = "suite-with-statement-cache"))]
fn suite_test_url(base: String) -> String {
    base
}

#[cfg(feature = "suite-with-statement-cache")]
fn suite_test_url(base: String) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}statement_cache_capacity=32")
}

/// The transport every suite helper connects over.
///
/// A function rather than a constant because the TLS arm has to build a
/// connector from the very `Config` it will be used with - `connect_raw`
/// refuses a connector that cannot attest the requested `sslmode`.
#[cfg(not(feature = "suite-over-tls"))]
pub fn suite_tls() -> compio_postgres::NoTls {
    compio_postgres::NoTls
}

#[cfg(feature = "suite-over-tls")]
pub fn suite_tls() -> compio_postgres::MakeRustlsConnect {
    static TLS: std::sync::OnceLock<compio_postgres::MakeRustlsConnect> =
        std::sync::OnceLock::new();
    TLS.get_or_init(|| {
        let config: compio_postgres::Config = test_url()
            .parse()
            .expect("the suite-over-tls DSN did not parse");
        compio_postgres::MakeRustlsConnect::from_config(&config)
            .expect("could not build the suite TLS connector")
    })
    .clone()
}

#[cfg(feature = "suite-over-tls")]
fn tls_descriptor() -> String {
    const DESCRIPTOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/live/tls_live.conf");
    std::fs::read_to_string(DESCRIPTOR).unwrap_or_else(|error| {
        panic!(
            "suite-over-tls needs the TLS servers. Run \
             libs/compio-postgres/tests/tls_live_setup.sh first ({DESCRIPTOR}: {error})"
        )
    })
}

#[cfg(feature = "suite-over-tls")]
fn descriptor_field(descriptor: &str, key: &str) -> String {
    descriptor
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("the TLS descriptor has no `{key}` line"))
        .to_string()
}

/// Drop replication slots left behind by test processes that are gone.
///
/// A test that panics or trips its watchdog never reaches its own cleanup, and
/// a logical slot outlives the connection that made it: it stays, and it PINS
/// WAL until something drops it. Six had accumulated by 2026-08-24 and took
/// the server to 9 of its 20 slots; the first symptom was an unrelated probe
/// failing with `max_replication_slots` exhausted, which reads as a server
/// misconfiguration rather than as test litter.
///
/// The sweep is keyed on the PID that [`test_object_name`] embeds, and drops
/// only slots whose process is no longer running. A slot belonging to a LIVE
/// process is left alone, so this is safe to call while other test binaries
/// are running concurrently - which is exactly when a blunter rule (drop every
/// inactive slot, drop by age) would delete a slot a running test is about to
/// use. `active` is not enough on its own: a slot sits inactive between its
/// creation and the START_REPLICATION that attaches to it.
pub async fn sweep_stale_replication_slots(client: &compio_postgres::Client) {
    let Ok(rows) = client
        .query(
            "SELECT slot_name FROM pg_replication_slots
              WHERE NOT active AND slot_type = 'logical'",
            &[],
        )
        .await
    else {
        return;
    };

    for row in rows {
        let name: String = row.get(0);
        let Some(pid) = pid_embedded_in(&name) else {
            continue;
        };
        if process_is_alive(pid) {
            continue;
        }
        // Best effort: another sweep may have taken it first, and losing that
        // race is the correct outcome, not an error.
        let _ = drop_replication_slot(client, &name).await;
    }
}

/// Drop publications and tables left behind by test processes that are gone.
///
/// Same rule and same reason as [`sweep_stale_replication_slots`], applied to
/// the other objects these suites create. These are not a bounded resource
/// the way slots are, so a leak does not break the next run - it accumulates.
/// Measured 2026-08-24: 58 tables and 55 publications had built up from runs
/// that died before their cleanup, which is slow to notice and tedious to
/// clear by hand.
///
/// Publications first: one can depend on a table, and dropping the table out
/// from under it fails.
pub async fn sweep_stale_test_objects(client: &compio_postgres::Client) {
    sweep_stale_replication_slots(client).await;

    if let Ok(rows) = client
        .query(
            "SELECT pubname FROM pg_publication WHERE pubname LIKE '%\\_%'",
            &[],
        )
        .await
    {
        for row in rows {
            let name: String = row.get(0);
            if pid_embedded_in(&name).is_some_and(|pid| !process_is_alive(pid)) {
                let _ = client
                    .execute(&format!("DROP PUBLICATION IF EXISTS \"{name}\""), &[])
                    .await;
            }
        }
    }

    if let Ok(rows) = client
        .query(
            "SELECT tablename FROM pg_tables
              WHERE schemaname = 'public' AND tablename LIKE '%\\_%'",
            &[],
        )
        .await
    {
        for row in rows {
            let name: String = row.get(0);
            if pid_embedded_in(&name).is_some_and(|pid| !process_is_alive(pid)) {
                let _ = client
                    .execute(&format!("DROP TABLE IF EXISTS \"{name}\" CASCADE"), &[])
                    .await;
            }
        }
    }
}

/// The PID [`test_object_name`] put in the middle of `<readable>_<pid>_<hash>`.
///
/// Returns `None` for any name that is not that shape, so a slot this suite
/// did not create is never a candidate.
fn pid_embedded_in(object_name: &str) -> Option<u32> {
    // `test_object_name` builds `<readable>_<pid>_<hash>` with the hash a
    // fixed 16 hex digits, and callers append their own suffix - `_s`, `_t`,
    // `_ours`, or nothing at all. Anchoring on the HASH rather than counting
    // from the end therefore works whatever the suffix is; counting from the
    // end only worked for the one-component case and silently skipped the
    // rest, which is how a sweep can look busy while missing most of its
    // targets.
    let parts: Vec<&str> = object_name.split('_').collect();
    let hash_at = parts
        .iter()
        .position(|part| part.len() == 16 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
    parts.get(hash_at.checked_sub(1)?)?.parse::<u32>().ok()
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Elsewhere, never claim a process is dead - leaving a slot is recoverable,
/// dropping a live test's slot is not.
#[cfg(not(target_os = "linux"))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

/// Drop a replication slot once its walsender has actually let go.
///
/// Dropping the stream closes the connection CLIENT-side; the server takes a
/// moment longer to retire the walsender, and until it does the slot is still
/// `active` and `pg_drop_replication_slot` fails with 55006. That window is
/// invisible when the test runs alone and opens up under full-suite load,
/// which is exactly the shape that produces a flake nobody can reproduce.
///
/// Polls the server rather than sleeping a fixed amount: a sleep long enough
/// to be safe on a loaded machine is wasted on every green run, and one tuned
/// on an idle machine is the flake again. A slot that is already gone is
/// success, not an error.
pub async fn drop_replication_slot(
    client: &compio_postgres::Client,
    slot: &str,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let outcome = client
            .execute(
                "SELECT pg_drop_replication_slot(s.slot_name)
                   FROM pg_replication_slots s WHERE s.slot_name = $1",
                &[&slot],
            )
            .await;
        let Err(error) = outcome else {
            return Ok(());
        };
        let still_held = error.code().is_some_and(|code| code.code() == "55006");
        if !still_held || std::time::Instant::now() >= deadline {
            return Err(error_chain(&error));
        }
        compio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// A `Config` for a replication connection to the test server.
///
/// Carries the test DSN's credentials, host and port, and nothing else - a
/// replication connection cannot be opened by parsing the DSN alone, because
/// `connect_replication` needs a `Config` rather than a URL.
///
/// Here for the same reason [`test_url`] is: three test binaries had grown
/// their own copy, and a fourth was about to.
pub fn replication_config(application_name: &str) -> compio_postgres::Config {
    use compio_postgres::Config;
    use compio_postgres::config::Host;

    let url = test_url();
    let parsed: Config = url.parse().expect("test DSN did not parse");
    let mut config = Config::new();
    if let Some(user) = parsed.get_user() {
        config.user(user);
    }
    if let Some(password) = parsed.get_password() {
        config.password(password);
    }
    if let Some(dbname) = parsed.get_dbname() {
        config.dbname(dbname);
    }
    for host in parsed.get_hosts() {
        match host {
            Host::Tcp(name) => {
                config.host(name.clone());
            }
            #[cfg(unix)]
            Host::Unix(path) => panic!(
                "the replication tests need a TCP endpoint, got the socket {}",
                path.display()
            ),
        }
    }
    for port in parsed.get_ports() {
        config.port(*port);
    }
    // The TLS settings are part of the endpoint, not decoration. This rebuilt
    // config used to drop them, so under `suite-over-tls` every replication
    // test asked for the default `sslmode=prefer` while `suite_tls()` handed
    // it a connector attesting `verify-full` - and `connect_raw` refused the
    // pair with `TlsUnattested` before a socket was opened. Eleven tests
    // failed that way, none of them for a reason that had anything to do with
    // replication.
    config.ssl_mode(parsed.get_ssl_mode());
    config.ssl_root_cert(parsed.get_ssl_root_cert().clone());
    config.ssl_cert_mode(parsed.get_ssl_cert_mode());
    if let Some(cert) = parsed.get_ssl_cert() {
        config.ssl_cert(cert);
    }
    config.application_name(application_name);
    config
}

/// A DSN for a client that cannot speak TLS.
///
/// The differential suite runs `tokio-postgres` beside this crate as an
/// oracle, and the oracle stays on PLAINTEXT even when this crate is built
/// with `suite-over-tls`. It also stays free of the compio-postgres-only
/// `statement_cache_capacity` parameter under `suite-with-statement-cache`.
/// That is the comparison worth making: the reference should differ from the
/// subject only in the property the suite mode is exercising.
///
/// Without this the oracle would inherit `sslmode=verify-full` from
/// [`test_url`] and fail to connect at all.
#[cfg(feature = "suite-over-tls")]
pub fn plaintext_url() -> String {
    descriptor_field(&tls_descriptor(), "tls_url")
}

#[cfg(not(feature = "suite-over-tls"))]
pub fn plaintext_url() -> String {
    plaintext_test_url()
}

/// Replace the password in a `postgres://user:pass@host/db` DSN.
///
/// Returns `None` when the DSN carries no password to replace, which is the
/// whole reason this exists. `wrong_password` used to build its bad DSN with
/// `url.replace(":zeroship@", ":wrong_password_xyz@")` - a literal that is
/// correct for the default plaintext DSN and matches NOTHING otherwise. Under
/// `--features suite-over-tls` the password is different, so the replacement
/// silently did nothing and the test connected with the RIGHT password and
/// then failed at "expected connection to fail". A test that can degrade into
/// asserting the opposite of its name should not depend on a string literal.
///
/// The userinfo is located exactly as [`redact_dsn`] locates it: the scan for
/// the `@` runs to the end of the authority, so a password containing `@`
/// cannot cut it short.
pub fn with_password(dsn: &str, password: &str) -> Option<String> {
    let scheme_end = dsn.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = dsn[authority_start..]
        .find(['/', '?'])
        .map_or(dsn.len(), |offset| authority_start + offset);
    let authority = &dsn[authority_start..authority_end];

    let at = authority.rfind('@')?;
    let userinfo = &authority[..at];
    let colon = userinfo.find(':')?;

    Some(format!(
        "{}{}:{}{}",
        &dsn[..authority_start],
        &userinfo[..colon],
        password,
        &dsn[authority_start + at..]
    ))
}

/// A DSN for a server with TLS switched OFF entirely.
///
/// Distinct from [`plaintext_url`], which names the same encrypted server
/// without asking for encryption - fine for an unencrypted client, useless to
/// a test whose subject is what happens when the server answers `N` to
/// `SSLRequest`. Under `--features suite-over-tls` that is the descriptor's
/// `plain_url` (the setup script's `plain` server, `ssl` off), and outside it
/// the ordinary test server, which has no TLS either.
///
/// `sslmode_require_fails_closed_over_a_plaintext_server` needs this or its
/// name stops being true: pointed at a TLS-capable server, `sslmode=require`
/// is SATISFIED and the test measures nothing.
#[cfg(feature = "suite-over-tls")]
pub fn tls_disabled_url() -> String {
    let base = descriptor_field(&tls_descriptor(), "plain_url");
    let field = |key: &str| {
        base.split_whitespace()
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("the TLS descriptor's plain_url has no `{key}`"))
            .to_string()
    };
    format!(
        "postgres://{}:{}@{}:{}/{}",
        field("user"),
        field("password"),
        field("host"),
        field("port"),
        field("dbname"),
    )
}

#[cfg(not(feature = "suite-over-tls"))]
pub fn tls_disabled_url() -> String {
    test_url()
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_POSTGRES_IDENTIFIER_LEN, pid_embedded_in, redact_dsn, test_object_name, with_password,
    };

    #[test]
    fn a_slot_named_by_this_suite_yields_the_pid_that_made_it() {
        // Exactly the shape the fixtures build: test_object_name() plus a
        // one-character suffix.
        let name = format!("{}_s", test_object_name("cpg stream off"));
        let pid = pid_embedded_in(&name).expect("the sweep must find the pid it embedded");
        assert_eq!(
            pid,
            std::process::id(),
            "the pid parsed back must be the one test_object_name wrote: {name}"
        );
    }

    #[test]
    fn a_slot_this_suite_did_not_create_is_never_a_candidate() {
        // The sweep DROPS what it matches, so failing to parse must mean
        // "leave it alone", not "guess". A production slot caught by a loose
        // rule is deleted WAL retention, and nothing announces it.
        for foreign in [
            "my_app_slot",
            "debezium",
            "",
            "_",
            "cpg_missing_hash",
            "slot_with_no_digits_here_x",
        ] {
            assert_eq!(
                pid_embedded_in(foreign),
                None,
                "{foreign:?} is not this suite's shape and must not be swept"
            );
        }
    }

    /// The suffix varies by caller, and an earlier parser counted components
    /// from the END, so it found the pid only for a one-component suffix and
    /// silently returned None for every other shape - including a bare name
    /// with no suffix at all. A sweep built on that looks busy while missing
    /// most of what it is meant to collect.
    #[test]
    fn the_pid_is_found_whatever_suffix_the_caller_appended() {
        let base = test_object_name("cpg shapes");
        for name in [
            base.clone(),
            format!("{base}_s"),
            format!("{base}_t"),
            format!("{base}_ours"),
            format!("{base}_theirs"),
        ] {
            assert_eq!(
                pid_embedded_in(&name),
                Some(std::process::id()),
                "the pid was not found in {name}"
            );
        }
    }

    #[test]
    fn a_pid_that_is_not_a_number_is_refused_rather_than_coerced() {
        assert_eq!(
            pid_embedded_in("cpg_thing_notapid_abcdef0123456789_s"),
            None
        );
        // Negative and overflowing values are not pids either.
        assert_eq!(pid_embedded_in("cpg_thing_-1_abcdef0123456789_s"), None);
        assert_eq!(
            pid_embedded_in("cpg_thing_99999999999999999999_abcdef_s"),
            None
        );
    }

    #[test]
    fn test_object_names_are_safe_bounded_and_process_scoped() {
        let name = test_object_name("9-MiXeD/fixture");
        let pid_marker = format!("_{}_", std::process::id());

        assert!(name.len() <= MAX_POSTGRES_IDENTIFIER_LEN);
        assert!(name.starts_with('_'));
        assert!(name.contains(&pid_marker));
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        );
    }

    #[test]
    fn test_object_name_hash_keeps_truncated_or_normalised_names_distinct() {
        let common_prefix = "a".repeat(200);
        let first = test_object_name(&format!("{common_prefix}first"));
        let second = test_object_name(&format!("{common_prefix}second"));
        let punctuation = test_object_name("fixture-name");
        let underscore = test_object_name("fixture_name");
        let pid_marker = format!("_{}_", std::process::id());

        assert_eq!(first.len(), MAX_POSTGRES_IDENTIFIER_LEN);
        assert_eq!(second.len(), MAX_POSTGRES_IDENTIFIER_LEN);
        assert!(first.contains(&pid_marker));
        assert!(second.contains(&pid_marker));
        assert_ne!(first, second);
        assert_ne!(punctuation, underscore);
        assert_eq!(first, test_object_name(&format!("{common_prefix}first")));
    }

    #[test]
    fn redaction_removes_the_password_and_keeps_everything_else() {
        assert_eq!(
            redact_dsn("postgres://postgres:zeroship@localhost:5440/zeroship"),
            "postgres://postgres:***@localhost:5440/zeroship"
        );
    }

    /// The one-variable partner: a DSN with no password must come back
    /// unchanged, or the "redacted" claim is really "mangled".
    #[test]
    fn redaction_leaves_a_dsn_without_a_password_alone() {
        assert_eq!(
            redact_dsn("postgres://localhost:5440/zeroship"),
            "postgres://localhost:5440/zeroship"
        );
        assert_eq!(
            redact_dsn("postgres://postgres@localhost:5440/zeroship"),
            "postgres://postgres@localhost:5440/zeroship"
        );
    }

    /// A password containing `@` must not push the split point left and leak
    /// its tail. Taking the FIRST `@` would print `pa***ss@word@localhost`.
    #[test]
    fn redaction_handles_an_at_sign_inside_the_password() {
        assert_eq!(
            redact_dsn("postgres://user:p@ss@localhost:5440/db"),
            "postgres://user:***@localhost:5440/db"
        );
    }

    /// A `/` or `?` in the path must not be mistaken for the authority's end
    /// marker AFTER the authority - and a query string with an `@` in it must
    /// not be treated as userinfo.
    #[test]
    fn redaction_stops_at_the_end_of_the_authority() {
        assert_eq!(
            redact_dsn("postgres://u:p@host/db?options=-c%20search_path%3Da@b"),
            "postgres://u:***@host/db?options=-c%20search_path%3Da@b"
        );
    }

    /// The shape that matters: the suite's TLS DSN carries a query string, and
    /// the replacement must not disturb it.
    #[test]
    fn a_password_is_replaced_without_touching_the_query_string() {
        assert_eq!(
            with_password(
                "postgres://postgres:secret@localhost:5447/postgres?sslmode=verify-full&sslrootcert=/tmp/ca.crt",
                "wrong"
            )
            .as_deref(),
            Some(
                "postgres://postgres:wrong@localhost:5447/postgres?sslmode=verify-full&sslrootcert=/tmp/ca.crt"
            )
        );
    }

    /// Same rule as `redact_dsn`: the scan for the separator runs to the end of
    /// the authority, so an `@` inside the password cannot cut it short.
    #[test]
    fn a_password_containing_an_at_sign_is_replaced_whole() {
        assert_eq!(
            with_password("postgres://user:p@ss@localhost:5440/db", "wrong").as_deref(),
            Some("postgres://user:wrong@localhost:5440/db")
        );
    }

    /// `None`, not a silently unchanged DSN. This is the whole point: the
    /// caller asked to make a password wrong, and a DSN with no password
    /// cannot honour that. Returning the input would let `wrong_password`
    /// connect with valid credentials and then fail claiming the server had
    /// accepted a bad one.
    #[test]
    fn a_dsn_without_a_password_is_refused_rather_than_returned_unchanged() {
        assert_eq!(with_password("postgres://user@localhost/db", "wrong"), None);
        assert_eq!(
            with_password("host=localhost port=5440 user=postgres", "wrong"),
            None
        );
    }

    /// The causes [`super::connection_failure_report`] tells apart, each
    /// produced by a real driver call rather than by a hand-built error, so
    /// the classification is measured on the chains the drivers return.
    ///
    /// Each case that must NOT prescribe provisioning sits beside one that
    /// must, so a classifier that answers the same for everything fails here.
    mod connection_failure_report {
        use super::super::{
            PROVISION_COMMAND, connection_failure_report, error_chain, server_answered,
        };
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        /// Bounds every blocking call a scripted peer makes, so a client that
        /// never arrives ends the peer instead of leaking its thread.
        const PEER_WATCHDOG: Duration = Duration::from_secs(10);

        const UNREACHABLE: &str = "PostgreSQL is unreachable";
        const UNINTELLIGIBLE: &str = "Something answered at the address this test dialled";
        const INVALID: &str = "The connection this test requires was refused as invalid";
        const CLIENT_SIDE: &str =
            "The connection this test requires was stopped on the client side";

        /// A loopback port whose connections are refused, held for as long as
        /// the returned socket lives.
        ///
        /// Bound but never listening: the port stays ours, so nothing can take
        /// it between choosing it and dialling it, and a dial is answered with
        /// a reset rather than accepted.
        fn refusing_port() -> (socket2::Socket, u16) {
            let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                .expect("create the refusing socket");
            socket
                .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
                .expect("bind the refusing socket");
            let port = socket
                .local_addr()
                .expect("read the refusing socket's address")
                .as_socket()
                .expect("the refusing socket has an IP address")
                .port();
            (socket, port)
        }

        /// A peer that accepts ONE connection and runs `script` on it.
        ///
        /// Every blocking call, the accept included, is bounded by
        /// [`PEER_WATCHDOG`]. The script owns the stream: dropping it hangs up,
        /// and [`hold`] keeps it open until the client leaves.
        fn scripted_peer(
            script: impl FnOnce(TcpStream) -> std::io::Result<()> + Send + 'static,
        ) -> (u16, std::thread::JoinHandle<std::io::Result<()>>) {
            let listener = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                .expect("create the scripted peer");
            listener
                .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
                .expect("bind the scripted peer");
            listener.listen(1).expect("listen on the scripted peer");
            // SO_RCVTIMEO bounds `accept` on Linux as well as `read`.
            listener
                .set_read_timeout(Some(PEER_WATCHDOG))
                .expect("bound the scripted peer's accept");
            let listener: std::net::TcpListener = listener.into();
            let port = listener.local_addr().expect("scripted peer address").port();
            let peer = std::thread::spawn(move || {
                let (stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(PEER_WATCHDOG))?;
                stream.set_write_timeout(Some(PEER_WATCHDOG))?;
                script(stream)
            });
            (port, peer)
        }

        /// Read one length-prefixed startup-phase packet and return its body.
        fn read_packet(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
            let mut length = [0u8; 4];
            stream.read_exact(&mut length)?;
            let length = u32::from_be_bytes(length) as usize;
            if !(8..=10_000).contains(&length) {
                return Err(std::io::Error::other(format!(
                    "implausible startup length {length}"
                )));
            }
            let mut body = vec![0u8; length - 4];
            stream.read_exact(&mut body)?;
            Ok(body)
        }

        /// Read the client's startup packet, declining `SSLRequest` and
        /// `GSSENCRequest` on the way, as a plaintext `PostgreSQL` does.
        fn read_startup(stream: &mut TcpStream) -> std::io::Result<()> {
            loop {
                let body = read_packet(stream)?;
                let code = u32::from_be_bytes(body[..4].try_into().unwrap());
                if code == 80_877_103 || code == 80_877_104 {
                    stream.write_all(b"N")?;
                    continue;
                }
                return Ok(());
            }
        }

        /// Keep the connection open until the client leaves. A reset is the
        /// client leaving with bytes it never read, not a peer failure.
        fn hold(mut stream: TcpStream) -> std::io::Result<()> {
            let mut scratch = [0u8; 256];
            loop {
                match stream.read(&mut scratch) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }
        }

        #[allow(clippy::future_not_send)]
        async fn dial(dsn: &str) -> compio_postgres::Error {
            match compio_postgres::connect(dsn, compio_postgres::NoTls).await {
                Ok(_) => panic!("{dsn} completed a login this test scripted to fail"),
                Err(error) => error,
            }
        }

        /// The words that prescribe provisioning. The timeout report names the
        /// provisioning script too, as the way to CHECK the server, so the
        /// command alone does not say whether provisioning was prescribed.
        const PRESCRIPTION: &str = "provision it and re-run";

        fn assert_classified(report: &str, heading: &str, provisions: bool) {
            assert!(
                report.starts_with(heading),
                "expected a report beginning {heading:?}, got:\n{report}"
            );
            assert_eq!(
                report.contains(PRESCRIPTION),
                provisions,
                "provisioning was {} this report:\n{report}",
                if provisions {
                    "not prescribed by"
                } else {
                    "prescribed by"
                }
            );
            if provisions {
                assert!(
                    report.contains(PROVISION_COMMAND),
                    "the report prescribed provisioning without naming {PROVISION_COMMAND}:\n{report}"
                );
            }
        }

        /// A dial nothing accepts: provisioning is the remedy.
        #[compio::test]
        async fn a_dial_nothing_answers_prescribes_provisioning() {
            let (_held, port) = refusing_port();
            let dsn = format!("postgres://u:p@127.0.0.1:{port}/d?sslmode=disable");
            let error = dial(&dsn).await;

            let report = connection_failure_report(&dsn, &error);
            assert_classified(&report, UNREACHABLE, true);
            assert!(
                !report.contains(":p@"),
                "the report leaked the password:\n{report}"
            );
        }

        /// A peer that hangs up before sending a byte produced no reply
        /// either, so it is reported the same way as a refused dial.
        #[compio::test]
        async fn a_peer_that_hangs_up_before_replying_prescribes_provisioning() {
            let (port, peer) = scripted_peer(|mut stream| read_startup(&mut stream));
            let dsn = format!("postgres://u@127.0.0.1:{port}/d?sslmode=disable");
            let error = dial(&dsn).await;
            peer.join()
                .expect("the scripted peer panicked")
                .expect("the scripted peer failed");

            assert_classified(&connection_failure_report(&dsn, &error), UNREACHABLE, true);
        }

        /// Another protocol on the port. An HTTP response's first five bytes
        /// read as a `PostgreSQL` frame header too large for any message, and
        /// the driver files that under its communication kind, beside a reset
        /// socket - which is why the report cannot go by driver kind.
        #[compio::test]
        async fn a_peer_speaking_another_protocol_is_not_reported_missing() {
            let (port, peer) = scripted_peer(|mut stream| {
                read_startup(&mut stream)?;
                stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")?;
                hold(stream)
            });
            let dsn = format!("postgres://u@127.0.0.1:{port}/d?sslmode=disable");
            let error = dial(&dsn).await;
            peer.join()
                .expect("the scripted peer panicked")
                .expect("the scripted peer failed");

            assert_classified(
                &connection_failure_report(&dsn, &error),
                UNINTELLIGIBLE,
                false,
            );
        }

        /// A well-framed message with a tag the protocol does not define. A
        /// peer answered, but `postgres-protocol` reports the tag as
        /// `InvalidInput`, the kind std also uses for a refused argument, so
        /// the report may say neither "answered" nor "missing".
        #[compio::test]
        async fn an_unknown_message_tag_is_not_reported_missing() {
            let (port, peer) = scripted_peer(|mut stream| {
                read_startup(&mut stream)?;
                stream.write_all(b"X\x00\x00\x00\x08abcd")?;
                hold(stream)
            });
            let dsn = format!("postgres://u@127.0.0.1:{port}/d?sslmode=disable");
            let error = dial(&dsn).await;
            peer.join()
                .expect("the scripted peer panicked")
                .expect("the scripted peer failed");

            assert_classified(&connection_failure_report(&dsn, &error), INVALID, false);
        }

        /// An argument the operating system refuses before anything is dialled:
        /// a Unix socket path past `sun_path`. The same kind as the unknown tag
        /// above, and this time nothing answered - so a report that claimed a
        /// peer answered, or prescribed provisioning, would be wrong here.
        #[cfg(unix)]
        #[compio::test]
        async fn an_argument_the_client_side_refuses_is_not_reported_as_an_answer() {
            let dir = format!("/tmp/{}", "cpg_overlong_socket_dir_segment/".repeat(8));
            let dsn = format!("host={dir} user=u dbname=d");
            let error = dial(&dsn).await;

            let report = connection_failure_report(&dsn, &error);
            assert_classified(&report, INVALID, false);
            assert!(
                !report.contains("Something answered"),
                "a refusal on the client side was reported as a peer's answer:\n{report}"
            );
        }

        /// A peer that agrees to TLS and then does not speak it: the shape of
        /// a TLS server whose certificate or protocol the client rejects.
        #[cfg(feature = "tls")]
        #[compio::test]
        async fn a_peer_that_accepts_tls_then_sends_garbage_is_not_reported_missing() {
            let (port, peer) = scripted_peer(|mut stream| {
                let request = read_packet(&mut stream)?;
                if request[..4] != 80_877_103u32.to_be_bytes() {
                    return Err(std::io::Error::other("the client did not ask for TLS"));
                }
                stream.write_all(b"S")?;
                stream.write_all(b"this is not a TLS record, it is plain text\r\n")?;
                hold(stream)
            });
            let dsn = format!("postgres://u@127.0.0.1:{port}/d?sslmode=require");
            let config: compio_postgres::Config = dsn.parse().expect("parse the TLS DSN");
            let tls = compio_postgres::MakeRustlsConnect::from_config(&config)
                .expect("build the TLS connector");
            let Err(error) = config.connect(tls).await else {
                panic!("a peer that sent plain text completed a TLS login");
            };
            peer.join()
                .expect("the scripted peer panicked")
                .expect("the scripted peer failed");

            assert_classified(
                &connection_failure_report(&dsn, &error),
                UNINTELLIGIBLE,
                false,
            );
        }

        /// A real refusal by the driver after it reached a peer: the DSN
        /// demands TLS and the connector cannot provide it.
        #[compio::test]
        async fn a_refusal_by_the_driver_itself_prescribes_no_provisioning() {
            let (port, peer) = scripted_peer(hold);
            let dsn = format!("postgres://u@127.0.0.1:{port}/d?sslmode=require");
            let error = dial(&dsn).await;
            peer.join()
                .expect("the scripted peer panicked")
                .expect("the scripted peer failed");

            let report = connection_failure_report(&dsn, &error);
            assert_classified(&report, CLIENT_SIDE, false);
            assert!(
                report.contains(&error_chain(&error)),
                "the report dropped the error that names the refusal:\n{report}"
            );
        }

        /// An expired bound is reported as a timeout, not as a missing server.
        ///
        /// The peer is a listener that never accepts: the kernel completes the
        /// TCP handshake into its backlog and nothing ever replies, so the pool
        /// warm-up bound expires every time. That is also the shape a loaded
        /// machine produces against a healthy server - a pool bound expiring
        /// mid-handshake - so "unreachable", with the provisioning remedy,
        /// would be the wrong diagnosis for it.
        #[compio::test]
        async fn an_expired_bound_is_reported_as_a_timeout_not_a_missing_server() {
            let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a silent peer");
            let dsn = format!(
                "postgres://u:p@127.0.0.1:{}/d?sslmode=disable",
                silent.local_addr().expect("silent peer address").port()
            );
            let config: compio_postgres::Config = dsn.parse().expect("parse the silent-peer DSN");
            let mut pool_config = compio_postgres::PoolConfig::new();
            pool_config
                .max_size(1)
                .min_idle(0)
                .warm_up_timeout(Duration::from_millis(100));
            let Err(error) = compio_postgres::Pool::connect_with_config(config, pool_config).await
            else {
                panic!("a peer that never replies completed a pool warm-up");
            };
            assert!(error.is_pool_timeout(), "{}", error_chain(&error));

            let report = connection_failure_report(&dsn, &error);
            assert_classified(
                &report,
                "The connection this test requires ran out of time",
                false,
            );
            assert!(
                report.contains(&error_chain(&error)),
                "the report dropped the bound that expired:\n{report}"
            );
            drop(silent);
        }

        /// A `PostgreSQL` refusal is a server answer from either driver. The
        /// `tokio-postgres` oracle's connect failures reach the same report,
        /// and its `DbError` is a different type from this driver's.
        #[test]
        fn a_server_refusal_is_recognised_from_either_driver() {
            fn refusal(mut stream: TcpStream) -> std::io::Result<()> {
                read_startup(&mut stream)?;
                let mut fields = Vec::new();
                for (tag, value) in [
                    (b'S', "FATAL"),
                    (b'V', "FATAL"),
                    (b'C', "28P01"),
                    (b'M', "password authentication failed for user \"u\""),
                ] {
                    fields.push(tag);
                    fields.extend_from_slice(value.as_bytes());
                    fields.push(0);
                }
                fields.push(0);
                let mut frame = vec![b'E'];
                frame.extend_from_slice(&u32::try_from(fields.len() + 4).unwrap().to_be_bytes());
                frame.extend_from_slice(&fields);
                stream.write_all(&frame)
            }

            let (port, peer) = scripted_peer(refusal);
            let dsn = format!("postgres://u:p@127.0.0.1:{port}/d?sslmode=disable");
            let ours = compio::runtime::Runtime::new()
                .expect("build a compio runtime")
                .block_on(compio_postgres::connect(&dsn, compio_postgres::NoTls))
                .map(|_| ())
                .expect_err("the refusing peer accepted this driver's login");
            peer.join()
                .expect("the refusing peer panicked")
                .expect("the refusing peer failed");

            let (port, peer) = scripted_peer(refusal);
            let dsn = format!("postgres://u:p@127.0.0.1:{port}/d?sslmode=disable");
            let theirs = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build a tokio runtime")
                .block_on(tokio_postgres::connect(&dsn, tokio_postgres::NoTls))
                .map(|_| ())
                .expect_err("the refusing peer accepted the oracle's login");
            peer.join()
                .expect("the refusing peer panicked")
                .expect("the refusing peer failed");

            for (driver, error) in [
                (
                    "compio-postgres",
                    &ours as &(dyn std::error::Error + 'static),
                ),
                ("tokio-postgres", &theirs),
            ] {
                assert!(
                    server_answered(error),
                    "{driver}: a FATAL ErrorResponse was not recognised as a server answer: {}",
                    error_chain(error)
                );
                assert_classified(
                    &connection_failure_report(&dsn, error),
                    "PostgreSQL refused",
                    false,
                );
            }
            assert!(
                error_chain(&ours).contains("SQLSTATE 28P01"),
                "the refusal's SQLSTATE was not rendered: {}",
                error_chain(&ours)
            );
        }
    }
}
