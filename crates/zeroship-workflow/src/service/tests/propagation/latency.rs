//! A propagation page's journal statements against the attempt ceiling that
//! bounds them.

use super::*;
use crate::service::{
    delivery::ATTEMPT_IO_CEILING,
    tests::wire::{Recorded, Wire},
};
use std::time::Duration;

/// Runs the baseline page reaches: two of each kind the page treats apart,
/// so its intents are written as several rows, as the measured page's are.
const FEW: usize = 4;
/// Runs the measured page reaches, within one membership list.
const MANY: usize = 64;

/// A cascade page executes the same statements however many children it
/// cancels, so it commits within its attempt when every round trip is slow.
///
/// Half of each page's children hold a task and half are idle, so both
/// cancellation updates and the idle children's frontier records run. The
/// store reaches its server through a proxy that, while armed, holds every
/// round trip, read or write, and records each statement executed by the
/// journal table it names. A page of [`FEW`] children sets the baseline with
/// nothing held; a page of [`MANY`] then runs with each round trip held long
/// enough that four times the baseline's round trips fill
/// [`ATTEMPT_IO_CEILING`], and must commit inside it and execute exactly the
/// baseline's statements on every table.
#[compio::test]
async fn postgres_cascade_page_statements_do_not_grow_with_its_children() {
    let fixture = PostgresFixture::start().await;
    let wire = Wire::start(&fixture.journal_url).await;
    let store = Rc::new(Box::pin(orm_store(wire.url())).await);
    let (service, many, few, _deployments) = Box::pin(registered_service(store)).await;
    let few = service.fixture_app(few);
    let many = service.fixture_app(many);
    let few_children = Box::pin(cascading(&service, &few, FEW)).await;
    let many_children = Box::pin(cascading(&service, &many, MANY)).await;
    let (baseline, measured, elapsed, held) = Box::pin(measure(&wire, &few, &many)).await;
    assert!(
        elapsed >= held * measured.round_trips,
        "the page did not wait on its held round trips: {elapsed:?}"
    );
    assert_eq!(
        measured.statements, baseline.statements,
        "a cascade page of {MANY} children executed other statements than one of {FEW}"
    );
    for (scope, (leased, idle)) in [(&few, few_children), (&many, many_children)] {
        for child in &leased {
            let row = run_row(&service, scope.app_id(), child).await;
            assert_eq!(row.text("control").unwrap(), "cancel");
            assert_eq!(row.integer("frontier_revision").unwrap(), 1);
        }
        for child in &idle {
            let row = run_row(&service, scope.app_id(), child).await;
            assert_eq!(row.text("control").unwrap(), "cancel");
            assert_eq!(row.integer("frontier_revision").unwrap(), 2);
            assert!(row.optional_integer("due_at").unwrap().is_some());
        }
        assert_eq!(
            page_results(&service, scope.app_id()).await[0]["affected"],
            json!(leased.len() + idle.len())
        );
    }
}

/// A notify page executes the same statements however many parents it wakes,
/// so it commits within its attempt when every round trip is slow.
///
/// Half of each page's parents wait idle and half are paused, so the page both
/// wakes parents and passes over parents it may not wake. The proxy and the
/// baseline are those of
/// [`postgres_cascade_page_statements_do_not_grow_with_its_children`].
#[compio::test]
async fn postgres_notify_page_statements_do_not_grow_with_its_parents() {
    let fixture = PostgresFixture::start().await;
    let wire = Wire::start(&fixture.journal_url).await;
    let store = Rc::new(Box::pin(orm_store(wire.url())).await);
    let (service, many, few, _deployments) = Box::pin(registered_service(store)).await;
    let few = service.fixture_app(few);
    let many = service.fixture_app(many);
    let few_parents = Box::pin(waiting(&service, &few, FEW)).await;
    let many_parents = Box::pin(waiting(&service, &many, MANY)).await;
    let (baseline, measured, elapsed, held) = Box::pin(measure(&wire, &few, &many)).await;
    assert!(
        elapsed >= held * measured.round_trips,
        "the page did not wait on its held round trips: {elapsed:?}"
    );
    assert_eq!(
        measured.statements, baseline.statements,
        "a notify page of {MANY} parents executed other statements than one of {FEW}"
    );
    for (scope, (idle, paused)) in [(&few, few_parents), (&many, many_parents)] {
        for parent in &idle {
            let row = run_row(&service, scope.app_id(), parent).await;
            assert!(row.optional_integer("due_at").unwrap().is_some());
            assert_eq!(row.integer("frontier_revision").unwrap(), 2);
        }
        for parent in &paused {
            let row = run_row(&service, scope.app_id(), parent).await;
            assert_eq!(row.optional_integer("due_at").unwrap(), None);
            assert_eq!(row.integer("frontier_revision").unwrap(), 1);
        }
        assert_eq!(
            page_results(&service, scope.app_id()).await[0]["affected"],
            json!(idle.len())
        );
    }
}

/// Deliver `few`'s open page with nothing held, then `many`'s with each round
/// trip held so four times the baseline's round trips fill the attempt
/// ceiling. Returns both recordings, the measured page's time and the hold.
async fn measure(
    wire: &Wire,
    few: &AppWorkflows,
    many: &AppWorkflows,
) -> (Recorded, Recorded, Duration, Duration) {
    let (baseline, _) = Box::pin(deliver(wire, few, Duration::ZERO)).await;
    assert!(
        baseline.round_trips > 0,
        "the proxy carried the baseline page"
    );
    let held = ATTEMPT_IO_CEILING / (4 * baseline.round_trips);
    let (measured, elapsed) = Box::pin(deliver(wire, many, held)).await;
    (baseline, measured, elapsed, held)
}

/// Deliver `scope`'s one open propagation page with every round trip held for
/// `held`. The page must commit within its first attempt and finish its
/// obligation.
async fn deliver(wire: &Wire, scope: &AppWorkflows, held: Duration) -> (Recorded, Duration) {
    let job = open_page(scope).await;
    wire.arm(held);
    let started = Instant::now();
    let receipt = scope
        .propagation_job(&JobGrant::new(&job), PropagationOptions::default())
        .await;
    let elapsed = started.elapsed();
    let recorded = wire.disarm();
    assert_eq!(
        receipt
            .expect("the page commits within its first attempt")
            .outcome,
        JobOutcome::Completed {}
    );
    (recorded, elapsed)
}

/// Seed a parent with `count` cascading children, half of them leased, and
/// cancel the parent so its cascade obligation opens. Returns the leased and
/// the idle children.
async fn cascading(
    service: &WorkflowService,
    scope: &AppWorkflows,
    count: usize,
) -> (Vec<String>, Vec<String>) {
    let app = scope.app_id();
    let parent = seed_runs(service, app, "Example", 1).await.remove(0);
    let children = seed_runs(service, app, "Child", count).await;
    update_runs(
        service,
        app,
        &children,
        json!({"parent_id":parent, "parent_generation":0, "cascade":1, "depth":1}),
    )
    .await;
    let (leased, idle) = children.split_at(count / 2);
    for child in leased {
        update_runs(
            service,
            app,
            std::slice::from_ref(child),
            json!({"state":"running", "task_id":storage_id(), "due_at":i64::MAX}),
        )
        .await;
    }
    assert_eq!(
        cancel_idle(scope, &parent).await.outcome,
        JobOutcome::Completed {}
    );
    (leased.to_vec(), idle.to_vec())
}

/// Seed a child with `count` parents waiting on it, half of them paused, and
/// complete the child so its notify obligation opens. Returns the idle and the
/// paused parents.
async fn waiting(
    service: &WorkflowService,
    scope: &AppWorkflows,
    count: usize,
) -> (Vec<String>, Vec<String>) {
    let app = scope.app_id();
    let child = seed_runs(service, app, "Child", 1).await.remove(0);
    let parents = seed_runs(service, app, "Example", count).await;
    let waits: Vec<_> = parents
        .iter()
        .map(|parent| graph::Wait {
            id: storage_id(),
            run: parent.clone(),
            generation: 0,
            ordinal: 0,
            child: child.clone(),
        })
        .collect();
    let tx = service.begin().await.unwrap();
    graph::seed_waits(&tx, app, &waits).await;
    tx.commit().await.unwrap();
    let (idle, paused) = parents.split_at(count / 2);
    update_runs(
        service,
        app,
        idle,
        json!({"state":"waiting", "due_at":null}),
    )
    .await;
    update_runs(
        service,
        app,
        paused,
        json!({"state":"paused", "control":"pause", "due_at":null}),
    )
    .await;
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, app).await.unwrap();
    let run = app::lock_run(&mut tx, app, &child).await.unwrap();
    let now = tx.now().await.unwrap();
    crate::service::frontier::finish(&mut tx, app, &run, RunState::Completed, None, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (idle.to_vec(), paused.to_vec())
}
