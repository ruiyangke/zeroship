//! Postgres implementation of [`crate::backend::ChangeStream`].
//!
//! This is the single ownership path for provisioning, starting,
//! stopping, and cleaning up a worker's logical-decoding consumer.
//!
//! **Why an adapter, not a direct `impl ChangeStream for PostgresBackend`**
//! The lifecycle path needs both the pool and configured URL. Pulling those values is cleaner
//! through a borrowed-reference wrapper than through extra accessor
//! methods on `PostgresBackend`; it also keeps the `ChangeStream` impl
//! co-located with the WAL-consumer plumbing it routes to. The SQLite
//! peer (`crate::backend::sqlite::cdc::SqliteChangeStream`) follows the
//! same shape for the same reasons (session-actor ownership lives next
//! to the impl).

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::backend::postgres::PostgresBackend;
use crate::backend::{BrokerPauseGuard, ChangeStream, SchemaPendingGuard};
use crate::error::DbError;

#[derive(Debug, Default)]
struct ExitState {
    result: Option<Result<(), DbError>>,
    waiters: Vec<flume::Sender<Result<(), DbError>>>,
}

#[derive(Debug, Default)]
struct SharedExit(Mutex<ExitState>);

impl SharedExit {
    fn complete(&self, result: Result<(), DbError>) {
        let waiters = {
            let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.result.is_some() {
                return;
            }
            state.result = Some(result.clone());
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            let _ = waiter.try_send(result.clone());
        }
    }

    async fn wait(&self) -> Result<(), DbError> {
        let receiver = {
            let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(result) = &state.result {
                return result.clone();
            }
            let (sender, receiver) = flume::bounded(1);
            state.waiters.push(sender);
            receiver
        };
        receiver.recv_async().await.unwrap_or_else(|_| {
            Err(DbError::Internal {
                message: "wal consumer: exit notification channel closed".to_string(),
            })
        })
    }
}

/// Cloneable control and completion handle for a running WAL consumer.
///
/// Clones share a broadcast-style completion state. A lifecycle owner
/// can retain one clone while a detached monitor awaits another; no
/// receiver competes for the single exit result.
#[derive(Clone)]
pub struct WalConsumerHandle {
    app_id: String,
    worker_id: String,
    shutdown: flume::Sender<()>,
    exit: Arc<SharedExit>,
}

impl std::fmt::Debug for WalConsumerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalConsumerHandle")
            .field("app_id", &self.app_id)
            .field("worker_id", &self.worker_id)
            .finish_non_exhaustive()
    }
}

impl WalConsumerHandle {
    /// Signal shutdown without blocking. Idempotent and safe from any
    /// isolate thread.
    pub fn request_shutdown(&self) {
        match self.shutdown.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {}
        }
    }

    /// Wait for the supervisor and worker-slot cleanup to finish.
    pub async fn wait(&self) -> Result<(), DbError> {
        self.exit.wait().await
    }

    /// Signal shutdown and wait until the worker slot has been dropped.
    pub async fn shutdown(self) -> Result<(), DbError> {
        self.request_shutdown();
        self.wait().await
    }
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
pub struct PgChangeStream {
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

    /// Idempotently tear down CDC state for `app_id` — the §17.7 PG
    /// drop-namespace teardown for THIS worker's slot + publication.
    ///
    /// Routes through
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
        crate::replication::drop_publication_and_slots(self.backend.pool(), app_id).await
    }

    /// Provision this worker's slot, spawn its supervised consumer,
    /// and return only after Postgres accepts `START_REPLICATION`.
    /// Startup failure is logged and returned. The spawned task always
    /// drops its worker slot before publishing its exit result.
    async fn spawn_consumer(
        &self,
        app_id: &str,
        worker_id: &str,
    ) -> Result<Self::ConsumerHandle, DbError> {
        let setup = crate::replication::ensure_publication_and_worker_slot(
            self.backend.pool(),
            app_id,
            worker_id,
        )
        .await?;
        let consumer = crate::wal_consumer::WalConsumer::new(
            app_id,
            worker_id,
            self.backend.url(),
        )?
        .with_start_lsn(setup.confirmed_flush_lsn);

        let (startup_tx, startup_rx) = flume::bounded(1);
        let (shutdown_tx, shutdown_rx) = flume::bounded(1);
        let exit = Arc::new(SharedExit::default());
        let handle = WalConsumerHandle {
            app_id: app_id.to_string(),
            worker_id: worker_id.to_string(),
            shutdown: shutdown_tx,
            exit: exit.clone(),
        };
        let pool = self.backend.pool().clone();
        let app_for_task = app_id.to_string();
        let worker_for_task = worker_id.to_string();
        compio::runtime::spawn(async move {
            let consumer_result = crate::wal_consumer::run_supervised_controlled(
                consumer,
                startup_tx,
                shutdown_rx,
            )
            .await;
            let cleanup_result = crate::replication::drop_worker_slot(
                &pool,
                &app_for_task,
                &worker_for_task,
            )
            .await;
            let result = match (consumer_result, cleanup_result) {
                (Err(error), Err(cleanup)) => {
                    tracing::error!(
                        app_id = %app_for_task,
                        worker_id = %worker_for_task,
                        error = ?cleanup,
                        "wal consumer: worker slot cleanup failed after consumer error"
                    );
                    Err(error)
                }
                (Err(error), Ok(())) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Ok(()), Ok(())) => Ok(()),
            };
            exit.complete(result);
        })
        .detach();

        match startup_rx.recv_async().await {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(error)) => {
                tracing::error!(
                    app_id,
                    worker_id,
                    error = ?error,
                    "wal consumer: CDC startup failed"
                );
                let _ = handle.wait().await;
                Err(error)
            }
            Err(_) => {
                let error = DbError::Internal {
                    message: format!(
                        "wal consumer: startup task exited without readiness for app {app_id}"
                    ),
                };
                tracing::error!(app_id, worker_id, error = ?error, "wal consumer: CDC startup failed");
                let _ = handle.wait().await;
                Err(error)
            }
        }
    }

    /// Return a [`BrokerPauseGuard`] for `app_id`. Construction
    /// suppresses the app via `wal_consumer::suppress_app`; the
    /// guard's `Drop` unsuppresses and emits a resync.
    fn pause_broker(&self, app_id: &str) -> BrokerPauseGuard {
        BrokerPauseGuard::new(app_id.to_string())
    }

    /// Return a [`SchemaPendingGuard`] for `app_id`. Construction adds
    /// the app to the broker's `schema_pending_apps` set; the guard's
    /// `Drop` clears it and calls `resume_app_with_resync`.
    fn engage_schema_pending(&self, app_id: &str) -> SchemaPendingGuard {
        SchemaPendingGuard::new(app_id.to_string())
    }
}

// Compile-time trait-shape assertions for `impl ChangeStream for
// PgChangeStream` live in `crate::backend::tests` alongside the rest of
// the `assert_postgres_backend_impls_*` family. Folding them into the
// existing `compile_time_assertions_link` test keeps the lib-test
// count stable.

#[cfg(test)]
mod tests {
    use super::WalConsumerHandle;

    #[test]
    fn consumer_handle_is_send_static_and_clone() {
        fn assert_shape<T: Send + Clone + 'static>() {}
        assert_shape::<WalConsumerHandle>();
    }
}
