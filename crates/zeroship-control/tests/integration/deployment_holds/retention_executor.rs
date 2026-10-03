//! The retention executor: the bounded sessions deployment holds run on, apart
//! from the publication catalog's.
//!
//! These cases observe the sessions holds occupy in `pg_stat_activity`, the
//! order an executor's lanes admit, a queued operation whose caller has gone,
//! and an authority budget that expires between a placed hold's last check and
//! its commit.

use super::*;
use futures::{channel::oneshot, future::join_all, FutureExt as _};
use std::{
    cell::Cell,
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::atomic::AtomicI64,
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
    let blocker = platform::connect(
        &fixture
            .control_url
            .replacen("zeroship_control@", "postgres@", 1),
    )
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
    let api = DeploymentHoldApi::new(
        executor(&fixture, BOUND).await,
        "http://127.0.0.1:9",
        fixture.state.service_auth.clone(),
        Options::default(),
    )
    .unwrap();
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
            assignment_revision: None,
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

/// A coordinator whose check number `shorten_at` shortens the placement's
/// lease, and whose third check answers only after that lease has run out.
struct ExpiringAuthority {
    calls: AtomicUsize,
    /// Which check shortens the lease: the first, on the serving thread, or
    /// the second, inside the transaction.
    shorten_at: AtomicUsize,
    /// Wall-clock milliseconds at which the shortened lease ends; zero until
    /// the shortening check sets it.
    expires: AtomicI64,
    /// Whether the third check should wait out the lease. The control case
    /// answers at once.
    stall: std::sync::atomic::AtomicBool,
}

fn wall_millis() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

async fn expiring_authority(
    authority: State<Arc<ExpiringAuthority>>,
    body: Json<VerifyAssignment>,
) -> web::HttpResponse {
    let request = body.into_inner();
    let call = authority.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let expires = if call == authority.shorten_at.load(Ordering::SeqCst) {
        let shortened = wall_millis() + 1_000;
        authority.expires.store(shortened, Ordering::SeqCst);
        shortened
    } else if call == 3 && authority.stall.load(Ordering::SeqCst) {
        // Answer only once the shortened lease is over, with a lease that
        // would otherwise extend it.
        let over = authority.expires.load(Ordering::SeqCst) + 100;
        let wait = u64::try_from(over - wall_millis()).unwrap_or(0);
        compio::time::sleep(Duration::from_millis(wait)).await;
        assert!(wall_millis() > authority.expires.load(Ordering::SeqCst));
        wall_millis() + 60_000
    } else {
        wall_millis() + 60_000
    };
    web::HttpResponse::Ok().json(&Assignment {
        app_id: request.app_id,
        worker_id: request.worker_id,
        revision: request.assignment_revision,
        expires_at: expires.try_into().unwrap(),
    })
}

/// A placed hold whose authority runs out after its hold is staged, while its
/// last placement check is outstanding and before COMMIT, does not commit: the
/// lease a check shortened reaches the budget, and the budget cancels the
/// transaction before COMMIT. That holds whether the first check, made on the
/// serving thread, or the second, made inside the transaction, shortened it.
/// The same hold with authority that does not run out commits.
#[compio::test(crate = "crate::support::live::system")]
async fn authority_that_expires_before_commit_commits_nothing() {
    let fixture = Fixture::new().await;
    let authority = Arc::new(ExpiringAuthority {
        calls: AtomicUsize::new(0),
        shorten_at: AtomicUsize::new(0),
        expires: AtomicI64::new(0),
        stall: false.into(),
    });
    let state = authority.clone();
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource(endpoints::WORKFLOW_VERIFY_ASSIGNMENT.path_template())
                    .route(web::post().to(expiring_authority)),
            )
        }
    })
    .await;
    crate::support::live::register_listener(server.addr());
    let executor = executor(&fixture, 1).await;
    let api = DeploymentHoldApi::new(
        executor.clone(),
        &origin(&server),
        fixture.state.service_auth.clone(),
        Options::default(),
    )
    .unwrap();
    let worker = WorkerId::mint();

    for shorten_at in [1, 2] {
        let (app, deploy, _) = fixture
            .deployment(&format!("retention-expiry-{shorten_at}"))
            .await;
        let request = HoldRequest {
            app_id: app,
            assignment_revision: Some(1.try_into().unwrap()),
            deploy_id: deploy,
            generation: generation(1),
        };
        let before = fixture.rows().await;
        authority.calls.store(0, Ordering::SeqCst);
        authority.shorten_at.store(shorten_at, Ordering::SeqCst);
        authority.stall.store(true, Ordering::SeqCst);
        let expired = api.acquire(&worker, &request).await;
        assert!(
            matches!(
                expired,
                Err(zeroship_workflow_manager::deployments::Error::Timeout)
            ),
            "shortened by check {shorten_at}: {expired:?}"
        );
        // The third check began, so the hold was staged in the transaction
        // before authority ran out.
        assert_eq!(authority.calls.load(Ordering::SeqCst), 3);
        // The executor has one lane, so this runs only once the expired hold
        // has left it: nothing it staged can still commit after this.
        executor
            .run(|_database| async { Ok::<_, CatalogError>(()) })
            .await
            .unwrap();
        assert_eq!(
            fixture.rows().await,
            before,
            "a hold whose authority check {shorten_at} shortened committed after it expired"
        );

        // The control: the same hold, with authority that does not run out.
        authority.calls.store(0, Ordering::SeqCst);
        authority.shorten_at.store(0, Ordering::SeqCst);
        authority.stall.store(false, Ordering::SeqCst);
        let receipt = api.acquire(&worker, &request).await.unwrap();
        assert_eq!(authority.calls.load(Ordering::SeqCst), 3);
        assert_eq!(receipt.state, HoldState::Held);
        assert_eq!(fixture.rows().await.len(), before.len() + 1);
    }
}

/// A coordinator that refuses every placement check while `refuse` is set.
struct RefusingAuthority {
    calls: AtomicUsize,
    refuse: std::sync::atomic::AtomicBool,
}

async fn refusing_authority(
    authority: State<Arc<RefusingAuthority>>,
    body: Json<VerifyAssignment>,
) -> web::HttpResponse {
    let request = body.into_inner();
    authority.calls.fetch_add(1, Ordering::SeqCst);
    if authority.refuse.load(Ordering::SeqCst) {
        return web::HttpResponse::Forbidden().json(&Failure {
            code: FailureCode::Denied,
        });
    }
    web::HttpResponse::Ok().json(&Assignment {
        app_id: request.app_id,
        worker_id: request.worker_id,
        revision: request.assignment_revision,
        expires_at: (wall_millis() + 60_000).try_into().unwrap(),
    })
}

/// A worker the coordinator does not place on the app is refused on the
/// serving thread, without waiting for or occupying a retention lane: with
/// every lane busy, its refusal still arrives. The same worker, placed, holds
/// once the lane is free.
#[compio::test(crate = "crate::support::live::system")]
async fn an_unplaced_worker_is_refused_without_a_lane() {
    let fixture = Fixture::new().await;
    let (app, deploy, _) = fixture.deployment("retention-unplaced").await;
    let authority = Arc::new(RefusingAuthority {
        calls: AtomicUsize::new(0),
        refuse: true.into(),
    });
    let state = authority.clone();
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource(endpoints::WORKFLOW_VERIFY_ASSIGNMENT.path_template())
                    .route(web::post().to(refusing_authority)),
            )
        }
    })
    .await;
    crate::support::live::register_listener(server.addr());
    let executor = executor(&fixture, 1).await;
    let api = DeploymentHoldApi::new(
        executor.clone(),
        &origin(&server),
        fixture.state.service_auth.clone(),
        Options::default(),
    )
    .unwrap();
    let worker = WorkerId::mint();
    let request = HoldRequest {
        app_id: app,
        assignment_revision: Some(1.try_into().unwrap()),
        deploy_id: deploy,
        generation: generation(1),
    };

    // The executor's one lane is busy until this test releases it.
    let (entered, running) = oneshot::channel::<()>();
    let (release, released) = oneshot::channel::<()>();
    let busy = executor.run(move |_database| async move {
        entered.send(()).unwrap();
        released.await.unwrap();
        Ok::<_, CatalogError>(())
    });
    futures::pin_mut!(busy);
    match futures::future::select(busy.as_mut(), running).await {
        futures::future::Either::Right((entered, _)) => entered.unwrap(),
        futures::future::Either::Left((outcome, _)) => {
            panic!("the busy operation finished early: {outcome:?}")
        }
    }

    let refused = api.acquire(&worker, &request).await;
    assert!(
        matches!(
            refused,
            Err(zeroship_workflow_manager::deployments::Error::PermissionDenied)
        ),
        "{refused:?}"
    );
    assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
    // The lane was held throughout: the busy operation has not finished.
    assert!(
        busy.as_mut().now_or_never().is_none(),
        "the busy operation finished before it was released"
    );

    // The control: placed, the same hold holds once the lane is free.
    release.send(()).unwrap();
    busy.await.unwrap();
    authority.calls.store(0, Ordering::SeqCst);
    authority.refuse.store(false, Ordering::SeqCst);
    let receipt = api.acquire(&worker, &request).await.unwrap();
    assert_eq!(receipt.state, HoldState::Held);
    assert_eq!(authority.calls.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.rows().await.len(), 1);
}
