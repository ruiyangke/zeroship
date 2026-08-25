//! What a failed connection tells the caller, and what it must never tell them.
//!
//! Two claims, checked together because they are in tension. A connection
//! error has to say enough to ACT on - a wrong port and a wrong password are
//! different problems with different fixes, and this session found three
//! places where the driver knew exactly what went wrong and reported only
//! that something had. It must also never carry the PASSWORD, because a
//! connect failure is the single most likely thing a caller logs verbatim,
//! and a secret in a log is at rest wherever that log goes.
//!
//! Every case therefore asserts both: the cause chain names the real problem,
//! AND the password does not appear anywhere in it.

#[allow(dead_code)]
mod common;
use common::test_url;
use compio_postgres::{Config, Error, NoTls};
use std::time::Duration;

/// A DNS lookup and a refused connect both have to finish well inside this;
/// it exists so a hung case fails the suite rather than stalling it.
const WATCHDOG: Duration = Duration::from_secs(15);

/// Distinctive enough that finding it in an error is unambiguous, and not a
/// substring of anything else here.
const WRONG_PASSWORD: &str = "zz-wrong-password-marker";

/// The error's full text, including every cause in the chain - which is where
/// the useful part lives; the top-level Display is deliberately terse.
fn chain(error: &Error) -> String {
    let mut rendered = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        rendered.push_str(" | ");
        rendered.push_str(&cause.to_string());
        source = std::error::Error::source(cause);
    }
    rendered
}

struct Server {
    host: String,
    port: u16,
    user: String,
    dbname: String,
    password: String,
}

fn server() -> Server {
    let config: Config = test_url().parse().expect("the suite DSN parses");
    let host = match config.get_hosts().first().expect("a host") {
        compio_postgres::config::Host::Tcp(host) => host.clone(),
        other => panic!("these tests need a TCP host, got {other:?}"),
    };
    let user = config.get_user().expect("a user").to_owned();
    Server {
        host,
        port: config.get_ports().first().copied().unwrap_or(5432),
        dbname: config.get_dbname().unwrap_or(&user).to_owned(),
        user,
        password: config
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .expect("the suite DSN carries a password"),
    }
}

/// Connect, expect failure, and return the whole cause chain.
async fn failure(dsn: String) -> String {
    let outcome = compio::time::timeout(WATCHDOG, compio_postgres::connect(&dsn, NoTls))
        .await
        .expect("the connection attempt exceeded its watchdog");
    match outcome {
        Ok(_) => panic!("this connection was supposed to fail"),
        Err(error) => chain(&error),
    }
}

fn assert_says(chain: &str, expected: &str, case: &str) {
    assert!(
        chain.contains(expected),
        "{case}: the error does not say what went wrong. Wanted something \
         containing {expected:?}, got: {chain}"
    );
}

fn assert_hides(chain: &str, secret: &str, case: &str) {
    assert!(
        !chain.contains(secret),
        "{case}: the error carries the password, which a caller will log: {chain}"
    );
}

#[compio::test]
async fn a_wrong_password_says_so_without_repeating_it() {
    let s = server();
    let chain = failure(format!(
        "host={} port={} user={} dbname={} password={WRONG_PASSWORD}",
        s.host, s.port, s.user, s.dbname
    ))
    .await;

    assert_says(&chain, "password authentication failed", "wrong password");
    assert_hides(&chain, WRONG_PASSWORD, "wrong password");
}

#[compio::test]
async fn a_host_that_does_not_resolve_says_so() {
    let s = server();
    // `.invalid` is reserved by RFC 2606 and never resolves.
    let chain = failure(format!(
        "host=nonexistent.invalid port=5432 user={} dbname={} password={WRONG_PASSWORD}",
        s.user, s.dbname
    ))
    .await;

    assert_says(&chain, "lookup address", "unknown host");
    assert_hides(&chain, WRONG_PASSWORD, "unknown host");
}

#[compio::test]
async fn a_refused_port_says_so_rather_than_blaming_credentials() {
    let s = server();
    let chain = failure(format!(
        "host={} port=1 user={} dbname={} password={WRONG_PASSWORD}",
        s.host, s.user, s.dbname
    ))
    .await;

    assert_says(&chain, "refused", "refused port");
    // The distinction that matters: a transport failure must not be reported
    // as an authentication failure, or the caller fixes the wrong thing.
    assert!(
        !chain.contains("password authentication failed"),
        "a refused connection was reported as an auth failure: {chain}"
    );
    assert_hides(&chain, WRONG_PASSWORD, "refused port");
}

/// Uses the REAL password, because otherwise authentication fails first and
/// the database error is never reached - which is what an earlier version of
/// this test measured without noticing.
#[compio::test]
async fn a_missing_database_is_named() {
    let s = server();
    let chain = failure(format!(
        "host={} port={} user={} dbname=zz_no_such_database password={}",
        s.host, s.port, s.user, s.password
    ))
    .await;

    assert_says(&chain, "zz_no_such_database", "missing database");
    assert_says(&chain, "does not exist", "missing database");
    assert_hides(&chain, &s.password, "missing database");
}

/// `sslmode=require` with no connector compiled in is a configuration
/// contradiction the driver can detect before touching the network, and it
/// says exactly that rather than failing later as a transport error.
#[compio::test]
async fn requiring_tls_without_a_connector_says_which_setting_is_at_fault() {
    let s = server();
    let chain = failure(format!(
        "host={} port={} user={} dbname={} password={WRONG_PASSWORD} sslmode=require",
        s.host, s.port, s.user, s.dbname
    ))
    .await;

    assert_says(&chain, "sslmode=require", "tls required");
    assert_hides(&chain, WRONG_PASSWORD, "tls required");
}

/// THE CONTROL. The same server, the same credentials, nothing wrong: it
/// connects. Without this every test above would also pass against a server
/// that was simply unreachable, and they would be measuring nothing.
#[compio::test]
async fn the_same_settings_without_a_fault_connect() {
    let s = server();
    let dsn = format!(
        "host={} port={} user={} dbname={} password={}",
        s.host, s.port, s.user, s.dbname, s.password
    );
    let (client, connection) = compio_postgres::connect(&dsn, NoTls)
        .await
        .expect("the unmodified settings connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .expect("the control connection works");
    assert_eq!(one, 1);
}
