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

/// Whether the server answered. A `DbError` anywhere in the chain is a message
/// PostgreSQL composed and sent, so something was listening, authenticated the
/// startup packet far enough to reply, and refused on purpose.
pub fn server_answered(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut link = Some(error);
    while let Some(current) = link {
        if current.is::<compio_postgres::error::DbError>() {
            return true;
        }
        link = current.source();
    }
    false
}

/// Fail the calling test because the connection this test needs was not made.
///
/// This is what a missing database does now. It used to announce a skip, which
/// cargo counts as a pass, and the only way to make it fatal was to remember to
/// export `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` - a flag whose whole design was
/// that the person who most needed it was the one who did not know it existed.
///
/// The message has to answer three questions or it is no better than the
/// `connection refused` it replaces: WHICH backend, WHERE it was dialled (with
/// the password removed - see [`redact_dsn`]), and WHAT COMMAND provisions one.
///
/// The third answer is only correct when nothing answered, and this printed it
/// unconditionally. Measured 2026-08-20: with `crate::release` disabled, nine
/// tests in `integration.rs` failed against a running, healthy server that had
/// hit its `max_connections` ceiling, and every one of them reported
///
/// ```text
///   error:   db error
/// Provision it, then re-run: ...
/// ```
///
/// That is the wrong cause, and a remedy for a server that was already up. So
/// the branch below asks whether PostgreSQL replied before it prescribes
/// anything, and [`error_chain`] prints what it said.
#[track_caller]
pub fn postgres_unreachable(dsn: &str, error: &(dyn std::error::Error + 'static)) -> ! {
    let dialled = redact_dsn(dsn);
    let cause = error_chain(error);

    if server_answered(error) {
        panic!(
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
        )
    }

    panic!(
        "PostgreSQL is unreachable, and this test requires it.\n\
         \n\
         \x20 backend: PostgreSQL\n\
         \x20 dialled: {dialled}\n\
         \x20 error:   {cause}\n\
         \n\
         Nothing answered, so provision it and re-run:\n\
         \x20 tests/provision_test_backends.sh\n\
         \n\
         That brings up the `postgres` and `redis` services from\n\
         deploy/compose/docker-compose.yml and waits for both to be healthy.\n\
         Point the tests somewhere else with PG_TEST_URL.\n\
         \n\
         There is no environment variable that makes this a skip. A database\n\
         this suite cannot reach is a failed run, not a green one."
    )
}

/// The DSN the integration suites use when `PG_TEST_URL` is unset.
///
/// ONE DEFINITION, because `tests/provision_test_backends.sh` says so in as
/// many words: it provisions `deploy/compose`'s postgres on 127.0.0.1:5440 and
/// notes that the defaults line up on purpose, so a developer who runs that
/// script needs to export nothing. That invariant only holds while the port
/// appears once per crate; it had drifted into 42 test files.
///
/// Absent is NOT "do not run" - see `TestEnvKey::PgTestUrl`. A target that
/// cannot reach this server must fail, not skip.
#[cfg(not(feature = "suite-over-tls"))]
pub fn test_url() -> String {
    env::get(env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
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
    format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=verify-full&sslrootcert={ca}",
        field("user"),
        field("password"),
        field("host"),
        field("port"),
        field("dbname"),
    )
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
    let config: compio_postgres::Config = test_url()
        .parse()
        .expect("the suite-over-tls DSN did not parse");
    compio_postgres::MakeRustlsConnect::from_config(&config)
        .expect("could not build the suite TLS connector")
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
        let _ = client
            .execute("SELECT pg_drop_replication_slot($1)", &[&name])
            .await;
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

/// Drop a replication slot, waiting for its walsender to let go first.
///
/// `drop(stream)` closes the connection CLIENT-side; the server retires the
/// walsender a moment later, and until it does the slot is still `active` and
/// `pg_drop_replication_slot` fails with 55006. Tests that ignore that error
/// LEAK THE SLOT, and slots are a bounded server resource - a run that leaks
/// enough of them starts failing with "max_replication_slots" on whatever
/// happens to run next, which reads as a misconfigured server rather than as
/// test litter.
///
/// Polls rather than sleeping a fixed amount: a sleep long enough for a loaded
/// machine is wasted on every green run, and one tuned on an idle machine is
/// the flake again. A slot that is already gone is success, not an error.
pub async fn drop_replication_slot(client: &compio_postgres::Client, slot: &str) {
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
            return;
        };
        let still_held = error.code().is_some_and(|code| code.code() == "55006");
        if !still_held || std::time::Instant::now() >= deadline {
            // Best effort: the caller is cleaning up, often after a failure
            // that is more interesting than this one.
            eprintln!("could not drop slot {slot}: {}", error_chain(&error));
            return;
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
/// with `suite-over-tls`. That is the comparison worth making: the transport
/// must not change any observable protocol behaviour, so the reference should
/// differ from the subject in exactly the transport and nothing else.
///
/// Without this the oracle would inherit `sslmode=verify-full` from
/// [`test_url`] and fail to connect at all.
#[cfg(feature = "suite-over-tls")]
pub fn plaintext_url() -> String {
    descriptor_field(&tls_descriptor(), "tls_url")
}

#[cfg(not(feature = "suite-over-tls"))]
pub fn plaintext_url() -> String {
    test_url()
}

#[cfg(test)]
mod tests {
    use super::{MAX_POSTGRES_IDENTIFIER_LEN, pid_embedded_in, redact_dsn, test_object_name};

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
}
