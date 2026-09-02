//! Per-worker-thread DB context — single typed home for every plug-in
//! thread-local. The plug-in previously carried several separate
//! `thread_local!` declarations (`DB_POOL`, `DB_URL`, `TX_CONN`,
//! `PENDING_EMITS` in `lib.rs`; `RUNNING_CONSUMERS` in
//! `replication_ops.rs`). Each had its own borrow/take/replace ritual;
//! lifecycle invariants were enforced by convention only.
//!
//! This module folds all of those slots into a single
//! [`ThreadDbContext`] stashed in one [`thread_local!`]. Typed
//! accessors enforce the invariants in one place:
//!
//! * [`crate::context::with`] / [`crate::context::with_mut`] are
//!   the only entry points; every consumer goes through them.
//! * `*_tx_*` methods coordinate the tx-state slots (`tx_conn`,
//!   `savepoint_depth`, `tx_claims`) so the single-connection model the
//!   transaction orchestrator relies on holds (one BEGIN per app per
//!   worker thread, nested `SAVEPOINT`s reusing the same connection). The
//!   `tx_claims` set is what makes "one BEGIN" true: it is held from
//!   before the BEGIN until after the settle, covering the window in
//!   which `tx_conn` is still empty and two overlapping `transaction()`
//!   calls used to both read it as free.
//! * Pending broker emits live on the context; the queue is drained by
//!   the transaction settle path (`drain_pending_emits_on_commit`) and
//!   cleared on ROLLBACK / fresh BEGIN.
//!
//! Each worker thread carries one context, and every V8 isolate scheduled on
//! that thread shares it. The compio runtime is single-threaded per worker, so
//! plain `RefCell` / `Cell` is sufficient. State that must distinguish
//! co-resident isolates therefore needs an explicit binding key; thread-local
//! storage alone does not provide isolate identity.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use compio_postgres::{OwnedPooledClient, Pool};

use crate::backend::sqlite::session::SqliteSessionHandle;
use crate::backend::{BackendHandle, PostgresBackend};
use zeroship_data_core::binding::DbBinding;
use crate::encryption::{LocalKeySource, SuppliedRootKeys};
use zeroship_data_core::error::DbError;
use crate::service::DbResourceKey;
use zeroship_core::change_event::ChangeEvent;

/// Everything true of one app's open transaction.
///
/// Created by `try_claim_tx` and dropped by `release_tx_claim`, which is the
/// bracket the old `tx_claims` set stood for. Between those two points the
/// reducer is admitted, a session is installed and taken and returned, a
/// canceller is recorded, savepoints mark the emit queue, and the queue is
/// drained or discarded - all of it keyed by one `app_id`, all of it ending
/// together.
///
/// Every field is `Option` or a collection with a meaningful empty state,
/// because a lane exists before it has any of them: `try_claim_tx` wins the
/// race first and `BEGIN` runs after.
#[derive(Default)]
pub(crate) struct TxLane {
    /// The SC-1 state machine. `None` between the claim and `admit_transaction`.
    reducer: Option<crate::transaction::reducer::TxReducer>,

    /// The pinned session, or `None` while a `TxClientSlotGuard` holds it out
    /// of the lane across an await. "In transaction" is the lane existing, not
    /// this being `Some`.
    session: Option<TxConnection>,

    /// How to reach the session out-of-band. Captured at install, because the
    /// one moment forced cleanup needs it is the one moment it cannot borrow
    /// the session.
    canceller: Option<crate::transaction::cancel::TxCanceller>,

    /// Parked on this lane ending, so a second top-level `transaction()` for
    /// the same app waits instead of racing.
    claim_waiters: Vec<std::task::Waker>,

    /// Parked on [`Self::session`] refilling. Forced cleanup waits here for a
    /// cancelled statement's holder to give the session back.
    slot_waiters: Vec<std::task::Waker>,

    /// One watermark per open savepoint: the emit-queue length when the frame
    /// opened, so `ROLLBACK TO` truncates to it.
    emit_marks: Vec<usize>,

    /// Change events queued by this transaction, fired on COMMIT and discarded
    /// on ROLLBACK. Only fills inside a transaction - the autocommit path emits
    /// directly.
    pending_emits: Vec<ChangeEvent>,
}

/// **A lane cannot outlive its session, and the disposition is destroy.**
///
/// Removing a lane drops whatever is still parked in it, and for PostgreSQL a
/// plain drop is the WRONG disposition: `OwnedPooledClient::drop` returns the
/// lease to the pool, which would publish a connection still inside its
/// transaction block to the next borrower - on a worker thread that multiplexes
/// co-resident apps, potentially a different tenant's.
///
/// Every settle path disposes of the session before the lane is released, so in
/// practice this fires only on the paths that do not: a teardown, or a lane torn
/// down out from under a holder. Making it structural rather than a rule every
/// caller must remember is the point - see [`destroy_tx_connection`] for what
/// "destroy" costs and why a drop does not achieve it.
impl Drop for TxLane {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            destroy_tx_connection(session);
        }
    }
}

/// Result of trying to claim the per-thread backend initialisation slot.
pub(crate) enum BackendInitState {
    /// A backend is already installed; no work needed.
    Ready,
    /// This caller claimed the slot and must call `finish_backend_init`.
    Acquired,
    /// Another request is currently building the backend.
    InProgress,
}

/// Pinned transaction client parked in the app-keyed, per-thread tx map.
///
/// Postgres keeps a dedicated libpq connection alive for the lifetime of
/// the transaction; SQLite keeps a handle to the shared session actor and
/// drives `BEGIN` / `SAVEPOINT` / `COMMIT` / `ROLLBACK` over that single
/// worker-owned connection.
// `large_enum_variant` fires here as of 2026-08-27: main's compio-postgres work
// grew `Client` to at least 232 bytes against the SQLite handle's 8, and clippy
// was clean on this crate immediately before that merge.
//
// NOT boxed, deliberately. The lint assumes the enum is stored in bulk, where
// the padding multiplies. This one lives in the per-app transaction map at one
// entry per app with an OPEN transaction, and concurrent transactions are
// bounded by the data pool's 8 connections - so the whole population is under
// 2 KB per thread. Boxing would buy that back at the cost of a heap allocation
// on every transaction begin and a pointer chase on every operation inside it,
// which is the wrong trade on the hot path.
//
// What would change the answer: if `TxConnection` ever becomes something held
// per operation, per row, or in a collection that scales with apps rather than
// with open transactions, box it - the lint's assumption would then be true.
#[allow(clippy::large_enum_variant)]
pub(crate) enum TxConnection {
    Postgres(OwnedPooledClient),
    Sqlite(SqliteSessionHandle),
}

impl TxConnection {
    /// Execute a control statement (`BEGIN`, `SAVEPOINT`, `RELEASE`,
    /// `ROLLBACK TO`) on this pinned transaction session.
    ///
    /// **Terminal statements do NOT come through here.** A terminal statement's
    /// command tag is not cosmetic - PostgreSQL answers `COMMIT` with the tag
    /// `ROLLBACK` when the transaction is in a failed state, and this path
    /// returns a row count, which throws the tag away.
    /// [`crate::transaction::driver`]'s `terminal` reads the tag and classifies
    /// the three-way `TerminalResult` the state machine needs.
    ///
    /// **This takes no backend, and that is the point.** Each variant holds
    /// exactly the `SqlExecutor::Client` of the backend that produced it -
    /// `PostgresBackend::Client = OwnedPooledClient`, `SqliteBackend::Client =
    /// SqliteSessionHandle` - so the variant already names the vendor. Passing a
    /// `BackendHandle` alongside and matching on the pair, as this operation did
    /// until 2026-09-02, re-proved that pairing at runtime and needed an
    /// unreachable "backend/client mismatch" arm to be total. Neither backend's
    /// `client_exec` reads `&self`, so the second half carried no information.
    pub(crate) async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        match self {
            Self::Postgres(client) => {
                let rows = client
                    .query_text_params(sql, params)
                    .await
                    .map_err(|e| crate::backend::pg_error::classify(&e))?;
                Ok(rows.len() as u64)
            }
            Self::Sqlite(handle) => handle.exec(sql, params).await,
        }
    }
}

/// Destroy a transaction session's physical connection instead of returning it.
///
/// **This is what SC-1's `WithdrawSession` means, and a plain `drop` is not it.**
/// `OwnedPooledClient::drop` calls `pool.return_client(entry)`, which
/// republishes the lease as idle - so dropping a withdrawn session hands the
/// next borrower exactly the connection the protocol withdrew. Closing the
/// client's request channel first makes `PoolEntry::is_pool_eligible` false (it
/// tests `!client.is_closed()`), and `return_client` then evicts the entry and
/// releases its capacity slot rather than publishing it. The same
/// close-before-drop idiom is what `backend::lock_guard::LockGuard::drop` uses
/// to terminate a session whose advisory lock it could not release.
///
/// SQLite has no pool to return to - the handle is an `Rc` clone of the single
/// writer actor - so the withdrawal is the best-effort detached `ROLLBACK` that
/// stops the actor holding the transaction open. Dropping the handle alone does
/// not touch the live transaction on the worker thread.
pub(crate) fn destroy_tx_connection(client: TxConnection) {
    match client {
        TxConnection::Postgres(mut client) => {
            client.__private_api_close();
            drop(client);
        }
        TxConnection::Sqlite(handle) => {
            if let Err(error) = handle.try_exec_detached("ROLLBACK", &[]) {
                tracing::warn!(
                    error = %error,
                    "sc1: withdrawing a SQLite transaction session could not enqueue its \
                     fallback ROLLBACK; the actor may hold the transaction until it is reaped"
                );
            }
            drop(handle);
        }
    }
}

/// RAII guard for a transaction client temporarily removed from the
/// per-thread map.
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
    /// Drain `app_id`'s transaction client out of the per-thread map.
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

/// Per-worker-thread DB plug-in state, held by [`THREAD_DB_CTX`].
///
/// This is deliberately named for its actual ownership: a worker thread may
/// host many app isolates, including current and deploy-pinned isolates of the
/// same app, and all of them share this value.
///
/// All fields are private. Every consumer goes through an accessor
/// method on this `impl` — [`Self::pool`], [`Self::savepoint_depth_for`],
/// etc. Direct field access from inside the
/// crate is rejected at compile time.
#[allow(missing_debug_implementations)]
pub struct ThreadDbContext {
    /// Connection pool — created lazily on first DB operation.
    pool: Option<Rc<Pool>>,

    /// Database URL — poisoned during `register()`, consumed on first
    /// pool creation.
    db_url: Option<String>,

    /// Stable identity shared by every isolate in this worker process.
    /// Logical-replication slots are per `(app, worker process)`, so all
    /// isolates must stamp the same value while different containers stamp
    /// different values.
    cdc_worker_id: Option<String>,

    /// Every app's open transaction, one entry each.
    ///
    /// **The entry IS the claim.** This map replaced nine parallel
    /// `HashMap<String, _>` on 2026-09-02 - the session, the reducer, the
    /// canceller, the withdrawal tombstone, two waiter lists, the savepoint
    /// emit marks, the pending-emit queue, and a `tx_claims: HashSet<String>`
    /// that existed only to say "one of these is in flight". They were created
    /// together, mutated together and destroyed together, so they were one
    /// entity written nine ways.
    ///
    /// What that buys, beyond the crate boundary it unblocks
    /// (`docs/proposals/2026-09-02-thread-context-ownership.md`):
    ///
    /// * "One top-level BEGIN per app" is now `HashMap::entry` - occupied or
    ///   vacant - rather than an invariant two containers had to agree on. The
    ///   window the old claim existed to cover, between taking it and filling
    ///   the connection slot, is `TxLane { session: None, .. }`: representable.
    ///   A session with no claim, which was a bug, is not.
    /// * [`Self::retire_transaction`] was a hand-written destructor across
    ///   three maps whose doc warned that "leaving the watermarks would let the
    ///   next transaction's first savepoint pop a stale mark and truncate that
    ///   transaction's buffer to an unrelated length". Dropping one struct
    ///   cannot half-happen.
    ///
    /// SEC-1 is unchanged and still the reason for the key: a worker OS thread
    /// multiplexes up to ~200 isolates, and a creator's
    /// `env.db.transaction(async () => await fetch(slow))` parks its session
    /// here across the `await`. Keying by `app_id` makes app A's lane invisible
    /// and untouchable to co-resident app B, which would otherwise run B's SQL
    /// inside A's transaction, snapshot and per-app role.
    lanes: HashMap<String, TxLane>,

    /// Apps whose transaction session was withdrawn: anything returning to the
    /// slot is destroyed rather than parked.
    ///
    /// **Thread-level, NOT per-lane, and the reason is the whole point of a
    /// tombstone: it has to outlive the thing it marks.** SC-1 withdraws a
    /// session and retires its lane in the same settle, but the session itself
    /// can be out of the slot in another future's hands, and that future's
    /// `Drop` returns it afterwards - to a lane that no longer exists. A flag
    /// inside [`TxLane`] would die with the lane and let a session the protocol
    /// condemned reach the pool, and from there the next borrower.
    ///
    /// Cleared by [`Self::admit_transaction`], never by retirement: the
    /// tombstone belongs to the withdrawn session, not to the app.
    withdrawn_tx_sessions: HashSet<String>,

    /// The backend generation stamped on the next installed transaction
    /// session, for SC-1's guard order step 4.
    ///
    /// Thread-level, NOT per-lane, and that is why it did not move into
    /// [`TxLane`]: monotonic for the life of the thread and never reset. A
    /// completion naming a generation the current session does not carry is
    /// stale, and a counter that restarted per lane would let a stale
    /// completion authenticate against a later session by arithmetic
    /// coincidence.
    backend_generation: u64,

    /// This thread's slice of THE schema authority: the runtime descriptor.
    ///
    /// One entry per `(app_id, deploy_token, collection)`. The value is the
    /// descriptor's own field map for that collection - `{ <column>: FieldDef }`
    /// including the v2 `storage` block - carried verbatim from the validated
    /// `manifest.runtime_descriptor` by `DbPlugin::bind_runtime_descriptor`.
    ///
    /// **There is no second source.** The live-catalog introspection cache that
    /// used to sit beside this map is deleted: it re-derived a strict SUBSET of
    /// what the descriptor already carries (`{type, encrypted?, mask?}`, with
    /// `vector`/`geoPoint`/`idPrefix`/`vectorDims` unrecoverable), from sentinels
    /// the migration engine had written out of the same DSL this descriptor is
    /// folded from. It was a round trip, not an independent authority.
    ///
    /// **Keyed by the DEPLOY, not just the app.** A worker thread can hold a
    /// deploy-pinned and a current isolate of one app at the same time, and they
    /// have different schemas. The declared cache was keyed `"{app}:{coll}"` and
    /// would have aliased them; only the deleted introspection cache was
    /// deploy-keyed. Consolidating onto one map keeps the stronger key.
    schemas: HashMap<String, Arc<serde_json::Value>>,

    /// The identity of the database this thread's resources belong to.
    ///
    /// Minted once from validated configuration by `DbService` and stamped here
    /// by `DbPlugin::register`. `install_db_resources` compares on it rather
    /// than on the URL string, so "same database" is decided by the one value
    /// the pools are indexed by; a thread pointed at a different database drops
    /// its pool instead of silently aliasing the old one to the new URL.
    ///
    /// [`DbResourceKey::UNBOUND`] until a URL is installed.
    resource_key: DbResourceKey,

    /// Which backend this thread's URL resolves to, decided ONCE by
    /// `DbService::new` at composition and stamped here by
    /// `DbPlugin::register`.
    ///
    /// `init_pool_async` reads it instead of re-parsing the URL. `None` until a
    /// URL is installed, which is also the "DB plugin disabled" state.
    backend_selection: Option<crate::BackendUrl>,

    /// Per-thread, per-app mask-policy cache. Seeded
    /// on first unmask attempt by reading durable storage (PG admin
    /// schema or SQLite sidecar file); refreshed write-through by the
    /// `setMaskPolicy` op when the SDK's `defineMaskPolicy()` flushes.
    ///
    /// `Some(policy)` — the app declared a policy; the unmask
    /// authorisation path honours it.
    /// `None` (entry missing) — no policy cached on this worker thread
    /// yet. The unmask path then falls through to the default-deny
    /// rule (`auto` actor allowed; everyone else denied).
    ///
    /// Keyed by `app_id`. The entry is never proactively evicted, so it lives
    /// for the worker thread's lifetime unless explicitly replaced.
    mask_policies: HashMap<String, crate::crud::mask_policy::MaskPolicy>,

    /// Backend handle wrapping the pool, as the typed
    /// [`BackendHandle`] enum (see `docs/archive/db-system-design.md`
    /// §5.5 and `docs/archive/p0-implementation-plan.md`).
    /// Created alongside the pool by [`Self::set_pool`] so consumers
    /// can call `ctx.backend()` to get a [`BackendHandle`] without
    /// naming `compio_postgres::Pool` directly.
    ///
    /// **No `dyn Backend` here**: the enum carries the concrete arm
    /// (`Postgres(Rc<PostgresBackend>)` today; `Sqlite(…)` gated on
    /// the `sqlite` feature) so every trait-method call still
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

    /// Column root keys handed to this worker thread directly, in place of
    /// `ZEROSHIP_COLUMN_KEY_<KEYID>`.
    ///
    /// Every backend installed on this worker thread resolves column keys
    /// through [`Self::local_key_source`], so this is the per-thread injection
    /// vehicle for root key material. Unlike app/deploy identity, root keys are
    /// currently configured for the whole worker rather than carried by a V8
    /// wrapper.
    ///
    /// `None` today on every production vector: nothing in the worker or
    /// the CLI installs roots here yet, so those backends read env vars.
    /// The installer that does exist is
    /// `crate::supply_root_keys_for_tests`, which is how the test suites
    /// drive encrypted columns without mutating the process environment.
    supplied_root_keys: Option<Rc<SuppliedRootKeys>>,

    /// This thread's projection of the OPERATOR CHARTER: which columns the
    /// platform assigns, by which generator, on which write event.
    ///
    /// Stamped by `DbPlugin::register` from the charter the plugin prototype
    /// parsed at composition - the same route `meter` and `resource_key` take,
    /// and for the same reason: the value is process-wide, so threading it
    /// through every CRUD call site would carry one constant down every path.
    ///
    /// It is deliberately NOT keyed by app or deploy. The charter is compiled
    /// into the binary; a creator's descriptor may mirror it but may never
    /// replace it, so there is nothing per-tenant to key on.
    assignment_plan: Option<Rc<crate::system_shape_charter::AssignmentPlan>>,
}

impl ThreadDbContext {
    /// Build a fresh per-worker-thread context. Called once per worker
    /// thread on first access (via [`THREAD_DB_CTX`]'s `const`
    /// initialiser path is too restrictive for `HashSet::new`, so the
    /// `RefCell` is initialised lazily through `std::cell::RefCell::new`
    /// in the `thread_local!` body).
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            pool: None,
            db_url: None,
            cdc_worker_id: None,
            lanes: HashMap::new(),
            withdrawn_tx_sessions: HashSet::new(),
            backend_generation: 0,
            schemas: HashMap::new(),
            resource_key: DbResourceKey::UNBOUND,
            backend_selection: None,
            mask_policies: HashMap::new(),
            backend: None,
            backend_init_in_progress: false,
            meter: None,
            supplied_root_keys: None,
            assignment_plan: None,
        }
    }

    // ----- operator assignment charter --------------------------------

    /// This thread's projection of the operator charter, if one is stamped.
    ///
    /// `None` on a thread whose plugin has not registered yet;
    /// [`crate::system_shape_charter::plan`] is the accessor every consumer
    /// uses, and it derives-and-stamps from the same compiled bytes on a miss.
    pub(crate) fn assignment_plan(
        &self,
    ) -> Option<Rc<crate::system_shape_charter::AssignmentPlan>> {
        self.assignment_plan.as_ref().map(Rc::clone)
    }

    /// Stamp the charter projection (called from `DbPlugin::register`, and by
    /// the derive-on-miss path). Idempotent overwrite - registration may fire
    /// more than once per worker thread, and every plugin projects the same
    /// compiled charter.
    pub(crate) fn set_assignment_plan(
        &mut self,
        plan: Rc<crate::system_shape_charter::AssignmentPlan>,
    ) {
        self.assignment_plan = Some(plan);
    }

    // ----- column root keys -------------------------------------------

    /// Install the root-key material this worker thread's backends should resolve
    /// column keys from, in place of `ZEROSHIP_COLUMN_KEY_<KEYID>`.
    ///
    /// `None` restores env sourcing. Installing an EMPTY
    /// [`SuppliedRootKeys`] is not the same thing: that is a source that
    /// provably holds no key, which is what a caller wants when it needs
    /// the not-configured error to be a fact about the source rather than
    /// about the ambient environment.
    pub(crate) fn set_supplied_root_keys(&mut self, keys: Option<Rc<SuppliedRootKeys>>) {
        self.supplied_root_keys = keys;
    }

    /// The local root-key source every backend built on this worker thread
    /// should use: whatever was installed above, else the env vars.
    ///
    /// Backends call this THROUGH the context they are being installed
    /// into rather than reaching for the thread-local themselves, because
    /// [`Self::set_pool`] constructs a `PostgresBackend` while it already
    /// holds `&mut self` and a second borrow of `THREAD_DB_CTX` would panic.
    pub(crate) fn local_key_source(&self) -> LocalKeySource {
        match &self.supplied_root_keys {
            Some(keys) => LocalKeySource::Supplied(Rc::clone(keys)),
            None => LocalKeySource::EnvVar,
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
    /// [`BackendHandle::Postgres`] arm.
    pub(crate) fn set_pool(&mut self, pool: Rc<Pool>) {
        let url = self.db_url.clone().unwrap_or_default();
        let backend = Rc::new(PostgresBackend::new_with_key_source(
            Rc::clone(&pool),
            url,
            self.local_key_source(),
        ));
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
    /// This is the production installer used by `init_pool_async`'s
    /// runtime URL dispatch, not a test-only seam.
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

    /// The identity of the database this thread's resources belong to.
    pub(crate) fn resource_key(&self) -> DbResourceKey {
        self.resource_key
    }

    /// The backend the service selected for this thread's URL. `None` when no
    /// database is configured - the "DB plugin disabled" state.
    pub(crate) fn backend_selection(&self) -> Option<crate::BackendUrl> {
        self.backend_selection.clone()
    }

    /// Hand this thread the service's database resources: the URL its lazy
    /// backend init will open, the backend selection already made for it, and
    /// the stable resource key everything is indexed by.
    ///
    /// Returns `true` iff the resource CHANGED - i.e. this thread was pointed
    /// at a different database. The caller drops the pool in that case so the
    /// next CRUD call rebuilds it, rather than silently aliasing the old pool
    /// to the new URL.
    ///
    /// The comparison is on the resource key, not the URL string: the key is
    /// minted once from validated configuration, so "same database" is decided
    /// by the same value the caches and pools are indexed by rather than
    /// re-decided from a string here.
    pub(crate) fn install_db_resources(
        &mut self,
        url: &str,
        resource_key: DbResourceKey,
        backend_selection: crate::BackendUrl,
    ) -> bool {
        let different = self.resource_key != resource_key;
        self.db_url = Some(url.to_string());
        self.resource_key = resource_key;
        self.backend_selection = Some(backend_selection);
        different
    }

    /// Read the worker-process identity used in CDC slot names.
    pub(crate) fn cdc_worker_id(&self) -> Option<String> {
        self.cdc_worker_id.clone()
    }

    /// Stamp the worker-process identity during plug-in registration.
    pub(crate) fn set_cdc_worker_id(&mut self, worker_id: &str) {
        self.cdc_worker_id = Some(worker_id.to_string());
    }

    /// The descriptor-store key for one collection under one binding.
    fn schema_key(binding: &DbBinding, collection: &str) -> String {
        format!(
            "{}:{}:{}",
            binding.app_id(),
            binding.deploy_token(),
            collection
        )
    }

    /// Install one collection's descriptor entry for this binding.
    /// Test fixtures use this narrow helper; production boot replaces the
    /// binding's complete descriptor through [`Self::replace_schemas`].
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn cache_schema(
        &mut self,
        binding: &DbBinding,
        collection: &str,
        schema: serde_json::Value,
    ) {
        self.schemas
            .insert(Self::schema_key(binding, collection), Arc::new(schema));
    }

    /// Replace the complete descriptor for one app-at-deploy binding.
    ///
    /// Callers fully validate and collect every entry before borrowing the
    /// context mutably. The retain-and-insert sequence is therefore one
    /// synchronous publication point: no callback can observe a partial
    /// descriptor, removed collections do not survive a dev isolate restart,
    /// and an empty input leaves a schema-less binding empty.
    pub(crate) fn replace_schemas(
        &mut self,
        binding: &DbBinding,
        schemas: Vec<(String, serde_json::Value)>,
    ) {
        let prefix = format!("{}:{}:", binding.app_id(), binding.deploy_token());
        self.schemas.retain(|key, _| !key.starts_with(&prefix));
        for (collection, schema) in schemas {
            self.schemas
                .insert(Self::schema_key(binding, &collection), Arc::new(schema));
        }
    }

    /// The descriptor entry for one collection under one binding.
    ///
    /// `None` means the descriptor this isolate was built from does not declare
    /// the collection. Callers must NOT treat that as "carry on without a
    /// schema" - see [`crate::descriptor::collection_schema`], which is the only
    /// thing that should call this and which turns the miss into a typed error.
    pub(crate) fn schema_for(
        &self,
        binding: &DbBinding,
        collection: &str,
    ) -> Option<Arc<serde_json::Value>> {
        self.schemas
            .get(&Self::schema_key(binding, collection))
            .cloned()
    }

    /// Enumerate every `(collection, schema)` pair the descriptor store holds
    /// for one BINDING. `mint_tx_view` uses it to mint one `Collection` per
    /// declared name onto the `tx` view; the drift-check sweep uses it to walk
    /// every declared collection. Empty when the isolate has installed no
    /// schema.
    ///
    /// Key shape: `<app_id>:<deploy_token>:<collection>` (the same format
    /// [`Self::schema_key`] writes); the collection name is the suffix.
    pub(crate) fn cached_schemas_for_binding(
        &self,
        binding: &DbBinding,
    ) -> Vec<(String, Arc<serde_json::Value>)> {
        let prefix = format!("{}:{}:", binding.app_id(), binding.deploy_token());
        self.schemas
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix(&prefix)
                    .map(|coll| (coll.to_string(), v.clone()))
            })
            .collect()
    }

    // ----- MASK_POLICIES ---------------------------------------------

    /// Fetch the cached mask policy for `app_id`.
    /// Returns `None` when the cache holds no entry for the app
    /// (caller falls through to durable-storage load + cache install,
    /// or to the default-deny rule on a miss).
    pub(crate) fn mask_policy_for(
        &self,
        app_id: &str,
    ) -> Option<crate::crud::mask_policy::MaskPolicy> {
        self.mask_policies.get(app_id).cloned()
    }

    /// Write-through cache install. `Some(policy)`
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

    /// `true` iff a mask-policy entry is cached for
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
        self.lanes.get(app_id).is_some_and(|l| l.session.is_some())
    }

    /// Take `app_id`'s top-level-transaction claim if it is free.
    /// `true` means this caller now owns it and MUST release it via
    /// [`Self::release_tx_claim`] when its transaction settles.
    ///
    /// **Claiming IS opening the lane.** The check and the set are one
    /// `HashMap::entry`, so the window that used to exist between them cannot:
    /// a second `transaction()` for the same app finds the entry occupied and
    /// parks, rather than racing to fill a slot both read as free.
    pub(crate) fn try_claim_tx(&mut self, app_id: &str) -> bool {
        match self.lanes.entry(app_id.to_string()) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(TxLane::default());
                true
            }
        }
    }

    /// `true` if some top-level transaction for `app_id` is in flight —
    /// including one whose `BEGIN` has not landed yet, which is the
    /// window [`Self::has_tx_for`] cannot see.
    pub(crate) fn tx_claimed_by(&self, app_id: &str) -> bool {
        self.lanes.contains_key(app_id)
    }

    /// Close `app_id`'s lane and wake everything parked on it.
    ///
    /// The lane is REMOVED, which is the release: every field goes with it, so
    /// no residue can outlive the transaction that owned it. The waiters are
    /// taken out first and woken after, because they are about to re-claim.
    ///
    /// A session still in the lane at this point is DESTROYED by
    /// [`TxLane::drop`], never returned to the pool - see that impl for why the
    /// distinction is a security one rather than a tidiness one.
    pub(crate) fn release_tx_claim(&mut self, app_id: &str) {
        let Some(mut lane) = self.lanes.remove(app_id) else {
            return;
        };
        for waker in std::mem::take(&mut lane.claim_waiters) {
            waker.wake();
        }
    }

    /// Park a waker on `app_id`'s lane closing.
    ///
    /// A no-op when there is no lane: the claim is already free, so the caller
    /// will win it on its next poll rather than sleeping for a wake that has
    /// no one to send it.
    pub(crate) fn push_tx_waiter(&mut self, app_id: &str, waker: std::task::Waker) {
        if let Some(lane) = self.lanes.get_mut(app_id) {
            lane.claim_waiters.push(waker);
        } else {
            waker.wake();
        }
    }

    /// Park a connection in `app_id`'s transaction slot. Returns the
    /// previous occupant for that app, if any (callers should ensure
    /// this is `None` — every top-level begin path holds `app_id`'s
    /// claim from [`Self::try_claim_tx`] first, which is what keeps two
    /// in-flight `BEGIN`s from reaching here). A different app's parked
    /// tx is never disturbed (SEC-1).
    pub(crate) fn install_tx_client(
        &mut self,
        app_id: &str,
        client: TxConnection,
    ) -> Option<TxConnection> {
        self.lanes
            .entry(app_id.to_string())
            .or_default()
            .session
            .replace(client)
    }

    /// Take `app_id`'s transaction client out of the slot. The caller
    /// must either return it via [`Self::put_tx_client_for`] (when the
    /// await is short and the slot should remain "in transaction") or
    /// drop the client (when settling the tx). Returns `None` when no tx
    /// is parked for `app_id` — including when another app owns the only
    /// parked tx (SEC-1: app B cannot drain app A's client).
    pub(crate) fn take_tx_client_for(&mut self, app_id: &str) -> Option<TxConnection> {
        self.lanes.get_mut(app_id)?.session.take()
    }

    /// Return a client previously taken via [`Self::take_tx_client_for`]
    /// to `app_id`'s slot.
    ///
    /// **A withdrawn session is destroyed here rather than parked.** SC-1's
    /// [`Action::WithdrawSession`](crate::transaction::reducer::Action::WithdrawSession)
    /// can land while another future holds the session out of the slot; that
    /// future's [`TxClientSlotGuard`] restores it on drop, and without this
    /// check the restoration would hand a withdrawn session straight back to
    /// the pool.
    pub(crate) fn put_tx_client_for(&mut self, app_id: &str, client: TxConnection) {
        // **Checked before the lane, because the tombstone outlives it.** A
        // withdrawal retires its lane moments later, and the holder's `Drop`
        // can land after that; consulting the lane first would find nothing and
        // fall through to parking a session the protocol already condemned.
        if self.withdrawn_tx_sessions.contains(app_id) {
            destroy_tx_connection(client);
            return;
        }
        // No lane and no tombstone means the transaction that owned this
        // session settled normally and released it. There is nowhere to park it
        // and nobody to serve it, so it is destroyed rather than resurrecting a
        // closed lane.
        let Some(lane) = self.lanes.get_mut(app_id) else {
            destroy_tx_connection(client);
            return;
        };
        // **An occupied slot means this client is not the current session.**
        // The normal take/put cycle leaves the slot empty between the two calls,
        // so an occupant here can only be a LATER transaction's session - which
        // happens when a withdrawal races the guard that was holding the old
        // one: the next transaction is admitted (clearing the tombstone) before
        // the guard's `Drop` runs. Without this arm the dead session would
        // overwrite the live one, and the tombstone alone cannot cover it
        // because it is cleared by exactly the admission that creates the race.
        if lane.session.is_some() {
            tracing::warn!(
                app_id,
                "a transaction session returned to an occupied slot; destroying it \
                 rather than clobbering the session that is there"
            );
            destroy_tx_connection(client);
            return;
        }
        lane.session = Some(client);
        // The session is back. Wake anything waiting to reclaim it - forced
        // cleanup that cancelled the statement this guard was running is
        // parked on exactly this moment.
        //
        // Deliberately NOT woken on either arm above: a destroyed session is
        // not a reclaimable one, and waking there would hand a waiter an empty
        // slot it has to re-check anyway. Those waiters are released by
        // `retire_transaction` instead, which is what a destroyed session's
        // transaction always reaches.
        self.wake_tx_slot_waiters(app_id);
    }

    /// Wake everything parked on `app_id`'s transaction slot.
    fn wake_tx_slot_waiters(&mut self, app_id: &str) {
        if let Some(waiters) = self
            .lanes
            .get_mut(app_id)
            .map(|lane| std::mem::take(&mut lane.slot_waiters))
        {
            for waker in waiters {
                waker.wake();
            }
        }
    }

    /// Park a waker on `app_id`'s transaction slot refilling.
    ///
    /// Deduplicated by [`std::task::Waker::will_wake`] because the waiter
    /// re-registers on every poll and a `timeout` wrapper polls it more than
    /// once per wake; without this the list would grow for the life of the
    /// wait.
    pub(crate) fn push_tx_slot_waiter(&mut self, app_id: &str, waker: &std::task::Waker) {
        // No lane means no session is ever coming back to this slot, so the
        // caller is woken to re-check rather than parked on a wake that has no
        // sender. `retire_transaction` served that role before the lane owned
        // its own waiters.
        let Some(lane) = self.lanes.get_mut(app_id) else {
            waker.wake_by_ref();
            return;
        };
        if lane.slot_waiters.iter().any(|p| p.will_wake(waker)) {
            return;
        }
        lane.slot_waiters.push(waker.clone());
    }

    /// Record the canceller for the session just installed in `app_id`'s slot.
    pub(crate) fn install_tx_canceller(
        &mut self,
        app_id: &str,
        canceller: crate::transaction::cancel::TxCanceller,
    ) {
        self.lanes.entry(app_id.to_string()).or_default().canceller = Some(canceller);
    }

    /// Clone out `app_id`'s canceller.
    ///
    /// Cloned rather than borrowed on purpose: cancelling is `async`, and a
    /// `RefCell` borrow of this context must never be held across an await.
    pub(crate) fn tx_canceller_for(
        &self,
        app_id: &str,
    ) -> Option<crate::transaction::cancel::TxCanceller> {
        self.lanes.get(app_id).and_then(|lane| lane.canceller.clone())
    }

    /// Drop `app_id`'s canceller.
    ///
    /// **This must happen BEFORE the pooled lease is returned, and that is not
    /// tidiness - it is the difference between keeping the connection and losing
    /// it.** `Pool::return_client` calls `pool_cancel_lease_prevents_reuse`,
    /// which is `Arc::strong_count(lease) > 1 || lease.is_uncertain()`, and
    /// retires the physical session when it is true. A retained `CancelToken`
    /// holds one of those strong references, so a canceller still parked here
    /// when the session goes back destroys exactly the connection cancellation
    /// exists to preserve. That is the pool's documented contract - "if it is
    /// retained, the pool retires the physical session instead of letting the
    /// token target its next borrower" - and it is a real defence, not an
    /// inconvenience: it is why a canceller cannot outlive its lease and reach
    /// the next borrower's query.
    ///
    /// [`Self::retire_transaction`] also drops it, as a backstop for the paths
    /// that never install a session.
    pub(crate) fn remove_tx_canceller(&mut self, app_id: &str) {
        if let Some(lane) = self.lanes.get_mut(app_id) { lane.canceller = None; }
    }

    // ----- SC-1 TRANSACTION REDUCER -----------------------------------

    /// Admit a transaction for `app_id` and return the actions admission
    /// emits.
    ///
    /// The execution deadline is armed on this transition - the same one that
    /// grants admission - so queue time does not consume the transaction's
    /// execution budget.
    pub(crate) fn admit_transaction(
        &mut self,
        app_id: &str,
        expected: crate::transaction::reducer::identity::ExpectedAuthority,
        budgets: crate::transaction::reducer::TxBudgets,
        now: std::time::Instant,
        max_depth: u32,
    ) -> Vec<crate::transaction::reducer::Action> {
        let (reducer, actions) =
            crate::transaction::reducer::TxReducer::admit(expected, budgets, now, max_depth);
        let lane = self.lanes.entry(app_id.to_string()).or_default();
        let previous = lane.reducer.replace(reducer);
        debug_assert!(
            previous.is_none(),
            "admit_transaction: a transaction is already admitted for this app",
        );
        // A fresh transaction starts from a clean withdrawal state; the
        // tombstone belongs to the session that was withdrawn, not to the app.
        self.withdrawn_tx_sessions.remove(app_id);
        actions
    }

    /// Apply one event to `app_id`'s reducer. `None` means no transaction is
    /// admitted for that app.
    pub(crate) fn apply_transaction_event(
        &mut self,
        app_id: &str,
        event: crate::transaction::reducer::TxEvent,
        now: std::time::Instant,
    ) -> Option<Vec<crate::transaction::reducer::Action>> {
        self.lanes
            .get_mut(app_id).and_then(|lane| lane.reducer.as_mut())
            .map(|reducer| reducer.apply(event, now))
    }

    /// Borrow `app_id`'s reducer, for the frame stack and the latched cleanup
    /// cause the driver reads back.
    pub(crate) fn transaction_reducer(
        &self,
        app_id: &str,
    ) -> Option<&crate::transaction::reducer::TxReducer> {
        self.lanes.get(app_id).and_then(|lane| lane.reducer.as_ref())
    }

    /// The authority `app_id`'s transaction was admitted under, for the events
    /// that must carry it (guard order step 1).
    pub(crate) fn transaction_expected_authority(
        &self,
        app_id: &str,
    ) -> Option<&crate::transaction::reducer::identity::ExpectedAuthority> {
        self.lanes.get(app_id).and_then(|lane| lane.reducer.as_ref()).map(|r| r.expected())
    }

    /// Drop `app_id`'s settled reducer and the frame watermarks that died with
    /// its frames.
    ///
    /// Leaving the watermarks would let the next transaction's first savepoint
    /// pop a stale mark and truncate that transaction's buffer to an unrelated
    /// length.
    ///
    /// It also drops the canceller and RELEASES anything parked on the slot.
    /// The waiter is forced cleanup waiting to reclaim a cancelled statement's
    /// session; if this transaction has been retired out from under it - which
    /// is what the `CancellationSql` deadline does - the session it is waiting
    /// for is never coming, and it must be woken to discover that rather than
    /// sitting out its whole grace.
    pub(crate) fn retire_transaction(&mut self, app_id: &str) {
        if let Some(lane) = self.lanes.get_mut(app_id) {
            lane.reducer = None;
            lane.emit_marks.clear();
            lane.canceller = None;
        }
        self.wake_tx_slot_waiters(app_id);
    }

    /// Mint the backend generation for the next installed session.
    pub(crate) const fn next_backend_generation(&mut self) -> u64 {
        self.backend_generation += 1;
        self.backend_generation
    }

    /// SC-1 `Action::WithdrawSession`: mark `app_id`'s session withdrawn and
    /// hand the caller whatever is in the slot to destroy.
    ///
    /// The tombstone outlives this call deliberately, and **outlives the lane**
    /// - see [`Self::put_tx_client_for`]. It is recorded beside the lanes
    /// rather than inside one because its whole purpose is to describe a
    /// session whose lane is gone: set here, cleared by the next
    /// [`Self::admit_transaction`], never by retirement.
    pub(crate) fn withdraw_tx_session(&mut self, app_id: &str) -> Option<TxConnection> {
        self.withdrawn_tx_sessions.insert(app_id.to_string());
        self.lanes.get_mut(app_id)?.session.take()
    }

    /// Has `app_id`'s transaction session been withdrawn?
    pub(crate) fn tx_session_withdrawn(&self, app_id: &str) -> bool {
        self.withdrawn_tx_sessions.contains(app_id)
    }

    // ----- FRAME EFFECT WATERMARKS ------------------------------------

    /// Record the queued-event watermark for a frame that just opened.
    pub(crate) fn push_frame_emit_mark(&mut self, app_id: &str) {
        let lane = self.lanes.entry(app_id.to_string()).or_default();
        let mark = lane.pending_emits.len();
        lane.emit_marks.push(mark);
    }

    /// Pop a released frame's watermark without truncating: those events belong
    /// to the enclosing frame now, exactly as its rows do.
    pub(crate) fn pop_frame_emit_mark(&mut self, app_id: &str) -> Option<usize> {
        self.lanes
            .get_mut(app_id)
            .and_then(|lane| lane.emit_marks.pop())
    }

    /// Discard the events the current frame queued, truncating back to its
    /// watermark.
    ///
    /// Called **only after `ROLLBACK TO SAVEPOINT` has succeeded**, because only
    /// then are the matching database changes known to be undone. Discarding
    /// before the statement ran is the "mutate on the assumption it will
    /// succeed" shape: it leaves the failure row's documented fate - retain the
    /// effects for diagnosis, poison the transaction - unachievable, since the
    /// evidence is already gone.
    ///
    /// The watermark is NOT popped here. A rolled-back frame is not closed
    /// until its `RELEASE` lands, and that is the call that pops it.
    pub(crate) fn discard_frame_effects(&mut self, app_id: &str) {
        // A missing mark means the stacks desynced. Truncating to 0 would
        // discard the ENCLOSING frame's events too, so leave the buffer alone:
        // over-publishing is a bug, but silently dropping a committed row's
        // event is a worse one.
        let Some(lane) = self.lanes.get_mut(app_id) else {
            return;
        };
        if let Some(mark) = lane.emit_marks.last().copied() {
            if mark <= lane.pending_emits.len() {
                lane.pending_emits.truncate(mark);
            }
        }
    }

    // ----- PENDING_EMITS ---------------------------------------------

    /// Push a `ChangeEvent` onto the owning app's pending-emits queue
    /// (keyed by the event's own `app_id`; the queue is allocated
    /// lazily on first push within that app's tx).
    pub(crate) fn push_pending_emit(&mut self, ev: ChangeEvent) {
        self.lanes
            .entry(ev.app_id.clone())
            .or_default()
            .pending_emits
            .push(ev);
    }

    /// Drain `app_id`'s pending-emits queue (returns `Vec::new()` if the
    /// app has none queued). Called by the transaction settle path on
    /// COMMIT. SEC-1: only the committing app's events are returned, so
    /// one app's COMMIT cannot fire another's pre-commit events.
    pub(crate) fn drain_pending_emits_for(&mut self, app_id: &str) -> Vec<ChangeEvent> {
        self.lanes
            .get_mut(app_id)
            .map(|lane| std::mem::take(&mut lane.pending_emits))
            .unwrap_or_default()
    }

    /// Clear `app_id`'s pending-emits queue without firing any events.
    /// Called by the transaction settle path on ROLLBACK and by
    /// `exec_begin` to drop any stale residue from an interrupted prior
    /// run. A different app's queue is untouched (SEC-1).
    pub(crate) fn clear_pending_emits_for(&mut self, app_id: &str) {
        if let Some(lane) = self.lanes.get_mut(app_id) {
            lane.pending_emits.clear();
        }
    }
}

impl Default for ThreadDbContext {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// The DB context shared by all isolates on this worker thread. Replaces
    /// the slot-per-thread-local
    /// lattice that previously lived in `lib.rs`, `migrations.rs`, and
    /// `replication_ops.rs`.
    pub(crate) static THREAD_DB_CTX: RefCell<ThreadDbContext> =
        RefCell::new(ThreadDbContext::new());
}

/// Run `f` with a shared reference to the per-worker-thread DB context.
///
/// The compio runtime is single-threaded per worker; this never
/// contends. Callers must NOT re-enter [`with`] / [`with_mut`] from
/// inside `f` (the underlying `RefCell` will panic).
pub fn with<R>(f: impl FnOnce(&ThreadDbContext) -> R) -> R {
    THREAD_DB_CTX.with(|c| f(&c.borrow()))
}

/// Run `f` with an exclusive reference to the per-worker-thread DB context.
/// Same re-entrancy rule as [`with`].
pub fn with_mut<R>(f: impl FnOnce(&mut ThreadDbContext) -> R) -> R {
    THREAD_DB_CTX.with(|c| f(&mut c.borrow_mut()))
}

/// The column-key source for a backend that constructs itself rather than
/// being built by [`ThreadDbContext::set_pool`].
///
/// Both backends are in that position: `SqliteBackend::{new, open}` and
/// `PostgresBackend::new` are called directly (by `init_pool_async`, and
/// by tests) and then handed to the context, so they read the worker
/// thread's installed root keys here. One function serves both because
/// key sourcing no longer differs by backend -- see
/// `crate::encryption::keys`. Same re-entrancy rule as [`with`] - do not
/// call this from inside a context closure. `set_pool` does NOT call it:
/// it already holds `&mut` on the context and reads
/// [`ThreadDbContext::local_key_source`] off `self` instead.
pub(crate) fn isolate_key_source() -> crate::encryption::LocalKeySource {
    with(ThreadDbContext::local_key_source)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the per-worker-thread DB context state machine.
    //!
    //! Every test constructs a fresh [`ThreadDbContext`] directly via
    //! [`ThreadDbContext::new`] — the thread-local [`THREAD_DB_CTX`] is
    //! avoided so test ordering on the same OS thread cannot cause one
    //! test to observe state another mutated.
    //!
    //! ## Why some accessors aren't covered here
    //!
    //! The slots that store a live transaction client
    //! (`tx_conn`) need a real backend handle to
    //! exercise. The Postgres side still needs a `compio_postgres::Client`
    //! (`Client::new` is `pub(crate)` on the driver), and the SQLite side
    //! would need a live session actor. These unit tests stay pure-state;
    //! end-to-end slot round-trips live in the integration targets.
    //!
    //! Concretely, the following can only be exercised by
    //! `tests/integration.rs` (which spins up a real PG):
    //!
    //! * [`ThreadDbContext::install_tx_client`] /
    //!   [`ThreadDbContext::take_tx_client`] /
    //!   [`ThreadDbContext::put_tx_client`] round-trip with a real
    //!   backend client.
    //! * The `debug_assert!` inside [`ThreadDbContext::push_savepoint`]
    //!   that a savepoint requires an active `tx_conn` — same constraint;
    //!   the pop/reset arms (no such precondition) are unit-tested.
    //!
    //! For [`ThreadDbContext::set_pool`] / [`ThreadDbContext::backend`]
    //! we need an `Rc<Pool>`, which only `Pool::connect` produces. Those
    //! lifecycles are covered by `tests/integration.rs`.

    use super::*;

    use crate::BackendUrl;
    use std::collections::HashMap;
    use zeroship_core::change_event::{ChangeEvent, ChangeOp};

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

    // ----- ThreadDbContext::new / Default -------------------------------

    #[test]
    fn new_yields_fully_cleared_slots() {
        let ctx = ThreadDbContext::new();
        assert!(ctx.pool().is_none());
        assert!(!ctx.pool_initialised());
        assert!(ctx.backend().is_none());
        assert!(ctx.db_url().is_none());
        assert!(!ctx.has_tx_for("a"));
        assert!(ctx.transaction_reducer("a").is_none());
        assert!(!ctx.tx_session_withdrawn("a"));
        // pending_emits starts empty (each app's queue is allocated
        // lazily on first push).
        assert!(ctx.lanes.values().all(|lane| lane.pending_emits.is_empty()));
    }

    #[test]
    fn default_matches_new() {
        let a = ThreadDbContext::default();
        let b = ThreadDbContext::new();
        // Compare observable state (no PartialEq on the struct).
        assert_eq!(a.pool_initialised(), b.pool_initialised());
        assert_eq!(
            a.transaction_reducer("a").is_none(),
            b.transaction_reducer("a").is_none()
        );
        assert_eq!(a.has_tx_for("a"), b.has_tx_for("a"));
        assert_eq!(a.db_url(), b.db_url());
    }

    // ----- installed DB resources ----------------------------------------

    /// Install the resources a service over `url` would hand this thread.
    fn install(ctx: &mut ThreadDbContext, url: &str) -> bool {
        ctx.install_db_resources(
            url,
            DbResourceKey::for_url(url),
            crate::service::select_backend(url).expect("test URLs must be valid"),
        )
    }

    #[test]
    fn installing_a_new_database_reports_the_change() {
        let mut ctx = ThreadDbContext::new();
        assert!(install(&mut ctx, "postgres://a"));
        assert_eq!(ctx.db_url().as_deref(), Some("postgres://a"));
        assert_eq!(ctx.resource_key(), DbResourceKey::for_url("postgres://a"));
    }

    #[test]
    fn reinstalling_the_same_database_reports_no_change() {
        let mut ctx = ThreadDbContext::new();
        assert!(install(&mut ctx, "postgres://a"));
        assert!(!install(&mut ctx, "postgres://a"));
        assert_eq!(ctx.db_url().as_deref(), Some("postgres://a"));
    }

    #[test]
    fn installing_a_second_database_reports_the_change() {
        let mut ctx = ThreadDbContext::new();
        install(&mut ctx, "postgres://a");
        assert!(install(&mut ctx, "postgres://b"));
        assert_eq!(ctx.db_url().as_deref(), Some("postgres://b"));
        assert_ne!(ctx.resource_key(), DbResourceKey::for_url("postgres://a"));
    }

    /// The URL and its backend selection are installed together, so no reader
    /// can observe one without the other. `init_pool_async` depends on that:
    /// it opens the pool from this selection rather than re-parsing the URL.
    #[test]
    fn the_url_and_its_backend_selection_are_installed_together() {
        let mut ctx = ThreadDbContext::new();
        assert!(ctx.db_url().is_none() && ctx.backend_selection().is_none());

        install(&mut ctx, "postgres://a");
        assert!(matches!(
            ctx.backend_selection(),
            Some(BackendUrl::Postgres)
        ));

        install(&mut ctx, "sqlite:/tmp/ctx-install.sqlite");
        assert!(matches!(
            ctx.backend_selection(),
            Some(BackendUrl::Sqlite { .. })
        ));
    }

    #[test]
    fn installing_resources_does_not_touch_the_pool_slot() {
        // `install_db_resources` is *just* the configuration slots; the caller
        // (lib.rs) invokes `clear_pool` when the resource changed. We can't
        // install a real pool here (Pool::connect needs PG), but we can pin
        // that the installer itself leaves the pool/backend slots alone.
        let mut ctx = ThreadDbContext::new();
        install(&mut ctx, "postgres://a");
        assert!(ctx.pool().is_none());
        assert!(ctx.backend().is_none());
        let _ = install(&mut ctx, "postgres://a"); // no-op
        assert!(ctx.pool().is_none());
        assert!(ctx.backend().is_none());
    }

    // ----- clear_pool ----------------------------------------------------

    #[test]
    fn clear_pool_when_unset_is_noop() {
        let mut ctx = ThreadDbContext::new();
        ctx.clear_pool();
        assert!(ctx.pool().is_none());
        assert!(ctx.backend().is_none());
    }

    // ----- backend cold-init single-flight -------------------------------

    #[test]
    fn backend_init_slot_allows_one_initializer_at_a_time() {
        let mut ctx = ThreadDbContext::new();

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
        let mut ctx = ThreadDbContext::new();

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

    // ----- TX token monotonic counter ------------------------------------

    #[test]
    fn frame_emit_marks_never_discard_an_enclosing_frames_events() {
        let mut ctx = ThreadDbContext::new();
        // No frame is open, so there is no watermark to pop.
        assert_eq!(
            ctx.pop_frame_emit_mark("app_t"),
            None,
            "no open frame yields no watermark"
        );

        // Discarding with no watermark on the stack must leave the buffer ALONE
        // rather than truncating to zero, which would drop the enclosing
        // frame's events. Over-publishing is a bug; silently dropping a
        // committed row's event is a worse one.
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.discard_frame_effects("app_t");
        assert_eq!(
            ctx.lanes.get("app_t").map(|lane| lane.pending_emits.len()),
            Some(1),
            "a missing watermark must not discard the enclosing frame's events"
        );

        // A real watermark discards exactly the frame's own events: the mark is
        // taken when the frame opens, so everything queued after it is the
        // frame's and everything before it is the parent's.
        ctx.push_frame_emit_mark("app_t");
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.discard_frame_effects("app_t");
        let kept = &ctx.lanes.get("app_t").expect("lane").pending_emits;
        assert_eq!(kept.len(), 1, "truncate to the frame's watermark");
        assert_eq!(
            kept[0].collection, "c1",
            "the enclosing frame's event survives"
        );

        // `discard_frame_effects` does NOT pop: a rolled-back frame is not
        // closed until its RELEASE lands, and that is the call that pops.
        assert_eq!(ctx.pop_frame_emit_mark("app_t"), Some(1));
        assert_eq!(ctx.pop_frame_emit_mark("app_t"), None);
    }

    /// `retire_transaction` drops the watermarks that died with the frames.
    ///
    /// Leaving them would let the next transaction's first savepoint pop a
    /// stale mark and truncate that transaction's buffer to an unrelated
    /// length.
    #[test]
    fn retiring_a_transaction_drops_its_frame_watermarks() {
        let mut ctx = ThreadDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_frame_emit_mark("app_t");
        ctx.retire_transaction("app_t");
        assert_eq!(
            ctx.pop_frame_emit_mark("app_t"),
            None,
            "a retired transaction leaves no watermark behind"
        );
    }

    /// The backend generation is monotonic and never restarts.
    ///
    /// A counter that restarted would let a stale completion authenticate
    /// against a later session by arithmetic coincidence, which is exactly what
    /// SC-1's guard order step 4 exists to refuse.
    #[test]
    fn backend_generations_are_monotonic() {
        let mut ctx = ThreadDbContext::new();
        let first = ctx.next_backend_generation();
        let second = ctx.next_backend_generation();
        assert!(second > first, "generations must strictly increase");
        ctx.retire_transaction("app_t");
        assert!(
            ctx.next_backend_generation() > second,
            "retiring a transaction must not restart the sequence"
        );
    }

    // ----- PENDING_EMITS state machine -----------------------------------

    #[test]
    fn pending_emits_start_empty() {
        let ctx = ThreadDbContext::new();
        assert!(ctx.lanes.values().all(|lane| lane.pending_emits.is_empty()));
    }

    #[test]
    fn push_pending_emit_allocates_slot_lazily() {
        // dummy_event tags app_id "app_t"; the queue keys on that.
        let mut ctx = ThreadDbContext::new();
        assert!(ctx.lanes.values().all(|lane| lane.pending_emits.is_empty()));
        ctx.push_pending_emit(dummy_event("c1"));
        assert!(ctx.lanes.contains_key("app_t"));
        assert_eq!(ctx.lanes.get("app_t").unwrap().pending_emits.len(), 1);
    }

    #[test]
    fn push_pending_emit_accumulates() {
        let mut ctx = ThreadDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.push_pending_emit(dummy_event("c3"));
        let evs = &ctx.lanes.get("app_t").unwrap().pending_emits;
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].collection, "c1");
        assert_eq!(evs[1].collection, "c2");
        assert_eq!(evs[2].collection, "c3");
    }

    #[test]
    fn drain_pending_emits_returns_and_clears() {
        let mut ctx = ThreadDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        let drained = ctx.drain_pending_emits_for("app_t");
        assert_eq!(drained.len(), 2);
        // After drain the app's queue is cleared — subsequent pushes
        // re-allocate.
        assert!(ctx.lanes.get("app_t").is_none_or(|lane| lane.pending_emits.is_empty()));
    }

    #[test]
    fn drain_pending_emits_on_empty_returns_empty_vec() {
        let mut ctx = ThreadDbContext::new();
        let drained = ctx.drain_pending_emits_for("app_t");
        assert!(drained.is_empty());
        assert!(ctx.lanes.values().all(|lane| lane.pending_emits.is_empty()));
    }

    #[test]
    fn drain_then_push_starts_fresh() {
        let mut ctx = ThreadDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        let _ = ctx.drain_pending_emits_for("app_t");
        ctx.push_pending_emit(dummy_event("c2"));
        let evs = &ctx.lanes.get("app_t").unwrap().pending_emits;
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].collection, "c2");
    }

    #[test]
    fn clear_pending_emits_drops_without_returning() {
        let mut ctx = ThreadDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.clear_pending_emits_for("app_t");
        assert!(ctx.lanes.get("app_t").is_none_or(|lane| lane.pending_emits.is_empty()));
        // A subsequent drain returns empty (queue is gone).
        assert!(ctx.drain_pending_emits_for("app_t").is_empty());
    }

    #[test]
    fn clear_pending_emits_on_empty_is_idempotent() {
        let mut ctx = ThreadDbContext::new();
        ctx.clear_pending_emits_for("app_t");
        ctx.clear_pending_emits_for("app_t");
        assert!(ctx.lanes.values().all(|lane| lane.pending_emits.is_empty()));
    }

    // ----- SEC-1: per-app scoping of the tx / savepoint / emit slots

    // A worker thread multiplexes up to ~200 isolates (one per app).
    // Every slot below used to be a single per-OS-thread cell shared by
    // ALL co-resident apps: app B could observe and drain app A's
    // parked transaction client (running B's SQL inside A's
    // transaction, snapshot, and per-app role), corrupt A's savepoint
    // bookkeeping, and drain A's pre-commit broker queue. These tests
    // pin the per-app ownership contract.

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
        let backend =
            crate::backend_selection::new_sqlite_backend(std::path::PathBuf::from(dir.path()))
                .expect("open sqlite backend");
        let client = backend
            .acquire_dedicated_client("slot_state_probe")
            .await
            .expect("acquire sqlite client");
        TxConnection::Sqlite(client)
    }

    #[test]
    fn sec1_tx_parked_by_app_a_is_invisible_and_untakable_for_app_b() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut ctx = ThreadDbContext::new();
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
    fn sec1_frame_watermarks_are_scoped_per_app() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut ctx = ThreadDbContext::new();
            ctx.install_tx_client("app_a", sqlite_tx_conn(&dir).await);

            ctx.push_frame_emit_mark("app_a");
            ctx.push_frame_emit_mark("app_a");
            assert_eq!(
                ctx.pop_frame_emit_mark("app_b"),
                None,
                "SEC-1: app_b must not inherit app_a's frame watermarks \
                 (a shared stack lets one app truncate the other's queue)",
            );

            // app_b settling its own (nonexistent) transaction must not clobber
            // app_a's live frame bookkeeping.
            ctx.retire_transaction("app_b");
            assert_eq!(
                ctx.pop_frame_emit_mark("app_a"),
                Some(0),
                "SEC-1: app_b's settle must not drop app_a's frame watermarks",
            );
            assert_eq!(ctx.pop_frame_emit_mark("app_a"), Some(0));
            assert_eq!(ctx.pop_frame_emit_mark("app_a"), None);
        });
    }

    #[test]
    fn sec1_pending_emits_drain_is_scoped_per_app() {
        let mut ctx = ThreadDbContext::new();
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
}
