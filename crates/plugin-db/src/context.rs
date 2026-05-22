//! Per-isolate DB context — single typed home for every plug-in
//! thread-local. Before Stage 8d-R4 the plug-in carried ten separate
//! `thread_local!` declarations (`DB_POOL`, `DB_URL`, `REGISTERED_MODELS`,
//! `TX_CONN`, `AUTO_TX_OWNED`, `TX_TOKEN`, `TX_TOKEN_COUNTER`,
//! `PENDING_EMITS` in `lib.rs`; `MIG_LOCK` in `migrations.rs`;
//! `RUNNING_CONSUMERS` in `replication_ops.rs`). Each had its own
//! borrow/take/replace ritual; lifecycle invariants (e.g. "TX_TOKEN
//! matches the live transaction's token" or "MIG_LOCK never holds two
//! `MigrationLock` snapshots") were enforced by convention only.
//!
//! This module folds all of those slots into a single
//! [`IsolateDbContext`] stashed in one [`thread_local!`]. Typed
//! accessors enforce the invariants in one place:
//!
//! * [`IsolateDbContext::with`] / [`IsolateDbContext::with_mut`] are
//!   the only entry points; every consumer goes through them.
//! * `*_tx_*` methods coordinate the four tx-state slots (`tx_conn`,
//!   `tx_token`, `tx_token_counter`, `auto_tx_owned`) so that the
//!   TX_TOKEN-versus-Drop race documented on
//!   [`crate::v8_classes::transaction::Transaction`] still holds.
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
use std::collections::HashSet;
use std::rc::Rc;

use compio_postgres::{Client, Pool};

use crate::backend::PostgresBackend;
use crate::broker::ChangeEvent;
use crate::migrations::MigrationLock;

/// Per-isolate DB plug-in state. One instance per worker thread, held
/// by the [`ISOLATE_CTX`] thread-local.
#[allow(missing_debug_implementations)]
pub struct IsolateDbContext {
    /// Connection pool — created lazily on first DB operation.
    pub(crate) pool: Option<Rc<Pool>>,

    /// Database URL — poisoned during `register()`, consumed on first
    /// pool creation.
    pub(crate) db_url: Option<String>,

    /// Registered models — keyed by "app_id:collection". Prevents
    /// redundant DDL on subsequent cold starts within the same deploy.
    pub(crate) registered_models: HashSet<String>,

    /// Active transaction connection. Only one transaction at a time
    /// per isolate (V8 is single-threaded). If `Some`, all CRUD ops
    /// route through this connection instead of the pool.
    ///
    /// We hold a raw [`Client`] (not a `compio_postgres::Transaction`)
    /// because the lifetime of `Transaction<'a>` is tied to its parent
    /// `Client` — which cannot live inside a thread-local. Instead we
    /// issue `BEGIN`/`COMMIT`/`ROLLBACK` via `client.execute(...)`
    /// directly.
    pub(crate) tx_conn: Option<Client>,

    /// True when the active [`Self::tx_conn`] was opened by the
    /// auto-tx wrapper (`__zsBeginAutoTx`) — defense-in-depth
    /// read-only/serializable envelope around `query()`/`mutation()`
    /// handlers.
    ///
    /// User-driven `db.transaction(async tx => {...})` calls leave
    /// this `false`, so the auto-tx end callback never touches a
    /// user-owned tx.
    pub(crate) auto_tx_owned: bool,

    /// Ownership token for the active transaction connection.
    ///
    /// Stamped non-zero by `begin_transaction` on success and cleared
    /// to zero by any path that drains [`Self::tx_conn`] (the
    /// `Transaction` v8_class's `.commit()` / `.rollback()` methods,
    /// or its Weak-finalizer-driven `Drop`).
    ///
    /// Each `Transaction` wrapper carries the token it was minted
    /// with; commit / rollback / GC all compare against the live
    /// `tx_token` before acting, so the wrapper never double-acts on
    /// a transaction another path already settled (e.g. an explicit
    /// `.commit()` followed by the finalizer running on GC).
    pub(crate) tx_token: u64,

    /// Monotonic counter feeding [`Self::tx_token`]. Incremented
    /// inside [`Self::next_tx_token`]; never reset (a u64 at 1 GHz
    /// tx/s would take ~584 years to wrap, so non-uniqueness within a
    /// worker lifetime is a non-issue).
    pub(crate) tx_token_counter: u64,

    /// Broker events queued during an active transaction.
    ///
    /// While [`Self::tx_conn`] is `Some`, every successful CRUD
    /// mutation pushes its `ChangeEvent` here instead of calling
    /// [`crate::wal_consumer::emit_local`] directly. The transaction
    /// settle path (`Transaction::end` for user-driven tx;
    /// `exec_auto_end` for the auto-tx wrapper) drains the queue and
    /// either fires every event through `emit_local` on COMMIT or
    /// clears it on ROLLBACK. This closes the "emit-before-commit"
    /// dual-write window where a subscriber could `find()` rows that
    /// don't yet exist on disk (or that a ROLLBACK is about to
    /// undo).
    ///
    /// `None` outside a transaction; non-empty `Some(Vec<_>)` only
    /// while a tx is active. Drained atomically by `Vec::take`.
    pub(crate) pending_emits: Option<Vec<ChangeEvent>>,

    /// Active migration owner state. `Some` after a successful
    /// `migrationBegin`; `None` once `migrationCommitBatch` with
    /// `isDone=true` (or `migrationCancel` on the owner thread)
    /// clears it. Single-isolate invariant — only one migration may
    /// be active per V8 thread at a time (mirrors [`Self::tx_conn`]).
    pub(crate) mig_lock: Option<MigrationLock>,

    /// Per-thread "is the consumer already running for this app?"
    /// guard. Keyed by app_id (a single worker may host multiple
    /// apps over its lifetime via the LRU cache, but only one
    /// consumer per app at a time).
    pub(crate) running_consumers: HashSet<String>,

    /// Backend handle wrapping the pool — Stage 8e-R2. Created
    /// alongside the pool by [`Self::set_pool`] so consumers can call
    /// `ctx.backend()` to get a [`crate::backend::Backend`] facade
    /// without naming `compio_postgres::Pool` directly.
    ///
    /// `None` until the pool is initialised; same lifecycle as
    /// [`Self::pool`] (cleared whenever the pool is cleared).
    pub(crate) backend: Option<Rc<PostgresBackend>>,
}

impl IsolateDbContext {
    /// Build a fresh per-isolate context. Called once per worker
    /// thread on first access (via [`ISOLATE_CTX`]'s `const`
    /// initialiser path is too restrictive for `HashSet::new`, so the
    /// `RefCell` is initialised lazily through `std::cell::RefCell::new`
    /// in the `thread_local!` body).
    #[must_use]
    pub fn new() -> Self {
        Self {
            pool: None,
            db_url: None,
            registered_models: HashSet::new(),
            tx_conn: None,
            auto_tx_owned: false,
            tx_token: 0,
            tx_token_counter: 0,
            pending_emits: None,
            mig_lock: None,
            running_consumers: HashSet::new(),
            backend: None,
        }
    }

    // ----- DB_POOL ----------------------------------------------------

    /// Snapshot the pool handle (cloned `Rc`).
    pub fn pool(&self) -> Option<Rc<Pool>> {
        self.pool.as_ref().map(Rc::clone)
    }

    /// True iff the pool has been initialised.
    pub fn pool_initialised(&self) -> bool {
        self.pool.is_some()
    }

    /// Install the pool — called by `init_pool_async` once Postgres
    /// `connect` succeeds. Constructs the [`PostgresBackend`] facade
    /// in lockstep so the two never drift.
    pub fn set_pool(&mut self, pool: Rc<Pool>) {
        let url = self.db_url.clone().unwrap_or_default();
        self.backend = Some(Rc::new(PostgresBackend::new(Rc::clone(&pool), url)));
        self.pool = Some(pool);
    }

    /// Drop the cached pool (e.g. when the URL changes on
    /// `register`). The next CRUD call will re-`init_pool_async`
    /// against the new URL.
    pub fn clear_pool(&mut self) {
        self.pool = None;
        self.backend = None;
    }

    /// Snapshot the backend facade (cloned `Rc`).
    pub fn backend(&self) -> Option<Rc<PostgresBackend>> {
        self.backend.as_ref().map(Rc::clone)
    }

    // ----- DB_URL -----------------------------------------------------

    /// Read the configured URL.
    pub fn db_url(&self) -> Option<String> {
        self.db_url.clone()
    }

    /// Poison the URL slot. Returns `true` iff the URL changed (the
    /// caller wants to drop the pool in that case so the next CRUD
    /// call rebuilds it).
    pub fn set_db_url(&mut self, url: &str) -> bool {
        let different = self.db_url.as_deref() != Some(url);
        if different {
            self.db_url = Some(url.to_string());
        }
        different
    }

    // ----- REGISTERED_MODELS -----------------------------------------

    /// Check whether the model has been registered on this isolate.
    pub fn is_model_registered(&self, app_id: &str, collection: &str) -> bool {
        let key = format!("{app_id}:{collection}");
        self.registered_models.contains(&key)
    }

    /// Mark the model as registered (idempotent).
    pub fn mark_model_registered(&mut self, app_id: &str, collection: &str) {
        let key = format!("{app_id}:{collection}");
        self.registered_models.insert(key);
    }

    // ----- TX_CONN / TX_TOKEN / TX_TOKEN_COUNTER / AUTO_TX_OWNED ------

    /// `true` if a transaction connection is currently parked in the
    /// slot (`tx_conn = Some`). Note: returns `true` even between an
    /// in-flight take/return on the same Client (`take_tx_client` →
    /// `put_tx_client`), because callers wrap the await in those two
    /// calls and the slot is conceptually still "active". See
    /// [`Self::has_tx`] for the conservative caller-facing predicate.
    pub fn has_tx(&self) -> bool {
        self.tx_conn.is_some()
    }

    /// Park a connection in the transaction slot. Returns the
    /// previous occupant, if any (callers should ensure this is `None`
    /// — every begin path checks [`Self::has_tx`] first).
    pub fn install_tx_client(&mut self, client: Client) -> Option<Client> {
        self.tx_conn.replace(client)
    }

    /// Take the transaction client out of the slot. The caller must
    /// either return it via [`Self::put_tx_client`] (when the await
    /// is short and the slot should remain "in transaction") or drop
    /// the client (when settling the tx).
    pub fn take_tx_client(&mut self) -> Option<Client> {
        self.tx_conn.take()
    }

    /// Return a client previously taken via [`Self::take_tx_client`].
    pub fn put_tx_client(&mut self, client: Client) {
        self.tx_conn = Some(client);
    }

    /// Read the live ownership token (zero outside a transaction).
    pub fn tx_token(&self) -> u64 {
        self.tx_token
    }

    /// Stamp the live ownership token. Called by the begin path after
    /// the BEGIN SQL succeeds; cleared to zero by every settle path.
    ///
    /// Invariant: a non-zero token implies the tx_conn slot is
    /// occupied — every settle path drains the client BEFORE
    /// clearing the token.
    pub fn set_tx_token(&mut self, token: u64) {
        debug_assert!(
            token == 0 || self.tx_conn.is_some(),
            "set_tx_token: non-zero token without an active tx_conn",
        );
        self.tx_token = token;
    }

    /// Allocate a fresh non-zero TX_TOKEN value. Called by
    /// `orchestrator::transaction::begin_transaction_dispatch` right
    /// before stamping the token onto the freshly-minted
    /// `Transaction` wrapper.
    pub fn next_tx_token(&mut self) -> u64 {
        self.tx_token_counter = self.tx_token_counter.wrapping_add(1);
        self.tx_token_counter
    }

    /// True iff the live transaction was opened by the auto-tx
    /// wrapper (vs. a user-driven `db.beginTransaction`).
    pub fn auto_tx_owned(&self) -> bool {
        self.auto_tx_owned
    }

    /// Mark the live transaction as auto-tx-owned (or clear the
    /// flag).
    ///
    /// Invariant: flipping `owned = true` requires an active
    /// transaction connection. The `false` path is always allowed —
    /// `exec_auto_end` clears the flag right after taking the client
    /// out, so the slot is briefly `None` while the flag is also
    /// being cleared.
    pub fn set_auto_tx_owned(&mut self, owned: bool) {
        debug_assert!(
            !owned || self.tx_conn.is_some(),
            "set_auto_tx_owned(true) called without an active tx_conn",
        );
        self.auto_tx_owned = owned;
    }

    // ----- PENDING_EMITS ---------------------------------------------

    /// Push a `ChangeEvent` onto the pending-emits queue (initialises
    /// the slot to `Some(Vec::new())` on first push within a tx).
    pub fn push_pending_emit(&mut self, ev: ChangeEvent) {
        self.pending_emits.get_or_insert_with(Vec::new).push(ev);
    }

    /// Drain the pending-emits queue (returns `Vec::new()` if the
    /// slot was empty). Called by the transaction settle path on
    /// COMMIT.
    pub fn drain_pending_emits(&mut self) -> Vec<ChangeEvent> {
        self.pending_emits.take().unwrap_or_default()
    }

    /// Clear the pending-emits queue without firing any events.
    /// Called by the transaction settle path on ROLLBACK and by
    /// `exec_begin` to drop any stale residue from an interrupted
    /// prior run.
    pub fn clear_pending_emits(&mut self) {
        self.pending_emits = None;
    }

    // ----- MIG_LOCK ---------------------------------------------------

    /// True iff a migration run is active on this isolate.
    pub fn has_mig_lock(&self) -> bool {
        self.mig_lock.is_some()
    }

    /// Install a fresh migration lock state. Returns the previous
    /// state if any (callers should ensure this is `None` — every
    /// begin path checks [`Self::has_mig_lock`] first).
    pub(crate) fn set_mig_lock(&mut self, lock: MigrationLock) -> Option<MigrationLock> {
        self.mig_lock.replace(lock)
    }

    /// Drop the active migration lock state. Best-effort —
    /// idempotent.
    pub fn clear_mig_lock(&mut self) {
        self.mig_lock = None;
    }

    /// Take the lock client out of the active migration state for an
    /// await; the caller's future is responsible for putting it back
    /// via [`Self::return_mig_client`].
    pub fn take_mig_client(&mut self) -> Option<Client> {
        self.mig_lock.as_mut().and_then(|l| l.client.take())
    }

    /// Restore the lock client after an await. No-op if the migration
    /// state has been cleared in the meantime (e.g. by an operator
    /// cancel).
    pub fn return_mig_client(&mut self, client: Client) {
        if let Some(lock) = self.mig_lock.as_mut() {
            lock.client = Some(client);
        }
    }

    /// Snapshot the migration lock's identifying fields (name,
    /// collection, audit_id, dry_run, start_generation). Returns
    /// `None` outside an active run.
    pub fn mig_lock_snapshot(&self) -> Option<(String, String, i64, bool, i64)> {
        self.mig_lock.as_ref().map(|l| {
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
    pub fn is_consumer_running(&self, app_id: &str) -> bool {
        self.running_consumers.contains(app_id)
    }

    /// Mark a replication consumer as running for this app.
    pub fn mark_consumer_running(&mut self, app_id: &str) {
        self.running_consumers.insert(app_id.to_string());
    }

    /// Mark a replication consumer as no-longer-running.
    pub fn unmark_consumer_running(&mut self, app_id: &str) {
        self.running_consumers.remove(app_id);
    }

    /// Clear every entry from the consumer registry (test-only —
    /// production code should rely on the supervised task's exit path
    /// to call [`Self::unmark_consumer_running`]).
    pub fn clear_consumer_registry(&mut self) {
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
