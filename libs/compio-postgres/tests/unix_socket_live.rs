//! Connecting over a Unix domain socket, end to end against a live server.
//!
//! `libs/compio-postgres/tests/suite/unix_socket_path_limit.rs` proves that a path past `sun_path` is
//! REFUSED rather than truncated. That is the failure half. This is the
//! success half, and until 2026-08-25 nothing covered it: the fixture the
//! suite shipped mounted its socket 110 bytes deep, so it could not be
//! connected to at all, and `Host::Unix` reaching a real server was asserted
//! nowhere. See `tests/unix_socket_setup.sh` for how that happened.
//!
//! Behind `live-unix-socket` for the reason the `live-tls-tests` feature
//! gives in Cargo.toml: a runtime "is the socket there?" check can only fail
//! or return early, and an early return is counted by the harness as a PASS,
//! so the absent-fixture case would look identical to the working one.

#[allow(dead_code)]
mod common;
use compio_postgres::{Client, Config, NoTls};
use std::path::PathBuf;

const DESCRIPTOR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/live/unix_socket.conf"
);

struct Fixture {
    socket_dir: String,
    dbname: String,
    user: String,
}

fn fixture() -> Fixture {
    let text = std::fs::read_to_string(DESCRIPTOR).unwrap_or_else(|error| {
        panic!(
            "cannot read {DESCRIPTOR}: {error}\n\
             Run libs/compio-postgres/tests/unix_socket_setup.sh first. This suite is opted \
             into with --features live-unix-socket and does not skip."
        )
    });
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("{DESCRIPTOR} has no {key}=; re-run unix_socket_setup.sh"))
    };
    Fixture {
        socket_dir: field("socket_dir"),
        dbname: field("dbname"),
        user: field("user"),
    }
}

fn dsn(fixture: &Fixture) -> String {
    format!(
        "host={} user={} dbname={}",
        fixture.socket_dir, fixture.user, fixture.dbname
    )
}

async fn connect(dsn: &str) -> Client {
    let (client, connection) = compio_postgres::connect(dsn, NoTls)
        .await
        .unwrap_or_else(|error| panic!("connecting over the Unix socket failed: {error}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

#[compio::test]
async fn a_unix_socket_connection_runs_queries() {
    let fixture = fixture();
    let client = connect(&dsn(&fixture)).await;

    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .expect("a query over the socket");
    assert_eq!(one, 1);
}

/// The server's own view, so this cannot pass by having quietly used TCP.
///
/// `pg_stat_activity.client_addr` is NULL for a Unix-socket backend and holds
/// an address for a TCP one, which is the discriminator. Asserting only that a
/// query succeeded would be satisfied by a driver that ignored the socket path
/// and dialled localhost.
#[compio::test]
async fn the_server_sees_a_unix_socket_backend_not_a_tcp_one() {
    let fixture = fixture();
    let client = connect(&dsn(&fixture)).await;

    let over_socket: bool = client
        .query_one_scalar(
            "SELECT client_addr IS NULL FROM pg_stat_activity WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .expect("the server reports how this backend connected");
    assert!(
        over_socket,
        "the server sees a client address, so this connection went over TCP \
         and the socket path was ignored"
    );
}

/// The parsed config really is a Unix host, not a hostname that happens to
/// look like a path.
#[compio::test]
async fn the_socket_directory_parses_as_a_unix_host() {
    let fixture = fixture();
    let config: Config = dsn(&fixture).parse().expect("the fixture DSN parses");
    match config.get_hosts().first() {
        Some(compio_postgres::config::Host::Unix(path)) => {
            assert_eq!(path, &PathBuf::from(&fixture.socket_dir));
        }
        other => panic!("expected a Unix host, got {other:?}"),
    }
}

/// A socket directory with no socket in it must fail, or the tests above would
/// pass for a driver that reached the server some other way.
#[compio::test]
async fn a_directory_without_a_socket_is_refused() {
    let fixture = fixture();
    let empty = std::env::temp_dir().join(format!("zscpg_empty_{}", std::process::id()));
    std::fs::create_dir_all(&empty).expect("create an empty socket directory");

    let dsn = format!(
        "host={} user={} dbname={}",
        empty.display(),
        fixture.user,
        fixture.dbname
    );
    let result = compio_postgres::connect(&dsn, NoTls).await;
    std::fs::remove_dir_all(&empty).ok();

    assert!(
        result.is_err(),
        "a directory holding no socket must not yield a connection"
    );
}
