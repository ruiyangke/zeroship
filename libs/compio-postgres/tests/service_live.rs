//! `pg_service.conf`, end to end against a live server.
//!
//! Locating a service file reads `PGSERVICEFILE` from the PROCESS
//! environment, and this suite does not set environment variables in-process:
//! `std::env::set_var` is unsound to call once other threads exist, and a
//! variable set for one test leaks into every other test in the binary.
//!
//! So the real path is exercised in a CHILD process. The parent writes a
//! service file, re-runs this same test binary with `PGSERVICEFILE` pointing
//! at it, and reads the child's exit status. `Command::env` is the safe way to
//! set a variable, because it configures the child rather than mutating
//! anything here.
//!
//! The child case is `#[ignore]`d so an ordinary run skips it rather than
//! failing with no service file; the parent runs it explicitly with
//! `--ignored --exact`. That also means it cannot pass vacuously - a skipped
//! test is reported as ignored, and the parent asserts on the exit status of a
//! run it requested by name.

#[allow(dead_code)]
mod common;
use common::{suite_tls, test_url};
use compio_postgres::Config;
use std::path::PathBuf;
use std::process::Command;

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

/// Run the ignored child case with `PGSERVICEFILE` set to `path`.
fn run_child(path: &PathBuf) -> std::process::Output {
    Command::new(std::env::current_exe().expect("the test binary's own path"))
        .args([
            "--ignored",
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "a_service_names_the_connection",
        ])
        .env("PGSERVICEFILE", path)
        .env("PG_TEST_URL", test_url())
        .output()
        .expect("re-run this test binary as a child")
}

/// THE CHILD CASE. Connects using nothing but a service name, so every
/// connection parameter comes from the file the parent wrote.
#[compio::test]
#[ignore = "run by the parent case with PGSERVICEFILE set"]
async fn a_service_names_the_connection() {
    let dsn = format!("service={SERVICE}");
    let config = dsn
        .parse::<Config>()
        .expect("the service expands into a usable config");
    assert_eq!(
        config.get_service(),
        Some(SERVICE),
        "the service name was not recorded"
    );

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
}

#[test]
fn a_service_file_supplies_every_connection_parameter() {
    let path = service_file_path("good");
    std::fs::write(&path, service_file_contents()).expect("write the service file");

    let output = run_child(&path);

    std::fs::remove_file(&path).ok();
    assert!(
        output.status.success(),
        "the child could not connect through the service file:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // A run that matched no test also exits 0, which would make the assertion
    // above pass without connecting to anything.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 passed"),
        "the child ran no test, so its success says nothing:\n{stdout}"
    );
}

/// The control: the same machinery with a service file that does NOT define
/// the service must fail. Without it, a child that silently ignored the file
/// and connected some other way would look like success above.
#[test]
fn an_undefined_service_is_an_error() {
    let path = service_file_path("undefined");
    std::fs::write(&path, "[some_other_service]\nhost=127.0.0.1\n")
        .expect("write the service file");

    let output = run_child(&path);

    std::fs::remove_file(&path).ok();
    assert!(
        !output.status.success(),
        "a service that is not defined connected anyway"
    );
}

/// Naming a service with no service file anywhere is an error too, rather
/// than a connection that quietly falls back to defaults.
#[test]
fn a_missing_service_file_is_an_error() {
    let path = service_file_path("absent_never_written");
    assert!(!path.exists(), "this case needs a path that does not exist");

    let output = run_child(&path);

    assert!(
        !output.status.success(),
        "a service naming a nonexistent file connected anyway"
    );
}
