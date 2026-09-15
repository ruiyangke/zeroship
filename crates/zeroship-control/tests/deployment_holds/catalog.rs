//! One bounded catalog database shared by every deploy, archive and restore.
//!
//! Each case gates catalog transactions on an advisory lock the test holds, so
//! the sessions they occupy can be observed in `pg_stat_activity` while they
//! wait, then releases the gate and checks every outcome.

use super::*;
use futures::future::join_all;
use std::{cell::Cell, collections::BTreeSet, num::NonZeroUsize};
use zeroship_control::publication::{
    catalog::APPLICATION_NAME, Acceptance, CatalogError, CatalogOptions,
};

use super::deployment_commands::{deploy, labelled};

const GATE: i64 = 73_921_901;

async fn app(fixture: &Fixture, name: &str) -> AppId {
    let organization = OrganizationId::mint();
    let project = ProjectId::mint();
    let app = AppId::mint();
    let admin = &fixture.platform.admin;
    admin
        .execute(
            "INSERT INTO zeroship.organizations(id,slug,name,billing_email) \
             VALUES($1,$2::citext,$2,$3::citext)",
            &[&organization.as_str(), &name, &format!("{name}@zeroship.test")],
        )
        .await
        .unwrap();
    admin
        .execute(
            "INSERT INTO zeroship.projects(id,organization_id,slug,name) \
             VALUES($1,$2,'default','Catalog Project')",
            &[&project.as_str(), &organization.as_str()],
        )
        .await
        .unwrap();
    admin
        .execute(
            "INSERT INTO zeroship.apps(id,name,plan_id,project_id,organization_id) \
             VALUES($1,$2,$3,$4,$5)",
            &[
                &app.as_str(),
                &name,
                &zeroship_control::plan_catalog::free_plan_id(),
                &project.as_str(),
                &organization.as_str(),
            ],
        )
        .await
        .unwrap();
    app
}

/// A registry whose shared catalog holds at most `bound` sessions, and the
/// catalog sessions that were open before it, which are the fixture's own.
async fn bounded(fixture: &Fixture, bound: usize) -> (Registry, BTreeSet<i32>) {
    let before = sessions(fixture).await;
    let registry = Registry::connect(
        &fixture.control_url,
        CatalogOptions {
            max_connections: NonZeroUsize::new(bound).unwrap(),
        },
    )
    .await
    .unwrap();
    (registry, before)
}

async fn sessions(fixture: &Fixture) -> BTreeSet<i32> {
    fixture
        .platform
        .admin
        .query(
            "SELECT pid FROM pg_stat_activity WHERE application_name = $1",
            &[&APPLICATION_NAME],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

/// Hold a gate that every lifecycle intent insert waits on. The insert runs
/// after the app row lock, so a gated transaction holds that lock and its
/// session while it waits.
async fn gate(fixture: &Fixture) -> (compio_postgres::Client, i32) {
    fixture
        .platform
        .admin
        .batch_execute(&format!(
            "CREATE FUNCTION zeroship.gate_catalog_intent() RETURNS trigger
             LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM pg_advisory_xact_lock({GATE});
                 RETURN NEW;
             END $$;
             CREATE TRIGGER gate_catalog_intent
             BEFORE INSERT ON zeroship.app_lifecycle_intents
             FOR EACH ROW EXECUTE FUNCTION zeroship.gate_catalog_intent();"
        ))
        .await
        .unwrap();
    let blocker = platform::connect(&fixture.control_url.replacen("zeroship_control@", "postgres@", 1))
        .await;
    blocker
        .query_one("SELECT pg_advisory_lock($1)", &[&GATE])
        .await
        .unwrap();
    let pid = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    (blocker, pid)
}

async fn release(blocker: &compio_postgres::Client) {
    let released: bool = blocker
        .query_one("SELECT pg_advisory_unlock($1)", &[&GATE])
        .await
        .unwrap()
        .get(0);
    assert!(released);
}

/// Catalog sessions waiting directly on a lock `holder` holds.
async fn blocked_by(fixture: &Fixture, holder: i32) -> BTreeSet<i32> {
    fixture
        .platform
        .admin
        .query(
            "SELECT pid FROM pg_stat_activity \
              WHERE application_name = $1 AND $2 = ANY(pg_blocking_pids(pid))",
            &[&APPLICATION_NAME, &holder],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

/// Catalog sessions whose wait ends at `holder`, however many waiters stand
/// between them. `pg_blocking_pids` names only a session's direct blockers,
/// and a queue on one row puts each later waiter behind the one before it,
/// so a direct count sees one waiter however long the queue is.
async fn waiting_on(fixture: &Fixture, holder: i32) -> BTreeSet<i32> {
    fixture
        .platform
        .admin
        .query(
            "WITH RECURSIVE chain(pid, blocker) AS ( \
                 SELECT a.pid, b.blocker \
                   FROM pg_stat_activity a, \
                        LATERAL unnest(pg_blocking_pids(a.pid)) AS b(blocker) \
                  WHERE a.application_name = $1 \
                 UNION \
                 SELECT c.pid, n.blocker \
                   FROM chain c, LATERAL unnest(pg_blocking_pids(c.blocker)) AS n(blocker) \
             ) \
             SELECT DISTINCT pid FROM chain WHERE blocker = $2",
            &[&APPLICATION_NAME, &holder],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

/// What every catalog session is doing, so a wait that never ends says why.
async fn catalog_state(fixture: &Fixture) -> Vec<String> {
    fixture
        .platform
        .admin
        .query(
            "SELECT format('pid=%s state=%s wait=%s/%s blocked_by=%s query=%s', \
                           pid, state, coalesce(wait_event_type,'-'), \
                           coalesce(wait_event,'-'), pg_blocking_pids(pid)::text, \
                           left(regexp_replace(query, '\\s+', ' ', 'g'), 120)) \
               FROM pg_stat_activity WHERE application_name = $1 ORDER BY pid",
            &[&APPLICATION_NAME],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

async fn until<F, Fut>(fixture: &Fixture, what: &str, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if compio::time::timeout(Duration::from_secs(20), async {
        while !ready().await {
            compio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_err()
    {
        let state = catalog_state(fixture).await;
        panic!("timed out waiting until {what}; catalog sessions: {state:#?}");
    }
}

async fn revisions(fixture: &Fixture, app: &AppId) -> Vec<i64> {
    fixture
        .platform
        .admin
        .query(
            "SELECT revision FROM zeroship.app_lifecycle_intents WHERE app_id=$1 ORDER BY revision",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

/// Deploys to more apps than the bound all succeed, and the catalog never
/// holds more sessions than the bound while the surplus waits for one.
#[ntex::test]
async fn concurrent_deploys_to_many_apps_share_the_session_bound() {
    const BOUND: usize = 2;
    let fixture = Fixture::new().await;
    let actor = fixture.actor().await;
    let mut apps = Vec::new();
    for index in 0..BOUND * 3 {
        apps.push(app(&fixture, &format!("catalog-bound-{index}")).await);
    }
    let (registry, shared) = bounded(&fixture, BOUND).await;
    let (blocker, gate_holder) = gate(&fixture).await;
    let done = Cell::new(false);
    let most = Cell::new(0_usize);
    let deploys = async {
        let outcomes = join_all(
            apps.iter()
                .map(|app| deploy(&registry, app, &actor, labelled(app.as_str()))),
        )
        .await;
        done.set(true);
        outcomes
    };
    let observe = async {
        let own = || async { sessions(&fixture).await.difference(&shared).count() };
        let sample = || async {
            let held = own().await;
            most.set(most.get().max(held));
            held
        };
        // Every session the bound allows is busy at the gate at once.
        until(&fixture, "every catalog session waits at the gate", || async {
            sample().await;
            blocked_by(&fixture, gate_holder).await.len() == BOUND
        })
        .await;
        // The surplus deploys wait for a session instead of opening one.
        for _ in 0..40 {
            sample().await;
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        release(&blocker).await;
        while !done.get() {
            sample().await;
            compio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    let (outcomes, ()) = futures::join!(deploys, observe);
    for (app, outcome) in apps.iter().zip(outcomes) {
        match outcome {
            Ok(Acceptance::Accepted(result)) => {
                assert_eq!(result.lifecycle_revision.map(|r| r.get()), Some(1), "{app:?}");
            }
            other => panic!("deploy to {app:?} did not succeed: {other:?}"),
        }
        assert_eq!(revisions(&fixture, app).await, [1]);
    }
    assert!(
        most.get() <= BOUND,
        "the catalog held {} sessions under a bound of {BOUND}",
        most.get()
    );
}

/// Concurrent deploys to one app still serialize on the app row lock when the
/// catalog has a session for each of them, and receive consecutive revisions.
#[ntex::test]
async fn concurrent_deploys_to_one_app_serialize_on_its_row_lock() {
    const DEPLOYS: usize = 3;
    let fixture = Fixture::new().await;
    let actor = fixture.actor().await;
    let target = app(&fixture, "catalog-serial").await;
    let (registry, _) = bounded(&fixture, DEPLOYS).await;
    let (blocker, gate_holder) = gate(&fixture).await;
    let deploys = join_all((0..DEPLOYS).map(|index| {
        deploy(
            &registry,
            &target,
            &actor,
            labelled(&format!("catalog-serial-{index}")),
        )
    }));
    // Observation never ends the test itself: it always releases the gate, so
    // the deploys finish and report their own outcome beside what it saw.
    let observe = async {
        // One deploy holds the app row and waits at the gate; the others wait
        // for the row, each on a session of its own.
        let gated = std::cell::RefCell::new(BTreeSet::new());
        let (seen, fixture) = (&gated, &fixture);
        until(fixture, "one deploy holds the app lock at the gate", || async move {
            let found = blocked_by(fixture, gate_holder).await;
            let ready = found.len() == 1;
            *seen.borrow_mut() = found;
            ready
        })
        .await;
        let first = gated.into_inner();
        let holder = *first.first().unwrap();
        let waiting = compio::time::timeout(Duration::from_secs(10), async {
            while waiting_on(fixture, holder).await.len() != DEPLOYS - 1 {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        // Taken while the gate still holds the first deploy, so a wait that
        // never arrived is reported as of the moment it should have.
        let state = catalog_state(fixture).await;
        let gated_now = blocked_by(fixture, gate_holder).await;
        release(&blocker).await;
        (waiting.is_ok(), state, gated_now, first)
    };
    let (outcomes, (waited, state, gated_now, first)) = futures::join!(deploys, observe);
    assert!(
        waited,
        "the other deploys never waited for the app lock: {state:#?}"
    );
    assert_eq!(
        gated_now, first,
        "the app lock holder is still the only deploy at the gate"
    );
    let mut granted: Vec<i64> = outcomes
        .into_iter()
        .map(|outcome| match outcome {
            Ok(Acceptance::Accepted(result)) => result.lifecycle_revision.unwrap().get(),
            other => panic!("a serialized deploy did not succeed: {other:?}"),
        })
        .collect();
    granted.sort_unstable();
    assert_eq!(granted, [1, 2, 3]);
    assert_eq!(revisions(&fixture, &target).await, [1, 2, 3]);
}

/// A caller that stops waiting cancels its catalog transaction, as it did when
/// the transaction ran on the caller's own thread: nothing it staged commits.
#[ntex::test]
async fn a_dropped_caller_rolls_back_its_catalog_transaction() {
    let fixture = Fixture::new().await;
    let actor = fixture.actor().await;
    let abandoned = app(&fixture, "catalog-abandoned").await;
    let later = app(&fixture, "catalog-later").await;
    let (registry, _) = bounded(&fixture, 1).await;
    let (blocker, gate_holder) = gate(&fixture).await;
    let waited = compio::time::timeout(Duration::from_secs(20), async {
        let pending = deploy(&registry, &abandoned, &actor, labelled("abandoned"));
        let watch = until(&fixture, "the deploy waits at the gate", || async {
            blocked_by(&fixture, gate_holder).await.len() == 1
        });
        futures::pin_mut!(pending, watch);
        match futures::future::select(pending, watch).await {
            futures::future::Either::Left((outcome, _)) => {
                panic!("the gated deploy finished early: {outcome:?}")
            }
            // Dropping the deploy here abandons it while its transaction waits.
            futures::future::Either::Right(((), _)) => {}
        }
    })
    .await;
    waited.expect("the gated deploy reached the gate");
    release(&blocker).await;
    // The catalog has one session, so this deploy runs only after the
    // abandoned transaction has released it.
    let committed = deploy(&registry, &later, &actor, labelled("later")).await;
    assert!(matches!(committed, Ok(Acceptance::Accepted(_))), "{committed:?}");
    assert_eq!(revisions(&fixture, &later).await, [1]);
    assert!(revisions(&fixture, &abandoned).await.is_empty());
    let row = fixture
        .platform
        .admin
        .query_one(
            "SELECT lifecycle_revision, deploy_hash IS NULL, \
                    (SELECT COUNT(*)::bigint FROM zeroship.app_deploy_commands WHERE app_id=$1) \
               FROM zeroship.apps WHERE id=$1",
            &[&abandoned.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        (row.get::<_, i64>(0), row.get::<_, bool>(1), row.get::<_, i64>(2)),
        (0, true, 0),
        "the abandoned deploy committed"
    );
    // A refusal still reaches a caller that waits for it.
    let absent = deploy(&registry, &AppId::mint(), &actor, labelled("absent")).await;
    assert!(matches!(absent, Err(CatalogError::AppAbsent)), "{absent:?}");
}
