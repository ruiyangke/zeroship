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

/// Fail the calling test because PostgreSQL could not be reached.
///
/// This is what a missing database does now. It used to announce a skip, which
/// cargo counts as a pass, and the only way to make it fatal was to remember to
/// export `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` - a flag whose whole design was
/// that the person who most needed it was the one who did not know it existed.
///
/// The message has to answer three questions or it is no better than the
/// `connection refused` it replaces: WHICH backend, WHERE it was dialled (with
/// the password removed - see [`redact_dsn`]), and WHAT COMMAND provisions one.
#[track_caller]
pub fn postgres_unreachable(dsn: &str, error: &dyn std::fmt::Display) -> ! {
    panic!(
        "PostgreSQL is unreachable, and this test requires it.\n\
         \n\
         \x20 backend: PostgreSQL\n\
         \x20 dialled: {}\n\
         \x20 error:   {error}\n\
         \n\
         Provision it, then re-run:\n\
         \x20 tests/provision_test_backends.sh\n\
         \n\
         That brings up the `postgres` and `redis` services from\n\
         deploy/compose/docker-compose.yml and waits for both to be healthy.\n\
         Point the tests somewhere else with PG_TEST_URL.\n\
         \n\
         There is no environment variable that makes this a skip. A database\n\
         this suite cannot reach is a failed run, not a green one.",
        redact_dsn(dsn)
    )
}

#[cfg(test)]
mod tests {
    use super::redact_dsn;

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
