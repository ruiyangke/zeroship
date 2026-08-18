//! Skip announcer, and the hard failure that replaced the skip for Redis.
//!
//! The announcer is a deliberate copy of `crates/test-support`, which is where
//! the reasoning behind the marker and the direct-handle write is written down.
//! `compio-redis` is a standalone, publishable library with no zeroship
//! dependency, and it does not grow one for a test helper. What must stay
//! identical is the MARKER TEXT: one search over a run log has to find every
//! skip in the workspace, whichever side of that line it came from.

// Shared by two test targets that use DIFFERENT subsets of it: `cluster.rs`
// calls `skip` and never `connect_pool`, `integration.rs` the reverse. Cargo
// compiles this module once per target, so each build legitimately sees the
// other's half as dead. The alternative is splitting one small helper across
// two files to satisfy a lint.
#![allow(dead_code)]

pub mod env;

use std::io::Write;

use compio_redis::{Client, Pool};

pub const SKIP_MARKER: &str = "ZEROSHIP-TEST-SKIPPED";

/// The single-node Redis every target in this crate dials when `REDIS_TEST_URL`
/// is unset.
///
/// 6390, not 6379, and the port is the load-bearing part. `deploy/compose`
/// publishes this crate's Redis on `127.0.0.1:6390`; 6379 is the conventional
/// default and is therefore exactly the port some OTHER project's container is
/// already holding on a shared development machine, which is a worse failure
/// than no Redis at all - the suite connects, passes, and was never testing the
/// server anyone thought it was.
pub const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:6390";

/// Announce that a test did nothing because an OPTIONAL backend is absent.
///
/// Single-node Redis is not one; it is dialled through [`connect`] /
/// [`connect_pool`], which fail. What still legitimately announces here is the
/// three-node Dragonfly CLUSTER in `cluster.rs`: a different topology, which
/// `deploy/compose/cluster.yml` stands up separately and which
/// `tests/provision_test_backends.sh` does not.
pub fn skip(reason: &str) {
    let _ = std::io::stderr().write_all(format!("{SKIP_MARKER}: {reason}\n").as_bytes());
}

/// The URL the tests dial: `REDIS_TEST_URL` if set, else [`DEFAULT_REDIS_URL`].
///
/// It has no `None` arm any more. An unset variable used to mean "do not run",
/// so the suite reported eleven passes against no server at all.
pub fn test_url() -> String {
    env::get(env::TestEnvKey::RedisTestUrl).unwrap_or_else(|| DEFAULT_REDIS_URL.to_string())
}

/// Hide the password in a `redis://user:pass@host:port` URL.
///
/// The message below prints the address that was actually dialled, and
/// `REDIS_TEST_URL` can carry a credential. Only the userinfo inside the
/// authority is touched; the split takes the LAST `@` before the end of the
/// authority, so a password containing `@` cannot leak its tail.
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?'])
        .map_or(url.len(), |offset| authority_start + offset);
    let authority = &url[authority_start..authority_end];

    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };

    format!(
        "{}{}:***{}",
        &url[..authority_start],
        &userinfo[..colon],
        &url[authority_start + at..]
    )
}

/// Fail the calling test because Redis could not be reached.
///
/// This is what a missing Redis does now. It used to announce a skip, which
/// cargo counts as a pass, and the only way to make it fatal was to remember to
/// export `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` - a flag whose whole design was
/// that the person who most needed it was the one who did not know it existed.
///
/// The message answers WHICH backend, WHERE it was dialled (password removed -
/// see [`redact_url`]), and WHAT COMMAND provisions one.
#[track_caller]
pub fn redis_unreachable(url: &str, error: &dyn std::fmt::Display) -> ! {
    panic!(
        "Redis is unreachable, and this test requires it.\n\
         \n\
         \x20 backend: Redis (single node)\n\
         \x20 dialled: {}\n\
         \x20 error:   {error}\n\
         \n\
         Provision it, then re-run:\n\
         \x20 tests/provision_test_backends.sh\n\
         \n\
         That brings up the `postgres` and `redis` services from\n\
         deploy/compose/docker-compose.yml and waits for both to be healthy.\n\
         Point the tests somewhere else with REDIS_TEST_URL.\n\
         \n\
         There is no environment variable that makes this a skip. A Redis this\n\
         suite cannot reach is a failed run, not a green one.",
        redact_url(url)
    )
}

/// Open a client, or fail with the message above.
pub async fn connect(url: &str) -> Client {
    match Client::connect(url).await {
        Ok(client) => client,
        Err(e) => redis_unreachable(url, &e),
    }
}

/// Open a pool, or fail with the message above.
pub async fn connect_pool(url: &str, size: usize) -> Pool {
    match Pool::connect(url, size).await {
        Ok(pool) => pool,
        Err(e) => redis_unreachable(url, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn redaction_removes_the_password_and_keeps_everything_else() {
        assert_eq!(
            redact_url("redis://default:hunter2@127.0.0.1:6390"),
            "redis://default:***@127.0.0.1:6390"
        );
    }

    /// The one-variable partner: a URL with no password must come back
    /// unchanged, or the "redacted" claim is really "mangled". The DEFAULT URL
    /// has no userinfo at all, so this is the common case, not the exotic one.
    #[test]
    fn redaction_leaves_a_url_without_a_password_alone() {
        assert_eq!(
            redact_url("redis://127.0.0.1:6390"),
            "redis://127.0.0.1:6390"
        );
        assert_eq!(
            redact_url("redis://default@127.0.0.1:6390"),
            "redis://default@127.0.0.1:6390"
        );
    }

    /// A password containing `@` must not push the split point left and leak
    /// its tail. Taking the FIRST `@` would print `p***ss@word@127.0.0.1`.
    #[test]
    fn redaction_handles_an_at_sign_inside_the_password() {
        assert_eq!(
            redact_url("redis://u:p@ss@127.0.0.1:6390/0"),
            "redis://u:***@127.0.0.1:6390/0"
        );
    }
}
