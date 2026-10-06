//! Connecting over a Unix domain socket, end to end against a live server.
//!
//! `libs/compio-postgres/tests/integration/unix_socket_path_limit.rs` proves
//! that a path past `sun_path` is REFUSED rather than truncated. That is the
//! failure half. This is the success half: `Host::Unix` reaching a real server.
//! The server is `zeroship_testkit_server::compio_postgres::unix`'s, which puts
//! its socket in a directory this host shares with its container and hands out
//! a short path to it, so the socket address fits `sun_path` however deep the
//! checkout is.

use compio_postgres::{Client, Config, NoTls};
use zeroship_testkit_server::compio_postgres::unix::UnixServer;

fn fixture() -> &'static UnixServer {
    zeroship_testkit_server::compio_postgres::unix::server()
}

fn dsn(fixture: &UnixServer) -> String {
    format!(
        "host={} user={} dbname={}",
        fixture.socket_dir().display(),
        fixture.user(),
        fixture.dbname()
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
    let client = connect(&dsn(fixture)).await;

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
    let client = connect(&dsn(fixture)).await;

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
    let config: Config = dsn(fixture).parse().expect("the fixture DSN parses");
    match config.get_hosts().first() {
        Some(compio_postgres::config::Host::Unix(path)) => {
            assert_eq!(path, fixture.socket_dir());
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
        fixture.user(),
        fixture.dbname()
    );
    let result = compio_postgres::connect(&dsn, NoTls).await;
    std::fs::remove_dir_all(&empty).ok();

    assert!(
        result.is_err(),
        "a directory holding no socket must not yield a connection"
    );
}
