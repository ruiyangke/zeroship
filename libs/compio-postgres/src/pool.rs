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
//! is `!Send` because compio's TcpStream uses `Rc` internally - the type
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
//! [`Pool::close`] is the coordinated shutdown path. It irreversibly rejects
//! acquisitions, wakes queued callers, drops idle and assigned entries, stops
//! housekeeping, and waits for this pool's borrowed [`PooledClient`]s to be
//! returned. It deliberately does not use the driver's thread-wide connection
//! drain, which would couple one pool's shutdown to unrelated pools.
//!
//! Lifecycle callbacks are part of [`PoolConfig`]. `after_connect` and
//! `before_acquire` are asynchronous and run before a candidate becomes active,
//! and they divide the candidates between them rather than both seeing every
//! one: `after_connect` owns connections the pool has just opened, and
//! `before_acquire` owns connections being recycled out of the idle set.
//! `after_release` is a synchronous keep-or-discard predicate because return
//! happens through `Drop`; a rejected session is closed instead of being made
//! visible to another borrower.
//!
//! Transport
//! ---------
//! The pool has no transport policy of its own. It accepts the driver's typed
//! [`Config`] directly (with URL helpers that parse one), and every `sslmode`
//! then means through the pool exactly what it means through
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
use crate::{CancelToken, Client, Config, Connection, Error, Socket, TransactionStatus};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A boxed, single-threaded future returned by an asynchronous pool hook.
///
/// Pool hooks do not require [`Send`] because [`Pool`] invokes them on its
/// single-threaded local compio executor. The future may borrow the client for
/// the duration of the hook invocation.
pub type PoolHookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

type AfterConnectHook = dyn for<'a> Fn(&'a Client) -> PoolHookFuture<'a, Result<(), Error>>;
type BeforeAcquireHook = dyn for<'a> Fn(&'a Client) -> PoolHookFuture<'a, Result<bool, Error>>;
type AfterReleaseHook = dyn Fn(&Client) -> bool;

/// Pool tuning and lifecycle callbacks with HikariCP-inspired defaults.
///
/// The setters follow the same mutable-builder style as [`Config::ssl_mode`]
/// and [`Config::channel_binding`]. Callback futures do not require [`Send`]
/// because [`Pool`] invokes them on its single-threaded local compio executor.
///
/// # Callback reentrancy
///
/// Async callbacks may await operations on the supplied [`Client`], but must
/// not acquire from the same pool or wait for work that needs that pool's
/// capacity. The candidate connection remains counted and unavailable while
/// its callback is running, so a recursive checkout can exhaust the pool and
/// deadlock.
/// A callback that needs to refer to its pool should capture a [`Weak`] handle;
/// capturing a strong [`Rc`] can form a cycle that keeps the pool alive.
///
/// `after_release` is synchronous because it runs from [`PooledClient::drop`].
/// It must return quickly and must not block, start a nested runtime, re-enter
/// the pool, or panic. Return `false` when asynchronous cleanup would otherwise
/// be required; the pool discards that session and opens a clean one later. A
/// panic during another panic's unwind aborts the process, as with any `Drop`.
///
/// Holding the hooks as `Rc` makes this type `!Send` even when no hook is set,
/// so a config cannot be built on one thread and moved to another. That matches
/// the [`Pool`] it configures, which is `!Send` by construction (`RefCell`,
/// `Cell`, `Rc` throughout) because every pool is owned by one compio thread.
/// Build the config on the thread that will own the pool.
#[derive(Clone)]
#[must_use]
pub struct PoolConfig {
    max_size: usize,
    min_idle: usize,
    max_lifetime: Duration,
    idle_timeout: Duration,
    acquire_timeout: Duration,
    command_timeout: Option<Duration>,
    validation_bypass: Duration,
    after_connect: Option<Rc<AfterConnectHook>>,
    before_acquire: Option<Rc<BeforeAcquireHook>>,
    after_release: Option<Rc<AfterReleaseHook>>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 8,
            min_idle: 2,
            max_lifetime: Duration::from_secs(1800),
            idle_timeout: Duration::from_secs(600),
            acquire_timeout: Duration::from_secs(30),
            command_timeout: None,
            validation_bypass: Duration::from_millis(500),
            after_connect: None,
            before_acquire: None,
            after_release: None,
        }
    }
}

impl PoolConfig {
    /// Create a pool configuration with the default tuning and no callbacks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum number of connections (default: 8).
    pub fn max_size(&mut self, max_size: usize) -> &mut Self {
        self.max_size = max_size;
        self
    }

    /// Get the maximum number of connections.
    #[must_use]
    pub fn get_max_size(&self) -> usize {
        self.max_size
    }

    /// Set the minimum idle connections maintained by the housekeeper
    /// (default: 2).
    pub fn min_idle(&mut self, min_idle: usize) -> &mut Self {
        self.min_idle = min_idle;
        self
    }

    /// Get the minimum idle connections maintained by the housekeeper.
    #[must_use]
    pub fn get_min_idle(&self) -> usize {
        self.min_idle
    }

    /// Set the maximum connection lifetime before forced rotation
    /// (default: 30 min).
    ///
    /// This prevents silent death from firewalls, PostgreSQL idle timeouts, and
    /// DNS failover. Each entry gets a +/-25% jitter so expiries are staggered.
    ///
    /// This is also what bounds a pooled connection's MEMORY. A connection's
    /// read buffer grows to the largest message it has carried and is never
    /// shrunk, so an entry that once served a 50 MB row holds 50 MB until it
    /// rotates - measured, and the same in tokio-postgres. Size a pool for
    /// `max_size * largest expected message`, and shorten this if that product
    /// is uncomfortable.
    ///
    /// Enforced WITHOUT the housekeeper as well: a connection past its lifetime
    /// is discarded when it is returned, not only when background maintenance
    /// sweeps. So this setting takes effect on a pool that never called
    /// [`Pool::start_housekeeper`] - unlike [`PoolConfig::idle_timeout`], which
    /// does not. No-housekeeper checkout rotation is measured and pinned by
    /// `tests/suite/pool_lifetime.rs`.
    pub fn max_lifetime(&mut self, max_lifetime: Duration) -> &mut Self {
        self.max_lifetime = max_lifetime;
        self
    }

    /// Get the maximum connection lifetime.
    #[must_use]
    pub fn get_max_lifetime(&self) -> Duration {
        self.max_lifetime
    }

    /// Set the idle timeout (default: 10 min).
    ///
    /// Connections idle longer than this are closed only when the idle count
    /// exceeds `min_idle`.
    ///
    /// REQUIRES THE HOUSEKEEPER. Idle eviction happens only in background
    /// maintenance, so on a pool that never called [`Pool::start_housekeeper`]
    /// this setting does nothing at all - an idle connection is handed straight
    /// back out however long it sat. That is worth saying here rather than only
    /// on the type, because [`PoolConfig::max_lifetime`] IS enforced without
    /// the housekeeper, and a caller who sets both and watches lifetime
    /// rotation work will reasonably conclude this one is working too.
    pub fn idle_timeout(&mut self, idle_timeout: Duration) -> &mut Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// Get the idle timeout.
    #[must_use]
    pub fn get_idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Set how long `get()` waits for a connection (default: 30 s).
    pub fn acquire_timeout(&mut self, acquire_timeout: Duration) -> &mut Self {
        self.acquire_timeout = acquire_timeout;
        self
    }

    /// Get the connection timeout.
    #[must_use]
    pub fn get_acquire_timeout(&self) -> Duration {
        self.acquire_timeout
    }

    /// Set the client command deadline applied by [`PooledClient::command`].
    ///
    /// This builds clock (1), a client-side command deadline. When it expires,
    /// the pool first keeps a session that is already proven idle and free of
    /// COPY or cancellation authority. Otherwise it sends a `PostgreSQL`
    /// `CancelRequest` using the connector it owns, waits for the postmaster to
    /// close that cancellation connection, then drains through `ReadyForQuery`
    /// before returning a distinguishable [`Error::is_command_timeout`] error.
    /// It is disabled by default.
    ///
    /// It is deliberately separate from the other four clocks:
    ///
    /// - Clock (2), `PostgreSQL`'s server-side `statement_timeout`, is a GUC,
    ///   already reachable with `options=-c statement_timeout=...`.
    /// - Clock (3), [`Config::read_timeout`], bounds post-startup socket-read
    ///   inactivity and retires a session rather than trying to cancel it.
    ///   `tcp_user_timeout` is separate TCP-level unacknowledged-data liveness;
    ///   it does not detect a healthy peer that simply sends nothing.
    /// - Clock (4), [`Config::connect_timeout`], applies per address to
    ///   connection setup, including the handshake.
    /// - Clock (5), [`PoolConfig::acquire_timeout`], limits waiting to
    ///   acquire a pooled connection.
    ///
    /// This is pool policy, not a libpq connection parameter, and therefore is
    /// not accepted in a `PostgreSQL` connection string. See
    /// [`PooledClient::command`] for the exclusive command scope and its
    /// streaming limitations.
    pub fn command_timeout(&mut self, command_timeout: Duration) -> &mut Self {
        self.command_timeout = Some(command_timeout);
        self
    }

    /// Get the configured client command deadline, or `None` when disabled.
    #[must_use]
    pub fn get_command_timeout(&self) -> Option<Duration> {
        self.command_timeout
    }

    /// Set the alive-validation bypass window (default: 500 ms).
    ///
    /// Hot connections used within this window are assumed alive.
    pub fn validation_bypass(&mut self, validation_bypass: Duration) -> &mut Self {
        self.validation_bypass = validation_bypass;
        self
    }

    /// Get the alive-validation bypass window.
    #[must_use]
    pub fn get_validation_bypass(&self) -> Duration {
        self.validation_bypass
    }

    /// Run an asynchronous callback once after each physical connection opens.
    ///
    /// The connection is not made visible to a borrower unless the callback
    /// returns `Ok(())`. An error closes that connection and is returned to an
    /// on-demand caller (or aborts pool warm-up).
    pub fn after_connect<F>(&mut self, hook: F) -> &mut Self
    where
        F: for<'a> Fn(&'a Client) -> PoolHookFuture<'a, Result<(), Error>> + 'static,
    {
        self.after_connect = Some(Rc::new(hook));
        self
    }

    /// Run an asynchronous callback before handing out a RECYCLED connection.
    ///
    /// This is **not** invoked for connections the pool has just opened; use
    /// [`PoolConfig::after_connect`] for those. The split is what keeps a
    /// rejecting hook from turning one checkout into a reconnect storm: with no
    /// idle connection left to fall back on, a rejected fresh connection would
    /// only be replaced by another fresh connection, until `acquire_timeout`.
    ///
    /// `Ok(true)` votes to accept the connection; the pool then rechecks that
    /// it is still inside its lifetime, open, read-healthy, and not owned by an
    /// active COPY operation. `Ok(false)` discards it and retries with the next
    /// idle candidate. An error discards it and fails the checkout.
    pub fn before_acquire<F>(&mut self, hook: F) -> &mut Self
    where
        F: for<'a> Fn(&'a Client) -> PoolHookFuture<'a, Result<bool, Error>> + 'static,
    {
        self.before_acquire = Some(Rc::new(hook));
        self
    }

    /// Run a synchronous keep-or-discard predicate when a live connection is
    /// returned.
    ///
    /// Returning `true` votes to make the connection available for another
    /// checkout; the pool rechecks its lifetime, liveness, and COPY ownership
    /// after the callback. Returning `false` closes it and releases its
    /// capacity slot. The callback is not invoked for a connection already
    /// known to be expired or closed, or for a return during [`Pool::close`]:
    /// shutdown has already chosen to discard that connection, so a reuse
    /// predicate has no decision to make. On an open pool it runs before the
    /// raw-transaction rollback barrier.
    pub fn after_release<F>(&mut self, hook: F) -> &mut Self
    where
        F: Fn(&Client) -> bool + 'static,
    {
        self.after_release = Some(Rc::new(hook));
        self
    }

    async fn run_after_connect(&self, client: &Client) -> Result<(), Error> {
        let Some(hook) = self.after_connect.clone() else {
            return Ok(());
        };
        hook(client).await
    }

    async fn run_before_acquire(&self, client: &Client) -> Result<bool, Error> {
        let Some(hook) = self.before_acquire.clone() else {
            return Ok(true);
        };
        hook(client).await
    }

    fn run_after_release(&self, client: &Client) -> bool {
        let Some(hook) = self.after_release.clone() else {
            return true;
        };
        hook(client)
    }

    fn validate(&self) -> Result<(), Error> {
        // A pool that can hold nothing is not a pool. Refuse this before any
        // connection attempt; otherwise every checkout waits for capacity that
        // can never exist and times out.
        if self.max_size == 0 {
            return Err(Error::config("pool max_size must be at least 1".into()));
        }

        // These are contradictory instructions, not a preference the pool can
        // approximate. Report them rather than silently choosing one value.
        if self.min_idle > self.max_size {
            return Err(Error::config(
                format!(
                    "pool min_idle ({}) exceeds max_size ({}); set min_idle explicitly",
                    self.min_idle, self.max_size
                )
                .into(),
            ));
        }

        Ok(())
    }
}

impl std::fmt::Debug for PoolConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolConfig")
            .field("max_size", &self.max_size)
            .field("min_idle", &self.min_idle)
            .field("max_lifetime", &self.max_lifetime)
            .field("idle_timeout", &self.idle_timeout)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("command_timeout", &self.command_timeout)
            .field("validation_bypass", &self.validation_bypass)
            .field("after_connect", &self.after_connect.is_some())
            .field("before_acquire", &self.before_acquire.is_some())
            .field("after_release", &self.after_release.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Lightweight random (xorshift64*) for lifetime jitter.
//
// Seeded once per thread from the system clock - good enough for load
// spreading, not for security. Avoids pulling in `rand`.
// ---------------------------------------------------------------------------

thread_local! {
    static JITTER_RNG: Cell<u64> = Cell::new({
        // Seed from the system clock (wall-clock nanos) - good enough for
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

/// Compute jittered lifetime: +/-25% of `base`.
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
    fn new(mut client: Client, max_lifetime: Duration) -> Self {
        let now = Instant::now();
        // Pool lifecycle hooks receive `&Client`, so lease scoping must be in
        // place before the first hook can retain a token. Idle and hook tokens
        // remain inactive; checkout installs a fresh active generation only at
        // the final, non-awaiting handoff to a PooledClient.
        client.enter_pool();
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

    fn has_active_copy(&self) -> bool {
        self.client.has_active_copy()
    }

    /// Whether this entry can be committed to a borrower, idle storage, or a
    /// FIFO hand-off right now. Async hooks and validation are caller code and
    /// may change any of these facts while the entry is held out of the pool.
    fn is_pool_eligible(&self) -> bool {
        !self.is_expired()
            && !self.client.is_closed()
            && !self.is_read_retired()
            && !self.has_active_copy()
    }

    /// Whether the connection's read task published terminal poison before
    /// its main task had a chance to drop the Client request receiver.
    fn is_read_retired(&self) -> bool {
        self.client
            .tx_status_handle()
            .load(std::sync::atomic::Ordering::Acquire)
            == crate::connection::READ_RETIRED_STATUS
    }

    fn ineligibility_error(&self, fallback: impl FnOnce() -> Error) -> Error {
        self.client
            .inner()
            .terminal_server_error()
            .unwrap_or_else(fallback)
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Pool metrics - all Cell<u64> since we're single-threaded.
#[derive(Debug)]
pub struct PoolMetrics {
    /// Total connections created since pool start.
    pub connections_created: Cell<u64>,
    /// Total times `get()` timed out waiting for a connection.
    pub timeouts: Cell<u64>,
    /// Total connections evicted (lifetime, idle, broken, or hook rejection).
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

#[derive(Debug)]
struct PoolClosedError;

impl std::fmt::Display for PoolClosedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("pool is closed")
    }
}

impl std::error::Error for PoolClosedError {}

fn pool_closed_error() -> Error {
    Error::connect(io::Error::other(PoolClosedError))
}

impl Error {
    /// Whether this error reports an acquisition rejected by a closed pool.
    #[must_use]
    pub fn is_pool_closed(&self) -> bool {
        let mut source = std::error::Error::source(self);
        while let Some(error) = source {
            if error.is::<PoolClosedError>() {
                return true;
            }
            // `Error::connect` stores an `io::Error`, whose custom payload is
            // available through `get_ref()` but is not exposed as its standard
            // error-chain source on every supported Rust version.
            if let Some(io_error) = error.downcast_ref::<io::Error>()
                && io_error
                    .get_ref()
                    .is_some_and(<dyn std::error::Error + Send + Sync>::is::<PoolClosedError>)
            {
                return true;
            }
            source = error.source();
        }
        false
    }
}

/// Reject a mode that needs a TLS connector this build does not contain.
///
/// This is a build-capability check, not a policy: the contradictions between
/// `sslmode`, `sslrootcert` and `sslnegotiation` are `Config`'s to catch (see
/// `Config::validate_connection_settings`), and they are caught for every entry point,
/// not just this one.
///
/// libpq draws the line in the same place. Compiled without SSL support,
/// "using options `require`, `verify-ca`, or `verify-full` will cause an error,
/// while options `allow` and `prefer` will be accepted but libpq will not
/// actually attempt an SSL connection" - which, without the `tls` feature, is
/// precisely what [`NoTls`] does.
///
/// Answering here rather than at the socket matters for a second reason: this
/// pool retries a failed connection three times with backoff, so connection
/// settings that could never have worked would otherwise cost about two
/// seconds before reporting an error that blames the server.
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

/// The pool's connection recipe, resolved from typed configuration exactly once.
///
/// See the `Transport` section of the module docs for why the connector is
/// built here rather than per attempt. Resolving early also moves a broken
/// `sslrootcert` to where it belongs: pool construction fails immediately
/// naming the file, rather than after the retry loop.
#[derive(Clone)]
struct Transport {
    config: Config,
    /// Present whenever the configuration's `sslmode` may use TLS - i.e. every
    /// mode but `disable`. Which of them *verify* anything is the connector's
    /// business, not the pool's.
    #[cfg(feature = "tls")]
    tls: Option<crate::tls_rustls::MakeRustlsConnect>,
}

impl Transport {
    fn resolve(config: Config) -> Result<Transport, Error> {
        config.validate_connection_settings()?;
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

    /// Send an out-of-band `CancelRequest` with the same transport policy as
    /// this pool's sessions. This public form gives `connect_timeout` ownership
    /// of the complete attempt, including the postmaster-close barrier.
    async fn cancel_query(&self, token: &CancelToken) -> Result<(), Error> {
        #[cfg(feature = "tls")]
        if let Some(tls) = &self.tls {
            return token.cancel_query(tls.clone()).await;
        }

        token.cancel_query(NoTls).await
    }

    /// Pool command-timeout recovery supplies its own whole-recovery grace, so
    /// keep the postmaster-close wait outside `connect_timeout` in this form.
    async fn cancel_query_confirmed(&self, token: &CancelToken) -> Result<(), Error> {
        #[cfg(feature = "tls")]
        if let Some(tls) = &self.tls {
            return token.cancel_query_confirmed(tls.clone()).await;
        }

        token.cancel_query_confirmed(NoTls).await
    }

    /// Retry [`Transport::connect_one`] up to 3 times, sleeping 100ms and then
    /// 400ms between failures. The third attempt is never followed by a sleep,
    /// so the computed 1.6s delay is only ever discarded. Only used for pool
    /// warm-up - `get_inner`'s on-demand connect stays single-shot to keep the
    /// latency budget tight.
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
///
/// Dropping a pool without calling [`Pool::close`] preserves the original
/// immediate RAII behaviour: idle clients and the retained housekeeper handle
/// are dropped, with no asynchronous coordination step. Ordinary ownership
/// keeps the pool alive while a [`PooledClient`] borrows it; deliberately
/// forgetting a borrower bypasses its return path and cannot be recovered.
pub struct Pool {
    transport: Transport,
    config: PoolConfig,
    /// Idle connections available for checkout.
    idle: RefCell<Vec<PoolEntry>>,
    /// Number of connections currently borrowed (not in `idle`).
    active: Cell<usize>,
    /// Occupied capacity slots: idle, active, or in lifecycle machinery.
    /// Used for max_size enforcement.
    total: Cell<usize>,
    /// Callers waiting for a connection, in FIFO order.
    ///
    /// Each live slot is a shared [`WaiterSlot`] the pool can deposit a freed
    /// [`PoolEntry`] directly into - handing the connection straight to the
    /// longest-queued waiter instead of returning it to `idle` where a fresh
    /// caller could barge it (POOL-2 / bb8/deadpool fairness model).
    ///
    /// Cancelled waiters remove their slot by `Rc` identity, so this queue
    /// contains live callers only and cannot accumulate tombstones.
    waiters: RefCell<VecDeque<Rc<WaiterSlot>>>,
    /// Direct hand-offs popped from `waiters` but not yet claimed by their
    /// recipient. Tracking these otherwise-hidden entries lets close discard
    /// and account for them before it returns.
    handoffs: RefCell<Vec<Rc<WaiterSlot>>>,
    /// Irreversible shutdown state. All mutations happen on the owning compio
    /// thread, so a `Cell` is the atomic linearization point for this pool.
    closed: Cell<bool>,
    /// Every in-progress close gets its own wake slot. A single stored Waker
    /// would strand concurrent close callers by overwriting the earlier one.
    close_waiters: RefCell<Vec<Rc<CloseWaiterSlot>>>,
    /// Retaining the handle makes dropping the pool cancel housekeeping even
    /// if the task is blocked in a connection attempt. The task itself holds
    /// only a `Weak<Pool>`, so this field does not form a reference cycle.
    housekeeper: RefCell<Option<compio::runtime::JoinHandle<()>>>,
    /// Observability counters.
    pub metrics: PoolMetrics,
}

impl Pool {
    /// Attempts to cancel the connection identified by a pool lease's token.
    ///
    /// The pool retains the TLS policy maker used for its connections, so this
    /// is the public cancellation path for a token obtained from one of its
    /// borrows. Success means PostgreSQL consumed and closed the dedicated
    /// cancellation connection; an effective cancel is reported as SQLSTATE
    /// `57014` on the original connection.
    ///
    /// The token's lease checks still govern the operation. A token from a
    /// returned borrow is refused, and a cancel racing pool return retires the
    /// physical session rather than risking the next borrower.
    pub async fn cancel_query(&self, token: &CancelToken) -> Result<(), Error> {
        self.transport.cancel_query(token).await
    }

    /// Gracefully close this pool and wait for all borrowed clients to return.
    ///
    /// On its first poll, this method irreversibly marks the pool closed,
    /// rejects future acquisitions, wakes every queued acquisition with a
    /// pool-closed error, discards idle and not-yet-claimed hand-off entries,
    /// and cancels the housekeeper. Connections still held by callers remain
    /// usable until their [`PooledClient`] is dropped; each is then closed
    /// instead of returned to idle. The future completes when `active == 0`.
    ///
    /// Calls are idempotent. Concurrent and later callers join the same drain,
    /// and calls made after the drain return immediately.
    ///
    /// There is intentionally no timeout or force-close variant. A borrower
    /// that is never dropped can keep this future pending forever. Callers may
    /// wrap it in [`compio::time::timeout`]. Once the close future has begun,
    /// cancelling it (including through an elapsed timeout) leaves the pool
    /// closed; existing borrowers remain usable, and a later `close()` resumes
    /// waiting for them. A timeout whose timer wins before polling `close()`
    /// has not begun shutdown. An acquisition already inside an async
    /// connection or hook await is not a borrower and is not awaited; when it
    /// resumes (or is cancelled), its capacity guard discards the candidate.
    pub async fn close(&self) {
        self.begin_close();
        CloseWaiter::new(self).await;
    }

    /// Whether graceful shutdown has begun.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.closed.get() {
            Err(pool_closed_error())
        } else {
            Ok(())
        }
    }

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
        Self::connect_with_pool_config(url, config).await
    }

    /// Create a pool from a URL and full pool configuration.
    ///
    /// The URL is parsed into the driver's typed [`Config`], then construction
    /// delegates to [`Pool::connect_with_config`].
    ///
    /// Opens the configured minimum idle count upfront, or one connection when
    /// that count is zero, so the first burst of traffic does not pay full
    /// connect latency. Each warm-up connection gets up to three attempts,
    /// with 100ms and 400ms delays between failures, to survive Docker
    /// ordering, DNS blips, and brief PG restarts.
    ///
    /// # Errors
    ///
    /// Refuses a `max_size` of 0, and a `min_idle` greater than `max_size`.
    /// Both are configurations the pool cannot honour rather than preferences
    /// it can approximate, and both are cheaper to hear about here than as a
    /// checkout that blocks for `acquire_timeout`.
    pub async fn connect_with_pool_config(
        url: &str,
        pool_config: PoolConfig,
    ) -> Result<Self, Error> {
        // Preserve the URL constructor's error precedence: invalid pool tuning
        // is reported before parsing or resolving the connection recipe.
        pool_config.validate()?;
        let connection_config = url.parse::<Config>()?;
        Self::connect_with_config(connection_config, pool_config).await
    }

    /// Create a pool from typed connection and pool configuration.
    ///
    /// `after_connect` runs on every warm-up connection before the pool is
    /// returned. A callback error closes that connection and every earlier
    /// warm-up connection, then fails construction. The connection
    /// configuration is consumed because the pool retains it for reconnects.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use compio_postgres::config::TargetSessionAttrs;
    /// use compio_postgres::{Config, Error, Pool, PoolConfig};
    ///
    /// async fn connect_pool() -> Result<Pool, Error> {
    ///     let mut connection_config: Config =
    ///         "postgres://postgres@localhost/app".parse()?;
    ///     connection_config.target_session_attrs(TargetSessionAttrs::ReadWrite);
    ///
    ///     let mut pool_config = PoolConfig::new();
    ///     pool_config.max_size(16).min_idle(4);
    ///
    ///     Pool::connect_with_config(connection_config, pool_config).await
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pool or transport configuration, failed
    /// connection warm-up, a rejected `after_connect` callback, or a warm-up
    /// connection that becomes unusable before the pool can be published.
    pub async fn connect_with_config(
        connection_config: Config,
        pool_config: PoolConfig,
    ) -> Result<Self, Error> {
        pool_config.validate()?;
        let transport = Transport::resolve(connection_config)?;

        // `max(1)` keeps a `min_idle` of 0 from making the constructor prove
        // nothing about the connection settings - one connection verifies
        // them. No `min(max_size)`: with both guards above in place,
        // `min_idle` is never greater than `max_size` and `max_size` is never
        // 0, so clamping here could only ever change the `max_size == 0` case,
        // which is now refused rather than turned into a pool that deadlocks.
        let warm = pool_config.min_idle.max(1);
        let mut entries: Vec<PoolEntry> = Vec::with_capacity(warm);
        for i in 0..warm {
            match transport.connect_with_retry().await {
                Ok(client) => {
                    let entry = PoolEntry::new(client, pool_config.max_lifetime);
                    if let Err(e) = pool_config.run_after_connect(&entry.client).await {
                        // The hook rejected this physical connection before it
                        // entered `entries`; dropping it closes the session.
                        drop(entry);
                        drop(entries);
                        return Err(e);
                    }
                    if !entry.is_pool_eligible() {
                        let error = entry.ineligibility_error(|| {
                            pool_error(format!(
                                "warm-up after_connect left connection {} unusable after {i} \
                                 successful connection(s)",
                                i + 1
                            ))
                        });
                        drop(entry);
                        drop(entries);
                        return Err(error);
                    }
                    entries.push(entry);
                }
                Err(e) => {
                    // Drop any already-opened clients (Client drop closes the
                    // sender, the connection task observes it and exits).
                    drop(entries);
                    return Err(e);
                }
            }
        }

        // An earlier entry has sat across every later connect/retry/hook
        // await. Recheck the whole set at the actual publication point; a
        // per-entry post-hook check cannot prove that those earlier entries
        // remained open, read-healthy, COPY-free, and inside their lifetime.
        if let Some(index) = entries.iter().position(|entry| !entry.is_pool_eligible()) {
            let error = entries[index].ineligibility_error(|| {
                pool_error(format!(
                    "warm-up connection {} became unusable before pool publication",
                    index + 1
                ))
            });
            drop(entries);
            return Err(error);
        }

        let total = entries.len();
        let pool = Self {
            transport,
            config: pool_config,
            idle: RefCell::new(entries),
            active: Cell::new(0),
            total: Cell::new(total),
            waiters: RefCell::new(VecDeque::new()),
            handoffs: RefCell::new(Vec::new()),
            closed: Cell::new(false),
            close_waiters: RefCell::new(Vec::new()),
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
    /// pool and cancels even a blocked housekeeping connection attempt. Calling
    /// this after [`Pool::close`] is a no-op.
    pub fn start_housekeeper(self: &std::rc::Rc<Self>) {
        self.start_housekeeper_with_interval(Duration::from_secs(30));
    }

    /// Interval-injectable form used by deterministic lifecycle tests.
    fn start_housekeeper_with_interval(self: &std::rc::Rc<Self>, interval: Duration) {
        if self.closed.get() {
            return;
        }
        let weak = std::rc::Rc::downgrade(self);
        let handle = compio::runtime::spawn(async move {
            loop {
                compio::time::sleep(interval).await;
                if !Self::housekeep(&weak).await {
                    break;
                }
            }
        });
        // Carry a completed handle out before destroying it. `Task::drop` reads
        // a completed output inline (async-task 4.7.1, task.rs:249-264), and a
        // housekeeper panic can carry a caller-owned payload from `after_connect`.
        // Its destructor is therefore arbitrary code and may re-enter the pool.
        let replaced = self.housekeeper.borrow_mut().replace(handle);
        drop(replaced);
    }

    /// Perform the synchronous, one-shot half of graceful shutdown. The closed
    /// bit is set before any other mutation, and every counter/owner transition
    /// is committed before an arbitrary acquisition Waker is invoked.
    fn begin_close(&self) {
        if self.closed.replace(true) {
            return;
        }

        // Dropping a compio JoinHandle cancels its task. Do this after marking
        // closed so a cancellation guard, or a task already queued to resume,
        // can only release capacity and can never refill the pool.
        let housekeeper = self.housekeeper.borrow_mut().take();
        drop(housekeeper);

        let mut discarded_entries = std::mem::take(&mut *self.idle.borrow_mut());
        let queued_waiters: Vec<_> = self.waiters.borrow_mut().drain(..).collect();
        let assigned_handoffs = std::mem::take(&mut *self.handoffs.borrow_mut());
        let mut wakers = Vec::with_capacity(queued_waiters.len() + assigned_handoffs.len());

        for slot in queued_waiters {
            if let Some(waker) = slot.waker.borrow_mut().take() {
                wakers.push(waker);
            }
        }

        // A direct FIFO hand-off has already been popped from `waiters`, but
        // is not active until its recipient polls it out and passes checkout
        // validation. Close owns the decision first: take and discard the
        // entry here, then the recipient observes `closed` on its next poll.
        for slot in assigned_handoffs {
            if let Some(entry) = slot.entry.borrow_mut().take() {
                discarded_entries.push(entry);
            }
            if let Some(waker) = slot.waker.borrow_mut().take() {
                wakers.push(waker);
            }
        }

        self.release_total_slots(discarded_entries.len());
        drop(discarded_entries);

        // Wake all queued callers even if one custom Waker panics. Accounting
        // and ownership are already final, so rethrowing afterwards cannot let
        // any caller acquire an entry shutdown decided to discard.
        wake_all(wakers);
    }

    fn release_total_slots(&self, slots: usize) {
        if slots == 0 {
            return;
        }
        let total = self.total.get();
        debug_assert!(
            slots <= total,
            "pool total underflow: releasing {slots} slot(s) from {total}"
        );
        self.total.set(total.saturating_sub(slots));
    }

    fn discard_unowned_entry(&self, entry: PoolEntry) {
        self.release_total_slots(1);
        drop(entry);
    }

    /// Remove idle entries whose pool-owned lifecycle state already forbids
    /// reuse. Ownership and accounting are committed before destroying a
    /// Client or waking caller code.
    fn reap_ineligible_idle(&self) -> usize {
        let discarded = {
            let mut idle = self.idle.borrow_mut();
            let mut discarded = Vec::new();
            let mut kept = Vec::with_capacity(idle.len());
            for entry in idle.drain(..) {
                if entry.is_pool_eligible() {
                    kept.push(entry);
                } else {
                    discarded.push(entry);
                }
            }
            *idle = kept;
            discarded
        };
        let count = discarded.len();
        if count > 0 {
            self.release_total_slots(count);
            for _ in 0..count {
                self.metrics.inc_evictions();
            }
            self.wake_one_waiter();
        }
        drop(discarded);
        count
    }

    fn remove_handoff_slot(&self, slot: &Rc<WaiterSlot>) {
        let mut handoffs = self.handoffs.borrow_mut();
        if let Some(index) = handoffs
            .iter()
            .position(|assigned| Rc::ptr_eq(assigned, slot))
        {
            handoffs.swap_remove(index);
        }
    }

    fn wake_close_waiters_if_drained(&self) {
        if self.active.get() != 0 {
            return;
        }

        let slots = std::mem::take(&mut *self.close_waiters.borrow_mut());
        let wakers = slots
            .into_iter()
            .filter_map(|slot| slot.waker.borrow_mut().take())
            .collect();
        wake_all(wakers);
    }

    /// Acquire a connection from the pool.
    ///
    /// Tries idle connections first (with alive-bypass validation), then
    /// creates a new connection if under `max_size`, then waits up to
    /// `acquire_timeout` for a connection to be returned. Async lifecycle
    /// hooks are inside that timeout; cancellation discards their candidate and
    /// releases its capacity slot.
    pub async fn get(&self) -> Result<PooledClient<'_>, Error> {
        let entry = self.checkout().await?;
        Ok(PooledClient::new(entry, self))
    }

    /// Acquire a connection whose lease owns an `Rc` of this pool.
    ///
    /// Same acquisition path as [`Pool::get`] - same FIFO fairness, same
    /// `acquire_timeout`, same return-on-drop - but the borrow carries no
    /// lifetime, so a caller can hold it inside an owned, `'static` future.
    /// That is what [`OwnedPooledClient`] exists for: a session that drives raw
    /// `BEGIN`/`COMMIT` and outlives the stack frame that opened it cannot hold
    /// `PooledClient<'a>`, and `Transaction<'a>` (which borrows `&'a mut
    /// Client`) is unavailable for the same reason.
    ///
    /// # Errors
    ///
    /// Identical to [`Pool::get`]: pool closed, acquisition timeout, or a
    /// connection/lifecycle-hook failure.
    pub async fn get_owned(self: &Rc<Self>) -> Result<OwnedPooledClient, Error> {
        let entry = self.checkout().await?;
        Ok(OwnedPooledClient::new(entry, Rc::clone(self)))
    }

    /// The shared acquisition body behind [`Pool::get`] and
    /// [`Pool::get_owned`]: one deadline, one FIFO turn, one entry.
    async fn checkout(&self) -> Result<PoolEntry, Error> {
        self.ensure_open()?;
        match compio::time::timeout(self.config.acquire_timeout, self.get_inner()).await {
            Ok(result) => result,
            Err(_) => {
                // If shutdown raced the timeout, report the terminal state and
                // do not count a capacity timeout. The pool cannot become open
                // again, so this is the more specific and stable answer.
                self.ensure_open()?;
                self.metrics.inc_timeouts();
                Err(pool_error(format!(
                    // `{:?}` on a Duration, not `as_secs()`: that truncated, so
                    // every sub-second acquire timeout described itself as
                    // `0s` - which reads as a misconfigured zero and sends the
                    // caller after the wrong thing. Debug renders `300ms`,
                    // `1.5s`, `2s`.
                    "connection timeout after {:?} (pool: {}/{} idle, {}/{} total)",
                    self.config.acquire_timeout,
                    self.idle.borrow().len(),
                    self.config.max_size,
                    self.total.get(),
                    self.config.max_size,
                )))
            }
        }
    }

    /// `get_inner` wrapped in a borrowed lease, for the tests that drive the
    /// acquisition loop directly (no `acquire_timeout`) and still need the
    /// entry to be returned to the pool on drop. A bare `PoolEntry` has no
    /// `Drop` accounting, so a test holding one would leak `active`.
    #[cfg(test)]
    async fn get_inner_leased(&self) -> Result<PooledClient<'_>, Error> {
        Ok(PooledClient::new(self.get_inner().await?, self))
    }

    async fn get_inner(&self) -> Result<PoolEntry, Error> {
        // A caller that arrives behind an existing waiter must join the FIFO
        // before looking at idle entries or unreserved capacity. A capacity
        // wake is advisory: the head keeps its queue slot until it is polled,
        // so without this turn bit a fresh caller can reserve the freed slot
        // first. Once a waiter removes itself from the head it retains that
        // turn across validation rejection and connection retries.
        let mut has_fifo_turn = self.waiters.borrow().is_empty();
        loop {
            self.ensure_open()?;

            if !has_fifo_turn {
                if let Some(entry) = Waiter::new(self).await {
                    self.idle.borrow_mut().push(entry);
                }
                has_fifo_turn = true;
                continue;
            }

            // 1. Try to pop an idle connection
            let entry = self.idle.borrow_mut().pop();
            if let Some(mut entry) = entry {
                // Adopt the popped slot: it is already counted in `total`, and
                // until a PooledClient owns it (or it is pushed back to idle)
                // its decrement must ride on Drop so cancellation during the
                // dirty barrier / alive validation below - or an eviction
                // `continue` - releases it exactly once. The manual
                // `total -= 1` on the eviction paths is therefore gone; the
                // guard's Drop does it when the loop body unwinds on `continue`.
                let permit = PermitGuard::adopt(self);

                // If the lifetime elapsed, the Client's sender closed, the
                // reader published terminal poison, or COPY still owns the
                // protocol, this entry cannot be handed out.
                if !entry.is_pool_eligible() {
                    self.metrics.inc_evictions();
                    continue;
                }

                // Dirty barrier: if a `Transaction::drop` or a release left a
                // fire-and-forget ROLLBACK on the wire, drain it before handing
                // the client out, so the next caller cannot race against
                // in-flight messages. `simple_query("")` serializes behind the
                // pending command because the Connection processes requests
                // FIFO, so an `Ok` here means every earlier request has reached
                // its own `ReadyForQuery`. An `Err` means this round trip itself
                // did not complete: evict.
                //
                // THAT IS ALL AN `Ok` MEANS, and this comment used to claim
                // more - that the barrier keeps the next caller from inheriting
                // a broken transaction "if the ROLLBACK failed". It cannot see
                // that. The queued command's outcome went to its own dropped
                // response channel, and an empty simple query returns `Ok`
                // inside an open OR an aborted transaction, so the barrier
                // clears `dirty` on a session it never proved idle.
                // `__private_api_rollback` carries the same wrong claim about
                // this code for its encode-failure arm ("the pool's next-get
                // barrier will still detect + evict it").
                //
                // What actually keeps a transaction off the next borrower is
                // `return_client`, which queues a ROLLBACK unconditionally
                // unless the session is provably `Idle` - never this barrier.
                // Making the claim true costs one line (require
                // `transaction_status() == Some(Idle)` after the `Ok` and evict
                // otherwise) but is not here, because with release rolling back
                // unconditionally there is no interleaving that reaches it, and
                // an unreachable guard is a claim in its own right.
                //
                // Runs regardless of `validation_bypass` - a dirty
                // connection must be validated even if last_used is
                // extremely recent, because dirtiness is precisely the case
                // where "last_used was recent" is insufficient.
                if entry.client.is_dirty() {
                    let validation = entry.client.simple_query("").await;
                    self.ensure_open()?;
                    match validation {
                        Ok(_) => entry.client.clear_dirty(),
                        Err(_) => {
                            self.metrics.inc_evictions();
                            continue;
                        }
                    }
                    // The barrier itself provides the alive-check; skip the
                    // second `simple_query("")` below.
                } else if entry.last_used.elapsed() > self.config.validation_bypass {
                    let validation = entry.client.simple_query("").await;
                    self.ensure_open()?;
                    if validation.is_err() {
                        // Alive-bypass validation: a clean connection used
                        // outside the bypass window gets a cheap server round
                        // trip. Dirty connections used the stronger barrier
                        // above instead.
                        self.metrics.inc_evictions();
                        continue;
                    }
                }

                let before_acquire = self.config.run_before_acquire(&entry.client).await;
                self.ensure_open()?;
                match before_acquire {
                    Ok(true) => {}
                    Ok(false) => {
                        self.metrics.inc_evictions();
                        continue;
                    }
                    Err(e) => {
                        self.metrics.inc_evictions();
                        return Err(e);
                    }
                }

                // The hook is arbitrary async caller code. Its `Ok(true)` is
                // a vote to reuse, not authority to override lifecycle facts
                // that changed while it awaited.
                if !entry.is_pool_eligible() {
                    self.metrics.inc_evictions();
                    continue;
                }

                // This check is deliberately adjacent to the active commit.
                // Every awaited path checks above as well, and the
                // single-threaded executor cannot interleave close between
                // these two synchronous statements.
                self.ensure_open()?;
                entry.touch();
                self.active.set(self.active.get() + 1);
                // PooledClient now owns the slot; its Drop -> return_client
                // handles total/active. Disarm so the guard doesn't also
                // decrement total.
                permit.disarm();
                return Ok(entry);
            }

            // 2. No idle connections - create a new one if under limit.
            // Reserve the slot synchronously *before* the await so concurrent
            // callers in the same loop see the bumped `total` and don't race
            // past `max_size`. The reservation rides on a PermitGuard: if this
            // future is cancelled while parked at `connect_one().await` (the
            // outer `acquire_timeout`, or a caller dropping the get()), the
            // guard's Drop releases the slot - without it the `+1` would leak
            // forever (POOL-1). On Err the guard also releases it on return.
            if self.total.get() < self.config.max_size {
                let permit = PermitGuard::reserve(self);
                let connected = self.transport.connect_one().await;
                self.ensure_open()?;
                let client = match connected {
                    Ok(c) => c,
                    Err(e) => {
                        // `permit` drops here -> total -= 1.
                        return Err(e);
                    }
                };
                self.metrics.inc_created();
                let mut entry = PoolEntry::new(client, self.config.max_lifetime);
                let after_connect = self.config.run_after_connect(&entry.client).await;
                self.ensure_open()?;
                if let Err(e) = after_connect {
                    self.metrics.inc_evictions();
                    // `entry` is dropped and `permit` releases the reserved
                    // slot; this connection is never made active or idle.
                    return Err(e);
                }
                if !entry.is_pool_eligible() {
                    self.metrics.inc_evictions();
                    return Err(entry.ineligibility_error(|| {
                        pool_error("after_connect left the new pool connection unusable")
                    }));
                }
                // `before_acquire` is deliberately NOT run here. It is a
                // recycling check -- "is this idle connection still fit to
                // hand out" -- and this connection was opened microseconds ago
                // and already vetted by `after_connect`, which is the hook that
                // owns new connections.
                //
                // Consulting it here was also actively harmful. `Ok(false)`
                // reaches the loop's `continue`, and with no idle entry to fall
                // back on the next iteration opens ANOTHER connection, is
                // refused again, and repeats until `acquire_timeout` -- a full
                // TCP connect plus startup handshake each time. Measured before
                // this change: ~3 connections per 300ms, extrapolating to
                // roughly 300 for a single `get()` at the 30s default. A hook
                // like "reject if the server is in recovery" answers false for
                // every connection during a failover and turns one checkout
                // into sustained load on an already-struggling server.
                //
                // This matches sqlx ("This is _not_ invoked for new
                // connections. Use `after_connect` for those.") and deadpool,
                // whose `recycle` structurally cannot see a new object.
                // Pinned by `before_acquire_is_not_consulted_for_a_freshly_connected_client`.
                self.ensure_open()?;
                entry.touch();
                self.active.set(self.active.get() + 1);
                // PooledClient now owns the slot; its Drop -> return_client
                // handles total/active. Disarm so the guard doesn't also
                // decrement total.
                permit.disarm();
                return Ok(entry);
                // H7 design note: we don't wake a waiter on successful
                // connect. The freshly-connected client is immediately
                // consumed by the current caller - there's no idle entry
                // for a waiter to acquire. Waiters are woken on
                // `return_client`, which is when a borrowed entry
                // actually becomes available.
            }

            // 3. Pool is full - park in the FIFO wait queue. Resolves either
            // with a connection handed *directly* to us by `return_client`
            // (bypassing `idle`, so no fresh caller can barge it - POOL-2), or
            // with `None` meaning capacity/idle opened up and we should loop.
            if let Some(entry) = Waiter::new(self).await {
                // A direct hand-off still needs the same expiry, closed, dirty,
                // and age checks as an idle entry. Put it where the next loop
                // iteration will synchronously pop it through that common path.
                // There is no await between this deposit and the pop, so a fresh
                // caller cannot barge ahead of this waiter.
                self.idle.borrow_mut().push(entry);
            }
            has_fifo_turn = true;
            // else: woken for capacity/idle - loop and retry the acquire.
        }
    }

    /// Return a connection to the pool (called by `PooledClient::drop`).
    fn return_client(&self, mut entry: PoolEntry) {
        // The lease boundary is an authority boundary. Revoke before any hook
        // or pool publication. If a token escaped this lease, retire the
        // physical session instead of allowing that backend-wide credential to
        // target a later borrower. A cancellation already in progress also
        // holds the Arc, so this covers the load-before-revoke race.
        entry.client.revoke_pool_cancel_lease();
        // The returning client is no longer active. (When the entry is handed
        // directly to a waiter below, checkout re-bumps `active` only after
        // validating it, so a successful hand-off nets zero.)
        self.active.set(self.active.get().saturating_sub(1));
        // The entry is counted in `total` but is neither active nor available
        // until this synchronous return path decides its fate. The guard makes
        // rejection and hook panic release that slot exactly once and wake the
        // FIFO head; successful redeposit disarms it.
        let permit = ReturnPermitGuard::adopt(self);

        // `after_release` is a reuse predicate. Once shutdown has begun there
        // is no keep/discard decision left to delegate to user code: close the
        // session, release its exact capacity slot, then notify every close
        // caller if this was the last borrower.
        if self.closed.get() {
            drop(entry);
            drop(permit);
            self.wake_close_waiters_if_drained();
            return;
        }

        if entry.client.pool_cancel_lease_prevents_reuse() {
            entry.client.force_close();
            self.metrics.inc_evictions();
            return;
        }

        // Eviction criteria:
        //   - expired (max_lifetime reached)
        //   - closed: Client::is_closed() indicates the connection task exited
        //   - read-retired: the dedicated reader synchronously marked poison
        //     before its main task could close the Client channel
        if !entry.is_pool_eligible() {
            self.metrics.inc_evictions();
            return;
        }

        // `Drop` cannot await cleanup. The release hook is therefore a
        // synchronous keep-or-discard predicate, and the entry stays invisible
        // to both `idle` and waiter slots until it returns. False drops the
        // session; closing it also rolls back any open transaction.
        let keep = self.config.run_after_release(&entry.client);
        // Re-check after arbitrary hook code. Re-entry is forbidden by the
        // hook contract, and the synchronous hook cannot otherwise interleave
        // a close on this single-threaded path. Only forbidden re-entry can
        // make this arm observable; keeping it preserves shutdown accounting
        // if a violating hook closes the pool before returning.
        //
        // THE ARM IS REACHED; THE WAKE INSIDE IT IS NOT. Measured 2026-08-31:
        // `panic!` here fails exactly one test,
        // `reentrant_close_from_after_release_cannot_redeposit_after_shutdown`,
        // which simulates the forbidden re-entry. But deleting the
        // `wake_close_waiters_if_drained()` call fails NOTHING, because in that
        // state there is no parked close waiter to wake: `close()` runs
        // `begin_close()` BEFORE awaiting `CloseWaiter`, so any parked waiter
        // implies the pool was already closed when the return started - and
        // then the PRE-hook arm above fires instead of this one. Observing this
        // wake needs a hook that both re-enters AND leaves its own close
        // parked, i.e. two stacked contract violations. The call stays for
        // accounting safety, not because a test can pin it; a mutation report
        // calling it unbound is correct and needs no new test.
        if self.closed.get() {
            drop(entry);
            drop(permit);
            self.wake_close_waiters_if_drained();
            return;
        }
        if !keep {
            self.metrics.inc_evictions();
            return;
        }
        if !entry.is_pool_eligible() {
            self.metrics.inc_evictions();
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
        // `dirty` does NOT license skipping this, and until 2026-08-23 it did:
        // `if !is_dirty() && status != Some(Idle)`. The flag means "a
        // fire-and-forget command was queued and nobody observed its outcome",
        // and NOTHING on the session clears it - not an awaited `query`, not an
        // awaited `batch_execute`. Only the next checkout's barrier, or an
        // explicit `Transaction::commit`/`rollback`, does. So the flag outlives
        // the command it describes, and read here as "a ROLLBACK is already
        // queued" it suppressed the rollback for a transaction opened AFTER
        // that one: abandon a `Transaction`, then `batch_execute("BEGIN; INSERT
        // ...")`, then release, and the next borrower inherited the open
        // transaction and its uncommitted row. The checkout barrier then
        // laundered the state rather than catching it - an empty `simple_query`
        // succeeds inside a transaction, so it returned Ok and cleared `dirty`
        // without ending anything.
        //
        // There is no cheap accurate version of that test. Restricting the skip
        // to `transaction_status() == None` is not enough either: `None` only
        // says some request is unsettled, never that the unsettled one is the
        // ROLLBACK rather than a `BEGIN` a cancelled borrower left in flight
        // behind it. So the correctness action is unconditional, and a
        // heuristic flag no longer gets a veto over it. The cost is one
        // redundant ROLLBACK frame on a session that is not provably idle -
        // fire-and-forget, pipelined behind the one already queued, drained by
        // the same checkout barrier, and answered with a `no transaction in
        // progress` warning rather than an error. A session released while
        // provably `Idle` - the overwhelmingly common case - still sends
        // nothing.
        if entry.client.transaction_status() != Some(TransactionStatus::Idle) {
            entry.client.__private_api_rollback(None);
        }

        // Live connection. Hand it DIRECTLY to the longest-queued waiter if one
        // exists - bypassing `idle` so a fresh, never-parked caller cannot pop
        // it first (POOL-2 FIFO fairness). Otherwise park it in `idle`.
        entry.touch();
        let waker = self.deposit_freed_entry(entry);
        // The entry is now owned by idle or a waiter and remains counted. Commit
        // that accounting before invoking an arbitrary Waker implementation.
        permit.disarm();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Handle an un-owned [`PoolEntry`] reclaimed by `Waiter::drop` after a
    /// handoff was deposited into its slot but cancelled before being polled
    /// out. An eligible entry goes directly to the front live waiter, or to
    /// `idle` if none is waiting; it remains counted in `total`, and `active`
    /// is bumped only after the next checkout's validation succeeds. An
    /// ineligible entry releases its `total` slot and records an eviction here.
    ///
    /// This is only the waiter-reclaim path. `return_client` does NOT come
    /// through here; it calls the sibling `deposit_freed_entry` under an armed
    /// `ReturnPermitGuard`.
    fn redeposit_freed_entry(&self, entry: PoolEntry) {
        if self.closed.get() {
            self.discard_unowned_entry(entry);
            return;
        }
        if !entry.is_pool_eligible() {
            // An assigned handoff can dwell across arbitrary connection-task
            // progress before its waiter is cancelled. Recheck at this second
            // publication boundary instead of advertising a corpse until a
            // later checkout or housekeeping pass happens to reap it.
            self.release_total_slots(1);
            self.metrics.inc_evictions();
            self.wake_one_waiter();
            drop(entry);
            return;
        }
        if let Some(waker) = self.deposit_freed_entry(entry) {
            waker.wake();
        }
    }

    /// Transfer ownership to idle or the FIFO head without invoking its waker.
    /// Callers with an armed accounting permit use this as their commit point:
    /// disarm after this returns, then wake, so a panicking waker cannot make a
    /// deposited entry disappear from `total`.
    fn deposit_freed_entry(&self, entry: PoolEntry) -> Option<Waker> {
        debug_assert!(!self.closed.get(), "deposited an entry into a closed pool");
        match self.take_front_waiter() {
            Some(slot) => {
                // Deposit into the waiter's rendezvous slot and wake it. The
                // waiter has been popped from the queue (it is no longer
                // "waiting"); it now owns the right to this entry via its
                // retained `Rc<WaiterSlot>` clone.
                *slot.entry.borrow_mut() = Some(entry);
                self.handoffs.borrow_mut().push(Rc::clone(&slot));
                slot.waker.borrow_mut().take()
            }
            None => {
                self.idle.borrow_mut().push(entry);
                None
            }
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
        if self.closed.get() {
            return;
        }
        let slot = self.waiters.borrow().front().cloned();
        // Take the waker OUT of the borrow before waking. As the scrutinee of an
        // `if let` the `RefMut` would still be live inside the body: Rust 2024
        // moved that drop ahead of the `else` block, not ahead of the `then`
        // block. A waker that synchronously re-enters the pool and reaches this
        // slot would then hit `BorrowMutError`, and the unwind escapes before
        // `return_client` disarms its permit, so `total` is decremented for a
        // connection that is still alive. Closing over the take keeps the
        // temporary inside the closure, which matches `wake_all` collecting into
        // a `Vec` and `return_client` taking the waker as a return value.
        //
        // THE WHOLE CLASS WAS SWEPT 2026-08-26, not just this line: every site
        // that hands control to arbitrary code - the six `wake` calls here and
        // in `buf_stream.rs`, plus the `before_acquire`/`after_release` hooks -
        // was checked for a live borrow at the call. This was the only one out
        // of step, and `buf_stream::wake_reader` already carried the rationale
        // in as many words ("End the RefCell borrow before invoking an arbitrary
        // waker"). So the hazard was known and this site simply missed it; a
        // fix that stopped at one call site would have been the real risk.
        let waker = slot.and_then(|slot| slot.waker.borrow_mut().take());
        if let Some(w) = waker {
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
        !self.closed.get()
            && (!self.idle.borrow().is_empty() || self.total.get() < self.config.max_size)
    }

    /// Run one housekeeper cycle. A strong pool reference is held only while
    /// reading or mutating pool state, never across a connection await.
    /// Returns false once the pool has been dropped or closed.
    async fn housekeep(weak: &Weak<Self>) -> bool {
        let Some(pool) = weak.upgrade() else {
            return false;
        };
        if pool.closed.get() {
            return false;
        }

        // Take counts and do all mutations inside tight borrows. Release
        // every borrow before awaiting.

        let (before, evicted_unusable, evicted_idle, discarded) = {
            let mut idle = pool.idle.borrow_mut();
            let before = idle.len();

            // Evicted entries are CARRIED OUT, not dropped here. Dropping a
            // `PoolEntry` inside this borrow runs arbitrary user code: the entry
            // owns a `Client`, whose `QueryObserver` owns the `QueryEvent`
            // sender, and dropping the last sender calls
            // `recv_task.wake()` (futures-channel 0.3.32, mpsc/mod.rs:969 ->
            // :515). That waker belongs to whoever polls the PUBLIC
            // `Client::query_events`, so it is user code reachable from safe
            // Rust via `impl Wake`.
            //
            // A waker that touches the pool would panic here with
            // `BorrowMutError`, and the unwind would escape BEFORE the
            // accounting below - so `total` would never be decremented for
            // entries that are already gone, and the pool would permanently
            // believe it holds capacity it does not.
            let mut discarded: Vec<PoolEntry> = Vec::new();

            // 1. Evict entries that are expired or otherwise already known to
            // be unusable.
            let mut evicted_unusable = 0usize;
            let mut kept = Vec::with_capacity(idle.len());
            for entry in idle.drain(..) {
                if !entry.is_pool_eligible() {
                    evicted_unusable += 1;
                    discarded.push(entry);
                } else {
                    kept.push(entry);
                }
            }
            *idle = kept;

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
                        discarded.push(idle.swap_remove(idx));
                        evicted_idle += 1;
                    }
                    _ => break,
                }
            }

            (before, evicted_unusable, evicted_idle, discarded)
        };

        let mut evicted = evicted_unusable + evicted_idle;
        if evicted > 0 {
            let cur = pool.total.get();
            pool.total.set(cur.saturating_sub(evicted));
            for _ in 0..evicted {
                pool.metrics.inc_evictions();
            }
            pool.wake_one_waiter();
        }
        // Destroyed here: the `idle` borrow is released AND the accounting
        // above is committed, so a destructor that unwinds cannot leave `total`
        // claiming capacity these entries no longer represent.
        drop(discarded);

        // 3. Refill to min_idle. No retry here - if connect fails, back off
        // until the next 30s tick. Both idle demand and total capacity are
        // recomputed before EVERY reservation: a checkout can consume an
        // earlier refill while a later connection or hook is awaiting, so the
        // pre-await deficit is not a durable loop budget.
        drop(pool);

        let mut created = 0usize;
        loop {
            // RE-CHECK BOTH BOUNDS. A budget computed before the first
            // `connect_one().await` stops being true at that await:
            // `get_inner` can reserve capacity or consume an idle refill while
            // this task is parked. The on-demand path never had this problem
            // because it checks and reserves with no await between the two.
            // Re-upgrade only long enough to recheck and reserve capacity and
            // clone the connection recipe. The weak permit accounts for an
            // error or cancellation without retaining the pool.
            let (transport, pool_config, permit) = {
                let Some(pool) = weak.upgrade() else {
                    return false;
                };
                if pool.closed.get() {
                    return false;
                }
                // Entries deposited by an earlier iteration have crossed the
                // later iteration's connection and hook awaits. Reap any that
                // became ineligible before using the live idle count as the
                // refill stop condition.
                evicted += pool.reap_ineligible_idle();
                if pool.closed.get() {
                    return false;
                }
                if pool.idle.borrow().len() >= pool.config.min_idle
                    || pool.total.get() >= pool.config.max_size
                {
                    break;
                }
                let transport = pool.transport.clone();
                let pool_config = pool.config.clone();
                let permit = WeakPermitGuard::reserve(&pool);
                (transport, pool_config, permit)
            };

            match transport.connect_one().await {
                Ok(client) => {
                    let Some(pool) = weak.upgrade() else {
                        return false;
                    };
                    if pool.closed.get() {
                        return false;
                    }
                    pool.metrics.inc_created();
                    drop(pool);

                    let entry = PoolEntry::new(client, pool_config.max_lifetime);
                    let after_connect = pool_config.run_after_connect(&entry.client).await;
                    let Some(pool) = weak.upgrade() else {
                        return false;
                    };
                    if pool.closed.get() {
                        return false;
                    }
                    drop(pool);
                    if let Err(e) = after_connect {
                        if let Some(pool) = weak.upgrade()
                            && !pool.closed.get()
                        {
                            pool.metrics.inc_evictions();
                        }
                        eprintln!(
                            "[compio-postgres] housekeeper: after_connect rejected connection: {e}"
                        );
                        // `entry` closes and `permit` releases the reserved
                        // slot before the next housekeeper tick.
                        break;
                    }

                    if !entry.is_pool_eligible() {
                        let error = entry.ineligibility_error(|| {
                            pool_error("after_connect left connection unusable")
                        });
                        if let Some(pool) = weak.upgrade()
                            && !pool.closed.get()
                        {
                            pool.metrics.inc_evictions();
                        }
                        eprintln!(
                            "[compio-postgres] housekeeper: after_connect left connection \
                             unusable: {error:?}"
                        );
                        // `entry` closes and `permit` releases the reserved
                        // slot before the next housekeeper tick.
                        break;
                    }

                    let Some(pool) = weak.upgrade() else {
                        return false;
                    };
                    if pool.closed.get() {
                        return false;
                    }
                    created += 1;
                    let waker = pool.deposit_freed_entry(entry);
                    // The entry is now idle or assigned to the head waiter and
                    // remains counted in `total`; disarm the reservation.
                    permit.disarm();
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }
                Err(e) => {
                    // `permit` drops here -> total -= 1.
                    if weak.upgrade().is_some_and(|pool| pool.closed.get()) {
                        return false;
                    }
                    eprintln!("[compio-postgres] housekeeper: failed to create connection: {e}");
                    break;
                }
            }
        }

        if evicted > 0 || created > 0 {
            let Some(pool) = weak.upgrade() else {
                return false;
            };
            if pool.closed.get() {
                return false;
            }
            let after_idle = pool.idle.borrow().len();
            let active = pool.active.get();
            let total = pool.total.get();
            eprintln!(
                "[compio-postgres] housekeeper: before={before}, evicted={evicted}, \
                 created={created}, idle={after_idle}, active={active}, total={total}",
            );
        }

        weak.upgrade().is_some_and(|pool| !pool.closed.get())
    }

    // -- Convenience methods ----------------------------------------------

    /// Acquire a connection, run a query, return the connection.
    pub async fn query(
        &self,
        sql: &str,
        params: &[&(dyn crate::types::ToSql + Sync)],
    ) -> Result<Vec<crate::Row>, Error> {
        let mut client = self.get().await?;
        client
            .command(async |client| client.query(sql, params).await)
            .await
    }

    /// Query with text-format string parameters. Parse is sent without type
    /// hints, so the server infers each parameter's type from its SQL position;
    /// Bind then sends each value in text format.
    pub async fn query_text_params(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<crate::Row>, Error> {
        let mut client = self.get().await?;
        client
            .command(async |client| client.query_text_params(sql, params).await)
            .await
    }

    /// Acquire a connection, execute a statement, return the connection.
    ///
    /// Uses the **extended/prepared** protocol - exactly ONE command per call.
    /// Multi-statement SQL (e.g. DDL with a trailing `COMMENT ON COLUMN`
    /// sentinel) raises `42601 cannot insert multiple commands into a prepared
    /// statement`; use [`Pool::batch_execute`] for those.
    pub async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn crate::types::ToSql + Sync)],
    ) -> Result<u64, Error> {
        let mut client = self.get().await?;
        client
            .command(async |client| client.execute(sql, params).await)
            .await
    }

    /// Acquire a connection and run one or more `;`-separated statements via the
    /// **simple-query** protocol, then return the connection.
    ///
    /// This is the correct primitive for multi-statement DDL - e.g. the
    /// `CREATE TABLE ...; COMMENT ON COLUMN ... IS 'zsenc:...'` / `'__zsmask:...'`
    /// sentinel batches the schema builder emits (P4 HALF A / P5.5 PR 6).
    /// `Pool::execute` cannot run those (it prepares a single command).
    ///
    /// # Errors
    /// Returns [`Error`] if a connection cannot be acquired or the server
    /// rejects any statement in the batch.
    pub async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        let mut client = self.get().await?;
        client
            .command(async |client| client.batch_execute(sql).await)
            .await
    }

    // -- Pool stats (for metrics endpoint) --------------------------------

    /// Number of idle connections available.
    pub fn idle_count(&self) -> usize {
        self.idle.borrow().len()
    }

    /// Number of connections currently borrowed.
    pub fn active_count(&self) -> usize {
        self.active.get()
    }

    /// Total occupied capacity slots.
    ///
    /// This is normally idle plus active connections. It also includes a
    /// connection being opened, validated, or passed through an async hook so
    /// those in-flight candidates cannot race past `max_size`.
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
/// the timer wins - so a post-await `total -= 1` statement is skipped on
/// cancellation, leaking the permit forever (POOL-1). Tying the decrement to
/// `Drop` makes it fire on every exit: success, error, early return, panic,
/// and - crucially - cancellation. The guard is `disarm()`ed once a
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

    /// Adopt an EXISTING slot already counted in `total` - a `PoolEntry`
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

/// Accounting guard for the synchronous return path.
///
/// A returned entry has already left `active`, but remains counted in `total`
/// while expiry, liveness, and `after_release` decide whether it can be reused.
/// Drop releases that slot and wakes the FIFO head on every discard or unwind.
/// A successful redeposit disarms the guard after the entry becomes available.
struct ReturnPermitGuard<'a> {
    pool: &'a Pool,
    armed: bool,
}

impl<'a> ReturnPermitGuard<'a> {
    fn adopt(pool: &'a Pool) -> Self {
        Self { pool, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ReturnPermitGuard<'_> {
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
        let idle = self.idle.borrow().len();
        let waiters = self.waiters.borrow().len();
        let handoffs = self.handoffs.borrow().len();
        let close_waiters = self.close_waiters.borrow().len();
        f.debug_struct("Pool")
            .field("connection_config", &"***")
            .field("idle", &idle)
            .field("active", &self.active.get())
            .field("total", &self.total.get())
            .field("max_size", &self.config.max_size)
            .field("waiters", &waiters)
            .field("handoffs", &handoffs)
            .field("closed", &self.closed.get())
            .field("close_waiters", &close_waiters)
            .field("pool_config", &self.config)
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
    T: compio::io::AsyncRead
        + compio::io::AsyncWrite
        + Unpin
        + crate::buf_stream::SplitStream
        + 'static,
    <T as crate::buf_stream::SplitStream>::ReadHalf: 'static,
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
// Waiter - drop-safe slot in the wait queue with direct connection hand-off
// ---------------------------------------------------------------------------

/// Shared rendezvous between a parked [`Waiter`] and `return_client`.
///
/// `return_client` deposits a freed [`PoolEntry`] into `entry` and wakes the
/// waiter via `waker`, handing the connection *directly* to the longest-queued
/// caller instead of pushing it to `idle` (which a fresh, never-parked caller
/// could pop first - the POOL-2 barge). The slot is an `Rc` so the pool holds
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
///   - `Some(entry)` - `return_client` handed us a connection directly; the
///     caller must run it through the common checkout checks.
///   - `None` - we were woken because capacity opened up or an idle entry
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
    /// queue entry on hand-off - so `poll`/`drop` can still see a deposited
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
        // Shutdown takes priority over a direct hand-off. `begin_close`
        // discards tracked assigned entries before waking their recipients; a
        // defensive reclaim in Drop handles any entry deposited by a broken
        // future caller without ever exposing it to checkout.
        if self.pool.closed.get() {
            return Poll::Ready(None);
        }

        // RawWaker's clone vtable is caller code. Clone before inspecting the
        // hand-off and queue state, then inspect both from scratch in case the
        // callback re-entered the pool. Re-check close for the same reason.
        let new_waker = cx.waker().clone();
        if self.pool.closed.get() {
            return Poll::Ready(None);
        }

        // 1. A connection handed directly to us takes priority - claim it and
        // remove its otherwise-hidden ownership record.
        if let Some(slot) = self.slot.as_ref().map(Rc::clone) {
            let entry = slot.entry.borrow_mut().take();
            if let Some(entry) = entry {
                self.pool.remove_handoff_slot(&slot);
                return Poll::Ready(Some(entry));
            }
        }

        // 2. Register or refresh our FIFO slot before consulting global
        // availability. A later waiter must remain parked even if it receives a
        // spurious poll while the head owns the first claim.
        let current_slot = self.slot.as_ref().map(Rc::clone);
        let mut waiters = self.pool.waiters.borrow_mut();

        // The waker this poll REPLACES, carried out so it is destroyed with no
        // borrow held. Assigning through the `RefMut` would drop the old
        // `Waker` while both this borrow and `waiters` are live, and a `Waker`
        // is arbitrary caller code: its `Drop` re-entering the pool would hit
        // `BorrowError`. Same rule as waking outside the borrow in
        // `wake_one_waiter`, applied to the destructor rather than the wake.
        let mut replaced: Option<Waker> = None;

        if let Some(slot) = current_slot
            && waiters.iter().any(|queued| Rc::ptr_eq(queued, &slot))
        {
            let mut w = slot.waker.borrow_mut();
            match &*w {
                Some(existing) if existing.will_wake(&new_waker) => {}
                _ => replaced = w.replace(new_waker),
            }
        } else {
            // First poll, or our slot was popped for a direct hand-off and we
            // need to re-park. Register a fresh live slot at the back.
            let slot = WaiterSlot::new(new_waker);
            waiters.push_back(Rc::clone(&slot));
            self.slot = Some(slot);
        }
        drop(waiters);
        // Now that no borrow is held, letting an arbitrary destructor run is
        // safe.
        drop(replaced);

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
        if let Some(slot) = self.slot.take() {
            let entry = slot.entry.borrow_mut().take();
            if let Some(entry) = entry {
                self.pool.remove_handoff_slot(&slot);
                // On an open pool this preserves the existing reclaim and FIFO
                // semantics. During close it discards the entry and releases
                // its `total` slot instead of making it visible again.
                self.pool.redeposit_freed_entry(entry);
            }
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

fn wake_all(wakers: Vec<Waker>) {
    let mut first_panic: Option<Box<dyn std::any::Any + Send>> = None;
    for waker in wakers {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()))
            && first_panic.is_none()
        {
            first_panic = Some(payload);
        }
    }
    if let Some(payload) = first_panic {
        std::panic::resume_unwind(payload);
    }
}

// ---------------------------------------------------------------------------
// Close waiter - cancellation-safe multi-caller active drain
// ---------------------------------------------------------------------------

struct CloseWaiterSlot {
    waker: RefCell<Option<Waker>>,
}

impl CloseWaiterSlot {
    fn new(waker: Waker) -> Rc<Self> {
        Rc::new(Self {
            waker: RefCell::new(Some(waker)),
        })
    }
}

struct CloseWaiter<'a> {
    pool: &'a Pool,
    slot: Option<Rc<CloseWaiterSlot>>,
}

impl<'a> CloseWaiter<'a> {
    fn new(pool: &'a Pool) -> Self {
        Self { pool, slot: None }
    }

    fn remove_slot(&self) {
        let Some(slot) = &self.slot else {
            return;
        };
        let mut close_waiters = self.pool.close_waiters.borrow_mut();
        if let Some(index) = close_waiters
            .iter()
            .position(|registered| Rc::ptr_eq(registered, slot))
        {
            close_waiters.swap_remove(index);
        }
    }
}

impl Future for CloseWaiter<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.pool.active.get() == 0 {
            return Poll::Ready(());
        }

        // RawWaker's clone vtable is caller code. Clone before borrowing either
        // registration container, then re-check the completion condition in
        // case that callback returned the last active client.
        let new_waker = cx.waker().clone();
        if self.pool.active.get() == 0 {
            return Poll::Ready(());
        }

        let current_slot = self.slot.as_ref().map(Rc::clone);
        let mut close_waiters = self.pool.close_waiters.borrow_mut();
        // Carried out for the same reason as in `Waiter::poll`: a replaced
        // `Waker` is arbitrary caller code and must not be destroyed while a
        // borrow is held.
        let mut replaced: Option<Waker> = None;
        if let Some(slot) = current_slot
            && close_waiters
                .iter()
                .any(|registered| Rc::ptr_eq(registered, &slot))
        {
            let mut waker = slot.waker.borrow_mut();
            match &*waker {
                Some(existing) if existing.will_wake(&new_waker) => {}
                _ => replaced = waker.replace(new_waker),
            }
        } else {
            let slot = CloseWaiterSlot::new(new_waker);
            close_waiters.push(Rc::clone(&slot));
            self.slot = Some(slot);
        }
        drop(close_waiters);
        drop(replaced);

        Poll::Pending
    }
}

impl Drop for CloseWaiter<'_> {
    fn drop(&mut self) {
        self.remove_slot();
    }
}

// ---------------------------------------------------------------------------
// PooledClient
// ---------------------------------------------------------------------------

/// How long timeout recovery may spend sending `CancelRequest` and proving that
/// the original session reached `ReadyForQuery`.
///
/// This is a bounded cleanup grace after clock (1), not a sixth user-facing
/// deadline. Without it, a hung cancellation connection or a lost packet could
/// turn an expired command deadline into an indefinitely pending future. If it
/// expires, the pool synchronously shuts down and later evicts the session.
const COMMAND_TIMEOUT_RECOVERY_GRACE: Duration = Duration::from_secs(5);

/// Retires a physical session whose command-timeout recovery did not finish.
///
/// Recovery is the one place a pooled session is knowingly left mid-protocol:
/// a CancelRequest is in flight and the backend has not yet been proven idle.
/// Every exit from that state has to either complete the recovery or destroy
/// the session, and a dropped future takes neither branch on its own.
struct CommandRecoveryGuard<'a> {
    client: &'a Client,
    armed: bool,
}

impl<'a> CommandRecoveryGuard<'a> {
    fn new(client: &'a Client) -> Self {
        Self {
            client,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CommandRecoveryGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.client.force_close();
        }
    }
}

/// A borrowed connection that returns to the pool on drop.
///
/// Dereferences to [`Client`] - call any client method (`.query(...)`,
/// `.execute(...)`, `.transaction()`, ...) directly on the borrow. Those direct
/// calls retain bare-Client semantics and do not acquire a command deadline;
/// use [`PooledClient::command`] to apply this pool's configured deadline.
/// [`Pool::query`], [`Pool::execute`], and the other Pool convenience methods
/// enter that scope automatically.
///
/// A [`CancelToken`] obtained through this borrow is lease-scoped. Returning
/// the borrow revokes the token; if it is retained, the pool retires the
/// physical session instead of letting the token target its next borrower.
pub struct PooledClient<'a> {
    entry: Option<PoolEntry>,
    pool: &'a Pool,
}

impl PooledClient<'_> {
    fn new(mut entry: PoolEntry, pool: &Pool) -> PooledClient<'_> {
        entry.client.activate_pool_cancel_lease();
        PooledClient {
            entry: Some(entry),
            pool,
        }
    }

    /// Run one exclusive logical command under the pool's command deadline.
    ///
    /// If [`PoolConfig::command_timeout`] is configured, expiry first checks
    /// whether the timed operation left any unsettled protocol work or cancel
    /// authority. A session already proven idle is kept without sending a
    /// cancellation. Otherwise expiry sends a real `PostgreSQL` `CancelRequest`
    /// over a new connection using the pool-owned TLS connector. It waits for
    /// the postmaster to close that dedicated connection so a late cancel cannot
    /// hit the next query. The timed operation future is dropped, but the
    /// connection task continues draining its server responses. This method then
    /// sends a FIFO `Sync` barrier and, if needed, rolls back a cancelled
    /// transaction. It does not return until the same session is idle at
    /// `ReadyForQuery`. A successful recovery returns an
    /// [`Error`] for which [`Error::is_command_timeout`] is true; `PostgreSQL`'s
    /// own errors, including SQLSTATE `57014`, remain ordinary database
    /// errors.
    ///
    /// `&mut self` makes the timeout's backend-wide `CancelRequest` exclusive to
    /// this pooled lease. The closure should still run one sequential logical
    /// command: deliberately starting pipelined requests inside it defeats the
    /// one-command targeting guarantee because `PostgreSQL` cancellation names a
    /// backend, not an individual request.
    ///
    /// A streaming command is covered only while the returned stream or `COPY`
    /// handle is consumed inside the closure. Returning that handle makes the
    /// closure finish and therefore ends this deadline scope.
    ///
    /// If sending `CancelRequest` fails, or recovery does not prove
    /// `ReadyForQuery` within a bounded grace period, the physical socket is
    /// synchronously shut down. The timeout remains distinguishable through
    /// [`Error::is_command_timeout`], its source describes the recovery
    /// failure, and this pool entry is evicted rather than reused.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use compio_postgres::{Error, PooledClient};
    /// async fn load(client: &mut PooledClient<'_>) -> Result<i32, Error> {
    ///     client
    ///         .command(async |client| {
    ///             client
    ///                 .query_one_scalar("SELECT 42::int4", &[])
    ///                 .await
    ///         })
    ///         .await
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the operation's own [`Error`] when it finishes before the
    /// deadline. On expiry, returns an error classified by
    /// [`Error::is_command_timeout`]; a source error means cancellation could
    /// not prove the session reusable and the physical connection was retired.
    pub async fn command<T, F>(&mut self, operation: F) -> Result<T, Error>
    where
        F: for<'client> AsyncFnOnce(&'client mut Client) -> Result<T, Error>,
    {
        let Some(entry) = self.entry.as_mut() else {
            // `entry` is taken only by Drop, which cannot race a method call.
            // Keep the impossible state an ordinary closed-client error rather
            // than putting a panic edge on a public database operation.
            return Err(Error::closed());
        };
        run_pool_command(self.pool, &mut entry.client, operation).await
    }
}

/// The deadline + cancellation-recovery body shared by
/// [`PooledClient::command`] and [`OwnedPooledClient::command`].
///
/// It lives outside both types because the two leases differ only in how they
/// hold the pool (`&'a Pool` versus `Rc<Pool>`); duplicating the recovery
/// state machine is how the two would drift, and the arm that decides whether a
/// mid-`CancelRequest` session is retired or reused is the last place in this
/// file that should exist twice.
async fn run_pool_command<T, F>(pool: &Pool, client: &mut Client, operation: F) -> Result<T, Error>
where
    F: for<'client> AsyncFnOnce(&'client mut Client) -> Result<T, Error>,
{
    let command_timeout = pool.config.command_timeout;
    let transport = &pool.transport;

    let Some(command_timeout) = command_timeout else {
        return operation(client).await;
    };

    if let Ok(result) = compio::time::timeout(command_timeout, operation(client)).await {
        return result;
    }

    // A settled Idle status proves every transaction-capable request reached
    // ReadyForQuery. With no active COPY or escaped/uncertain cancel lease,
    // dropping the operation left nothing that can act on a later borrower.
    // Sending CancelRequest in that state adds no proof and can turn a local
    // timeout into needless session loss.
    if !client.is_closed()
        && client.transaction_status() == Some(TransactionStatus::Idle)
        && !client.has_active_copy()
        && !client.pool_cancel_lease_prevents_reuse()
    {
        return Err(Error::command_timeout(None));
    }

    let cancel_token = client.cancel_token();

    // Recovery either restores this physical session or destroys it, and
    // the deciding arms below only run if this future is polled to
    // completion. A caller that drops it - a timeout of its own, a select
    // that lost, a cancelled task - would otherwise return a session that
    // is mid-CancelRequest to the pool, and the next borrower inherits it.
    // The guard makes retirement the default and success the exception.
    let recovery_guard = CommandRecoveryGuard::new(client);
    let recovery = compio::time::timeout(COMMAND_TIMEOUT_RECOVERY_GRACE, async {
        // Waiting for EOF on the dedicated cancel connection is the
        // cross-connection ordering barrier: write/flush alone would let a
        // delayed CancelRequest arrive after this backend's Sync and hit
        // the next command instead.
        transport.cancel_query_confirmed(&cancel_token).await?;
        // A query future can return its ErrorResponse before the
        // connection task receives the trailing ReadyForQuery.
        // Sync is a FIFO proof that every response belonging to
        // the dropped operation has been drained.
        client.check_connection().await?;

        if client.transaction_status() != Some(TransactionStatus::Idle) {
            // Cancellation inside a raw BEGIN leaves the backend in `E`,
            // where every follow-up statement fails with 25P02. Roll back
            // inside the same bounded recovery window so success means the
            // held client is immediately usable, not merely frame-aligned.
            client.__private_api_rollback(None);
            client.check_connection().await?;
            if client.transaction_status() != Some(TransactionStatus::Idle) {
                return Err(Error::io(io::Error::other(
                    "command-timeout recovery did not restore an idle transaction state",
                )));
            }
            client.clear_dirty();
        }

        Ok(())
    })
    .await;

    match recovery {
        Ok(Ok(())) => {
            // The only arm that keeps the session: recovery ran to
            // completion and left the backend idle and frame-aligned.
            recovery_guard.disarm();
            Err(Error::command_timeout(None))
        }
        Ok(Err(error)) => Err(Error::command_timeout(Some(Box::new(error)))),
        Err(_) => {
            Err(Error::command_timeout(Some(Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    // `{:?}`, not `{}s` with `as_secs()`. It renders `5s`
                    // identically for the current value, and keeps doing
                    // so if the grace ever becomes sub-second - where
                    // `as_secs()` would silently print `0s`, as the
                    // acquire timeout did.
                    "CancelRequest recovery did not reach ReadyForQuery within {:?}; \
                         the pooled session was discarded",
                    COMMAND_TIMEOUT_RECOVERY_GRACE
                ),
            )))))
        }
    }
}

/// A checked-out connection that owns its pool handle and returns on drop.
///
/// The lifetime-free peer of [`PooledClient`]. Everything else is the same:
/// same acquisition path, same FIFO fairness, same `acquire_timeout`, same
/// `return_client` on drop, same lease-scoped [`CancelToken`] revocation. Get
/// one from [`Pool::get_owned`].
///
/// Reach for this when the lease must outlive the frame that took it - a
/// session object that drives raw `BEGIN`/`COMMIT` across `await` points and is
/// stored somewhere `'static`. `PooledClient<'a>` and `Transaction<'a>` both
/// borrow, so neither can be held by such a value.
pub struct OwnedPooledClient {
    entry: Option<PoolEntry>,
    pool: Rc<Pool>,
}

impl OwnedPooledClient {
    fn new(mut entry: PoolEntry, pool: Rc<Pool>) -> Self {
        entry.client.activate_pool_cancel_lease();
        Self {
            entry: Some(entry),
            pool,
        }
    }

    /// Run one exclusive logical command under the pool's command deadline.
    ///
    /// Identical in every respect to [`PooledClient::command`] - both call the
    /// same body - including that a successful recovery returns an [`Error`]
    /// for which [`Error::is_command_timeout`] is true.
    ///
    /// # Errors
    ///
    /// See [`PooledClient::command`].
    pub async fn command<T, F>(&mut self, operation: F) -> Result<T, Error>
    where
        F: for<'client> AsyncFnOnce(&'client mut Client) -> Result<T, Error>,
    {
        let pool = Rc::clone(&self.pool);
        let Some(entry) = self.entry.as_mut() else {
            return Err(Error::closed());
        };
        run_pool_command(&pool, &mut entry.client, operation).await
    }

    /// The pool this lease came from.
    #[must_use]
    pub fn pool(&self) -> &Rc<Pool> {
        &self.pool
    }
}

impl Deref for OwnedPooledClient {
    type Target = Client;
    fn deref(&self) -> &Self::Target {
        &self
            .entry
            .as_ref()
            .expect("an owned pooled lease retains its entry until Drop returns it")
            .client
    }
}

impl DerefMut for OwnedPooledClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .entry
            .as_mut()
            .expect("an owned pooled lease retains its entry until Drop returns it")
            .client
    }
}

impl Drop for OwnedPooledClient {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.pool.return_client(entry);
        }
    }
}

impl std::fmt::Debug for OwnedPooledClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedPooledClient").finish()
    }
}

impl Deref for PooledClient<'_> {
    type Target = Client;
    fn deref(&self) -> &Self::Target {
        &self
            .entry
            .as_ref()
            .expect("a pooled lease retains its entry until Drop returns it")
            .client
    }
}

impl DerefMut for PooledClient<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .entry
            .as_mut()
            .expect("a pooled lease retains its entry until Drop returns it")
            .client
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
    use crate::codec::FrontendMessage;
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::{Request, RequestMessages};
    use compio::buf::{IoBuf, IoBufMut};
    use compio::io::{AsyncRead, AsyncWrite};
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
                Some(0.into()),
                None,
            ),
            receiver,
        )
    }

    struct ResetAfterCancelWrite {
        bytes_written: Rc<Cell<usize>>,
    }

    #[allow(clippy::future_not_send)]
    impl AsyncRead for ResetAfterCancelWrite {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            compio::BufResult(
                Err(std::io::Error::new(
                    ErrorKind::ConnectionReset,
                    "scripted post-write cancel reset",
                )),
                buf,
            )
        }
    }

    #[allow(clippy::future_not_send)]
    impl AsyncWrite for ResetAfterCancelWrite {
        async fn write<B: IoBuf>(&mut self, buf: B) -> compio::BufResult<usize, B> {
            let len = buf.buf_len();
            self.bytes_written.set(self.bytes_written.get() + len);
            compio::BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
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

    /// A fake server that answers every startup handshake it receives until the
    /// returned sender fires, then reports how many it answered.
    ///
    /// The two fixtures around it accept a FIXED number of sessions and block in
    /// `accept` for the rest, so neither can be asked "how many connections did
    /// the pool open?": one more attempt than the fixture expects hangs the
    /// test, and one fewer hangs the server thread. Both outcomes read as an
    /// unrelated failure. This one polls, so the count is a measurement and an
    /// over-eager pool is a wrong number rather than a stall.
    ///
    /// Each session gets its own `BackendKeyData` process id starting at 45, so
    /// "the pool reused a connection" and "the pool opened another one" are
    /// distinguishable at the borrower.
    fn accepting_postgres_server() -> (
        std::net::SocketAddr,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<usize>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let (count_tx, count_rx) = std::sync::mpsc::channel::<usize>();
        let server = std::thread::spawn(move || {
            let mut streams: Vec<std::net::TcpStream> = Vec::new();
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut length = [0u8; 4];
                        stream.read_exact(&mut length).unwrap();
                        let remaining = u32::from_be_bytes(length) as usize - length.len();
                        let mut startup = vec![0u8; remaining];
                        stream.read_exact(&mut startup).unwrap();

                        let pid = (45_u32 + streams.len() as u32).to_be_bytes();
                        stream
                            .write_all(&[
                                b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'K', 0, 0, 0, 12, pid[0], pid[1],
                                pid[2], pid[3], 0, 0, 0, 46, b'Z', 0, 0, 0, 5, b'I',
                            ])
                            .unwrap();
                        streams.push(stream);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        if finish_rx.try_recv().is_ok() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
            let _ = count_tx.send(streams.len());
        });
        (address, finish_tx, count_rx, server)
    }

    fn two_session_postgres_server() -> (
        std::net::SocketAddr,
        futures_channel::oneshot::Receiver<[bool; 2]>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (eof_tx, eof_rx) = futures_channel::oneshot::channel();
        let server = std::thread::spawn(move || {
            let mut streams = Vec::with_capacity(2);
            for process_id in [45_u32, 46] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut length = [0u8; 4];
                stream.read_exact(&mut length).unwrap();
                let remaining = u32::from_be_bytes(length) as usize - length.len();
                let mut startup = vec![0u8; remaining];
                stream.read_exact(&mut startup).unwrap();

                let pid = process_id.to_be_bytes();
                stream
                    .write_all(&[
                        b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'K', 0, 0, 0, 12, pid[0], pid[1], pid[2],
                        pid[3], 0, 0, 0, 46, b'Z', 0, 0, 0, 5, b'I',
                    ])
                    .unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                streams.push(stream);
            }

            let mut closed = [false; 2];
            for (index, stream) in streams.iter_mut().enumerate() {
                closed[index] = match stream.read(&mut [0_u8; 1]) {
                    Ok(0) => true,
                    Err(error) => matches!(
                        error.kind(),
                        ErrorKind::ConnectionAborted
                            | ErrorKind::ConnectionReset
                            | ErrorKind::BrokenPipe
                    ),
                    _ => false,
                };
            }
            let _ = eof_tx.send(closed);
        });
        (address, eof_rx, server)
    }

    fn test_pool(config: PoolConfig, idle: Vec<PoolEntry>, active: usize, total: usize) -> Pool {
        Pool {
            transport: Transport::resolve(
                "postgres://postgres@127.0.0.1/test?sslmode=disable"
                    .parse()
                    .unwrap(),
            )
            .unwrap(),
            config,
            idle: RefCell::new(idle),
            active: Cell::new(active),
            total: Cell::new(total),
            waiters: RefCell::new(VecDeque::new()),
            handoffs: RefCell::new(Vec::new()),
            closed: Cell::new(false),
            close_waiters: RefCell::new(Vec::new()),
            housekeeper: RefCell::new(None),
            metrics: PoolMetrics::new(),
        }
    }

    thread_local! {
        static HOUSEKEEPER_DROP_POOL: RefCell<Weak<Pool>> = RefCell::new(Weak::new());
        static HOUSEKEEPER_DROP_BORROWED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    struct HousekeeperPanicPayload;

    impl Drop for HousekeeperPanicPayload {
        fn drop(&mut self) {
            HOUSEKEEPER_DROP_POOL.with(|slot| {
                let borrowed = slot
                    .borrow()
                    .upgrade()
                    .map(|pool| pool.housekeeper.try_borrow_mut().is_err());
                HOUSEKEEPER_DROP_BORROWED.with(|observed| observed.set(borrowed));
            });
        }
    }

    struct PoolDebugBorrowProbe<'a> {
        pool: &'a Pool,
        wrote: bool,
        borrowed: [bool; 4],
    }

    impl std::fmt::Write for PoolDebugBorrowProbe<'_> {
        fn write_str(&mut self, _text: &str) -> std::fmt::Result {
            self.wrote = true;
            let borrowed = [
                self.pool.idle.try_borrow_mut().is_err(),
                self.pool.waiters.try_borrow_mut().is_err(),
                self.pool.handoffs.try_borrow_mut().is_err(),
                self.pool.close_waiters.try_borrow_mut().is_err(),
            ];
            for (observed, now) in self.borrowed.iter_mut().zip(borrowed) {
                *observed |= now;
            }
            Ok(())
        }
    }

    #[test]
    fn formatting_a_pool_does_not_borrow_its_state_while_the_caller_writer_runs() {
        let pool = test_pool(PoolConfig::default(), Vec::new(), 0, 0);
        let mut probe = PoolDebugBorrowProbe {
            pool: &pool,
            wrote: false,
            borrowed: [false; 4],
        };

        std::fmt::write(&mut probe, format_args!("{pool:?}"))
            .expect("formatting the pool into the probe failed");

        assert!(probe.wrote, "the formatter never invoked the caller writer");
        assert_eq!(
            probe.borrowed, [false; 4],
            "Pool::fmt held [idle, waiters, handoffs, close_waiters] borrows while the caller writer ran"
        );
    }

    #[test]
    fn lifetime_jitter_stays_within_twenty_five_percent() {
        let base = Duration::from_secs(4);
        let lower = base.mul_f64(0.75);
        let upper = base.mul_f64(1.25);

        JITTER_RNG.with(|rng| rng.set(1));
        for _ in 0..4_096 {
            let jittered = jittered_lifetime(base);
            assert!(
                (lower..=upper).contains(&jittered),
                "jittered lifetime {jittered:?} escaped {lower:?}..={upper:?}"
            );
        }
    }

    #[test]
    fn read_retirement_is_evicted_before_the_client_channel_closes() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(17);
        assert!(
            !client.is_closed(),
            "fixture request channel started closed"
        );
        client
            .tx_status_handle()
            .store(crate::connection::READ_RETIRED_STATUS, Ordering::Release);
        let held = PooledClient {
            entry: Some(PoolEntry::new(client, pool.config.max_lifetime)),
            pool: &pool,
        };

        drop(held);
        assert_eq!(pool.idle_count(), 0, "read-retired entry became idle");
        assert_eq!(pool.total_count(), 0, "read-retired entry kept its slot");
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.metrics.evictions.get(), 1);
    }

    #[test]
    fn accepted_before_acquire_cannot_publish_a_hook_closed_entry() {
        let calls = Rc::new(Cell::new(0));
        let hook_calls = Rc::clone(&calls);
        let (doomed, doomed_receiver) = fake_client(18);
        let receiver = Rc::new(RefCell::new(Some(doomed_receiver)));
        let hook_receiver = Rc::clone(&receiver);
        let (healthy, _healthy_receiver) = fake_client(19);
        let mut config = PoolConfig {
            max_size: 2,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        config.before_acquire(move |_| {
            let invocation = hook_calls.get() + 1;
            hook_calls.set(invocation);
            if invocation == 1 {
                drop(hook_receiver.borrow_mut().take());
            }
            Box::pin(async { Ok(true) })
        });
        // Idle is LIFO: the hook closes process 18 first, then the checkout
        // must discard it and continue to the healthy process 19 entry.
        let pool = test_pool(
            config,
            vec![
                PoolEntry::new(healthy, Duration::from_secs(600)),
                PoolEntry::new(doomed, Duration::from_secs(600)),
            ],
            0,
            2,
        );

        let mut acquire = Box::pin(pool.get_inner_leased());
        let client = match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("healthy fallback failed: {error}"),
            Poll::Pending => panic!("fake-client checkout unexpectedly awaited I/O"),
        };

        assert_eq!(client.process_id(), 19, "hook-closed entry was published");
        assert_eq!(
            calls.get(),
            2,
            "checkout did not inspect the fallback entry"
        );
        assert_eq!(pool.metrics.evictions.get(), 1);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.idle_count(), 0);
        drop(client);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 1);
    }

    #[test]
    fn accepted_after_release_cannot_redeposit_a_hook_closed_entry() {
        let (client, receiver) = fake_client(20);
        let receiver = Rc::new(RefCell::new(Some(receiver)));
        let hook_receiver = Rc::clone(&receiver);
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.after_release(move |_| {
            drop(hook_receiver.borrow_mut().take());
            true
        });
        let pool = test_pool(config, Vec::new(), 1, 1);
        let held = PooledClient {
            entry: Some(PoolEntry::new(client, Duration::from_secs(600))),
            pool: &pool,
        };

        drop(held);

        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 0, "hook-closed entry became available");
        assert_eq!(pool.total_count(), 0, "hook-closed entry kept its slot");
        assert_eq!(pool.metrics.evictions.get(), 1);
    }

    #[test]
    fn a_token_created_by_after_release_does_not_retire_the_reusable_session() {
        let retained_token = Rc::new(RefCell::new(None));
        let hook_token = Rc::clone(&retained_token);
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.after_release(move |client| {
            *hook_token.borrow_mut() = Some(client.cancel_token());
            true
        });
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(21);
        let held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);

        drop(held);

        assert!(
            retained_token.borrow().is_some(),
            "after_release did not retain the inactive token"
        );
        assert_eq!(
            pool.idle_count(),
            1,
            "an inactive release-hook token retired a reusable session"
        );
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.metrics.evictions.get(), 0);
    }

    #[compio::test]
    async fn an_uncertain_cancel_retires_after_its_token_is_dropped() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(22);
        let held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);
        let inner = Arc::clone(held.inner());
        let bytes_written = Rc::new(Cell::new(0));
        let token = held.cancel_token();
        let lease = Arc::downgrade(
            token
                .pool_lease
                .as_ref()
                .expect("pooled token omitted its cancellation lease"),
        );

        token
            .cancel_query_raw(
                ResetAfterCancelWrite {
                    bytes_written: Rc::clone(&bytes_written),
                },
                NoTls,
            )
            .await
            .expect_err("the scripted cancel reset unexpectedly confirmed delivery");
        assert!(
            bytes_written.get() > 0,
            "the scripted reset happened before the CancelRequest write"
        );
        assert!(
            !held.is_closed(),
            "the returned raw-cancel error independently closed the pooled client"
        );

        drop(token);
        assert_eq!(
            lease.strong_count(),
            1,
            "the cancellation token did not release its lease authority"
        );
        assert!(
            held.pool_cancel_lease_prevents_reuse(),
            "the uncertain cancel was forgotten after its token was dropped"
        );

        drop(held);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 0, "uncertain session became reusable");
        assert_eq!(pool.total_count(), 0, "uncertain session kept its slot");
        assert_eq!(pool.metrics.evictions.get(), 1);
        assert!(
            inner
                .send(RequestMessages::Single(FrontendMessage::Raw(
                    bytes::Bytes::from_static(b"S\0\0\0\x04"),
                )))
                .is_err(),
            "uncertain-session eviction did not synchronously close the client"
        );
    }

    #[compio::test]
    async fn a_timeout_before_protocol_work_does_not_retire_an_idle_session() {
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.command_timeout(Duration::from_millis(1));
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(22);
        let mut held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);

        let error = held
            .command(async |_| std::future::pending::<Result<(), Error>>().await)
            .await
            .expect_err("the local pending operation beat its command deadline");
        assert!(error.is_command_timeout());
        drop(held);

        assert_eq!(
            pool.idle_count(),
            1,
            "a timeout before any request retired a reusable session"
        );
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.metrics.evictions.get(), 0);
    }

    #[compio::test]
    async fn a_missing_cancel_key_recovery_closes_the_held_session() {
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.command_timeout(Duration::from_millis(1));
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (sender, _receiver) = mpsc::unbounded();
        let client = Client::new(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            25,
            None,
            None,
        );
        let mut held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);

        let error = held
            .command(async |client| {
                client.__private_api_rollback(None);
                std::future::pending::<Result<(), Error>>().await
            })
            .await
            .expect_err("the unsettled operation beat its command deadline");
        assert!(error.is_command_timeout());
        let recovery_error = std::error::Error::source(&error)
            .expect("command timeout omitted its recovery failure");
        let missing_key = recovery_error
            .source()
            .expect("missing-key recovery error omitted its cause");
        assert!(
            missing_key
                .to_string()
                .contains("did not provide BackendKeyData"),
            "command recovery took the wrong pre-attempt failure path: {missing_key}"
        );
        assert_eq!(
            pool.active_count(),
            1,
            "test returned the lease before observing synchronous retirement"
        );
        assert!(
            held.is_closed(),
            "failed command recovery left the held session open"
        );

        drop(held);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), 0);
        assert_eq!(pool.metrics.evictions.get(), 1);
    }

    #[compio::test]
    async fn a_timeout_with_an_unsettled_request_still_retires_the_session() {
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.command_timeout(Duration::from_millis(1));
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(23);
        let mut held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);

        let error = held
            .command(async |client| {
                client.__private_api_rollback(None);
                std::future::pending::<Result<(), Error>>().await
            })
            .await
            .expect_err("the unsettled operation beat its command deadline");
        assert!(error.is_command_timeout());
        drop(held);

        assert_eq!(
            pool.total_count(),
            0,
            "a timeout with unsettled backend work reused the session"
        );
    }

    #[compio::test]
    async fn a_timeout_with_escaped_cancellation_authority_still_retires_the_session() {
        let retained_token = Rc::new(RefCell::new(None));
        let operation_token = Rc::clone(&retained_token);
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.command_timeout(Duration::from_millis(1));
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(24);
        let mut held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);

        let error = held
            .command(async move |client| {
                *operation_token.borrow_mut() = Some(client.cancel_token());
                std::future::pending::<Result<(), Error>>().await
            })
            .await
            .expect_err("the token-retaining operation beat its command deadline");
        assert!(error.is_command_timeout());
        assert!(retained_token.borrow().is_some());
        drop(held);

        assert_eq!(
            pool.total_count(),
            0,
            "a timeout with escaped cancellation authority reused the session"
        );
    }

    #[compio::test]
    async fn accepted_after_connect_cannot_publish_a_hook_closed_new_entry() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.after_connect(|client| {
            client.force_close();
            Box::pin(async { Ok(()) })
        });
        let mut pool = test_pool(config, Vec::new(), 0, 0);
        let url = format!("postgres://postgres@{address}/fake?sslmode=disable");
        pool.transport = Transport::resolve(url.parse().unwrap()).unwrap();

        let outcome = pool.get_inner_leased().await;
        assert!(
            outcome.is_err(),
            "hook-closed newly connected entry was published"
        );
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), 0, "failed checkout leaked its permit");
        assert_eq!(pool.metrics.connections_created.get(), 1);
        assert_eq!(pool.metrics.evictions.get(), 1);

        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 1);
        server.join().expect("fake PostgreSQL server panicked");
    }

    /// Drain a fake client's request channel and count the `ROLLBACK`
    /// statements on it. Draining is deliberate: each call reports what was
    /// queued since the previous one.
    fn queued_rollbacks(receiver: &mut mpsc::UnboundedReceiver<Request>) -> usize {
        let mut rollbacks = 0;
        while let Ok(Some(request)) = receiver.try_next() {
            if let RequestMessages::Single(FrontendMessage::Raw(bytes)) = &request.messages
                && bytes.windows(b"ROLLBACK".len()).any(|w| w == b"ROLLBACK")
            {
                rollbacks += 1;
            }
        }
        rollbacks
    }

    /// Release a session that is dirty from an ALREADY SETTLED command and
    /// report how many `ROLLBACK`s the release queued.
    ///
    /// `settled_status` is the `ReadyForQuery` byte the borrower's next command
    /// left behind, published the way the connection task publishes it.
    fn release_rollbacks_after_settling_at(settled_status: u8) -> usize {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, mut receiver) = fake_client(51);

        // A borrower abandons a `Transaction`: `dirty` is set and one
        // fire-and-forget ROLLBACK goes on the wire.
        client.__private_api_rollback(None);
        assert_eq!(
            queued_rollbacks(&mut receiver),
            1,
            "fixture never queued the first ROLLBACK, so it cannot show a stale flag"
        );

        // The connection task consumes that ROLLBACK's ReadyForQuery and
        // publishes the state the borrower's NEXT, awaited command left behind.
        client
            .tx_status_handle()
            .store(settled_status, Ordering::Release);
        client
            .in_flight_requests_handle()
            .store(0, Ordering::Release);
        assert!(
            client.is_dirty(),
            "nothing clears `dirty` once its command settles, which is the whole defect"
        );

        let held = PooledClient {
            entry: Some(PoolEntry::new(client, pool.config.max_lifetime)),
            pool: &pool,
        };
        drop(held);
        queued_rollbacks(&mut receiver)
    }

    /// A stale `dirty` flag must not veto the release rollback.
    ///
    /// The flag means "a fire-and-forget command was queued and nobody observed
    /// its outcome", and nothing on the session clears it, so it outlives the
    /// command it describes. Read at release as "a ROLLBACK is already queued",
    /// it suppressed the rollback for a transaction opened AFTER that one and
    /// the next borrower inherited it.
    #[test]
    fn a_stale_dirty_flag_does_not_veto_the_release_rollback() {
        assert_eq!(
            release_rollbacks_after_settling_at(b'T'),
            1,
            "a session released inside a transaction queued no ROLLBACK because an \
             already-settled command had left `dirty` set"
        );
    }

    /// One-variable control for the test above: same fixture, same stale flag,
    /// settled status `I` instead of `T`. A provably idle session must still
    /// send nothing, or the test above would pass on a pool that rolled back on
    /// every release regardless of state - which is not the claim.
    #[test]
    fn a_stale_dirty_flag_on_an_idle_session_queues_no_release_rollback() {
        assert_eq!(
            release_rollbacks_after_settling_at(b'I'),
            0,
            "an idle session was rolled back on release"
        );
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

        let mut acquire = Box::pin(pool.get_inner_leased());
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

        let mut acquire = Box::pin(pool.get_inner_leased());
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
    fn cancelling_a_waiter_does_not_redeposit_a_handoff_that_closed_in_its_slot() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, receiver) = fake_client(13);
        let held = PooledClient {
            entry: Some(PoolEntry::new(client, pool.config.max_lifetime)),
            pool: &pool,
        };
        let mut waiter = Box::pin(Waiter::new(&pool));

        assert!(poll_once(waiter.as_mut()).is_pending());
        drop(held);
        assert_eq!(pool.pending_count(), 0, "the live entry was not handed off");

        // The entry is pool-owned but can dwell in the assigned slot until the
        // waiter is polled. Model the connection task ending in that interval,
        // then cancel the waiter so its Drop path must decide whether to
        // republish or retire the entry.
        drop(receiver);
        drop(waiter);

        assert_eq!(
            pool.idle_count(),
            0,
            "cancelled waiter redeposited an ineligible handoff"
        );
        assert_eq!(
            pool.total_count(),
            0,
            "closed handoff kept its capacity slot"
        );
        assert_eq!(pool.metrics.evictions.get(), 1);
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

        let mut acquire = Box::pin(pool.get_inner_leased());
        assert!(poll_once(acquire.as_mut()).is_pending());
        drop(held);

        match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => assert_eq!(client.process_id(), 33),
            Poll::Ready(Err(error)) => panic!("clean handoff failed: {error}"),
            Poll::Pending => panic!("clean recent handoff performed an unnecessary probe"),
        }
    }

    #[test]
    fn after_release_rejection_releases_capacity_to_fifo_head() {
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        config.after_release(|_| false);
        let (client, _receiver) = fake_client(34);
        let entry = PoolEntry::new(client, config.max_lifetime);
        let pool = test_pool(config, Vec::new(), 1, 1);
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());
        let held = PooledClient {
            entry: Some(entry),
            pool: &pool,
        };

        drop(held);

        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.total_count(), 0, "rejected return kept its slot");
        assert_eq!(pool.idle_count(), 0, "rejected return became idle");
        assert_eq!(
            wake_count.load(Ordering::Relaxed),
            1,
            "released capacity did not wake the FIFO head"
        );
        assert!(matches!(
            poll_with_waker(waiter.as_mut(), &waker),
            Poll::Ready(None)
        ));
    }

    fn counting_after_release_config(calls: &Rc<Cell<usize>>) -> PoolConfig {
        let hook_calls = Rc::clone(calls);
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        config.after_release(move |_| {
            hook_calls.set(hook_calls.get() + 1);
            true
        });
        config
    }

    #[test]
    fn after_release_skips_connections_already_expired() {
        let calls = Rc::new(Cell::new(0));
        let config = counting_after_release_config(&calls);
        let (expired_client, _expired_receiver) = fake_client(342);
        let expired_pool = test_pool(config, Vec::new(), 1, 1);
        drop(PooledClient {
            entry: Some(PoolEntry::new(expired_client, Duration::ZERO)),
            pool: &expired_pool,
        });
        assert_eq!(calls.get(), 0, "after_release ran for an expired entry");
        assert_eq!(expired_pool.total_count(), 0);
    }

    #[test]
    fn after_release_skips_connections_already_closed() {
        let calls = Rc::new(Cell::new(0));
        let config = counting_after_release_config(&calls);
        let (closed_client, closed_receiver) = fake_client(343);
        let closed_entry = PoolEntry::new(closed_client, config.max_lifetime);
        drop(closed_receiver);
        assert!(
            closed_entry.client.is_closed(),
            "fixture entry did not close"
        );
        let closed_pool = test_pool(config, Vec::new(), 1, 1);
        drop(PooledClient {
            entry: Some(closed_entry),
            pool: &closed_pool,
        });
        assert_eq!(calls.get(), 0, "after_release ran for a closed entry");
        assert_eq!(closed_pool.total_count(), 0);
    }

    #[test]
    fn reentrant_close_from_after_release_cannot_redeposit_after_shutdown() {
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool_slot = Rc::new(RefCell::new(Weak::<Pool>::new()));
        let hook_pool_slot = Rc::clone(&pool_slot);
        config.after_release(move |_| {
            let pool = hook_pool_slot
                .borrow()
                .upgrade()
                .expect("test pool disappeared inside after_release");
            let mut close = Box::pin(pool.close());
            assert!(
                poll_once(close.as_mut()).is_ready(),
                "close waited for a return that had already left active"
            );
            true
        });
        let pool = test_pool(config, Vec::new(), 1, 1);
        let pool = Rc::new(pool);
        *pool_slot.borrow_mut() = Rc::downgrade(&pool);
        let (client, _receiver) = fake_client(341);
        let held = PooledClient {
            entry: Some(PoolEntry::new(client, pool.config.max_lifetime)),
            pool: &pool,
        };

        drop(held);

        assert!(pool.is_closed());
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 0, "return was deposited after close");
        assert_eq!(pool.total_count(), 0, "return kept a slot after close");
    }

    #[test]
    fn panicking_borrower_returns_its_connection_exactly_once() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(34);
        let held = PooledClient::new(PoolEntry::new(client, pool.config.max_lifetime), &pool);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = held;
            panic!("borrower panicked while holding a pooled connection");
        }));
        assert!(panic.is_err(), "the borrower fixture did not panic");
        assert_eq!(pool.active_count(), 0, "unwind leaked the active lease");
        assert_eq!(pool.idle_count(), 1, "unwind did not return the connection");
        assert_eq!(
            pool.total_count(),
            1,
            "unwind discarded a reusable connection"
        );
        assert_eq!(pool.metrics.evictions.get(), 0);

        // `get_inner_leased`, not `get_inner`: the assertions below drop this
        // and require the entry to go BACK to the pool, which is
        // `PooledClient`'s Drop. A bare `PoolEntry` has no such Drop, so
        // `get_inner` would leave active_count at 1 forever.
        let mut acquire = Box::pin(pool.get_inner_leased());
        let reused = match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("reacquiring after unwind failed: {error}"),
            Poll::Pending => panic!("reacquiring the returned connection unexpectedly waited"),
        };
        assert_eq!(
            reused.process_id(),
            34,
            "unwind did not preserve the session"
        );
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), 1);
        drop(reused);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 1);
        assert_eq!(pool.total_count(), 1);
    }

    #[test]
    fn panicking_handoff_waker_does_not_unaccount_a_deposited_entry() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 1, 1);
        let (client, _receiver) = fake_client(35);
        let held = PooledClient {
            entry: Some(PoolEntry::new(client, pool.config.max_lifetime)),
            pool: &pool,
        };
        let panic_waker = Waker::from(Arc::new(PanicWake));
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_with_waker(waiter.as_mut(), &panic_waker).is_pending());

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(held)));
        assert!(panic.is_err(), "the test waker did not panic");
        assert_eq!(pool.active_count(), 0);
        assert_eq!(
            pool.total_count(),
            1,
            "handoff panic unaccounted an entry already deposited to its waiter"
        );

        let entry = match poll_once(waiter.as_mut()) {
            Poll::Ready(Some(entry)) => entry,
            _ => panic!("the deposited entry was lost during the handoff panic"),
        };
        assert_eq!(entry.client.process_id(), 35);
        pool.redeposit_freed_entry(entry);
        assert_eq!(pool.idle_count(), 1);
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
    fn failed_or_cancelled_housekeeping_refill_releases_its_permit() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 0, 0));

        let refill = WeakPermitGuard::reserve(&pool);
        assert_eq!(pool.total_count(), 1, "refill did not reserve capacity");

        drop(refill);

        assert_eq!(
            pool.total_count(),
            0,
            "failed or cancelled refill kept its reserved capacity"
        );
    }

    #[test]
    fn failed_or_cancelled_housekeeping_refill_wakes_fifo_head() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 0, 0));
        let refill = WeakPermitGuard::reserve(&pool);
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut waiter = Box::pin(Waiter::new(&pool));

        assert_eq!(pool.total_count(), 1, "refill did not reserve capacity");
        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());
        assert_eq!(pool.pending_count(), 1);
        assert_eq!(wake_count.load(Ordering::Relaxed), 0);

        drop(refill);

        assert_eq!(
            wake_count.load(Ordering::Relaxed),
            1,
            "failed or cancelled refill did not wake the FIFO head"
        );
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

    struct PanicWake;

    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("intentional handoff wake panic");
        }

        fn wake_by_ref(self: &Arc<Self>) {
            panic!("intentional handoff wake panic");
        }
    }

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

    thread_local! {
        static WAITER_CLONE_POOL: RefCell<Option<Rc<Pool>>> = const { RefCell::new(None) };
        static WAITER_CLONE_SLOT: RefCell<Option<Rc<WaiterSlot>>> = const { RefCell::new(None) };
        static WAITER_CLONE_BORROWED: Cell<Option<[bool; 2]>> = const { Cell::new(None) };
    }

    #[allow(unsafe_code)]
    mod waiter_clone_probe {
        use super::*;

        fn record() {
            let queue = WAITER_CLONE_POOL.with(|pool| {
                pool.borrow()
                    .as_ref()
                    .is_some_and(|pool| pool.waiters.try_borrow_mut().is_err())
            });
            let slot = WAITER_CLONE_SLOT.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|slot| slot.waker.try_borrow_mut().is_err())
            });
            WAITER_CLONE_BORROWED.with(|borrowed| borrowed.set(Some([queue, slot])));
        }

        fn raw_waker() -> std::task::RawWaker {
            unsafe fn clone(_: *const ()) -> std::task::RawWaker {
                record();
                raw_waker()
            }

            unsafe fn wake(_: *const ()) {}
            unsafe fn wake_by_ref(_: *const ()) {}
            unsafe fn drop(_: *const ()) {}

            static VTABLE: std::task::RawWakerVTable =
                std::task::RawWakerVTable::new(clone, wake, wake_by_ref, drop);
            std::task::RawWaker::new(std::ptr::null(), &VTABLE)
        }

        pub(super) fn waker() -> Waker {
            // The vtable owns no data and every operation preserves that.
            unsafe { Waker::from_raw(raw_waker()) }
        }
    }

    #[test]
    fn cloning_a_waiter_waker_does_not_borrow_pool_state() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 0, 1));
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_once(waiter.as_mut()).is_pending());
        let slot = pool
            .waiters
            .borrow()
            .front()
            .cloned()
            .expect("the first poll registered a waiter slot");
        WAITER_CLONE_POOL.with(|stored| *stored.borrow_mut() = Some(Rc::clone(&pool)));
        WAITER_CLONE_SLOT.with(|stored| *stored.borrow_mut() = Some(slot));
        WAITER_CLONE_BORROWED.with(|borrowed| borrowed.set(None));

        let probe = waiter_clone_probe::waker();
        assert!(poll_with_waker(waiter.as_mut(), &probe).is_pending());

        let observed = WAITER_CLONE_BORROWED.with(Cell::get);
        WAITER_CLONE_POOL.with(|stored| *stored.borrow_mut() = None);
        WAITER_CLONE_SLOT.with(|stored| *stored.borrow_mut() = None);
        assert_eq!(
            observed,
            Some([false, false]),
            "the caller waker's clone vtable ran while [waiters, slot.waker] was borrowed"
        );
    }

    thread_local! {
        static CLOSE_CLONE_POOL: RefCell<Option<Rc<Pool>>> = const { RefCell::new(None) };
        static CLOSE_CLONE_SLOT: RefCell<Option<Rc<CloseWaiterSlot>>> = const { RefCell::new(None) };
        static CLOSE_CLONE_BORROWED: Cell<Option<[bool; 2]>> = const { Cell::new(None) };
    }

    #[allow(unsafe_code)]
    mod close_waiter_clone_probe {
        use super::*;

        fn record() {
            let queue = CLOSE_CLONE_POOL.with(|pool| {
                pool.borrow()
                    .as_ref()
                    .is_some_and(|pool| pool.close_waiters.try_borrow_mut().is_err())
            });
            let slot = CLOSE_CLONE_SLOT.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|slot| slot.waker.try_borrow_mut().is_err())
            });
            CLOSE_CLONE_BORROWED.with(|borrowed| borrowed.set(Some([queue, slot])));
        }

        fn raw_waker() -> std::task::RawWaker {
            unsafe fn clone(_: *const ()) -> std::task::RawWaker {
                record();
                raw_waker()
            }

            unsafe fn wake(_: *const ()) {}
            unsafe fn wake_by_ref(_: *const ()) {}
            unsafe fn drop(_: *const ()) {}

            static VTABLE: std::task::RawWakerVTable =
                std::task::RawWakerVTable::new(clone, wake, wake_by_ref, drop);
            std::task::RawWaker::new(std::ptr::null(), &VTABLE)
        }

        pub(super) fn waker() -> Waker {
            // The vtable owns no data and every operation preserves that.
            unsafe { Waker::from_raw(raw_waker()) }
        }
    }

    #[test]
    fn cloning_a_close_waiter_waker_does_not_borrow_pool_state() {
        let pool = Rc::new(test_pool(PoolConfig::default(), Vec::new(), 0, 1));
        pool.active.set(1);
        let mut waiter = Box::pin(CloseWaiter::new(&pool));
        assert!(poll_once(waiter.as_mut()).is_pending());
        let slot = pool
            .close_waiters
            .borrow()
            .first()
            .cloned()
            .expect("the first poll registered a close-waiter slot");
        CLOSE_CLONE_POOL.with(|stored| *stored.borrow_mut() = Some(Rc::clone(&pool)));
        CLOSE_CLONE_SLOT.with(|stored| *stored.borrow_mut() = Some(slot));
        CLOSE_CLONE_BORROWED.with(|borrowed| borrowed.set(None));

        let probe = close_waiter_clone_probe::waker();
        assert!(poll_with_waker(waiter.as_mut(), &probe).is_pending());

        let observed = CLOSE_CLONE_BORROWED.with(Cell::get);
        CLOSE_CLONE_POOL.with(|stored| *stored.borrow_mut() = None);
        CLOSE_CLONE_SLOT.with(|stored| *stored.borrow_mut() = None);
        assert_eq!(
            observed,
            Some([false, false]),
            "the caller clone ran while [close_waiters, slot.waker] was borrowed"
        );
    }

    thread_local! {
        /// The slot whose waker is being invoked, so the probe below can ask
        /// whether the pool still holds it borrowed.
        static PROBE_SLOT: RefCell<Option<Rc<WaiterSlot>>> = const { RefCell::new(None) };
        /// `Some(true)` means the pool was STILL holding the borrow during
        /// `wake()`. `None` means the waker never ran, which must not read as
        /// success.
        static PROBE_BORROWED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    struct BorrowProbeWake;

    impl BorrowProbeWake {
        fn record() {
            PROBE_SLOT.with(|slot| {
                if let Some(slot) = slot.borrow().as_ref() {
                    PROBE_BORROWED
                        .with(|flag| flag.set(Some(slot.waker.try_borrow_mut().is_err())));
                }
            });
        }
    }

    impl Wake for BorrowProbeWake {
        fn wake(self: Arc<Self>) {
            Self::record();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            Self::record();
        }
    }

    /// A waiter's `Waker` must not run while the pool still holds that waiter's
    /// `waker` slot borrowed.
    ///
    /// `wake_one_waiter` took the waker through `slot.waker.borrow_mut().take()`
    /// as the scrutinee of an `if let`, then called `wake()` in the body. The
    /// scrutinee temporary is NOT dropped before the body - Rust 2024 moved that
    /// drop ahead of the `else` block only - so the `RefMut` was live across the
    /// wake. Any waker that synchronously re-enters the pool and reaches this
    /// slot then hits `BorrowMutError`, and because the unwind escapes before
    /// `permit.disarm()`, the return permit still fires and decrements `total`
    /// for a connection that is still alive, leaving `total < active`.
    ///
    /// Everywhere else in this file wakes OUTSIDE the borrow - `wake_all`
    /// collects into a `Vec` first, and `return_client` takes the waker as a
    /// return value - so this was the one site out of step.
    ///
    /// The assertion is on the BORROW rather than on a panic, so it states the
    /// invariant instead of one caller's way of tripping over it, and it needs no
    /// re-entrant waker to do it. `None` is failed deliberately: a probe that
    /// never ran proves nothing.
    #[test]
    fn waking_a_waiter_does_not_hold_its_waker_slot_borrowed() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let waker = Waker::from(Arc::new(BorrowProbeWake));
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());

        let slot = pool
            .waiters
            .borrow()
            .front()
            .cloned()
            .expect("the parked waiter registered a queue slot");
        PROBE_SLOT.with(|cell| *cell.borrow_mut() = Some(slot));
        PROBE_BORROWED.with(|flag| flag.set(None));

        pool.wake_one_waiter();

        let observed = PROBE_BORROWED.with(Cell::get);
        PROBE_SLOT.with(|cell| *cell.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "the waker ran while the pool still held its slot borrowed \
             (None means the probe never ran at all)"
        );
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
    async fn replacing_a_completed_housekeeper_drops_its_panic_payload_outside_the_borrow() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 0, 0));
        HOUSEKEEPER_DROP_POOL.with(|slot| *slot.borrow_mut() = Rc::downgrade(&pool));
        HOUSEKEEPER_DROP_BORROWED.with(|observed| observed.set(None));

        let (completed_tx, completed_rx) = futures_channel::oneshot::channel();
        let completed = compio::runtime::spawn(async move {
            let _ = completed_tx.send(());
            std::panic::panic_any(HousekeeperPanicPayload);
        });
        *pool.housekeeper.borrow_mut() = Some(completed);
        completed_rx
            .await
            .expect("the completed housekeeper did not run");

        pool.start_housekeeper_with_interval(Duration::from_secs(60));

        let observed = HOUSEKEEPER_DROP_BORROWED.with(Cell::get);
        HOUSEKEEPER_DROP_POOL.with(|slot| *slot.borrow_mut() = Weak::new());
        assert_eq!(
            observed,
            Some(false),
            "the completed housekeeper's caller panic payload was dropped while housekeeper was borrowed"
        );
        pool.begin_close();
    }

    #[compio::test]
    async fn close_stops_housekeeping_and_prevents_restart() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = Rc::new(test_pool(config, Vec::new(), 0, 0));
        pool.start_housekeeper_with_interval(Duration::from_secs(60));
        assert!(pool.housekeeper.borrow().is_some());

        pool.close().await;
        assert!(
            pool.housekeeper.borrow().is_none(),
            "close retained the housekeeper task"
        );

        pool.start_housekeeper_with_interval(Duration::ZERO);
        assert!(
            pool.housekeeper.borrow().is_none(),
            "closed pool restarted its housekeeper"
        );
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
    async fn idle_timeout_eviction_preserves_min_idle() {
        let config = PoolConfig {
            max_size: 2,
            min_idle: 1,
            idle_timeout: Duration::from_millis(1),
            ..PoolConfig::default()
        };
        let (first_client, _first_receiver) = fake_client(451);
        let (second_client, _second_receiver) = fake_client(452);
        let mut first = PoolEntry::new(first_client, config.max_lifetime);
        let mut second = PoolEntry::new(second_client, config.max_lifetime);
        first.last_used = Instant::now() - Duration::from_secs(1);
        second.last_used = Instant::now() - Duration::from_secs(1);

        let mut pool = test_pool(config, vec![first, second], 0, 2);
        pool.transport = Transport::resolve(
            "postgres://postgres@127.0.0.1:1/test?sslmode=disable"
                .parse()
                .unwrap(),
        )
        .unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);

        assert!(
            compio::time::timeout(Duration::from_millis(250), Pool::housekeep(&weak))
                .await
                .expect("idle eviction dipped below min_idle and attempted a refill")
        );
        assert_eq!(pool.idle_count(), 1, "idle eviction crossed min_idle");
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.metrics.evictions.get(), 1);
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
    async fn housekeeping_refill_runs_after_connect_before_deposit() {
        let (address, finish_tx, server) = fake_postgres_server();
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 1,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let url = format!("postgres://postgres@{address}/fake?sslmode=disable");
        let calls = Rc::new(Cell::new(0));
        let hook_calls = Rc::clone(&calls);
        config.after_connect(move |_client| {
            let hook_calls = Rc::clone(&hook_calls);
            Box::pin(async move {
                hook_calls.set(hook_calls.get() + 1);
                Ok(())
            })
        });
        let mut pool = test_pool(config, Vec::new(), 0, 0);
        pool.transport = Transport::resolve(url.parse().unwrap()).unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);

        assert!(Pool::housekeep(&weak).await);
        assert_eq!(calls.get(), 1);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(
            pool.idle_count(),
            1,
            "refill was not deposited after its hook"
        );
        assert_eq!(pool.active_count(), 0);

        drop(pool);
        let _ = finish_tx.send(());
        server.join().expect("fake PostgreSQL server panicked");
    }

    #[compio::test]
    async fn housekeeping_after_connect_failure_records_an_eviction() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let calls = Rc::new(Cell::new(0));
        let hook_calls = Rc::clone(&calls);
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 1,
            ..PoolConfig::default()
        };
        config.after_connect(move |_| {
            hook_calls.set(hook_calls.get() + 1);
            Box::pin(async { Err(pool_error("scripted housekeeping rejection")) })
        });
        let mut pool = test_pool(config, Vec::new(), 0, 0);
        pool.transport = Transport::resolve(
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap(),
        )
        .unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);

        assert!(Pool::housekeep(&weak).await);
        assert_eq!(calls.get(), 1, "housekeeping never ran after_connect");
        assert_eq!(pool.metrics.connections_created.get(), 1);
        assert_eq!(pool.metrics.evictions.get(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.total_count(), 0);

        drop(pool);
        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 1);
        server.join().expect("fake PostgreSQL server panicked");
    }

    #[compio::test]
    async fn housekeeping_after_connect_ineligibility_records_an_eviction() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let calls = Rc::new(Cell::new(0));
        let hook_calls = Rc::clone(&calls);
        let mut config = PoolConfig {
            max_size: 1,
            min_idle: 1,
            ..PoolConfig::default()
        };
        config.after_connect(move |client| {
            hook_calls.set(hook_calls.get() + 1);
            client.force_close();
            Box::pin(async { Ok(()) })
        });
        let mut pool = test_pool(config, Vec::new(), 0, 0);
        pool.transport = Transport::resolve(
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap(),
        )
        .unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);

        assert!(Pool::housekeep(&weak).await);
        assert_eq!(calls.get(), 1, "housekeeping never ran after_connect");
        assert_eq!(pool.metrics.connections_created.get(), 1);
        assert_eq!(pool.metrics.evictions.get(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.total_count(), 0);

        drop(pool);
        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 1);
        server.join().expect("fake PostgreSQL server panicked");
    }

    /// Drive one housekeeping refill whose FIRST `after_connect` parks, move the
    /// pool's `total` to `contended_total` while it is parked, then let it
    /// finish. Reports how many physical sessions that refill opened, counted
    /// twice: at the hook, and at the server that answered the handshakes.
    ///
    /// `contended_total` models the acquisitions that claimed capacity while the
    /// refill was inside its hook - the exact state the loop's reservation
    /// budget was computed BEFORE. `min_idle` 2 against `max_size` 3 makes that
    /// budget 2 in both directions, so the budget is not what varies here.
    async fn refill_sessions_opened_when_total_reaches(
        contended_total: usize,
    ) -> (usize, usize, usize) {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let mut config = PoolConfig {
            max_size: 3,
            min_idle: 2,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let entered = Rc::new(Cell::new(0_usize));
        let (release_tx, release_rx) = futures_channel::oneshot::channel();
        let gate = Rc::new(RefCell::new(Some(release_rx)));
        let hook_entered = Rc::clone(&entered);
        let hook_gate = Rc::clone(&gate);
        config.after_connect(move |_client| {
            hook_entered.set(hook_entered.get() + 1);
            // Only the first refill connection parks; a second one must be free
            // to run to completion so that "it opened one" and "it opened two"
            // differ in the count, not in whether the test finishes.
            let gate = hook_gate.borrow_mut().take();
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                Ok(())
            })
        });

        let mut pool = test_pool(config, Vec::new(), 0, 0);
        pool.transport = Transport::resolve(
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap(),
        )
        .unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);
        let housekeeping = compio::runtime::spawn(async move { Pool::housekeep(&weak).await });

        compio::time::timeout(Duration::from_secs(5), async {
            while entered.get() == 0 {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("housekeeping refill never reached after_connect");

        // Concurrent acquisitions take capacity while the refill is parked. The
        // parked candidate holds one slot itself, so the rest are active.
        pool.total.set(contended_total);
        pool.active.set(contended_total - 1);
        release_tx.send(()).expect("after_connect gate was dropped");
        assert!(
            compio::time::timeout(Duration::from_secs(5), housekeeping)
                .await
                .expect("housekeeping refill did not finish")
                .expect("housekeeping task panicked"),
            "housekeeping reported the pool gone or closed"
        );

        let final_total = pool.total_count();
        drop(pool);
        let _ = finish_tx.send(());
        let answered = count_rx
            .recv()
            .expect("fake PostgreSQL server never reported its session count");
        server.join().expect("fake PostgreSQL server panicked");
        (entered.get(), answered, final_total)
    }

    /// The refill loop must RE-CHECK capacity after each await, not trust the
    /// budget it computed before the first one.
    ///
    /// The budget is read once, before `connect_one`, and stops being true at
    /// that await: acquisitions reserve slots while this task is parked. Without
    /// the re-check the second iteration reserves a slot the pool no longer has
    /// and opens a physical connection past `max_size` - which is the whole
    /// point of `max_size`, and which nothing in this suite ruled on until now.
    #[compio::test]
    async fn housekeeping_refill_rechecks_capacity_after_its_hook_await() {
        let (hook_calls, sessions, total) = refill_sessions_opened_when_total_reaches(3).await;
        assert_eq!(
            (hook_calls, sessions),
            (1, 1),
            "refill opened a second connection after acquisitions took the last slot"
        );
        assert_eq!(total, 3, "refill reserved capacity past max_size");
    }

    /// One-variable control for the test above: the same interleaving, with the
    /// concurrent acquisitions taking only PART of the capacity.
    ///
    /// It must reach the OPPOSITE conclusion - the second connection is opened -
    /// or the test above would also pass on a refill loop that gave up after one
    /// connection for any reason at all, which is not the claim.
    #[compio::test]
    async fn housekeeping_refill_uses_capacity_still_free_after_its_hook_await() {
        let (hook_calls, sessions, total) = refill_sessions_opened_when_total_reaches(2).await;
        assert_eq!(
            (hook_calls, sessions),
            (2, 2),
            "refill abandoned capacity that was still free"
        );
        assert_eq!(total, 3);
    }

    /// A refill cycle must track the live idle deficit, not only the deficit it
    /// observed before its first connection await. The second hook is a precise
    /// gate: by the time it fires, the first refill is idle and can be checked
    /// out while the cycle is still running. Releasing the gate must therefore
    /// make the same cycle open a third session to restore two idle entries.
    #[compio::test]
    async fn housekeeping_refill_recomputes_min_idle_after_concurrent_checkout() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let mut config = PoolConfig {
            max_size: 3,
            min_idle: 2,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let calls = Rc::new(Cell::new(0_usize));
        let hook_calls = Rc::clone(&calls);
        let (second_entered_tx, second_entered_rx) = futures_channel::oneshot::channel();
        let entered_sender = Rc::new(RefCell::new(Some(second_entered_tx)));
        let hook_entered_sender = Rc::clone(&entered_sender);
        let (release_tx, release_rx) = futures_channel::oneshot::channel();
        let release_gate = Rc::new(RefCell::new(Some(release_rx)));
        let hook_release_gate = Rc::clone(&release_gate);
        config.after_connect(move |_| {
            let invocation = hook_calls.get() + 1;
            hook_calls.set(invocation);
            let entered = (invocation == 2)
                .then(|| hook_entered_sender.borrow_mut().take())
                .flatten();
            let release = (invocation == 2)
                .then(|| hook_release_gate.borrow_mut().take())
                .flatten();
            Box::pin(async move {
                if let Some(entered) = entered {
                    let _ = entered.send(());
                }
                if let Some(release) = release {
                    let _ = release.await;
                }
                Ok(())
            })
        });

        let mut pool = test_pool(config, Vec::new(), 0, 0);
        pool.transport = Transport::resolve(
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap(),
        )
        .unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);
        let housekeeping = compio::runtime::spawn(async move { Pool::housekeep(&weak).await });

        compio::time::timeout(Duration::from_secs(5), second_entered_rx)
            .await
            .expect("second refill hook was never reached")
            .expect("second refill hook dropped its entry signal");
        assert_eq!(pool.idle_count(), 1, "first refill was not deposited");
        assert_eq!(
            pool.total_count(),
            2,
            "second refill did not reserve a slot"
        );

        let borrowed = pool
            .get_inner_leased()
            .await
            .expect("concurrent checkout did not consume the first refill");
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.total_count(), 2);

        release_tx.send(()).expect("second hook gate was dropped");
        assert!(
            compio::time::timeout(Duration::from_secs(5), housekeeping)
                .await
                .expect("housekeeping refill did not finish")
                .expect("housekeeping task panicked"),
            "housekeeping reported the pool gone or closed"
        );

        assert_eq!(calls.get(), 3, "refill trusted its stale idle budget");
        assert_eq!(pool.idle_count(), 2, "refill did not restore min_idle");
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.total_count(), 3);

        drop(borrowed);
        drop(pool);
        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 3);
        server.join().expect("fake PostgreSQL server panicked");
    }

    #[compio::test]
    async fn housekeeping_rechecks_idle_eligibility_after_refill_await() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let (seeded, _seeded_receiver) = fake_client(98);
        let seeded_status = seeded.tx_status_handle();
        let calls = Rc::new(Cell::new(0_usize));
        let hook_calls = Rc::clone(&calls);
        let mut config = PoolConfig {
            max_size: 3,
            min_idle: 2,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        config.after_connect(move |_| {
            let invocation = hook_calls.get() + 1;
            hook_calls.set(invocation);
            if invocation == 1 {
                seeded_status.store(crate::connection::READ_RETIRED_STATUS, Ordering::Release);
            }
            Box::pin(async { Ok(()) })
        });

        let seeded = PoolEntry::new(seeded, Duration::from_secs(600));
        let mut pool = test_pool(config, vec![seeded], 0, 1);
        pool.transport = Transport::resolve(
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap(),
        )
        .unwrap();
        let pool = Rc::new(pool);
        let weak = Rc::downgrade(&pool);

        assert!(Pool::housekeep(&weak).await);

        assert_eq!(calls.get(), 2, "refill counted a retired idle entry");
        assert_eq!(pool.idle_count(), 2, "refill did not restore min_idle");
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.total_count(), 2);
        assert_eq!(pool.metrics.connections_created.get(), 2);
        assert_eq!(pool.metrics.evictions.get(), 1);

        drop(pool);
        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 2);
        server.join().expect("fake PostgreSQL server panicked");
    }

    /// Check out one connection from a pool holding a single idle entry aged
    /// `remaining_lifetime`, and report which backend the borrower got plus how
    /// many evictions the checkout recorded.
    ///
    /// The seeded entry announces process id 99; every connection the fake
    /// server answers announces 45 upward. "The pool reused the pooled session"
    /// and "the pool opened a replacement" are therefore distinguishable at the
    /// borrower rather than inferred from a counter.
    async fn checkout_backend_for_idle_entry_with_lifetime(
        remaining_lifetime: Duration,
    ) -> (i32, u64) {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            // No round trip on the seeded entry: it is neither dirty nor stale,
            // so the expiry check is the only thing that can reject it.
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let (seeded, _seeded_receiver) = fake_client(99);
        let entry = PoolEntry::new(seeded, remaining_lifetime);
        let mut pool = test_pool(config, vec![entry], 0, 1);
        pool.transport = Transport::resolve(
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap(),
        )
        .unwrap();

        let client = compio::time::timeout(Duration::from_secs(5), pool.get())
            .await
            .expect("checkout did not finish")
            .expect("checkout failed");
        let process_id = client.process_id();
        let evictions = pool.metrics.evictions.get();

        drop(client);
        drop(pool);
        let _ = finish_tx.send(());
        let _ = count_rx.recv();
        server.join().expect("fake PostgreSQL server panicked");
        (process_id, evictions)
    }

    /// Checkout enforces `max_lifetime` itself, not only through housekeeping.
    ///
    /// The housekeeper is optional - [`Pool::start_housekeeper`] is a separate
    /// call and its documented absence means "connections are never proactively
    /// evicted", not "connections are handed out forever". So on a pool that
    /// never started one, this arm is the ONLY thing enforcing `max_lifetime`,
    /// and nothing in this suite ruled on it: deleting the check left every
    /// other test printing exactly what an enforcing pool prints.
    #[compio::test]
    async fn checkout_evicts_an_idle_entry_past_its_max_lifetime() {
        assert_eq!(
            checkout_backend_for_idle_entry_with_lifetime(Duration::ZERO).await,
            (45, 1),
            "checkout handed out an entry past max_lifetime"
        );
    }

    /// One-variable control: the same fixture with lifetime left on the entry.
    /// The borrower must get the POOLED backend, and nothing may be evicted, or
    /// the test above would also pass on a checkout that discarded every idle
    /// entry it touched.
    #[compio::test]
    async fn checkout_reuses_an_idle_entry_inside_its_max_lifetime() {
        assert_eq!(
            checkout_backend_for_idle_entry_with_lifetime(Duration::from_secs(600)).await,
            (99, 0),
            "checkout discarded an entry that was still inside its max_lifetime"
        );
    }

    #[compio::test]
    async fn later_warmup_after_connect_failure_closes_earlier_sessions() {
        let (address, eof_rx, server) = two_session_postgres_server();
        let connection_config: Config =
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap();
        let calls = Rc::new(Cell::new(0));
        let hook_calls = Rc::clone(&calls);
        let mut pool_config = PoolConfig {
            max_size: 2,
            min_idle: 2,
            ..PoolConfig::default()
        };
        pool_config.after_connect(move |_client| {
            let invocation = hook_calls.get() + 1;
            hook_calls.set(invocation);
            Box::pin(async move {
                if invocation == 2 {
                    Err(pool_error("scripted second warm-up rejection"))
                } else {
                    Ok(())
                }
            })
        });

        let outcome = compio::time::timeout(
            Duration::from_secs(5),
            Pool::connect_with_config(connection_config, pool_config),
        )
        .await
        .expect("pool warm-up did not finish");
        assert!(
            outcome.is_err(),
            "second after_connect rejection was ignored"
        );
        assert_eq!(calls.get(), 2);

        let closed = compio::time::timeout(Duration::from_secs(5), eof_rx)
            .await
            .expect("server did not observe warm-up session teardown")
            .expect("server stopped before reporting warm-up session teardown");
        server.join().expect("fake PostgreSQL server panicked");
        assert_eq!(
            closed,
            [true, true],
            "warm-up failure left an earlier session open"
        );
    }

    #[compio::test]
    async fn warmup_rechecks_idle_eligibility_at_pool_publication() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let connection_config: Config =
            format!("postgres://postgres@{address}/fake?sslmode=disable")
                .parse()
                .unwrap();
        let calls = Rc::new(Cell::new(0_usize));
        let hook_calls = Rc::clone(&calls);
        let first_status = Rc::new(RefCell::new(None));
        let hook_first_status = Rc::clone(&first_status);
        let mut pool_config = PoolConfig {
            max_size: 2,
            min_idle: 2,
            ..PoolConfig::default()
        };
        pool_config.after_connect(move |client| {
            let invocation = hook_calls.get() + 1;
            hook_calls.set(invocation);
            if invocation == 1 {
                *hook_first_status.borrow_mut() = Some(client.tx_status_handle());
            } else if invocation == 2 {
                hook_first_status
                    .borrow()
                    .as_ref()
                    .expect("first hook did not retain its status handle")
                    .store(crate::connection::READ_RETIRED_STATUS, Ordering::Release);
            }
            Box::pin(async { Ok(()) })
        });

        let outcome = Pool::connect_with_config(connection_config, pool_config).await;
        let published = outcome.is_ok();
        drop(outcome);

        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 2);
        server.join().expect("fake PostgreSQL server panicked");
        assert_eq!(calls.get(), 2);
        assert!(
            !published,
            "warm-up published an earlier entry invalidated by a later hook"
        );
    }

    #[compio::test]
    async fn successful_housekeeping_refills_hand_connections_to_fifo_waiters() {
        let (address, finish_tx, count_rx, server) = accepting_postgres_server();
        let config = PoolConfig {
            max_size: 4,
            min_idle: 1,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let url = format!("postgres://postgres@{address}/fake?sslmode=disable");
        let mut pool = test_pool(config, Vec::new(), 0, 4);
        pool.transport = Transport::resolve(url.parse().unwrap()).unwrap();
        let pool = Rc::new(pool);
        let first_count = Arc::new(AtomicUsize::new(0));
        let second_count = Arc::new(AtomicUsize::new(0));
        let first_waker = counting_waker(&first_count);
        let second_waker = counting_waker(&second_count);
        let mut first = Box::pin(pool.get_inner_leased());
        let mut second = Box::pin(pool.get_inner_leased());

        assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
        assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
        // Model two in-progress attempts ending before their waiters are
        // repolled. Both released slots can serve queued callers; neither
        // hand-off counts as idle, so the live refill target remains unmet.
        pool.total.set(2);
        let weak = Rc::downgrade(&pool);
        assert!(
            compio::time::timeout(Duration::from_secs(5), Pool::housekeep(&weak))
                .await
                .expect("housekeeping refill did not complete")
        );

        assert_eq!(pool.idle_count(), 0, "refills bypassed FIFO waiters");
        assert_eq!(pool.pending_count(), 0);
        assert_eq!(pool.total_count(), 4);
        assert_eq!(first_count.load(Ordering::Relaxed), 1);
        assert_eq!(second_count.load(Ordering::Relaxed), 1);

        let first_client = match poll_with_waker(first.as_mut(), &first_waker) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("refilled connection was rejected: {error}"),
            Poll::Pending => panic!("FIFO head did not receive the refilled connection"),
        };
        assert_eq!(first_client.process_id(), 45);
        let second_client = match poll_with_waker(second.as_mut(), &second_waker) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("second refilled connection failed: {error}"),
            Poll::Pending => panic!("second FIFO waiter did not receive a refill"),
        };
        assert_eq!(second_client.process_id(), 46);

        drop(first_client);
        drop(second_client);
        drop(first);
        drop(second);
        drop(pool);
        let _ = finish_tx.send(());
        assert_eq!(count_rx.recv().unwrap(), 2);
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
        pool.transport = Transport::resolve(url.parse().unwrap()).unwrap();
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

    thread_local! {
        /// `Some(true)` means the pool still held a borrow while a REPLACED
        /// waker was being destroyed. `None` means the destructor never ran,
        /// which must not read as success.
        static DROP_BORROWED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    struct DropProbeWake;

    impl Wake for DropProbeWake {
        fn wake(self: Arc<Self>) {}
        fn wake_by_ref(self: &Arc<Self>) {}
    }

    impl Drop for DropProbeWake {
        fn drop(&mut self) {
            PROBE_SLOT.with(|slot| {
                if let Some(slot) = slot.borrow().as_ref() {
                    DROP_BORROWED.with(|flag| flag.set(Some(slot.waker.try_borrow_mut().is_err())));
                }
            });
        }
    }

    /// Replacing a waiter's `Waker` must not DESTROY the old one inside the
    /// borrow.
    ///
    /// `Waiter::poll` refreshed the slot with `*w = Some(new)`, which drops the
    /// previous `Waker` through the `RefMut` while that borrow AND the
    /// `waiters` borrow are both live. A `Waker` is arbitrary caller code, so a
    /// destructor that re-enters the pool meets `BorrowError` - the same hazard
    /// as waking inside the borrow, moved from the wake to the drop.
    ///
    /// The assertion is on the BORROW rather than on a panic, so it states the
    /// invariant instead of one way of tripping over it. `None` fails
    /// deliberately: a destructor that never ran proves nothing.
    #[test]
    fn replacing_a_waiter_waker_does_not_destroy_it_inside_the_borrow() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            ..PoolConfig::default()
        };
        let pool = test_pool(config, Vec::new(), 0, 1);
        let probe = Waker::from(Arc::new(DropProbeWake));
        let mut waiter = Box::pin(Waiter::new(&pool));
        assert!(poll_with_waker(waiter.as_mut(), &probe).is_pending());

        let slot = pool
            .waiters
            .borrow()
            .front()
            .cloned()
            .expect("the parked waiter registered a queue slot");
        PROBE_SLOT.with(|cell| *cell.borrow_mut() = Some(slot));
        DROP_BORROWED.with(|flag| flag.set(None));

        // Hand the slot the last `Arc`, so replacing it below runs the
        // destructor rather than merely decrementing a count.
        drop(probe);

        let counter = Arc::new(AtomicUsize::new(0));
        let other = counting_waker(&counter);
        assert!(poll_with_waker(waiter.as_mut(), &other).is_pending());

        let observed = DROP_BORROWED.with(Cell::get);
        PROBE_SLOT.with(|cell| *cell.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "the replaced waker was destroyed while the pool held its slot \
             borrowed (None means the destructor never ran at all)"
        );
    }

    // -----------------------------------------------------------------
    // `Pool::get_owned` / `OwnedPooledClient`
    //
    // What these rule on: the owned lease is accounted exactly like the
    // borrowed one (active while held, back to idle on drop, pool handle
    // released) and it is bounded by `max_size` - a second checkout parks
    // in the same FIFO rather than opening a connection of its own. That
    // second property is the whole point of the type on the plugin-db
    // side, where a transaction used to call `connect()` directly and had
    // no ceiling at all.
    //
    // What they do NOT rule on: anything about `command()`'s deadline or
    // its CancelRequest recovery. Both leases call the same
    // `run_pool_command` body and the borrowed lease's own tests cover it;
    // nothing here would notice if that body were wrong.
    // -----------------------------------------------------------------

    #[compio::test]
    async fn an_owned_lease_is_active_while_held_and_idle_again_on_drop() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let (client, _receiver) = fake_client(77);
        let entry = PoolEntry::new(client, config.max_lifetime);
        let pool = Rc::new(test_pool(config, vec![entry], 0, 1));

        let lease = pool.get_owned().await.expect("owned checkout");
        assert_eq!(lease.process_id(), 77);
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(
            Rc::strong_count(&pool),
            2,
            "the owned lease must hold a pool handle of its own"
        );

        drop(lease);

        assert_eq!(pool.active_count(), 0, "owned lease did not return");
        assert_eq!(pool.idle_count(), 1, "owned lease did not re-enter idle");
        assert_eq!(
            Rc::strong_count(&pool),
            1,
            "the owned lease leaked its pool handle"
        );
    }

    #[compio::test]
    async fn a_second_owned_checkout_is_bounded_by_max_size() {
        let config = PoolConfig {
            max_size: 1,
            min_idle: 0,
            validation_bypass: Duration::from_secs(60),
            ..PoolConfig::default()
        };
        let (client, _receiver) = fake_client(91);
        let entry = PoolEntry::new(client, config.max_lifetime);
        let pool = Rc::new(test_pool(config, vec![entry], 0, 1));

        let first = pool.get_owned().await.expect("first owned checkout");
        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&wake_count);
        let mut second = Box::pin(pool.get_owned());
        assert!(
            poll_with_waker(second.as_mut(), &waker).is_pending(),
            "a second owned checkout must queue behind max_size, not connect"
        );
        assert_eq!(
            pool.metrics.connections_created.get(),
            0,
            "the queued checkout opened a connection instead of waiting"
        );

        drop(first);

        let handed = second.await.expect("hand-off to the queued checkout");
        assert_eq!(
            handed.process_id(),
            91,
            "the queued checkout did not receive the returned session"
        );
        assert_eq!(pool.active_count(), 1);
    }
}
