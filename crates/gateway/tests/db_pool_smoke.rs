//! Live-PG smoke test for the gateway's per-worker connection pool
//! (`zeroship_gateway::db`).
//!
//! Skipped silently unless `GATEWAY_POOL_SMOKE_URL` is set (same env-skip
//! convention as the rest of the gateway PG tests). Needs no `auth`
//! schema — it runs a trivial `SELECT 1`, so any reachable Postgres works.
//!
//! Coverage:
//!   - `db::checkout` lazily builds the per-thread pool on first use and
//!     returns the per-thread `Rc<Pool>` to check connections out of.
//!   - N concurrent `pool.get()` checkouts each run a trivial query and
//!     return the right answer — exercising the pool handing out multiple
//!     distinct connections rather than serializing through one.
//!   - Connections are released on drop, so a second wave of checkouts
//!     after the first wave drops reuses the pool without exhausting it.
//!   - Repeated `db::checkout` calls return the SAME cached `Rc<Pool>`
//!     (no per-call reconnect), and the returned `Rc` is shared.

use zeroship_gateway::db::DbConfig;

#[compio::test]
async fn pool_checkout_runs_concurrent_trivial_queries() {
    let Ok(dsn) = std::env::var("GATEWAY_POOL_SMOKE_URL") else {
        eprintln!("skipping (no GATEWAY_POOL_SMOKE_URL)");
        return;
    };

    // Pool of 8; check out 6 connections "at once" (hold them all live),
    // run a trivial query on each, assert the answers. Holding all six
    // simultaneously forces the pool to materialise six distinct
    // connections — a single shared connection could not satisfy this.
    let cfg = DbConfig::new(dsn, 8);

    let pool = zeroship_gateway::db::checkout(&cfg).await.expect("checkout");
    // A second checkout must return the SAME per-thread pool (cached),
    // not a freshly reconnected one — that is the whole point of the
    // thread-local. `Rc::ptr_eq` proves it is the identical allocation.
    let pool2 = zeroship_gateway::db::checkout(&cfg)
        .await
        .expect("checkout (cache)");
    assert!(
        std::rc::Rc::ptr_eq(&pool, &pool2),
        "repeated checkout must reuse the cached per-thread pool"
    );
    drop(pool2);

    let mut guards = Vec::new();
    for i in 0..6i32 {
        let conn = pool.get().await.expect("pool.get");
        let rows = conn
            .query("SELECT $1::int4 AS n", &[&i])
            .await
            .expect("query");
        let n: i32 = rows[0].get("n");
        assert_eq!(n, i, "each pooled conn must return its own bound value");
        guards.push(conn);
    }
    // All six are still checked out here. Drop them — connections return
    // to the per-thread pool.
    drop(guards);

    // Second wave after release: the pool must hand connections back out
    // without re-exhausting (proves release-on-drop works).
    for i in 0..6i32 {
        let conn = pool.get().await.expect("pool.get (second wave)");
        let rows = conn
            .query("SELECT $1::int4 AS n", &[&i])
            .await
            .expect("query (second wave)");
        let n: i32 = rows[0].get("n");
        assert_eq!(n, i);
    }
}
