//! SQLite-side [`crate::backend::ChangeStream`] adapter — the
//! `preupdate_hook` / `commit_hook` / `rollback_hook` integration
//! introduced in **P2 PR 2** lives here. **PR 1** ships the stub:
//! struct definition + a no-behaviour [`ChangeStream`] impl so the
//! [`crate::backend::BackendHandle::as_change_stream_sqlite`]
//! accessor + the trait-shape compile-time checks have a target.
//!
//! Source plan: `docs/proposals/p2-sqlite-cdc-implementation-plan.md`
//! §1, §2.2, §9 PR 2.
//!
//! **PR 1 scope**: this file declares the type; its `ChangeStream`
//! impl methods are no-ops. **PR 2** lands `SqliteCdcDispatcher`,
//! the per-session hook installation, the flume publisher task, and
//! materialises real `ChangeEvent`s onto the broker.

use std::rc::Rc;

use crate::backend::sqlite::SqliteBackend;
use crate::backend::{BrokerPauseGuard, ChangeStream, SchemaPendingGuard};
use crate::error::DbError;

/// Handle returned by [`SqliteChangeStream::spawn_consumer`].
///
/// **P2 PR 1**: a unit-struct marker — PR 2 grows it to carry the
/// flume `Receiver` end of the worker→compio publisher channel + the
/// session-actor command handle the supervisor uses to disengage
/// hooks on teardown. The shape exists at PR 1 so the
/// [`ChangeStream::ConsumerHandle`] associated type binds cleanly.
#[derive(Debug)]
pub struct SqliteConsumerHandle;

/// SQLite arm of the [`ChangeStream`] capability.
///
/// Constructed via [`crate::backend::BackendHandle::as_change_stream_sqlite`].
/// Owns an `Rc<SqliteBackend>` (Rc-cloned from the
/// [`crate::backend::BackendHandle::Sqlite`] arm) — same ownership
/// shape as the PG-arm [`crate::change_stream_pg::PgChangeStream`], for
/// the same reason: the trait's `'static` bound + `async fn`-in-trait
/// futures don't compose with a borrowed-reference (`<'b>`) self.
///
/// **PR 1 stub** — every method body is a no-op (`provision` /
/// `deprovision` / `spawn_consumer` return `Ok(())`/unit; the two
/// guard accessors return PR-1 no-op guards). PR 2 backfills the
/// real `preupdate_hook` install + `commit_hook` packet ship via the
/// session actor.
#[allow(dead_code)]
pub struct SqliteChangeStream {
    backend: Rc<SqliteBackend>,
}

impl SqliteChangeStream {
    /// Construct an adapter holding an Rc-clone of `backend`.
    /// Crate-private — the
    /// [`crate::backend::BackendHandle::as_change_stream_sqlite`] accessor
    /// is the public entry point.
    pub(crate) fn new(backend: Rc<SqliteBackend>) -> Self {
        Self { backend }
    }
}

impl ChangeStream for SqliteChangeStream {
    type ConsumerHandle = SqliteConsumerHandle;

    /// **PR 1**: no-op. PR 2 installs the `preupdate_hook` /
    /// `commit_hook` / `rollback_hook` triplet on the per-app session
    /// via a new `session::Command` admin variant.
    async fn provision(&self, _app_id: &str) -> Result<(), DbError> {
        Ok(())
    }

    /// **PR 1**: no-op. PR 2 disarms the hook triplet for the
    /// session bound to `_app_id`.
    async fn deprovision(&self, _app_id: &str) -> Result<(), DbError> {
        Ok(())
    }

    /// **PR 1**: returns a unit handle. PR 2 spawns the
    /// worker→compio publisher task + arms the per-session hook
    /// triplet; PR 3 onwards wires the relation filter + broker
    /// fanout.
    async fn spawn_consumer(&self, _app_id: &str) -> Result<Self::ConsumerHandle, DbError> {
        Ok(SqliteConsumerHandle)
    }

    /// **PR 1**: returns a no-op [`BrokerPauseGuard`]. PR 4 wires
    /// the guard's `Drop` to clear the per-session
    /// `Command::CdcSuppress { on: false }` admin flag + emit
    /// `Broker::resume_app_with_resync`.
    fn pause_broker(&self, app_id: &str) -> BrokerPauseGuard {
        BrokerPauseGuard::new(app_id.to_string())
    }

    /// **PR 1**: returns a no-op [`SchemaPendingGuard`]. PR 4
    /// wires the broker's thread-local `schema_pending_apps` set +
    /// `subscribe` rejection.
    fn engage_schema_pending(&self, app_id: &str) -> SchemaPendingGuard {
        SchemaPendingGuard::new(app_id.to_string())
    }
}

// Compile-time trait-shape assertions for
// `impl ChangeStream for SqliteChangeStream` live alongside the
// existing `assert_sqlite_backend_impls_*` family in
// `crate::backend::sqlite::tests`. Folding them into the existing
// `compile_time_trait_assertions_link` test there keeps the
// `--features sqlite` lib-test count stable (PR 1 is a mechanical
// refactor — no new tests).
