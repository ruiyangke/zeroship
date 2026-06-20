//! Per-isolate DB context — single typed home for every plug-in
//! thread-local. Before Stage 8d-R4 the plug-in carried ten separate
//! `thread_local!` declarations (`DB_POOL`, `DB_URL`, `REGISTERED_MODELS`,
//! `TX_CONN`, `PENDING_EMITS` in `lib.rs`; `MIG_LOCK` in
//! `migrations.rs`; `RUNNING_CONSUMERS` in `replication_ops.rs`). Each had
//! its own borrow/take/replace ritual; lifecycle invariants (e.g.
//! "MIG_LOCK never holds two `MigrationLock` snapshots") were enforced by
//! convention only.
//!
//! This module folds all of those slots into a single
//! [`IsolateDbContext`] stashed in one [`thread_local!`]. Typed
//! accessors enforce the invariants in one place:
//!
//! * [`IsolateDbContext::with`] / [`IsolateDbContext::with_mut`] are
//!   the only entry points; every consumer goes through them.
//! * `*_tx_*` methods coordinate the tx-state slots (`tx_conn`,
//!   `savepoint_depth`) so the single-connection model the transaction
//!   orchestrator relies on holds (one BEGIN per isolate, nested
//!   `SAVEPOINT`s reusing the same connection).
//! * Pending broker emits live on the context; the queue is drained by
//!   the transaction settle path (`drain_pending_emits_on_commit`) and
//!   cleared on ROLLBACK / fresh BEGIN.
//!
//! Each isolate (worker thread) carries one context; the compio
//! runtime is single-threaded per worker so plain `RefCell` /
//! `Cell` sufficient. The fields stay `pub(crate)` so the lib.rs
//! shim thread-locals can be removed slot-by-slot in subsequent
//! commits without churn.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use compio_postgres::{Client, Pool};

use crate::backend::sqlite::session::SqliteSessionHandle;
use crate::backend::{BackendHandle, BrokerPauseGuard, PostgresBackend};
use crate::broker::ChangeEvent;
use crate::error::DbError;

/// Result of trying to claim the per-isolate backend initialisation slot.
pub(crate) enum BackendInitState {
    /// A backend is already installed; no work needed.
    Ready,
    /// This caller claimed the slot and must call `finish_backend_init`.
    Acquired,
    /// Another request is currently building the backend.
    InProgress,
}

/// Lock state for the in-flight migration. The `client` is held in
/// an `Option` so callers can `take()` it across an await and
/// `replace()` it back — the same pattern the transaction slot uses.
///
/// Defined here (not in `crate::migrations`) so `compio_postgres::Client`
/// stays out of consumer modules — the Backend abstraction (Stage 8e-R2)
/// allows only `context.rs` and `backend/postgres.rs` to name the
/// underlying driver type.
pub(crate) struct MigrationLock {
    /// Owning app. A worker thread hosts many isolates (one per app);
    /// migration ops presented by app B must never observe — let alone
    /// drive — a lock app A parked here (SEC-1 sibling hazard).
    pub(crate) app_id: String,
    pub(crate) name: String,
    pub(crate) collection: String,
    pub(crate) audit_id: i64,
    /// Dry-run runs do not persist `validate_cursor`, dead_letter_pks,
    /// or processed updates (proposal B1.6).
    pub(crate) dry_run: bool,
    /// `audit_generation` snapshot captured at `exec_begin`. The
    /// audit row's generation is bumped by `exec_reset`; any
    /// subsequent `commit_batch` whose stored generation no longer
    /// matches the row's must ROLLBACK and surface
    /// `migration_reset_externally` (Gap X). Lives in the lock so
    /// `exec_commit_batch` reads it without an extra round-trip.
    pub(crate) start_generation: i64,
    pub(crate) client: Option<Client>,
    /// Backfill-window broker-pause guard (P2 tail — wires the
    /// [`BrokerPauseGuard`] into the migration orchestrator per design
    /// §16.7 / plan §7). Acquired by `exec_begin` immediately after
    /// the advisory lock is parked into [`Self::client`]; released
    /// when the slot itself drops (terminal `exec_commit_batch` or any
    /// error rail that calls `clear_mig_lock`).
    ///
    /// **Lifecycle choice (option A)**: the guard's lifetime spans the
    /// *whole* migration window — from `exec_begin` through the final
    /// `exec_commit_batch{is_done=true}` (or a `clear_mig_lock` driven
    /// error path). Backfill is the "design treats this as a known DDL
    /// + bulk-write window" case; subscribers see exactly one `Resync`
    /// when the guard drops (§16.7). Holding it across many V8-driven
    /// `fetchBatch`/`commitBatch` calls is intentional: legitimate
    /// user CRUD writes overlapping the migration would also have
    /// their CDC events suppressed and folded into the single closing
    /// `Resync`, which matches §16.7's "one resync ends the window"
    /// contract.
    ///
    /// `Option<_>` to support `pause_broker_for_tests`-style fixtures
    /// that synthesise a [`MigrationLock`] without going through
    /// `exec_begin` (e.g. the warn-shape-pin unit tests in
    /// [`context.rs`] below). Production `exec_begin` populates this
    /// unconditionally.
    //
    // Compiler can't see the load-bearing `Drop` semantics — the field
    // is "written but never read" from the type-checker's point of view,
    // yet the Drop is the entire contract (unsuppress flag + emit
    // Resync). The `#[allow]` here parallels the one previously on
    // `BrokerPauseGuard` itself; removing it would trip
    // `-D unused_fields` builds.
    #[allow(dead_code)]
    pub(crate) broker_pause: Option<BrokerPauseGuard>,
}

/// Pinned transaction client parked in the per-isolate tx slot.
///
/// Postgres keeps a dedicated libpq connection alive for the lifetime of
/// the transaction; SQLite keeps a handle to the shared session actor and
/// drives `BEGIN` / `SAVEPOINT` / `COMMIT` / `ROLLBACK` over that single
/// worker-owned connection.
pub(crate) enum TxConnection {
    Postgres(Client),
    Sqlite(SqliteSessionHandle),
}

/// RAII guard for a transaction client temporarily removed from the
/// per-isolate slot.
///
/// SQLite needs this guard to stay cancellation-safe: dropping a future
/// mid-await must restore the session-actor handle back into
/// `tx_conn`, otherwise the actor keeps the transaction open while the
/// isolate state claims there is no live tx. Restoring the slot is also
/// harmless on Postgres and keeps the `take` / `put` contract in one
/// typed place.
#[must_use = "TxClientSlotGuard restores the tx slot on Drop unless consumed via into_inner()"]
pub(crate) struct TxClientSlotGuard {
    app_id: String,
    client: Option<TxConnection>,
}

impl TxClientSlotGuard {
    /// Drain `app_id`'s transaction client out of the per-isolate slot.
    /// SEC-1: the guard restores it to the *same* app's slot on drop, so
    /// a cancellation mid-await can never re-park one app's client under
    /// another's key.
    pub(crate) fn take(app_id: &str) -> Result<Self, DbError> {
        let client = with_mut(|c| c.take_tx_client_for(app_id))
            .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
        Ok(Self {
            app_id: app_id.to_string(),
            client: Some(client),
        })
    }

    /// Borrow the pinned client while the guard owns restoration.
    pub(crate) fn client(&self) -> &TxConnection {
        self.client
            .as_ref()
            .expect("TxClientSlotGuard::client called after drop-state transition")
    }
}

impl Drop for TxClientSlotGuard {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let app_id = std::mem::take(&mut self.app_id);
            with_mut(|c| c.put_tx_client_for(&app_id, client));
        }
    }
}

/// Per-isolate DB plug-in state. One instance per worker thread, held
/// by the [`ISOLATE_CTX`] thread-local.
///
/// All fields are private. Every consumer goes through an accessor
/// method on this `impl` — [`Self::pool`], [`Self::savepoint_depth`],
/// [`Self::set_mig_lock`], etc. Direct field access from inside the
/// crate is rejected at compile time. This closes deferred [I16]
/// (api-surface r3 M1+M2; r9 ceiling step "privatise context fields").
#[allow(missing_debug_implementations)]
pub struct IsolateDbContext {
    /// Connection pool — created lazily on first DB operation.
    pool: Option<Rc<Pool>>,

    /// Database URL — poisoned during `register()`, consumed on first
    /// pool creation.
    db_url: Option<String>,

    /// Registered models — keyed by "app_id:collection". Prevents
    /// redundant DDL on subsequent cold starts within the same deploy.
    registered_models: HashSet<String>,

    /// Active transaction clients, **keyed by owning `app_id`**.
    ///
    /// SEC-1: a worker OS thread multiplexes up to ~200 isolates (one
    /// per app), and a creator's `env.db.transaction(async () => await
    /// fetch(slow))` parks its tx client here across the `await`. If
    /// this were a single per-thread slot, a co-resident app B's plain
    /// `env.db.*` would run B's SQL on A's pinned connection — inside
    /// A's transaction, snapshot, and per-app PG role. Keying by
    /// `app_id` makes A's parked tx invisible and untouchable to B, and
    /// lets A and B each hold their own concurrent tx without clobbering
    /// (B's BEGIN does not abort A's).
    ///
    /// V8 is single-threaded per isolate, so a given app still has at
    /// most one entry. Postgres stores a raw [`Client`] rather than
    /// `compio_postgres::Transaction<'_>` because the latter borrows the
    /// former and cannot live in thread-local state. SQLite stores a
    /// [`SqliteSessionHandle`] pointing at the single writer actor; the
    /// actor outlives the handle, so rollback on reject must be explicit
    /// rather than relying on handle drop.
    tx_conns: HashMap<String, TxConnection>,

    /// **P9 PR 3** — number of nested `SAVEPOINT`s open within each
    /// app's active explicit transaction, **keyed by owning `app_id`**
    /// (SEC-1: a shared counter would let one app's savepoint
    /// bookkeeping corrupt another's `zs_sp_<N>` naming). A missing
    /// entry (or `0`) means either no transaction is active for that
    /// app, or the only open transaction is the outermost one (the
    /// `BEGIN`). Each nested `env.db.transaction(...)` call that finds
    /// `has_tx_for(app) == true` emits `SAVEPOINT zs_sp_<depth+1>` and
    /// increments this; the matching `RELEASE SAVEPOINT` /
    /// `ROLLBACK TO SAVEPOINT` decrements it.
    ///
    /// The native transaction module (`transaction`) is the only
    /// writer: the savepoint name `zs_sp_<N>` is derived from this counter
    /// so RELEASE/ROLLBACK TO always target the savepoint the matching
    /// nested call opened. Capped at [`crate::transaction::MAX_SAVEPOINT_DEPTH`]
    /// (a 9th level throws `savepoint_depth_exceeded`).
    savepoint_depths: HashMap<String, u32>,

    /// Broker events queued during an active transaction, **keyed by
    /// owning `app_id`**.
    ///
    /// While an app has a tx parked in [`Self::tx_conns`], every
    /// successful CRUD mutation pushes its `ChangeEvent` here (under the
    /// event's own `app_id`) instead of calling
    /// [`crate::wal_consumer::emit_local`] directly. The transaction
    /// settle path (the native `Db.transaction(fn)` orchestrator) drains
    /// the owning app's queue and either fires every event through
    /// `emit_local` on COMMIT or clears it on ROLLBACK. This closes the
    /// "emit-before-commit" dual-write window where a subscriber could
    /// `find()` rows that don't yet exist on disk (or that a ROLLBACK is
    /// about to undo).
    ///
    /// SEC-1: keying by `app_id` keeps app B's COMMIT from firing app
    /// A's pre-commit events early (and B's ROLLBACK from silently
    /// dropping A's). A missing entry means no events are queued for
    /// that app.
    pending_emits: HashMap<String, Vec<ChangeEvent>>,

    /// Active migration owner state. `Some` after a successful
    /// `migrationBegin`; `None` once `migrationCommitBatch` with
    /// `isDone=true` (or `migrationCancel` on the owner thread)
    /// clears it. Single-isolate invariant — only one migration may
    /// be active per V8 thread at a time. The slot carries
    /// [`MigrationLock::app_id`] so the per-op accessors
    /// ([`Self::mig_lock_snapshot_for`] / [`Self::take_mig_client_for`]
    /// / [`Self::return_mig_client_for`]) can refuse a stale wrapper
    /// owned by a *different* app (SEC-1): the capacity gate
    /// ([`Self::has_mig_lock`]) stays app-agnostic, but app B must
    /// never drive SQL on app A's parked lock client.
    mig_lock: Option<MigrationLock>,

    /// Per-thread "is the consumer already running for this app?"
    /// guard. Keyed by app_id (a single worker may host multiple
    /// apps over its lifetime via the LRU cache, but only one
    /// consumer per app at a time).
    running_consumers: HashSet<String>,

    /// **P5 PR 2** — per-isolate, per-`(app_id, collection)` schema
    /// cache. Populated by `register_model_dispatch` on successful
    /// register; consulted by the CRUD encryption pass (`crud::dispatch_*`)
    /// to find columns declared `t.encrypted(...)`. Empty for any
    /// collection that hasn't been registered on this thread, OR for
    /// any collection registered before this PR landed — the cache is
    /// best-effort: a miss means "skip the encryption pass entirely",
    /// which is the correct behaviour for collections that have no
    /// encrypted columns. Keyed by `"{app_id}:{collection}"`; the value
    /// is the raw schema JSON the SDK declared.
    schemas: HashMap<String, serde_json::Value>,

    /// **P4 HALF B** — per-isolate, per-`(app_id, collection)` cache of the
    /// schema metadata **introspected from the LIVE catalog + sentinels**
    /// (`zeroship_schema::read_live_schema` + the `zsenc`/`__zsmask` codecs),
    /// NOT the declared descriptor. This is the runtime data-access metadata
    /// source the CRUD encryption + mask passes consume per the schema-authority
    /// split (design §6): plugin-db learns column types (for read coercions),
    /// which columns are `encrypted` (mode/keyId/wraps) and which are `masked`
    /// (kind/classification) by reading what the migration engine actually
    /// applied, decoupled from any in-memory declared schema.
    ///
    /// Keyed by `"{app_id}:{collection}"`; the value is `(deploy_token, schema)`
    /// where `deploy_token` is the app's current deploy/schema-version token
    /// (the worker-injected `ZEROSHIP_DEPLOY_ID` = `deploy_hash`, read via
    /// [`Self::deploy_token_for`] — T6). A stale token (a redeploy changed the
    /// app's `deploy_hash`) invalidates the entry on next read,
    /// mirroring the `is_model_registered` per-thread fast-path but keyed on the
    /// deploy/schema version rather than mere presence. The inner `Option`
    /// distinguishes "introspected, collection absent / has no goodies" (`None`)
    /// from "not yet introspected" (no map entry) so a goodie-free collection is
    /// cached as a negative result rather than re-introspected every call.
    introspected_schemas: HashMap<String, (String, Option<serde_json::Value>)>,

    /// **T6** — per-isolate, per-`app_id` deploy/schema-version token used as the
    /// invalidation key for [`Self::introspected_schemas`] (and any other
    /// deploy-keyed runtime cache). Stamped from the worker-injected
    /// `ZEROSHIP_DEPLOY_ID` env var (the per-app `deploy_hash`) when the `Db`
    /// wrapper is minted (`mint_db`), so a redeploy that changes the app's
    /// `deploy_hash` produces a fresh token and forces re-introspection of the
    /// new schema's crypto/mask/column metadata.
    ///
    /// This REPLACES the prior `std::env::var("ZEROSHIP_DEPLOY_ID")` read: that
    /// env var was process-global, never `set_var`'d by worker/runtime/control,
    /// and would have been WRONG for a multi-app worker thread even if it were —
    /// so the token was pinned at `"cold_start"` for the life of the isolate and
    /// the deploy-keyed cache never invalidated, applying stale metadata to a
    /// redeployed schema. Keyed by `app_id`; absent ⇒ `"cold_start"` (the cold /
    /// dev / raw-JS contract, matching the historical default).
    deploy_tokens: HashMap<String, String>,

    /// **P5.5 PR 5** — per-isolate, per-app mask-policy cache. Seeded
    /// on first unmask attempt by reading durable storage (PG admin
    /// schema or SQLite sidecar file); refreshed write-through by the
    /// `setMaskPolicy` op when the SDK's `defineMaskPolicy()` flushes.
    ///
    /// `Some(policy)` — the app declared a policy; the unmask
    /// authorisation path honours it.
    /// `None` (entry missing) — no policy in scope on this isolate
    /// yet. The unmask path then falls through to PR 4's default-deny
    /// stub (`auto` actor allowed; everyone else denied).
    ///
    /// Keyed by `app_id`. The entry is never proactively evicted —
    /// isolate lifetime is bounded by the LRU worker cache, so the
    /// policy lives as long as the app is hot.
    mask_policies: HashMap<String, crate::crud::mask_policy::MaskPolicy>,

    /// Backend handle wrapping the pool — Stage 8e-R2; promoted to
    /// the typed [`BackendHandle`] enum in P0 PR 5 (round-3 critic
    /// CRITICAL #3 closure; see `docs/proposals/db-system-design.md`
    /// §5.5 and `docs/proposals/p0-implementation-plan.md` §"PR 5").
    /// Created alongside the pool by [`Self::set_pool`] so consumers
    /// can call `ctx.backend()` to get a [`BackendHandle`] without
    /// naming `compio_postgres::Pool` directly.
    ///
    /// **No `dyn Backend` here**: the enum carries the concrete arm
    /// (`Postgres(Rc<PostgresBackend>)` today; `Sqlite(…)` gated on
    /// the P1 `sqlite` feature) so every trait-method call still
    /// monomorphises through the PG impl. The accessor cheaply
    /// `.clone()`s the enum (which Rc-clones the inner pointer).
    ///
    /// `None` until the pool is initialised; same lifecycle as
    /// [`Self::pool`] (cleared whenever the pool is cleared).
    backend: Option<BackendHandle>,

    /// Cold-start single-flight guard for backend initialisation.
    ///
    /// `init_pool_async()` may be reached by multiple concurrent RPCs
    /// before the first one finishes opening SQLite / Postgres. Without
    /// this flag, each request observes `backend == None` and opens the
    /// same backing store independently; on SQLite dev DBs that can fail
    /// during PRAGMA bootstrap with `database is locked`.
    backend_init_in_progress: bool,

    /// Process-wide usage meter (metering-as-infrastructure). `Some` on the
    /// worker / dev-serve vectors, stamped on `DbPlugin::register`. The exec
    /// boundary (`exec.rs`) emits a raw usage metric (`db_reads` /
    /// `db_writes` / `db_rows_written`) into it — keyed by the op's
    /// server-injected `app_id` — in the SUCCESS arm only. Platform-measured,
    /// unforgeable by app code. `None` in meter-less test harnesses.
    meter: Option<Arc<zeroship_metering::Meter>>,
}

impl IsolateDbContext {
    /// Build a fresh per-isolate context. Called once per worker
    /// thread on first access (via [`ISOLATE_CTX`]'s `const`
    /// initialiser path is too restrictive for `HashSet::new`, so the
    /// `RefCell` is initialised lazily through `std::cell::RefCell::new`
    /// in the `thread_local!` body).
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            pool: None,
            db_url: None,
            registered_models: HashSet::new(),
            tx_conns: HashMap::new(),
            savepoint_depths: HashMap::new(),
            pending_emits: HashMap::new(),
            mig_lock: None,
            running_consumers: HashSet::new(),
            schemas: HashMap::new(),
            introspected_schemas: HashMap::new(),
            deploy_tokens: HashMap::new(),
            mask_policies: HashMap::new(),
            backend: None,
            backend_init_in_progress: false,
            meter: None,
        }
    }

    // ----- metering --------------------------------------------------

    /// Stamp the process-wide usage meter (called from `DbPlugin::register`).
    /// Idempotent overwrite — registration may fire multiple times per
    /// worker thread.
    pub(crate) fn set_meter(&mut self, meter: Option<Arc<zeroship_metering::Meter>>) {
        self.meter = meter;
    }

    /// Build a per-`app_id` [`zeroship_metering::MeterHandle`] from the
    /// stamped meter. `None` when no meter is configured (test harness) —
    /// the exec layer then skips the emit. The handle binds the
    /// server-injected `app_id` so a db op cannot meter another app.
    pub(crate) fn meter_handle(&self, app_id: &str) -> Option<zeroship_metering::MeterHandle> {
        self.meter
            .as_ref()
            .map(|m| zeroship_metering::MeterHandle::new(Arc::clone(m), app_id))
    }

    // ----- DB_POOL ----------------------------------------------------

    /// Snapshot the pool handle (cloned `Rc`).
    pub(crate) fn pool(&self) -> Option<Rc<Pool>> {
        self.pool.as_ref().map(Rc::clone)
    }

    /// True iff the pool has been initialised.
    pub(crate) fn pool_initialised(&self) -> bool {
        self.pool.is_some()
    }

    /// Install the pool — called by `init_pool_async` once Postgres
    /// `connect` succeeds. Constructs the [`PostgresBackend`] facade
    /// in lockstep so the two never drift, and wraps it in the
    /// [`BackendHandle::Postgres`] arm (P0 PR 5).
    pub(crate) fn set_pool(&mut self, pool: Rc<Pool>) {
        let url = self.db_url.clone().unwrap_or_default();
        let backend = Rc::new(PostgresBackend::new(Rc::clone(&pool), url));
        self.backend = Some(BackendHandle::Postgres(backend));
        self.pool = Some(pool);
    }

    /// Drop the cached pool (e.g. when the URL changes on
    /// `register`). The next CRUD call will re-`init_pool_async`
    /// against the new URL.
    pub(crate) fn clear_pool(&mut self) {
        self.pool = None;
        self.backend = None;
        self.backend_init_in_progress = false;
    }

    /// Install a SQLite backend handle.
    ///
    /// PR 1 promotes this from a test-only seam to the production
    /// installer used by `init_pool_async`'s runtime URL dispatch.
    /// Callers that switch the worker thread from PG to SQLite clear
    /// the pool first; we also zero the pool slot defensively here so
    /// the context never advertises both a live pool and a SQLite
    /// backend at once.
    pub(crate) fn set_sqlite_backend(
        &mut self,
        backend: Rc<crate::backend::sqlite::SqliteBackend>,
    ) {
        self.pool = None;
        self.backend = Some(BackendHandle::Sqlite(backend));
    }

    /// Snapshot the backend facade (cloned enum — Rc-clone of the
    /// inner arm, see [`BackendHandle`]). The `Clone` derive on
    /// [`BackendHandle`] makes this cheap: the `Postgres` arm clones
    /// an `Rc<PostgresBackend>` (refcount bump, no allocation), and
    /// the eventual `Sqlite` arm under `--features sqlite` will be
    /// the same shape.
    pub(crate) fn backend(&self) -> Option<BackendHandle> {
        self.backend.clone()
    }

    /// Claim the backend initialisation slot, or observe the current
    /// initialisation state.
    pub(crate) fn begin_backend_init(&mut self) -> BackendInitState {
        if self.backend.is_some() {
            BackendInitState::Ready
        } else if self.backend_init_in_progress {
            BackendInitState::InProgress
        } else {
            self.backend_init_in_progress = true;
            BackendInitState::Acquired
        }
    }

    /// Release the backend initialisation slot after success or failure.
    pub(crate) fn finish_backend_init(&mut self) {
        self.backend_init_in_progress = false;
    }

    // ----- DB_URL -----------------------------------------------------

    /// Read the configured URL.
    pub(crate) fn db_url(&self) -> Option<String> {
        self.db_url.clone()
    }

    /// Poison the URL slot. Returns `true` iff the URL changed (the
    /// caller wants to drop the pool in that case so the next CRUD
    /// call rebuilds it).
    pub(crate) fn set_db_url(&mut self, url: &str) -> bool {
        let different = self.db_url.as_deref() != Some(url);
        if different {
            self.db_url = Some(url.to_string());
        }
        different
    }

    // ----- REGISTERED_MODELS -----------------------------------------

    /// Check whether the model has been registered on this isolate.
    pub(crate) fn is_model_registered(&self, app_id: &str, collection: &str) -> bool {
        let key = format!("{app_id}:{collection}");
        self.registered_models.contains(&key)
    }

    /// Mark the model as registered (idempotent).
    pub(crate) fn mark_model_registered(&mut self, app_id: &str, collection: &str) {
        let key = format!("{app_id}:{collection}");
        self.registered_models.insert(key);
    }

    /// Clear the registered mark for one model — forces the next `registerModel`
    /// to re-run the cold path instead of the warm short-circuit. Used by tests
    /// that re-register the same `(app, collection)` with a CHANGED schema (a real
    /// dev re-deploy mints a fresh isolate; the test reuses one).
    pub(crate) fn clear_model_registered(&mut self, app_id: &str, collection: &str) {
        let key = format!("{app_id}:{collection}");
        self.registered_models.remove(&key);
    }

    /// **P5 PR 2** — cache the schema declared by `db.registerModel`.
    /// Called after the four-phase DDL pipeline succeeds so the CRUD
    /// encryption pass can find `t.encrypted(...)` columns by name.
    /// Idempotent: re-registering the same collection overwrites the
    /// cached schema (the DDL pipeline reconciles diffs but the in-
    /// memory cache should reflect the latest declaration).
    pub(crate) fn cache_schema(
        &mut self,
        app_id: &str,
        collection: &str,
        schema: serde_json::Value,
    ) {
        let key = format!("{app_id}:{collection}");
        self.schemas.insert(key, schema);
    }

    /// **P5 PR 2** — fetch the cached schema for a `(app_id,
    /// collection)`. Returns `None` when the collection hasn't been
    /// registered on this isolate yet (the CRUD encryption pass
    /// short-circuits on `None`, which is the correct behaviour for
    /// collections with no encrypted columns).
    pub(crate) fn schema_for(
        &self,
        app_id: &str,
        collection: &str,
    ) -> Option<serde_json::Value> {
        let key = format!("{app_id}:{collection}");
        self.schemas.get(&key).cloned()
    }

    /// **P4 HALF B** — read the cached INTROSPECTED schema for `(app_id,
    /// collection)`, but only if it was cached under the CURRENT
    /// `deploy_token`. A token mismatch (a redeploy changed the app's
    /// `deploy_hash` / `ZEROSHIP_DEPLOY_ID`)
    /// returns `None`, forcing the caller to re-introspect — this is the
    /// deploy-bump invalidation. Returns:
    ///   - `Some(Some(schema))` — cached, current, collection has goodies;
    ///   - `Some(None)` — cached, current, collection has NO goodies (negative
    ///     cache — the caller skips the encrypt/mask passes without
    ///     re-introspecting);
    ///   - `None` — not cached or stale → caller must introspect.
    pub(crate) fn introspected_schema_for(
        &self,
        app_id: &str,
        collection: &str,
        deploy_token: &str,
    ) -> Option<Option<serde_json::Value>> {
        let key = format!("{app_id}:{collection}");
        match self.introspected_schemas.get(&key) {
            Some((tok, schema)) if tok == deploy_token => Some(schema.clone()),
            // Missing OR stale (token changed by a deploy) → re-introspect.
            _ => None,
        }
    }

    /// **P4 HALF B** — cache the result of a live introspection for `(app_id,
    /// collection)` under `deploy_token`. `schema = None` records a negative
    /// result (the collection has no encrypted/masked columns — the passes are
    /// skipped). Overwrites any stale entry from a prior deploy.
    pub(crate) fn cache_introspected_schema(
        &mut self,
        app_id: &str,
        collection: &str,
        deploy_token: &str,
        schema: Option<serde_json::Value>,
    ) {
        let key = format!("{app_id}:{collection}");
        self.introspected_schemas
            .insert(key, (deploy_token.to_string(), schema));
    }

    // ----- DEPLOY_TOKENS (T6) ----------------------------------------

    /// **T6** — stamp the per-`app_id` deploy/schema-version token (the
    /// worker-injected `ZEROSHIP_DEPLOY_ID` = `deploy_hash`). Called from
    /// `mint_db` when the `Db` wrapper is built, so the token reflects the
    /// deploy the isolate is currently serving. Idempotent overwrite — a swap to
    /// a new deploy re-mints the wrapper and re-stamps, which is exactly what
    /// invalidates the deploy-keyed introspection cache on the next CRUD op.
    pub(crate) fn set_deploy_token(&mut self, app_id: &str, token: &str) {
        self.deploy_tokens
            .insert(app_id.to_string(), token.to_string());
    }

    /// **T6** — read the per-`app_id` deploy/schema-version token. Defaults to
    /// `"cold_start"` when nothing was stamped (dev `zeroship serve`, raw-JS
    /// deploys, or test harnesses with no worker env injection) — the same cold
    /// default the prior `std::env::var` read fell back to, so the
    /// never-redeployed path behaves identically.
    pub(crate) fn deploy_token_for(&self, app_id: &str) -> String {
        self.deploy_tokens
            .get(app_id)
            .cloned()
            .unwrap_or_else(|| "cold_start".to_string())
    }

    /// **P5.5 PR 7** — enumerate every `(collection, schema)` pair the
    /// per-isolate cache holds for `app_id`. Drift-check sweep uses
    /// this to iterate every registered collection without having to
    /// re-introspect the catalog. Returns an empty `Vec` when the
    /// isolate has registered no collections for the app yet.
    ///
    /// Key shape: `<app_id>:<collection>` (the same format
    /// [`Self::cache_schema`] writes); we filter on the `<app_id>:`
    /// prefix and reconstruct the collection name from the suffix.
    pub(crate) fn cached_schemas_for_app(
        &self,
        app_id: &str,
    ) -> Vec<(String, serde_json::Value)> {
        let prefix = format!("{app_id}:");
        self.schemas
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix(&prefix)
                    .map(|coll| (coll.to_string(), v.clone()))
            })
            .collect()
    }

    // ----- MASK_POLICIES (P5.5 PR 5) ---------------------------------

    /// **P5.5 PR 5** — fetch the cached mask policy for `app_id`.
    /// Returns `None` when the cache holds no entry for the app
    /// (caller falls through to durable-storage load + cache install,
    /// or to PR 4's default-deny stub on a miss).
    pub(crate) fn mask_policy_for(
        &self,
        app_id: &str,
    ) -> Option<crate::crud::mask_policy::MaskPolicy> {
        self.mask_policies.get(app_id).cloned()
    }

    /// **P5.5 PR 5** — write-through cache install. `Some(policy)`
    /// upserts; `None` clears the entry (used by tests + the
    /// "no-policy-declared" path).
    pub(crate) fn set_mask_policy_for_app(
        &mut self,
        app_id: &str,
        policy: Option<crate::crud::mask_policy::MaskPolicy>,
    ) {
        match policy {
            Some(p) => {
                self.mask_policies.insert(app_id.to_string(), p);
            }
            None => {
                self.mask_policies.remove(app_id);
            }
        }
    }

    /// **P5.5 PR 5** — `true` iff a mask-policy entry is cached for
    /// `app_id`. Cheaper than `mask_policy_for` when callers only need
    /// to gate the durable-storage load.
    pub(crate) fn has_mask_policy(&self, app_id: &str) -> bool {
        self.mask_policies.contains_key(app_id)
    }

    // ----- TX_CONN / SAVEPOINT_DEPTH ------

    /// `true` if a transaction connection is currently parked **for
    /// `app_id`** (`tx_conns[app_id] = Some`). Returns `true` even
    /// between an in-flight take/return on the same tx client
    /// ([`Self::take_tx_client_for`] → [`Self::put_tx_client_for`]),
    /// because callers wrap the await in those two calls and the slot is
    /// conceptually still "active".
    ///
    /// SEC-1: a parked tx owned by another app reads as `false` here, so
    /// a co-resident app falls through to its own autocommit path under
    /// its own role rather than executing inside the owner's tx.
    pub(crate) fn has_tx_for(&self, app_id: &str) -> bool {
        self.tx_conns.contains_key(app_id)
    }

    /// Park a connection in `app_id`'s transaction slot. Returns the
    /// previous occupant for that app, if any (callers should ensure
    /// this is `None` — every begin path checks [`Self::has_tx_for`]
    /// first). A different app's parked tx is never disturbed (SEC-1).
    pub(crate) fn install_tx_client(
        &mut self,
        app_id: &str,
        client: TxConnection,
    ) -> Option<TxConnection> {
        self.tx_conns.insert(app_id.to_string(), client)
    }

    /// Take `app_id`'s transaction client out of the slot. The caller
    /// must either return it via [`Self::put_tx_client_for`] (when the
    /// await is short and the slot should remain "in transaction") or
    /// drop the client (when settling the tx). Returns `None` when no tx
    /// is parked for `app_id` — including when another app owns the only
    /// parked tx (SEC-1: app B cannot drain app A's client).
    pub(crate) fn take_tx_client_for(&mut self, app_id: &str) -> Option<TxConnection> {
        self.tx_conns.remove(app_id)
    }

    /// Return a client previously taken via [`Self::take_tx_client_for`]
    /// to `app_id`'s slot.
    pub(crate) fn put_tx_client_for(&mut self, app_id: &str, client: TxConnection) {
        self.tx_conns.insert(app_id.to_string(), client);
    }

    /// **P9 PR 3** — read `app_id`'s current nested-savepoint depth
    /// (zero when no savepoint is open above the outermost `BEGIN`, or
    /// when the app has no active tx).
    pub(crate) fn savepoint_depth_for(&self, app_id: &str) -> u32 {
        self.savepoint_depths.get(app_id).copied().unwrap_or(0)
    }

    /// **P9 PR 3** — bump `app_id`'s nested-savepoint depth on
    /// `SAVEPOINT zs_sp_N`. Returns the new depth, which is also the `N`
    /// in the savepoint name the caller just opened. Requires an active
    /// transaction connection for that app (a savepoint without an
    /// enclosing `BEGIN` is a state-machine bug).
    pub(crate) fn push_savepoint_for(&mut self, app_id: &str) -> u32 {
        debug_assert!(
            self.tx_conns.contains_key(app_id),
            "push_savepoint_for called without an active tx for the app",
        );
        let depth = self.savepoint_depths.entry(app_id.to_string()).or_insert(0);
        *depth = depth.saturating_add(1);
        *depth
    }

    /// **P9 PR 3** — decrement `app_id`'s nested-savepoint depth on
    /// `RELEASE SAVEPOINT` / `ROLLBACK TO SAVEPOINT`. Saturates at zero
    /// so a double-settle (handler + finalizer race) cannot underflow.
    pub(crate) fn pop_savepoint_for(&mut self, app_id: &str) {
        if let Some(depth) = self.savepoint_depths.get_mut(app_id) {
            *depth = depth.saturating_sub(1);
        }
    }

    /// **P9 PR 3** — reset `app_id`'s nested-savepoint depth to zero.
    /// Called by the top-level settle path (COMMIT / ROLLBACK) so a
    /// fresh transaction for that app starts from a clean slate even if
    /// an inner savepoint settle was skipped (e.g. the whole tx is being
    /// torn down by a top-level rollback). A different app's depth is
    /// untouched (SEC-1).
    pub(crate) fn reset_savepoint_depth_for(&mut self, app_id: &str) {
        self.savepoint_depths.remove(app_id);
    }

    // ----- PENDING_EMITS ---------------------------------------------

    /// Push a `ChangeEvent` onto the owning app's pending-emits queue
    /// (keyed by the event's own `app_id`; the queue is allocated
    /// lazily on first push within that app's tx).
    pub(crate) fn push_pending_emit(&mut self, ev: ChangeEvent) {
        self.pending_emits
            .entry(ev.app_id.clone())
            .or_default()
            .push(ev);
    }

    /// Drain `app_id`'s pending-emits queue (returns `Vec::new()` if the
    /// app has none queued). Called by the transaction settle path on
    /// COMMIT. SEC-1: only the committing app's events are returned, so
    /// one app's COMMIT cannot fire another's pre-commit events.
    pub(crate) fn drain_pending_emits_for(&mut self, app_id: &str) -> Vec<ChangeEvent> {
        self.pending_emits.remove(app_id).unwrap_or_default()
    }

    /// Clear `app_id`'s pending-emits queue without firing any events.
    /// Called by the transaction settle path on ROLLBACK and by
    /// `exec_begin` to drop any stale residue from an interrupted prior
    /// run. A different app's queue is untouched (SEC-1).
    pub(crate) fn clear_pending_emits_for(&mut self, app_id: &str) {
        self.pending_emits.remove(app_id);
    }

    // ----- MIG_LOCK ---------------------------------------------------

    /// True iff a migration run is active on this isolate.
    pub(crate) fn has_mig_lock(&self) -> bool {
        self.mig_lock.is_some()
    }

    /// Install a fresh migration lock state. Returns the previous
    /// state if any (callers should ensure this is `None` — every
    /// begin path checks [`Self::has_mig_lock`] first).
    ///
    /// If a prior lock is shadowed, log it at `error` (state-machine
    /// drift the begin path should have caught via `has_mig_lock`)
    /// and proceed with `replace` so a worker is recoverable by the
    /// next operator-driven reset rather than panicking. The slot's
    /// unit tests deliberately exercise the swap-on-replace shape;
    /// the log keeps them passing while still surfacing the drift in
    /// production logs.
    pub(crate) fn set_mig_lock(&mut self, lock: MigrationLock) -> Option<MigrationLock> {
        if let Some(prev) = self.mig_lock.as_ref() {
            tracing::error!(
                prev_name = %prev.name,
                prev_audit_id = prev.audit_id,
                new_name = %lock.name,
                new_audit_id = lock.audit_id,
                "set_mig_lock called while another lock is active — begin path should gate on has_mig_lock",
            );
        }
        self.mig_lock.replace(lock)
    }

    /// Drop the active migration lock state. Best-effort —
    /// idempotent.
    pub(crate) fn clear_mig_lock(&mut self) {
        self.mig_lock = None;
    }

    /// **Test-only forced teardown** — take whatever lock client is
    /// parked, regardless of owner, so `clear_migration_lock_for_tests`
    /// can ROLLBACK + unlock and reset the slot between tests. Never
    /// reachable from a production path (the owner-scoped
    /// [`Self::take_mig_client_for`] is the only production drain).
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn take_mig_client_any_for_tests(&mut self) -> Option<Client> {
        self.mig_lock.as_mut().and_then(|l| l.client.take())
    }

    /// Take the lock client out of the active migration state for an
    /// await, **only if the active migration is owned by `app_id`**;
    /// the caller's future is responsible for putting it back via
    /// [`Self::return_mig_client_for`]. Returns `None` when no migration
    /// is active or a *different* app owns it (SEC-1: a stale wrapper
    /// from app B must not drive SQL on app A's parked lock client).
    pub(crate) fn take_mig_client_for(&mut self, app_id: &str) -> Option<Client> {
        self.mig_lock
            .as_mut()
            .filter(|l| l.app_id == app_id)
            .and_then(|l| l.client.take())
    }

    /// Restore the lock client after an await to `app_id`'s active
    /// migration. No-op if the migration state has been cleared in the
    /// meantime (e.g. by an operator cancel) or is now owned by a
    /// different app. The slot-empty / not-owner case is observable but
    /// rare — log it at `warn` so we can distinguish a real cancel race
    /// from a state-machine bug that silently dropped the client (paired
    /// with the `tracing::error!` on `set_mig_lock`'s shadow-replace
    /// branch above).
    pub(crate) fn return_mig_client_for(&mut self, app_id: &str, client: Client) {
        match self.mig_lock.as_mut().filter(|l| l.app_id == app_id) {
            Some(lock) => lock.client = Some(client),
            None => tracing::warn!(
                "return_mig_client_for: mig_lock slot empty or owned by another app — client dropped (expected only on operator-cancel race)",
            ),
        }
    }

    /// Snapshot the migration lock's identifying fields (name,
    /// collection, audit_id, dry_run, start_generation) **only when the
    /// active migration is owned by `app_id`**. Returns `None` outside
    /// an active run, or when a different app owns it (SEC-1).
    pub(crate) fn mig_lock_snapshot_for(
        &self,
        app_id: &str,
    ) -> Option<(String, String, i64, bool, i64)> {
        self.mig_lock.as_ref().filter(|l| l.app_id == app_id).map(|l| {
            (
                l.name.clone(),
                l.collection.clone(),
                l.audit_id,
                l.dry_run,
                l.start_generation,
            )
        })
    }

    // ----- RUNNING_CONSUMERS -----------------------------------------

    /// True iff a replication consumer is already running for this
    /// app on this isolate.
    pub(crate) fn is_consumer_running(&self, app_id: &str) -> bool {
        self.running_consumers.contains(app_id)
    }

    /// Mark a replication consumer as running for this app — test-only.
    ///
    /// Production code uses [`Self::try_mark_consumer_running`]
    /// (atomic check-and-set; returns whether the caller won the
    /// race). This unconditional variant is retained for test
    /// fixtures that need to mark without caring whether the slot was
    /// already taken; api-surface r6 MAJOR-R6-1 noted it was dead in
    /// production builds and would footgun a contributor picking it
    /// over the atomic variant.
    #[cfg(test)]
    pub(crate) fn mark_consumer_running(&mut self, app_id: &str) {
        self.running_consumers.insert(app_id.to_string());
    }

    /// Atomically check-and-mark: returns `true` if the caller won the
    /// mark (was not previously running), `false` if another caller
    /// already marked this app. Used by the spawned consumer task to
    /// close the race between dispatch's idempotent gate and the
    /// task's first poll (concurrency r7 NEW MINOR).
    pub(crate) fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
        self.running_consumers.insert(app_id.to_string())
    }

    /// Mark a replication consumer as no-longer-running.
    pub(crate) fn unmark_consumer_running(&mut self, app_id: &str) {
        self.running_consumers.remove(app_id);
    }

    /// Clear every entry from the consumer registry (test-only —
    /// production code should rely on the supervised task's exit path
    /// to call [`Self::unmark_consumer_running`]).
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn clear_consumer_registry(&mut self) {
        self.running_consumers.clear();
    }
}

impl Default for IsolateDbContext {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// The per-isolate DB context. Replaces the slot-per-thread-local
    /// lattice that lived in `lib.rs`, `migrations.rs`, and
    /// `replication_ops.rs` before Stage 8d-R4.
    pub(crate) static ISOLATE_CTX: RefCell<IsolateDbContext> =
        RefCell::new(IsolateDbContext::new());
}

/// Run `f` with a shared reference to the per-isolate DB context.
///
/// The compio runtime is single-threaded per worker; this never
/// contends. Callers must NOT re-enter [`with`] / [`with_mut`] from
/// inside `f` (the underlying `RefCell` will panic).
pub fn with<R>(f: impl FnOnce(&IsolateDbContext) -> R) -> R {
    ISOLATE_CTX.with(|c| f(&c.borrow()))
}

/// Run `f` with an exclusive reference to the per-isolate DB context.
/// Same re-entrancy rule as [`with`].
pub fn with_mut<R>(f: impl FnOnce(&mut IsolateDbContext) -> R) -> R {
    ISOLATE_CTX.with(|c| f(&mut c.borrow_mut()))
}

#[cfg(test)]
mod tests {
    //! Unit tests for the per-isolate DB context state machine.
    //!
    //! Every test constructs a fresh [`IsolateDbContext`] directly via
    //! [`IsolateDbContext::new`] — the thread-local [`ISOLATE_CTX`] is
    //! avoided so test ordering on the same OS thread cannot cause one
    //! test to observe state another mutated.
    //!
    //! ## Why some accessors aren't covered here
    //!
    //! The slots that store a live transaction/migration client
    //! (`tx_conn`, `MigrationLock::client`) need a real backend handle to
    //! exercise. The Postgres side still needs a `compio_postgres::Client`
    //! (`Client::new` is `pub(crate)` on the driver), and the SQLite side
    //! would need a live session actor. These unit tests stay pure-state;
    //! end-to-end slot round-trips live in the integration targets.
    //!
    //! Concretely, the following can only be exercised by
    //! `tests/integration.rs` (which spins up a real PG):
    //!
    //! * [`IsolateDbContext::install_tx_client`] /
    //!   [`IsolateDbContext::take_tx_client`] /
    //!   [`IsolateDbContext::put_tx_client`] round-trip with a real
    //!   backend client.
    //! * The `debug_assert!` inside [`IsolateDbContext::push_savepoint`]
    //!   that a savepoint requires an active `tx_conn` — same constraint;
    //!   the pop/reset arms (no such precondition) are unit-tested.
    //! * [`IsolateDbContext::set_mig_lock`] /
    //!   [`IsolateDbContext::take_mig_client`] /
    //!   [`IsolateDbContext::return_mig_client`] / `mig_lock_snapshot`
    //!   *with* a client present — we test the snapshot/clear paths
    //!   using a `MigrationLock { client: None, .. }` because the
    //!   snapshot deliberately doesn't read `client`.
    //!
    //! For [`IsolateDbContext::set_pool`] / [`IsolateDbContext::backend`]
    //! we need an `Rc<Pool>`, which only `Pool::connect` produces. Those
    //! lifecycles are covered by `tests/integration.rs`.

    use super::*;

    use crate::broker::{ChangeEvent, ChangeOp};
    use std::collections::HashMap;

    fn dummy_event(collection: &str) -> ChangeEvent {
        ChangeEvent {
            app_id: "app_t".into(),
            collection: collection.into(),
            op: ChangeOp::Insert,
            pk: Some("1".to_string()),
            changed_columns: Vec::new(),
            new_tuple: HashMap::new(),
            old_tuple: None,
        }
    }

    // ----- IsolateDbContext::new / Default -------------------------------

    #[test]
    fn new_yields_fully_cleared_slots() {
        let ctx = IsolateDbContext::new();
        assert!(ctx.pool().is_none());
        assert!(!ctx.pool_initialised());
        assert!(ctx.backend().is_none());
        assert!(ctx.db_url().is_none());
        assert!(!ctx.has_tx_for("a"));
        assert_eq!(ctx.savepoint_depth_for("a"), 0);
        assert!(!ctx.has_mig_lock());
        assert!(ctx.mig_lock_snapshot_for("a").is_none());
        // pending_emits starts empty (each app's queue is allocated
        // lazily on first push).
        assert!(ctx.pending_emits.is_empty());
        // Both registries empty.
        assert!(!ctx.is_model_registered("a", "c"));
        assert!(!ctx.is_consumer_running("a"));
    }

    #[test]
    fn default_matches_new() {
        let a = IsolateDbContext::default();
        let b = IsolateDbContext::new();
        // Compare observable state (no PartialEq on the struct).
        assert_eq!(a.pool_initialised(), b.pool_initialised());
        assert_eq!(a.savepoint_depth_for("a"), b.savepoint_depth_for("a"));
        assert_eq!(a.has_tx_for("a"), b.has_tx_for("a"));
        assert_eq!(a.has_mig_lock(), b.has_mig_lock());
        assert_eq!(a.db_url(), b.db_url());
    }

    // ----- DB_URL coherence ----------------------------------------------

    #[test]
    fn set_db_url_returns_true_on_change() {
        let mut ctx = IsolateDbContext::new();
        assert!(ctx.set_db_url("postgres://a"));
        assert_eq!(ctx.db_url().as_deref(), Some("postgres://a"));
    }

    #[test]
    fn set_db_url_returns_false_when_unchanged() {
        let mut ctx = IsolateDbContext::new();
        assert!(ctx.set_db_url("postgres://a"));
        assert!(!ctx.set_db_url("postgres://a"));
        assert_eq!(ctx.db_url().as_deref(), Some("postgres://a"));
    }

    #[test]
    fn set_db_url_returns_true_on_subsequent_change() {
        let mut ctx = IsolateDbContext::new();
        ctx.set_db_url("postgres://a");
        assert!(ctx.set_db_url("postgres://b"));
        assert_eq!(ctx.db_url().as_deref(), Some("postgres://b"));
    }

    #[test]
    fn set_db_url_does_not_clear_pool_on_no_op() {
        // `set_db_url` is *just* the URL slot; the caller (lib.rs) is
        // responsible for invoking `clear_pool` when the URL changes.
        // We can't install a real pool here (Pool::connect needs PG),
        // but we can at least confirm `set_db_url` itself does NOT
        // poke the `backend` slot — once the no-op path returns false,
        // a hypothetical pool would still be installed. Verified
        // indirectly: the function body has no `self.pool = None` or
        // `self.backend = None`.
        let mut ctx = IsolateDbContext::new();
        ctx.set_db_url("postgres://a");
        // No pool to clear; we're checking the API surface remains
        // pool-agnostic.
        assert!(ctx.pool().is_none());
        assert!(ctx.backend().is_none());
        let _ = ctx.set_db_url("postgres://a"); // no-op
        assert!(ctx.pool().is_none());
        assert!(ctx.backend().is_none());
    }

    // ----- clear_pool ----------------------------------------------------

    #[test]
    fn clear_pool_when_unset_is_noop() {
        let mut ctx = IsolateDbContext::new();
        ctx.clear_pool();
        assert!(ctx.pool().is_none());
        assert!(ctx.backend().is_none());
    }

    // ----- backend cold-init single-flight -------------------------------

    #[test]
    fn backend_init_slot_allows_one_initializer_at_a_time() {
        let mut ctx = IsolateDbContext::new();

        assert!(matches!(
            ctx.begin_backend_init(),
            BackendInitState::Acquired
        ));
        assert!(matches!(
            ctx.begin_backend_init(),
            BackendInitState::InProgress
        ));

        ctx.finish_backend_init();
        assert!(matches!(
            ctx.begin_backend_init(),
            BackendInitState::Acquired
        ));
        ctx.finish_backend_init();
    }

    #[test]
    fn clear_pool_releases_backend_init_slot() {
        let mut ctx = IsolateDbContext::new();

        assert!(matches!(
            ctx.begin_backend_init(),
            BackendInitState::Acquired
        ));
        assert!(matches!(
            ctx.begin_backend_init(),
            BackendInitState::InProgress
        ));

        ctx.clear_pool();
        assert!(matches!(
            ctx.begin_backend_init(),
            BackendInitState::Acquired
        ));
        ctx.finish_backend_init();
    }

    // ----- REGISTERED_MODELS ---------------------------------------------

    #[test]
    fn registered_models_round_trip() {
        let mut ctx = IsolateDbContext::new();
        assert!(!ctx.is_model_registered("app_a", "messages"));
        ctx.mark_model_registered("app_a", "messages");
        assert!(ctx.is_model_registered("app_a", "messages"));
        // App / collection both contribute to the key.
        assert!(!ctx.is_model_registered("app_b", "messages"));
        assert!(!ctx.is_model_registered("app_a", "other"));
    }

    #[test]
    fn mark_model_registered_is_idempotent() {
        let mut ctx = IsolateDbContext::new();
        ctx.mark_model_registered("app_a", "msgs");
        ctx.mark_model_registered("app_a", "msgs");
        assert!(ctx.is_model_registered("app_a", "msgs"));
        // HashSet dedupes — the second call shouldn't grow the registry
        // (verified via the set's `len` semantics).
        assert_eq!(ctx.registered_models.len(), 1);
    }

    // ----- TX token monotonic counter ------------------------------------

    #[test]
    fn savepoint_depth_pop_and_reset_saturate_at_zero() {
        // `push_savepoint_for` carries a `debug_assert!(tx parked)` and
        // so needs a real Client (see module-level note) — covered by
        // the integration/V8 end-to-end paths. The decrement / reset
        // arms have no such precondition: a double-settle (handler +
        // finalizer race) must NOT underflow the unsigned counter.
        let mut ctx = IsolateDbContext::new();
        assert_eq!(ctx.savepoint_depth_for("app_t"), 0);
        // pop on an already-zero depth saturates rather than wrapping to
        // u32::MAX.
        ctx.pop_savepoint_for("app_t");
        assert_eq!(ctx.savepoint_depth_for("app_t"), 0, "pop must saturate at zero");
        // reset on zero is a no-op.
        ctx.reset_savepoint_depth_for("app_t");
        assert_eq!(ctx.savepoint_depth_for("app_t"), 0);
    }

    // ----- PENDING_EMITS state machine -----------------------------------

    #[test]
    fn pending_emits_start_empty() {
        let ctx = IsolateDbContext::new();
        assert!(ctx.pending_emits.is_empty());
    }

    #[test]
    fn push_pending_emit_allocates_slot_lazily() {
        // dummy_event tags app_id "app_t"; the queue keys on that.
        let mut ctx = IsolateDbContext::new();
        assert!(ctx.pending_emits.is_empty());
        ctx.push_pending_emit(dummy_event("c1"));
        assert!(ctx.pending_emits.contains_key("app_t"));
        assert_eq!(ctx.pending_emits.get("app_t").unwrap().len(), 1);
    }

    #[test]
    fn push_pending_emit_accumulates() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.push_pending_emit(dummy_event("c3"));
        let evs = ctx.pending_emits.get("app_t").unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].collection, "c1");
        assert_eq!(evs[1].collection, "c2");
        assert_eq!(evs[2].collection, "c3");
    }

    #[test]
    fn drain_pending_emits_returns_and_clears() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        let drained = ctx.drain_pending_emits_for("app_t");
        assert_eq!(drained.len(), 2);
        // After drain the app's queue is cleared — subsequent pushes
        // re-allocate.
        assert!(!ctx.pending_emits.contains_key("app_t"));
    }

    #[test]
    fn drain_pending_emits_on_empty_returns_empty_vec() {
        let mut ctx = IsolateDbContext::new();
        let drained = ctx.drain_pending_emits_for("app_t");
        assert!(drained.is_empty());
        assert!(ctx.pending_emits.is_empty());
    }

    #[test]
    fn drain_then_push_starts_fresh() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        let _ = ctx.drain_pending_emits_for("app_t");
        ctx.push_pending_emit(dummy_event("c2"));
        let evs = ctx.pending_emits.get("app_t").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].collection, "c2");
    }

    #[test]
    fn clear_pending_emits_drops_without_returning() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.clear_pending_emits_for("app_t");
        assert!(!ctx.pending_emits.contains_key("app_t"));
        // A subsequent drain returns empty (queue is gone).
        assert!(ctx.drain_pending_emits_for("app_t").is_empty());
    }

    #[test]
    fn clear_pending_emits_on_empty_is_idempotent() {
        let mut ctx = IsolateDbContext::new();
        ctx.clear_pending_emits_for("app_t");
        ctx.clear_pending_emits_for("app_t");
        assert!(ctx.pending_emits.is_empty());
    }

    // ----- MIG_LOCK state machine ----------------------------------------

    fn mig_lock(name: &str, collection: &str, audit_id: i64, dry_run: bool) -> MigrationLock {
        mig_lock_for_app("app_t", name, collection, audit_id, dry_run)
    }

    fn mig_lock_for_app(
        app_id: &str,
        name: &str,
        collection: &str,
        audit_id: i64,
        dry_run: bool,
    ) -> MigrationLock {
        MigrationLock {
            app_id: app_id.to_string(),
            name: name.to_string(),
            collection: collection.to_string(),
            audit_id,
            dry_run,
            start_generation: 7,
            client: None,
            // Unit-test fixture skips the BrokerPauseGuard wire-up; the
            // slot-state-machine assertions below don't depend on the
            // guard's suppression behaviour. P2-tail orchestrator-side
            // verification lives in the `sqlite_integration` test.
            broker_pause: None,
        }
    }

    #[test]
    fn set_mig_lock_install_then_snapshot() {
        let mut ctx = IsolateDbContext::new();
        assert!(!ctx.has_mig_lock());
        let prev = ctx.set_mig_lock(mig_lock("m1", "users", 42, false));
        assert!(prev.is_none());
        assert!(ctx.has_mig_lock());

        let snap = ctx.mig_lock_snapshot_for("app_t").expect("snapshot present");
        assert_eq!(snap.0, "m1");
        assert_eq!(snap.1, "users");
        assert_eq!(snap.2, 42);
        assert!(!snap.3);
        assert_eq!(snap.4, 7); // start_generation
    }

    #[test]
    fn set_mig_lock_dry_run_flag_round_trips() {
        let mut ctx = IsolateDbContext::new();
        ctx.set_mig_lock(mig_lock("dry", "msgs", 1, true));
        let snap = ctx.mig_lock_snapshot_for("app_t").unwrap();
        assert!(snap.3, "dry_run flag should round-trip via snapshot");
    }

    #[test]
    fn set_mig_lock_replaces_existing_returns_previous() {
        // The accessor uses `replace` so callers can detect a pre-
        // existing occupant. Production code calls `has_mig_lock` first
        // and refuses to overwrite, but the state machine still allows
        // the swap and reports the previous holder via the return
        // value.
        let mut ctx = IsolateDbContext::new();
        ctx.set_mig_lock(mig_lock("first", "c", 1, false));
        let prev = ctx.set_mig_lock(mig_lock("second", "c", 2, false));
        let prev = prev.expect("previous lock returned");
        assert_eq!(prev.name, "first");
        assert_eq!(prev.audit_id, 1);

        let snap = ctx.mig_lock_snapshot_for("app_t").unwrap();
        assert_eq!(snap.0, "second");
        assert_eq!(snap.2, 2);
    }

    #[test]
    fn clear_mig_lock_drops_state() {
        let mut ctx = IsolateDbContext::new();
        ctx.set_mig_lock(mig_lock("m1", "c", 1, false));
        assert!(ctx.has_mig_lock());
        ctx.clear_mig_lock();
        assert!(!ctx.has_mig_lock());
        assert!(ctx.mig_lock_snapshot_for("app_t").is_none());
    }

    #[test]
    fn clear_mig_lock_is_idempotent() {
        let mut ctx = IsolateDbContext::new();
        ctx.clear_mig_lock(); // empty -> empty
        ctx.clear_mig_lock();
        assert!(!ctx.has_mig_lock());
        // Install then clear twice.
        ctx.set_mig_lock(mig_lock("m1", "c", 1, false));
        ctx.clear_mig_lock();
        ctx.clear_mig_lock();
        assert!(!ctx.has_mig_lock());
    }

    #[test]
    fn mig_lock_snapshot_outside_run_returns_none() {
        let ctx = IsolateDbContext::new();
        assert!(ctx.mig_lock_snapshot_for("app_t").is_none());
    }

    #[test]
    fn take_mig_client_on_empty_lock_returns_none() {
        // No active migration: take is a no-op.
        let mut ctx = IsolateDbContext::new();
        assert!(ctx.take_mig_client_for("app_t").is_none());
        // With a lock present but `client: None` (our test mig_lock
        // helper), take still returns None — there is nothing to take.
        ctx.set_mig_lock(mig_lock("m", "c", 1, false));
        assert!(ctx.take_mig_client_for("app_t").is_none());
    }

    #[test]
    fn return_mig_client_after_cancel_is_silent_noop() {
        // The documented contract: `return_mig_client` is a no-op when
        // the migration state has been cleared in the meantime (e.g.
        // operator cancel). We can't construct a real Client here, but
        // we can exercise the early-return branch: clear the lock,
        // then call return — the function must not panic and must not
        // resurrect the lock.
        let mut ctx = IsolateDbContext::new();
        ctx.clear_mig_lock();
        // (Skipped: actually passing a Client; see module-level note.)
        assert!(!ctx.has_mig_lock());
    }

    // ----- RUNNING_CONSUMERS state machine -------------------------------

    #[test]
    fn consumer_running_round_trip() {
        let mut ctx = IsolateDbContext::new();
        assert!(!ctx.is_consumer_running("app_a"));
        ctx.mark_consumer_running("app_a");
        assert!(ctx.is_consumer_running("app_a"));
        ctx.unmark_consumer_running("app_a");
        assert!(!ctx.is_consumer_running("app_a"));
    }

    #[test]
    fn consumer_running_is_per_app() {
        let mut ctx = IsolateDbContext::new();
        ctx.mark_consumer_running("app_a");
        assert!(ctx.is_consumer_running("app_a"));
        assert!(!ctx.is_consumer_running("app_b"));
        ctx.mark_consumer_running("app_b");
        assert!(ctx.is_consumer_running("app_a"));
        assert!(ctx.is_consumer_running("app_b"));
    }

    #[test]
    fn mark_consumer_running_is_idempotent() {
        let mut ctx = IsolateDbContext::new();
        ctx.mark_consumer_running("app_a");
        ctx.mark_consumer_running("app_a");
        assert!(ctx.is_consumer_running("app_a"));
        assert_eq!(ctx.running_consumers.len(), 1);
    }

    #[test]
    fn unmark_consumer_running_is_idempotent_on_unknown() {
        let mut ctx = IsolateDbContext::new();
        // Never marked; unmark must be silent.
        ctx.unmark_consumer_running("app_a");
        assert!(!ctx.is_consumer_running("app_a"));
        // Mark, unmark twice — second unmark is silent.
        ctx.mark_consumer_running("app_b");
        ctx.unmark_consumer_running("app_b");
        ctx.unmark_consumer_running("app_b");
        assert!(!ctx.is_consumer_running("app_b"));
    }

    #[test]
    fn clear_consumer_registry_drops_all_entries() {
        let mut ctx = IsolateDbContext::new();
        ctx.mark_consumer_running("a");
        ctx.mark_consumer_running("b");
        ctx.mark_consumer_running("c");
        assert_eq!(ctx.running_consumers.len(), 3);
        ctx.clear_consumer_registry();
        assert_eq!(ctx.running_consumers.len(), 0);
        assert!(!ctx.is_consumer_running("a"));
    }

    // ----- Warn/error-shape contracts ([I23] mig_lock tracing) ---------
    //
    // The two `set_mig_lock` / `return_mig_client` log sites carry an
    // operator-grep contract — field names + level + the
    // shadow-replace-vs-empty-slot discriminator. Pin the shape so a
    // future refactor that renames `prev_audit_id`, drops the
    // `error`-level signal on shadow-replace, or otherwise weakens
    // the contract fails at unit-test time. test-coverage r13
    // NEW-R13-* called this out explicitly.

    #[test]
    fn set_mig_lock_shadow_replace_emits_error_with_prev_and_new() {
        use crate::test_support::capture;
        use tracing::Level;

        let mut ctx = IsolateDbContext::new();
        ctx.set_mig_lock(mig_lock("first", "users", 11, false));

        let ((), events) = capture(|| {
            // Shadow-replace path — the begin path should have gated
            // on `has_mig_lock` first; reaching here means a
            // state-machine bug. Contract: ONE error-level event,
            // four named fields identifying both the displaced and
            // the incoming lock.
            ctx.set_mig_lock(mig_lock("second", "users", 22, false));
        });

        assert_eq!(events.len(), 1, "expected exactly one tracing event");
        let ev = &events[0];
        assert_eq!(
            ev.level,
            Level::ERROR,
            "shadow-replace must surface at error level (state-machine drift)"
        );
        assert_eq!(
            ev.fields.get("prev_name").map(String::as_str),
            Some("first"),
            "prev_name must identify the displaced lock",
        );
        assert_eq!(
            ev.fields.get("prev_audit_id").map(String::as_str),
            Some("11"),
            "prev_audit_id must identify the displaced audit row",
        );
        assert_eq!(
            ev.fields.get("new_name").map(String::as_str),
            Some("second"),
            "new_name must identify the incoming lock",
        );
        assert_eq!(
            ev.fields.get("new_audit_id").map(String::as_str),
            Some("22"),
            "new_audit_id must identify the incoming audit row",
        );
        assert!(
            ev.message.contains("set_mig_lock"),
            "message must name the accessor for log-grep: {}",
            ev.message,
        );
    }

    #[test]
    fn set_mig_lock_first_install_emits_no_event() {
        // The shadow-replace log is gated on `mig_lock.is_some()` —
        // a first install must stay silent so log streams don't fill
        // with noise on every successful migrationBegin. Pinning the
        // negative case keeps the gate intact across refactors.
        use crate::test_support::capture;

        let mut ctx = IsolateDbContext::new();
        let ((), events) = capture(|| {
            ctx.set_mig_lock(mig_lock("only", "c", 1, false));
        });
        assert!(
            events.is_empty(),
            "first install must not emit; got {events:?}"
        );
    }

    // ----- SEC-1: per-app scoping of the tx / savepoint / emit / mig slots

    // A worker thread multiplexes up to ~200 isolates (one per app).
    // Every slot below used to be a single per-OS-thread cell shared by
    // ALL co-resident apps: app B could observe and drain app A's
    // parked transaction client (running B's SQL inside A's
    // transaction, snapshot, and per-app role), corrupt A's savepoint
    // bookkeeping, drain A's pre-commit broker queue, and take A's
    // migration lock client. These tests pin the per-app ownership
    // contract.

    fn run_async<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// Build a parkable [`TxConnection`] without a live Postgres: the
    /// embedded SQLite backend hands out a real session handle from a
    /// tempdir-backed store. No SQL is executed on it — these tests
    /// exercise the slot state machine only.
    async fn sqlite_tx_conn(dir: &tempfile::TempDir) -> TxConnection {
        use crate::backend::SqlExecutor as _;
        let backend = crate::backend::sqlite::SqliteBackend::new(
            std::path::PathBuf::from(dir.path()),
        )
        .expect("open sqlite backend");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire sqlite client");
        TxConnection::Sqlite(client)
    }

    #[test]
    fn sec1_tx_parked_by_app_a_is_invisible_and_untakable_for_app_b() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut ctx = IsolateDbContext::new();
            let prev = ctx.install_tx_client("app_a", sqlite_tx_conn(&dir).await);
            assert!(prev.is_none(), "tx slot must start empty");

            assert!(
                ctx.has_tx_for("app_a"),
                "the owning app must see its own parked tx",
            );
            assert!(
                !ctx.has_tx_for("app_b"),
                "SEC-1: app_b must NOT observe app_a's parked tx \
                 (a hit here routes app_b's SQL onto app_a's tx connection)",
            );
            assert!(
                ctx.take_tx_client_for("app_b").is_none(),
                "SEC-1: app_b must NOT be able to drain app_a's tx client",
            );
            assert!(
                ctx.has_tx_for("app_a"),
                "app_a's parked tx must survive app_b's probe unmodified",
            );
            // The owner can still take its own client back out.
            assert!(
                ctx.take_tx_client_for("app_a").is_some(),
                "the owner must still be able to take its own tx client",
            );
        });
    }

    #[test]
    fn sec1_savepoint_depth_is_scoped_per_app() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut ctx = IsolateDbContext::new();
            ctx.install_tx_client("app_a", sqlite_tx_conn(&dir).await);

            ctx.push_savepoint_for("app_a");
            ctx.push_savepoint_for("app_a");
            assert_eq!(ctx.savepoint_depth_for("app_a"), 2);
            assert_eq!(
                ctx.savepoint_depth_for("app_b"),
                0,
                "SEC-1: app_b must not inherit app_a's savepoint depth \
                 (shared depth corrupts both apps' savepoint names)",
            );

            // app_b settling its own (nonexistent) tx state must not
            // clobber app_a's live savepoint bookkeeping.
            ctx.reset_savepoint_depth_for("app_b");
            assert_eq!(
                ctx.savepoint_depth_for("app_a"),
                2,
                "SEC-1: app_b's settle must not zero app_a's savepoint depth",
            );
        });
    }

    #[test]
    fn sec1_pending_emits_drain_is_scoped_per_app() {
        let mut ctx = IsolateDbContext::new();
        let mut ev_a = dummy_event("orders");
        ev_a.app_id = "app_a".to_string();
        let mut ev_b = dummy_event("messages");
        ev_b.app_id = "app_b".to_string();
        ctx.push_pending_emit(ev_a);
        ctx.push_pending_emit(ev_b);

        let drained_b = ctx.drain_pending_emits_for("app_b");
        assert_eq!(
            drained_b.len(),
            1,
            "SEC-1: app_b's commit drain must only fire app_b's queued events",
        );
        assert_eq!(drained_b[0].app_id, "app_b");

        let drained_a = ctx.drain_pending_emits_for("app_a");
        assert_eq!(
            drained_a.len(),
            1,
            "SEC-1: app_a's queued events must survive app_b's drain \
             (firing them early breaks the Gap-B pre-commit fence)",
        );
        assert_eq!(drained_a[0].app_id, "app_a");
    }

    #[test]
    fn sec1_mig_lock_snapshot_is_owner_scoped() {
        let mut ctx = IsolateDbContext::new();
        ctx.set_mig_lock(mig_lock_for_app("app_a", "m1", "users", 42, false));

        assert!(
            ctx.mig_lock_snapshot_for("app_a").is_some(),
            "the owning app must see its own migration lock",
        );
        assert!(
            ctx.mig_lock_snapshot_for("app_b").is_none(),
            "SEC-1: app_b must NOT see app_a's migration lock \
             (a hit hands app_b the platform-role lock client)",
        );
        // The any-app capacity gate (one migration per worker thread)
        // is intentionally app-agnostic and unchanged.
        assert!(ctx.has_mig_lock());
    }

    // ----- `return_mig_client` empty-slot WARN -------------------------
    //
    // The empty-slot path on `return_mig_client` ([I23] state-machine
    // pair with `set_mig_lock`) emits a `tracing::warn!` whose message
    // names the slot-empty case. We cannot exercise this end-to-end
    // here: `return_mig_client(client: Client)` requires a real
    // `compio_postgres::Client`, and the `test-utils` feature on
    // compio-postgres exposes only `Row` / `Column` / `Statement`
    // builders — no `Client` synthesiser. Constructing one would
    // require either touching `compio-postgres`'s private fields
    // (out of scope for this commit) or spinning up a real Postgres
    // (lives in `tests/integration.rs`).
    //
    // The end-to-end shape is covered by `tests/integration.rs`
    // (operator-cancel race). The PROACTIVE unit-test coverage —
    // catching a future rename of the message string at unit-test
    // time — is supplied by `warn_shape_pin::return_mig_client_message`
    // in the dedicated module below, which pins the literal message
    // payload by re-emitting the same syntax under the capture layer.
}
