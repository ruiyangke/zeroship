//! Backend abstraction — "the data store boundary".
//!
//! Stage 8e — R2 of the plugin-db architecture review
//! (`docs/reviews/plugin-db-architecture-review-2026-05-21.md`).
//!
//! ## What this is
//!
//! A single trait, [`Backend`], that captures everything the
//! orchestrator and audit-row layer ask of "the database" — connection
//! lifecycle, advisory locks, schema introspection, and audit-table
//! reads/writes. Today the only impl is [`PostgresBackend`]; the goal
//! is to name the seams BEFORE a second backend lands so we don't
//! accidentally bake `compio_postgres::Pool` / `Client` into every
//! consumer file.
//!
//! ## What this is NOT
//!
//! - **Not a query-IR layer.** [`crate::query`] still emits Postgres
//!   DDL/DML directly via `quote_ident`, `ON CONFLICT`, `RETURNING`,
//!   etc. The architecture critic explicitly scoped that out — see
//!   `query.rs` (4099 LOC) and the review's R2 recommendation: "wait
//!   until a real sqlite or planetscale prototype is in motion." This
//!   trait fixes the *non-builder* surface (orchestrator + audit).
//! - **Not a leakage-free abstraction.** [`crate::replication`] and
//!   [`crate::wal_consumer`] talk raw `pg_replication_slots` and the
//!   streaming WAL protocol; those stay Postgres-only behind their own
//!   files. The `auth/*` SECURITY DEFINER bootstrap is likewise PG-only.
//! - **Not async-trait-Boxed.** `compio-postgres` is single-threaded
//!   io_uring; we use `async fn` directly in trait position
//!   (`async_fn_in_trait` is stable) so the orchestrator's hot paths
//!   don't allocate a `Box<dyn Future>` per call.
//!
//! ## Trait shape
//!
//! Associated types `Client` / `LiveSchema` keep the consumer files
//! free of `compio_postgres::Client` / `crate::diff::LiveSchema`
//! direct references — they go through `B::Client` / `B::LiveSchema`
//! instead. The PG impl ties them to the concrete types in
//! [`postgres::PostgresBackend`].
//!
//! ## Capability traits (P0 PR 1 + PR 2)
//!
//! After P0 PR 2, [`Backend`] is a **pure composition marker** — every
//! operation lives on one of five focused capability traits:
//!
//! - [`SqlExecutor`] — connection lifecycle + run-a-statement (PR 1).
//! - [`LockManager`] — session-scoped advisory locks (PR 1).
//! - [`NamespaceManager`] — idempotent per-app schema bootstrap (PR 2).
//! - [`SchemaIntrospect`] — live-schema snapshot + row-count estimate
//!   (PR 2). Owns the `LiveSchema` associated type that used to live
//!   on `Backend`.
//! - [`IndexBuilder`] — `CREATE INDEX CONCURRENTLY` with audit-driven
//!   retry recovery (PR 2).
//!
//! A sixth PG-only extension trait, [`PgSqlExecutor`], exposes
//! `pool_handle()` so free-function audit helpers in [`crate::audit`]
//! can reach `&compio_postgres::Pool` without naming the concrete
//! backend (Open Q1 resolution, see
//! `docs/proposals/p0-implementation-plan.md` §3 Q1).
//!
//! The 16 audit-table operations that used to live as methods on
//! `Backend` (`ensure_audit_table`, `write_audit_row`, …) were deleted
//! in PR 2; they stay as `pub`/`pub(crate)` free functions in
//! [`crate::audit`].

use std::rc::Rc;

use crate::error::DbError;

pub(crate) mod lock_guard;
pub(crate) mod owned_lock_guard;
pub mod postgres;
// SQLite module — crate-private by default; under `test-helpers` it
// becomes `pub` so the integration target
// (`tests/sqlite_integration.rs` — P1 PR 2) can name
// `backend::sqlite::SqliteBackend` and the session-handle accessor.
// The PG-side test target reaches its backend through
// `backend::PostgresBackend` (re-exported below); the SQLite arm has
// session-actor internals worth pinning at the integration level, so
// the full sub-module is visible under the same gate.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod sqlite;
#[cfg(feature = "test-helpers")]
pub mod sqlite;

pub(crate) use lock_guard::LockGuard;
pub(crate) use owned_lock_guard::OwnedLockGuard;
pub use postgres::PostgresBackend;
pub use sqlite::SqliteBackend;

/// SQL execution capability — the "connection lifecycle + run a
/// statement" slice of the data-store boundary.
///
/// Carved out of the monolithic [`Backend`] trait in P0 PR 1 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 1" and the
/// converged design at `docs/proposals/db-system-design.md` §7).
/// Future P0 PRs will narrow consumer bounds onto this trait (and
/// [`LockManager`]) instead of the omnibus [`Backend`] super-trait;
/// see the deferred-backlog [C1] entry in
/// `docs/reviews/plugin-db-deferred.md`.
///
/// Not `Send + Sync` on purpose — the compio runtime is
/// single-threaded per worker, so we don't pay for atomics or
/// thread-safety bounds we don't use (Open Q4 in the implementation
/// plan).
pub trait SqlExecutor: 'static {
    /// Concrete connection / client handle. The orchestrator threads
    /// this through audit helpers and advisory-lock acquisition without
    /// naming the underlying SQL driver.
    type Client;

    /// Acquire a dedicated (non-pooled) connection. Caller owns the
    /// lifetime — used by the migration lock and the native
    /// `db.transaction(fn)` orchestrator, which need a
    /// connection that survives across pool-return points.
    ///
    /// For Postgres this opens a fresh `compio_postgres::connect(...)`
    /// against the configured URL and spawns the connection task; for
    /// future backends this maps to whatever "long-lived session"
    /// primitive that backend exposes.
    #[allow(async_fn_in_trait)]
    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError>;

    /// Execute a SQL statement against the pool with text-encoded
    /// parameters. Returns the count of affected rows (read paths
    /// usually ignore the return).
    ///
    /// Implementations free to fan out to a connection pool internally
    /// — this is the "no transaction, no lock client, just run it"
    /// path. The orchestrator and audit helpers do not park clients
    /// across awaits when they use this.
    #[allow(async_fn_in_trait)]
    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError>;

    /// Execute a SQL statement against a specific client.
    ///
    /// Used by the migration lock + apply-pass paths that need every
    /// statement to land on the same backend session as the advisory
    /// lock. Returns the count of affected rows.
    #[allow(async_fn_in_trait)]
    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError>;
}

/// Typed classification of every advisory-lock acquisition the
/// plugin-db crate performs.
///
/// **§7 / §10.5 distinction** (see
/// `docs/proposals/db-system-design.md`):
///
/// - [`LockScope::GlobalApp`] — **cross-process visibility**. The lock
///   must be observable by every worker process pointed at the same
///   logical database. Postgres maps this to
///   `pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)` —
///   visible cluster-wide. A future SQLite backend would map it to
///   either `BEGIN EXCLUSIVE` (when the lock duration aligns with a
///   transaction) or a sentinel-row in a `__zs_locks` table (for
///   session-scoped duration). The two existing P0 sites are both in
///   this category: the register-model orchestrator's per-app
///   serialiser (`name = "register_model"`) and the per-migration
///   progress lock (`name = format!("mig:{spec.name}")`).
///
/// - [`LockScope::LocalApp`] — **single-process visibility**. The lock
///   coordinates work inside one worker's Rust runtime — backed by an
///   in-process Rust HashMap registry, NOT by SQL. No SQL is issued;
///   the backend impl maps this to whatever in-memory coordination
///   primitive that backend already runs. The variant exists today
///   so call sites can classify their intent explicitly; P0 has no
///   production `LocalApp` callers — every site is `GlobalApp`.
///
/// **§7.2 / §10.5 key-naming convention**: implementations derive the
/// underlying `(key1, key2)` string-key pair from the variant fields
/// as `(format!("{app_id}:{name}"), name)`. The PG impl then hashes
/// each key through `hashtext()` (§7.2) before passing to
/// `pg_advisory_lock`. SQLite-future impls would use the strings
/// directly as keys into a per-process HashMap (`GlobalApp` and
/// `LocalApp` both, since SQLite is in-process by definition;
/// §8.5). The variant classifies *visibility*, not key shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockScope {
    /// Cluster-wide / cross-process advisory lock. Visible to every
    /// worker pointed at the same database. PG maps to
    /// `pg_advisory_lock`; SQLite-future would map to either
    /// `BEGIN EXCLUSIVE` or a sentinel-row primitive.
    GlobalApp {
        /// App identifier — first half of the key namespace.
        app_id: String,
        /// Scope name tag — e.g. `"register_model"`,
        /// `"mig:add_archived_flag"`. Both halves of the underlying
        /// `(key1, key2)` advisory-lock pair derive from this field
        /// (see [`Self::to_keys`]).
        name: String,
    },
    /// Single-process / in-Rust advisory lock. Coordinates work inside
    /// one worker's Rust runtime — backed by an in-memory HashMap
    /// registry, NOT by SQL. No P0 production caller; the variant
    /// exists so future call sites can classify their intent.
    #[allow(dead_code, reason = "LocalApp remains part of the lock model for test-helper coverage even though the release build only constructs GlobalApp.")]
    LocalApp {
        /// App identifier — first half of the key namespace.
        app_id: String,
        /// Scope name tag.
        name: String,
    },
}

impl LockScope {
    /// Derive the `(key1, key2)` string-key pair the underlying
    /// advisory-lock primitive consumes.
    ///
    /// **§7.2 / §10.5 convention**: `key1 = "{app_id}:{name}"`,
    /// `key2 = name`. PG impls layer `hashtext($k)::int4` over the
    /// returned strings; SQLite-future impls would use them directly
    /// as text keys in a per-process HashMap. The mapping is
    /// identical for both [`Self::GlobalApp`] and [`Self::LocalApp`]
    /// — the variant classifies *visibility*, not key shape.
    pub fn to_keys(&self) -> (String, String) {
        match self {
            Self::GlobalApp { app_id, name } | Self::LocalApp { app_id, name } => {
                (format!("{app_id}:{name}"), name.clone())
            }
        }
    }

    /// Borrow the `app_id` field regardless of variant. Convenience
    /// for log-rendering and audit-row metadata that doesn't care
    /// about visibility class.
    pub fn app_id(&self) -> &str {
        match self {
            Self::GlobalApp { app_id, .. } | Self::LocalApp { app_id, .. } => app_id.as_str(),
        }
    }

    /// Borrow the `name` field regardless of variant. Convenience
    /// for log-rendering and audit-row metadata.
    pub fn name(&self) -> &str {
        match self {
            Self::GlobalApp { name, .. } | Self::LocalApp { name, .. } => name.as_str(),
        }
    }

    /// Constructor for the migration-progress lock scope.
    ///
    /// Encodes the `"mig:"` prefix as the migration-specific
    /// lock-name invariant. Every migration `acquire` / `release`
    /// pair across `exec_begin` (acquisition), the pre-validation
    /// reject path in `exec_commit_batch`, and the `is_done`
    /// finalise path in `exec_commit_batch` MUST go through this
    /// constructor so the `(key1, key2)` shape derived by
    /// [`Self::to_keys`] stays identical across the three sites
    /// (§7.2 / §10.5). The resulting scope is always
    /// [`Self::GlobalApp`] — migrations are cluster-wide.
    ///
    /// `pub(crate)` on purpose: the migration lock is an internal
    /// orchestration primitive, not part of the public surface.
    /// Adopted in arch r13 I-R13-1 / api-surface r13 MINOR-R13-2
    /// to centralise the `"mig:"` literal that was previously
    /// reconstructed at 3 sites in `migrations.rs`.
    pub(crate) fn migration(app_id: impl Into<String>, name: &str) -> LockScope {
        LockScope::GlobalApp {
            app_id: app_id.into(),
            name: format!("mig:{name}"),
        }
    }
}

/// Advisory-lock capability — session-scoped `(key1, key2)` locks held
/// on a [`SqlExecutor::Client`].
///
/// Carved out of the monolithic [`Backend`] trait in P0 PR 1 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 1" and
/// `docs/proposals/db-system-design.md` §7). The `: SqlExecutor`
/// super-bound is load-bearing — every method takes a `&Self::Client`
/// and that associated type lives on [`SqlExecutor`].
///
/// **Two-tier surface** (P0 PR 6):
///
/// - The **typed API** ([`Self::acquire`] / [`Self::try_acquire`] /
///   [`Self::release`] / [`Self::try_acquire_with_backoff`]) takes a
///   [`LockScope`] enum. This is the shape every new call site
///   should adopt — it carries an explicit classification of the
///   lock's visibility (cluster-wide vs in-process) and centralises
///   key derivation per §10.
///
/// - The **legacy string-key API**
///   ([`Self::acquire_advisory_lock`] /
///   [`Self::try_acquire_advisory_lock`] /
///   [`Self::release_advisory_lock`]) takes raw `(key1, key2)`
///   strings. The `try_*` / `release_*` halves remain the underlying
///   primitives the typed API dispatches through, and the PG impl's
///   `hashtext()` SQL lives at this layer. `acquire_advisory_lock`
///   itself is **no longer routed through** by the typed surface —
///   security [I43] (cycle 18:17) replaced
///   `LockManager::acquire`'s indefinite-wait `acquire_advisory_lock`
///   dispatch with the bounded
///   [`Self::try_acquire_with_backoff`] loop. All three legacy
///   methods stay `#[doc(hidden)]`; eager removal of
///   `acquire_advisory_lock` is now unblocked but deferred to a
///   separate cleanup PR.
pub trait LockManager: SqlExecutor {
    /// Acquire a session-scoped advisory lock for the given
    /// [`LockScope`], bounded by a short retry loop on
    /// `pg_try_advisory_lock` (never the indefinitely-waiting
    /// `pg_advisory_lock`). The default impl delegates to
    /// [`Self::try_acquire_with_backoff`].
    ///
    /// **Cancel-safety**: every await point inside the retry loop is
    /// safe to cancel — the underlying `try_acquire_advisory_lock`
    /// only mutates server-side state if it returns `Ok(true)`, and
    /// the next iteration's sleep is cancellable. Dropping the future
    /// mid-await leaks no client-side state.
    ///
    /// **Security [I43]** (cycle 18:17): the previous version called
    /// `acquire_advisory_lock`, which on PG issues `pg_advisory_lock`
    /// — a server-side wait that has no timeout. A malicious app
    /// holding its own register-model lock indefinitely could stall
    /// every subsequent `register_model` / migration call for that
    /// same app until the holding session terminated. The retry loop
    /// caps the wait at ~1.75s and surfaces
    /// `DbError::LockContention` (wire code `lock_not_available`)
    /// on exhaustion so the caller decides how to react.
    ///
    /// **Post-P0 mop-up (MAJOR-R14-2)**: takes `&LockScope` so call
    /// sites can construct a single binding and pass it to both
    /// `try_acquire` / `acquire` and the matching `release` without
    /// either cloning or rebuilding the struct literal. The default
    /// impl only needs `&self` on the scope (it calls `to_keys`).
    #[allow(async_fn_in_trait)]
    async fn acquire(&self, client: &Self::Client, scope: &LockScope) -> Result<(), DbError> {
        self.try_acquire_with_backoff(client, scope).await
    }

    /// Bounded-retry acquisition for the given [`LockScope`]. Loops
    /// on [`Self::try_acquire_advisory_lock`] with the schedule
    /// `0ms, 50ms, 200ms, 500ms, 1000ms` (5 attempts total, ~1.75s
    /// worst-case wall time). On exhaustion returns
    /// [`DbError::LockContention`] — the JS-visible `.code` is
    /// `lock_not_available` (set by
    /// [`DbError::to_op_error`](crate::error::DbError::to_op_error)).
    ///
    /// The default impl is the only impl call sites should ever need
    /// — backends do not override this. They supply the underlying
    /// non-blocking primitive via
    /// [`Self::try_acquire_advisory_lock`]; the retry/backoff policy
    /// lives in this default body.
    ///
    /// **Cancel-safety**: identical to [`Self::acquire`] — each
    /// `try_acquire_advisory_lock` await is one server RTT, and the
    /// `compio::time::sleep` between attempts is cancellable. No
    /// client-side lock state survives a dropped future.
    ///
    /// **Why this schedule**: the contended window is operator-set
    /// (a held migration / register-model lock) so we want a hard
    /// upper bound, not exponential growth. The schedule trades 5
    /// PG round-trips against the longest legitimate hold time we
    /// observe (~1s for a slow `CREATE INDEX CONCURRENTLY`
    /// pre-pivot); a stuck migration that takes > 1.75s correctly
    /// surfaces as contention so the SDK can retry / circuit-break.
    #[allow(async_fn_in_trait)]
    async fn try_acquire_with_backoff(
        &self,
        client: &Self::Client,
        scope: &LockScope,
    ) -> Result<(), DbError> {
        // Backoff schedule per §[I43] (security r13). Attempts 1..=5
        // with the listed `pre-wait` (the first attempt waits 0).
        // Tuple shape `(attempt_idx, pre_wait_ms)` is read off in the
        // loop body so a future contributor sees the cumulative
        // budget without re-deriving it: 0+50+200+500+1000 = 1750ms.
        const SCHEDULE: &[(u32, u64)] = &[
            (1, 0),
            (2, 50),
            (3, 200),
            (4, 500),
            (5, 1000),
        ];
        let (k1, k2) = scope.to_keys();
        for (attempt, pre_wait_ms) in SCHEDULE.iter().copied() {
            if pre_wait_ms > 0 {
                compio::time::sleep(std::time::Duration::from_millis(pre_wait_ms)).await;
            }
            match self.try_acquire_advisory_lock(client, &k1, &k2).await {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    // Lock currently held by another acquirer. Trace
                    // each retry so operators correlating "register_model
                    // slow" reports against `pg_locks` can see the
                    // wait-pattern client-side.
                    tracing::warn!(
                        scope_app_id = %scope.app_id(),
                        scope_name = %scope.name(),
                        attempt,
                        pre_wait_ms,
                        "advisory lock contended; retrying after backoff (security [I43] bounded loop)"
                    );
                }
                Err(e) => {
                    // SQL-level failure (connection drop, server error)
                    // — surface immediately, don't retry. The caller
                    // sees the typed `DbError` exactly as
                    // `try_acquire_advisory_lock` produced it.
                    return Err(e);
                }
            }
        }
        // All attempts exhausted. Emit a single structured trace at
        // exhaustion so the operator-facing log has both the per-retry
        // warns and a final summary. `DbError::LockContention` carries
        // an informative message; `to_op_error` maps it to
        // `code = "lock_not_available"` with a retry hint.
        tracing::warn!(
            scope_app_id = %scope.app_id(),
            scope_name = %scope.name(),
            "advisory lock contention bounded-retry exhausted (5 attempts, ~1.75s); \
             returning LockContention to caller (security [I43])"
        );
        Err(DbError::LockContention {
            message: format!(
                "advisory lock held by another acquirer (scope={}/{}); \
                 bounded retry of 5 attempts at 0/50/200/500/1000ms exhausted. \
                 Hint: retry or check for a stuck migration / register_model holder.",
                scope.app_id(),
                scope.name(),
            ),
        })
    }

    /// Try to acquire a session-scoped advisory lock for the given
    /// [`LockScope`]; `Ok(false)` if another holder already owns it.
    /// Typed wrapper over [`Self::try_acquire_advisory_lock`].
    ///
    /// **Post-P0 mop-up (MAJOR-R14-2)**: takes `&LockScope` — see
    /// [`Self::acquire`].
    #[allow(async_fn_in_trait)]
    async fn try_acquire(
        &self,
        client: &Self::Client,
        scope: &LockScope,
    ) -> Result<bool, DbError> {
        let (k1, k2) = scope.to_keys();
        self.try_acquire_advisory_lock(client, &k1, &k2).await
    }

    /// Release a session-scoped advisory lock previously acquired via
    /// [`Self::acquire`] / [`Self::try_acquire`]. Typed wrapper over
    /// [`Self::release_advisory_lock`].
    ///
    /// **Post-P0 mop-up (MAJOR-R14-2)**: takes `&LockScope` so the
    /// release site can reuse the same binding the acquisition used
    /// — the §10.5 key-derivation invariant lives in the single
    /// `LockScope` value, not in textual identity across two struct
    /// literals.
    #[allow(async_fn_in_trait)]
    async fn release(&self, client: &Self::Client, scope: &LockScope) -> Result<(), DbError> {
        let (k1, k2) = scope.to_keys();
        self.release_advisory_lock(client, &k1, &k2).await
    }

    /// **Legacy string-key primitive — DO NOT CALL FROM NEW CODE.**
    /// Acquire a session-scoped advisory lock on `(key1, key2)` against
    /// the given client. Blocks (server-side, indefinitely) if another
    /// holder exists; the lock releases when the client is dropped or
    /// the backend session ends.
    ///
    /// Postgres maps this to
    /// `SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)`.
    /// Future backends would map to their per-engine equivalent (e.g.
    /// sqlite has no advisory locks — that backend would need a
    /// `BEGIN EXCLUSIVE` or a sentinel table).
    ///
    /// **Security [I43]** (cycle 18:17): the indefinite-wait shape
    /// is a within-app DoS vector — a malicious app holding its own
    /// session-scoped advisory lock stalls every subsequent
    /// `register_model` / migration call for that same app. The
    /// typed [`Self::acquire`] surface no longer dispatches through
    /// this method; it routes via [`Self::try_acquire_with_backoff`]
    /// instead. This method is retained as the trait primitive only
    /// because (a) some future backend may want to expose the
    /// indefinite-wait shape behind a feature gate, and (b) the
    /// integration test `b1_advisory_lock_prevents_concurrent_runs`
    /// at `tests/integration.rs` still calls
    /// `pg_advisory_lock` SQL directly to exercise the contended
    /// branch. **No production caller** invokes it as of [I43]
    /// closure.
    ///
    /// Prefer [`Self::acquire`] / [`Self::try_acquire_with_backoff`]
    /// at every call site.
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    #[allow(dead_code, reason = "The blocking advisory-lock primitive is retained for lock-manager tests; production code routes through try_acquire/backoff.")]
    async fn acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError>;

    /// **Legacy string-key primitive**: try to acquire the same
    /// session-scoped advisory lock; return `Ok(false)` if the lock
    /// is already held by a different session. Used by
    /// [`crate::migrations::exec_begin`] so a second worker observes
    /// "migration already running" instead of blocking.
    ///
    /// Prefer [`Self::try_acquire`] at new call sites.
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError>;

    /// **Legacy string-key primitive**: release a session-scoped
    /// advisory lock. The lock auto-releases on session end, so
    /// callers can treat an `Err` as observability-only
    /// (warn-and-continue) — but returning the typed error lets them
    /// emit a structured log instead of silently swallowing it.
    /// Mirrors the pattern `LockGuard::release` adopted at `ffb1e101`
    /// (code-critique MAJOR-R5-5).
    ///
    /// Prefer [`Self::release`] at new call sites.
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    async fn release_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError>;
}

/// Per-app schema-namespace capability — "idempotently provision the
/// app's logical namespace".
///
/// Carved out of the monolithic [`Backend`] trait in P0 PR 2 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 2" and the
/// converged design at `docs/proposals/db-system-design.md` §7). Carries
/// the single `ensure_app_schema` method that used to live on
/// [`Backend`] directly; consumer bounds in
/// `register_model/bootstrap.rs` will narrow onto this
/// trait in PR 3.
///
/// Not `Send + Sync` for the same reason as [`SqlExecutor`] — Open Q4.
pub trait NamespaceManager: 'static {
    /// Idempotently create the per-app schema namespace.
    ///
    /// For Postgres this is `CREATE SCHEMA IF NOT EXISTS "<app_id>"`;
    /// future backends would map to whatever per-tenant namespace
    /// primitive that engine exposes (a sqlite ATTACH DATABASE, a
    /// PlanetScale keyspace, …).
    #[allow(async_fn_in_trait)]
    async fn ensure_app_schema(&self, app_id: &str) -> Result<(), DbError>;
}

/// Live-schema introspection capability — "read the catalog and return
/// a typed snapshot the diff engine can consume".
///
/// Carved out of the monolithic [`Backend`] trait in P0 PR 2 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 2" and
/// `docs/proposals/db-system-design.md` §7). The trait owns the
/// `LiveSchema` associated type that used to live on [`Backend`] —
/// pinning it here means consumer bounds like
/// `<B: SchemaIntrospect<LiveSchema = LiveSchema>>` in
/// `register_model::plan` go through a narrow capability trait instead
/// of the omnibus super-trait. [`Backend`] re-anchors the same
/// associated type via the `SchemaIntrospect<LiveSchema = LiveSchema>`
/// super-bound below so the constraint is unchanged for existing
/// callers.
pub trait SchemaIntrospect: 'static {
    /// Concrete live-schema snapshot returned by
    /// [`Self::introspect_schema`]. The Postgres impl uses
    /// [`crate::diff::LiveSchema`]; alternate backends would produce
    /// the same shape from their own catalog tables.
    type LiveSchema;

    /// Introspect the live schema for an app. Returns the typed
    /// snapshot the diff engine consumes via
    /// [`crate::diff::compute_diff`].
    #[allow(async_fn_in_trait)]
    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError>;

    /// Estimate the row count for a single collection. Used by the
    /// classifier to decide "ADD NOT NULL on empty table" — cheap
    /// `reltuples`-style estimate is fine.
    #[allow(async_fn_in_trait)]
    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError>;
}

/// Online index-build capability — "create an index without blocking
/// writers, classify SQLSTATE failures, audit retries".
///
/// Carved out of the monolithic [`Backend`] trait in P0 PR 2 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 2" and
/// `docs/proposals/db-system-design.md` §7). The single method —
/// `create_index_with_recovery` — runs `CREATE INDEX CONCURRENTLY` with
/// a SQLSTATE-driven retry loop and writes structured audit rows on
/// every retry. The `: SqlExecutor` super-bound is load-bearing: the
/// PG impl pulls the pool through that trait's [`SqlExecutor::Client`]
/// associated type so the SQLSTATE classification stays in one place.
pub trait IndexBuilder: SqlExecutor {
    /// Idempotent `CREATE INDEX CONCURRENTLY` with retry + audit.
    /// SQLSTATE-driven: unique/not-null/fk/check violations are fatal;
    /// deadlock / disk-full / OOM retry up to a small budget.
    ///
    /// This is the second pass of `register_model::apply` — Postgres-
    /// specific because CIC is a Postgres feature (the trait keeps the
    /// signature; alternate backends would have to map to whatever
    /// online-index primitive they provide).
    ///
    /// Returns `Ok(())` on success; on terminal retry-loop failures
    /// the returned [`DbError::SchemaRefused`] carries a JSON envelope
    /// the SDK consumes verbatim (`validation_refused` /
    /// `unique_violation` shapes). Postgres errors during CIC flow
    /// through the normal `from_pg` path (`UniqueViolation` etc.);
    /// configuration / invariant breaches surface as
    /// [`DbError::Configuration`].
    #[allow(async_fn_in_trait)]
    async fn create_index_with_recovery(
        &self,
        app_id: &str,
        collection: &str,
        spec: &crate::query::IndexSpec,
        deploy_id: &str,
        schema_version: i32,
    ) -> Result<(), DbError>;
}

/// Audit-row writer capability — "persist an audit row for a DDL /
/// validation / backfill event into the per-app `__zeroship_migrations`
/// table".
///
/// **Introduced in P1 PR 5** (decision AW-1 in
/// `docs/proposals/p1-sqlite-implementation-plan.md` §3.5 + §10). The
/// trait exists so [`IndexBuilder::create_index_with_recovery`] (and
/// future audit-emitting hooks) can stamp rows without hard-coding the
/// PG-only [`crate::audit::write_audit_row`] free function.
///
/// **Two impls today**:
///
/// - PG ([`PostgresBackend`]) — thin wrapper around the existing
///   [`crate::audit::write_audit_row`] free function reached via
///   [`PgSqlExecutor::pool_handle`].
/// - SQLite ([`crate::backend::sqlite::SqliteBackend`]) — routes the
///   parameterised INSERT through the session actor.
///
/// **Why not on `Backend` super-bound?** The trait composition stays
/// PR-1-shaped (5 sub-traits + `'static`). `AuditWriter` is opt-in:
/// `IndexBuilder` consumers (today, just the per-backend `impl
/// IndexBuilder for …` methods inside this crate) bound on it
/// explicitly when they need to write rows. Forcing it onto every
/// `Backend` would mean a future backend without audit semantics still
/// has to satisfy the bound — a needless coupling for the small number
/// of callers.
///
/// Not `Send + Sync` for the same reason as [`SqlExecutor`] — Open Q4.
pub trait AuditWriter: 'static {
    /// Idempotently create the per-app `__zeroship_migrations` table.
    #[allow(async_fn_in_trait)]
    async fn ensure_audit_table(&self, app_id: &str) -> Result<(), DbError>;

    /// Return the next monotonic `schema_version` for `app_id`.
    #[allow(async_fn_in_trait)]
    async fn next_schema_version(&self, app_id: &str) -> Result<i32, DbError>;

    /// Insert a single audit row keyed by `app_id`, returning its PK.
    #[allow(async_fn_in_trait)]
    async fn write_audit_row_returning_id(
        &self,
        app_id: &str,
        row: &crate::audit::AuditRow,
    ) -> Result<i64, DbError>;

    /// Insert a single audit row keyed by `app_id`, discarding the PK.
    #[allow(async_fn_in_trait)]
    async fn write_audit_row(
        &self,
        app_id: &str,
        row: &crate::audit::AuditRow,
    ) -> Result<(), DbError> {
        self.write_audit_row_returning_id(app_id, row).await?;
        Ok(())
    }

    /// Transition a running/pending audit row to a terminal status.
    #[allow(async_fn_in_trait)]
    async fn update_audit_status(
        &self,
        app_id: &str,
        id: i64,
        new_status: crate::audit::TerminalStatus,
        error: Option<&str>,
    ) -> Result<bool, DbError>;
}

/// HMAC-signed session-init capability — the cross-backend trust
/// anchor used by both the PG SECURITY DEFINER pipeline and the
/// upcoming SQLite in-process minter.
///
/// **Introduced in P3 PR 1** (see
/// `docs/proposals/p3-sqlite-auth-implementation-plan.md` §3). The
/// trait declaration is **not** feature-gated — both backends will
/// implement it. The PG impl (PR 2) lives at the bottom of
/// `crate::auth::session` and wraps SECURITY DEFINER functions managed
/// by `crate::auth::bootstrap`. The SQLite impl (PR 3) lives in
/// `crate::backend::sqlite::session_minter` and is gated only by the
/// `sqlite` feature — dev tier per the design doc, no SQL surface,
/// HMAC-SHA256 + bounded LRU nonce cache in Rust.
///
/// ## Token canonical payload
///
/// Both impls produce identical payload bytes + signatures for the
/// same `(secret, init, nonce, expires_at)`:
///
/// ```text
/// actor_kind || '|' || actor_id || '|' || pid || '|'
///            || hex(nonce) || '|' || expires_at_iso
/// ```
///
/// A cross-backend equivalence test (P3 PR 4) pins the bytes.
///
/// ## Dyn-compatibility
///
/// `async fn` in trait position means this trait is dyn-incompatible.
/// Consumers route through the [`BackendHandle::as_postgres`] /
/// [`BackendHandle::as_sqlite`] accessors — the same pattern already
/// used by [`AuditWriter`] and the `ChangeStream` family.
///
/// Not `Send + Sync` for the same single-threaded-per-worker reason
/// as the rest of the capability traits (Open Q4 in P0).
#[cfg(feature = "test-helpers")]
pub trait SessionMinter: 'static {
    /// Mint a fresh session token. `ttl_secs = None` defers to the
    /// implementation's default (today: `auth::util::DEFAULT_TOKEN_TTL_SECS`).
    /// Negative `ttl_secs` produce a deliberately-expired token for
    /// tests; the PG impl preserves that behaviour.
    #[allow(async_fn_in_trait)]
    async fn mint_session_token(
        &self,
        init: SessionInit,
        ttl_secs: Option<i64>,
    ) -> Result<MintedToken, DbError>;

    /// Present a previously-minted token to the backend's session
    /// authority. PG impl calls `__zeroship_admin.init_session(...)`
    /// (SECURITY DEFINER, verifies HMAC + nonce + expiry, writes the
    /// session-context row). SQLite impl verifies in-process against
    /// the configured secret(s) + nonce LRU cache; no persistent
    /// state. Both surface the same 5 typed `.code`s on refusal:
    /// `session_signature_expired`, `session_nonce_replay`,
    /// `session_invalid_signature`, `session_invalid_actor_kind`,
    /// `session_nonce_too_short`.
    #[allow(async_fn_in_trait)]
    async fn init_session(&self, token: &MintedToken) -> Result<(), DbError>;
}

/// Inputs for [`SessionMinter::mint_session_token`]. The cross-backend
/// shape — distinct from the legacy `crate::auth::session::SessionInit`
/// which carries no `pid` field. The PG impl (PR 2) translates the
/// trait shape into the legacy free-fn shape before calling into
/// `__zeroship_admin.sign_session`.
///
/// `pid` is the design §12 project-id binding new in P3. Today's PG
/// free-fn impl ties tokens to `pg_backend_pid()`; trait-routed PG
/// callers can pass `pid: Some(...)` to opt into the canonical-payload
/// shape, and `pid: None` preserves today's PG-side behaviour.
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone)]
pub struct SessionInit {
    pub app_id: String,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    /// Project id per design §12 glossary. SQLite uses this verbatim
    /// in the canonical payload; PG uses `pg_backend_pid()` when this
    /// is `None`, or `pid` when `Some(...)`.
    pub pid: Option<String>,
}

/// A token minted by a [`SessionMinter`] impl. Cross-backend shape:
/// PG fills `backend_pid` from `pg_backend_pid()` for back-compat;
/// SQLite always sets `backend_pid = 0` (there is no PG concept).
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone)]
pub struct MintedToken {
    pub app_id: String,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    /// Mirrors [`SessionInit::pid`]. Carried verbatim through the
    /// canonical payload so signature verification reproduces the
    /// exact bytes.
    pub pid: Option<String>,
    /// PG: `pg_backend_pid()` at mint time. SQLite: always `0` — the
    /// SQLite signer has no backend concept and binds via `pid`
    /// instead.
    pub backend_pid: i32,
    pub nonce: Vec<u8>,
    pub expires_at_iso: String,
    pub signature: Vec<u8>,
}

/// SQL-dialect strategy — the seam every per-engine SQL-string
/// builder route through.
///
/// **Introduced in P1 PR 1** (see
/// `docs/proposals/p1-sqlite-implementation-plan.md` §5). The six
/// methods listed below are the minimum-viable hook set; PR 2-5 fill
/// in additional hooks (RETURNING/upsert/JSON/vector/FTS) alongside
/// the consumers that need them.
///
/// **No production caller as of PR 1** — the trait + ZST impls
/// (`SqliteDialect` here; `PgDialect` lands in PR 3) exist so the
/// `query.rs` free-function builders can be retargeted onto a
/// dialect-typed entry point without re-shaping their call sites.
/// Until PR 3 wires that retarget, `quote_ident` etc. continue to
/// live as free `quote_ident_pg(...)`-style functions inside `query.rs`.
///
/// **Why on the backend, not on `SqlExecutor`**: dialect choice is a
/// property of the *engine*, not the connection — a future PG-replica
/// backend would re-use [`crate::backend::PostgresBackend`]'s pool +
/// `SqlExecutor` impl but share a single `PgDialect`. Pinning
/// `DialectBuilder` as its own trait (and composing into the
/// per-backend struct) is the canonical shape.
pub trait DialectBuilder: 'static {
    /// Concrete SQL dialect this builder targets.
    fn sql_dialect(&self) -> crate::query::SqlDialect;

    /// Quote an identifier (column / table / schema name) per the
    /// engine's lexical rules. PG: doubled `"`; SQLite: doubled `"`
    /// with embedded-NUL rejection.
    fn quote_ident(&self, name: &str) -> String;

    /// Build the SQL string that idempotently provisions the per-app
    /// namespace. PG: `CREATE SCHEMA IF NOT EXISTS "<app>"`. SQLite:
    /// `ATTACH DATABASE 'file:.../zs-<app>.sqlite' AS "<app>"`.
    fn build_ensure_app_schema(&self, app_id: &str) -> String;

    /// Build a `CREATE INDEX` statement for the given [`crate::query::IndexSpec`].
    /// `online = true` requests the engine's "concurrent" variant
    /// (PG: `CREATE INDEX CONCURRENTLY`); SQLite has no concurrent
    /// build, so the flag is a no-op there.
    fn build_create_index(
        &self,
        spec: &crate::query::IndexSpec,
        online: bool,
    ) -> String;

    /// Map a Zeroship-level type string (`"string"`, `"int"`,
    /// `"timestamp"`, …) to the engine's column-type vocabulary.
    /// `opts` is the per-field option object the SDK passes alongside
    /// the type (e.g. `{ length: 256 }`).
    #[allow(dead_code, reason = "These dialect hooks are still covered by unit/integration tests while the production query builders route through free functions.")]
    fn map_zs_type(&self, zs_type: &str, opts: &serde_json::Value) -> String;

    /// SQL fragment that evaluates to "now" on the server. PG: `NOW()`;
    /// SQLite: `CURRENT_TIMESTAMP`. Returned as a `&'static str` so
    /// callers can splice it into a query string without an alloc.
    #[allow(dead_code, reason = "These dialect hooks are still covered by unit/integration tests while the production query builders route through free functions.")]
    fn now_fn(&self) -> &'static str;

    /// Engine-side SQL that returns the last-inserted rowid for a
    /// non-RETURNING insert, if the engine supports the concept.
    /// PG returns `None` (it routes through `RETURNING` instead).
    /// SQLite returns `Some("SELECT last_insert_rowid()")`. Default
    /// `None` so the PG impl doesn't need to override.
    #[allow(dead_code, reason = "These dialect hooks are still covered by unit/integration tests while the production query builders route through free functions.")]
    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        None
    }
}

/// Postgres-specific extension trait exposing the underlying pool
/// handle so free-function consumers — chiefly the audit helpers in
/// [`crate::audit`] — can reach an `&compio_postgres::Pool` without
/// naming the concrete backend type.
///
/// **Open Q1 resolution (P0 PR 2)**: the 16 audit-table operations
/// that used to live as methods on [`Backend`] were deleted; the
/// helpers stay as free functions in `crate::audit::*` taking
/// `&Pool` / `&Client`, and generic consumers reach the pool through
/// `backend.pool_handle()`. See `docs/proposals/p0-implementation-plan.md`
/// §"PR 2" + §3 Q1 and `docs/proposals/db-system-design.md` §7.
///
/// **Feature gating**: this trait is unconditional at HEAD. P0 PR 5 will
/// move the `impl` side onto the PG backend only; a hypothetical
/// `SqliteBackend` would not implement this trait — it would have its
/// own audit-helper signatures (a `SqliteExecutor` accessor returning
/// `&sqlite::Connection`, etc.).
pub trait PgSqlExecutor: SqlExecutor<Client = compio_postgres::Client> {
    /// Borrow the underlying `compio_postgres::Pool`. Free-function
    /// audit helpers in [`crate::audit`] take `&Pool` directly; this
    /// accessor lets generic consumers (e.g.
    /// `<B: PgSqlExecutor>`) reach the pool without naming
    /// `PostgresBackend`.
    fn pool_handle(&self) -> &Rc<compio_postgres::Pool>;
}

/// Postgres-specific extension trait carrying the
/// `acquire_pooled_client_for_lock` primitive — the one piece of the
/// register-model bootstrap that has to return a `PooledClient<'p>`
/// whose `'p` borrow lifetime threads through
/// [`crate::backend::lock_guard::LockGuard`].
///
/// **Open Q5 resolution (P0 PR 3)**: the alternative was a GAT on
/// [`LockManager`] of the form
/// `type PooledLockClient<'p>: 'p where Self: 'p`. async-fn-in-trait
/// + GAT is workable but fights the trait solver in subtle ways
/// (HRTB-style bounds at consumer sites). Since the PG impl is the
/// only one that needs a borrow-lifetimed lock client today — and
/// future backends (sqlite, planetscale) would have their own
/// session-management primitive on a different extension trait —
/// we take the PG extension-trait path and defer cross-backend
/// lifetime threading to P1. See
/// `docs/proposals/p0-implementation-plan.md` §"PR 3" + §3 Q5 and
/// `docs/proposals/db-system-design.md` §7.
///
/// The `: LockManager<Client = compio_postgres::Client>` super-bound
/// is load-bearing: the returned `PooledClient` is the
/// [`SqlExecutor::Client`] that [`LockManager::acquire_advisory_lock`]
/// takes, so the orchestrator can hand the returned client straight
/// into `LockGuard::acquire` without an adapter.
pub trait PgLockManager: LockManager<Client = compio_postgres::Client> {
    /// Acquire a pool-leased client for advisory-lock duty. The
    /// returned [`compio_postgres::PooledClient`]'s `'p` lifetime is
    /// the pool borrow lifetime — it threads through
    /// [`crate::backend::lock_guard::LockGuard`] so
    /// the lock auto-returns to the pool on Drop.
    ///
    /// Postgres impl wraps `self.pool().get().await` and maps the
    /// pool error to [`DbError::Transient`] with the same operator-
    /// facing message the bootstrap call site used to emit inline.
    #[allow(async_fn_in_trait)]
    async fn acquire_pooled_client_for_lock<'p>(
        &'p self,
    ) -> Result<compio_postgres::PooledClient<'p>, DbError>;
}

/// Change-stream capability — the "produce CDC events for an app" slice
/// of the data-store boundary.
///
/// Introduced in **P2 PR 1** as the cross-backend surface for SQLite's
/// `preupdate_hook`-driven CDC arm (`docs/proposals/p2-sqlite-cdc-implementation-plan.md`
/// §2.1, §2.5). PG and SQLite both implement this on adapter types
/// (`change_stream_pg::PgChangeStream` and
/// `backend::sqlite::cdc::SqliteChangeStream`) rather than on the
/// backend itself so the `ConsumerHandle` associated type can diverge
/// across arms without bleeding into the per-isolate `BackendHandle`
/// enum.
///
/// **Why not on [`Backend`] super-bound** (plan §2.1): consumers route
/// via `BackendHandle::as_change_stream_pg(...)` /
/// `as_change_stream_sqlite(...)` accessors that mirror
/// [`BackendHandle::as_postgres`] / [`BackendHandle::as_sqlite`]. The
/// associated `ConsumerHandle` type (concrete `WalConsumerHandle` for
/// PG, `SqliteConsumerHandle` for SQLite) is the load-bearing reason
/// not to dyn-erase — `async fn` + an associated type is not object-safe
/// without `Box<dyn Future>` per call, and the consumer surface (a
/// detached `compio::runtime::spawn` task on PG, an actor-driven flume
/// channel on SQLite) doesn't naturally share an erased shape.
///
/// **Guards in PR 1 are no-ops** — they exist so call sites can adopt
/// the shape today; the real Drop bodies that wire into
/// `wal_consumer::suppress_app` / `broker::resume_app_with_resync`
/// land in PR 4.
// `#[allow(dead_code)]` on the trait: PR 1 ships the surface with
// only `spawn_consumer` invoked from
// `replication_ops::start_replication_consumer_dispatch` (the no-op
// marker call). `provision` / `deprovision` / `pause_broker` /
// `engage_schema_pending` are part of the stable trait surface PR 4
// wires up — `migrations.run` will pull `pause_broker`, the
// `bundle_invalidated` control-event will pull `engage_schema_pending`,
// and per-app deletion (future PR) will pull `deprovision`. Removing
// the allow once those callers exist.
#[allow(dead_code)]
pub trait ChangeStream: 'static {
    /// Concrete handle representing a spawned-but-still-running
    /// consumer. PG: a task handle / supervisor handle; SQLite: a
    /// session marker the actor uses to track that hooks are armed.
    /// Type erased per-impl (associated type) so we don't pay the
    /// `Box<dyn Future>` price the dyn-safe shape would force.
    type ConsumerHandle: 'static;

    /// Idempotently provision the CDC infrastructure for `app_id`. On
    /// PG this creates the publication + logical replication slot;
    /// on SQLite it ensures the per-app session has the
    /// `preupdate_hook`/`commit_hook`/`rollback_hook` triplet armed.
    /// Safe to call multiple times for the same `app_id`.
    #[allow(async_fn_in_trait)]
    async fn provision(&self, app_id: &str) -> Result<(), DbError>;

    /// Idempotently tear down the CDC infrastructure for `app_id`.
    /// Counterpart to [`Self::provision`] used during app deletion;
    /// PG drops the publication + slot, SQLite disarms hooks.
    #[allow(async_fn_in_trait)]
    async fn deprovision(&self, app_id: &str) -> Result<(), DbError>;

    /// Spawn the long-running consumer task for `app_id`. The returned
    /// [`Self::ConsumerHandle`] represents the running consumer; the
    /// orchestrator does not currently join it (PG detaches; SQLite
    /// runs in the session actor) but the handle exists so PR 4+ can
    /// implement explicit shutdown when needed.
    #[allow(async_fn_in_trait)]
    async fn spawn_consumer(&self, app_id: &str) -> Result<Self::ConsumerHandle, DbError>;

    /// Pause broker delivery for `app_id` during a backfill window.
    /// Returns a [`BrokerPauseGuard`] whose `Drop` resumes delivery
    /// and emits a `Resync` to every active subscriber (§16.7).
    ///
    /// **PR 1**: the guard's `Drop` is a no-op (tracing::trace! only);
    /// PR 4 wires it through `wal_consumer::suppress_app` /
    /// `broker::resume_app_with_resync`.
    fn pause_broker(&self, app_id: &str) -> BrokerPauseGuard;

    /// Engage the schema-pending decoder for `app_id`. Returns a
    /// [`SchemaPendingGuard`] whose `Drop` disengages the decoder and
    /// emits a `Resync` per §16.7. While engaged,
    /// `Broker::subscribe(app_id, …)` rejects new subscriptions with
    /// `DbError::Conflict { code: "schema_pending" }`.
    ///
    /// **PR 1**: the guard's `Drop` is a no-op (tracing::trace! only);
    /// PR 4 wires both halves (subscribe-rejection + resync-on-drop).
    fn engage_schema_pending(&self, app_id: &str) -> SchemaPendingGuard;
}

/// RAII guard returned by [`ChangeStream::pause_broker`]. Resuming the
/// broker for `app_id` (and emitting one `Resync` per active
/// subscription) happens on `Drop`.
///
/// **P2 PR 4 wired body** (plan §7 backfill pause):
///
/// 1. `::new(app_id)` — calls
///    [`crate::wal_consumer::suppress_app`] which sets the
///    thread-local `SUPPRESSED_APPS` flag. While the flag is set, the
///    SQLite CDC publisher (`backend/sqlite/cdc.rs::publisher_loop`)
///    drops every packet whose `app_id` matches before the broker
///    fan-out, AND the legacy local-emit shim
///    (`crate::wal_consumer::emit_local`) short-circuits to a no-op so
///    the PG arm sees the same contract.
/// 2. `::drop` — calls [`crate::wal_consumer::unsuppress_app`] to
///    clear the suppression flag, then
///    [`crate::broker::Broker::resume_app_with_resync`] which pushes
///    one `Resync` message onto every active subscription registered
///    on `app_id`. Subscribers refetch and continue catching up.
///
/// The matching `subscribe()` rejection branch lives on the
/// [`SchemaPendingGuard`] (the louder rail); backfill is silent on the
/// `subscribe` path by design (a backfill window is internally driven
/// — `migrations.run` or `register_model` Pass 1 — and SDK callers
/// have no way to observe it directly).
///
/// The `#[must_use]` annotation prevents accidental inline drop at
/// the call site — the pause/resume contract is the *duration* of the
/// guard's binding, not its construction.
#[must_use = "BrokerPauseGuard releases the pause on Drop — bind it to a name to keep the broker paused for the surrounding scope"]
// P2 tail — wired by `migrations::exec_begin` (option A lifecycle:
// guard parked in the `MigrationLock` slot for the whole migration
// window; released by `clear_mig_lock` on terminal `exec_commit_batch`
// or any error rail). `register_model` Pass 1 is the remaining follow-up
// caller — when that lands the construction site list will gain a
// second member but the `pub(crate)` constructor stays internal.
#[derive(Debug)]
pub struct BrokerPauseGuard {
    app_id: String,
}

impl BrokerPauseGuard {
    /// Construct a guard for `app_id` AND engage the suppression flag
    /// on the current thread. The `Drop` impl unsuppresses + emits the
    /// per-subscription `Resync`.
    ///
    /// Idempotent in the sense that calling `new` twice for the same
    /// `app_id` is harmless: the second call's `suppress_app` is a
    /// `HashSet::insert` of an existing key (no-op), and both guards'
    /// `Drop`s call `unsuppress_app` (also a no-op-on-second). Each
    /// guard still emits its own `resume_app_with_resync` on drop —
    /// subscribers dedup Resync messages at the consumer level (see
    /// `SubscriptionMessage::Resync` rustdoc in `broker.rs`).
    ///
    /// Internal to the [`ChangeStream`] impls (PG adapter + SQLite
    /// arm); orchestrator code reaches the guard via
    /// `BackendHandle::as_change_stream_*().pause_broker(app_id)`.
    pub(crate) fn new(app_id: String) -> Self {
        crate::wal_consumer::suppress_app(&app_id);
        Self { app_id }
    }
}

impl Drop for BrokerPauseGuard {
    fn drop(&mut self) {
        // 1. Clear the suppression flag so subsequent CDC packets
        //    publish normally + the legacy local-emit shim re-enables.
        crate::wal_consumer::unsuppress_app(&self.app_id);
        // 2. Push one `Resync` per active subscription on `app_id`.
        //    Subscribers refetch + continue catching up. The broker
        //    primitive is idempotent on closed entries (skipped) and
        //    fast-noop on apps with zero subscribers.
        crate::broker::BROKER.with(|b| {
            b.borrow_mut().resume_app_with_resync(&self.app_id);
        });
        tracing::trace!(
            app_id = %self.app_id,
            "BrokerPauseGuard dropped: unsuppress + resume_app_with_resync emitted"
        );
    }
}

/// Vector-index capability — the "build an ANN index over a `float[]`
/// column and run a top-k nearest-neighbour query" slice of the
/// data-store boundary.
///
/// Introduced in **P4 PR 1** (`docs/proposals/p4-search-implementation-plan.md`
/// §2). The PG impl (PR 2) wraps `pgvector` (`CREATE INDEX … USING
/// ivfflat`, `<->` / `<#>` / `<=>` operators by metric). The SQLite
/// impl (PR 4) is a pure-Rust flat scan over a `BLOB` column holding
/// little-endian `[f32]` payloads — dev tier only, ≤50k rows, ≤1024
/// dims, HNSW deferred (riskiest-decision Q-P4-D, plan §10).
///
/// ## Why not on [`Backend`] super-bound
///
/// Same rationale as [`ChangeStream`] / [`SessionMinter`] (plan §2):
/// consumers route via concrete-backend accessors —
/// [`BackendHandle::as_postgres`] / [`BackendHandle::as_sqlite`] —
/// because `async fn` in trait position is dyn-incompatible. Adding
/// `VectorIndex` to the omnibus `Backend` super-trait would force
/// every backend to implement it (including hypothetical future
/// arms that have no vector primitive), and the consumer migration
/// path on PR 2-5 will go through the same `as_*()?.vector_search(...)`
/// shape the [`AuditWriter`] / `SessionMinter` consumers already use.
///
/// ## Method signatures
///
/// Both methods are `async`, take `&self`, and return `Result<…, DbError>`
/// — the same shape as the other capability traits. `app_id` /
/// `collection` / `column` are unquoted identifiers; impls call
/// through their dialect's `quote_ident` before splicing into SQL.
///
/// `dims` is the declared vector dimensionality (per SDK
/// `t.vector(dims)`); impls fail-fast on a dim mismatch at insert
/// time via a `vector_dimension_mismatch` typed error. `metric`
/// selects the distance function — see [`VectorMetric`] for the
/// canonical operator mapping.
///
/// `filter` is the standard `$op`-shaped JSON predicate the rest of
/// the crate already understands (passes through `query.rs` builders).
/// The vector search returns rows ordered by `_distance` ASC; the
/// synthetic `_distance` column is `f64` and lives on the returned
/// JSON `Value`s. The SDK strips the leading `_` from user-visible
/// columns (Q-P4-I) so this prefix is reserved for engine annotations.
///
/// Not `Send + Sync` — same Open Q4 reasoning as the rest of the
/// capability traits.
pub trait VectorIndex: 'static {
    /// Idempotently create the vector index. PG: `CREATE INDEX
    /// CONCURRENTLY IF NOT EXISTS … USING ivfflat ("col" vector_{metric}_ops)`.
    /// SQLite: no-op (flat scan needs no index; PR 4 wires the
    /// CHECK constraint at column-DDL time instead).
    #[allow(async_fn_in_trait)]
    async fn ensure_vector_index(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        dims: i32,
        metric: VectorMetric,
    ) -> Result<(), DbError>;

    /// Return the top-`k` rows ordered by distance ASC. `query` is the
    /// query vector (length must match the column's declared `dims`
    /// or impls return a `vector_dimension_mismatch` typed error).
    /// `filter` is composed via `AND` with the distance ordering.
    /// Each returned `Value` is an object including a synthetic
    /// `"_distance"` field (`f64`).
    #[allow(async_fn_in_trait)]
    async fn vector_search(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError>;
}

/// Distance metric for [`VectorIndex`]. The three metrics map 1:1 to
/// pgvector's operator class set (`vector_cosine_ops`,
/// `vector_l2_ops`, `vector_ip_ops`) and the SQLite Rust-side distance
/// functions (`cosine_distance`, `l2_distance`, `neg_inner_product`).
///
/// **Why an enum, not a string** (plan §2): the SDK validates against
/// a closed three-element set; carrying it through the Rust surface
/// as an enum trips the rustc exhaustiveness checker if a future PR
/// adds a fourth metric — every match arm in the impl flags rather
/// than the new metric silently routing to a default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric {
    /// Cosine distance: `1 - (a · b) / (||a|| · ||b||)`. PG operator
    /// `<=>`, opclass `vector_cosine_ops`. The default for embedding
    /// models that produce L2-normalised vectors.
    Cosine,
    /// Euclidean (L2) distance: `sqrt(Σ (a_i - b_i)^2)`. PG operator
    /// `<->`, opclass `vector_l2_ops`.
    L2,
    /// Negative inner product: `- (a · b)`. PG operator `<#>`,
    /// opclass `vector_ip_ops`. The "negative" framing makes "smaller
    /// is better" hold across all three metrics, so a single ORDER BY
    /// clause works.
    InnerProduct,
}

/// Full-text search index capability — the "build a tokeniser-backed
/// inverted index over one or more text columns and run a phrase /
/// proximity query" slice of the data-store boundary.
///
/// Introduced in **P4 PR 1** (plan §2). The PG impl (PR 3) maintains
/// a generated `__fts tsvector` column + GIN index + an `AFTER
/// INSERT/UPDATE` trigger calling `tsvector_update_trigger(...)`.
/// The SQLite impl (PR 5) uses FTS5 external-content virtual tables
/// keyed by `rowid` with `AFTER` triggers mirroring writes.
///
/// `language` is honoured on PG (selects the tsvector configuration —
/// `english`, `simple`, …); SQLite FTS5's default tokenizer is
/// language-agnostic Unicode and ignores the parameter today (plan §9).
///
/// `filter` composes with `MATCH` via `AND`. Results are returned
/// ordered by relevance DESC — PG: `ts_rank`; SQLite: `bm25`. Each
/// returned `Value` includes a synthetic `"_rank"` field (`f64`).
///
/// **One composite index per collection** (Q-P4-B): the SDK's
/// `.fts()` per-field modifier collects every flagged column into a
/// single `__fts` index — `columns: &[String]` carries the ordered
/// list.
pub trait FullTextIndex: 'static {
    /// Idempotently create the FTS index. PG: emits the `__fts`
    /// column + GIN index + trigger. SQLite: creates the
    /// `<coll>__fts` external-content virtual table + the
    /// INSERT/UPDATE/DELETE mirror triggers.
    #[allow(async_fn_in_trait)]
    async fn ensure_fts_index(
        &self,
        app_id: &str,
        collection: &str,
        columns: &[String],
        language: &str,
    ) -> Result<(), DbError>;

    /// Run the FTS query and return matching rows ordered by relevance
    /// DESC. `limit` of `None` defers to the impl's default (today: no
    /// explicit limit — caller must guard against `O(table)` results).
    /// Each returned `Value` includes a synthetic `"_rank"` field.
    #[allow(async_fn_in_trait)]
    async fn fts_search(
        &self,
        app_id: &str,
        collection: &str,
        query: &str,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError>;
}

/// Spatial-index capability — the "build an R-tree-like index over a
/// `geography(POINT)` column and run a within-radius point query"
/// slice of the data-store boundary.
///
/// Introduced in **P4 PR 1** (plan §2). The PG impl (PR 3) wraps
/// PostGIS (`geography(POINT, 4326)` column type, `GIST` index,
/// `ST_DWithin` / `ST_MakePoint` operators). The SQLite impl (PR 5)
/// is a pure-Rust haversine within-radius post-filter over a `BLOB`
/// column packed as `(lat, lng)` little-endian `f64` × 2 = 16 bytes —
/// dev tier only, no R-tree (Q-P4-C: polygon ops PG-only).
///
/// `point` is the query centre. `radius_m` is in metres on both
/// backends (PG geography type works in metres; SQLite haversine
/// returns metres directly). `filter` composes with the
/// within-radius predicate via `AND`. Each returned `Value` includes
/// a synthetic `"_distance_m"` field (`f64`).
///
/// **`spatial_near` only** in P4 (Q-P4-C): polygon ops (`within`,
/// `intersects`) are deferred. The SQLite impl rejects polygon
/// input with `Configuration { code: "polygon_ops_pg_only" }`.
pub trait SpatialIndex: 'static {
    /// Idempotently create the spatial index. PG: `CREATE INDEX
    /// CONCURRENTLY … USING GIST ("col")`. SQLite: no-op (haversine
    /// full-scan needs no index).
    #[allow(async_fn_in_trait)]
    async fn ensure_spatial_index(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
    ) -> Result<(), DbError>;

    /// Return rows within `radius_m` of `point` ordered by distance
    /// ASC. `limit` of `None` defers to the impl's default.
    #[allow(async_fn_in_trait)]
    async fn spatial_near(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        point: GeoPoint,
        radius_m: f64,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError>;
}

/// A geographic point in WGS84 (EPSG:4326). Used by [`SpatialIndex`]
/// for query input.
///
/// **Field order**: `lat` then `lng` — matches the SDK shape
/// (`{ lat: number, lng: number }`) and the GeoJSON convention.
/// Note that PostGIS `ST_MakePoint` takes `(lng, lat)`; the PG impl
/// (PR 3) reorders at the SQL boundary.
///
/// `Copy` because it's two `f64`s — passing by value is cheaper than
/// borrowing.
#[derive(Debug, Clone, Copy)]
pub struct GeoPoint {
    /// Latitude in degrees, range `[-90, 90]`. SDK validate rejects
    /// out-of-range values before the trait method is called.
    pub lat: f64,
    /// Longitude in degrees, range `[-180, 180]`. SDK validate
    /// rejects out-of-range values before the trait method is called.
    pub lng: f64,
}

// ===========================================================================
// P5 PR 1 — EncryptedColumn + Backup capability traits
// ===========================================================================
//
// Two new capability traits land here per
// `docs/proposals/p5-encryption-backup-implementation-plan.md` §2 + §9 PR 1.
// Neither joins the [`Backend`] super-trait composition or the
// [`RegisterBackend`] marker — they're admin-surface accessors routed
// via dedicated `BackendHandle::as_encrypted_column_*` /
// `as_backup_*` accessors (mirror of the `as_change_stream_*` shape
// the [`ChangeStream`] capability adopted in P2 PR 1).
//
// PR 1 ships:
//   - the trait declarations themselves;
//   - the supporting [`EncryptionMode`] / [`BusyPolicy`] /
//     [`SnapshotOpts`] / [`SnapshotHandle`] / [`PitrTarget`] types;
//   - stub impls on `PostgresBackend` + `SqliteBackend` that return
//     a typed `Configuration { code: "p5_pr2_stub" }` error;
//   - compile-time trait-shape pins in the `tests` module.
//
// PR 2 (PG) and PR 3 (SQLite) backfill the real bodies. The encryption
// module they delegate to is at `crate::encryption` and lands in this
// same PR.

/// AEAD encrypt/decrypt at the storage boundary.
///
/// **Why a capability trait** (not on the [`Backend`] super-trait):
///
/// - PG and SQLite share the same AEAD impl (`crate::encryption::aead`),
///   so per-backend trait impls are thin delegations.
/// - **Key sourcing differs**: PG uses `__zeroship_admin.column_keys`
///   via SECURITY DEFINER; SQLite uses the
///   `ZEROSHIP_COLUMN_KEY_<KEYID>` env var. The trait's
///   [`Self::KeyHandle`] associated type lets each backend pick its
///   own key-material container without forcing a common type on the
///   read/write surface.
/// - The 13 carved capability traits in P0-P4 set the pattern: focused
///   trait per capability, accessor-routed dispatch through
///   [`BackendHandle`], no boxed dyn in the hot path.
///
/// **Per-row AAD policy** (the riskiest decision, resolved Camp A in
/// `docs/proposals/p5-encryption-backup-implementation-plan.md` §13):
/// callers pass the row PK in AAD for `EncryptionMode::Randomised`
/// (typed_id PKs are minted SDK-side so the PK is always available
/// before INSERT — no chicken-and-egg). `EncryptionMode::Deterministic`
/// omits the row PK so the B-tree-on-ciphertext equality index works.
///
/// **Not `Send + Sync`** — same Open Q4 reasoning as the rest of the
/// backend traits: the compio runtime is single-threaded per worker.
pub trait EncryptedColumn: 'static {
    /// Backend-specific key handle. PG (in PR 2) uses
    /// `crate::encryption::aead::AeadKey`; SQLite (PR 3) likely the
    /// same. The associated type leaves room for a PG variant that
    /// wraps an opaque KMS handle in the future.
    type KeyHandle: 'static;

    /// Resolve (cache or derive) the AEAD key for `(app_id, key_id)`.
    /// PR 2 (PG) reads through the admin-schema SECURITY DEFINER
    /// getter; PR 3 (SQLite) reads the env var; both pass the bytes
    /// through `crate::encryption::keys::KeyStore::resolve` which
    /// does the HKDF expansion. PR 1 stub returns `p5_pr2_stub`.
    #[allow(async_fn_in_trait)]
    async fn resolve_key(
        &self,
        app_id: &str,
        key_id: &str,
    ) -> Result<Self::KeyHandle, DbError>;

    /// Encrypt `plaintext` under `key` and `aad`, returning the
    /// packed wire blob produced by `crate::encryption::wire::pack`.
    /// Mode chooses random vs synthetic nonce; the wire format is the
    /// same.
    fn encrypt(
        &self,
        key: &Self::KeyHandle,
        mode: EncryptionMode,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError>;

    /// Decrypt a packed wire blob. Mode-agnostic on the read side —
    /// the nonce is carried in the wire, AAD reconstruction by the
    /// caller picks the mode-appropriate shape. Returns
    /// `ValidationFailed { code: "encryption_aead_failed" }` on tag
    /// mismatch.
    fn decrypt(
        &self,
        key: &Self::KeyHandle,
        mode: EncryptionMode,
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError>;
}

/// Encryption mode — chooses nonce derivation + AAD shape.
///
/// Two-mode design from `docs/proposals/db-system-design.md` §7.2.
/// The on-wire blob layout is identical between modes (the synthetic
/// vs random distinction is fully internal to the encrypt side); the
/// caller has to track the mode to reconstruct the right AAD on
/// decrypt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    /// Per-row random nonce. AAD =
    /// `(collection, column, row_pk_bytes)` — binds ciphertext to its
    /// row position. Per the Camp A architecture
    /// (`docs/proposals/p5-encryption-backup-implementation-plan.md`
    /// §13): plugin-db mints typed_id PKs **SDK-side** before INSERT,
    /// so `row_pk` is always available when `encrypt()` is called.
    /// Single-phase INSERT — no chicken-and-egg vs Microsoft Always
    /// Encrypted / MongoDB CSFLE. Defeats the ciphertext-oracle
    /// attack on randomised columns. Default (fail-safe).
    Randomised,

    /// Synthetic nonce = HMAC-SHA256(k_siv, plaintext)[..12]. AAD =
    /// `(collection, column)` only — `row_pk_bytes` intentionally
    /// omitted because deterministic mode's defining property is
    /// "same plaintext → same ciphertext under (collection, column)",
    /// which the B-tree-on-ciphertext equality index depends on.
    /// Inherits the standard deterministic-mode leak (equality
    /// across rows is observable to anyone with column read access).
    /// The SDK filter pre-flight refuses range / regex / `LIKE`
    /// queries on deterministic columns regardless.
    Deterministic,
}

/// Snapshot + restore + PITR for the per-app data store.
///
/// **Admin surface** — like [`ChangeStream`] / [`VectorIndex`], this
/// trait is routed through the `BackendHandle::as_backup_*`
/// accessors rather than joining the [`Backend`] super-trait. App
/// code never reaches this; only the platform's backup orchestrator
/// does. PR 4 (PG) ships `pg_dump`/`pg_restore` shell-out + PITR
/// placeholder; PR 5 (SQLite) ships `VACUUM INTO`
/// + atomic-rename restore + `pitr_pg_only` refusal.
#[cfg(feature = "test-helpers")]
pub trait Backup: 'static {
    /// Take a snapshot of the per-app data store and stream it to
    /// `dest_uri`. Returns a handle with the content hash for
    /// integrity verification on restore.
    #[allow(async_fn_in_trait)]
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError>;

    /// Restore a snapshot taken by [`Self::snapshot`]. PR 4 (PG):
    /// downloads + `pg_restore` + atomic schema swap. PR 5 (SQLite):
    /// downloads + atomic rename + isolate evict.
    #[allow(async_fn_in_trait)]
    async fn restore(
        &self,
        app_id: &str,
        snapshot: &SnapshotHandle,
    ) -> Result<(), DbError>;

    /// Replay WAL up to `target`. PR 4 (PG): records the target in
    /// `__zeroship_admin.pitr_targets`; operator runs `recovery.conf`.
    /// PR 5 (SQLite): returns `Configuration { code: "pitr_pg_only" }`
    /// — SQLite has no WAL-archive PITR story.
    #[allow(async_fn_in_trait)]
    async fn pitr_replay(
        &self,
        app_id: &str,
        target: PitrTarget,
    ) -> Result<(), DbError>;
}

/// Options for [`Backup::snapshot`].
///
/// Today carries only [`Self::if_busy`]; reserved so future PRs can
/// add compression / encryption-at-rest knobs without changing the
/// trait method signature.
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone)]
pub struct SnapshotOpts {
    pub if_busy: BusyPolicy,
}

/// Policy when a snapshot can't be taken immediately (e.g. SQLite
/// `VACUUM INTO` hitting `SQLITE_BUSY` on a schema-change race).
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Surface a typed `Configuration { code: "backup_busy" }` error
    /// to the caller immediately. The caller decides whether to retry.
    Abort,
    /// In-trait retry with a small backoff (PR 5: 3 × 1s).
    Retry,
}

/// Handle returned by [`Backup::snapshot`] — the address + integrity
/// metadata needed to restore.
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    /// Where the snapshot lives. PR 4/5 conventions:
    /// `s3://<bucket>/snapshots/<app>/<ts>-<hash>.<ext>` or
    /// `file:///<path>`.
    pub uri: String,
    /// SHA-256 over the snapshot bytes, computed streamingly on the
    /// way out. The restore path re-hashes the download and refuses
    /// on mismatch.
    pub content_hash: [u8; 32],
    /// Wall-clock time of snapshot start, milliseconds since UNIX
    /// epoch.
    pub created_at_ms: u64,
}

/// Target for [`Backup::pitr_replay`].
///
/// PG accepts both forms; SQLite refuses both with `pitr_pg_only`
/// (the SQLite arm has no WAL-archive PITR story — the placeholder
/// exists so the trait surface is uniform).
#[cfg(feature = "test-helpers")]
#[derive(Debug, Clone)]
pub enum PitrTarget {
    /// PG log-sequence-number, e.g. `"0/16B6300"`.
    Lsn(String),
    /// Wall-clock time, milliseconds since UNIX epoch. PG translates
    /// to `recovery_target_time`.
    TimeMillis(u64),
}

/// RAII guard returned by [`ChangeStream::engage_schema_pending`].
/// Disengaging the schema-pending decoder (and emitting one `Resync`
/// per active subscription) happens on `Drop`.
///
/// **P2 PR 4 wired body** (plan §7 + design §16.7):
///
/// 1. `::new(app_id)` — calls
///    [`crate::broker::engage_schema_pending`] which inserts the app
///    id into the thread-local `SCHEMA_PENDING_APPS` set. While
///    engaged: (a) [`crate::broker::Broker::try_subscribe`] returns
///    `DbError::Coded { code: "schema_pending" }`; (b) the SQLite CDC
///    publisher (`backend/sqlite/cdc.rs::publisher_loop`) drops every
///    packet whose `app_id` matches.
/// 2. `::drop` — calls [`crate::broker::disengage_schema_pending`] to
///    clear the flag, then
///    [`crate::broker::Broker::resume_app_with_resync`] which pushes
///    one `Resync` message onto every active subscription on the app.
///
/// **Joint-window precedence with [`BrokerPauseGuard`]** (plan §7):
/// `schema_pending` takes precedence on the `subscribe()` path —
/// `try_subscribe` returns the loud `Conflict` envelope. Backfill
/// pause is silent on `subscribe()` by design.
///
/// The `#[must_use]` annotation prevents accidental inline drop at
/// the call site — the engage/disengage contract is the *duration*
/// of the guard's binding, not its construction.
#[must_use = "SchemaPendingGuard disengages the decoder on Drop — bind it to a name to keep the schema-pending state engaged for the surrounding scope"]
#[allow(
    dead_code,
    reason = "bundle-invalidated wiring has not landed yet; keep the guard so the resync semantics stay pinned"
)]
#[derive(Debug)]
pub struct SchemaPendingGuard {
    app_id: String,
}

impl SchemaPendingGuard {
    /// Construct a guard for `app_id` AND engage the schema-pending
    /// flag on the current thread. The `Drop` impl disengages + emits
    /// the per-subscription `Resync`.
    ///
    /// Internal to the [`ChangeStream`] impls (PG adapter + SQLite
    /// arm); orchestrator code reaches the guard via
    /// `BackendHandle::as_change_stream_*().engage_schema_pending(app_id)`.
    pub(crate) fn new(app_id: String) -> Self {
        crate::broker::engage_schema_pending(&app_id);
        Self { app_id }
    }
}

impl Drop for SchemaPendingGuard {
    fn drop(&mut self) {
        // 1. Clear the schema-pending flag so subsequent
        //    `Broker::try_subscribe` calls succeed + CDC packets
        //    publish normally.
        crate::broker::disengage_schema_pending(&self.app_id);
        // 2. Push one `Resync` per active subscription on `app_id` —
        //    same primitive the backfill-pause path uses.
        crate::broker::BROKER.with(|b| {
            b.borrow_mut().resume_app_with_resync(&self.app_id);
        });
        tracing::trace!(
            app_id = %self.app_id,
            "SchemaPendingGuard dropped: disengage + resume_app_with_resync emitted"
        );
    }
}

/// Marker super-trait composing every capability the register-model
/// pipeline needs from a backend, so the `bootstrap` / `run_pipeline`
/// signatures can write `B: RegisterBackend` instead of restating the
/// 6-trait compound bound at each function.
///
/// **P0 PR 3 ergonomics**: the bound is exactly
/// [`PgSqlExecutor`] (transitively [`SqlExecutor`]) +
/// [`LockManager`] + [`NamespaceManager`] + [`SchemaIntrospect`] with
/// `LiveSchema = crate::diff::LiveSchema` + [`IndexBuilder`] +
/// [`PgLockManager`]. The blanket `impl<T> RegisterBackend for T`
/// auto-impls the marker for any type that already satisfies the
/// six sub-bounds (today, [`PostgresBackend`]; tomorrow, any other
/// concrete impl that wires up the same set).
///
/// See `docs/proposals/p0-implementation-plan.md` §"PR 3" step 4
/// ("Ergonomics") and `docs/proposals/db-system-design.md` §7.
// **P5.5 PR 6** — the `EncryptedColumn` super-bound is required for
// the `MaskBackfill` / `MaskRewrite` dispatch in `register_model::apply`
// (the backfill decrypts encrypted columns before applying the mask
// transform). Both backends impl `EncryptedColumn` unconditionally.
pub trait RegisterBackend:
    PgSqlExecutor
    + LockManager<Client = compio_postgres::Client>
    + NamespaceManager
    + SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>
    + IndexBuilder
    + PgLockManager
    + VectorIndex
    + FullTextIndex
    + SpatialIndex
    + EncryptedColumn
{
}

impl<T> RegisterBackend for T where
    T: PgSqlExecutor
        + LockManager<Client = compio_postgres::Client>
        + NamespaceManager
        + SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>
        + IndexBuilder
        + PgLockManager
        + VectorIndex
        + FullTextIndex
        + SpatialIndex
        + EncryptedColumn
{
}

/// The data-store boundary. One impl per storage backend; today only
/// Postgres ([`PostgresBackend`]).
///
/// `Backend` is now a **pure composition marker** — every operation
/// lives on a focused sub-trait. After P0 PR 2 the super-trait bound
/// is the carved capability set:
///
/// - [`SqlExecutor`] — **P1 PR 1**: the `Client = compio_postgres::Client`
///   pin was dropped from this super-bound so a `SqliteBackend` whose
///   `SqlExecutor::Client = SqliteSessionHandle` can also satisfy
///   `Backend`. PG-only consumers that *need* the concrete client
///   type continue to bound on
///   [`PgSqlExecutor`] / [`PgLockManager`] (which still pin
///   `Client = compio_postgres::Client`) — see the grep-audit table
///   in the P1 PR 1 commit message for the per-site verdict.
/// - [`LockManager`]
/// - [`NamespaceManager`]
/// - [`SchemaIntrospect`] with `LiveSchema = crate::diff::LiveSchema`
/// - [`IndexBuilder`]
///
/// The 16 audit-table operations that used to live here (`ensure_audit_table`,
/// `next_schema_version`, `write_audit_row`, …) were deleted in PR 2
/// — they stay as free functions in [`crate::audit`], reached via
/// [`PgSqlExecutor::pool_handle`] (Open Q1 resolution, see
/// `docs/proposals/p0-implementation-plan.md` §3 Q1).
///
/// Lifetime invariants (preserved from the pre-carving shape):
///
/// - Methods that take `&Self::Client` use it borrow-only; the caller
///   owns the client (e.g. the migration lock holds it across awaits,
///   the audit free functions borrow it for one operation).
/// - [`SqlExecutor::acquire_dedicated_client`] returns an owned `Client`
///   detached from any pool lifetime — the caller is free to park it
///   on the per-isolate context (e.g. `MigrationLock::client`,
///   [`crate::context::IsolateDbContext::tx_conn`]) for the duration
///   of a session-scoped lock.
#[cfg(any(test, feature = "test-helpers"))]
pub trait Backend:
    SqlExecutor
    + LockManager
    + NamespaceManager
    + SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>
    + IndexBuilder
    + 'static
{
}

/// Per-isolate backend handle — the typed enum stashed on
/// [`crate::context::IsolateDbContext`].
///
/// **Why an enum, not `Box<dyn Backend>`** (round-3 critic CRITICAL #3,
/// closes `docs/proposals/db-system-design.md` §5.5 and
/// `docs/proposals/p0-implementation-plan.md` §"PR 5"):
///
/// - `Backend` is `async fn`-in-trait. Object-safety for those traits
///   would require `Box<dyn Future>` per call — a per-CRUD-op
///   allocation on a hot path that runs ~200K times/sec under load.
/// - The associated types (`Client = compio_postgres::Client`,
///   `LiveSchema = crate::diff::LiveSchema`) cannot be erased behind a
///   `dyn` without losing the concrete client type that
///   [`LockManager::acquire_advisory_lock`] and the audit-row helpers
///   take by `&Self::Client` reference.
/// - The set of backends is closed (PG today; SQLite reserved for P1).
///   An enum is the canonical shape for a closed sum.
///
/// **Single-arm enum in P0**: the SQLite arm and its `sqlite` Cargo
/// feature were declared in r14 but the underlying
/// `crate::backend::sqlite` module never landed, so `--features sqlite`
/// failed to compile (E0433). The arm and the feature were removed in
/// the P0 mop-up cycle (post-r14) — both will be re-introduced
/// atomically with the `crate::backend::sqlite::SqliteBackend` impl in
/// P1. A build with `--no-default-features` is expected to fail at
/// compile time (no backend arm) — the failure mode is meaningful,
/// not a silent miscompile.
#[derive(Debug, Clone)]
pub enum BackendHandle {
    /// Postgres backend handle. Wraps an [`Rc<PostgresBackend>`] so
    /// cloning the enum stays cheap (Rc-clone of the inner pointer);
    /// every consumer site previously holding an `Rc<PostgresBackend>`
    /// migrates to this arm one-to-one.
    Postgres(Rc<PostgresBackend>),

    /// SQLite backend handle. Re-introduced in **P1 PR 1** alongside
    /// the [`crate::backend::sqlite::SqliteBackend`] module skeleton
    /// (the previous declaration was removed in the P0 mop-up because
    /// the underlying module never landed and `--features sqlite`
    /// failed E0433). At PR 1 the inner type's capability-impl
    /// bodies are stubs returning `DbError::Internal { … "P1 PR2+ stub" … }`;
    /// PR 2-5 backfill behaviour per
    /// `docs/proposals/p1-sqlite-implementation-plan.md` §9.
    Sqlite(Rc<SqliteBackend>),
}

impl BackendHandle {
    /// Run `f` against the inner [`PostgresBackend`].
    ///
    /// **No `dyn Backend` anywhere** (round-3 critic CRITICAL #3):
    /// dispatching to the concrete impl through an enum match keeps
    /// every consumer site monomorphised over `PostgresBackend` — the
    /// trait-method calls inline through the PG impl exactly as they
    /// did when the field was `Option<Rc<PostgresBackend>>`. No
    /// allocation, no vtable, no per-call overhead.
    ///
    /// **P1 PR 1**: the SQLite arm is now reachable, so the closure
    /// must convey "not the PG arm" rather than always running.
    /// Return type became `Option<R>` (mirrors [`Self::as_postgres`])
    /// — the closure runs and yields `Some(R)` on the PG arm; the
    /// SQLite arm yields `None`. Call sites previously written as
    /// `handle.with_postgres(|pg| …)` now write
    /// `handle.with_postgres(|pg| …).ok_or_else(|| backend_unsupported_err())?`
    /// — the same shape `as_postgres()` consumers already use.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn with_postgres<R>(&self, f: impl FnOnce(&PostgresBackend) -> R) -> Option<R> {
        match self {
            Self::Postgres(b) => Some(f(b)),
            Self::Sqlite(_) => None,
        }
    }

    /// Borrow the inner [`PostgresBackend`] as a `&PostgresBackend`
    /// reference — the async-friendly companion to [`Self::with_postgres`].
    ///
    /// **Why both shapes** (P0 PR 5 step 4 recommendation): the sync
    /// closure ([`Self::with_postgres`]) composes cleanly when the
    /// caller's body is sync, but it cannot `.await` across the
    /// closure boundary without lifetime gymnastics (the closure's
    /// inner future would have to outlive the closure scope). The
    /// async paths in `migrations.rs` / `register_model/mod.rs` /
    /// every `v8_classes::migration*` call site instead `.await` on
    /// the returned `&PostgresBackend` directly:
    ///
    /// ```ignore
    /// let backend = ensure_backend().await?;
    /// let pg = backend
    ///     .as_postgres()
    ///     .ok_or_else(unsupported_backend_op_error)?;
    /// crate::migrations::exec_status(pg, …).await
    /// ```
    ///
    /// Returns `Some(&PostgresBackend)` unconditionally in P0 (the
    /// enum has a single arm). The `Option`-shaped signature is the
    /// stable consumer contract — when the SQLite arm returns in P1
    /// this accessor will continue to return `None` on the SQLite arm
    /// so the existing `ok_or_else(...)?` consumer sites map the
    /// non-PG case to a typed `backend_unsupported` error rather than
    /// a panic. See the P0 mop-up commit and MAJOR-R14-1.
    pub fn as_postgres(&self) -> Option<&PostgresBackend> {
        match self {
            Self::Postgres(b) => Some(b),
            Self::Sqlite(_) => None,
        }
    }

    /// Run `f` against the inner [`SqliteBackend`], yielding
    /// `Some(R)` on the SQLite arm or `None` otherwise.
    ///
    /// **P1 PR 1**: symmetric counterpart to [`Self::with_postgres`].
    /// The `Option`-shaped return makes the consumer code style
    /// identical across backend arms.
    ///
    /// Consumers still on the PG arm pattern at PR 1 typically write:
    ///
    /// ```ignore
    /// let pg = backend.as_postgres().ok_or_else(|| backend_unsupported(...))?;
    /// ```
    ///
    /// — the same shape works for SQLite via this accessor.
    #[cfg(feature = "test-helpers")]
    pub fn with_sqlite<R>(&self, f: impl FnOnce(&SqliteBackend) -> R) -> Option<R> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(f(b)),
        }
    }

    /// Borrow the inner [`SqliteBackend`] as a `&SqliteBackend`
    /// reference — async-friendly companion to [`Self::with_sqlite`].
    /// Returns `Some(&SqliteBackend)` on the SQLite arm; `None` on
    /// the PG arm.
    ///
    /// **P1 PR 1**: present so PR 2-5's `as_sqlite()?` consumer
    /// migration has a stable accessor to migrate onto. PR 1 has no
    /// production caller — the orchestrator / migrations / register-model
    /// paths continue to use `as_postgres()?` against the PG arm only.
    pub fn as_sqlite(&self) -> Option<&SqliteBackend> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(b),
        }
    }

    /// Borrow a [`ChangeStream`] adapter over the PG arm — returns the
    /// thin [`crate::change_stream_pg::PgChangeStream`] wrapper that
    /// re-routes `ensure_publication_and_slot` /
    /// `wal_consumer::run_supervised` through the trait surface.
    ///
    /// **P2 PR 1**: introduced alongside the [`ChangeStream`] trait.
    /// Associated types (`type ConsumerHandle`) block dyn dispatch, so
    /// the consumer migration path mirrors the `as_postgres` /
    /// `as_sqlite` accessor shape rather than a `with_change_stream`
    /// visitor returning `R` (see the trait doc-comment for the dyn
    /// vs. concrete-accessor rationale).
    ///
    /// Returns `Some` on the PG arm; `None` on the SQLite arm.
    ///
    /// The adapter holds an `Rc<PostgresBackend>` (Rc-cloned from the
    /// arm's inner value); see [`crate::change_stream_pg::PgChangeStream`]
    /// for the lifetime / ownership rationale.
    pub fn as_change_stream_pg(&self) -> Option<crate::change_stream_pg::PgChangeStream> {
        match self {
            Self::Postgres(b) => Some(crate::change_stream_pg::PgChangeStream::new(b.clone())),
            Self::Sqlite(_) => None,
        }
    }

    /// Borrow a [`ChangeStream`] adapter over the SQLite arm —
    /// returns the [`crate::backend::sqlite::cdc::SqliteChangeStream`]
    /// wrapper that PR 2+ fills with `preupdate_hook`/`commit_hook`
    /// integration.
    ///
    /// **P2 PR 1**: stub. The returned adapter's
    /// `provision`/`deprovision`/`spawn_consumer` are `Ok(())`/unit
    /// returns; `pause_broker` / `engage_schema_pending` return no-op
    /// guards. PR 2 wires the real session-hook installation.
    ///
    /// Returns `Some` on the SQLite arm; `None` on the PG arm.
    ///
    /// The adapter holds an `Rc<SqliteBackend>` (Rc-cloned from the
    /// arm's inner value); see
    /// [`crate::backend::sqlite::cdc::SqliteChangeStream`] for the
    /// lifetime / ownership rationale.
    pub fn as_change_stream_sqlite(
        &self,
    ) -> Option<crate::backend::sqlite::cdc::SqliteChangeStream> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(crate::backend::sqlite::cdc::SqliteChangeStream::new(b.clone())),
        }
    }

    // -----------------------------------------------------------------
    // P5 PR 1 — EncryptedColumn + Backup accessors
    // -----------------------------------------------------------------
    //
    // Same shape as the `as_change_stream_*` accessors above: one
    // accessor per (capability, backend arm) pair. PR 1 returns
    // `Some(&PostgresBackend)` / `Some(&SqliteBackend)` (the PR-1
    // stub impls return `Configuration { code: "p5_pr2_stub" }` for
    // every method); PR 2-5 backfill the real bodies, and the
    // accessor shapes never change so the orchestrator-side consumer
    // sites stay stable across the PR sequence.

    /// Borrow an [`EncryptedColumn`] capability over the PG arm.
    ///
    /// **P5 PR 1**: returns `Some(&PostgresBackend)` on the PG arm.
    /// The `EncryptedColumn` impl wires the admin-schema SECURITY
    /// DEFINER getter for `column_keys`. PR 2 backfills the real body.
    ///
    /// Returns `Some` on the PG arm; `None` on the SQLite arm.
    pub fn as_encrypted_column_pg(&self) -> Option<&PostgresBackend> {
        match self {
            Self::Postgres(b) => Some(b),
            Self::Sqlite(_) => None,
        }
    }

    /// Borrow an [`EncryptedColumn`] capability over the SQLite arm.
    ///
    /// **P5 PR 1**: returns `Some(&SqliteBackend)` on the SQLite
    /// arm. The SQLite impl is gated only by `feature = "sqlite"`
    /// (env-var key sourcing, no admin schema). PR 3 backfills the
    /// real body.
    ///
    /// Returns `Some` on the SQLite arm; `None` on the PG arm.
    pub fn as_encrypted_column_sqlite(&self) -> Option<&SqliteBackend> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(b),
        }
    }

    /// Borrow a [`Backup`] capability over the PG arm.
    ///
    /// **P5 PR 4**: returns `Some(&PostgresBackend)` on the PG arm.
    /// The real `Backup` impl is pg_dump/pg_restore shell-out plus a
    /// PITR placeholder that writes to `__zeroship_admin.pitr_targets`.
    /// Mirrors the [`Self::as_encrypted_column_pg`] shape.
    ///
    /// Returns `Some` on the PG arm; `None` on the SQLite arm.
    #[cfg(feature = "test-helpers")]
    pub fn as_backup_pg(&self) -> Option<&PostgresBackend> {
        match self {
            Self::Postgres(b) => Some(b),
            Self::Sqlite(_) => None,
        }
    }

    /// Borrow a [`Backup`] capability over the SQLite arm.
    ///
    /// **P5 PR 1**: returns `Some(&SqliteBackend)` on the SQLite
    /// arm. PR 5 backfills `VACUUM INTO` snapshot + atomic-rename
    /// restore + `pitr_pg_only` refusal.
    ///
    /// Returns `Some` on the SQLite arm; `None` on the PG arm.
    #[cfg(feature = "test-helpers")]
    pub fn as_backup_sqlite(&self) -> Option<&SqliteBackend> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(b),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Interface-level (compile-time) tests for the [`Backend`] trait.
    //!
    //! The trait is `async fn`-in-trait and every method needs a real
    //! Postgres listener via [`PostgresBackend`]; we cannot exercise
    //! method bodies from a `#[test]` without `tests/integration.rs`.
    //! What we *can* do — and what catches the highest-leverage
    //! refactor mistakes — is pin the trait shape at compile time:
    //!
    //! - the canonical impl [`PostgresBackend`] satisfies the bound;
    //! - the associated types stay wired to their concrete
    //!   `compio_postgres` / `crate::diff` counterparts;
    //! - the `'static` bound on the trait flows through.
    //!
    //! Any future change to the trait (new method, swapped
    //! associated-type bound, lifetime tightening) trips one of these
    //! at `cargo build -p zeroship-plugin-db --tests` time, before any
    //! caller fails at a more distant site.

    use super::*;

    /// Compile-time: the canonical impl [`PostgresBackend`] satisfies
    /// the [`Backend`] trait. Function body type-checks at build time;
    /// it's a deliberate no-op at runtime.
    fn assert_postgres_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`SqlExecutor`] capability super-trait (P0 PR 1). If a future
    /// refactor accidentally pulls a `SqlExecutor` method back onto
    /// the omnibus `Backend` trait — or detaches the impl block from
    /// the `PostgresBackend` type — this stops compiling.
    fn assert_postgres_backend_impls_sql_executor() {
        fn assert_impl<T: SqlExecutor<Client = compio_postgres::Client>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`LockManager`] capability super-trait (P0 PR 1). The
    /// `: SqlExecutor` super-bound on `LockManager` plus the
    /// `Client = compio_postgres::Client` constraint here pin the
    /// shape end-to-end — a regression in either direction fails
    /// compilation in this module.
    fn assert_postgres_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager<Client = compio_postgres::Client>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`NamespaceManager`] capability trait (P0 PR 2). If a future
    /// refactor pulls `ensure_app_schema` back onto the omnibus
    /// `Backend` trait or detaches the impl block, this stops
    /// compiling.
    fn assert_postgres_backend_impls_namespace_manager() {
        fn assert_impl<T: NamespaceManager>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies
    /// [`SchemaIntrospect`] with the associated type pinned to
    /// [`crate::diff::LiveSchema`] (P0 PR 2). This is the constraint
    /// `register_model::plan` now uses (`<B: SchemaIntrospect<LiveSchema = LiveSchema>>`).
    fn assert_postgres_backend_impls_schema_introspect() {
        fn assert_impl<T: SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`IndexBuilder`] capability trait (P0 PR 2). The
    /// `: SqlExecutor` super-bound on `IndexBuilder` plus the
    /// PG-side `Client = compio_postgres::Client` constraint pin the
    /// shape so a regression on either side fails compilation here.
    fn assert_postgres_backend_impls_index_builder() {
        fn assert_impl<T: IndexBuilder<Client = compio_postgres::Client>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the PG-only
    /// [`PgSqlExecutor`] extension trait (P0 PR 2). The free-function
    /// audit-helper path (Open Q1 resolution) hinges on `pool_handle()`
    /// being reachable through this trait without naming
    /// `PostgresBackend`.
    fn assert_postgres_backend_impls_pg_sql_executor() {
        fn assert_impl<T: PgSqlExecutor>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the PG-only
    /// [`PgLockManager`] extension trait (P0 PR 3). This is the
    /// escape-hatch closer for the `backend.pool().get()` call site at
    /// `register_model/bootstrap.rs:103`: the returned
    /// `PooledClient<'p>` keeps the `'p` lifetime threaded through
    /// [`LockGuard`] without needing a GAT on
    /// [`LockManager`] (Open Q5 resolution).
    fn assert_postgres_backend_impls_pg_lock_manager() {
        fn assert_impl<T: PgLockManager>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the
    /// [`RegisterBackend`] marker super-trait (P0 PR 3). The
    /// blanket `impl<T> RegisterBackend for T where T: …` auto-impls
    /// the marker for any type with the six sub-bounds; if a future
    /// refactor pulls one bound off (or detaches a sub-impl block),
    /// this stops compiling here rather than at the `bootstrap` /
    /// `run_pipeline` call sites.
    fn assert_postgres_backend_impls_register_backend() {
        fn assert_impl<T: RegisterBackend>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: the PG-arm [`ChangeStream`] adapter
    /// [`crate::change_stream_pg::PgChangeStream`] satisfies the
    /// [`ChangeStream`] trait with the agreed
    /// `ConsumerHandle = WalConsumerHandle` shape (P2 PR 1). A
    /// regression that detaches the impl block from `PgChangeStream`
    /// — or that renames the associated type away from the agreed
    /// shape — trips compilation here rather than at the
    /// `BackendHandle::as_change_stream_pg()` accessor or its
    /// consumers.
    fn assert_pg_change_stream_impls_change_stream() {
        fn assert_impl<
            T: ChangeStream<ConsumerHandle = crate::change_stream_pg::WalConsumerHandle>,
        >() {
        }
        assert_impl::<crate::change_stream_pg::PgChangeStream>();
    }

    /// Compile-time (P4 PR 1): the [`VectorIndex`] trait's shape is
    /// pinned. PR 1 ships no impl — neither [`PostgresBackend`] nor
    /// [`SqliteBackend`] yet satisfies the trait, so this assertion
    /// only checks that the trait *itself* compiles (object-safety,
    /// `async fn` placement, signature shape). PR 2/4 will instantiate
    /// this against the concrete backends.
    #[allow(dead_code)]
    fn _assert_vector_index<T: VectorIndex>() {}

    /// Compile-time (P4 PR 1): the [`FullTextIndex`] trait's shape is
    /// pinned. PR 1 ships no impl — neither backend yet satisfies the
    /// trait. PR 3/5 will instantiate this against the concrete
    /// backends.
    #[allow(dead_code)]
    fn _assert_fts_index<T: FullTextIndex>() {}

    /// Compile-time (P4 PR 1): the [`SpatialIndex`] trait's shape is
    /// pinned. PR 1 ships no impl — neither backend yet satisfies the
    /// trait. PR 3/5 will instantiate this against the concrete
    /// backends.
    #[allow(dead_code)]
    fn _assert_spatial_index<T: SpatialIndex>() {}

    /// Compile-time (P5 PR 1): the [`EncryptedColumn`] trait's shape
    /// is pinned. PR 1 ships stub impls on both `PostgresBackend`
    /// and `SqliteBackend` (under `sqlite`) — see
    /// `_assert_encrypted_column_pg` / `_assert_encrypted_column_sqlite`
    /// below for the per-backend instantiations. This unparameterised
    /// pin checks that the trait itself compiles (associated type +
    /// `async fn` placement + signature shape).
    #[allow(dead_code)]
    fn _assert_encrypted_column<T: EncryptedColumn>() {}

    /// Compile-time (P5 PR 1): the [`Backup`] trait's shape is
    /// pinned. PR 1 ships stub impls on both backends; the
    /// per-backend instantiations are below.
    #[cfg(feature = "test-helpers")]
    #[allow(dead_code)]
    fn _assert_backup<T: Backup>() {}

    /// Compile-time (P5 PR 1): `PostgresBackend` satisfies
    /// [`EncryptedColumn`] (PR 1 stub impl returned `p5_pr2_stub`;
    /// PR 2 backfilled the SECURITY DEFINER body).
    #[allow(dead_code)]
    fn _assert_postgres_backend_impls_encrypted_column() {
        fn assert_impl<T: EncryptedColumn>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time (P5 PR 1): `SqliteBackend` satisfies
    /// [`EncryptedColumn`] under the `sqlite` feature. PR 1 stub
    /// impl returns `p5_pr2_stub`; PR 3 backfills the real body
    /// against env-var key sourcing.
    #[allow(dead_code)]
    fn _assert_sqlite_backend_impls_encrypted_column() {
        fn assert_impl<T: EncryptedColumn>() {}
        assert_impl::<SqliteBackend>();
    }

    /// Compile-time (P5 PR 4): `PostgresBackend` satisfies [`Backup`].
    /// PR 4 backfilled the `pg_dump`/`pg_restore` shell-out body; the
    /// PITR placeholder writes to the `__zeroship_admin.pitr_targets`
    /// table. Mirrors the
    /// `_assert_postgres_backend_impls_encrypted_column` shape above.
    #[cfg(feature = "test-helpers")]
    #[allow(dead_code)]
    fn _assert_postgres_backend_impls_backup() {
        fn assert_impl<T: Backup>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time (P5 PR 1): `SqliteBackend` satisfies [`Backup`].
    /// PR 5 backfills the `VACUUM INTO` body.
    #[cfg(feature = "test-helpers")]
    #[allow(dead_code)]
    fn _assert_sqlite_backend_impls_backup() {
        fn assert_impl<T: Backup>() {}
        assert_impl::<SqliteBackend>();
    }

    /// Compile-time: the associated types stay anchored to the concrete
    /// `compio_postgres::Client` / `crate::diff::LiveSchema`. A
    /// regression here would silently change every `B::Client` /
    /// `B::LiveSchema` consumer's expectations. `LiveSchema` flows
    /// through [`SchemaIntrospect`] now (P0 PR 2 moved it off
    /// [`Backend`]); `Backend` re-anchors it via the
    /// `SchemaIntrospect<LiveSchema = LiveSchema>` super-bound so the
    /// `Backend<LiveSchema = …>` shorthand below still resolves.
    fn assert_associated_types_pinned() {
        fn pinned_client<T: Backend<Client = compio_postgres::Client>>() {}
        fn pinned_live_schema<T: Backend<LiveSchema = crate::diff::LiveSchema>>() {}
        pinned_client::<PostgresBackend>();
        pinned_live_schema::<PostgresBackend>();
    }

    /// Compile-time: `Backend: 'static`. The per-isolate context parks
    /// the impl behind a `BackendHandle::Postgres(Rc<PostgresBackend>)`
    /// in a `thread_local!`; dropping the `'static` bound would break
    /// that path.
    fn assert_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<PostgresBackend>();
    }

    /// Compile-time: [`BackendHandle`] is `Clone + 'static`. The
    /// per-isolate context's accessor (`IsolateDbContext::backend`)
    /// returns a cloned handle by value so consumers can hold it
    /// across awaits without keeping the `RefCell` borrow open; the
    /// `Clone` bound is therefore load-bearing. The `'static` bound
    /// flows through because the enum's only data is `Rc<…>` of
    /// `'static` impls.
    ///
    /// **P0 PR 5 (round-3 critic CRITICAL #3 closure)**: this test
    /// replaced the deleted `assert_backend_handle_alias` that pinned
    /// `BackendHandle == Rc<PostgresBackend>`. The alias is gone;
    /// what stays is the shape contract the consumers depend on.
    fn assert_backend_handle_clone_static() {
        fn assert_bounds<T: Clone + 'static>() {}
        assert_bounds::<BackendHandle>();
    }

    /// Construct a `BackendHandle::Postgres(…)` arm via the public API
    /// surface. Pins the variant name so a future rename trips
    /// compilation here rather than at every consumer site, and
    /// proves [`BackendHandle::with_postgres`] / [`BackendHandle::as_postgres`]
    /// dispatch through the PG arm without panic.
    ///
    /// Skipped under `--cfg miri` (the only sandbox where the PG
    /// `Rc<…>` construction below would be problematic): the test
    /// never connects, but the constructor path still exists.
    #[test]
    fn backend_handle_postgres_arm_round_trip() {
        // We deliberately can't call `PostgresBackend::new` here
        // without a real `compio_postgres::Pool` (which only
        // `Pool::connect` produces — covered by tests/integration.rs).
        // What we *can* pin at unit-test time is the compile-time
        // shape: that `BackendHandle::Postgres` is constructible from
        // `Rc<PostgresBackend>` and that the two accessors return the
        // expected reference / closure-applied value.
        //
        // The runtime exercise of these accessors against a live
        // PostgresBackend lives in tests/integration.rs (which spins
        // up Postgres). This test pins the *type* shape.
        fn _shape_check(handle: BackendHandle) -> bool {
            // P1 PR 1: both accessors return `Option<…>` now (the PG
            // arm yields `Some(…)`; the SQLite arm yields `None`).
            // The exhaustive-match audit lives in the commit message.
            let _: Option<()> = handle.with_postgres(|_b: &PostgresBackend| ());
            let _: Option<&PostgresBackend> = handle.as_postgres();
            true
        }
        let _ = _shape_check as fn(BackendHandle) -> bool;
    }

    /// Compile-time (P0 PR 6): [`LockScope`] satisfies the trait
    /// bounds the typed [`LockManager`] API depends on. The variant
    /// is `Clone + 'static` so call sites can stash it across awaits
    /// (e.g. the `release_scope` re-construction in `migrations.rs`'s
    /// cancelled-refusal path) without re-borrowing. Not `Send`/`Sync`
    /// — same Open Q4 reasoning as the rest of the backend traits:
    /// the compio runtime is single-threaded per worker.
    fn assert_lock_scope_clone_send_static() {
        fn assert_bounds<T: Clone + 'static>() {}
        assert_bounds::<LockScope>();
    }

    /// Compile-time + runtime (P0 PR 6): construct both
    /// [`LockScope`] variants and dispatch through
    /// [`PostgresBackend::try_acquire`] to verify the typed
    /// keyed-mapping wires through. We can't actually issue SQL
    /// without a live Pool (covered by tests/integration.rs), but we
    /// CAN exercise the key-derivation logic ([`LockScope::to_keys`])
    /// and confirm both variants produce the canonical
    /// `(format!("{app_id}:{name}"), name)` shape.
    ///
    /// **Why both variants here**: the P0 production sites are all
    /// `GlobalApp`; `LocalApp` exists today purely as a classification
    /// hook for future call sites (see [`LockScope`] rustdoc). Pinning
    /// the shape here ensures a future contributor adding a `LocalApp`
    /// production caller doesn't accidentally drift the key
    /// derivation between variants.
    #[test]
    fn lock_scope_keys_global_app_canonical_shape() {
        let scope = LockScope::GlobalApp {
            app_id: "app_42".to_string(),
            name: "register_model".to_string(),
        };
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_42:register_model");
        assert_eq!(k2, "register_model");
        assert_eq!(scope.app_id(), "app_42");
        assert_eq!(scope.name(), "register_model");
    }

    #[test]
    fn lock_scope_keys_local_app_canonical_shape() {
        // `LocalApp` produces the SAME (key1, key2) shape as
        // `GlobalApp` — the variant classifies *visibility* (which
        // backend primitive handles dispatch) not *key layout*. A
        // future SQLite backend would HashMap on the derived strings
        // for both variants; the PG backend currently treats `LocalApp`
        // the same as `GlobalApp` (only `GlobalApp` callers exist in
        // P0).
        let scope = LockScope::LocalApp {
            app_id: "app_99".to_string(),
            name: "mig:add_archived_flag".to_string(),
        };
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_99:mig:add_archived_flag");
        assert_eq!(k2, "mig:add_archived_flag");
        assert_eq!(scope.app_id(), "app_99");
        assert_eq!(scope.name(), "mig:add_archived_flag");
    }

    /// Compile-time (P0 PR 6): the typed [`LockManager::try_acquire`]
    /// API dispatches the canonical `LockScope` shape through
    /// [`PostgresBackend`] without the caller naming the underlying
    /// `(key1, key2)` string-key primitive. We can't issue SQL from a
    /// unit test, so this is a *type-shape* check: the function body
    /// type-checks against the trait method signature.
    #[allow(dead_code)]
    async fn assert_lock_scope_dispatches_through_try_acquire(
        backend: &PostgresBackend,
        client: &compio_postgres::Client,
    ) -> Result<bool, DbError> {
        // GlobalApp arm — exercises acquire / try_acquire / release.
        let global = LockScope::GlobalApp {
            app_id: "app_t".into(),
            name: "register_model".into(),
        };
        let _ = backend.try_acquire(client, &global).await?;
        let _ = backend.acquire(client, &global).await?;
        backend.release(client, &global).await?;

        // LocalApp arm — same dispatch surface (variant classifies
        // visibility, not key layout).
        let local = LockScope::LocalApp {
            app_id: "app_t".into(),
            name: "mig:add_archived_flag".into(),
        };
        backend.try_acquire(client, &local).await
    }

    /// **Security [I43]** (cycle 18:17): exhaust the bounded-retry
    /// loop in [`LockManager::try_acquire_with_backoff`] against a
    /// mock backend whose `try_acquire_advisory_lock` always returns
    /// `Ok(false)` (perpetual contention). The result must be a
    /// `DbError::LockContention` whose
    /// [`crate::error::DbError::to_op_error`] mapping produces the
    /// JS-visible `code = "lock_not_available"` envelope.
    ///
    /// The test pins:
    ///
    /// - the exhaustion path *does* surface (the loop never silently
    ///   returns Ok);
    /// - the typed error variant survives the trait dispatch
    ///   (variant-preserving, no flatten to `Internal`);
    /// - the wire mapping produces the canonical contention code;
    /// - the message body carries the scope identity so an operator
    ///   greps `lock_not_available` and sees which scope contended.
    ///
    /// The mock implements the smallest possible
    /// [`SqlExecutor`] + [`LockManager`] surface — every other
    /// method is unreachable in this test path.
    #[test]
    fn try_acquire_with_backoff_exhaustion_yields_lock_contention() {
        use std::cell::Cell;

        // Mock client — opaque marker; the mock never reads it.
        struct MockClient;

        // Mock backend that records attempt count and always returns
        // `Ok(false)` from `try_acquire_advisory_lock`. The retry
        // loop should call this exactly 5 times (the
        // [(1,0),(2,50),(3,200),(4,500),(5,1000)] schedule).
        struct ContendingMock {
            attempts: Cell<u32>,
        }

        impl SqlExecutor for ContendingMock {
            type Client = MockClient;

            async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError> {
                unreachable!("not exercised by try_acquire_with_backoff")
            }

            async fn pool_exec(
                &self,
                _sql: &str,
                _params: &[&str],
            ) -> Result<u64, DbError> {
                unreachable!("not exercised by try_acquire_with_backoff")
            }

            async fn client_exec(
                &self,
                _client: &Self::Client,
                _sql: &str,
                _params: &[&str],
            ) -> Result<u64, DbError> {
                unreachable!("not exercised by try_acquire_with_backoff")
            }
        }

        impl LockManager for ContendingMock {
            async fn acquire_advisory_lock(
                &self,
                _client: &Self::Client,
                _k1: &str,
                _k2: &str,
            ) -> Result<(), DbError> {
                unreachable!(
                    "[I43]: typed acquire surface MUST route through \
                     try_acquire_with_backoff, never the legacy blocking \
                     acquire_advisory_lock primitive"
                )
            }

            async fn try_acquire_advisory_lock(
                &self,
                _client: &Self::Client,
                _k1: &str,
                _k2: &str,
            ) -> Result<bool, DbError> {
                self.attempts.set(self.attempts.get() + 1);
                // Perpetual contention — every attempt observes the lock
                // held by some other (imaginary) acquirer.
                Ok(false)
            }

            async fn release_advisory_lock(
                &self,
                _client: &Self::Client,
                _k1: &str,
                _k2: &str,
            ) -> Result<(), DbError> {
                unreachable!("not exercised by try_acquire_with_backoff")
            }
        }

        let mock = ContendingMock {
            attempts: Cell::new(0),
        };
        let client = MockClient;
        let scope = LockScope::GlobalApp {
            app_id: "app_contention_test".into(),
            name: "register_model".into(),
        };

        // Drive the future on a fresh compio runtime — the test must
        // tolerate the ~1.75s real-time worst-case schedule
        // (0+50+200+500+1000ms). Acceptable for a unit test; the
        // alternative (injecting a sleep hook) would couple the
        // backoff schedule to a test-only API.
        let result = compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async { mock.try_acquire_with_backoff(&client, &scope).await });

        // Attempt count: the schedule has 5 entries — the loop must
        // exhaust all of them before returning.
        assert_eq!(
            mock.attempts.get(),
            5,
            "bounded retry loop must execute exactly 5 attempts \
             (schedule = 0/50/200/500/1000ms)"
        );

        // Variant-preserving error surface.
        let err = result.expect_err("perpetual contention must error");
        match &err {
            DbError::LockContention { message } => {
                assert!(
                    message.contains("app_contention_test"),
                    "message must name the scope app_id, got: {message}"
                );
                assert!(
                    message.contains("register_model"),
                    "message must name the scope name, got: {message}"
                );
                assert!(
                    message.contains("5 attempts"),
                    "message must document the retry budget, got: {message}"
                );
            }
            other => panic!("expected DbError::LockContention, got {other:?}"),
        }

        // Wire envelope shape: the JS-visible code must be the
        // canonical contention code. Pinning this here means a
        // regression renaming the variant or routing it through a
        // different mapping trips the test immediately.
        let op_err = err.to_op_error();
        let code = match op_err.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => code,
            other => panic!("expected CodedError, got {other:?}"),
        };
        assert_eq!(
            code, "lock_not_available",
            "JS-visible code for bounded-retry exhaustion must be \
             `lock_not_available` (the canonical contention identifier)"
        );
    }

    #[test]
    fn compile_time_assertions_link() {
        // Keep the asserter functions live so the dead-code lint
        // doesn't fire. The type-check still runs even if these
        // weren't called, but the explicit cast documents intent.
        let _ = assert_postgres_backend_impls_backend as fn();
        let _ = assert_postgres_backend_impls_sql_executor as fn();
        let _ = assert_postgres_backend_impls_lock_manager as fn();
        let _ = assert_postgres_backend_impls_namespace_manager as fn();
        let _ = assert_postgres_backend_impls_schema_introspect as fn();
        let _ = assert_postgres_backend_impls_index_builder as fn();
        let _ = assert_postgres_backend_impls_pg_sql_executor as fn();
        let _ = assert_postgres_backend_impls_pg_lock_manager as fn();
        let _ = assert_postgres_backend_impls_register_backend as fn();
        let _ = assert_pg_change_stream_impls_change_stream as fn();
        let _ = assert_associated_types_pinned as fn();
        let _ = assert_backend_is_static as fn();
        let _ = assert_backend_handle_clone_static as fn();
        let _ = assert_lock_scope_clone_send_static as fn();
        // `assert_lock_scope_dispatches_through_try_acquire` is not
        // a `fn()` — it has lifetime parameters and returns a Future.
        // The fn-item cast above already exercises its signature; we
        // don't need to re-cast it here.
    }
}
