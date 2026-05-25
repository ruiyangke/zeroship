//! PG-side [`crate::backend::ChangeStream`] adapter — the thin wrapper
//! that re-routes `replication::ensure_publication_and_slot` +
//! `wal_consumer::run_supervised` through the trait surface introduced
//! in **P2 PR 1**.
//!
//! Source plan: `docs/proposals/p2-sqlite-cdc-implementation-plan.md`
//! §2.5, §9 PR 1.
//!
//! **PR 1 is a mechanical refactor.** PG behaviour is unchanged: this
//! adapter wraps the same calls the orchestrator made directly from
//! `replication_ops.rs` before P2; the dispatcher just routes through
//! one extra fn. Every existing PG CDC test passes unchanged.
//!
//! **Why an adapter, not a direct `impl ChangeStream for PostgresBackend`**
//! — the `provision`/`deprovision` paths reach for the
//! `compio_postgres::Pool` plus the configured URL (the latter for
//! `WalConsumer::new(app_id, &url)`). Pulling those values is cleaner
//! through a borrowed-reference wrapper than through extra accessor
//! methods on `PostgresBackend`; it also keeps the `ChangeStream` impl
//! co-located with the WAL-consumer plumbing it routes to. The SQLite
//! peer (`crate::backend::sqlite::cdc::SqliteChangeStream`, PR 2 onward)
//! follows the same shape for the same reasons (session-actor
//! ownership lives next to the impl).

use std::rc::Rc;

use crate::backend::postgres::PostgresBackend;
use crate::backend::{BrokerPauseGuard, ChangeStream, SchemaPendingGuard};
use crate::error::DbError;

/// Handle returned by [`PgChangeStream::spawn_consumer`]. **PR 1**:
/// a unit struct — the existing
/// `compio::runtime::spawn(...).detach()` shape in
/// `replication_ops::start_replication_consumer_dispatch` does not
/// hand back a join handle (the supervisor exits cleanly on CopyDone
/// or a fatal error and there is nowhere to await it). The unit
/// struct exists so future PRs can swap in a real handle without
/// re-shaping the [`ChangeStream`] surface.
///
/// PR 4 may upgrade this to carry the `app_id` + a shutdown signal
/// so explicit teardown becomes possible (matching the SQLite-side
/// session-actor model). PR 1 ships the minimal shape that makes
/// the trait surface stable across both arms.
#[derive(Debug)]
pub struct WalConsumerHandle {
    /// The app id the consumer was spawned for. Carried so a future
    /// `shutdown(...)` extension knows which `wal_consumer::suppress_app`
    /// / supervisor-marker entry to clear without re-deriving it from
    /// the spawn closure's captures.
    _app_id: String,
}

/// PG arm of the [`ChangeStream`] capability.
///
/// Constructed via [`crate::backend::BackendHandle::as_change_stream_pg`].
/// Owns an `Rc<PostgresBackend>` (Rc-cloned from the
/// [`crate::backend::BackendHandle::Postgres`] arm) so the adapter
/// satisfies the trait's `'static` bound — `async fn`-in-trait under
/// today's compiler shapes futures with a `Self: 'static` requirement
/// on the impl, and a borrowed-reference form (`<'b> PgChangeStream<'b>`)
/// would force the async future to carry `'b` through every site.
/// Rc-clone is cheap (one pointer-increment) and pairs with the same
/// shape `BackendHandle` already uses.
///
/// **`#[allow(dead_code)]` on `backend`**: PR 1 only calls
/// `spawn_consumer` from the existing dispatcher (a no-op marker —
/// see the method's rustdoc) so `provision` (the only consumer of
/// `self.backend.pool()`) is dead from rustc's perspective until
/// PR 4 lands the migrations / register-model call sites. Removing
/// the allow once those callers exist.
pub struct PgChangeStream {
    #[allow(dead_code)]
    backend: Rc<PostgresBackend>,
}

impl std::fmt::Debug for PgChangeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Mirrors `PostgresBackend`'s opaque Debug impl — no field
        // exposure; the inner `Rc<PostgresBackend>` may carry the
        // configured DB URL which we don't want spilled to log lines.
        f.debug_struct("PgChangeStream").finish()
    }
}

impl PgChangeStream {
    /// Construct an adapter holding an Rc-clone of `backend`.
    /// Crate-private — the
    /// [`crate::backend::BackendHandle::as_change_stream_pg`] accessor
    /// is the public entry point.
    pub(crate) fn new(backend: Rc<PostgresBackend>) -> Self {
        Self { backend }
    }
}

impl ChangeStream for PgChangeStream {
    type ConsumerHandle = WalConsumerHandle;

    /// Idempotently provision the publication + logical replication
    /// slot for `app_id`. Routes through
    /// [`crate::replication::ensure_publication_and_slot`] — the same
    /// helper `replication_ops::replication_setup_dispatch` /
    /// `start_replication_consumer_dispatch` called directly before
    /// PR 1. Returns `Ok(())` on success; the typed `SetupOutcome`
    /// returned by the underlying helper is discarded here because
    /// the [`ChangeStream::provision`] contract is "provisioned" /
    /// "failed", not "what LSN are we at" (PR 4+ can re-introduce
    /// the LSN if any caller needs it).
    async fn provision(&self, app_id: &str) -> Result<(), DbError> {
        let _outcome =
            crate::replication::ensure_publication_and_slot(self.backend.pool(), app_id)
                .await?;
        Ok(())
    }

    /// Idempotently tear down CDC state for `app_id` — the §17.7 PG
    /// drop-namespace teardown for THIS worker's slot + publication.
    ///
    /// **P6a-3**: routes through
    /// [`crate::replication::drop_publication_and_slot`], which runs the
    /// §17.7 PG order: force the slot inactive
    /// (`pg_terminate_backend` against the listed backend after the
    /// caller's grace), `pg_drop_replication_slot`, then
    /// `DROP PUBLICATION`. Idempotent — a missing slot/publication is a
    /// no-op, so a retry after partial failure is safe (§17.7 "retry
    /// from step 3").
    ///
    /// The consumer-cancellation courtesy of §17.7 step 2 (cancel the
    /// in-process WAL consumer + await its exit) happens in the
    /// drop-namespace orchestrator BEFORE this is called; by the time we
    /// reach the slot teardown the consumer has been asked to stop and
    /// the grace has elapsed.
    ///
    /// Runs under the platform-role pool (§17.5) — the only role that
    /// may terminate a replication backend and drop a slot.
    async fn deprovision(&self, app_id: &str) -> Result<(), DbError> {
        crate::replication::drop_publication_and_slot(self.backend.pool(), app_id).await
    }

    /// Spawn the supervised WAL consumer for `app_id`.
    ///
    /// **PR 1 mechanical-refactor scope**: this method does NOT spawn
    /// anything today. The existing call site
    /// ([`crate::replication_ops::start_replication_consumer_dispatch`])
    /// keeps the `compio::runtime::spawn` + `ConsumerRunningGuard`
    /// claim on its own side because the guard's atomic try-mark /
    /// Drop unmark needs the `app_id` capture to live alongside the
    /// spawn closure. Routing the spawn through this method would
    /// require either lifting the guard into the adapter (and
    /// re-implementing the per-isolate `running_consumers` registry
    /// plumbing) or returning the future un-spawned (changing the
    /// signature to `Result<impl Future<Output = ()>, DbError>`,
    /// which doesn't fit the `Self::ConsumerHandle` shape).
    ///
    /// PR 1 ships the trait surface and the new indirection without
    /// changing PG behaviour: the dispatcher continues to call
    /// `replication::ensure_publication_and_slot` directly (it needs
    /// the `SetupOutcome` for both the `with_start_lsn(...)` argument
    /// and the JS response envelope's `confirmed_flush_lsn` field)
    /// and continues to own the `compio::runtime::spawn(run_supervised)`
    /// + `ConsumerRunningGuard` block. The dispatcher additionally
    /// calls `change_stream().spawn_consumer(app_id)` so the new
    /// indirection is wired through — that call is the no-op marker
    /// proving the trait is reachable from the consumer's
    /// `BackendHandle`-routed call shape.
    ///
    /// **PR 4** (suppression-rail integration) is where the spawn
    /// itself migrates into this method: the broker-pause /
    /// schema-pending plumbing needs the guard lifetime to wrap the
    /// spawned task, so the natural home is here.
    async fn spawn_consumer(&self, app_id: &str) -> Result<Self::ConsumerHandle, DbError> {
        // PR 1 marker — no `provision`, no spawn. See rustdoc above
        // for why. The handle exists so the trait surface is stable
        // across PR boundaries.
        Ok(WalConsumerHandle {
            _app_id: app_id.to_string(),
        })
    }

    /// Return a [`BrokerPauseGuard`] for `app_id`. **PR 1**: the
    /// guard's `Drop` is a no-op (see the trait doc comment); PR 4
    /// wires it through `wal_consumer::suppress_app`.
    fn pause_broker(&self, app_id: &str) -> BrokerPauseGuard {
        BrokerPauseGuard::new(app_id.to_string())
    }

    /// Return a [`SchemaPendingGuard`] for `app_id`. **PR 1**: the
    /// guard's `Drop` is a no-op (see the trait doc comment); PR 4
    /// wires it through the broker's `schema_pending_apps` set +
    /// `resume_app_with_resync`.
    fn engage_schema_pending(&self, app_id: &str) -> SchemaPendingGuard {
        SchemaPendingGuard::new(app_id.to_string())
    }
}

// Compile-time trait-shape assertions for `impl ChangeStream for
// PgChangeStream` live in `crate::backend::tests` alongside the rest of
// the `assert_postgres_backend_impls_*` family. Folding them into the
// existing `compile_time_assertions_link` test keeps the lib-test
// count stable (PR 1 is a mechanical refactor — no new tests).
