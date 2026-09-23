//! Process-wide subscription leases for embedded capture and the remote relay.
//! V8 and native Rust subscriptions share readiness, cancellation, and teardown.
//!
//! **A consumer is claimed per ROUTE, not per app.** One relay connection
//! carries one `(app, database)` stream, so an app that reaches two databases
//! needs two. Keyed on the app alone, the second database's first subscriber
//! would find the first database's consumer already `Running`, be told capture
//! was ready, and then receive nothing for the rest of its life.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use super::relay::{RelayConfig, RelayHandle};
use crate::backend::BackendHandle;
use crate::binding::DbRoute;
use crate::error::DbError;

#[derive(Debug)]
enum RunningConsumer {
    Relay(RelayHandle),
    /// The backend publishes committed changes itself and owns no per-app
    /// consumer task here.
    Embedded,
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
struct RouteState {
    generation: u64,
    subscribers: usize,
    force_stop: bool,
    shutdown_waiters: Vec<flume::Sender<()>>,
    state: ConsumerState,
}

#[derive(Debug, Default)]
struct LifecycleManager {
    next_generation: u64,
    routes: HashMap<DbRoute, RouteState>,
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
/// Dropping the lease releases the subscription claim. Native callers and V8
/// wrappers use the same ownership rule.
#[derive(Debug)]
pub struct CdcLease {
    route: DbRoute,
    generation: u64,
}

impl Drop for CdcLease {
    fn drop(&mut self) {
        release(&self.route, self.generation);
    }
}

/// Register a subscription before waiting for capture readiness.
pub fn acquire(route: &DbRoute) -> CdcLease {
    let mut lifecycle = manager();
    let generation = if let Some(claim) = lifecycle.routes.get_mut(route) {
        claim.subscribers = claim.subscribers.saturating_add(1);
        claim.generation
    } else {
        lifecycle.next_generation = lifecycle.next_generation.wrapping_add(1).max(1);
        let generation = lifecycle.next_generation;
        lifecycle.routes.insert(
            route.clone(),
            RouteState {
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
        route: route.clone(),
        generation,
    }
}

fn release(route: &DbRoute, generation: u64) {
    let mut shutdown = None;
    let mut remove = false;
    let mut shutdown_waiters = Vec::new();
    {
        let mut lifecycle = manager();
        let Some(claim) = lifecycle.routes.get_mut(route) else {
            return;
        };
        if claim.generation != generation || claim.subscribers == 0 {
            return;
        }
        claim.subscribers -= 1;
        if claim.subscribers != 0 {
            return;
        }

        match &claim.state {
            ConsumerState::Running(RunningConsumer::Relay(handle)) => {
                shutdown = Some(handle.clone());
                claim.state = ConsumerState::Stopping {
                    waiters: Vec::new(),
                };
            }
            ConsumerState::Starting { .. } => {
                // Startup owns no handle until the relay reports ready.
                // finish_start observes subscribers == 0 and stops the handle
                // before exposing a healthy result.
            }
            ConsumerState::Stopping { .. } => {}
            ConsumerState::Idle
            | ConsumerState::Failed(_)
            | ConsumerState::Running(RunningConsumer::Embedded) => remove = true,
        }
        if remove {
            if let Some(removed) = lifecycle.routes.remove(route) {
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

fn ready_action(route: &DbRoute) -> Result<ReadyAction, DbError> {
    let mut lifecycle = manager();
    let claim = lifecycle.routes.get_mut(route).ok_or_else(|| {
        DbError::config(
            "subscription_closed",
            "db subscription was closed before CDC became ready",
        )
    })?;
    if claim.force_stop {
        return Ok(ReadyAction::Failed(DbError::config(
            "subscription_app_dropped",
            "db subscription's app is being deleted",
        )));
    }
    match &mut claim.state {
        ConsumerState::Idle => {
            claim.state = ConsumerState::Starting {
                waiters: Vec::new(),
            };
            Ok(ReadyAction::Start {
                generation: claim.generation,
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

/// Wait for capture before taking the subscription's initial snapshot.
/// Backends that do not publish commits locally require a composed relay client.
pub async fn ensure_ready(
    route: &DbRoute,
    backend: BackendHandle,
    relay: Option<RelayConfig>,
) -> Result<(), DbError> {
    loop {
        match ready_action(route)? {
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
                // Startup must finish even if the caller drops its readiness
                // future. Otherwise the route remains Starting indefinitely.
                let route = route.clone();
                let backend = backend.clone();
                let relay = relay.clone();
                compio::runtime::spawn(async move {
                    use futures::FutureExt;
                    let result =
                        std::panic::AssertUnwindSafe(start_consumer(&route, backend, relay))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| {
                                Err(DbError::Internal {
                                    message: "CDC startup panicked".into(),
                                })
                            });
                    let _ = finish_start(&route, generation, result);
                })
                .detach();
            }
        }
    }
}

async fn start_consumer(
    route: &DbRoute,
    backend: BackendHandle,
    relay: Option<RelayConfig>,
) -> Result<RunningConsumer, DbError> {
    let _suppression = super::broker::SuppressGuard::activate(route);
    if backend.publishes_committed_changes() {
        Ok(RunningConsumer::Embedded)
    } else if let Some(relay) = relay {
        Ok(RunningConsumer::Relay(relay.spawn(route).await?))
    } else {
        Err(DbError::config(
            "cdc_relay_missing",
            "this database backend requires the CDC relay",
        ))
    }
}

fn finish_start(
    route: &DbRoute,
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
        let Some(claim) = lifecycle.routes.get_mut(route) else {
            if let Ok(RunningConsumer::Relay(handle)) = result {
                handle.request_shutdown();
            }
            return Err(DbError::config(
                "subscription_closed",
                "db subscription closed during CDC startup",
            ));
        };
        if claim.generation != generation {
            if let Ok(RunningConsumer::Relay(handle)) = result {
                handle.request_shutdown();
            }
            return Err(DbError::config(
                "subscription_replaced",
                "db subscription generation changed during CDC startup",
            ));
        }
        if let ConsumerState::Starting { waiters: pending } = &mut claim.state {
            waiters = std::mem::take(pending);
        }

        match result {
            Ok(RunningConsumer::Relay(handle)) => {
                monitor = Some(handle.clone());
                if claim.subscribers == 0 || claim.force_stop {
                    shutdown = Some(handle);
                    claim.state = ConsumerState::Stopping {
                        waiters: Vec::new(),
                    };
                    outcome = Err(DbError::config(
                        "subscription_closed",
                        "db subscription closed during CDC startup",
                    ));
                } else {
                    claim.state = ConsumerState::Running(RunningConsumer::Relay(handle));
                }
            }
            Ok(RunningConsumer::Embedded) => {
                if claim.subscribers == 0 || claim.force_stop {
                    if let Some(removed) = lifecycle.routes.remove(route) {
                        shutdown_waiters = removed.shutdown_waiters;
                    }
                    outcome = Err(DbError::config(
                        "subscription_closed",
                        "db subscription closed during CDC startup",
                    ));
                } else {
                    claim.state = ConsumerState::Running(RunningConsumer::Embedded);
                }
            }
            Err(error) => {
                tracing::error!(
                    app_id = %route.app_id(),
                    database = %route.database_text(),
                    error = %error,
                    "db CDC failed to start; refusing live subscription"
                );
                outcome = Err(error.clone());
                if claim.subscribers == 0 || claim.force_stop {
                    if let Some(removed) = lifecycle.routes.remove(route) {
                        shutdown_waiters = removed.shutdown_waiters;
                    }
                } else {
                    claim.state = ConsumerState::Failed(error);
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
        let route = route.clone();
        compio::runtime::spawn(async move {
            let result = handle.wait().await;
            consumer_exited(&route, generation, result);
        })
        .detach();
    }
    outcome
}

fn consumer_exited(route: &DbRoute, generation: u64, result: Result<(), DbError>) {
    let mut stop_waiters = Vec::new();
    let mut shutdown_waiters = Vec::new();
    let mut log_error = None;
    {
        let mut lifecycle = manager();
        let Some(claim) = lifecycle.routes.get_mut(route) else {
            return;
        };
        if claim.generation != generation {
            return;
        }
        match &mut claim.state {
            ConsumerState::Stopping { waiters } => {
                stop_waiters = std::mem::take(waiters);
                if claim.subscribers == 0 || claim.force_stop {
                    if let Some(removed) = lifecycle.routes.remove(route) {
                        shutdown_waiters = removed.shutdown_waiters;
                    }
                } else {
                    claim.state = ConsumerState::Idle;
                }
            }
            ConsumerState::Running(_) => {
                let error = result.err().unwrap_or_else(|| DbError::Transient {
                    message: "db CDC consumer exited unexpectedly".to_string(),
                });
                log_error = Some(error.clone());
                claim.state = ConsumerState::Failed(error);
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
            app_id = %route.app_id(),
            database = %route.database_text(),
            error = %error,
            "db CDC consumer stopped; live subscriptions are unhealthy"
        );
        // Wake every pending native next() and turn the failure into a
        // stream-visible close. db.live maps an unexpected close to
        // LIVE_SUBSCRIPTION_CLOSED instead of hanging as a static snapshot.
        //
        // Scoped to the ROUTE whose consumer died. The app's other databases
        // have their own consumers and their subscribers' snapshots are still
        // valid; closing them would report one stream's failure as every
        // stream's.
        super::broker::drop_route(route);
    }
}

/// Stop this process's consumers immediately when an app is deleted.
///
/// **Every route the app holds**, because the app is going away and a consumer
/// left running on one of its databases would keep a relay connection and a
/// replication slot alive for a tenant that no longer exists.
///
/// The relay owns PostgreSQL slot teardown after the connection closes. Leases finalized later are ignored through the generation check.
pub async fn shutdown_app(app_id: &str) {
    let mut shutdown = Vec::new();
    let mut notify_now = Vec::new();
    let mut receivers = Vec::new();
    {
        let mut lifecycle = manager();
        let routes: Vec<DbRoute> = lifecycle
            .routes
            .keys()
            .filter(|route| route.app_id() == app_id)
            .cloned()
            .collect();
        for route in routes {
            let (sender, receiver) = flume::bounded(1);
            let Some(claim) = lifecycle.routes.get_mut(&route) else {
                continue;
            };
            claim.force_stop = true;
            claim.subscribers = 0;
            claim.shutdown_waiters.push(sender);
            receivers.push(receiver);
            match &claim.state {
                ConsumerState::Running(RunningConsumer::Relay(handle)) => {
                    shutdown.push(handle.clone());
                    claim.state = ConsumerState::Stopping {
                        waiters: Vec::new(),
                    };
                }
                ConsumerState::Starting { .. } | ConsumerState::Stopping { .. } => {}
                ConsumerState::Idle
                | ConsumerState::Failed(_)
                | ConsumerState::Running(RunningConsumer::Embedded) => {
                    if let Some(removed) = lifecycle.routes.remove(&route) {
                        notify_now.extend(removed.shutdown_waiters);
                    }
                }
            }
        }
    }
    notify_shutdown(notify_now);
    for handle in shutdown {
        handle.request_shutdown();
    }
    for receiver in receivers {
        let _ = receiver.recv_async().await;
    }
}

#[cfg(test)]
fn subscriber_count_for_tests(route: &DbRoute) -> usize {
    manager()
        .routes
        .get(route)
        .map_or(0, |claim| claim.subscribers)
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

    /// A fresh route for one app, on a database minted for this arm alone.
    fn route(app: &str) -> DbRoute {
        DbRoute::new(app, Some(zeroship_core::DatabaseId::mint()))
    }

    #[compio::test]
    async fn cancelled_readiness_does_not_strand_startup_or_shutdown() {
        let files = tempfile::tempdir().unwrap();
        let sqlite = crate::backend::SqliteBackend::open(
            files.path(),
            std::sync::Arc::new(super::super::broker::BrokerChangeSink),
            crate::encryption::ProjectKeySource::unavailable(),
        )
        .await
        .unwrap();
        let backend = BackendHandle::new(std::rc::Rc::new(sqlite));
        let app = format!("cancel-start-{}", uuid::Uuid::new_v4());
        let route = route(&app);
        let lease = acquire(&route);
        let mut readiness = Box::pin(ensure_ready(&route, backend, None));
        assert!(futures::poll!(&mut readiness).is_pending());
        drop(readiness);
        drop(lease);
        compio::time::timeout(std::time::Duration::from_secs(5), shutdown_app(&app))
            .await
            .expect("cancelled readiness must not strand shutdown");
        assert_eq!(subscriber_count_for_tests(&route), 0);
    }

    #[test]
    fn lease_count_is_process_wide_and_last_drop_removes_idle_route() {
        let route = route(&format!("lease-test-{}", uuid::Uuid::new_v4()));
        let first = acquire(&route);
        let second = std::thread::spawn({
            let route = route.clone();
            move || acquire(&route)
        })
        .join()
        .expect("lease thread");
        assert_eq!(subscriber_count_for_tests(&route), 2);
        drop(first);
        assert_eq!(subscriber_count_for_tests(&route), 1);
        drop(second);
        assert_eq!(subscriber_count_for_tests(&route), 0);
    }

    /// **One app's two databases hold two independent claims.**
    ///
    /// The control is the first route's count staying put while the second is
    /// acquired and released: without it, a manager that ignored the database
    /// half would show one shared counter and pass the arm below by accident.
    #[test]
    fn an_app_s_two_databases_claim_two_consumers() {
        let app = format!("two-db-lease-{}", uuid::Uuid::new_v4());
        let mine = route(&app);
        let theirs = route(&app);
        assert_eq!(mine.app_id(), theirs.app_id(), "the control: one tenant");
        assert_ne!(mine.database(), theirs.database(), "two databases");

        let first = acquire(&mine);
        assert_eq!(subscriber_count_for_tests(&mine), 1);
        assert_eq!(
            subscriber_count_for_tests(&theirs),
            0,
            "a claim on one database must not be counted on the other"
        );
        let second = acquire(&theirs);
        assert_eq!(subscriber_count_for_tests(&mine), 1);
        assert_eq!(subscriber_count_for_tests(&theirs), 1);
        drop(second);
        assert_eq!(
            subscriber_count_for_tests(&mine),
            1,
            "releasing one database's claim must leave the other's standing"
        );
        drop(first);
        assert_eq!(subscriber_count_for_tests(&mine), 0);
    }

    #[test]
    fn app_shutdown_removes_every_route_s_idle_claim_and_ignores_stale_leases() {
        let app = format!("shutdown-test-{}", uuid::Uuid::new_v4());
        let mine = route(&app);
        let theirs = route(&app);
        let leases = [acquire(&mine), acquire(&theirs)];
        assert_eq!(subscriber_count_for_tests(&mine), 1);
        assert_eq!(subscriber_count_for_tests(&theirs), 1);
        shutdown_app_sync_for_tests(&app);
        assert_eq!(subscriber_count_for_tests(&mine), 0);
        assert_eq!(
            subscriber_count_for_tests(&theirs),
            0,
            "deleting the app stops every database's consumer, not the first"
        );
        drop(leases);
        assert_eq!(subscriber_count_for_tests(&mine), 0);
        assert_eq!(subscriber_count_for_tests(&theirs), 0);
    }
}
