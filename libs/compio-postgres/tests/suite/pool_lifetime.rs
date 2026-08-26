//! Which pool lifetime settings work WITHOUT the housekeeper, and which do not.
//!
//! `Pool`'s own documentation lists "idle eviction, max-lifetime rotation,
//! min-idle refill" together as background maintenance, which reads as though
//! none of them happens until [`Pool::start_housekeeper`] is called. Measured,
//! that is not so:
//!
//! * `max_lifetime` IS enforced without a housekeeper, because an expired
//!   entry is discarded when it is RETURNED.
//! * `idle_timeout` is NOT - eviction lives only in the sweep.
//!
//! The asymmetry is the point. A caller who sets both, sees connections
//! rotating on schedule, and concludes the pool is honouring their settings
//! would be half right, with no signal about which half. These tests pin both
//! sides so the documentation cannot drift away from the behaviour.

#[allow(unused_imports)]
use crate::common;
use common::test_url;
use compio_postgres::{Config, Pool, PoolConfig, PooledClient};
use std::time::Duration;

/// The backend serving this lease, which identifies the physical connection.
async fn backend_pid(lease: &PooledClient<'_>) -> i32 {
    lease
        .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
        .await
        .expect("read the backend pid")
}

async fn single_connection_pool(configure: impl FnOnce(&mut PoolConfig)) -> Pool {
    let config: Config = test_url().parse().expect("the suite DSN parses");
    let mut pool_config = PoolConfig::new();
    pool_config
        .max_size(1)
        .min_idle(0)
        .acquire_timeout(Duration::from_secs(5));
    configure(&mut pool_config);
    Pool::connect_with_config(config, pool_config)
        .await
        .expect("build the pool")
}

/// NO housekeeper is started anywhere in this file. That is the condition
/// under test, not an oversight.
#[compio::test]
async fn max_lifetime_rotates_without_a_housekeeper() {
    let pool = single_connection_pool(|config| {
        config.max_lifetime(Duration::from_millis(600));
    })
    .await;

    let first = backend_pid(&pool.get().await.expect("first checkout")).await;

    // Well inside the lifetime: the same physical connection comes back.
    let reused = backend_pid(&pool.get().await.expect("second checkout")).await;
    assert_eq!(
        reused, first,
        "the pool opened a new connection while the first was still young, so \
         the rotation below would prove nothing"
    );

    compio::time::sleep(Duration::from_millis(900)).await;

    let after = backend_pid(&pool.get().await.expect("checkout after expiry")).await;
    assert_ne!(
        after, first,
        "a connection past max_lifetime was handed out again; expiry is \
         checked when an entry is returned, so it should not need the sweep"
    );
}

/// The other half. With the same shape and no housekeeper, an idle connection
/// well past `idle_timeout` is still handed straight back out.
///
/// This asserts the CURRENT contract rather than a wish: idle eviction is
/// background work. If that ever changes, this test is where the change gets
/// noticed, and the `idle_timeout` documentation has to change with it.
#[compio::test]
async fn idle_timeout_does_nothing_without_a_housekeeper() {
    let pool = single_connection_pool(|config| {
        config.idle_timeout(Duration::from_millis(300));
        // Long enough that lifetime rotation cannot be what happens here.
        config.max_lifetime(Duration::from_secs(3600));
    })
    .await;

    let first = backend_pid(&pool.get().await.expect("first checkout")).await;
    assert_eq!(pool.idle_count(), 1, "the connection returned to the pool");

    compio::time::sleep(Duration::from_millis(900)).await;

    assert_eq!(
        pool.idle_count(),
        1,
        "an idle connection was evicted with no housekeeper running"
    );
    let after = backend_pid(&pool.get().await.expect("checkout after idling")).await;
    assert_eq!(
        after, first,
        "the pool replaced an idle connection without a housekeeper; if this \
         is now intended, PoolConfig::idle_timeout's documentation is wrong"
    );
}
