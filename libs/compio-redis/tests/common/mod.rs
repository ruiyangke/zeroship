//! The two refusals this crate's test targets use in place of a skip: one for
//! the single-node Redis, one for the three-node Dragonfly cluster.
//!
//! THERE IS NO SKIP ANNOUNCER HERE ANY MORE, and no `ZEROSHIP-TEST-SKIPPED`
//! marker. The marker was a deliberate copy of a workspace helper, kept
//! byte-identical so that one search over a suite log would find every
//! announced no-op on either side of the `libs/` boundary. Its consumer was a
//! shell census, and both the census and the helper are gone: an absent backend
//! is now a failed run rather than a line somebody has to be reading.
//!
//! `compio-redis` is a standalone, publishable library with no zeroship
//! dependency, and it does not grow one for a test helper. That property is
//! what made the copy necessary and is unaffected by its deletion - these
//! panics name only this crate's own backends and read no configuration.

// Shared by two test targets that use DIFFERENT subsets of it: `cluster.rs`
// calls `cluster_seeds_unset` and never `connect_pool`, `integration.rs` the
// reverse. Cargo compiles this module once per target, so each build
// legitimately sees the other's half as dead. The alternative is splitting one
// small helper across two files to satisfy a lint.
#![allow(dead_code)]

pub mod env;

use compio_redis::{Client, Pool};

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

/// Fail the calling test because the Dragonfly CLUSTER was never named.
///
/// A different topology from the single-node Redis above, provisioned by a
/// different pair of commands, so it gets its own refusal rather than sharing
/// [`redis_unreachable`]. This used to announce a skip - the last such
/// announcement in this crate, and the reason `cluster.rs` could report every
/// one of its tests as passing against no cluster at all.
///
/// BOTH COMMANDS BELOW ARE REQUIRED, and the second is the one that gets
/// dropped: a Dragonfly node started in cluster mode comes up with no slot map
/// and answers nothing until the bootstrap script pushes one. `up -d` alone
/// leaves a cluster that is running and useless.
#[track_caller]
pub fn cluster_seeds_unset() -> ! {
    panic!(
        "No Dragonfly cluster was named, and this test requires one.\n\
         \n\
         \x20 backend: Dragonfly (three-node cluster)\n\
         \x20 missing: DRAGONFLY_CLUSTER_SEEDS\n\
         \n\
         This is NOT the single-node Redis that tests/provision_test_backends.sh\n\
         brings up; that script does not stand up a cluster. Provision one:\n\
         \x20 docker compose -f deploy/compose/cluster.yml up -d\n\
         \x20 ./deploy/scripts/bootstrap-dragonfly-cluster.sh\n\
         \n\
         The second command is not optional. A node started in cluster mode\n\
         ships with no slot map and serves nothing until it is pushed one, so\n\
         `up -d` on its own leaves a cluster that is up and cannot answer.\n\
         \n\
         Then re-run with the seeds:\n\
         \x20 DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \\\n\
         \x20   cargo test -p compio-redis --test cluster\n\
         \n\
         There is no environment variable that makes this a skip. A cluster\n\
         this suite cannot reach is a failed run, not a green one."
    )
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
