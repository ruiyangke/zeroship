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
pub fn test_url() -> String {
    env::get(env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
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
    config.application_name(application_name);
    config
}

#[cfg(test)]
mod tests {
    use super::{MAX_POSTGRES_IDENTIFIER_LEN, redact_dsn, test_object_name};

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
