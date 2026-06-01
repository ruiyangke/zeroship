//! Per-worker-thread Postgres connection pool for the gateway.
//!
//! The compio-postgres [`Pool`] is single-threaded by design: it uses
//! `Rc`/`Cell`/`RefCell` internally (compio's `TcpStream` is `!Send`), so
//! it is `!Send + !Sync`. The gateway runs ntex's multi-worker HTTP
//! server, where the `Arc<GateState>` is shared across worker *arbiter*
//! threads — which means a `Pool` cannot live in `GateState`.
//!
//! We solve this the same way the sandbox controller handles its
//! per-compio-worker pool (`crates/sandbox/src/db.rs`): the shared state
//! carries only the `Send + Sync` connection *parameters* ([`DbConfig`]),
//! and the actual [`Pool`] is built lazily **per worker thread**, wrapped
//! in an [`Rc`], and stashed in a thread-local. The first DB touch on a
//! given thread opens the pool's warm connections and starts its
//! housekeeper; every subsequent touch on that thread reuses it.
//!
//! Each per-request DB operation [`checkout`]s this thread's `Rc<Pool>`
//! and then `.get()`s a [`PooledClient`] from it for exactly ONE
//! operation, releasing the connection on drop — so no single shared
//! connection serializes gateway DB work, which was the whole point of
//! the pool migration. Hold the `PooledClient` only across the single DB
//! call; the `Rc<Pool>` it borrows must stay in scope alongside it.
//!
//! Unlike a leak-to-`'static` design, the `Rc<Pool>` shape reclaims a
//! race-loser pool cleanly: on a single compio worker thread, two
//! concurrent first-checkouts can both observe `None` and both connect,
//! but [`install_pool`] keeps the first-installed pool and the loser's
//! `Rc` simply drops — its housekeeper's `Weak` upgrades to `None` on the
//! next tick and the task exits. Nothing is orphaned for the process
//! lifetime.

use std::cell::RefCell;
use std::rc::Rc;

use compio_postgres::{Error, Pool};

/// `Send + Sync` connection parameters stored in `GateState.db`.
///
/// The real [`Pool`] is `!Send`, so it cannot be shared across ntex
/// worker threads; this struct carries everything needed to build a
/// per-thread pool on demand.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// PostgreSQL DSN (carries the password — never logged).
    dsn: String,
    /// Maximum pooled connections per worker thread.
    pool_size: usize,
}

impl DbConfig {
    /// Build a config. `pool_size` is clamped to at least 1.
    #[must_use]
    pub fn new(dsn: impl Into<String>, pool_size: usize) -> Self {
        Self {
            dsn: dsn.into(),
            pool_size: pool_size.max(1),
        }
    }

    /// Maximum pooled connections per worker thread.
    #[must_use]
    pub const fn pool_size(&self) -> usize {
        self.pool_size
    }
}

thread_local! {
    /// One pool per worker thread, keyed by DSN. `Rc<Pool>` (not a
    /// leak-to-`'static`) so a race-loser pool — built when two
    /// first-checkouts on the same compio thread both observe `None`
    /// before either installs — drops cleanly instead of being orphaned
    /// for the process lifetime. The DSN tiebreaker lets a test fixture
    /// that rotates DSNs mid-process evict a stale pool rather than
    /// return it. Mirrors `crates/sandbox/src/db.rs`.
    static POOL: RefCell<Option<(String, Rc<Pool>)>> = const { RefCell::new(None) };
}

/// Try a thread-local cache read. Returns `Some(Rc<Pool>)` if the cached
/// entry matches `dsn`; `None` otherwise.
fn cached_pool(dsn: &str) -> Option<Rc<Pool>> {
    POOL.with(|c| match &*c.borrow() {
        Some((cached_dsn, pool)) if cached_dsn == dsn => Some(Rc::clone(pool)),
        _ => None,
    })
}

/// Install a freshly-built pool into the thread-local cache. If a
/// concurrent task already installed one for the same DSN (post-await
/// race), drop the local build and return the cached entry — its
/// housekeeper's `Weak` upgrades to `None` next tick once the loser `Rc`
/// drops. If the cached entry is for a different DSN (test fixture
/// rotation), evict and replace.
fn install_pool(dsn: String, pool: Rc<Pool>) -> Rc<Pool> {
    POOL.with(|c| {
        let mut borrow = c.borrow_mut();
        if let Some((cached_dsn, cached_pool)) = borrow.as_ref() {
            if cached_dsn == &dsn {
                // Lost the race — caller's local build drops on return.
                return Rc::clone(cached_pool);
            }
            // DSN mismatch — fall through to replace.
        }
        let out = Rc::clone(&pool);
        *borrow = Some((dsn, pool));
        out
    })
}

/// Get (or lazily build) this thread's pool and return an `Rc<Pool>`
/// clone to check a connection out of.
///
/// Callers bind the returned `Rc<Pool>` and then `pool.get().await?` for
/// one operation; the resulting [`PooledClient`](compio_postgres::PooledClient)
/// returns its connection to the pool on drop. Hold the `PooledClient`
/// only across the single DB call, never across an outbound HTTP request,
/// and keep the `Rc<Pool>` in scope for as long as the `PooledClient`
/// borrows it.
///
/// The `Rc<Pool>` shape (no leak-to-`'static`) mirrors the sandbox
/// controller (`crates/sandbox/src/db.rs`): a race-loser pool drops
/// cleanly, and the pool's housekeeper handles idle eviction /
/// `max_lifetime` rotation / `min_idle` refill.
///
/// # Errors
///
/// Returns the underlying [`Error`] if the per-thread pool cannot be
/// opened (first use only).
pub async fn checkout(cfg: &DbConfig) -> Result<Rc<Pool>, Error> {
    if let Some(pool) = cached_pool(&cfg.dsn) {
        return Ok(pool);
    }
    // First DB touch on this worker thread (or after a DSN rotation):
    // open the pool and start its housekeeper so idle conns are
    // proactively evicted, `max_lifetime` rotation happens, and
    // `min_idle` is refilled. Without the housekeeper the pool retains
    // connections unbounded up to `max_size` per worker and never detects
    // PG-side idle timeouts / failover until the next checkout — see
    // `crates/compio-postgres/src/pool.rs` ("Without the housekeeper, the
    // pool still works but connections are never proactively evicted").
    // Called on the locally-built pool before `install_pool` so the
    // (rare) race-loser pool also gets a housekeeper that self-terminates
    // next tick once the loser `Rc` drops.
    let pool = Rc::new(Pool::connect(&cfg.dsn, cfg.pool_size).await?);
    pool.start_housekeeper();
    Ok(install_pool(cfg.dsn.clone(), pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_config_clamps_pool_size_to_one() {
        let cfg = DbConfig::new("postgres://x", 0);
        assert_eq!(cfg.pool_size(), 1);
        let cfg = DbConfig::new("postgres://x", 32);
        assert_eq!(cfg.pool_size(), 32);
    }

    #[test]
    fn db_config_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DbConfig>();
    }
}
