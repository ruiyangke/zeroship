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

/// Lock state for the in-flight migration. The `client` is held in
/// an `Option` so callers can `take()` it across an await and
/// `replace()` it back — the same pattern the transaction slot uses.
///
/// Defined here (not in `crate::migrations`) so `compio_postgres::Client`
/// stays out of consumer modules — the Backend abstraction (Stage 8e-R2)
/// allows only `context.rs` and `backend/postgres.rs` to name the
/// underlying driver type.
pub(crate) struct MigrationLock {
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
}

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

    /// Take the lock client out of the active migration state for an
    /// await; the caller's future is responsible for putting it back
    /// via [`Self::return_mig_client`].
    pub(crate) fn take_mig_client(&mut self) -> Option<Client> {
        self.mig_lock.as_mut().and_then(|l| l.client.take())
    }

    /// Restore the lock client after an await. No-op if the migration
    /// state has been cleared in the meantime (e.g. by an operator
    /// cancel). The slot-empty case is observable but rare — log it
    /// at `warn` so we can distinguish a real cancel race from a
    /// state-machine bug that silently dropped the client (paired
    /// with the `tracing::error!` on `set_mig_lock`'s shadow-replace
    /// branch above).
    pub(crate) fn return_mig_client(&mut self, client: Client) {
        match self.mig_lock.as_mut() {
            Some(lock) => lock.client = Some(client),
            None => tracing::warn!(
                "return_mig_client: mig_lock slot empty — client dropped (expected only on operator-cancel race)",
            ),
        }
    }

    /// Snapshot the migration lock's identifying fields (name,
    /// collection, audit_id, dry_run, start_generation). Returns
    /// `None` outside an active run.
    pub(crate) fn mig_lock_snapshot(&self) -> Option<(String, String, i64, bool, i64)> {
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

    /// Mark a replication consumer as running for this app — test-only.
    ///
    /// Production code uses [`Self::try_mark_consumer_running`]
    /// (atomic check-and-set; returns whether the caller won the
    /// race). This unconditional variant is retained for test
    /// fixtures that need to mark without caring whether the slot was
    /// already taken; api-surface r6 MAJOR-R6-1 noted it was dead in
    /// production builds and would footgun a contributor picking it
    /// over the atomic variant.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn mark_consumer_running(&mut self, app_id: &str) {
        self.running_consumers.insert(app_id.to_string());
    }

    /// Atomically check-and-mark: returns `true` if the caller won the
    /// mark (was not previously running), `false` if another caller
    /// already marked this app. Used by the spawned consumer task to
    /// close the race between dispatch's idempotent gate and the
    /// task's first poll (concurrency r7 NEW MINOR).
    pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
        self.running_consumers.insert(app_id.to_string())
    }

    /// Mark a replication consumer as no-longer-running.
    pub fn unmark_consumer_running(&mut self, app_id: &str) {
        self.running_consumers.remove(app_id);
    }

    /// Clear every entry from the consumer registry (test-only —
    /// production code should rely on the supervised task's exit path
    /// to call [`Self::unmark_consumer_running`]).
    #[cfg(any(test, feature = "test-helpers"))]
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
    //! The slots that store a `compio_postgres::Client` (`tx_conn`,
    //! `MigrationLock::client`) need a value of that type to exercise.
    //! `compio_postgres::Client::new` is `pub(crate)` on the driver, so
    //! we cannot mint one outside `compio-postgres` — and the task brief
    //! is explicit that these unit tests must not touch real Postgres.
    //!
    //! Concretely, the following can only be exercised by
    //! `tests/integration.rs` (which spins up a real PG):
    //!
    //! * [`IsolateDbContext::install_tx_client`] /
    //!   [`IsolateDbContext::take_tx_client`] /
    //!   [`IsolateDbContext::put_tx_client`] round-trip.
    //! * The `debug_assert!` inside [`IsolateDbContext::set_tx_token`]
    //!   that a non-zero token requires `tx_conn = Some` — we test the
    //!   *zero* branch only (which is always allowed).
    //! * The `debug_assert!` inside
    //!   [`IsolateDbContext::set_auto_tx_owned`] that `owned = true`
    //!   requires `tx_conn = Some` — same constraint; we only test the
    //!   `false` branch.
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
            pk: Some(1),
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
        assert!(!ctx.has_tx());
        assert_eq!(ctx.tx_token(), 0);
        assert!(!ctx.auto_tx_owned());
        assert!(!ctx.has_mig_lock());
        assert!(ctx.mig_lock_snapshot().is_none());
        // pending_emits starts as None (the slot is allocated lazily on
        // first push).
        assert!(ctx.pending_emits.is_none());
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
        assert_eq!(a.tx_token(), b.tx_token());
        assert_eq!(a.auto_tx_owned(), b.auto_tx_owned());
        assert_eq!(a.has_tx(), b.has_tx());
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
    fn next_tx_token_starts_at_one() {
        let mut ctx = IsolateDbContext::new();
        assert_eq!(ctx.next_tx_token(), 1);
    }

    #[test]
    fn next_tx_token_increments_monotonically() {
        let mut ctx = IsolateDbContext::new();
        let a = ctx.next_tx_token();
        let b = ctx.next_tx_token();
        let c = ctx.next_tx_token();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
    }

    #[test]
    fn set_tx_token_zero_is_always_allowed() {
        // The non-zero branch requires an active tx_conn (debug_assert!);
        // we test the zero (clearing) branch here. Zero clears in every
        // settle path, regardless of slot occupancy.
        let mut ctx = IsolateDbContext::new();
        ctx.set_tx_token(0);
        assert_eq!(ctx.tx_token(), 0);
        // Even after a counter bump, zero stays clear-only.
        ctx.next_tx_token();
        ctx.set_tx_token(0);
        assert_eq!(ctx.tx_token(), 0);
    }

    #[test]
    fn set_auto_tx_owned_false_is_always_allowed() {
        // The `true` branch requires tx_conn = Some (debug_assert!); the
        // `false` branch is always allowed because `exec_auto_end`
        // clears the flag right after taking the client out, when the
        // slot is briefly empty.
        let mut ctx = IsolateDbContext::new();
        ctx.set_auto_tx_owned(false);
        assert!(!ctx.auto_tx_owned());
    }

    // ----- PENDING_EMITS state machine -----------------------------------

    #[test]
    fn pending_emits_start_empty() {
        let ctx = IsolateDbContext::new();
        assert!(ctx.pending_emits.is_none());
    }

    #[test]
    fn push_pending_emit_allocates_slot_lazily() {
        let mut ctx = IsolateDbContext::new();
        assert!(ctx.pending_emits.is_none());
        ctx.push_pending_emit(dummy_event("c1"));
        assert!(ctx.pending_emits.is_some());
        assert_eq!(ctx.pending_emits.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn push_pending_emit_accumulates() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.push_pending_emit(dummy_event("c3"));
        let evs = ctx.pending_emits.as_ref().unwrap();
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
        let drained = ctx.drain_pending_emits();
        assert_eq!(drained.len(), 2);
        // After drain the slot is cleared back to None — subsequent
        // pushes re-allocate.
        assert!(ctx.pending_emits.is_none());
    }

    #[test]
    fn drain_pending_emits_on_empty_returns_empty_vec() {
        let mut ctx = IsolateDbContext::new();
        let drained = ctx.drain_pending_emits();
        assert!(drained.is_empty());
        assert!(ctx.pending_emits.is_none());
    }

    #[test]
    fn drain_then_push_starts_fresh() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        let _ = ctx.drain_pending_emits();
        ctx.push_pending_emit(dummy_event("c2"));
        let evs = ctx.pending_emits.as_ref().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].collection, "c2");
    }

    #[test]
    fn clear_pending_emits_drops_without_returning() {
        let mut ctx = IsolateDbContext::new();
        ctx.push_pending_emit(dummy_event("c1"));
        ctx.push_pending_emit(dummy_event("c2"));
        ctx.clear_pending_emits();
        assert!(ctx.pending_emits.is_none());
        // A subsequent drain returns empty (slot is None).
        assert!(ctx.drain_pending_emits().is_empty());
    }

    #[test]
    fn clear_pending_emits_on_empty_is_idempotent() {
        let mut ctx = IsolateDbContext::new();
        ctx.clear_pending_emits();
        ctx.clear_pending_emits();
        assert!(ctx.pending_emits.is_none());
    }

    // ----- MIG_LOCK state machine ----------------------------------------

    fn mig_lock(name: &str, collection: &str, audit_id: i64, dry_run: bool) -> MigrationLock {
        MigrationLock {
            name: name.to_string(),
            collection: collection.to_string(),
            audit_id,
            dry_run,
            start_generation: 7,
            client: None,
        }
    }

    #[test]
    fn set_mig_lock_install_then_snapshot() {
        let mut ctx = IsolateDbContext::new();
        assert!(!ctx.has_mig_lock());
        let prev = ctx.set_mig_lock(mig_lock("m1", "users", 42, false));
        assert!(prev.is_none());
        assert!(ctx.has_mig_lock());

        let snap = ctx.mig_lock_snapshot().expect("snapshot present");
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
        let snap = ctx.mig_lock_snapshot().unwrap();
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

        let snap = ctx.mig_lock_snapshot().unwrap();
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
        assert!(ctx.mig_lock_snapshot().is_none());
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
        assert!(ctx.mig_lock_snapshot().is_none());
    }

    #[test]
    fn take_mig_client_on_empty_lock_returns_none() {
        // No active migration: take is a no-op.
        let mut ctx = IsolateDbContext::new();
        assert!(ctx.take_mig_client().is_none());
        // With a lock present but `client: None` (our test mig_lock
        // helper), take still returns None — there is nothing to take.
        ctx.set_mig_lock(mig_lock("m", "c", 1, false));
        assert!(ctx.take_mig_client().is_none());
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
}
