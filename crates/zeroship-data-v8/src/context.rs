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

#[cfg(any(test, feature = "test-helpers"))]
#[allow(unused_imports)]
use zeroship_data_orm::fixtures::DatabaseFixture;

use std::cell::RefCell;
use std::rc::Rc;

use crate::backend::{BackendHandle, PostgresBackend};
use crate::encryption::{LocalKeySource, SuppliedRootKeys};
use crate::service::DbResourceKey;

/// Result of trying to claim the per-thread backend initialisation slot.
pub(crate) enum BackendInitState {
    /// A backend is already installed; no work needed.
    Ready,
    /// This caller claimed the slot and must call `finish_backend_init`.
    Acquired,
    /// Another request is currently building the backend.
    InProgress,
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
    /// Database URL — poisoned during `register()`, consumed on first
    /// pool creation.
    db_url: Option<String>,

    /// Authenticated relay configuration inherited from process composition.
    cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,

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
            db_url: None,
            cdc_relay: None,
            resource_key: DbResourceKey::UNBOUND,
            backend_selection: None,
            backend: None,
            backend_init_in_progress: false,
            supplied_root_keys: None,
        }
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

    // ----- DB_POOL ----------------------------------------------------

    /// True iff a Postgres pool has been installed.
    ///
    /// Derived from the backend slot rather than from a second field, since
    /// there is no longer a second field to ask - see [`Self::set_pool`].
    ///
    /// `cfg(test)` alone, NOT `any(test, test-helpers)`: no integration target
    /// uses it (checked across `tests/` and `benches/`), so the wider gate
    /// compiled it into the `test-helpers` lib with no caller and raised the
    /// very dead_code warning it was added to remove.
    ///
    /// **`matches!` is correct here and stays**, unlike
    /// `exec::backend_publishes_committed_changes`, which was the same spelling
    /// and had to become an exhaustive `match` on 2026-09-04. The difference is
    /// what is being asked. This asks "is the installed arm the Postgres one",
    /// a question whose subject is one named variant; a third backend answers
    /// `false` and that answer is right without anybody deciding it. That one
    /// asked a per-backend CAPABILITY question wearing a variant test as a
    /// disguise, where `false` for an unwritten backend is an assumption.
    /// Making this exhaustive would only conscript a future author into
    /// writing an arm with exactly one possible value.
    #[cfg(test)]
    pub(crate) fn pool_initialised(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(|backend| backend.get::<crate::backend::PostgresBackend>().is_some())
    }

    /// Install an already-connected Postgres backend, wrapped in the
    /// [`BackendHandle::Postgres`] arm. The exact peer of
    /// [`Self::set_sqlite_backend`], and it did not used to be.
    ///
    /// **The pool is not a parameter, and not a field.** Until 2026-09-02 this
    /// was `set_pool(Rc<compio_postgres::Pool>)`: it connected nothing but took
    /// the driver's pool, built the facade here, and ALSO stored the pool in a
    /// second slot - the very `Rc` it had just cloned into `PostgresBackend`. So
    /// one pool lived in two places, and the adapter named a vendor type in a
    /// signature, which is the conflict recorded as #166.
    ///
    /// Both halves are gone. `PostgresBackend::connect` owns the connect inside
    /// the vendor crate, which is the only tier permitted to name
    /// `compio_postgres::Pool` - pushing the composer UP into the engine instead
    /// was tried the same day and refused by `tests/vendor_embedding_gate.sh`.
    pub(crate) fn set_postgres_backend(&mut self, backend: Rc<PostgresBackend>) {
        self.backend = Some(BackendHandle::new(backend));
    }

    /// Drop the cached pool (e.g. when the URL changes on
    /// `register`). The next CRUD call will re-`init_pool_async`
    /// against the new URL.
    pub(crate) fn clear_pool(&mut self) {
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
        // Replacing the backend IS dropping the Postgres pool: the pool has
        // no slot of its own, so switching arms cannot leave one behind.
        self.backend = Some(BackendHandle::new(backend));
    }

    /// Clone the registered backend handle without reopening the database.
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
    ///
    /// TEST-ONLY for the same reason as [`Self::pool_initialised`]: the key is
    /// SET on every URL install and read back only by the tests that assert two
    /// URLs land on different keys. Production reads it off `service`, not here.
    #[cfg(test)]
    pub(crate) fn resource_key(&self) -> DbResourceKey {
        self.resource_key
    }

    /// The backend the service selected for this thread's URL. `None` when no
    /// database is configured - the "DB plugin disabled" state.
    pub(crate) fn backend_selection(&self) -> Option<crate::BackendUrl> {
        self.backend_selection.clone()
    }

    /// The registered driver determines query syntax. Before initialization,
    /// derive it from the built-in URL selection.
    pub(crate) fn sql_dialect(&self) -> zeroship_data_sql::compile::SqlDialect {
        use zeroship_data_sql::compile::SqlDialect;
        match &self.backend {
            Some(backend) => backend.dialect(),
            None => match &self.backend_selection {
                Some(crate::BackendUrl::Sqlite { .. }) => SqlDialect::Sqlite,
                Some(crate::BackendUrl::Postgres) | None => SqlDialect::Postgres,
            },
        }
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

    /// Clone the authenticated relay configuration installed for this isolate.
    pub(crate) fn cdc_relay(&self) -> Option<zeroship_data_orm::cdc::relay::RelayConfig> {
        self.cdc_relay.clone()
    }

    pub(crate) fn set_cdc_relay(&mut self, relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>) {
        self.cdc_relay = relay;
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
    // The lane types are imported HERE, not at file scope. The lanes left this
    // module on 2026-09-02 and only its tests still name them, so a file-scope
    // import would be unused in the lib build and warn - while `--all-targets`
    // reports that warning even though the test target needs the import, which
    // is how deleting it "to fix a warning" turned into nine compile errors.
    use crate::tx_lanes::TxLanes;
    use std::collections::HashMap;
    use zeroship_data_orm::cdc::{ChangeEvent, ChangeOp};
    use zeroship_data_orm::driver::Session;

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
    fn new_yields_fully_cleared_adapter_slots() {
        let ctx = ThreadDbContext::new();
        assert!(!ctx.pool_initialised());
        assert!(ctx.backend().is_none());
        assert!(ctx.db_url().is_none());
    }

    /// The lane half of what `new_yields_fully_cleared_slots` used to assert.
    /// It split when the lanes became their own owner with their own
    /// thread-local: "a fresh thread has nothing open" is now two statements
    /// about two objects, and asserting them together would hide either one
    /// regressing alone.
    #[test]
    fn new_yields_fully_cleared_lane_slots() {
        let lanes = TxLanes::new();
        assert!(!lanes.has_tx_for("a"));
        assert!(lanes.transaction_reducer("a").is_none());
        assert!(!lanes.tx_session_withdrawn("a"));
        // pending_emits starts empty (each app's queue is allocated lazily on
        // first push).
        assert!(
            lanes
                .by_app()
                .values()
                .all(|lane| lane.pending_emits().is_empty())
        );
    }

    #[test]
    fn default_matches_new() {
        let a = ThreadDbContext::default();
        let b = ThreadDbContext::new();
        // Compare observable state (no PartialEq on the struct).
        assert_eq!(a.pool_initialised(), b.pool_initialised());
        assert_eq!(a.db_url(), b.db_url());
        // The two lane assertions that used to sit here moved out with the
        // lanes: `TxLanes` has its own `Default` and its own test.
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
        // that the installer itself leaves the backend slot alone.
        let mut ctx = ThreadDbContext::new();
        install(&mut ctx, "postgres://a");
        assert!(ctx.backend().is_none());
        let _ = install(&mut ctx, "postgres://a"); // no-op
        assert!(ctx.backend().is_none());
    }

    // ----- clear_pool ----------------------------------------------------

    #[test]
    fn clear_pool_when_unset_is_noop() {
        let mut ctx = ThreadDbContext::new();
        ctx.clear_pool();
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
        let mut lanes = TxLanes::new();
        // No frame is open, so there is no watermark to pop.
        assert_eq!(
            lanes.pop_frame_emit_mark("app_t"),
            None,
            "no open frame yields no watermark"
        );

        // Discarding with no watermark on the stack must leave the buffer ALONE
        // rather than truncating to zero, which would drop the enclosing
        // frame's events. Over-publishing is a bug; silently dropping a
        // committed row's event is a worse one.
        lanes.push_pending_emit(dummy_event("c1"));
        lanes.discard_frame_effects("app_t");
        assert_eq!(
            lanes
                .by_app()
                .get("app_t")
                .map(|lane| lane.pending_emits().len()),
            Some(1),
            "a missing watermark must not discard the enclosing frame's events"
        );

        // A real watermark discards exactly the frame's own events: the mark is
        // taken when the frame opens, so everything queued after it is the
        // frame's and everything before it is the parent's.
        lanes.push_frame_emit_mark("app_t");
        lanes.push_pending_emit(dummy_event("c2"));
        lanes.discard_frame_effects("app_t");
        let kept = lanes.by_app().get("app_t").expect("lane").pending_emits();
        assert_eq!(kept.len(), 1, "truncate to the frame's watermark");
        assert_eq!(
            kept[0].collection, "c1",
            "the enclosing frame's event survives"
        );

        // `discard_frame_effects` does NOT pop: a rolled-back frame is not
        // closed until its RELEASE lands, and that is the call that pops.
        assert_eq!(lanes.pop_frame_emit_mark("app_t"), Some(1));
        assert_eq!(lanes.pop_frame_emit_mark("app_t"), None);
    }

    /// `retire_transaction` drops the watermarks that died with the frames.
    ///
    /// Leaving them would let the next transaction's first savepoint pop a
    /// stale mark and truncate that transaction's buffer to an unrelated
    /// length.
    #[test]
    fn retiring_a_transaction_drops_its_frame_watermarks() {
        let mut lanes = TxLanes::new();
        lanes.push_pending_emit(dummy_event("c1"));
        lanes.push_frame_emit_mark("app_t");
        lanes.retire_transaction("app_t");
        assert_eq!(
            lanes.pop_frame_emit_mark("app_t"),
            None,
            "a retired transaction leaves no watermark behind"
        );
    }

    // ----- PENDING_EMITS state machine -----------------------------------

    #[test]
    fn pending_emits_start_empty() {
        let lanes = TxLanes::new();
        assert!(
            lanes
                .by_app()
                .values()
                .all(|lane| lane.pending_emits().is_empty())
        );
    }

    #[test]
    fn push_pending_emit_allocates_slot_lazily() {
        // dummy_event tags app_id "app_t"; the queue keys on that.
        let mut lanes = TxLanes::new();
        assert!(
            lanes
                .by_app()
                .values()
                .all(|lane| lane.pending_emits().is_empty())
        );
        lanes.push_pending_emit(dummy_event("c1"));
        assert!(lanes.by_app().contains_key("app_t"));
        assert_eq!(
            lanes.by_app().get("app_t").unwrap().pending_emits().len(),
            1
        );
    }

    #[test]
    fn push_pending_emit_accumulates() {
        let mut lanes = TxLanes::new();
        lanes.push_pending_emit(dummy_event("c1"));
        lanes.push_pending_emit(dummy_event("c2"));
        lanes.push_pending_emit(dummy_event("c3"));
        let evs = lanes.by_app().get("app_t").unwrap().pending_emits();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].collection, "c1");
        assert_eq!(evs[1].collection, "c2");
        assert_eq!(evs[2].collection, "c3");
    }

    #[test]
    fn drain_pending_emits_returns_and_clears() {
        let mut lanes = TxLanes::new();
        lanes.push_pending_emit(dummy_event("c1"));
        lanes.push_pending_emit(dummy_event("c2"));
        let drained = lanes.drain_pending_emits_for("app_t");
        assert_eq!(drained.len(), 2);
        // After drain the app's queue is cleared — subsequent pushes
        // re-allocate.
        assert!(
            lanes
                .by_app()
                .get("app_t")
                .is_none_or(|lane| lane.pending_emits().is_empty())
        );
    }

    #[test]
    fn drain_pending_emits_on_empty_returns_empty_vec() {
        let mut lanes = TxLanes::new();
        let drained = lanes.drain_pending_emits_for("app_t");
        assert!(drained.is_empty());
        assert!(
            lanes
                .by_app()
                .values()
                .all(|lane| lane.pending_emits().is_empty())
        );
    }

    #[test]
    fn drain_then_push_starts_fresh() {
        let mut lanes = TxLanes::new();
        lanes.push_pending_emit(dummy_event("c1"));
        let _ = lanes.drain_pending_emits_for("app_t");
        lanes.push_pending_emit(dummy_event("c2"));
        let evs = lanes.by_app().get("app_t").unwrap().pending_emits();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].collection, "c2");
    }

    #[test]
    fn clear_pending_emits_drops_without_returning() {
        let mut lanes = TxLanes::new();
        lanes.push_pending_emit(dummy_event("c1"));
        lanes.push_pending_emit(dummy_event("c2"));
        lanes.clear_pending_emits_for("app_t");
        assert!(
            lanes
                .by_app()
                .get("app_t")
                .is_none_or(|lane| lane.pending_emits().is_empty())
        );
        // A subsequent drain returns empty (queue is gone).
        assert!(lanes.drain_pending_emits_for("app_t").is_empty());
    }

    #[test]
    fn clear_pending_emits_on_empty_is_idempotent() {
        let mut lanes = TxLanes::new();
        lanes.clear_pending_emits_for("app_t");
        lanes.clear_pending_emits_for("app_t");
        assert!(
            lanes
                .by_app()
                .values()
                .all(|lane| lane.pending_emits().is_empty())
        );
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

    /// Build a parkable [`Session`] without a live Postgres: the
    /// embedded SQLite backend hands out a real session handle from a
    /// tempdir-backed store. No SQL is executed on it — these tests
    /// exercise the slot state machine only.
    async fn sqlite_tx_conn(dir: &tempfile::TempDir) -> Session {
        use zeroship_data_orm::fixtures::DatabaseFixture;
        // The key source is a parameter now: the engine composer cannot read
        // this crate's per-isolate context, so the caller that owns it does the
        // lookup. This is the one adapter-side caller.
        let backend = crate::backend_selection::new_sqlite_backend(
            std::path::PathBuf::from(dir.path()),
            isolate_key_source(),
        )
        .expect("open sqlite backend");
        let client = backend
            .fixture_session("slot_state_probe")
            .await
            .expect("acquire sqlite client");
        Session::new(client)
    }

    #[test]
    fn sec1_tx_parked_by_app_a_is_invisible_and_untakable_for_app_b() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut lanes = TxLanes::new();
            let prev = lanes.install_tx_client("app_a", sqlite_tx_conn(&dir).await);
            assert!(prev.is_none(), "tx slot must start empty");

            assert!(
                lanes.has_tx_for("app_a"),
                "the owning app must see its own parked tx",
            );
            assert!(
                !lanes.has_tx_for("app_b"),
                "SEC-1: app_b must NOT observe app_a's parked tx \
                 (a hit here routes app_b's SQL onto app_a's tx connection)",
            );
            assert!(
                lanes.take_tx_client_for("app_b").is_none(),
                "SEC-1: app_b must NOT be able to drain app_a's tx client",
            );
            assert!(
                lanes.has_tx_for("app_a"),
                "app_a's parked tx must survive app_b's probe unmodified",
            );
            // The owner can still take its own client back out.
            assert!(
                lanes.take_tx_client_for("app_a").is_some(),
                "the owner must still be able to take its own tx client",
            );
        });
    }

    #[test]
    fn sec1_frame_watermarks_are_scoped_per_app() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut lanes = TxLanes::new();
            lanes.install_tx_client("app_a", sqlite_tx_conn(&dir).await);

            lanes.push_frame_emit_mark("app_a");
            lanes.push_frame_emit_mark("app_a");
            assert_eq!(
                lanes.pop_frame_emit_mark("app_b"),
                None,
                "SEC-1: app_b must not inherit app_a's frame watermarks \
                 (a shared stack lets one app truncate the other's queue)",
            );

            // app_b settling its own (nonexistent) transaction must not clobber
            // app_a's live frame bookkeeping.
            lanes.retire_transaction("app_b");
            assert_eq!(
                lanes.pop_frame_emit_mark("app_a"),
                Some(0),
                "SEC-1: app_b's settle must not drop app_a's frame watermarks",
            );
            assert_eq!(lanes.pop_frame_emit_mark("app_a"), Some(0));
            assert_eq!(lanes.pop_frame_emit_mark("app_a"), None);
        });
    }

    #[test]
    fn sec1_pending_emits_drain_is_scoped_per_app() {
        let mut lanes = TxLanes::new();
        let mut ev_a = dummy_event("orders");
        ev_a.app_id = "app_a".to_string();
        let mut ev_b = dummy_event("messages");
        ev_b.app_id = "app_b".to_string();
        lanes.push_pending_emit(ev_a);
        lanes.push_pending_emit(ev_b);

        let drained_b = lanes.drain_pending_emits_for("app_b");
        assert_eq!(
            drained_b.len(),
            1,
            "SEC-1: app_b's commit drain must only fire app_b's queued events",
        );
        assert_eq!(drained_b[0].app_id, "app_b");

        let drained_a = lanes.drain_pending_emits_for("app_a");
        assert_eq!(
            drained_a.len(),
            1,
            "SEC-1: app_a's queued events must survive app_b's drain \
             (firing them early breaks the Gap-B pre-commit fence)",
        );
        assert_eq!(drained_a[0].app_id, "app_a");
    }
}
