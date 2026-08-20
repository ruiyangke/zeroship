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
//! if the connection is closed or past `max_lifetime`). A connection released
//! with a transaction still open on the wire gets a fire-and-forget ROLLBACK
//! first, so the next borrower never inherits it; see `Pool::return_client`
//! for why that is a ROLLBACK and not a `DISCARD ALL`.
//!
//! Transport
//! ---------
//! The pool has no transport policy of its own. It parses the URL, and every
//! `sslmode` then means through the pool exactly what it means through
//! [`Config::connect`] - see [`SslMode`] for the six, and [`crate::config::SslRootCert`]
//! for what each verifies. The pool's only jobs here are to build the
//! connector **once** rather than per connection, and to fail early.
//!
//! *This paragraph used to say the opposite.* Until the six modes landed, the
//! pool hardcoded [`NoTls`] and read `prefer` - the default - as plaintext,
//! while `Config::connect` with a real connector read the same word as
//! "TLS, and hard-fail if it does not work". One spelling, two behaviours,
//! chosen by which entry point you happened to use. The justification was
//! honest as far as it went (the driver had no reconnect-in-plaintext
//! fallback, so a verifying connector under `prefer` would have converted
//! "works, in plaintext" into "fails"), but the fix was to build the fallback,
//! which `connect.rs` now has.
//!
//! Building the connector once is not just a saving: it reads `sslrootcert`
//! from disk, and `sslrootcert=system` walks the operating system's store. The
//! pool opens connections at warm-up, on demand, and on every reconnect after
//! an eviction, so resolving per attempt would put a filesystem scan on the
//! reconnect path.
//!
//! Without the `tls` feature there is no connector to build, so
//! `require`/`verify-ca`/`verify-full` are refused by
//! [`Pool::connect_with_config`] before any socket is opened. libpq behaves the
//! same way when compiled without SSL support: those three modes are an error,
//! while `allow` and `prefer` are accepted and simply never attempt TLS.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[cfg(doc)]
use crate::config::SslMode;
use crate::tls::NoTls;
use crate::{Client, Config, Connection, Error, Socket, TransactionStatus};

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

/// Reject a mode that needs a TLS connector this build does not contain.
///
/// This is a build-capability check, not a policy: the contradictions between
/// `sslmode`, `sslrootcert` and `sslnegotiation` are `Config`'s to catch (see
/// `Config::validate_tls_settings`), and they are caught for every entry point,
/// not just this one.
///
/// libpq draws the line in the same place. Compiled without SSL support,
/// "using options `require`, `verify-ca`, or `verify-full` will cause an error,
/// while options `allow` and `prefer` will be accepted but libpq will not
/// actually attempt an SSL connection" - which, without the `tls` feature, is
/// precisely what [`NoTls`] does.
///
/// Answering here rather than at the socket matters for a second reason: this
/// pool retries a failed connection three times with backoff, so a URL that
/// could never have worked would otherwise cost about two seconds before
/// reporting an error that blames the server.
#[cfg(not(feature = "tls"))]
fn reject_tls_without_a_connector(config: &Config) -> Result<(), Error> {
    if !config.get_ssl_mode().permits_plaintext() {
        return Err(pool_error(format!(
            "sslmode={} cannot be satisfied: compio-postgres was built without the `tls` \
             feature, so no TLS connector exists. Rebuild with `--features tls`, or drop to \
             sslmode=prefer if plaintext is acceptable.",
            config.get_ssl_mode().as_str()
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Transport - everything needed to open one connection, resolved once
// ---------------------------------------------------------------------------

/// The pool's connection recipe, resolved from the URL exactly once.
///
/// See the `Transport` section of the module docs for why the connector is
/// built here rather than per attempt. Resolving early also moves a broken
/// `sslrootcert` to where it belongs: `Pool::connect` fails immediately naming
/// the file, rather than after the retry loop.
#[derive(Clone)]
struct Transport {
    config: Config,
    /// Present whenever the URL's `sslmode` may use TLS - i.e. every mode but
    /// `disable`. Which of them *verify* anything is the connector's business,
    /// not the pool's.
    #[cfg(feature = "tls")]
    tls: Option<crate::tls_rustls::MakeRustlsConnect>,
}

impl Transport {
    fn resolve(url: &str) -> Result<Transport, Error> {
        let config: Config = url.parse()?;
        config.validate_tls_settings()?;
        #[cfg(not(feature = "tls"))]
        reject_tls_without_a_connector(&config)?;

        #[cfg(feature = "tls")]
        let tls = if config.get_ssl_mode().permits_tls() {
            Some(crate::tls_rustls::MakeRustlsConnect::from_config(&config)?)
        } else {
            None
        };

        Ok(Transport {
            config,
            #[cfg(feature = "tls")]
            tls,
        })
    }

    /// Open a single client + spawn its Connection task. Returns the Client;
    /// the task is detached and self-terminates when the Client's sender is
    /// closed (i.e. when the Client is dropped).
    async fn connect_one(&self) -> Result<Client, Error> {
        #[cfg(feature = "tls")]
        if let Some(tls) = &self.tls {
            let (client, connection) = self.config.connect(tls.clone()).await?;
            spawn_connection_task(connection);
            return Ok(client);
        }

        let (client, connection) = self.config.connect(NoTls).await?;
        spawn_connection_task(connection);
        Ok(client)
    }

    /// Retry [`Transport::connect_one`] up to 3 times with 100ms, 400ms, 1.6s
    /// backoff. Only used for pool warm-up - `get_inner`'s on-demand connect
    /// stays single-shot to keep the latency budget tight.
    async fn connect_with_retry(&self) -> Result<Client, Error> {
        let mut delay = Duration::from_millis(100);
        let mut last_err: Option<Error> = None;
        for attempt in 0..3 {
            match self.connect_one().await {
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
    transport: Transport,
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
    /// Cancelled waiters remove their slot by `Rc` identity, so this queue
    /// contains live callers only and cannot accumulate tombstones.
    waiters: RefCell<VecDeque<Rc<WaiterSlot>>>,
    /// Retaining the handle makes dropping the pool cancel housekeeping even
    /// if the task is blocked in a connection attempt. The task itself holds
    /// only a `Weak<Pool>`, so this field does not form a reference cycle.
    housekeeper: RefCell<Option<compio::runtime::JoinHandle<()>>>,
    /// Observability counters.
    pub metrics: PoolMetrics,
}

impl Pool {
    /// Create a new pool. Eagerly opens `min_idle` connections to verify
    /// the URL and warm the pool.
    ///
    /// `min_idle` is lowered to `max_size` when the default would exceed it.
    /// This is the only constructor that can produce that combination, because
    /// it is the only one that sets `max_size` without the caller also seeing
    /// `min_idle`: `Pool::connect(url, 1)` against the default `min_idle` of 2
    /// used to warm up to two connections and hand out both, so the argument
    /// named `max_size` did not bound anything.
    pub async fn connect(url: &str, max_size: usize) -> Result<Self, Error> {
        let defaults = PoolConfig::default();
        let config = PoolConfig {
            max_size,
            min_idle: defaults.min_idle.min(max_size),
            ..defaults
        };
        Self::connect_with_config(url, config).await
    }

    /// Create a pool with full configuration.
    ///
    /// Opens `config.min_idle.max(1)` connections upfront so the first burst
    /// of traffic does not pay full connect latency. Each connection attempt
    /// is retried with exponential backoff (3 attempts: 100ms, 400ms, 1.6s)
    /// to survive Docker ordering, DNS blips, and brief PG restarts.
    ///
    /// # Errors
    ///
    /// Refuses a `max_size` of 0, and a `min_idle` greater than `max_size`.
    /// Both are configurations the pool cannot honour rather than preferences
    /// it can approximate, and both are cheaper to hear about here than as a
    /// checkout that blocks for `connection_timeout`.
    pub async fn connect_with_config(url: &str, config: PoolConfig) -> Result<Self, Error> {
        // A pool that can hold nothing is not a pool. Refused here rather than
        // constructed, because the alternative is worse than it looks: with
        // `max_size` 0 there is no idle entry to pop and no capacity to create
        // one, so `get()` parks on a waiter nothing can ever wake and every
        // caller pays the full `connection_timeout` before hearing about it.
        if config.max_size == 0 {
            return Err(Error::config("pool max_size must be at least 1".into()));
        }

        // `min_idle` above `max_size` is not a preference the pool can honour,
        // it is two contradictory instructions, and the pool cannot tell which
        // number the caller meant. Refused rather than clamped, so the answer
        // comes from whoever wrote them.
        //
        // NOT only reachable from a config spelled out in full, which is what
        // this comment used to claim. `PoolConfig` has public fields and no
        // `#[non_exhaustive]`, so `PoolConfig { max_size: 1, ..Default::default() }`
        // is the ordinary idiom and `Default` supplies `min_idle` behind the
        // caller's back - see `crates/plugin-db/tests/missing_role.rs`, which
        // is written exactly that way. A caller who never typed `min_idle` can
        // therefore land here.
        if config.min_idle > config.max_size {
            return Err(Error::config(
                format!(
                    "pool min_idle ({}) exceeds max_size ({}); set min_idle explicitly",
                    config.min_idle, config.max_size
                )
                .into(),
            ));
        }

        let transport = Transport::resolve(url)?;

        // `max(1)` keeps a `min_idle` of 0 from making the constructor prove
        // nothing about the URL - one connection is what verifies it. No
        // `min(max_size)`: with both guards above in place, `min_idle` is never
        // greater than `max_size` and `max_size` is never 0, so clamping here
        // could only ever change the `max_size == 0` case - which is now
        // refused rather than turned into a pool that deadlocks.
        let warm = config.min_idle.max(1);
        let mut entries: Vec<PoolEntry> = Vec::with_capacity(warm);
        for i in 0..warm {
            match transport.connect_with_retry().await {
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
            transport,
            config,
            idle: RefCell::new(entries),
            active: Cell::new(0),
            total: Cell::new(total),
            waiters: RefCell::new(VecDeque::new()),
            housekeeper: RefCell::new(None),
            metrics: PoolMetrics::new(),
        };
        for _ in 0..total {
            pool.metrics.inc_created();
        }
        Ok(pool)
    }

    /// Start the background housekeeper task. Runs every 30 seconds:
    ///   1. Evict connections past max_lifetime
    ///   2. Evict idle connections past idle_timeout (keep min_idle)
    ///   3. Refill to min_idle
    ///
    /// Must be called on the compio event loop thread that owns the pool.
    /// The housekeeper holds a `Weak<Pool>` across awaits, and the pool retains
    /// its task handle. Dropping the last strong `Rc` therefore releases the
    /// pool and cancels even a blocked housekeeping connection attempt.
    pub fn start_housekeeper(self: &std::rc::Rc<Self>) {
        self.start_housekeeper_with_interval(Duration::from_secs(30));
    }

    /// Interval-injectable form used by deterministic lifecycle tests.
    fn start_housekeeper_with_interval(self: &std::rc::Rc<Self>, interval: Duration) {
        let weak = std::rc::Rc::downgrade(self);
        let handle = compio::runtime::spawn(async move {
            loop {
                compio::time::sleep(interval).await;
                if !Self::housekeep(&weak).await {
                    break;
                }
            }
        });
        *self.housekeeper.borrow_mut() = Some(handle);
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
                let client = match self.transport.connect_one().await {
                    Ok(c) => c,
                    Err(e) => {
                        // `permit` drops here -> total -= 1.
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
            if let Some(entry) = Waiter::new(self).await {
                // A direct hand-off still needs the same expiry, closed, dirty,
                // and age checks as an idle entry. Put it where the next loop
                // iteration will synchronously pop it through that common path.
                // There is no await between this deposit and the pop, so a fresh
                // caller cannot barge ahead of this waiter.
                self.idle.borrow_mut().push(entry);
            }
            // else: woken for capacity/idle — loop and retry the acquire.
        }
    }

    /// Return a connection to the pool (called by `PooledClient::drop`).
    fn return_client(&self, mut entry: PoolEntry) {
        // The returning client is no longer active. (When the entry is handed
        // directly to a waiter below, checkout re-bumps `active` only after
        // validating it, so a successful hand-off nets zero.)
        self.active.set(self.active.get().saturating_sub(1));

        // Eviction criteria:
        //   - expired (max_lifetime reached)
        //   - closed: Client::is_closed() indicates the connection task exited
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

        // Clear any transaction still open on the wire before anyone else can
        // see this connection.
        //
        // `Transaction` borrows the client mutably, so a `PooledClient` cannot
        // be released while one is alive and its Drop already queues the
        // ROLLBACK. A transaction opened as raw SQL (`BEGIN` through
        // `execute`/`batch_execute`) has no such guard, and without this the
        // next borrower silently continues it: its writes join a transaction it
        // never began, and it can read rows the previous borrower never
        // committed. An aborted block (`E`) is worse still - the next borrower
        // gets `25P02` for every statement.
        //
        // ROLLBACK, not `DISCARD ALL`. The transaction is the only thing the
        // next borrower must not inherit; session state is something callers
        // are entitled to hand across a release. `DISCARD ALL` would take out
        // session-scoped advisory locks (crates/plugin-db's LockGuard holds one
        // on a pooled client), every prepared statement (this driver's own
        // type-info cache holds those for the life of the Client, so the next
        // use of a cached entry would fail), and every session GUC. It also
        // cannot run inside a transaction block at all - the server rejects it
        // with `25001` - which is precisely the state this code addresses.
        //
        // Fire-and-forget, exactly as `Transaction::drop` does: release is a
        // `Drop` and cannot await. The message is queued on the same FIFO
        // channel as every other request, so it is on the wire ahead of
        // anything the next borrower sends - including a borrower the entry is
        // handed to directly below. `__private_api_rollback` marks the client
        // dirty, which makes the next checkout run its barrier and drain the
        // ROLLBACK (evicting the connection if it failed).
        //
        // An in-flight transaction-capable request also requires a rollback.
        // Release cannot wait for the connection task to receive its
        // ReadyForQuery, and until it does the cached status describes the
        // preceding request. Queueing the ROLLBACK on the same FIFO puts it
        // after that unknown request and before anything the next borrower can
        // send.
        //
        // Anything but a settled `Idle` gets a ROLLBACK. `None` - a request
        // whose `ReadyForQuery` the connection task has not consumed yet - is
        // included deliberately: release cannot wait for it, and guessing
        // `Idle` is how an aborted transaction reaches the next borrower.
        //
        // This used to read the in-flight count and the status as two separate
        // arms, in an order the comment had to explain. `transaction_status`
        // now folds the check into its own return type, so there is no ordering
        // left to get wrong here.
        //
        // Skipped when the client is already dirty: a ROLLBACK is queued and a
        // second one would be pure noise on the wire.
        if !entry.client.is_dirty()
            && entry.client.transaction_status() != Some(TransactionStatus::Idle)
        {
            entry.client.__private_api_rollback(None);
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
    /// and already counted. `active` is bumped only after the next checkout's
    /// validation succeeds.
    ///
    /// Shared by `return_client` (normal release of a live connection) and
    /// `Waiter::drop` (reclaim of an entry deposited into a slot that was then
    /// cancelled before polling it out).
    fn redeposit_freed_entry(&self, entry: PoolEntry) {
        match self.take_front_waiter() {
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

    /// Pop the front waiter for a direct connection hand-off. The caller takes
    /// responsibility for depositing an entry and waking it.
    fn take_front_waiter(&self) -> Option<Rc<WaiterSlot>> {
        self.waiters.borrow_mut().pop_front()
    }

    /// Wake the first waiter for newly available capacity without removing its
    /// queue slot. Keeping the slot queued preserves FIFO if another caller
    /// takes the capacity before the waiter is polled, and lets `Waiter::drop`
    /// pass the wake onward if that waiter is cancelled while capacity remains.
    fn wake_one_waiter(&self) {
        let slot = self.waiters.borrow().front().cloned();
        if let Some(slot) = slot
            && let Some(w) = slot.waker.borrow_mut().take()
        {
            w.wake();
        }
    }

    /// Notify the head waiter when at least one unclaimed resource exists.
    /// Call this after claiming one resource so additional capacity or idle
    /// entries continue through the FIFO queue one claimant at a time.
    fn wake_one_waiter_if_available(&self) {
        if self.has_available_resource() {
            self.wake_one_waiter();
        }
    }

    fn has_available_resource(&self) -> bool {
        !self.idle.borrow().is_empty() || self.total.get() < self.config.max_size
    }

    /// Run one housekeeper cycle. A strong pool reference is held only while
    /// reading or mutating pool state, never across a connection await.
    /// Returns false once the pool has been dropped.
    async fn housekeep(weak: &Weak<Self>) -> bool {
        let Some(pool) = weak.upgrade() else {
            return false;
        };

        // Take counts and do all mutations inside tight borrows. Release
        // every borrow before awaiting.

        let (before, evicted_expired, evicted_idle) = {
            let mut idle = pool.idle.borrow_mut();
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
            let target = pool.config.min_idle;
            let mut evicted_idle = 0usize;
            while idle.len() > target {
                let lru_idx = idle
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(i, _)| i);
                match lru_idx {
                    Some(idx) if idle[idx].is_idle_too_long(pool.config.idle_timeout) => {
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
            let cur = pool.total.get();
            pool.total.set(cur.saturating_sub(evicted));
            for _ in 0..evicted {
                pool.metrics.inc_evictions();
            }
            pool.wake_one_waiter();
        }

        // 3. Refill to min_idle. No retry here — if connect fails, back off
        // until the next 30s tick. Reserve the slot before the await so
        // concurrent get_inner calls see the bumped `total`.
        let (need, can_create) = {
            let idle_len = pool.idle.borrow().len();
            let total = pool.total.get();
            let need = pool.config.min_idle.saturating_sub(idle_len);
            let can_create = pool.config.max_size.saturating_sub(total);
            (need, can_create)
        };
        let to_create = need.min(can_create);
        drop(pool);

        let mut created = 0usize;
        for _ in 0..to_create {
            // RE-CHECK CAPACITY, do not trust `to_create`. That budget was
            // computed before the first `connect_one().await`, and it stops
            // being true at that await: `get_inner` reserves slots while this
            // task is parked, so a later iteration can reserve one the pool no
            // longer has. Measured shape on the defaults - max_size 8, four
            // checked out, housekeeper picks to_create 2, reserves one and
            // parks; four acquisitions take total to 8; the second iteration
            // then reserved a ninth. The on-demand path never had this because
            // it checks and reserves with no await between the two.
            // Re-upgrade only long enough to recheck and reserve capacity and
            // clone the connection recipe. The weak permit accounts for an
            // error or cancellation without retaining the pool.
            let (transport, permit) = {
                let Some(pool) = weak.upgrade() else {
                    return false;
                };
                if pool.total.get() >= pool.config.max_size {
                    break;
                }
                let transport = pool.transport.clone();
                let permit = WeakPermitGuard::reserve(&pool);
                (transport, permit)
            };

            match transport.connect_one().await {
                Ok(client) => {
                    let Some(pool) = weak.upgrade() else {
                        return false;
                    };
                    pool.metrics.inc_created();
                    created += 1;
                    let entry = PoolEntry::new(client, pool.config.max_lifetime);
                    pool.redeposit_freed_entry(entry);
                    // The entry is now idle or assigned to the head waiter and
                    // remains counted in `total`; disarm the reservation.
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
            let Some(pool) = weak.upgrade() else {
                return false;
            };
            let after_idle = pool.idle.borrow().len();
            let active = pool.active.get();
            let total = pool.total.get();
            eprintln!(
                "[compio-postgres] housekeeper: before={before}, evicted={evicted}, \
                 created={created}, idle={after_idle}, active={active}, total={total}",
            );
        }

        weak.upgrade().is_some()
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
    ///
    /// # Errors
    /// Returns [`Error`] if a connection cannot be acquired or the server
    /// rejects any statement in the batch.
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

    /// Number of callers waiting for a connection.
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
/// alive-bypass validation). `Pool::get` runs `get_inner`
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
    /// Reserve a NEW slot (`total += 1`) on the on-demand connect path.
    fn reserve(pool: &'a Pool) -> Self {
        pool.total.set(pool.total.get() + 1);
        let guard = Self { pool, armed: true };
        pool.wake_one_waiter_if_available();
        guard
    }

    /// Adopt an EXISTING slot already counted in `total` — a `PoolEntry`
    /// popped out of `idle`. Does not touch `total`; only governs the
    /// decrement-on-drop so a cancellation during the barrier / validation
    /// await releases the popped entry's slot.
    fn adopt(pool: &'a Pool) -> Self {
        let guard = Self { pool, armed: true };
        pool.wake_one_waiter_if_available();
        guard
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
            self.pool.total.set(self.pool.total.get().saturating_sub(1));
            self.pool.wake_one_waiter();
        }
    }
}

/// A housekeeper reservation that does not keep the pool alive while a new
/// connection is being opened. If the pool still exists, error/cancellation
/// releases the reserved slot and notifies one waiter.
struct WeakPermitGuard {
    pool: Weak<Pool>,
    armed: bool,
}

impl WeakPermitGuard {
    fn reserve(pool: &Rc<Pool>) -> Self {
        pool.total.set(pool.total.get() + 1);
        let guard = Self {
            pool: Rc::downgrade(pool),
            armed: true,
        };
        pool.wake_one_waiter_if_available();
        guard
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for WeakPermitGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(pool) = self.pool.upgrade()
        {
            pool.total.set(pool.total.get().saturating_sub(1));
            pool.wake_one_waiter();
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
fn spawn_connection_task<T>(connection: Connection<Socket, T>)
where
    T: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin + 'static,
{
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
    /// waiter's next poll. Not yet counted in `active`; that happens only after
    /// the common checkout validation succeeds.
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
///     caller must run it through the common checkout checks.
///   - `None` — we were woken because capacity opened up or an idle entry
///     appeared (e.g. an eviction freed a `total` slot); the caller should loop
///     and retry the acquire (pop idle / create).
///
/// Drop-safety: a cancelled future (outer timeout, caller dropped) removes its
/// queue slot so `return_client`/`wake_one_waiter` cannot target it. If
/// a connection was already deposited into our slot but we are dropped before
/// polling it out, Drop re-homes that entry (to the next live waiter or `idle`)
/// so the connection is never lost.
struct Waiter<'a> {
    pool: &'a Pool,
    /// Our own clone of the shared slot, retained even after the pool pops our
    /// queue entry on hand-off — so `poll`/`drop` can still see a deposited
    /// entry. `None` until first poll registers us.
    slot: Option<Rc<WaiterSlot>>,
}

impl<'a> Waiter<'a> {
    fn new(pool: &'a Pool) -> Self {
        Self { pool, slot: None }
    }

    /// Remove this waiter's slot if it is still queued. Returns whether a live
    /// queue entry was removed.
    fn remove_queued_slot(&self) -> bool {
        let Some(slot) = &self.slot else {
            return false;
        };
        let mut waiters = self.pool.waiters.borrow_mut();
        let Some(index) = waiters.iter().position(|queued| Rc::ptr_eq(queued, slot)) else {
            return false;
        };
        waiters.remove(index);
        true
    }

    /// Remove this waiter only if it is still at the FIFO head. Capacity and
    /// idle resources are claimable by the head waiter, never by a later waiter
    /// that happened to receive a spurious poll first.
    fn remove_front_slot(&self) -> bool {
        let Some(slot) = &self.slot else {
            return false;
        };
        let mut waiters = self.pool.waiters.borrow_mut();
        if waiters.front().is_some_and(|front| Rc::ptr_eq(front, slot)) {
            waiters.pop_front();
            true
        } else {
            false
        }
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

        // 2. Register or refresh our FIFO slot before consulting global
        // availability. A later waiter must remain parked even if it receives a
        // spurious poll while the head owns the first claim.
        let new_waker = cx.waker();
        let current_slot = self.slot.as_ref().map(Rc::clone);
        let mut waiters = self.pool.waiters.borrow_mut();

        if let Some(slot) = current_slot
            && waiters.iter().any(|queued| Rc::ptr_eq(queued, &slot))
        {
            // Slot still live in the queue - refresh the waker if changed.
            let mut w = slot.waker.borrow_mut();
            match &*w {
                Some(existing) if existing.will_wake(new_waker) => {}
                _ => *w = Some(new_waker.clone()),
            }
        } else {
            // First poll, or our slot was popped for a direct hand-off and we
            // need to re-park. Register a fresh live slot at the back.
            let slot = WaiterSlot::new(new_waker.clone());
            waiters.push_back(Rc::clone(&slot));
            self.slot = Some(slot);
        }
        drop(waiters);

        // 3. Only the FIFO head may consume global availability. Removing the
        // slot here marks a successful retry, so Drop will not mistake it for a
        // cancelled capacity wake and notify the next waiter prematurely.
        if self.pool.has_available_resource() && self.remove_front_slot() {
            return Poll::Ready(None);
        }

        Poll::Pending
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        // 1. Remove our queue slot (if still queued) by stable Rc identity.
        //    Removing the element immediately keeps the queue live-only;
        //    no positional ids need updating and no tombstones accumulate.
        let removed_queued_slot = self.remove_queued_slot();

        // 2. Reclaim-on-drop: if a connection was deposited into our slot but
        //    we were cancelled before polling it out, re-home it so it is not
        //    lost. `active` was NEVER incremented for this entry (that happens
        //    only after checkout validation), so the reclaim must NOT touch
        //    `active`; nor `total` (the connection is still alive and counted).
        if let Some(slot) = self.slot.take()
            && let Some(entry) = slot.entry.borrow_mut().take()
        {
            self.pool.redeposit_freed_entry(entry);
        }

        // A capacity-only wake deliberately leaves the slot queued. If that
        // waiter is cancelled before it can retry, pass the wake to the next
        // waiter only while capacity (or an idle entry) is still available.
        // If another caller already took the capacity, its eventual release or
        // failed reservation is responsible for the next wake.
        if removed_queued_slot && self.pool.has_available_resource() {
            self.pool.wake_one_waiter();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::Request;
    use futures_channel::mpsc;
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    fn fake_client(process_id: i32) -> (Client, mpsc::UnboundedReceiver<Request>) {
        let (sender, receiver) = mpsc::unbounded();
        (
            Client::new(
                sender,
                SslMode::Disable,
                SslNegotiation::Postgres,
                process_id,
                0,
                None,
            ),
            receiver,
        )
    }

    fn fake_postgres_server() -> (
        std::net::SocketAddr,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0u8; 4];
            stream.read_exact(&mut length).unwrap();
            let remaining = u32::from_be_bytes(length) as usize - length.len();
            let mut startup = vec![0u8; remaining];
            stream.read_exact(&mut startup).unwrap();

            // AuthenticationOk, BackendKeyData, ReadyForQuery('I').
            stream
                .write_all(&[
                    b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'K', 0, 0, 0, 12, 0, 0, 0, 45, 0, 0, 0, 46,
                    b'Z', 0, 0, 0, 5, b'I',
                ])
                .unwrap();
            let _ = finish_rx.recv();
        });
        (address, finish_tx, server)
    }

    fn test_pool(config: PoolConfig, idle: Vec<PoolEntry>, active: usize, total: usize) -> Pool {
        Pool {
            transport: Transport::resolve("postgres://postgres@127.0.0.1/test?sslmode=disable")
                .unwrap(),
            config,
            idle: RefCell::new(idle),
            active: Cell::new(active),
            total: Cell::new(total),
            waiters: RefCell::new(VecDeque::new()),
            housekeeper: RefCell::new(None),
            metrics: PoolMetrics::new(),
        }
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let mut cx = Context::from_waker(Waker::noop());
        future.poll(&mut cx)
    }

    #[test]
    fn dirty_handoff_is_validated_before_waiter_receives_it() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 2);
        let (stale, stale_receiver) = fake_client(11);
        let held = PooledClient {
            entry: Some(PoolEntry::new(stale, pool.config.max_lifetime)),
            pool: &pool,
        };

        let mut acquire = Box::pin(pool.get_inner());
        assert!(poll_once(acquire.as_mut()).is_pending());
        assert_eq!(pool.pending_count(), 1);

        held.__private_api_rollback(None);
        drop(held);
        assert_eq!(pool.pending_count(), 0, "the stale entry was handed off");

        assert!(
            poll_once(acquire.as_mut()).is_pending(),
            "dirty handed-off connection bypassed checkout validation"
        );

        let (fresh, _fresh_receiver) = fake_client(22);
        pool.idle
            .borrow_mut()
            .push(PoolEntry::new(fresh, pool.config.max_lifetime));
        drop(stale_receiver);

        match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => assert_eq!(
                client.process_id(),
                22,
                "waiter received the stale handed-off connection"
            ),
            Poll::Ready(Err(error)) => panic!("fresh replacement was rejected: {error}"),
            Poll::Pending => panic!("fresh replacement should be ready without I/O"),
        }
    }

    #[test]
    fn closed_handoff_is_evicted_before_waiter_receives_it() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 2);
        let (stale, stale_receiver) = fake_client(12);
        let held = PooledClient {
            entry: Some(PoolEntry::new(stale, pool.config.max_lifetime)),
            pool: &pool,
        };

        let mut acquire = Box::pin(pool.get_inner());
        assert!(poll_once(acquire.as_mut()).is_pending());
        drop(held);
        assert_eq!(pool.pending_count(), 0, "the stale entry was handed off");

        drop(stale_receiver);
        let (fresh, _fresh_receiver) = fake_client(22);
        pool.idle
            .borrow_mut()
            .push(PoolEntry::new(fresh, pool.config.max_lifetime));

        match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => assert_eq!(
                client.process_id(),
                22,
                "waiter received the closed handed-off connection"
            ),
            Poll::Ready(Err(error)) => panic!("fresh replacement was rejected: {error}"),
            Poll::Pending => panic!("fresh replacement should be ready without I/O"),
        }
    }

    #[test]
    fn clean_recent_handoff_skips_validation() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(33);
        let held = PooledClient {
            entry: Some(PoolEntry::new(client, pool.config.max_lifetime)),
            pool: &pool,
        };

        let mut acquire = Box::pin(pool.get_inner());
        assert!(poll_once(acquire.as_mut()).is_pending());
        drop(held);

        match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => assert_eq!(client.process_id(), 33),
            Poll::Ready(Err(error)) => panic!("clean handoff failed: {error}"),
            Poll::Pending => panic!("clean recent handoff performed an unnecessary probe"),
        }
    }

    #[test]
    fn cancelled_connection_reservation_wakes_waiter() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 0);
        let permit = PermitGuard::reserve(&pool);
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut waiter = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());
        assert_eq!(pool.pending_count(), 1);
        drop(permit);

        assert_eq!(
            wake_count.load(Ordering::Relaxed),
            1,
            "released capacity did not wake the parked waiter"
        );
        assert!(matches!(
            poll_with_waker(waiter.as_mut(), &waker),
            Poll::Ready(None)
        ));
        drop(waiter);
        assert_eq!(pool.pending_count(), 0);
    }

    #[test]
    fn successful_connection_reservation_does_not_wake_waiter() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 0);
        let permit = PermitGuard::reserve(&pool);
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut waiter = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());
        permit.disarm();

        assert_eq!(
            wake_count.load(Ordering::Relaxed),
            0,
            "a consumed reservation must not wake a parked waiter"
        );
        assert_eq!(
            pool.pending_count(),
            1,
            "a consumed reservation must not advertise capacity"
        );
    }

    #[test]
    fn cancelling_capacity_woken_waiter_wakes_successor_when_capacity_remains() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let released = PermitGuard::adopt(&pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(Waiter::new(&pool));
        let mut second = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        drop(released);
        assert_eq!(pool.total_count(), 0);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);

        drop(first);
        assert_eq!(
            second_count.load(Ordering::Relaxed),
            1,
            "cancelling the capacity-woken waiter stranded its successor"
        );
    }

    #[test]
    fn cancelling_capacity_woken_waiter_does_not_wake_successor_when_capacity_was_taken() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let released = PermitGuard::adopt(&pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(Waiter::new(&pool));
        let mut second = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        drop(released);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);

        let replacement = PermitGuard::reserve(&pool);
        drop(first);
        assert_eq!(
            second_count.load(Ordering::Relaxed),
            0,
            "a full pool must not advertise capacity to the next waiter"
        );
        replacement.disarm();
    }

    #[test]
    fn capacity_woken_waiter_that_retries_does_not_wake_successor() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let released = PermitGuard::adopt(&pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(Waiter::new(&pool));
        let mut second = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        drop(released);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);
        assert!(matches!(
            poll_with_waker(first.as_mut(), &first_waker),
            Poll::Ready(None)
        ));

        let claimed = PermitGuard::reserve(&pool);
        drop(first);
        assert_eq!(
            second_count.load(Ordering::Relaxed),
            0,
            "the waiter claiming capacity must keep the first opportunity"
        );
        assert_eq!(pool.pending_count(), 1);
        claimed.disarm();
    }

    #[test]
    fn multiple_capacity_releases_reach_multiple_waiters() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 2);
        let released_first = PermitGuard::adopt(&pool);
        let released_second = PermitGuard::adopt(&pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(Waiter::new(&pool));
        let mut second = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        drop(released_first);
        drop(released_second);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);
        assert_eq!(second_count.load(Ordering::Relaxed), 0);

        assert!(matches!(
            poll_with_waker(first.as_mut(), &first_waker),
            Poll::Ready(None)
        ));
        let claimed = PermitGuard::reserve(&pool);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(
            second_count.load(Ordering::Relaxed),
            1,
            "remaining capacity did not reach the next waiter"
        );
        claimed.disarm();
    }

    #[test]
    fn housekeeping_reservation_propagates_remaining_capacity() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 0, 2));
        let released_first = PermitGuard::adopt(&pool);
        let released_second = PermitGuard::adopt(&pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(Waiter::new(&pool));
        let mut second = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        drop(released_first);
        drop(released_second);
        assert!(matches!(
            poll_with_waker(first.as_mut(), &first_waker),
            Poll::Ready(None)
        ));

        let claimed = WeakPermitGuard::reserve(&pool);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(
            second_count.load(Ordering::Relaxed),
            1,
            "housekeeping claim hid remaining capacity from the next waiter"
        );
        claimed.disarm();
    }

    #[test]
    fn later_waiter_cannot_claim_capacity_before_fifo_head() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let released = PermitGuard::adopt(&pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(Waiter::new(&pool));
        let mut second = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        drop(released);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);

        assert!(
            poll_with_waker(second.as_mut(), &second_waker).is_pending(),
            "later waiter claimed capacity before the FIFO head"
        );
    }

    #[test]
    fn cancelled_waiters_do_not_accumulate_behind_live_waiter() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let mut head = Box::pin(Waiter::new(&pool));
        assert!(poll_once(head.as_mut()).is_pending());

        for _ in 0..32 {
            let mut cancelled = Box::pin(Waiter::new(&pool));
            assert!(poll_once(cancelled.as_mut()).is_pending());
            drop(cancelled);
            assert_eq!(
                pool.pending_count(),
                1,
                "cancelled waiter left a queue tombstone"
            );
        }
    }

    struct WakeCounter(Arc<AtomicUsize>);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counting_waker(counter: &Arc<AtomicUsize>) -> Waker {
        Waker::from(Arc::new(WakeCounter(Arc::clone(counter))))
    }

    fn poll_with_waker<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        let mut cx = Context::from_waker(waker);
        future.poll(&mut cx)
    }

    #[test]
    fn cancelling_middle_waiter_preserves_live_waiters_behind_it() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let oldest_count = Arc::new(AtomicUsize::new(0));
        let youngest_count = Arc::new(AtomicUsize::new(0));
        let waker_a = counting_waker(&oldest_count);
        let waker_c = counting_waker(&youngest_count);
        let mut front = Box::pin(Waiter::new(&pool));
        let mut middle = Box::pin(Waiter::new(&pool));
        let mut rear = Box::pin(Waiter::new(&pool));

        assert!(poll_with_waker(front.as_mut(), &waker_a).is_pending());
        assert!(poll_once(middle.as_mut()).is_pending());
        assert!(poll_with_waker(rear.as_mut(), &waker_c).is_pending());
        drop(middle);

        pool.wake_one_waiter();
        assert_eq!(oldest_count.load(Ordering::Relaxed), 1);
        assert_eq!(youngest_count.load(Ordering::Relaxed), 0);

        drop(front);
        pool.wake_one_waiter();
        assert_eq!(youngest_count.load(Ordering::Relaxed), 1);
    }

    #[compio::test]
    async fn live_pool_housekeeper_task_still_runs() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let (client, _receiver) = fake_client(44);
        let expired = PoolEntry::new(client, Duration::ZERO);
        let pool = Rc::new(test_pool(config, vec![expired], 0, 1));
        pool.start_housekeeper_with_interval(Duration::from_millis(1));

        compio::time::timeout(Duration::from_secs(1), async {
            while pool.idle_count() != 0 {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("spawned housekeeper did not run");
        assert_eq!(pool.idle_count(), 0, "expired connection was not evicted");
        assert_eq!(pool.total_count(), 0, "eviction did not release capacity");
        assert_eq!(pool.metrics.evictions.get(), 1);
    }

    #[compio::test]
    async fn housekeeping_eviction_wakes_waiter_for_released_capacity() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 2, 2));
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());

        let (client, _receiver) = fake_client(45);
        pool.active.set(1);
        pool.idle
            .borrow_mut()
            .push(PoolEntry::new(client, Duration::ZERO));
        let weak = Rc::downgrade(&pool);

        assert!(Pool::housekeep(&weak).await);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(
            wake_count.load(Ordering::Relaxed),
            1,
            "housekeeping released capacity without waking its waiter"
        );
    }

    #[compio::test]
    async fn housekeeping_without_available_resource_does_not_wake_waiter() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 2, 2));
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());

        let weak = Rc::downgrade(&pool);
        assert!(Pool::housekeep(&weak).await);
        assert_eq!(pool.total_count(), 2);
        assert_eq!(wake_count.load(Ordering::Relaxed), 0);
    }

    #[compio::test]
    async fn successful_housekeeping_refill_hands_connection_to_fifo_head() {
        let (address, finish_tx, server) = fake_postgres_server();
        let config = PoolConfig {
            max_size: 4,
            min_idle: 1,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let url = format!("postgres://postgres@{address}/fake?sslmode=disable");
        let mut pool = test_pool(config, Vec::new(), 0, 4);
        pool.transport = Transport::resolve(&url).unwrap();
        let pool = Rc::new(pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(pool.get_inner());
        let mut second = Box::pin(pool.get_inner());

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        // Model two in-progress attempts ending before their waiters are
        // repolled. Housekeeping may claim one slot; one must remain visible.
        pool.total.set(2);
        let weak = Rc::downgrade(&pool);
        assert!(
            compio::time::timeout(Duration::from_secs(5), Pool::housekeep(&weak))
                .await
                .expect("housekeeping refill did not complete")
        );

        assert_eq!(pool.idle_count(), 0, "refill bypassed the FIFO head");
        assert_eq!(pool.pending_count(), 1);
        assert_eq!(pool.total_count(), 3);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);
        assert_eq!(second_count.load(Ordering::Relaxed), 0);

        let first_client = match poll_with_waker(first.as_mut(), &first_waker) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("refilled connection was rejected: {error}"),
            Poll::Pending => panic!("FIFO head did not receive the refilled connection"),
        };
        assert_eq!(first_client.process_id(), 45);
        assert_eq!(
            second_count.load(Ordering::Relaxed),
            1,
            "remaining capacity did not reach the next waiter"
        );

        drop(first_client);
        drop(first);
        drop(second);
        drop(pool);
        let _ = finish_tx.send(());
        server.join().expect("fake PostgreSQL server panicked");
    }

    #[compio::test]
    async fn blackholed_housekeeper_does_not_retain_dropped_pool() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = futures_channel::oneshot::channel();
        let (eof_tx, eof_rx) = futures_channel::oneshot::channel();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0u8; 4];
            stream.read_exact(&mut length).unwrap();
            let remaining = u32::from_be_bytes(length) as usize - length.len();
            let mut startup = vec![0u8; remaining];
            stream.read_exact(&mut startup).unwrap();
            let _ = started_tx.send(());

            let mut byte = [0u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Err(error) => matches!(
                    error.kind(),
                    ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                        | ErrorKind::BrokenPipe
                ),
                _ => false,
            };
            let _ = eof_tx.send(closed);
        });

        let config = PoolConfig {
            max_size: 1,
            min_idle: 1,
            ..PoolConfig::default()
        };
        let url = format!("postgres://postgres@{address}/blackhole?sslmode=disable");
        let mut pool = test_pool(config, Vec::new(), 0, 0);
        pool.transport = Transport::resolve(&url).unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);
        pool.start_housekeeper_with_interval(Duration::ZERO);

        compio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("housekeeper never opened the test connection")
            .expect("blackhole server stopped before startup");
        assert_eq!(
            pool.total_count(),
            1,
            "housekeeper did not reserve capacity"
        );

        drop(pool);
        assert!(
            weak.upgrade().is_none(),
            "blackholed housekeeping retained the dropped pool"
        );

        let closed = compio::time::timeout(Duration::from_secs(5), eof_rx)
            .await
            .expect("housekeeper cancellation did not close the socket")
            .expect("blackhole server stopped before reporting EOF");
        assert!(closed, "blackhole server did not observe a closed socket");
        server.join().expect("blackhole server thread panicked");
    }
}
