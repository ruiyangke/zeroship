//! Process-wide CDC ownership for live-query subscriptions.
//!
//! The V8 runtime owns one isolate per app and worker thread, while a worker
//! process can host many such threads. Logical replication has a different
//! ownership boundary: one consumer and one slot per `(app, worker process)`.
//! This module bridges those boundaries. Every native Subscription owns a
//! [`CdcLease`]; the first lease that reaches its readiness handshake starts
//! the consumer, all other isolates await the same startup result, and the
//! last lease signals shutdown. The consumer task drops its worker slot before
//! publishing its exit result.
//!
//! A process-wide mutex is deliberate. It protects only counters, state enums,
//! and channel handles; no database or channel await occurs while it is held.
//! Keeping ownership here avoids one consumer per isolate and gives close/GC a
//! synchronous, cross-thread teardown seam.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::backend::{BackendHandle, ChangeStream};
use crate::change_stream_pg::WalConsumerHandle;
use crate::error::DbError;

#[derive(Debug)]
enum RunningConsumer {
    Postgres(WalConsumerHandle),
    /// SQLite installs its commit publisher with the backend session. It has
    /// no logical slot or per-app task to retain.
    Sqlite,
}

#[derive(Debug)]
enum ConsumerState {
    Idle,
    Starting {
        waiters: Vec<flume::Sender<Result<(), DbError>>>,
    },
    Running(RunningConsumer),
    Failed(DbError),
    Stopping {
        waiters: Vec<flume::Sender<()>>,
    },
}

#[derive(Debug)]
struct AppState {
    generation: u64,
    subscribers: usize,
    force_stop: bool,
    shutdown_waiters: Vec<flume::Sender<()>>,
    state: ConsumerState,
}

#[derive(Debug, Default)]
struct LifecycleManager {
    next_generation: u64,
    apps: HashMap<String, AppState>,
}

static MANAGER: LazyLock<Mutex<LifecycleManager>> =
    LazyLock::new(|| Mutex::new(LifecycleManager::default()));

fn manager() -> MutexGuard<'static, LifecycleManager> {
    MANAGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One live native Subscription's claim on the process-wide consumer.
///
/// The lease is not Clone: exactly one V8 wrapper owns it. Explicit close and
/// the wrapper's guaranteed finalizer both drop the same Option, so release is
/// immediate and idempotent.
#[derive(Debug)]
pub(crate) struct CdcLease {
    app_id: String,
    generation: u64,
}

impl Drop for CdcLease {
    fn drop(&mut self) {
        release(&self.app_id, self.generation);
    }
}

/// Register a subscription before returning its V8 wrapper.
pub(crate) fn acquire(app_id: &str) -> CdcLease {
    let mut lifecycle = manager();
    let generation = if let Some(app) = lifecycle.apps.get_mut(app_id) {
        app.subscribers = app.subscribers.saturating_add(1);
        app.generation
    } else {
        lifecycle.next_generation = lifecycle.next_generation.wrapping_add(1).max(1);
        let generation = lifecycle.next_generation;
        lifecycle.apps.insert(
            app_id.to_string(),
            AppState {
                generation,
                subscribers: 1,
                force_stop: false,
                shutdown_waiters: Vec::new(),
                state: ConsumerState::Idle,
            },
        );
        generation
    };
    CdcLease {
        app_id: app_id.to_string(),
        generation,
    }
}

fn release(app_id: &str, generation: u64) {
    let mut shutdown = None;
    let mut remove = false;
    let mut shutdown_waiters = Vec::new();
    {
        let mut lifecycle = manager();
        let Some(app) = lifecycle.apps.get_mut(app_id) else {
            return;
        };
        if app.generation != generation || app.subscribers == 0 {
            return;
        }
        app.subscribers -= 1;
        if app.subscribers != 0 {
            return;
        }

        match &app.state {
            ConsumerState::Running(RunningConsumer::Postgres(handle)) => {
                shutdown = Some(handle.clone());
                app.state = ConsumerState::Stopping {
                    waiters: Vec::new(),
                };
            }
            ConsumerState::Starting { .. } => {
                // Startup owns no handle until START_REPLICATION succeeds.
                // finish_start observes subscribers == 0 and stops the handle
                // before exposing a healthy result.
            }
            ConsumerState::Stopping { .. } => {}
            ConsumerState::Idle
            | ConsumerState::Failed(_)
            | ConsumerState::Running(RunningConsumer::Sqlite) => remove = true,
        }
        if remove {
            if let Some(removed) = lifecycle.apps.remove(app_id) {
                shutdown_waiters = removed.shutdown_waiters;
            }
        }
    }
    notify_shutdown(shutdown_waiters);
    if let Some(handle) = shutdown {
        handle.request_shutdown();
    }
}

fn notify_shutdown(waiters: Vec<flume::Sender<()>>) {
    for waiter in waiters {
        let _ = waiter.try_send(());
    }
}

enum ReadyAction {
    Ready,
    Failed(DbError),
    Start { generation: u64 },
    WaitForStart(flume::Receiver<Result<(), DbError>>),
    WaitForStop(flume::Receiver<()>),
}

fn ready_action(app_id: &str) -> Result<ReadyAction, DbError> {
    let mut lifecycle = manager();
    let app = lifecycle.apps.get_mut(app_id).ok_or_else(|| {
        DbError::config(
            "subscription_closed",
            "db subscription was closed before CDC became ready",
        )
    })?;
    if app.force_stop {
        return Ok(ReadyAction::Failed(DbError::config(
            "subscription_app_dropped",
            "db subscription's app is being deleted",
        )));
    }
    match &mut app.state {
        ConsumerState::Idle => {
            app.state = ConsumerState::Starting {
                waiters: Vec::new(),
            };
            Ok(ReadyAction::Start {
                generation: app.generation,
            })
        }
        ConsumerState::Starting { waiters } => {
            let (sender, receiver) = flume::bounded(1);
            waiters.push(sender);
            Ok(ReadyAction::WaitForStart(receiver))
        }
        ConsumerState::Running(_) => Ok(ReadyAction::Ready),
        ConsumerState::Failed(error) => Ok(ReadyAction::Failed(error.clone())),
        ConsumerState::Stopping { waiters } => {
            let (sender, receiver) = flume::bounded(1);
            waiters.push(sender);
            Ok(ReadyAction::WaitForStop(receiver))
        }
    }
}

/// Complete the subscription-open handshake.
///
/// This is called by native `Subscription.ready()` and before every `next()`.
/// A Postgres subscription does not report ready until publication/slot
/// provisioning and START_REPLICATION have both succeeded. Configuration and
/// startup failures therefore reject the stream before its initial snapshot.
pub(crate) async fn ensure_ready(app_id: &str) -> Result<(), DbError> {
    loop {
        match ready_action(app_id)? {
            ReadyAction::Ready => return Ok(()),
            ReadyAction::Failed(error) => return Err(error),
            ReadyAction::WaitForStart(receiver) => {
                return receiver.recv_async().await.unwrap_or_else(|_| {
                    Err(DbError::Internal {
                        message: "db CDC startup result channel closed".to_string(),
                    })
                });
            }
            ReadyAction::WaitForStop(receiver) => {
                receiver.recv_async().await.map_err(|_| DbError::Internal {
                    message: "db CDC shutdown notification channel closed".to_string(),
                })?;
            }
            ReadyAction::Start { generation } => {
                let result = start_on_current_isolate(app_id).await;
                return finish_start(app_id, generation, result);
            }
        }
    }
}

async fn start_on_current_isolate(app_id: &str) -> Result<RunningConsumer, DbError> {
    if crate::context::with(|context| context.backend().is_none()) {
        crate::init_pool_async().await.map_err(|message| {
            DbError::config("cdc_start_failed", format!("db CDC startup failed: {message}"))
        })?;
    }

    let worker_id = crate::context::with(|context| context.cdc_worker_id()).ok_or_else(|| {
        DbError::config_hinted(
            "cdc_worker_id_missing",
            "db CDC worker identity is not configured",
            "construct DbPlugin with the worker process identity",
        )
    })?;
    let backend = crate::context::with(|context| context.backend()).ok_or_else(|| {
        DbError::config("backend_not_initialized", "db backend is not initialized")
    })?;

    match backend {
        BackendHandle::Postgres(_) => {
            let change_stream = backend
                .as_change_stream_pg()
                .expect("Postgres backend must expose its change stream");
            // Suppress the local fast path before provisioning. The initial
            // snapshot is emitted only after readiness, so writes in this
            // startup window are represented by that snapshot. The consumer
            // installs its own overlapping guard before it reports ready;
            // the overlap prevents a local-plus-WAL duplicate-delivery gap.
            let startup_suppression = crate::broker::SuppressGuard::activate(app_id);
            let handle = change_stream.spawn_consumer(app_id, &worker_id).await;
            drop(startup_suppression);
            let handle = handle?;
            Ok(RunningConsumer::Postgres(handle))
        }
        BackendHandle::Sqlite(_) => {
            let change_stream = backend
                .as_change_stream_sqlite()
                .expect("SQLite backend must expose its change stream");
            let _handle = change_stream.spawn_consumer(app_id, &worker_id).await?;
            Ok(RunningConsumer::Sqlite)
        }
    }
}

fn finish_start(
    app_id: &str,
    generation: u64,
    result: Result<RunningConsumer, DbError>,
) -> Result<(), DbError> {
    let mut monitor = None;
    let mut shutdown = None;
    let mut waiters = Vec::new();
    let mut shutdown_waiters = Vec::new();
    let mut outcome = Ok(());
    {
        let mut lifecycle = manager();
        let Some(app) = lifecycle.apps.get_mut(app_id) else {
            if let Ok(RunningConsumer::Postgres(handle)) = result {
                handle.request_shutdown();
            }
            return Err(DbError::config(
                "subscription_closed",
                "db subscription closed during CDC startup",
            ));
        };
        if app.generation != generation {
            if let Ok(RunningConsumer::Postgres(handle)) = result {
                handle.request_shutdown();
            }
            return Err(DbError::config(
                "subscription_replaced",
                "db subscription generation changed during CDC startup",
            ));
        }
        if let ConsumerState::Starting { waiters: pending } = &mut app.state {
            waiters = std::mem::take(pending);
        }

        match result {
            Ok(RunningConsumer::Postgres(handle)) => {
                monitor = Some(handle.clone());
                if app.subscribers == 0 || app.force_stop {
                    shutdown = Some(handle);
                    app.state = ConsumerState::Stopping {
                        waiters: Vec::new(),
                    };
                    outcome = Err(DbError::config(
                        "subscription_closed",
                        "db subscription closed during CDC startup",
                    ));
                } else {
                    app.state = ConsumerState::Running(RunningConsumer::Postgres(handle));
                }
            }
            Ok(RunningConsumer::Sqlite) => {
                if app.subscribers == 0 || app.force_stop {
                    if let Some(removed) = lifecycle.apps.remove(app_id) {
                        shutdown_waiters = removed.shutdown_waiters;
                    }
                    outcome = Err(DbError::config(
                        "subscription_closed",
                        "db subscription closed during CDC startup",
                    ));
                } else {
                    app.state = ConsumerState::Running(RunningConsumer::Sqlite);
                }
            }
            Err(error) => {
                tracing::error!(
                    app_id = %app_id,
                    error = %error,
                    "db CDC failed to start; refusing live subscription"
                );
                outcome = Err(error.clone());
                if app.subscribers == 0 || app.force_stop {
                    if let Some(removed) = lifecycle.apps.remove(app_id) {
                        shutdown_waiters = removed.shutdown_waiters;
                    }
                } else {
                    app.state = ConsumerState::Failed(error);
                }
            }
        }
    }

    for waiter in waiters {
        let _ = waiter.try_send(outcome.clone());
    }
    notify_shutdown(shutdown_waiters);
    if let Some(handle) = shutdown {
        handle.request_shutdown();
    }
    if let Some(handle) = monitor {
        let app_id = app_id.to_string();
        compio::runtime::spawn(async move {
            let result = handle.wait().await;
            consumer_exited(&app_id, generation, result);
        })
        .detach();
    }
    outcome
}

fn consumer_exited(app_id: &str, generation: u64, result: Result<(), DbError>) {
    let mut stop_waiters = Vec::new();
    let mut shutdown_waiters = Vec::new();
    let mut log_error = None;
    {
        let mut lifecycle = manager();
        let Some(app) = lifecycle.apps.get_mut(app_id) else {
            return;
        };
        if app.generation != generation {
            return;
        }
        match &mut app.state {
            ConsumerState::Stopping { waiters } => {
                stop_waiters = std::mem::take(waiters);
                if app.subscribers == 0 || app.force_stop {
                    if let Some(removed) = lifecycle.apps.remove(app_id) {
                        shutdown_waiters = removed.shutdown_waiters;
                    }
                } else {
                    app.state = ConsumerState::Idle;
                }
            }
            ConsumerState::Running(_) => {
                let error = result.err().unwrap_or_else(|| DbError::Transient {
                    message: "db CDC consumer exited unexpectedly".to_string(),
                });
                log_error = Some(error.clone());
                app.state = ConsumerState::Failed(error);
            }
            _ => return,
        }
    }

    for waiter in stop_waiters {
        let _ = waiter.try_send(());
    }
    notify_shutdown(shutdown_waiters);
    if let Some(error) = log_error {
        tracing::error!(
            app_id = %app_id,
            error = %error,
            "db CDC consumer stopped; live subscriptions are unhealthy"
        );
        // Wake every pending native next() and turn the failure into a
        // stream-visible close. db.live maps an unexpected close to
        // LIVE_SUBSCRIPTION_CLOSED instead of hanging as a static snapshot.
        crate::broker::drop_app(Some(app_id));
    }
}

/// Stop this process's consumer immediately when an app is deleted.
///
/// Full Postgres object teardown is performed by the deletion path after this
/// signal. Leases finalized later are ignored through the generation check.
pub async fn shutdown_app(app_id: &str) {
    let mut shutdown = None;
    let mut notify_now = Vec::new();
    let receiver = {
        let (sender, receiver) = flume::bounded(1);
        let mut lifecycle = manager();
        let Some(app) = lifecycle.apps.get_mut(app_id) else {
            return;
        };
        app.force_stop = true;
        app.subscribers = 0;
        app.shutdown_waiters.push(sender);
        match &app.state {
            ConsumerState::Running(RunningConsumer::Postgres(handle)) => {
                shutdown = Some(handle.clone());
                app.state = ConsumerState::Stopping {
                    waiters: Vec::new(),
                };
            }
            ConsumerState::Starting { .. } | ConsumerState::Stopping { .. } => {}
            ConsumerState::Idle
            | ConsumerState::Failed(_)
            | ConsumerState::Running(RunningConsumer::Sqlite) => {
                if let Some(removed) = lifecycle.apps.remove(app_id) {
                    notify_now = removed.shutdown_waiters;
                }
            }
        }
        receiver
    };
    notify_shutdown(notify_now);
    if let Some(handle) = shutdown {
        handle.request_shutdown();
    }
    let _ = receiver.recv_async().await;
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn subscriber_count_for_tests(app_id: &str) -> usize {
    manager()
        .apps
        .get(app_id)
        .map_or(0, |app| app.subscribers)
}

#[cfg(test)]
fn shutdown_app_sync_for_tests(app_id: &str) {
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(shutdown_app(app_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_count_is_process_wide_and_last_drop_removes_idle_app() {
        let app = format!("lease-test-{}", uuid::Uuid::new_v4());
        let first = acquire(&app);
        let second = std::thread::spawn({
            let app = app.clone();
            move || acquire(&app)
        })
        .join()
        .expect("lease thread");
        assert_eq!(subscriber_count_for_tests(&app), 2);
        drop(first);
        assert_eq!(subscriber_count_for_tests(&app), 1);
        drop(second);
        assert_eq!(subscriber_count_for_tests(&app), 0);
    }

    #[test]
    fn app_shutdown_removes_idle_claim_and_ignores_stale_lease() {
        let app = format!("shutdown-test-{}", uuid::Uuid::new_v4());
        let lease = acquire(&app);
        assert_eq!(subscriber_count_for_tests(&app), 1);
        shutdown_app_sync_for_tests(&app);
        assert_eq!(subscriber_count_for_tests(&app), 0);
        drop(lease);
        assert_eq!(subscriber_count_for_tests(&app), 0);
    }
}
