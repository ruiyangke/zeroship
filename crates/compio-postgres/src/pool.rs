// Ported from zeroship-pg's HikariCP-inspired pool (MIT/Apache-2.0).
// Adapted to manage tokio-postgres-style `(Client, Connection)` pairs
// where the Connection runs as a detached compio task.

//! HikariCP-inspired single-threaded connection pool for compio-postgres.
//!
//! Key features learned from HikariCP:
//!   - Max lifetime with 25% variance (prevents mass expiry / thundering herd)
//!   - Idle timeout (shrinks pool when load drops)
//!   - Alive-bypass validation (skip validation if used within 500ms)
//!   - Async wait queue (callers wait instead of getting instant error)
//!   - Housekeeper task (periodic scan every 30s)
//!   - Metrics (acquire time, usage time, creation time, timeouts)
//!
//! Single-threaded: uses `Cell`/`RefCell` (no atomics, no Mutex). The pool
//! is `!Send` because compio's TcpStream uses `Rc` internally — the type
//! system enforces that all access is on one thread.
//!
//! Connection lifecycle
//! --------------------
//! Each pool entry owns one [`Client`]. When the entry is created, the pool
//! also [`compio::runtime::spawn`]s the matching [`Connection::run`] task and
//! detaches it. The task runs as long as the Client's sender is alive; when
//! the Client is dropped (on pool eviction) the sender closes, the task's
//! receiver observes `None`, and the task terminates gracefully.
//!
//! On [`PooledClient::drop`], the entry is returned to the pool (or evicted
//! if the connection is closed or past `max_lifetime`). There is no separate
//! `needs_rollback` flag — tokio-postgres's [`Transaction`] handles rollback
//! via Drop without any pool assistance.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::tls::NoTls;
use crate::{Client, Connection, Error, Socket};

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
    /// Each entry gets a ±25% jitter so expiries are staggered.
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
// Lightweight random (xorshift64*) for lifetime jitter.
//
// Seeded once per thread from the system clock — good enough for load
// spreading, not for security. Avoids pulling in `rand`.
// ---------------------------------------------------------------------------

thread_local! {
    static JITTER_RNG: Cell<u64> = Cell::new({
        // Seed from the system clock (wall-clock nanos) — good enough for
        // spreading expiries across a pool. Falls back to a constant if the
        // system clock is before UNIX_EPOCH or mod gives 0.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mixed = nanos
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
            .wrapping_mul(0xBF58_476D_1CE4_E5B9);
        if mixed == 0 { 0xDEAD_BEEF_CAFE_BABE } else { mixed }
    });
}

fn next_rand_u64() -> u64 {
    JITTER_RNG.with(|c| {
        let mut x = c.get();
        if x == 0 {
            x = 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.set(x);
        x
    })
}

/// Return a f64 in [0.0, 1.0).
fn next_rand_f64() -> f64 {
    // Use the top 53 bits to match f64 mantissa precision.
    (next_rand_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// Compute jittered lifetime: ±25% of `base`.
fn jittered_lifetime(base: Duration) -> Duration {
    let factor = 0.75 + 0.5 * next_rand_f64();
    base.mul_f64(factor)
}

// ---------------------------------------------------------------------------
// Pool entry
// ---------------------------------------------------------------------------

struct PoolEntry {
    client: Client,
    last_used: Instant,
    /// Wall-clock moment at which this connection must be rotated.
    /// Computed once at creation as `now + max_lifetime * (0.75..=1.25)`.
    expiry: Instant,
}

impl PoolEntry {
    fn new(client: Client, max_lifetime: Duration) -> Self {
        let now = Instant::now();
        Self {
            client,
            last_used: now,
            expiry: now + jittered_lifetime(max_lifetime),
        }
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.expiry
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
    /// Total times `get()` timed out waiting for a connection.
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
        self.connections_created
            .set(self.connections_created.get() + 1);
    }

    fn inc_timeouts(&self) {
        self.timeouts.set(self.timeouts.get() + 1);
    }

    fn inc_evictions(&self) {
        self.evictions.set(self.evictions.get() + 1);
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Wrap a pool-layer failure in `Error`. We synthesize an `io::Error` and
/// route it through `Error::connect` (which carries a user-facing kind).
fn pool_error(msg: impl Into<String>) -> Error {
    Error::connect(io::Error::other(msg.into()))
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
    /// Callers waiting for a connection.
    ///
    /// `Option<Waker>` allows waiters to tombstone their slot on drop: a
    /// cancelled future (outer timeout, caller dropped) writes `None` into
    /// its own slot so `return_client` skips it. Without this the queue leaks
    /// dead wakers and live waiters starve.
    waiters: RefCell<VecDeque<Option<Waker>>>,
    /// Index offset of `waiters.front()` — grows monotonically as waiters
    /// are popped. A waiter's slot id minus `waiters_base` gives its index.
    waiters_base: Cell<u64>,
    /// Observability counters.
    pub metrics: PoolMetrics,
}

impl Pool {
    /// Create a new pool. Eagerly opens `min_idle` connections to verify
    /// the URL and warm the pool.
    pub async fn connect(url: &str, max_size: usize) -> Result<Self, Error> {
        let config = PoolConfig {
            max_size,
            ..PoolConfig::default()
        };
        Self::connect_with_config(url, config).await
    }

    /// Create a pool with full configuration.
    ///
    /// Opens `config.min_idle.max(1)` connections upfront so the first burst
    /// of traffic does not pay full connect latency. Each connection attempt
    /// is retried with exponential backoff (3 attempts: 100ms, 400ms, 1.6s)
    /// to survive Docker ordering, DNS blips, and brief PG restarts.
    pub async fn connect_with_config(url: &str, config: PoolConfig) -> Result<Self, Error> {
        let warm = config.min_idle.max(1);
        let mut entries: Vec<PoolEntry> = Vec::with_capacity(warm);
        for i in 0..warm {
            match Self::connect_with_retry(url).await {
                Ok(client) => entries.push(PoolEntry::new(client, config.max_lifetime)),
                Err(e) => {
                    // Drop any already-opened clients (Client drop closes the
                    // sender, the connection task observes it and exits).
                    drop(entries);
                    return Err(if i == 0 {
                        e
                    } else {
                        pool_error(format!(
                            "warm-up failed after {i} successful connection(s): {e}"
                        ))
                    });
                }
            }
        }

        let total = entries.len();
        let pool = Self {
            url: url.to_string(),
            config,
            idle: RefCell::new(entries),
            active: Cell::new(0),
            total: Cell::new(total),
            waiters: RefCell::new(VecDeque::new()),
            waiters_base: Cell::new(0),
            metrics: PoolMetrics::new(),
        };
        for _ in 0..total {
            pool.metrics.inc_created();
        }
        Ok(pool)
    }

    /// Retry [`connect_one`] up to 3 times with 100ms, 400ms, 1.6s backoff.
    /// Only used for pool warm-up — `get_inner`'s on-demand connect stays
    /// single-shot to keep the latency budget tight.
    async fn connect_with_retry(url: &str) -> Result<Client, Error> {
        let mut delay = Duration::from_millis(100);
        let mut last_err: Option<Error> = None;
        for attempt in 0..3 {
            match Self::connect_one(url).await {
                Ok(client) => return Ok(client),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 2 {
                        compio::time::sleep(delay).await;
                        delay *= 4;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| pool_error("connect retries exhausted")))
    }

    /// Open a single client + spawn its Connection task. Returns the Client;
    /// the task is detached and self-terminates when the Client's sender is
    /// closed (i.e. when the Client is dropped).
    async fn connect_one(url: &str) -> Result<Client, Error> {
        let (client, connection) = crate::connect(url, NoTls).await?;
        spawn_connection_task(connection);
        Ok(client)
    }

    /// Start the background housekeeper task. Runs every 30 seconds:
    ///   1. Evict connections past max_lifetime
    ///   2. Evict idle connections past idle_timeout (keep min_idle)
    ///   3. Refill to min_idle
    ///
    /// Must be called on the compio event loop thread that owns the pool.
    /// The housekeeper holds a `Weak<Pool>` so it does not keep the pool
    /// alive: when the last strong Rc drops, `upgrade()` returns `None` and
    /// the task exits.
    pub fn start_housekeeper(self: &std::rc::Rc<Self>) {
        let weak = std::rc::Rc::downgrade(self);
        compio::runtime::spawn(async move {
            loop {
                compio::time::sleep(Duration::from_secs(30)).await;
                match weak.upgrade() {
                    Some(pool) => pool.housekeep().await,
                    None => break,
                }
            }
        })
        .detach();
    }

    /// Acquire a connection from the pool.
    ///
    /// Tries idle connections first (with alive-bypass validation), then
    /// creates a new connection if under `max_size`, then waits up to
    /// `connection_timeout` for a connection to be returned.
    pub async fn get(&self) -> Result<PooledClient<'_>, Error> {
        match compio::time::timeout(self.config.connection_timeout, self.get_inner()).await {
            Ok(result) => result,
            Err(_) => {
                self.metrics.inc_timeouts();
                Err(pool_error(format!(
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

    async fn get_inner(&self) -> Result<PooledClient<'_>, Error> {
        loop {
            // 1. Try to pop an idle connection
            let entry = self.idle.borrow_mut().pop();
            if let Some(mut entry) = entry {
                if entry.is_expired() {
                    self.total.set(self.total.get().saturating_sub(1));
                    self.metrics.inc_evictions();
                    continue;
                }

                // If the Client's sender is closed (connection task exited
                // due to I/O error, EOF, etc), we cannot use this entry.
                if entry.client.is_closed() {
                    self.total.set(self.total.get().saturating_sub(1));
                    self.metrics.inc_evictions();
                    continue;
                }

                // Dirty barrier: if a Transaction::drop left a fire-and-forget
                // ROLLBACK on the wire, we must drain it before handing the
                // client out — otherwise the next caller could inherit a
                // broken-transaction state (if the ROLLBACK failed) or race
                // against in-flight messages. `simple_query("")` serializes
                // behind the pending ROLLBACK because the Connection
                // processes requests FIFO, so once it returns Ok the queued
                // ROLLBACK has completed. On error, the connection is
                // genuinely broken: evict it.
                //
                // Runs regardless of `validation_bypass` — a dirty
                // connection must be validated even if last_used is
                // extremely recent, because dirtiness is precisely the case
                // where "last_used was recent" is insufficient.
                if entry.client.is_dirty() {
                    match entry.client.simple_query("").await {
                        Ok(_) => entry.client.clear_dirty(),
                        Err(_) => {
                            self.total.set(self.total.get().saturating_sub(1));
                            self.metrics.inc_evictions();
                            continue;
                        }
                    }
                    // The barrier itself provides the alive-check; skip the
                    // second `simple_query("")` below.
                    entry.touch();
                    self.active.set(self.active.get() + 1);
                    return Ok(PooledClient {
                        entry: Some(entry),
                        pool: self,
                    });
                }

                // Alive-bypass validation: skip if used recently.
                // simple_query("") is a cheap no-op that round-trips to the
                // server; if the connection is dead, the task has already
                // exited and this will return Err.
                if entry.last_used.elapsed() > self.config.validation_bypass
                    && entry.client.simple_query("").await.is_err()
                {
                    self.total.set(self.total.get().saturating_sub(1));
                    self.metrics.inc_evictions();
                    continue;
                }

                entry.touch();
                self.active.set(self.active.get() + 1);
                return Ok(PooledClient {
                    entry: Some(entry),
                    pool: self,
                });
            }

            // 2. No idle connections — create a new one if under limit.
            // Reserve the slot synchronously *before* the await so concurrent
            // callers in the same loop see the bumped `total` and don't race
            // past `max_size`. Decrement on failure.
            if self.total.get() < self.config.max_size {
                self.total.set(self.total.get() + 1);
                let client = match Self::connect_one(&self.url).await {
                    Ok(c) => c,
                    Err(e) => {
                        self.total.set(self.total.get().saturating_sub(1));
                        // Wake a waiter so they don't stall behind our
                        // failed reservation until the outer timeout fires.
                        self.wake_one_waiter();
                        return Err(e);
                    }
                };
                self.metrics.inc_created();
                self.active.set(self.active.get() + 1);
                let mut entry = PoolEntry::new(client, self.config.max_lifetime);
                entry.touch();
                return Ok(PooledClient {
                    entry: Some(entry),
                    pool: self,
                });
                // H7 design note: we don't wake a waiter on successful
                // connect. The freshly-connected client is immediately
                // consumed by the current caller — there's no idle entry
                // for a waiter to acquire. Waiters are woken on
                // `return_client`, which is when a borrowed entry
                // actually becomes available.
            }

            // 3. Pool is full — wait for a connection to be returned.
            Waiter::new(self).await;
        }
    }

    /// Return a connection to the pool (called by `PooledClient::drop`).
    fn return_client(&self, mut entry: PoolEntry) {
        self.active.set(self.active.get().saturating_sub(1));

        // Eviction criteria:
        //   - expired (max_lifetime reached)
        //   - closed: Client::is_closed() indicates the connection task exited
        //
        // tokio-postgres's Transaction handles ROLLBACK-on-drop internally
        // via its own Drop impl — no `needs_rollback` flag is needed at the
        // pool layer.
        if entry.is_expired() || entry.client.is_closed() {
            self.total.set(self.total.get().saturating_sub(1));
            self.metrics.inc_evictions();
        } else {
            entry.touch();
            self.idle.borrow_mut().push(entry);
        }

        self.wake_one_waiter();
    }

    /// Wake the first live waiter in the queue, skipping tombstoned slots.
    fn wake_one_waiter(&self) {
        let mut waiters = self.waiters.borrow_mut();
        while let Some(slot) = waiters.pop_front() {
            self.waiters_base.set(self.waiters_base.get() + 1);
            if let Some(w) = slot {
                w.wake();
                return;
            }
        }
    }

    /// Housekeeper cycle — called every 30s by the background task.
    async fn housekeep(&self) {
        // Take counts and do all mutations inside tight borrows. Release
        // every borrow before awaiting.

        let (before, evicted_expired, evicted_idle) = {
            let mut idle = self.idle.borrow_mut();
            let before = idle.len();

            // 1. Evict expired (max_lifetime reached).
            let mut evicted_expired = 0usize;
            idle.retain(|entry| {
                if entry.is_expired() {
                    evicted_expired += 1;
                    false
                } else {
                    true
                }
            });

            // 2. Evict idle-too-long, keeping at least min_idle.
            let target = self.config.min_idle;
            let mut evicted_idle = 0usize;
            while idle.len() > target {
                let lru_idx = idle
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(i, _)| i);
                match lru_idx {
                    Some(idx) if idle[idx].is_idle_too_long(self.config.idle_timeout) => {
                        idle.swap_remove(idx);
                        evicted_idle += 1;
                    }
                    _ => break,
                }
            }

            (before, evicted_expired, evicted_idle)
        };

        let evicted = evicted_expired + evicted_idle;
        if evicted > 0 {
            let cur = self.total.get();
            self.total.set(cur.saturating_sub(evicted));
            for _ in 0..evicted {
                self.metrics.inc_evictions();
            }
        }

        // 3. Refill to min_idle. No retry here — if connect fails, back off
        // until the next 30s tick. Reserve the slot before the await so
        // concurrent get_inner calls see the bumped `total`.
        let (need, can_create) = {
            let idle_len = self.idle.borrow().len();
            let total = self.total.get();
            let need = self.config.min_idle.saturating_sub(idle_len);
            let can_create = self.config.max_size.saturating_sub(total);
            (need, can_create)
        };
        let to_create = need.min(can_create);

        let mut created = 0usize;
        for _ in 0..to_create {
            self.total.set(self.total.get() + 1);
            match Self::connect_one(&self.url).await {
                Ok(client) => {
                    self.metrics.inc_created();
                    created += 1;
                    self.idle
                        .borrow_mut()
                        .push(PoolEntry::new(client, self.config.max_lifetime));
                }
                Err(e) => {
                    self.total.set(self.total.get().saturating_sub(1));
                    eprintln!("[compio-postgres] housekeeper: failed to create connection: {e}");
                    break;
                }
            }
        }

        if evicted > 0 || created > 0 {
            let after_idle = self.idle.borrow().len();
            let active = self.active.get();
            let total = self.total.get();
            eprintln!(
                "[compio-postgres] housekeeper: before={before}, evicted={evicted}, \
                 created={created}, idle={after_idle}, active={active}, total={total}",
            );
        }
    }

    // ── Convenience methods ──────────────────────────────────────────────

    /// Acquire a connection, run a query, return the connection.
    pub async fn query(
        &self,
        sql: &str,
        params: &[&(dyn crate::types::ToSql + Sync)],
    ) -> Result<Vec<crate::Row>, Error> {
        let client = self.get().await?;
        client.query(sql, params).await
    }

    /// Query with text-format string parameters. Parameters are bound as
    /// `Type::TEXT`; the server performs implicit text-to-target conversion
    /// on the first reference in the query.
    pub async fn query_text_params(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<crate::Row>, Error> {
        let client = self.get().await?;
        client.query_text_params(sql, params).await
    }

    /// Acquire a connection, execute a statement, return the connection.
    pub async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn crate::types::ToSql + Sync)],
    ) -> Result<u64, Error> {
        let client = self.get().await?;
        client.execute(sql, params).await
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

    /// Number of callers waiting for a connection (including tombstoned slots).
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
// Connection task spawner
// ---------------------------------------------------------------------------

/// Spawn the Connection's `run` loop as a detached compio task. The task
/// terminates when the Client is dropped (causing the sender to close, which
/// causes the Connection's receiver to return `None`, which triggers the
/// graceful Terminate+drain+exit path in `Connection::run`).
fn spawn_connection_task(connection: Connection<Socket, crate::tls::NoTlsStream>) {
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            // Connection terminated with an error (I/O failure, protocol
            // violation). The next pool checkout that tries to use a closed
            // client will detect `is_closed()` and evict.
            log::debug!("compio-postgres: connection task error: {e}");
        }
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Waiter — drop-safe slot in the wait queue
// ---------------------------------------------------------------------------

/// A future that registers a waker in the pool's wait queue and unregisters
/// it on drop. Solves the waker-leak bug: if the caller's outer timeout
/// cancels this future, Drop tombstones our slot so `return_client` won't
/// wake a dead task while live waiters starve.
struct Waiter<'a> {
    pool: &'a Pool,
    /// Monotonic slot id assigned on first poll. `None` before registration.
    slot_id: Option<u64>,
}

impl<'a> Waiter<'a> {
    fn new(pool: &'a Pool) -> Self {
        Self {
            pool,
            slot_id: None,
        }
    }

    /// Index into `waiters` for this waiter, if still present.
    fn slot_index(&self) -> Option<usize> {
        let id = self.slot_id?;
        let base = self.pool.waiters_base.get();
        (id >= base).then(|| (id - base) as usize)
    }
}

impl Future for Waiter<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Wake immediately if a connection is available or capacity opened up.
        if !self.pool.idle.borrow().is_empty() || self.pool.total.get() < self.pool.config.max_size
        {
            return Poll::Ready(());
        }

        let mut waiters = self.pool.waiters.borrow_mut();
        let new_waker = cx.waker();

        match self.slot_index() {
            Some(idx) if idx < waiters.len() => {
                // Slot still live — update waker only if different.
                match &waiters[idx] {
                    Some(existing) if existing.will_wake(new_waker) => {}
                    _ => waiters[idx] = Some(new_waker.clone()),
                }
            }
            _ => {
                // First poll (or slot was consumed by wake_one_waiter before
                // we saw Ready — rare race). Register a fresh slot.
                let base = self.pool.waiters_base.get();
                let id = base + waiters.len() as u64;
                waiters.push_back(Some(new_waker.clone()));
                self.slot_id = Some(id);
            }
        }

        Poll::Pending
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        if let Some(idx) = self.slot_index() {
            let mut waiters = self.pool.waiters.borrow_mut();
            if idx < waiters.len() {
                waiters[idx] = None;
                // Trim tombstones from the front so waiters_base / indices
                // don't drift without bound. Only touches the front; we can't
                // touch the middle without shifting, which would break
                // other live waiters' slot_ids.
                while matches!(waiters.front(), Some(None)) {
                    waiters.pop_front();
                    self.pool.waiters_base.set(self.pool.waiters_base.get() + 1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PooledClient
// ---------------------------------------------------------------------------

/// A borrowed connection that returns to the pool on drop.
///
/// Dereferences to [`Client`] — call any client method (`.query(...)`,
/// `.execute(...)`, `.transaction()`, …) directly on the borrow.
pub struct PooledClient<'a> {
    entry: Option<PoolEntry>,
    pool: &'a Pool,
}

impl Deref for PooledClient<'_> {
    type Target = Client;
    fn deref(&self) -> &Self::Target {
        &self.entry.as_ref().unwrap().client
    }
}

impl DerefMut for PooledClient<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry.as_mut().unwrap().client
    }
}

impl Drop for PooledClient<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.pool.return_client(entry);
        }
    }
}

impl std::fmt::Debug for PooledClient<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledClient").finish()
    }
}
