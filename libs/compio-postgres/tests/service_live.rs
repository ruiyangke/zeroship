//! `pg_service.conf`, end to end against a live server.
//!
//! The service file's PATH comes from the caller here, never from the
//! environment: this crate is a standalone, publishable driver and the
//! workspace forbids a published library reading process configuration
//! (`crates/core/tests/config_env_access_gate.rs`, exemption list empty). So
//! these tests write a file and name it with `Config::service_file`, which is
//! exactly what an application does.

#[allow(dead_code)]
mod common;
use common::{suite_tls, test_url};
use compio_postgres::Config;
use std::path::{Path, PathBuf};

const SERVICE: &str = "zs_compio_service_test";

fn service_file_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "compio_pg_service_{}_{tag}.conf",
        std::process::id()
    ))
}

/// A service section describing the suite's own server.
fn service_file_contents() -> String {
    let config = test_url().parse::<Config>().expect("the suite DSN parses");
    let host = match config.get_hosts().first().expect("a host") {
        compio_postgres::config::Host::Tcp(host) => host.clone(),
        other => panic!("this test needs a TCP host, got {other:?}"),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let user = config.get_user().expect("a user");
    let dbname = config.get_dbname().unwrap_or(user);
    let password = config
        .get_password()
        .map(|password| String::from_utf8_lossy(password).into_owned())
        .expect("the suite DSN carries a password");

    format!(
        "# written by tests/service_live.rs\n\
         [{SERVICE}]\n\
         host={host}\n\
         port={port}\n\
         dbname={dbname}\n\
         user={user}\n\
         password={password}\n"
    )
}

/// Parse `service=<name> servicefile=<path>`, exactly as an application would.
///
/// The file is named IN THE STRING because expansion happens while parsing -
/// the only point at which the set of explicitly given keys is still known.
fn config_for_service(path: &Path) -> Result<Config, compio_postgres::Error> {
    format!(
        "service={SERVICE} servicefile={}",
        path.to_str().expect("a UTF-8 temp path")
    )
    .parse()
}

#[compio::test]
async fn a_service_file_supplies_every_connection_parameter() {
    let path = service_file_path("good");
    std::fs::write(&path, service_file_contents()).expect("write the service file");

    let config = config_for_service(&path).expect("the service expands");
    let (client, connection) = config
        .connect(suite_tls())
        .await
        .expect("the service supplies a working connection");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .expect("the connection works");
    assert_eq!(one, 1);

    std::fs::remove_file(&path).ok();
}

/// The same working service, written in the awkward forms libpq accepts: a
/// header carrying a trailing comment, an indented comment line, leading
/// whitespace before a key, and `dbname` given TWICE where only the first is
/// real. Every one of these was probed against libpq 16.14
/// (`docs/runbooks/compio-postgres-service-file-semantics.md`); before that
/// this crate rejected the header outright and took the LAST duplicate, so
/// this connects only if the parser agrees with libpq rather than with the
/// tidy shape the format description suggests.
#[compio::test]
async fn a_service_written_in_libpq_s_awkward_forms_still_connects() {
    let plain = service_file_contents();
    let mut awkward = String::new();
    for line in plain.lines() {
        if let Some(rest) = line.strip_prefix('[') {
            let name = &rest[..rest.find(']').expect("the fixture header closes")];
            awkward.push_str(&format!("[{name}] # the section for this test\n"));
            awkward.push_str("   # an indented comment inside the section\n");
        } else if let Some(dbname) = line.strip_prefix("dbname=") {
            // The FIRST wins, so the bogus one must never be reached.
            awkward.push_str(&format!("dbname={dbname}\n"));
            awkward.push_str("dbname=zs_no_such_database\n");
        } else if let Some(port) = line.strip_prefix("port=") {
            awkward.push_str(&format!("   port={port}\n"));
        } else {
            awkward.push_str(line);
            awkward.push('\n');
        }
    }
    assert!(
        awkward.contains("zs_no_such_database"),
        "the fixture lost its duplicate key, so it proves nothing: {awkward}"
    );

    let path = service_file_path("awkward");
    std::fs::write(&path, &awkward).expect("write the service file");

    let config = config_for_service(&path).expect("the awkward service expands");
    let (client, connection) = config
        .connect(suite_tls())
        .await
        .expect("the awkward service supplies a working connection");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .expect("the connection works");
    assert_eq!(one, 1);

    std::fs::remove_file(&path).ok();
}

/// A service the file does not define is an error, not a silent fallback.
#[test]
fn an_undefined_service_is_an_error() {
    let path = service_file_path("undefined");
    std::fs::write(&path, "[some_other_service]\nhost=127.0.0.1\n")
        .expect("write the service file");

    let result = config_for_service(&path);

    std::fs::remove_file(&path).ok();
    let error = result.expect_err("a service that is not defined must not resolve");
    assert!(
        format!("{:?}", std::error::Error::source(&error)).contains(SERVICE),
        "the error does not name the service that was not found: {error:?}"
    );
}

/// Naming a service with no service file is an error rather than a connection
/// that quietly falls back to defaults - and specifically NOT a search of the
/// environment, which this driver does not perform.
#[test]
fn naming_a_service_without_a_file_is_an_error() {
    let error = format!("service={SERVICE}")
        .parse::<Config>()
        .expect_err("a service with nowhere to read it from must not resolve");
    assert!(
        format!("{:?}", std::error::Error::source(&error)).contains(SERVICE),
        "the error does not name the service: {error:?}"
    );
}
