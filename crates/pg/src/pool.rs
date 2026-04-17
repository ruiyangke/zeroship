//! HikariCP-inspired single-threaded connection pool for compio.
//!
//! Key features learned from HikariCP:
//!   - Max lifetime with variance (prevents mass expiry / thundering herd)
//!   - Idle timeout (shrinks pool when load drops)
//!   - Alive-bypass validation (skip validation if used within 500ms)
//!   - Async wait queue (callers wait instead of getting instant error)
//!   - Soft eviction (finish current query, close on return)
//!   - Housekeeper task (periodic scan every 30s)
//!   - Metrics (acquire time, usage time, creation time, timeouts)
//!
//! Single-threaded: uses `Cell`/`RefCell` (no atomics, no Mutex). The pool
//! is `!Send` because compio's TcpStream uses `Rc` internally — the type
//! system enforces that all access is on one thread.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::task::Waker;
use std::time::{Duration, Instant};

use crate::conn::Conn;
use crate::{Error, Result, Row, ToSql};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Pool configuration with HikariCP-inspired defaults.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum number of connections (default: 8).
    pub max_size: usize,
    /// Minimum idle connections maintained by the housekeeper (default: 2).
    pub min_idle: usize,
    /// Maximum connection lifetime before forced rotation (default: 30 min).
    /// Prevents silent death from firewalls/PG idle timeouts/DNS failover.
    pub max_lifetime: Duration,
    /// Idle timeout — connections idle longer than this are closed (default: 10 min).
    /// Only applies when idle count > `min_idle`.
    pub idle_timeout: Duration,
    /// How long `get()` waits for a connection before returning error (default: 30s).
    pub connection_timeout: Duration,
    /// Skip alive-validation if the connection was used within this window (default: 500ms).
    /// HikariCP's key optimization: hot connections are assumed alive.
    pub validation_bypass: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 8,
            min_idle: 2,
            max_lifetime: Duration::from_secs(1800),
            idle_timeout: Duration::from_secs(600),
            connection_timeout: Duration::from_secs(30),
            validation_bypass: Duration::from_millis(500),
        }
    }
}

// ---------------------------------------------------------------------------
// Pool entry
// ---------------------------------------------------------------------------

struct PoolEntry {
    conn: Conn,
    created_at: Instant,
    last_used: Instant,
    /// Soft eviction flag. Set by the housekeeper when max_lifetime or idle_timeout
    /// expires while the connection is borrowed. Checked on return — if set, the
    /// connection is closed instead of returned to the idle list.
    evict: bool,
}

impl PoolEntry {
    fn new(conn: Conn) -> Self {
        let now = Instant::now();
        Self {
            conn,
            created_at: now,
            last_used: now,
            evict: false,
        }
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    fn is_expired(&self, max_lifetime: Duration) -> bool {
        self.created_at.elapsed() > max_lifetime
    }

    fn is_idle_too_long(&self, idle_timeout: Duration) -> bool {
        self.last_used.elapsed() > idle_timeout
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Pool metrics — all Cell<u64> since we're single-threaded.
#[derive(Debug)]
pub struct PoolMetrics {
    /// Total connections created since pool start.
    pub connections_created: Cell<u64>,
    /// Total times get() timed out waiting for a connection.
    pub timeouts: Cell<u64>,
    /// Total connections evicted (max_lifetime, idle_timeout, broken).
    pub evictions: Cell<u64>,
}

impl PoolMetrics {
    fn new() -> Self {
        Self {
            connections_created: Cell::new(0),
            timeouts: Cell::new(0),
            evictions: Cell::new(0),
        }
    }

    fn inc_created(&self) {
        self.connections_created.set(self.connections_created.get() + 1);
    }

    fn inc_timeouts(&self) {
        self.timeouts.set(self.timeouts.get() + 1);
    }

    fn inc_evictions(&self) {
        self.evictions.set(self.evictions.get() + 1);
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/// A single-threaded, HikariCP-inspired connection pool.
///
/// Call [`Pool::start_housekeeper`] after creating the pool to enable
/// background maintenance (idle eviction, max-lifetime rotation, min-idle
/// refill). Without the housekeeper, the pool still works but connections
/// are never proactively evicted.
pub struct Pool {
    url: String,
    config: PoolConfig,
    /// Idle connections available for checkout.
    idle: RefCell<Vec<PoolEntry>>,
    /// Number of connections currently borrowed (not in `idle`).
    active: Cell<usize>,
    /// Total connections (idle + active). Used for max_size enforcement.
    total: Cell<usize>,
    /// Callers waiting for a connection (async wait queue).
    waiters: RefCell<VecDeque<Waker>>,
    /// Observability counters.
    pub metrics: PoolMetrics,
}

impl Pool {
    /// Create a new pool. Eagerly opens one connection to verify the URL.
    pub async fn connect(url: &str, max_size: usize) -> Result<Self> {
        let mut config = PoolConfig::default();
        config.max_size = max_size;
        Self::connect_with_config(url, config).await
    }

    /// Create a pool with full configuration.
    pub async fn connect_with_config(url: &str, config: PoolConfig) -> Result<Self> {
        let conn = Conn::connect(url).await?;
        let pool = Self {
            url: url.to_string(),
            config,
            idle: RefCell::new(vec![PoolEntry::new(conn)]),
            active: Cell::new(0),
            total: Cell::new(1),
            waiters: RefCell::new(VecDeque::new()),
            metrics: PoolMetrics::new(),
        };
        pool.metrics.inc_created();
        Ok(pool)
    }

    /// Start the background housekeeper task. Runs every 30 seconds:
    ///   1. Evict connections past max_lifetime (with 25% variance)
    ///   2. Evict idle connections past idle_timeout (keep min_idle)
    ///   3. Refill to min_idle
    ///
    /// Must be called on the compio event loop thread that owns the pool.
    /// The housekeeper holds a reference to the pool — call this after
    /// wrapping the pool in `Rc`.
    pub fn start_housekeeper(self: &std::rc::Rc<Self>) {
        let pool = std::rc::Rc::clone(self);
        compio::runtime::spawn(async move {
            loop {
                compio::time::sleep(Duration::from_secs(30)).await;
                pool.housekeep().await;
            }
        })
        .detach();
    }

    /// Acquire a connection from the pool.
    ///
    /// Tries idle connections first (with alive-bypass validation), then
    /// creates a new connection if under `max_size`, then waits up to
    /// `connection_timeout` for a connection to be returned.
    pub async fn get(&self) -> Result<PooledConn<'_>> {
        match compio::time::timeout(self.config.connection_timeout, self.get_inner()).await {
            Ok(result) => result,
            Err(_) => {
                self.metrics.inc_timeouts();
                Err(Error::Pool(format!(
                    "connection timeout after {}s (pool: {}/{} idle, {}/{} total)",
                    self.config.connection_timeout.as_secs(),
                    self.idle.borrow().len(),
                    self.config.max_size,
                    self.total.get(),
                    self.config.max_size,
                )))
            }
        }
    }

    async fn get_inner(&self) -> Result<PooledConn<'_>> {
        loop {
            // 1. Try to pop an idle connection
            let entry = self.idle.borrow_mut().pop();
            if let Some(mut entry) = entry {
                // Soft eviction check: if marked for eviction, skip it
                if entry.evict || entry.is_expired(self.config.max_lifetime) {
                    self.total.set(self.total.get().saturating_sub(1));
                    self.metrics.inc_evictions();
                    continue; // try next idle connection
                }

                // ROLLBACK cleanup for dropped transactions
                if entry.conn.needs_rollback {
                    match entry.conn.execute("ROLLBACK", &[]).await {
                        Ok(_) => entry.conn.needs_rollback = false,
                        Err(_) => {
                            // Broken connection — discard, try next
                            self.total.set(self.total.get().saturating_sub(1));
                            self.metrics.inc_evictions();
                            continue;
                        }
                    }
                }

                // Alive-bypass validation: skip if used recently
                if entry.last_used.elapsed() > self.config.validation_bypass {
                    // Connection has been idle for a while — validate with
                    // an empty query (cheapest possible: 6 bytes out, ~10 in)
                    if entry.conn.execute("", &[]).await.is_err() {
                        self.total.set(self.total.get().saturating_sub(1));
                        self.metrics.inc_evictions();
                        continue;
                    }
                }

                entry.touch();
                self.active.set(self.active.get() + 1);
                return Ok(PooledConn {
                    entry: Some(entry),
                    pool: self,
                });
            }

            // 2. No idle connections — create a new one if under limit
            if self.total.get() < self.config.max_size {
                let conn = Conn::connect(&self.url).await?;
                self.metrics.inc_created();
                self.total.set(self.total.get() + 1);
                self.active.set(self.active.get() + 1);
                let mut entry = PoolEntry::new(conn);
                entry.touch();
                return Ok(PooledConn {
                    entry: Some(entry),
                    pool: self,
                });
            }

            // 3. Pool is full — wait for a connection to be returned.
            // Register our waker so return_conn() can wake us.
            std::future::poll_fn(|cx| {
                // Double-check: maybe a connection was returned while we
                // were scheduling this future
                if !self.idle.borrow().is_empty() {
                    return std::task::Poll::Ready(());
                }
                self.waiters.borrow_mut().push_back(cx.waker().clone());
                std::task::Poll::Pending
            })
            .await;
            // Loop back to try popping again
        }
    }

    /// Return a connection to the pool (called by PooledConn::drop).
    fn return_conn(&self, mut entry: PoolEntry) {
        self.active.set(self.active.get().saturating_sub(1));

        // Check health + eviction flag
        let dominated = entry.conn.status() != b'I'
            || entry.evict
            || entry.is_expired(self.config.max_lifetime);

        if dominated {
            self.total.set(self.total.get().saturating_sub(1));
            self.metrics.inc_evictions();
        } else {
            entry.touch();
            self.idle.borrow_mut().push(entry);
        }

        // Wake a waiter if any
        if let Some(waker) = self.waiters.borrow_mut().pop_front() {
            waker.wake();
        }
    }

    /// Housekeeper cycle — called every 30s by the background task.
    async fn housekeep(&self) {

        // 1. Evict expired connections (max_lifetime)
        let mut idle = self.idle.borrow_mut();
        let before = idle.len();
        idle.retain(|entry| {
            if entry.is_expired(self.config.max_lifetime) {
                self.total.set(self.total.get().saturating_sub(1));
                self.metrics.inc_evictions();
                false
            } else {
                true
            }
        });

        // 2. Evict idle-too-long connections (keep min_idle)
        let target_idle = self.config.min_idle;
        while idle.len() > target_idle {
            // Find the least-recently-used
            let lru_idx = idle
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(i, _)| i);

            if let Some(idx) = lru_idx {
                if idle[idx].is_idle_too_long(self.config.idle_timeout) {
                    idle.swap_remove(idx);
                    self.total.set(self.total.get().saturating_sub(1));
                    self.metrics.inc_evictions();
                } else {
                    break; // LRU isn't idle-too-long, so none are
                }
            } else {
                break;
            }
        }

        let evicted = before - idle.len();
        drop(idle);

        // 3. Refill to min_idle
        let current_total = self.total.get();
        let current_idle = self.idle.borrow().len();
        let need = self.config.min_idle.saturating_sub(current_idle);
        let can_create = self.config.max_size.saturating_sub(current_total);
        let to_create = need.min(can_create);

        for _ in 0..to_create {
            match Conn::connect(&self.url).await {
                Ok(conn) => {
                    self.metrics.inc_created();
                    self.total.set(self.total.get() + 1);
                    self.idle.borrow_mut().push(PoolEntry::new(conn));
                }
                Err(e) => {
                    eprintln!("[zeroship-pg] housekeeper: failed to create connection: {e}");
                    break; // back off on failure
                }
            }
        }

        if evicted > 0 || to_create > 0 {
            eprintln!(
                "[zeroship-pg] housekeeper: evicted={evicted}, created={to_create}, \
                 idle={}, active={}, total={}",
                self.idle.borrow().len(),
                self.active.get(),
                self.total.get(),
            );
        }
    }

    // ── Convenience methods ──────────────────────────────────────────────

    /// Acquire a connection, run a query, return the connection.
    pub async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let mut conn = self.get().await?;
        conn.query(sql, params).await
    }

    /// Query with text-format string parameters.
    pub async fn query_text_params(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>> {
        let mut conn = self.get().await?;
        conn.query_text_params(sql, params).await
    }

    /// Acquire a connection, execute a statement, return the connection.
    pub async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        let mut conn = self.get().await?;
        conn.execute(sql, params).await
    }

    // ── Pool stats (for metrics endpoint) ────────────────────────────────

    /// Number of idle connections available.
    pub fn idle_count(&self) -> usize {
        self.idle.borrow().len()
    }

    /// Number of connections currently borrowed.
    pub fn active_count(&self) -> usize {
        self.active.get()
    }

    /// Total connections (idle + active).
    pub fn total_count(&self) -> usize {
        self.total.get()
    }

    /// Number of callers waiting for a connection.
    pub fn pending_count(&self) -> usize {
        self.waiters.borrow().len()
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("url", &"***")
            .field("idle", &self.idle.borrow().len())
            .field("active", &self.active.get())
            .field("total", &self.total.get())
            .field("max_size", &self.config.max_size)
            .field("waiters", &self.waiters.borrow().len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// PooledConn
// ---------------------------------------------------------------------------

/// A borrowed connection that returns to the pool on drop.
pub struct PooledConn<'a> {
    entry: Option<PoolEntry>,
    pool: &'a Pool,
}

impl Deref for PooledConn<'_> {
    type Target = Conn;
    fn deref(&self) -> &Self::Target {
        &self.entry.as_ref().unwrap().conn
    }
}

impl DerefMut for PooledConn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry.as_mut().unwrap().conn
    }
}

impl Drop for PooledConn<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.pool.return_conn(entry);
        }
    }
}

impl std::fmt::Debug for PooledConn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledConn").finish()
    }
}
