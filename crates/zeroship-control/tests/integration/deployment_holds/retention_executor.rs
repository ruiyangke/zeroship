//! The retention executor: the bounded sessions deployment holds run on, apart
//! from the publication catalog's.
//!
//! These cases observe the sessions holds occupy in `pg_stat_activity`, the
//! order an executor's lanes admit, and a queued operation whose caller has
//! gone.

use super::*;
use futures::{channel::oneshot, future::join_all, FutureExt as _};
use std::{
    cell::Cell,
    collections::BTreeMap,
    num::NonZeroUsize,
};
use zeroship_control::{
    publication::{Catalog, CatalogError, CatalogOptions, CatalogRole},
    sessions,
};
use zeroship_workflow_manager::deployments::DeploymentHolds;

const GATE: i64 = 73_921_911;

async fn executor(fixture: &Fixture, bound: usize) -> Catalog {
    Catalog::start(
        &fixture.control_url,
        CatalogRole::Retention,
        CatalogOptions {
            max_connections: NonZeroUsize::new(bound).unwrap(),
        },
    )
    .await
    .unwrap()
}

/// Every session Control's login holds in this database, by the name it
/// announced.
async fn control_sessions(fixture: &Fixture) -> BTreeMap<i32, String> {
    fixture
        .platform
        .admin
        .query(
            "SELECT pid, coalesce(application_name, '') FROM pg_stat_activity \
              WHERE usename = 'zeroship_control' AND datname = current_database()",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// Retention sessions waiting directly on a lock `holder` holds.
async fn gated(fixture: &Fixture, holder: i32) -> usize {
    fixture
        .platform
        .admin
        .query(
            "SELECT pid FROM pg_stat_activity \
              WHERE application_name = $1 AND $2 = ANY(pg_blocking_pids(pid))",
            &[&sessions::RETENTION, &holder],
        )
        .await
        .unwrap()
        .len()
}

/// Make every hold insert wait on an advisory lock a superuser session holds,
/// and return that session and its pid.
async fn gate_hold_inserts(fixture: &Fixture) -> (compio_postgres::Client, i32) {
    fixture
        .platform
        .admin
        .batch_execute(&format!(
            "CREATE FUNCTION zeroship.gate_hold_insert() RETURNS trigger
             LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM pg_advisory_xact_lock({GATE});
                 RETURN NEW;
             END $$;
             CREATE TRIGGER gate_hold_insert
             BEFORE INSERT ON zeroship.app_deploy_holds
             FOR EACH ROW EXECUTE FUNCTION zeroship.gate_hold_insert();"
        ))
        .await
        .unwrap();
    let blocker = platform::connect(fixture.platform.admin_url.as_str()).await;
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

/// Asserted holds on more deployments than the executor has lanes all
/// succeed. While they wait at the gate, every session they occupy is a
/// retention session, and never more of them than the bound.
#[compio::test(crate = "crate::support::live::system")]
async fn concurrent_holds_occupy_only_retention_sessions() {
    const BOUND: usize = 2;
    let fixture = Fixture::new().await;
    let mut deployments = Vec::new();
    for index in 0..BOUND * 2 {
        deployments.push(fixture.deployment(&format!("retention-bound-{index}")).await);
    }
    // The fixture's own sessions, open before the executor under test.
    let before = control_sessions(&fixture).await;
    let api = DeploymentHoldApi::new(executor(&fixture, BOUND).await);
    let opened: BTreeMap<i32, String> = control_sessions(&fixture)
        .await
        .into_iter()
        .filter(|(pid, _)| !before.contains_key(pid))
        .collect();
    // The instrument sees the executor: it opened its sessions at start.
    assert_eq!(
        opened.values().filter(|name| *name == sessions::RETENTION).count(),
        BOUND,
        "the executor's own sessions: {opened:?}"
    );
    let (blocker, holder) = gate_hold_inserts(&fixture).await;
    let done = Cell::new(false);
    let seen = std::cell::RefCell::new(BTreeMap::<i32, String>::new());
    let most = Cell::new(0_usize);
    let requests: Vec<HoldRequest> = deployments
        .iter()
        .map(|(app, deploy, _)| HoldRequest {
            app_id: app.clone(),
            deploy_id: deploy.clone(),
            generation: generation(1),
        })
        .collect();
    let holds = async {
        let outcomes = join_all(requests.iter().map(|request| api.acquire_asserted(request))).await;
        done.set(true);
        outcomes
    };
    let observe = async {
        let sample = || async {
            let now: BTreeMap<i32, String> = control_sessions(&fixture)
                .await
                .into_iter()
                .filter(|(pid, _)| !before.contains_key(pid))
                .collect();
            most.set(most.get().max(now.len()));
            seen.borrow_mut().extend(now);
        };
        let reached = compio::time::timeout(Duration::from_secs(10), async {
            loop {
                sample().await;
                if gated(&fixture, holder).await >= BOUND {
                    break;
                }
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        // The surplus holds wait for a lane instead of opening a session.
        for _ in 0..20 {
            sample().await;
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        let released: bool = blocker
            .query_one("SELECT pg_advisory_unlock($1)", &[&GATE])
            .await
            .unwrap()
            .get(0);
        assert!(released);
        while !done.get() {
            sample().await;
            compio::time::sleep(Duration::from_millis(2)).await;
        }
        reached.is_ok()
    };
    let (outcomes, reached) = futures::join!(holds, observe);
    assert!(
        reached,
        "the executor's lanes never all waited at the gate: {:?}",
        seen.borrow()
    );
    for ((app, deploy, _), outcome) in deployments.iter().zip(outcomes) {
        let receipt = outcome.unwrap_or_else(|error| panic!("hold on {deploy} refused: {error}"));
        assert_eq!(receipt.app_id, *app);
        assert_eq!(receipt.state, HoldState::Held);
    }
    let seen = seen.into_inner();
    assert!(
        seen.values().all(|name| name == sessions::RETENTION),
        "holds occupied a session that is not the retention executor's: {seen:?}"
    );
    assert!(
        most.get() <= BOUND,
        "holds occupied {} sessions under a bound of {BOUND}: {seen:?}",
        most.get()
    );
    assert_eq!(fixture.rows().await.len(), deployments.len());
}

/// An operation whose caller stops waiting while it is still queued behind
/// another is never built, so it never begins a transaction. The same
/// operation awaited to completion is built, which is what makes its absence
/// above a result.
#[compio::test(crate = "crate::support::live::system")]
async fn an_operation_abandoned_in_the_queue_never_starts() {
    let fixture = Fixture::new().await;
    let executor = executor(&fixture, 1).await;
    let (entered, running) = oneshot::channel::<()>();
    let (release, released) = oneshot::channel::<()>();
    let busy = executor.run(move |_database| async move {
        entered.send(()).unwrap();
        released.await.unwrap();
        Ok::<_, CatalogError>(())
    });
    futures::pin_mut!(busy);
    // The lane's one operation is running, so the next one queues behind it.
    match futures::future::select(busy.as_mut(), running).await {
        futures::future::Either::Right((entered, _)) => entered.unwrap(),
        futures::future::Either::Left((outcome, _)) => {
            panic!("the busy operation finished early: {outcome:?}")
        }
    }
    let built = Arc::new(AtomicUsize::new(0));
    let operation = |built: Arc<AtomicUsize>| {
        move |database: zeroship_data_orm::orm::Database| {
            built.fetch_add(1, Ordering::SeqCst);
            async move {
                DeploymentHolds::new(database)?
                    .acquire(
                        &HoldScope::for_queue(AppId::mint()),
                        &typed_id::generate("dep"),
                        generation(1),
                    )
                    .await
                    .map(|_| ())
            }
        }
    };
    // One poll hands the operation to the lane's queue; dropping it abandons it.
    let abandoned = executor.run(operation(built.clone()));
    assert!(
        abandoned.now_or_never().is_none(),
        "the queued operation finished before its lane was free"
    );
    release.send(()).unwrap();
    busy.await.unwrap();
    // The lane serves in order, so this runs after the abandoned slot.
    let control = executor.run(operation(built.clone())).await;
    assert!(
        matches!(
            control,
            Err(zeroship_workflow_manager::deployments::Error::PermissionDenied)
        ),
        "{control:?}"
    );
    assert_eq!(
        built.load(Ordering::SeqCst),
        1,
        "the abandoned operation was built after its caller had gone"
    );
}

/// An operation on a lane opens no request connection and waits for no lane,
/// neither for an operation nor for a background task to start, which is the
/// order that keeps a lane from waiting on a session whose owner waits for
/// that lane. The same calls off a lane succeed.
#[compio::test(crate = "crate::support::live::system")]
async fn a_lane_waits_for_no_request_connection_and_no_lane() {
    const REFUSED: &str =
        "invalid control catalog metadata: a catalog lane may not wait for a catalog lane";
    let fixture = Fixture::new().await;
    let registry = fixture.state.registry.clone();
    let (request, nested, spawned) = fixture
        .state
        .registry
        .retention()
        .run(move |_database| async move {
            let request = registry.live_app_exists(&AppId::mint()).await;
            let nested = registry
                .catalog()
                .run(|_database| async { Ok::<_, CatalogError>(()) })
                .await;
            let spawned = registry
                .catalog()
                .spawn(|_database, _closing| Ok(Box::pin(async {})))
                .await;
            Ok::<_, CatalogError>((
                request.map_err(|error| error.to_string()),
                nested.map_err(|error| error.to_string()),
                spawned.map_err(|error| error.to_string()),
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        request,
        Err("database: a catalog lane may not open a request connection".to_owned())
    );
    assert_eq!(nested, Err(REFUSED.to_owned()));
    assert_eq!(spawned, Err(REFUSED.to_owned()));
    // The controls, off any lane.
    assert!(
        !fixture
            .state
            .registry
            .live_app_exists(&AppId::mint())
            .await
            .unwrap()
    );
    fixture
        .state
        .registry
        .catalog()
        .run(|_database| async { Ok::<_, CatalogError>(()) })
        .await
        .unwrap();
    fixture
        .state
        .registry
        .catalog()
        .spawn(|_database, _closing| Ok(Box::pin(async {})))
        .await
        .unwrap();
}
