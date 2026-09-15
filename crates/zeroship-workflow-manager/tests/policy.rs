#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native policy fixtures stay on their compio runtime"
)]

#[allow(
    dead_code,
    reason = "native declarations include unrelated manager tables"
)]
#[path = "../src/models/schema_definition.rs"]
mod native_schema;
#[allow(dead_code, reason = "other manager suites use backend administration")]
mod support;

use futures::{channel::oneshot, future::ready};
use native_schema::schema::{assignments, queue_scopes, workers};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    time::{Duration, Instant},
};
use support::{Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, RegisterWorker, Revision, WorkerId, WorkerState,
    },
    workflow_policy::{AppPolicy, PolicyLeaseRequest},
};
use zeroship_data_orm::orm::{Database, FromRow};
use zeroship_workflow_manager::{
    coordinator::{Coordinator, Options, Placed},
    policy::{PolicyObservation, PolicySource},
    Error, Queue,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $body:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($body(&Fixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($body(&Fixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_policy_tuple_and_disabled_values,
    postgres_policy_tuple_and_disabled_values,
    exact_tuple
);
case!(
    sqlite_policy_authority_caps_without_renewal,
    postgres_policy_authority_caps_without_renewal,
    authority_caps
);
case!(
    sqlite_policy_rejects_foreign_and_unavailable_authority,
    postgres_policy_rejects_foreign_and_unavailable_authority,
    refusals
);
case!(
    sqlite_policy_revalidates_after_app_lock,
    postgres_policy_revalidates_after_app_lock,
    held_app_lock
);
case!(
    sqlite_policy_revalidates_enrollment_waits,
    postgres_policy_revalidates_enrollment_waits,
    enrollment_waits
);
case!(
    sqlite_policy_original_source_expires_during_wait,
    postgres_policy_original_source_expires_during_wait,
    original_source_expiry
);
case!(
    sqlite_policy_source_and_placement_extensions_preserve_attempt,
    postgres_policy_source_and_placement_extensions_preserve_attempt,
    extensions
);
case!(
    sqlite_policy_grant_clone_rechecks_source_and_expiry,
    postgres_policy_grant_clone_rechecks_source_and_expiry,
    retained_grants
);

const KEY: &str = "verified-enrolled-key";
const LONG: Duration = Duration::from_secs(60);

fn revision(value: i64) -> Revision {
    value.try_into().unwrap()
}
fn until(duration: Duration) -> Instant {
    Instant::now() + duration
}

#[derive(Debug)]
struct Version {
    observation: PolicyObservation,
    current_deadline: Instant,
    invalidated: bool,
}

/// An explicit finite test authority. Historical invalidation is sticky, and
/// revalidation never awaits or consults customer storage.
#[derive(Debug)]
struct Source {
    versions: RefCell<Vec<Version>>,
    selected: Cell<usize>,
    failure: Cell<Option<Error>>,
    observations: Cell<usize>,
    observed: RefCell<Option<oneshot::Sender<()>>>,
    resume: RefCell<Option<oneshot::Receiver<()>>>,
    validated: RefCell<Option<oneshot::Sender<()>>>,
}

impl Source {
    fn new(app: &AppId, policy: AppPolicy, deadline: Instant) -> Self {
        Self {
            versions: RefCell::new(vec![Version {
                observation: PolicyObservation::new(app.clone(), revision(7), policy, deadline)
                    .unwrap(),
                current_deadline: deadline,
                invalidated: false,
            }]),
            selected: Cell::new(0),
            failure: Cell::new(None),
            observations: Cell::new(0),
            observed: RefCell::new(None),
            resume: RefCell::new(None),
            validated: RefCell::new(None),
        }
    }

    fn observation(&self) -> PolicyObservation {
        self.versions.borrow()[self.selected.get()]
            .observation
            .clone()
    }

    fn pause_observation(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (notify, observed) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        *self.observed.borrow_mut() = Some(notify);
        *self.resume.borrow_mut() = Some(resume);
        (observed, release)
    }

    fn next_validation(&self) -> oneshot::Receiver<()> {
        let (notify, validated) = oneshot::channel();
        *self.validated.borrow_mut() = Some(notify);
        validated
    }

    fn change(&self, change: Change) {
        let previous = self.observation();
        let mut versions = self.versions.borrow_mut();
        for version in &mut *versions {
            if matches!(change, Change::ShortenRestore) {
                version.current_deadline = Instant::now();
            }
            version.invalidated = true;
        }
        if matches!(change, Change::Revoke) {
            self.failure.set(Some(Error::Unavailable));
            return;
        }
        let mut policy = previous.policy().clone();
        let next_revision = if matches!(change, Change::Replace) {
            policy.max_running += 1;
            revision(previous.revision().get() + 1)
        } else {
            previous.revision()
        };
        let expires = if matches!(change, Change::Reobserve) {
            previous.expires_at()
        } else {
            previous.expires_at() + LONG
        };
        versions.push(Version {
            observation: PolicyObservation::new(
                previous.app_id().clone(),
                next_revision,
                policy,
                expires,
            )
            .unwrap(),
            current_deadline: expires,
            invalidated: false,
        });
        self.selected.set(versions.len() - 1);
    }

    fn extend(&self) {
        let previous = self.observation();
        let expires = previous.expires_at() + LONG;
        let mut versions = self.versions.borrow_mut();
        // A fresh authoritative observation can extend current source validity.
        // The manager must still retain every earlier observation's own deadline.
        for version in versions.iter_mut().filter(|version| !version.invalidated) {
            version.current_deadline = expires;
        }
        versions.push(Version {
            observation: PolicyObservation::new(
                previous.app_id().clone(),
                previous.revision(),
                previous.policy().clone(),
                expires,
            )
            .unwrap(),
            current_deadline: expires,
            invalidated: false,
        });
        self.selected.set(versions.len() - 1);
    }
}

impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        _: &'a AppId,
    ) -> Pin<Box<dyn Future<Output = Result<PolicyObservation, Error>> + 'a>> {
        Box::pin(async move {
            self.observations.set(self.observations.get() + 1);
            if let Some(error) = self.failure.get() {
                return Err(error);
            }
            let observation = self.observation();
            let notify = self.observed.borrow_mut().take();
            let resume = self.resume.borrow_mut().take();
            if let Some(notify) = notify {
                let _ = notify.send(());
            }
            if let Some(resume) = resume {
                resume.await.map_err(|_| Error::Unavailable)?;
            }
            Ok(observation)
        })
    }

    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error> {
        if let Some(error) = self.failure.get() {
            return Err(error);
        }
        let deadline = {
            let versions = self.versions.borrow();
            let version = versions
                .iter()
                .find(|version| version.observation.same_observation(observation))
                .ok_or(Error::Unavailable)?;
            if version.invalidated {
                return Err(Error::Unavailable);
            }
            version.current_deadline
        };
        let notify = self.validated.borrow_mut().take();
        if let Some(notify) = notify {
            let _ = notify.send(());
        }
        Ok(deadline)
    }
}

#[derive(Clone, Copy)]
enum Change {
    Revoke,
    Replace,
    ShortenRestore,
    Reobserve,
}

struct Host {
    coordinator: Coordinator,
    scope: AssignedScope,
    request: PolicyLeaseRequest,
    worker: WorkerId,
}

async fn host(fixture: &Fixture, options: Options) -> Host {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let coordinator = support::coordinator(&queue, options);
    let worker = WorkerId::mint();
    coordinator
        .register(
            &worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(1).unwrap(),
                state: WorkerState::Ready,
            },
        )
        .await
        .unwrap();
    let Placed::Assigned(assignment) = coordinator.place(&AppId::mint()).await.unwrap() else {
        panic!("the app has one eligible worker");
    };
    let scope = AssignedScope {
        app_id: assignment.app_id,
        assignment_revision: assignment.revision,
    };
    Host {
        coordinator,
        worker,
        request: plain(&scope),
        scope,
    }
}

#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = assignments)]
struct PlacementState {
    revision: i64,
    expires_at: i64,
    released: bool,
}
#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = workers)]
struct RegistrationState {
    expires_at: i64,
    capacity: i64,
    state: String,
}

async fn state(database: &Database, host: &Host) -> (PlacementState, RegistrationState) {
    let placement = database
        .entity::<assignments::Entity>()
        .unwrap()
        .query()
        .filter(
            assignments::app_id
                .eq(host.scope.app_id.as_str())
                .unwrap()
                .and(assignments::worker_id.eq(host.worker.as_str()).unwrap()),
        )
        .first::<PlacementState>()
        .await
        .unwrap()
        .unwrap();
    let worker = database
        .entity::<workers::Entity>()
        .unwrap()
        .query()
        .filter(workers::id.eq(host.worker.as_str()).unwrap())
        .first::<RegistrationState>()
        .await
        .unwrap()
        .unwrap();
    (placement, worker)
}

async fn exact_tuple(fixture: &Fixture) {
    let host = host(fixture, Options::default()).await;
    let database = fixture.database().await;
    let before = state(&database, &host).await;
    let policy = AppPolicy {
        admission: false,
        dispatch: false,
        ingress: false,
        ..AppPolicy::default()
    };
    let source = Source::new(&host.scope.app_id, policy.clone(), until(LONG));
    for _ in 0..2 {
        let grant = host
            .coordinator
            .policy_lease(&host.worker, KEY, &host.request, &source, || {
                ready(Ok(host.worker.clone()))
            })
            .await
            .unwrap();
        let lease = grant.lease().unwrap();
        assert_eq!(lease.app_id, host.scope.app_id);
        assert_eq!(lease.worker_id, host.worker);
        assert_eq!(lease.signing_key_id, KEY);
        assert_eq!(lease.assignment_revision, host.scope.assignment_revision);
        assert_eq!(lease.policy_revision, revision(7));
        assert_eq!(lease.policy, policy);
        assert!(lease.remaining_ms.get() > 0);
        assert_eq!(
            serde_json::from_value::<zeroship_core::workflow_policy::PolicyLease>(
                serde_json::to_value(&lease).unwrap()
            )
            .unwrap(),
            lease
        );
    }
    assert_eq!(state(&database, &host).await, before);
}

async fn authority_caps(fixture: &Fixture) {
    for (source_ms, policy_ms, worker_ms, placement_ms) in [
        (2_000, 20_000, 30_000, 30_000),
        (30_000, 2_000, 30_000, 30_000),
        (30_000, 20_000, 2_000, 30_000),
        (30_000, 20_000, 30_000, 2_000),
    ] {
        let host = host(
            fixture,
            Options {
                worker_ttl: Duration::from_millis(worker_ms),
                assignment_ttl: Duration::from_millis(placement_ms),
                ..Options::default()
            },
        )
        .await;
        let database = fixture.database().await;
        let before = state(&database, &host).await;
        let source = Source::new(
            &host.scope.app_id,
            AppPolicy {
                lease_ms: i64::try_from(policy_ms).unwrap(),
                ..AppPolicy::default()
            },
            until(Duration::from_millis(source_ms)),
        );
        let grant = host
            .coordinator
            .policy_lease(&host.worker, KEY, &host.request, &source, || {
                ready(Ok(host.worker.clone()))
            })
            .await
            .unwrap();
        let remaining = grant.lease().unwrap().remaining_ms.get();
        assert!(remaining > 0);
        assert!(remaining <= source_ms.min(policy_ms).min(worker_ms).min(placement_ms));
        assert_eq!(state(&database, &host).await, before);
    }
}

async fn refusals(fixture: &Fixture) {
    let host = host(fixture, Options::default()).await;
    let source = Source::new(&host.scope.app_id, AppPolicy::default(), until(LONG));
    for (worker, scope) in [
        (WorkerId::mint(), host.scope.clone()),
        (
            host.worker.clone(),
            AssignedScope {
                app_id: AppId::mint(),
                ..host.scope.clone()
            },
        ),
        (
            host.worker.clone(),
            AssignedScope {
                assignment_revision: revision(host.scope.assignment_revision.get() + 1),
                ..host.scope.clone()
            },
        ),
    ] {
        let result = host
            .coordinator
            .policy_lease(&worker, KEY, &plain(&scope), &source, || ready(Ok(worker.clone())))
            .await;
        assert!(matches!(result, Err(Error::Denied)), "{result:?}");
    }
    assert_eq!(source.observations.get(), 0);
    let wrong = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(WorkerId::mint()))
        })
        .await;
    assert!(matches!(wrong, Err(Error::Denied)));
    assert_eq!(source.observations.get(), 0);
    let missing_key = host
        .coordinator
        .policy_lease(&host.worker, "", &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        })
        .await;
    assert!(matches!(missing_key, Err(Error::Invalid)));
    assert_eq!(source.observations.get(), 0);
    source.failure.set(Some(Error::Denied));
    let unavailable = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        })
        .await;
    assert!(matches!(unavailable, Err(Error::Unavailable)));
    let foreign = Source::new(&AppId::mint(), AppPolicy::default(), until(LONG));
    let mismatched = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &foreign, || {
            ready(Ok(host.worker.clone()))
        })
        .await;
    assert!(matches!(mismatched, Err(Error::Unavailable)));
}

async fn held_app_lock(fixture: &Fixture) {
    for change in [
        Change::Revoke,
        Change::Replace,
        Change::ShortenRestore,
        Change::Reobserve,
    ] {
        let host = host(fixture, Options::default()).await;
        let database = fixture.database().await;
        let before = state(&database, &host).await;
        let source = Source::new(&host.scope.app_id, AppPolicy::default(), until(LONG));
        let (observed, release_source) = source.pause_observation();
        let validated = source.next_validation();
        let calls = Cell::new(0);
        let issue = host
            .coordinator
            .policy_lease(&host.worker, KEY, &host.request, &source, || {
                calls.set(calls.get() + 1);
                ready(Ok(host.worker.clone()))
            });
        let hold = async {
            observed.await.unwrap();
            let source = &source;
            let calls = &calls;
            let app = &host.scope.app_id;
            database
                .transaction(|tx| async move {
                    assert_eq!(
                        tx.entity::<queue_scopes::Entity>()?
                            .update_many(
                                queue_scopes::id.eq(app.as_str())?,
                                queue_scopes::lock_version.set(0_i64)?,
                            )
                            .await?,
                        1,
                    );
                    release_source.send(()).unwrap();
                    validated.await.unwrap();
                    assert_eq!(
                        calls.get(),
                        1,
                        "final enrollment cannot pass the held app lock"
                    );
                    source.change(change);
                    Ok(())
                })
                .await
                .unwrap();
        };
        let (result, ()) = compio::time::timeout(Duration::from_secs(5), async {
            futures::join!(issue, hold)
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(Error::Unavailable)), "{result:?}");
        assert_eq!(state(&database, &host).await, before);
    }
}

struct Enrollment {
    worker: WorkerId,
    calls: Cell<usize>,
    wait_at: usize,
    entered: RefCell<Option<oneshot::Sender<()>>>,
    resume: RefCell<Option<oneshot::Receiver<()>>>,
    failure: Cell<Option<Error>>,
}
impl Enrollment {
    fn new(
        worker: &WorkerId,
        wait_at: usize,
    ) -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (notify, entered) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        (
            Self {
                worker: worker.clone(),
                calls: Cell::new(0),
                wait_at,
                entered: RefCell::new(Some(notify)),
                resume: RefCell::new(Some(resume)),
                failure: Cell::new(None),
            },
            entered,
            release,
        )
    }
    async fn authorize(&self) -> Result<WorkerId, Error> {
        let call = self.calls.get() + 1;
        self.calls.set(call);
        if call == self.wait_at {
            let notify = self.entered.borrow_mut().take().unwrap();
            let resume = self.resume.borrow_mut().take().unwrap();
            notify.send(()).unwrap();
            resume.await.map_err(|_| Error::Unavailable)?;
        }
        self.failure
            .get()
            .map_or_else(|| Ok(self.worker.clone()), Err)
    }
}

async fn enrollment_waits(fixture: &Fixture) {
    for wait_at in [2, 3] {
        for change in [
            Change::Revoke,
            Change::Replace,
            Change::ShortenRestore,
            Change::Reobserve,
        ] {
            let host = host(fixture, Options::default()).await;
            let source = Source::new(&host.scope.app_id, AppPolicy::default(), until(LONG));
            let (enrollment, entered, release) = Enrollment::new(&host.worker, wait_at);
            let issue =
                host.coordinator
                    .policy_lease(&host.worker, KEY, &host.request, &source, || {
                        enrollment.authorize()
                    });
            let invalidate = async {
                entered.await.unwrap();
                source.change(change);
                release.send(()).unwrap();
            };
            let (result, ()) = futures::join!(issue, invalidate);
            assert!(matches!(result, Err(Error::Unavailable)), "{result:?}");
        }
        let host = host(fixture, Options::default()).await;
        let source = Source::new(&host.scope.app_id, AppPolicy::default(), until(LONG));
        let (enrollment, entered, release) = Enrollment::new(&host.worker, wait_at);
        let issue = host
            .coordinator
            .policy_lease(&host.worker, KEY, &host.request, &source, || {
                enrollment.authorize()
            });
        let invalidate = async {
            entered.await.unwrap();
            enrollment.failure.set(Some(Error::Unavailable));
            release.send(()).unwrap();
        };
        let (result, ()) = futures::join!(issue, invalidate);
        assert!(matches!(result, Err(Error::Unavailable)), "{result:?}");
    }
}

async fn original_source_expiry(fixture: &Fixture) {
    let host = host(fixture, Options::default()).await;
    let source = Source::new(
        &host.scope.app_id,
        AppPolicy::default(),
        until(Duration::from_millis(700)),
    );
    let (enrollment, entered, release) = Enrollment::new(&host.worker, 2);
    let issue = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            enrollment.authorize()
        });
    let extend = async {
        entered.await.unwrap();
        source.extend();
    };
    let (result, ()) = compio::time::timeout(Duration::from_secs(3), async {
        futures::join!(issue, extend)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Timeout)), "{result:?}");
    assert!(
        release.send(()).is_err(),
        "expired issuance must drop its pending enrollment wait"
    );
    let fresh = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        })
        .await
        .unwrap();
    assert!(fresh.lease().unwrap().remaining_ms.get() > 700);
}

async fn extensions(fixture: &Fixture) {
    let host = host(
        fixture,
        Options {
            worker_ttl: LONG,
            assignment_ttl: Duration::from_secs(3),
            ..Options::default()
        },
    )
    .await;
    let source = Source::new(
        &host.scope.app_id,
        AppPolicy::default(),
        until(Duration::from_secs(2)),
    );
    let original = source.observation();
    let (observed, release) = source.pause_observation();
    let issue = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        });
    let renew = async {
        observed.await.unwrap();
        source.extend();
        host.coordinator
            .renew(&host.worker, &host.scope)
            .await
            .unwrap();
        release.send(()).unwrap();
    };
    let (result, ()) = futures::join!(issue, renew);
    let grant = result.unwrap();
    assert!(grant.lease().unwrap().remaining_ms.get() <= 2_000);
    assert_eq!(original.expires_at(), original.clone().expires_at());

    let database = fixture.database().await;
    let source = Source::new(&host.scope.app_id, AppPolicy::default(), until(LONG));
    let (observed, release) = source.pause_observation();
    let before = state(&database, &host).await;
    let issue = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        });
    let extend_placement = async {
        observed.await.unwrap();
        assert_eq!(
            database
                .entity::<assignments::Entity>()
                .unwrap()
                .update_many(
                    assignments::app_id
                        .eq(host.scope.app_id.as_str())
                        .unwrap()
                        .and(assignments::worker_id.eq(host.worker.as_str()).unwrap()),
                    assignments::expires_at
                        .set(before.0.expires_at + 60_000)
                        .unwrap(),
                )
                .await
                .unwrap(),
            1
        );
        release.send(()).unwrap();
    };
    let (result, ()) = futures::join!(issue, extend_placement);
    assert!(result.unwrap().lease().unwrap().remaining_ms.get() <= 3_000);
}

async fn retained_grants(fixture: &Fixture) {
    let host = host(fixture, Options::default()).await;
    for change in [
        Change::Revoke,
        Change::Replace,
        Change::ShortenRestore,
        Change::Reobserve,
    ] {
        let source = Source::new(&host.scope.app_id, AppPolicy::default(), until(LONG));
        let grant = host
            .coordinator
            .policy_lease(&host.worker, KEY, &host.request, &source, || {
                ready(Ok(host.worker.clone()))
            })
            .await
            .unwrap();
        let cloned = grant.clone();
        grant.lease().unwrap();
        source.change(change);
        assert_eq!(grant.lease(), Err(Error::Unavailable));
        assert_eq!(cloned.lease(), Err(Error::Unavailable));
        if !matches!(change, Change::Revoke) {
            host.coordinator
                .policy_lease(&host.worker, KEY, &host.request, &source, || {
                    ready(Ok(host.worker.clone()))
                })
                .await
                .unwrap()
                .lease()
                .unwrap();
            assert_eq!(grant.lease(), Err(Error::Unavailable));
        }
    }
    let source = Source::new(
        &host.scope.app_id,
        AppPolicy::default(),
        until(Duration::from_millis(500)),
    );
    let original = source.observation();
    let grant = host
        .coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        })
        .await
        .unwrap();
    let cloned = grant.clone();
    source.extend();
    compio::time::sleep(
        original
            .expires_at()
            .saturating_duration_since(Instant::now())
            + Duration::from_millis(2),
    )
    .await;
    assert_eq!(grant.lease(), Err(Error::Unavailable));
    assert_eq!(cloned.lease(), Err(Error::Unavailable));
    host.coordinator
        .policy_lease(&host.worker, KEY, &host.request, &source, || {
            ready(Ok(host.worker.clone()))
        })
        .await
        .unwrap()
        .lease()
        .unwrap();
}

#[test]
fn observations_reject_invalid_or_expired_raw_authority() {
    let app = AppId::mint();
    let original =
        PolicyObservation::new(app.clone(), revision(1), AppPolicy::default(), until(LONG))
            .unwrap();
    let cloned = original.clone();
    let replacement = PolicyObservation::new(
        app.clone(),
        original.revision(),
        original.policy().clone(),
        original.expires_at(),
    )
    .unwrap();
    assert!(original.same_observation(&cloned));
    assert_eq!(original.expires_at(), cloned.expires_at());
    assert!(!original.same_observation(&replacement));
    assert!(matches!(
        PolicyObservation::new(
            app.clone(),
            revision(1),
            AppPolicy::default(),
            Instant::now()
        ),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        PolicyObservation::new(
            app,
            revision(1),
            AppPolicy {
                lease_ms: 0,
                ..AppPolicy::default()
            },
            until(LONG)
        ),
        Err(Error::Unavailable)
    ));
}

/// A plain refresh: it never establishes or reports ingress.
fn plain(scope: &AssignedScope) -> PolicyLeaseRequest {
    PolicyLeaseRequest {
        scope: scope.clone(),
        establish: None,
        ingress_used: false,
    }
}
