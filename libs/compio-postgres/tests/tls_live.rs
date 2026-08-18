//! Live TLS tests for the rustls connector, against real PostgreSQL servers.
//!
//! Constructing a connector proves nothing about a handshake, and neither does
//! a single successful connection: a `sslmode=require` connection that succeeds
//! against a TLS-enabled server might also have succeeded without any of this
//! code. So every claim here is paired with the case that must go the other
//! way, and the encryption claim is settled by the *server's* view of the
//! session (`pg_stat_ssl`), never by `connect()` returning `Ok`.
//!
//! Run with:
//!   libs/compio-postgres/tests/tls_live_setup.sh
//!   cargo test -p compio-postgres --features tls --test tls_live
//!
//! Without that setup the suite skips - and says so on stdout, which
//! `cargo test -- --nocapture` shows. A skipped run is not a passing run.

#![cfg(feature = "tls")]

use compio_postgres::{Client, Error, NoTls, Pool};

/// Written by `tls_live_setup.sh`. A file, not an environment variable,
/// because `libs/compio-postgres` may not read the environment outside
/// `tests/common/env.rs` (see the header of that file).
const DESCRIPTOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/live/tls_live.conf");

struct Servers {
    /// A PostgreSQL with `ssl=on`, presenting a certificate signed by `ca`.
    tls_url: String,
    /// A PostgreSQL with TLS off entirely.
    plain_url: String,
    /// The private CA that signed the TLS server's certificate. It is in no
    /// system trust store, which is what makes the `sslrootcert=system` case
    /// below a real negative.
    ca: String,
}

impl Servers {
    fn load() -> Option<Servers> {
        let text = std::fs::read_to_string(DESCRIPTOR).ok()?;
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::to_owned)
        };
        Some(Servers {
            tls_url: field("tls_url")?,
            plain_url: field("plain_url")?,
            ca: field("ca")?,
        })
    }
}

/// Returns the servers, or `None` after printing why the test is not running.
fn servers() -> Option<Servers> {
    match Servers::load() {
        Some(servers) => Some(servers),
        None => {
            println!("SKIP: no {DESCRIPTOR}; run libs/compio-postgres/tests/tls_live_setup.sh");
            None
        }
    }
}

/// Ask the server - not the client - whether the session is encrypted.
///
/// `pg_stat_ssl.ssl` is the backend's own record of the connection it is
/// serving. A client-side assertion could only report what the client believes
/// it negotiated.
async fn server_reports_ssl(client: &Client) -> (bool, String) {
    let row = client
        .query_one(
            "SELECT ssl, coalesce(version, '') FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .expect("pg_stat_ssl");
    (row.get(0), row.get(1))
}

/// `sslmode=require` connects, and the session really is encrypted.
#[compio::test]
async fn require_connects_and_the_server_sees_an_encrypted_session() {
    let Some(servers) = servers() else { return };

    let url = format!(
        "{} sslmode=require sslrootcert={}",
        servers.tls_url, servers.ca
    );
    let pool = Pool::connect(&url, 2).await.expect("pool connects");
    let client = pool.get().await.expect("checkout");

    let (ssl, version) = server_reports_ssl(&client).await;
    assert!(ssl, "pg_stat_ssl.ssl is false: the session is NOT encrypted");
    assert!(
        version.starts_with("TLSv"),
        "pg_stat_ssl.version = {version:?}"
    );
    println!("require: pg_stat_ssl.ssl = {ssl}, version = {version}");
}

/// The control for the test above. Same pool, same server, same code path
/// except for `sslmode` - and the server must report an UNencrypted session.
///
/// Without this, "ssl = true" would be consistent with a `pg_stat_ssl` that
/// reports true for everything.
#[compio::test]
async fn disable_connects_and_the_server_sees_a_plaintext_session() {
    let Some(servers) = servers() else { return };

    let url = format!("{} sslmode=disable", servers.tls_url);
    let pool = Pool::connect(&url, 2).await.expect("pool connects");
    let client = pool.get().await.expect("checkout");

    let (ssl, version) = server_reports_ssl(&client).await;
    assert!(!ssl, "sslmode=disable produced an encrypted session");
    assert_eq!(version, "");
    println!("disable: pg_stat_ssl.ssl = {ssl}");
}

/// `sslmode=require` against a server with TLS switched off must FAIL.
///
/// This is what distinguishes "the connector works" from "the pool ignored
/// sslmode and connected in plaintext". `prefer` would legitimately fall back
/// here; `require` must not.
#[compio::test]
async fn require_fails_against_a_server_without_tls() {
    let Some(servers) = servers() else { return };

    let url = format!(
        "{} sslmode=require sslrootcert={}",
        servers.plain_url, servers.ca
    );
    let err = Pool::connect(&url, 2)
        .await
        .err()
        .expect("sslmode=require must not connect to a plaintext-only server");
    let text = format!("{err}: {:?}", std::error::Error::source(&err));
    assert!(
        text.contains("does not support TLS"),
        "unexpected error: {text}"
    );
    println!("require against plaintext server: {text}");
}

/// Verification is real: the same server, the same `sslmode=require`, but
/// pointed at the OS trust store instead of the CA that signed it, must fail.
///
/// This is the test that would catch a connector wired up with a
/// no-op certificate verifier - the failure mode where every other assertion
/// in this file still passes.
#[compio::test]
async fn require_fails_when_the_certificate_is_not_trusted() {
    let Some(servers) = servers() else { return };

    let url = format!("{} sslmode=require sslrootcert=system", servers.tls_url);
    let err = Pool::connect(&url, 2)
        .await
        .err()
        .expect("a certificate signed by an untrusted private CA must be rejected");
    let text = format!("{err}: {:?}", std::error::Error::source(&err));
    assert!(
        text.contains("UnknownIssuer") || text.contains("invalid peer certificate"),
        "expected a certificate-verification failure, got: {text}"
    );
    println!("require with system roots: {text}");
}

/// `channel_binding=require` forces SCRAM-SHA-256-PLUS, which fails unless the
/// `tls-server-end-point` value we compute equals the one the server computed
/// from the same certificate.
///
/// So this is the end-to-end check on `tls_server_end_point`: the unit tests in
/// `src/tls_rustls.rs` only show that the digest width follows the signature
/// algorithm.
#[compio::test]
async fn channel_binding_require_completes_scram_plus() {
    let Some(servers) = servers() else { return };

    let url = format!(
        "{} sslmode=require sslrootcert={} channel_binding=require",
        servers.tls_url, servers.ca
    );
    let pool = Pool::connect(&url, 2).await.expect("SCRAM-SHA-256-PLUS");
    let client = pool.get().await.expect("checkout");

    let (ssl, _) = server_reports_ssl(&client).await;
    assert!(ssl);
    println!("channel_binding=require: authenticated over SCRAM-SHA-256-PLUS");
}

/// `channel_binding=require` over a plaintext connection has no binding to
/// offer, so it must fail rather than silently downgrading to plain SCRAM.
#[compio::test]
async fn channel_binding_require_fails_without_tls() {
    let Some(servers) = servers() else { return };

    let url = format!("{} sslmode=disable channel_binding=require", servers.tls_url);
    let err = Pool::connect(&url, 2)
        .await
        .err()
        .expect("channel_binding=require must not succeed over plaintext");
    let text = format!("{err}: {:?}", std::error::Error::source(&err));
    assert!(
        text.contains("channel binding"),
        "unexpected error: {text}"
    );
    println!("channel_binding=require over plaintext: {text}");
}

/// A `sslrootcert` that names a file which is not a certificate must be an
/// error about that file, raised before any socket is opened - not a handshake
/// failure blamed on the server.
#[compio::test]
async fn bad_sslrootcert_is_reported_as_a_configuration_error() {
    let Some(servers) = servers() else { return };

    let url = format!(
        "{} sslmode=require sslrootcert=/nonexistent/ca.crt",
        servers.tls_url
    );
    let err = Pool::connect(&url, 2).await.err().expect("must fail");
    let text = format!("{err}: {:?}", std::error::Error::source(&err));
    assert!(
        text.contains("sslrootcert=/nonexistent/ca.crt"),
        "the error must name the file: {text}"
    );
}

/// The plaintext path must be untouched by any of this: `NoTls` against the
/// plain server still works, with the driver's own `connect` entry point.
#[compio::test]
async fn notls_connect_still_works() {
    let Some(servers) = servers() else { return };

    let (client, connection) = compio_postgres::connect(&servers.plain_url, NoTls)
        .await
        .expect("NoTls connect");
    compio::runtime::spawn(async move {
        let _: Result<(), Error> = connection.run().await;
    })
    .detach();

    let (ssl, _) = server_reports_ssl(&client).await;
    assert!(!ssl);
}
