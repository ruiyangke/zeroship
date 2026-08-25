//! The password file, end to end against a live server.
//!
//! `src/passfile.rs` unit-tests the matching rules without a filesystem. This
//! file proves the remaining half: that a password nobody put in the config
//! reaches authentication, that the permission rule is enforced against a real
//! file, and that a non-matching file leaves the connection unauthenticated.
//!
//! THE CONTROLS ARE THE POINT. The server must actually demand a password, or
//! every case here passes without the file being read at all. That is not
//! hypothetical: this server's `pg_hba.conf` grants `trust` to `127.0.0.1`,
//! and a probe run from INSIDE the container connected happily with a
//! deliberately wrong password. Connections from the test host arrive as the
//! docker gateway address and hit the `scram-sha-256` rule instead, which is
//! what makes these assertions mean anything. `a_wrong_password_in_the_file_
//! still_fails` is the standing check that this is still true.

#[allow(dead_code)]
mod common;
use common::{suite_tls, test_url};
use compio_postgres::Config;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The same DSN with the password removed from its userinfo.
///
/// Removing it is the whole setup: a config that still carries a password
/// never consults the file, so a test built on one would pass no matter what
/// this feature did.
fn url_without_password(dsn: &str) -> String {
    let scheme_end = dsn.find("://").expect("a URL-form DSN") + 3;
    let authority_end = dsn[scheme_end..]
        .find(['/', '?'])
        .map_or(dsn.len(), |offset| scheme_end + offset);
    let authority = &dsn[scheme_end..authority_end];
    let at = authority.rfind('@').expect("the test DSN carries userinfo");
    let userinfo = &authority[..at];
    let user = userinfo.split(':').next().unwrap_or(userinfo);

    format!(
        "{}{}{}{}",
        &dsn[..scheme_end],
        user,
        &authority[at..],
        &dsn[authority_end..]
    )
}

/// Write `contents` to a uniquely named file with `mode`.
fn write_passfile(name: &str, contents: &str, mode: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("compio_pgpass_{}_{}", std::process::id(), name));
    std::fs::write(&path, contents).expect("write the password file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        .expect("set the password file mode");
    path
}

/// The connection identity the file has to be keyed on, taken from the DSN so
/// this works against whichever server the suite is pointed at.
fn identity(config: &Config) -> (String, u16, String, String) {
    let host = match config.get_hosts().first().expect("a host") {
        compio_postgres::config::Host::Tcp(host) => host.clone(),
        other => panic!("this test needs a TCP host, got {other:?}"),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let user = config.get_user().expect("a user").to_owned();
    let dbname = config.get_dbname().unwrap_or(&user).to_owned();
    (host, port, dbname, user)
}

fn config_without_password() -> Config {
    url_without_password(&test_url())
        .parse::<Config>()
        .expect("the stripped DSN still parses")
}

async fn connect_with(passfile: &Path) -> Result<(), compio_postgres::Error> {
    let mut config = config_without_password();
    config.passfile(passfile.to_str().expect("a UTF-8 temp path"));
    let (client, connection) = config.connect(suite_tls()).await?;
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .expect("the connection works once authenticated");
    assert_eq!(one, 1);
    Ok(())
}

/// The password the suite's DSN carries, which the file must supply instead.
fn suite_password() -> String {
    let dsn = test_url();
    let scheme_end = dsn.find("://").expect("a URL-form DSN") + 3;
    let authority_end = dsn[scheme_end..]
        .find(['/', '?'])
        .map_or(dsn.len(), |offset| scheme_end + offset);
    let authority = &dsn[scheme_end..authority_end];
    let at = authority.rfind('@').expect("userinfo");
    let userinfo = &authority[..at];
    let colon = userinfo.find(':').expect("the test DSN carries a password");
    userinfo[colon + 1..].to_owned()
}

#[compio::test]
async fn a_password_file_authenticates_a_config_that_has_no_password() {
    let config = config_without_password();
    assert!(
        config.get_password().is_none(),
        "the DSN still carries a password, so the file would never be read"
    );
    let (host, port, dbname, user) = identity(&config);
    let line = format!("{host}:{port}:{dbname}:{user}:{}\n", suite_password());
    let path = write_passfile("ok", &line, 0o600);

    connect_with(&path)
        .await
        .expect("the password file authenticates");

    std::fs::remove_file(&path).ok();
}

/// The control that keeps every other case in this file honest: if the server
/// were not demanding a password, this would connect too.
#[compio::test]
async fn a_wrong_password_in_the_file_still_fails() {
    let config = config_without_password();
    let (host, port, dbname, user) = identity(&config);
    let line = format!("{host}:{port}:{dbname}:{user}:definitely_not_the_password\n");
    let path = write_passfile("wrong", &line, 0o600);

    let result = connect_with(&path).await;

    std::fs::remove_file(&path).ok();
    assert!(
        result.is_err(),
        "the server accepted a wrong password from the file, so this server \
         does not require one and NONE of the assertions in this file mean \
         anything - check pg_hba.conf for a trust rule covering the test host"
    );
}

/// MEASURED against libpq: modes 0644, 0640 and 0604 all make it ignore the
/// file. Any group or world bit disqualifies it, so this asserts a
/// group-readable file is not used even though its contents are correct.
#[compio::test]
async fn a_group_readable_password_file_is_ignored() {
    let config = config_without_password();
    let (host, port, dbname, user) = identity(&config);
    let line = format!("{host}:{port}:{dbname}:{user}:{}\n", suite_password());
    let path = write_passfile("perms", &line, 0o640);

    let result = connect_with(&path).await;

    std::fs::remove_file(&path).ok();
    assert!(
        result.is_err(),
        "a group-readable password file was used; libpq ignores it"
    );
}

/// The same correct password under a host that does not match must not be
/// used - otherwise the matcher is accepting everything and the passing tests
/// above prove nothing about matching.
#[compio::test]
async fn a_line_for_a_different_host_is_not_used() {
    let config = config_without_password();
    let (_host, port, dbname, user) = identity(&config);
    let line = format!(
        "a-host-this-test-never-connects-to:{port}:{dbname}:{user}:{}\n",
        suite_password()
    );
    let path = write_passfile("otherhost", &line, 0o600);

    let result = connect_with(&path).await;

    std::fs::remove_file(&path).ok();
    assert!(
        result.is_err(),
        "a password file keyed on a different host was used"
    );
}

/// A wildcard line is the common real-world shape, so pin that it works
/// against the live server rather than only in the unit tests.
#[compio::test]
async fn a_wildcard_line_authenticates() {
    let path = write_passfile(
        "wildcard",
        &format!("*:*:*:*:{}\n", suite_password()),
        0o600,
    );

    connect_with(&path)
        .await
        .expect("a wildcard line authenticates");

    std::fs::remove_file(&path).ok();
}

/// An absent file is not an error: libpq carries on and lets authentication
/// fail on its own terms. The connection must fail, but as an auth failure
/// rather than a config error about a missing file.
#[compio::test]
async fn an_absent_password_file_is_not_a_config_error() {
    let path = std::env::temp_dir().join(format!(
        "compio_pgpass_{}_absent_does_not_exist",
        std::process::id()
    ));
    assert!(
        !path.exists(),
        "the test's absent path must really be absent"
    );

    let error = connect_with(&path)
        .await
        .expect_err("no password is available, so the connection cannot succeed");

    let rendered = format!("{error}");
    assert!(
        !rendered.contains("passfile") && !rendered.contains("No such file"),
        "an absent password file surfaced as a file error rather than an \
         authentication failure: {rendered}"
    );
}
