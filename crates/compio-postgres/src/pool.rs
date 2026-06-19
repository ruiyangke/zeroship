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
use std::rc::Rc;
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
    /// Callers waiting for a connection, in FIFO order.
    ///
    /// Each live slot is a shared [`WaiterSlot`] the pool can deposit a freed
    /// [`PoolEntry`] directly into — handing the connection straight to the
    /// longest-queued waiter instead of returning it to `idle` where a fresh
    /// caller could barge it (POOL-2 / bb8/deadpool fairness model).
    ///
    /// `Option<…>` allows waiters to tombstone their queue slot on drop: a
    /// cancelled future (outer timeout, caller dropped) writes `None` here so
    /// `return_client` skips it. Without this the queue leaks dead waiters and
    /// live ones starve.
    waiters: RefCell<VecDeque<Option<Rc<WaiterSlot>>>>,
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
                // Adopt the popped slot: it is already counted in `total`, and
                // until a PooledClient owns it (or it is pushed back to idle)
                // its decrement must ride on Drop so cancellation during the
                // dirty barrier / alive validation below — or an eviction
                // `continue` — releases it exactly once. The manual
                // `total -= 1` on the eviction paths is therefore gone; the
                // guard's Drop does it when the loop body unwinds on `continue`.
                let permit = PermitGuard::adopt(self);

                if entry.is_expired() {
                    self.metrics.inc_evictions();
                    continue;
                }

                // If the Client's sender is closed (connection task exited
                // due to I/O error, EOF, etc), we cannot use this entry.
                if entry.client.is_closed() {
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
                            self.metrics.inc_evictions();
                            continue;
                        }
                    }
                    // The barrier itself provides the alive-check; skip the
                    // second `simple_query("")` below.
                    entry.touch();
                    self.active.set(self.active.get() + 1);
                    // PooledClient now owns the slot; its Drop -> return_client
                    // handles total/active. Disarm so the guard doesn't also
                    // decrement total.
                    permit.disarm();
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
                    self.metrics.inc_evictions();
                    continue;
                }

                entry.touch();
                self.active.set(self.active.get() + 1);
                // PooledClient now owns the slot; its Drop -> return_client
                // handles total/active. Disarm so the guard doesn't also
                // decrement total.
                permit.disarm();
                return Ok(PooledClient {
                    entry: Some(entry),
                    pool: self,
                });
            }

            // 2. No idle connections — create a new one if under limit.
            // Reserve the slot synchronously *before* the await so concurrent
            // callers in the same loop see the bumped `total` and don't race
            // past `max_size`. The reservation rides on a PermitGuard: if this
            // future is cancelled while parked at `connect_one().await` (the
            // outer `connection_timeout`, or a caller dropping the get()), the
            // guard's Drop releases the slot — without it the `+1` would leak
            // forever (POOL-1). On Err the guard also releases it on return.
            if self.total.get() < self.config.max_size {
                let permit = PermitGuard::reserve(self);
                let client = match Self::connect_one(&self.url).await {
                    Ok(c) => c,
                    Err(e) => {
                        // `permit` drops here -> total -= 1.
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
                // PooledClient now owns the slot; its Drop -> return_client
                // handles total/active. Disarm so the guard doesn't also
                // decrement total.
                permit.disarm();
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

            // 3. Pool is full — park in the FIFO wait queue. Resolves either
            // with a connection handed *directly* to us by `return_client`
            // (bypassing `idle`, so no fresh caller can barge it — POOL-2), or
            // with `None` meaning capacity/idle opened up and we should loop.
            if let Some(mut entry) = Waiter::new(self).await {
                // A live connection was handed to us. It is still counted in
                // `total`; `active` was NOT bumped on the hand-off, so we do it
                // here — at the moment we take ownership. No `.await` lies
                // between the take (inside the resolved future) and here, so
                // there is no cancellation window.
                entry.touch();
                self.active.set(self.active.get() + 1);
                return Ok(PooledClient {
                    entry: Some(entry),
                    pool: self,
                });
            }
            // else: woken for capacity/idle — loop and retry the acquire.
        }
    }

    /// Return a connection to the pool (called by `PooledClient::drop`).
    fn return_client(&self, mut entry: PoolEntry) {
        // The returning client is no longer active. (When the entry is handed
        // directly to a waiter below, that waiter re-bumps `active` only when
        // it actually takes the entry — so across a hand-off `active` is
        // decremented here and incremented there, netting zero.)
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
            // Capacity just opened up (`total` dropped below `max_size`); wake a
            // parked waiter so it can create a fresh connection rather than
            // stalling until its own timeout. The entry itself is gone, so this
            // is a pure capacity wake (the woken waiter resolves with `None`).
            self.wake_one_waiter();
            return;
        }

        // Live connection. Hand it DIRECTLY to the longest-queued waiter if one
        // exists — bypassing `idle` so a fresh, never-parked caller cannot pop
        // it first (POOL-2 FIFO fairness). Otherwise park it in `idle`.
        entry.touch();
        self.redeposit_freed_entry(entry);
    }

    /// Home an alive, un-owned [`PoolEntry`] that needs a new holder: hand it
    /// directly to the front live waiter, or push it to `idle` if none is
    /// waiting. Does NOT touch `active` or `total` — the connection is alive
    /// and already counted, and `active` is bumped only when a waiter actually
    /// takes the entry (or when a fresh caller pops it from `idle`).
    ///
    /// Shared by `return_client` (normal release of a live connection) and
    /// `Waiter::drop` (reclaim of an entry deposited into a slot that was then
    /// cancelled before polling it out).
    fn redeposit_freed_entry(&self, entry: PoolEntry) {
        match self.take_front_live_waiter() {
            Some(slot) => {
                // Deposit into the waiter's rendezvous slot and wake it. The
                // waiter has been popped from the queue (it is no longer
                // "waiting"); it now owns the right to this entry via its
                // retained `Rc<WaiterSlot>` clone.
                *slot.entry.borrow_mut() = Some(entry);
                if let Some(w) = slot.waker.borrow_mut().take() {
                    w.wake();
                }
            }
            None => self.idle.borrow_mut().push(entry),
        }
    }

    /// Pop and return the first live (non-tombstoned) waiter slot, advancing
    /// `waiters_base` past every slot removed. Returns `None` if no live waiter
    /// remains. The popped waiter is no longer in the queue; the caller takes
    /// responsibility for it (deposit an entry + wake, or — for a capacity
    /// wake — just wake).
    fn take_front_live_waiter(&self) -> Option<Rc<WaiterSlot>> {
        let mut waiters = self.waiters.borrow_mut();
        while let Some(slot) = waiters.pop_front() {
            self.waiters_base.set(self.waiters_base.get() + 1);
            if let Some(slot) = slot {
                return Some(slot);
            }
        }
        None
    }

    /// Wake the first live waiter in the queue (capacity wake — no entry
    /// handed). Used on the eviction path, where a `total` slot just freed up
    /// and a parked waiter should retry (and create a fresh connection).
    fn wake_one_waiter(&self) {
        if let Some(slot) = self.take_front_live_waiter()
            && let Some(w) = slot.waker.borrow_mut().take()
        {
            w.wake();
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
            // Same RAII discipline as the on-demand connect path: reserve the
            // slot before the await so concurrent get_inner calls see the
            // bumped `total`, and let the guard release it on Err / cancellation
            // (the housekeeper runs as a detached task and can be dropped).
            let permit = PermitGuard::reserve(self);
            match Self::connect_one(&self.url).await {
                Ok(client) => {
                    self.metrics.inc_created();
                    created += 1;
                    self.idle
                        .borrow_mut()
                        .push(PoolEntry::new(client, self.config.max_lifetime));
                    // The entry now lives in `idle` and is counted in `total`;
                    // disarm so the guard doesn't decrement it back out.
                    permit.disarm();
                }
                Err(e) => {
                    // `permit` drops here -> total -= 1.
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
    ///
    /// Uses the **extended/prepared** protocol — exactly ONE command per call.
    /// Multi-statement SQL (e.g. DDL with a trailing `COMMENT ON COLUMN`
    /// sentinel) raises `42601 cannot insert multiple commands into a prepared
    /// statement`; use [`Pool::batch_execute`] for those.
    pub async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn crate::types::ToSql + Sync)],
    ) -> Result<u64, Error> {
        let client = self.get().await?;
        client.execute(sql, params).await
    }

    /// Acquire a connection and run one or more `;`-separated statements via the
    /// **simple-query** protocol, then return the connection.
    ///
    /// This is the correct primitive for multi-statement DDL — e.g. the
    /// `CREATE TABLE …; COMMENT ON COLUMN … IS 'zsenc:…'` / `'__zsmask:…'`
    /// sentinel batches the schema builder emits (P4 HALF A / P5.5 PR 6).
    /// `Pool::execute` cannot run those (it prepares a single command).
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        let client = self.get().await?;
        client.batch_execute(sql).await
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

/// RAII guard for a `total` permit held between reservation/adoption and the
/// moment the slot is safely owned by a returned [`PooledClient`] (or pushed
/// back to `idle`).
///
/// `total` is the pool's hand-maintained capacity counter. Every path that
/// touches it crosses an `.await` (on-demand `connect_one`, the dirty barrier,
/// alive-bypass validation, housekeeper refill). `Pool::get` runs `get_inner`
/// under `compio::time::timeout`, a `select!` that DROPS the inner future when
/// the timer wins — so a post-await `total -= 1` statement is skipped on
/// cancellation, leaking the permit forever (POOL-1). Tying the decrement to
/// `Drop` makes it fire on every exit: success, error, early return, panic,
/// and — crucially — cancellation. The guard is `disarm()`ed once a
/// `PooledClient` owns the slot (its own Drop -> `return_client` then accounts
/// for it) or the entry is back in `idle`.
struct PermitGuard<'a> {
    pool: &'a Pool,
    armed: bool,
}

impl<'a> PermitGuard<'a> {
    /// Reserve a NEW slot (`total += 1`). Use on paths that create a
    /// connection: the on-demand connect path and the housekeeper refill.
    fn reserve(pool: &'a Pool) -> Self {
        pool.total.set(pool.total.get() + 1);
        Self { pool, armed: true }
    }

    /// Adopt an EXISTING slot already counted in `total` — a `PoolEntry`
    /// popped out of `idle`. Does not touch `total`; only governs the
    /// decrement-on-drop so a cancellation during the barrier / validation
    /// await releases the popped entry's slot.
    const fn adopt(pool: &'a Pool) -> Self {
        Self { pool, armed: true }
    }

    /// Hand off the slot: the caller now owns it (via a returned
    /// `PooledClient` or an entry pushed back to `idle`), so Drop must NOT
    /// decrement `total`.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PermitGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.pool
                .total
                .set(self.pool.total.get().saturating_sub(1));
        }
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
// Waiter — drop-safe slot in the wait queue with direct connection hand-off
// ---------------------------------------------------------------------------

/// Shared rendezvous between a parked [`Waiter`] and `return_client`.
///
/// `return_client` deposits a freed [`PoolEntry`] into `entry` and wakes the
/// waiter via `waker`, handing the connection *directly* to the longest-queued
/// caller instead of pushing it to `idle` (which a fresh, never-parked caller
/// could pop first — the POOL-2 barge). The slot is an `Rc` so the pool holds
/// one clone (in the `waiters` queue, for depositing) and the `Waiter` future
/// holds another (so it can still read a deposited entry even after the pool
/// has popped its queue slot).
struct WaiterSlot {
    waker: RefCell<Option<Waker>>,
    /// A connection handed to this waiter by `return_client`, awaiting the
    /// waiter's next poll. Not yet counted in `active` — `active += 1` happens
    /// only when the waiter actually takes it.
    entry: RefCell<Option<PoolEntry>>,
}

impl WaiterSlot {
    fn new(waker: Waker) -> Rc<Self> {
        Rc::new(Self {
            waker: RefCell::new(Some(waker)),
            entry: RefCell::new(None),
        })
    }
}

/// A future that registers itself in the pool's wait queue and unregisters on
/// drop. Resolves to:
///   - `Some(entry)` — `return_client` handed us a connection directly; the
///     caller must build a [`PooledClient`] from it (incrementing `active`).
///   - `None` — we were woken because capacity opened up or an idle entry
///     appeared (e.g. an eviction freed a `total` slot); the caller should loop
///     and retry the acquire (pop idle / create).
///
/// Drop-safety: a cancelled future (outer timeout, caller dropped) tombstones
/// its queue slot so `return_client`/`wake_one_waiter` skip it. CRITICALLY, if
/// a connection was already deposited into our slot but we are dropped before
/// polling it out, Drop re-homes that entry (to the next live waiter or `idle`)
/// so the connection is never lost.
struct Waiter<'a> {
    pool: &'a Pool,
    /// Monotonic slot id assigned on first poll. `None` before registration.
    slot_id: Option<u64>,
    /// Our own clone of the shared slot, retained even after the pool pops our
    /// queue entry on hand-off — so `poll`/`drop` can still see a deposited
    /// entry. `None` until first poll registers us.
    slot: Option<Rc<WaiterSlot>>,
}

impl<'a> Waiter<'a> {
    fn new(pool: &'a Pool) -> Self {
        Self {
            pool,
            slot_id: None,
            slot: None,
        }
    }

    /// Index into `waiters` for this waiter, if it is still queued. Returns
    /// `None` once the pool has popped our slot (hand-off or wake), in which
    /// case `slot_id < waiters_base`.
    fn slot_index(&self) -> Option<usize> {
        let id = self.slot_id?;
        let base = self.pool.waiters_base.get();
        (id >= base).then(|| (id - base) as usize)
    }
}

impl Future for Waiter<'_> {
    type Output = Option<PoolEntry>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 1. A connection handed directly to us takes priority — claim it.
        if let Some(slot) = &self.slot
            && let Some(entry) = slot.entry.borrow_mut().take()
        {
            return Poll::Ready(Some(entry));
        }

        // 2. Otherwise, if a connection is available or capacity opened up,
        //    resolve with `None` so the caller loops and retries the acquire.
        if !self.pool.idle.borrow().is_empty() || self.pool.total.get() < self.pool.config.max_size
        {
            return Poll::Ready(None);
        }

        // 3. Still no connection — (re)register and park.
        let mut waiters = self.pool.waiters.borrow_mut();
        let new_waker = cx.waker();

        match self.slot_index() {
            Some(idx) if idx < waiters.len() => {
                // Slot still live in the queue — refresh the waker if changed.
                let slot = self.slot.as_ref().expect("queued waiter has a slot");
                let mut w = slot.waker.borrow_mut();
                match &*w {
                    Some(existing) if existing.will_wake(new_waker) => {}
                    _ => *w = Some(new_waker.clone()),
                }
            }
            _ => {
                // First poll, or our slot was popped (hand-off / wake) and we
                // need to re-park. Register a fresh slot.
                let base = self.pool.waiters_base.get();
                let id = base + waiters.len() as u64;
                let slot = WaiterSlot::new(new_waker.clone());
                waiters.push_back(Some(Rc::clone(&slot)));
                self.slot = Some(slot);
                self.slot_id = Some(id);
            }
        }

        Poll::Pending
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        // 1. Tombstone our queue slot (if still queued) so the pool stops
        //    treating us as a live waiter.
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

        // 2. Reclaim-on-drop: if a connection was deposited into our slot but
        //    we were cancelled before polling it out, re-home it so it is not
        //    lost. `active` was NEVER incremented for this entry (that happens
        //    only when `poll` takes it), so the reclaim must NOT touch `active`;
        //    nor `total` (the connection is still alive and counted).
        if let Some(slot) = self.slot.take()
            && let Some(entry) = slot.entry.borrow_mut().take()
        {
            self.pool.redeposit_freed_entry(entry);
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
