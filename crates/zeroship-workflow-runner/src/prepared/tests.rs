use super::*;
use crate::{ExecutionBudget, TaskExecution};
use futures::FutureExt;
use std::collections::BTreeSet;
use zeroship_workflow::service::TaskAssignment;

/// An executor these cases never start: preparation is what they measure.
struct Unstarted;

impl TaskExecutor for Unstarted {
    fn start(
        &self,
        _: &TaskAssignment,
        _: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        panic!("a preparation case never starts an execution")
    }
}

#[derive(Default)]
struct Factory {
    opened: RefCell<BTreeMap<AppId, usize>>,
    resident: Rc<RefCell<BTreeMap<AppId, usize>>>,
    stalled: RefCell<BTreeSet<AppId>>,
    /// Stalled openings whose future was dropped rather than completed.
    abandoned: Rc<Cell<usize>>,
    unlisted: RefCell<BTreeSet<AppId>>,
}

impl Factory {
    fn opened(&self, app: &AppId) -> usize {
        self.opened.borrow().get(app).copied().unwrap_or(0)
    }

    fn resident(&self, app: &AppId) -> usize {
        self.resident.borrow().get(app).copied().unwrap_or(0)
    }
}

#[derive(Debug)]
struct Resident {
    app: AppId,
    live: Rc<RefCell<BTreeMap<AppId, usize>>>,
}

impl Drop for Resident {
    fn drop(&mut self) {
        *self.live.borrow_mut().get_mut(&self.app).unwrap() -= 1;
    }
}

struct Abandoned(Rc<Cell<usize>>);

impl Drop for Abandoned {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

struct Opener(Rc<Factory>);

impl CreatorFactory for Opener {
    type Journal = ();

    fn open<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<CreatorRuntime<()>, WorkflowServiceError>> {
        async move {
            *self.0.opened.borrow_mut().entry(app.clone()).or_default() += 1;
            if self.0.stalled.borrow().contains(app) {
                let _abandoned = Abandoned(self.0.abandoned.clone());
                std::future::pending::<()>().await;
            }
            *self.0.resident.borrow_mut().entry(app.clone()).or_default() += 1;
            Ok(CreatorRuntime {
                app: (),
                executor: Rc::new(Unstarted),
                residency: Rc::new(Resident {
                    app: app.clone(),
                    live: self.0.resident.clone(),
                }),
            })
        }
        .boxed_local()
    }
}

const LEASE: Duration = Duration::from_secs(30);

fn cache(capacity: usize, operation_timeout: Duration) -> (Rc<Factory>, PreparedApps<Opener>) {
    let factory = Rc::new(Factory::default());
    let listed = factory.clone();
    let apps = PreparedApps::new(
        Opener(factory.clone()),
        Rc::new(move |app: &AppId| !listed.unlisted.borrow().contains(app)),
        PreparedOptions {
            capacity,
            operation_timeout,
        },
    )
    .unwrap();
    (factory, apps)
}

#[compio::test]
async fn a_first_delivery_prepares_its_app_and_a_second_reuses_the_entry() {
    let (factory, apps) = cache(4, LEASE);
    let app = AppId::mint();
    let first = apps.get_or_prepare(&app, LEASE).await.unwrap();
    let second = apps.get_or_prepare(&app, LEASE).await.unwrap();
    assert!(Rc::ptr_eq(&first, &second));
    assert_eq!(first.app_id(), &app);
    assert_eq!(factory.opened(&app), 1);
    assert_eq!(factory.resident(&app), 1);
}

/// Eviction takes an idle entry, the least recently used of the idle ones,
/// and never one an execution holds: the held entry is the oldest here, so an
/// eviction that ignored holders would have taken it.
#[compio::test]
async fn eviction_takes_only_idle_entries_and_a_held_entry_stays_resident() {
    let (factory, apps) = cache(2, LEASE);
    let [held, idle, next, after, last] = std::array::from_fn(|_| AppId::mint());
    let executing = apps.get_or_prepare(&held, LEASE).await.unwrap();
    drop(apps.get_or_prepare(&idle, LEASE).await.unwrap());
    drop(apps.get_or_prepare(&next, LEASE).await.unwrap());
    assert_eq!(factory.resident(&held), 1, "an executing entry was evicted");
    assert_eq!(factory.resident(&idle), 0, "the idle entry was not the one evicted");
    assert_eq!(factory.resident(&next), 1);

    // Once its execution finishes, the held entry is idle and the oldest.
    drop(executing);
    let kept = apps.get_or_prepare(&after, LEASE).await.unwrap();
    assert_eq!(factory.resident(&held), 0);
    assert_eq!(factory.resident(&next), 1);

    // Every entry executing: nothing can be evicted, so nothing is cached and
    // the new app's resources go with the refusal.
    let also = apps.get_or_prepare(&next, LEASE).await.unwrap();
    assert!(matches!(
        apps.get_or_prepare(&last, LEASE).await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    assert_eq!(factory.opened(&last), 1);
    assert_eq!(factory.resident(&last), 0);
    drop((kept, also));
}

/// The version feed's prune drops the cache's reference even while an
/// execution holds the entry; that execution keeps the app resident until it
/// lets go. The control is a listed app, which stays cached.
#[compio::test]
async fn a_pruned_entry_stays_resident_while_an_execution_holds_it() {
    let (factory, apps) = cache(4, LEASE);
    let deleted = AppId::mint();
    let listed = AppId::mint();
    let executing = apps.get_or_prepare(&deleted, LEASE).await.unwrap();
    drop(apps.get_or_prepare(&listed, LEASE).await.unwrap());
    factory.unlisted.borrow_mut().insert(deleted.clone());
    apps.prune();
    assert_eq!(factory.resident(&deleted), 1);
    drop(executing);
    assert_eq!(
        factory.resident(&deleted),
        0,
        "the cache still held the app the feed dropped"
    );
    assert_eq!(factory.resident(&listed), 1);
    drop(apps.get_or_prepare(&listed, LEASE).await.unwrap());
    assert_eq!(factory.opened(&listed), 1, "the listed app was pruned too");
}

/// A miss runs under the delivery's remaining lease, never longer: a
/// preparation that outlives it is dropped and nothing is cached. The host's
/// operation bound caps it the same way when that is the shorter one, and a
/// spent lease opens nothing at all.
#[compio::test]
async fn a_preparation_is_bounded_by_the_delivery_lease() {
    let operation = Duration::from_secs(5);
    let (factory, apps) = cache(4, operation);
    let app = AppId::mint();
    factory.stalled.borrow_mut().insert(app.clone());
    let remaining = Duration::from_millis(100);
    let started = Instant::now();
    assert_eq!(
        apps.get_or_prepare(&app, remaining).await.unwrap_err(),
        WorkflowServiceError::Timeout
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= remaining);
    assert!(
        elapsed < operation / 2,
        "the preparation outlived the delivery's lease: {elapsed:?}"
    );
    assert_eq!(factory.abandoned.get(), 1, "the stalled opening was not dropped");
    assert_eq!(factory.resident(&app), 0);

    let (factory, apps) = cache(4, remaining);
    factory.stalled.borrow_mut().insert(app.clone());
    let started = Instant::now();
    assert_eq!(
        apps.get_or_prepare(&app, LEASE).await.unwrap_err(),
        WorkflowServiceError::Timeout
    );
    assert!(started.elapsed() < operation / 2);

    assert_eq!(
        apps.get_or_prepare(&AppId::mint(), Duration::ZERO)
            .await
            .unwrap_err(),
        WorkflowServiceError::Timeout
    );
    assert_eq!(factory.opened.borrow().len(), 1, "a spent lease opened an app");

    // Nothing was cached for the app whose opening was dropped.
    factory.stalled.borrow_mut().clear();
    drop(apps.get_or_prepare(&app, LEASE).await.unwrap());
    assert_eq!(factory.opened(&app), 2);
}

#[test]
fn prepared_apps_refuse_an_empty_or_unrepresentable_bound() {
    for (capacity, operation_timeout) in [
        (0, LEASE),
        (1, Duration::ZERO),
        (1, Duration::MAX),
    ] {
        assert!(matches!(
            PreparedApps::new(
                Opener(Rc::new(Factory::default())),
                Rc::new(|_: &AppId| true),
                PreparedOptions {
                    capacity,
                    operation_timeout,
                },
            ),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    assert!(PreparedApps::new(
        Opener(Rc::new(Factory::default())),
        Rc::new(|_: &AppId| true),
        PreparedOptions {
            capacity: 1,
            operation_timeout: LEASE,
        },
    )
    .is_ok());
}
