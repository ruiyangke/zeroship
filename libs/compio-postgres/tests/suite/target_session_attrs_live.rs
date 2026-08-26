//! Live `target_session_attrs` routing tests.

use compio_postgres::config::{Host, SslMode, TargetSessionAttrs};
use compio_postgres::{Client, Config, Error, SimpleQueryMessage};
use std::error::Error as _;
use std::io;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
}

fn config_for(mode: TargetSessionAttrs, application_name: &str) -> Config {
    let mut config: Config = test_url().parse().expect("test DSN did not parse");
    config
        .target_session_attrs(mode)
        .application_name(application_name);
    config
}

async fn connect_and_drive(config: &Config) -> Result<Client, Error> {
    let (client, connection) = config.connect(common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("target_session_attrs connection error: {error}");
        }
    })
    .detach();
    Ok(client)
}

fn source_io_error(error: &Error) -> Option<&io::Error> {
    let mut source = error.source();
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<io::Error>() {
            return Some(io);
        }
        source = error.source();
    }
    None
}

async fn transaction_read_only(client: &Client) -> String {
    client
        .simple_query("SHOW transaction_read_only")
        .await
        .expect("SHOW transaction_read_only failed")
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
            _ => None,
        })
        .expect("SHOW transaction_read_only returned no row")
}

async fn server_is_in_recovery(client: &Client) -> bool {
    let value = client
        .simple_query("SELECT pg_catalog.pg_is_in_recovery()")
        .await
        .expect("pg_is_in_recovery() failed")
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
            _ => None,
        })
        .expect("pg_is_in_recovery() returned no row");

    match value.as_str() {
        "t" => true,
        "f" => false,
        value => panic!("pg_is_in_recovery() returned {value:?}"),
    }
}

#[compio::test]
async fn primary_accepts_a_server_not_in_recovery() {
    let config = config_for(TargetSessionAttrs::Primary, "cpg_standby_primary");
    let client = connect_and_drive(&config)
        .await
        .expect("primary rejected the primary test server");

    assert!(!server_is_in_recovery(&client).await);
}

#[compio::test]
async fn standby_rejects_a_read_only_primary() {
    let mut config = config_for(TargetSessionAttrs::Standby, "cpg_standby_reject_primary");
    config.options("-c default_transaction_read_only=on");

    let Err(error) = config.connect(common::suite_tls()).await else {
        panic!("standby accepted a primary configured read only");
    };
    let io = source_io_error(&error).expect("target mismatch did not retain its I/O cause");
    assert_eq!(io.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(io.to_string(), "database server is not in recovery");
}

#[compio::test]
async fn primary_accepts_a_read_only_session() {
    let mut config = config_for(
        TargetSessionAttrs::Primary,
        "cpg_standby_primary_read_only",
    );
    config.options("-c default_transaction_read_only=on");
    let client = connect_and_drive(&config)
        .await
        .expect("primary rejected a read-only session on the primary server");

    assert_eq!(transaction_read_only(&client).await, "on");
    assert!(!server_is_in_recovery(&client).await);
}

#[compio::test]
async fn read_write_accepts_a_writable_server() {
    let config = config_for(TargetSessionAttrs::ReadWrite, "cpg_tsa_read_write");
    let client = connect_and_drive(&config)
        .await
        .expect("read-write rejected the writable test server");

    assert_eq!(transaction_read_only(&client).await, "off");
}

#[compio::test]
async fn read_only_rejects_a_writable_server() {
    let config = config_for(TargetSessionAttrs::ReadOnly, "cpg_tsa_read_only_reject");
    let error = match config.connect(common::suite_tls()).await {
        Ok(_) => panic!("read-only accepted a session whose transaction_read_only is off"),
        Err(error) => error,
    };
    let io = source_io_error(&error).expect("target mismatch did not retain its I/O cause");
    assert_eq!(io.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(io.to_string(), "database is not read only");
}

#[compio::test]
async fn read_only_accepts_a_session_configured_read_only() {
    let mut config = config_for(TargetSessionAttrs::ReadOnly, "cpg_tsa_read_only_accept");
    config.options("-c default_transaction_read_only=on");
    let client = connect_and_drive(&config)
        .await
        .expect("read-only rejected a session configured read only");

    assert_eq!(transaction_read_only(&client).await, "on");
}

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

fn live_endpoint(url: &str) -> (String, u16) {
    let parsed: Config = url.parse().expect("test DSN did not parse");
    let host = match parsed.get_hosts().first().expect("test DSN names no host") {
        Host::Tcp(host) => host.clone(),
        #[cfg(unix)]
        Host::Unix(path) => panic!(
            "the multi-host test needs TCP, got the socket {}",
            path.display()
        ),
    };
    let port = parsed.get_ports().first().copied().unwrap_or(5432);
    (host, port)
}

async fn closed_port() -> u16 {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().expect("listener address").port();
    drop(listener);
    port
}

#[compio::test]
async fn read_write_reaches_the_second_host_after_a_closed_first_host() {
    let url = test_url();
    let (live_host, live_port) = live_endpoint(&url);
    let dead_port = closed_port().await;
    let mut config = credentials_only(&url);
    config
        .host("127.0.0.1")
        .port(dead_port)
        .host(live_host)
        .port(live_port)
        .ssl_mode(SslMode::Disable)
        .target_session_attrs(TargetSessionAttrs::ReadWrite)
        .application_name("cpg_tsa_multi_host");

    let client = connect_and_drive(&config)
        .await
        .expect("the closed first host prevented reaching the live second host");
    assert_eq!(transaction_read_only(&client).await, "off");
}

#[compio::test]
async fn prefer_standby_retries_an_all_primary_host_list_in_any_mode() {
    let url = test_url();
    let (live_host, live_port) = live_endpoint(&url);
    let mut config = credentials_only(&url);
    config
        .host(live_host.clone())
        .port(live_port)
        .host(live_host)
        .port(live_port)
        .ssl_mode(SslMode::Disable)
        .target_session_attrs(TargetSessionAttrs::PreferStandby)
        .application_name("cpg_standby_prefer_fallback");

    let client = connect_and_drive(&config)
        .await
        .expect("prefer-standby did not retry the all-primary list in any mode");
    assert!(!server_is_in_recovery(&client).await);
}
