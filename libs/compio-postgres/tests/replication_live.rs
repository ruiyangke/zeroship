//! Live replication-protocol tests against a real walsender.
//!
//! What these need is `max_wal_senders > 0`, and a spare slot in it. NOT
//! `wal_level=logical`: `IDENTIFY_SYSTEM` answers on any `replication=database`
//! connection, and only `CREATE_REPLICATION_SLOT ... LOGICAL` and
//! `START_REPLICATION ... LOGICAL` need the logical level. `wal_level=minimal`
//! is the case that breaks them, because it forces `max_wal_senders` to 0.
//! The `replication=database` startup parameter puts the backend in walsender
//! mode, where the regular query grammar is gone and only the replication
//! commands answer.
//!
//! The crate had NO live coverage of `src/replication.rs` before this file.
//! Its unit tests build `IDENTIFY_SYSTEM` row bodies by hand, and that is
//! exactly how the defect below survived: the hand-built shape is not the one
//! `DataRowBody::buffer()` produces, so parser and fixture agreed on a row
//! layout the server never sends.

use compio_postgres::{Config, NoTls};

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

/// The credentials and database from the test DSN, with no host or port.
///
/// The multi-host tests below need to place the live endpoint at a chosen
/// position in a list, which a DSN's single host cannot express.
fn credentials_only(url: &str) -> Config {
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
    config
}

/// The host and port the test DSN names.
fn live_endpoint(url: &str) -> (String, u16) {
    let parsed: Config = url.parse().expect("test DSN did not parse");
    let host = match parsed.get_hosts().first().expect("test DSN names no host") {
        compio_postgres::config::Host::Tcp(host) => host.clone(),
        #[cfg(unix)]
        compio_postgres::config::Host::Unix(path) => {
            panic!("this test needs a TCP endpoint, got the socket {}", path.display())
        }
    };
    let port = parsed.get_ports().first().copied().unwrap_or(5432);
    (host, port)
}

/// A TCP port on loopback with nothing listening on it.
///
/// Bound and released rather than picked from thin air: the kernel hands out
/// a port it knows is free, and it does not hand the same one out again while
/// this test runs. A port nobody listens on REFUSES, which is what makes the
/// first attempt below fail fast instead of hanging on a backlog.
async fn closed_port() -> u16 {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().expect("listener address").port();
    drop(listener);
    port
}

/// A replication connection must try every host in the configuration.
///
/// `connect_replication` took `get_hosts().first()` and stopped there, so a
/// two-endpoint configuration had no failover at all: if the first host was
/// down, the call failed while a healthy second host sat unused. The same
/// `first()` was applied to DNS resolution, which is the more common way to
/// meet this - a name that resolves to both an AAAA and an A record against a
/// server bound only to IPv4 fails on the first address every time.
#[compio::test]
async fn replication_connect_tries_every_configured_host() {
    let url = test_url();
    let (live_host, live_port) = live_endpoint(&url);
    let dead_port = closed_port().await;

    let mut config = credentials_only(&url);
    config.host("127.0.0.1");
    config.port(dead_port);
    config.host(live_host);
    config.port(live_port);
    config.application_name("cpg_replication_failover");

    let mut replication =
        match compio_postgres::replication::connect_replication(NoTls, &config).await {
            Ok(connection) => connection,
            Err(e) if common::server_answered(&e) => panic!(
                "the server refused a replication connection: {}",
                common::error_chain(&e)
            ),
            Err(e) => panic!(
                "a live host listed after a dead one was never tried: {}",
                common::error_chain(&e)
            ),
        };

    let identity = replication
        .identify_system()
        .await
        .expect("IDENTIFY_SYSTEM failed on the host that was reached");
    assert!(
        identity.xlogpos.contains('/'),
        "xlogpos {:?} is not an LSN",
        identity.xlogpos
    );
}

/// The control for the host walk: one port covers every host.
///
/// libpq broadcasts a single `port` across all hosts and only requires a
/// one-per-host list when more than one is given. A host walk that demands
/// `ports.len() == hosts.len()`, or that indexes the port list by host
/// position without falling back to the first entry, refuses this
/// configuration - which is a valid one.
#[compio::test]
async fn replication_connect_broadcasts_a_single_port_across_hosts() {
    let url = test_url();
    let (live_host, live_port) = live_endpoint(&url);

    let mut config = credentials_only(&url);
    config.host(live_host.clone());
    config.host(live_host);
    config.port(live_port);
    config.application_name("cpg_replication_one_port");

    let mut replication =
        match compio_postgres::replication::connect_replication(NoTls, &config).await {
            Ok(connection) => connection,
            Err(e) if common::server_answered(&e) => panic!(
                "the server refused a replication connection: {}",
                common::error_chain(&e)
            ),
            Err(e) => panic!(
                "two hosts sharing one port must connect: {}",
                common::error_chain(&e)
            ),
        };

    replication
        .identify_system()
        .await
        .expect("IDENTIFY_SYSTEM failed");
}

/// The control for the failure path: when no host answers, the error is the
/// last endpoint's, not a success and not a hang.
#[compio::test]
async fn replication_connect_reports_the_error_when_no_host_answers() {
    let url = test_url();
    let first_dead = closed_port().await;
    let second_dead = closed_port().await;

    let mut config = credentials_only(&url);
    config.host("127.0.0.1");
    config.port(first_dead);
    config.host("127.0.0.1");
    config.port(second_dead);

    let err = compio_postgres::replication::connect_replication(NoTls, &config)
        .await
        .err()
        .expect("no host was listening, so this cannot succeed");
    assert!(
        !common::server_answered(&err),
        "nothing answered, so this must be a connect failure: {}",
        common::error_chain(&err)
    );
}

/// `IDENTIFY_SYSTEM` must return the server's real identity.
///
/// `postgres-protocol` consumes the `DataRow`'s `u16` field count during
/// `Message::parse` and keeps only the length-prefixed fields in
/// `DataRowBody::storage`, which is what `buffer()` hands back. The parser
/// read a `u16` count of its own as its first action, so it consumed the top
/// two bytes of the FIRST FIELD'S `i32` length instead. A 19-character
/// systemid is length `00 00 00 13`, whose leading two bytes are zero, so the
/// count read as 0, no fields were parsed, and the call returned empty
/// strings and a zero timeline while reporting success.
///
/// Asserted against the server's own `pg_control_system()` rather than
/// against a non-empty string, so the test pins the VALUE and not merely the
/// absence of the default.
#[compio::test]
async fn identify_system_returns_the_servers_real_identity() {
    let url = test_url();

    // The expected identity, read over an ordinary connection.
    //
    // Routed through `postgres_unreachable` rather than `expect`, because a
    // bare message here would diagnose the wrong cause: a `53300`
    // too_many_connections refusal is a server that ANSWERED, and printing a
    // replication-configuration hint for it is the exact mistake
    // `tests/common/mod.rs` records a measured incident of.
    let (client, connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(pair) => pair,
        Err(e) => common::postgres_unreachable(&url, &e),
    };
    let driver = compio::runtime::spawn(async move { connection.run().await });
    let expected_systemid: String = client
        .query_one_scalar(
            "SELECT system_identifier::text FROM pg_control_system()",
            &[],
        )
        .await
        .expect("pg_control_system() failed");
    drop(client);
    let _ = driver.await;

    let mut config: Config = url.parse().expect("test DSN did not parse");
    config.application_name("cpg_identify_system");
    // Same reasoning as above: let the shared helper decide whether the server
    // answered before it names a remedy.
    let mut replication = match compio_postgres::replication::connect_replication(NoTls, &config)
        .await
    {
        Ok(connection) => connection,
        Err(e) if common::server_answered(&e) => panic!(
            "the server refused a replication connection: {}",
            common::error_chain(&e)
        ),
        Err(e) => common::postgres_unreachable(&url, &e),
    };

    let identity = replication
        .identify_system()
        .await
        .expect("IDENTIFY_SYSTEM failed");

    assert_eq!(
        identity.systemid, expected_systemid,
        "IDENTIFY_SYSTEM reported a system identifier the server does not have"
    );
    assert!(
        identity.timeline >= 1,
        "timeline {} is not a real timeline",
        identity.timeline
    );
    assert!(
        identity.xlogpos.contains('/'),
        "xlogpos {:?} is not an LSN",
        identity.xlogpos
    );
}

/// `connect_replication`'s TLS refusal is keyed to the CONTRADICTION, not to
/// the endpoint.
///
/// A matched pair built from ONE config, differing in exactly one call:
/// `ssl_mode`. `sslrootcert=system` is a contradiction under `Prefer` and is
/// fine under `VerifyFull`, so the same endpoint must be refused in the first
/// case and dialled in the second. Both arms are constructed here rather than
/// leaning on the unit test in `src/replication.rs`, which differs from this
/// one in host form, listener, timeout and credentials -- pairing against it
/// would have varied five things at once and proved nothing about which one
/// mattered.
#[compio::test]
async fn replication_tls_refusal_is_keyed_to_the_contradiction_not_the_endpoint() {
    // One dead port for both arms: the endpoint is held fixed by construction.
    let port = closed_port().await;
    let config_with = |mode| {
        let mut config = credentials_only(&test_url());
        config.host("127.0.0.1");
        config.port(port);
        config.ssl_mode(mode);
        config.ssl_root_cert(compio_postgres::config::SslRootCert::System);
        config
    };

    let refused = compio_postgres::replication::connect_replication(
        NoTls,
        &config_with(compio_postgres::config::SslMode::Prefer),
    )
    .await
    .err()
    .expect("a weak sslmode with sslrootcert=system is a contradiction");
    let refused_chain = common::error_chain(&refused);
    assert!(
        refused_chain.contains("sslrootcert=system"),
        "the contradiction must be named by the refusal, got: {refused_chain}"
    );

    let dialled = compio_postgres::replication::connect_replication(
        NoTls,
        &config_with(compio_postgres::config::SslMode::VerifyFull),
    )
    .await
    .err()
    .expect("nothing is listening on that port, so this cannot succeed");
    let dialled_chain = common::error_chain(&dialled);
    assert!(
        !dialled_chain.contains("sslrootcert=system"),
        "verify-full is not a contradiction, so validation must let it through: {dialled_chain}"
    );
    // Asserted POSITIVELY: without this, any unrelated early failure that
    // merely lacks the literal would satisfy the check above.
    assert!(
        !common::server_answered(&dialled),
        "the second arm must reach the socket and be refused there: {dialled_chain}"
    );
}
