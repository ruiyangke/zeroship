//! Zone capacity follows the policy-filtered backlog the queue holds.
#![allow(
    clippy::future_not_send,
    reason = "native capacity fixtures stay on their compio runtime"
)]

use crate::support::{self, policies::Policies, Backend, Fixture};

use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Revision, RunId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
    workflow_policy::AppPolicy,
    zone_id::ZoneId,
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    capacity::{
        Capacity, CapacityProvider, CapacityReply, CapacityFuture, Exchange, StaticPool,
    },
    driver, recovery, Options, Queue,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($contract(&Fixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($contract(&Fixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_static_pool_refuses_exactly_above_its_slots,
    postgres_static_pool_refuses_exactly_above_its_slots,
    static_pool_refuses_exactly_above_its_slots
);
case!(
    sqlite_a_disabled_app_has_no_demand,
    postgres_a_disabled_app_has_no_demand,
    a_disabled_app_has_no_demand
);
case!(
    sqlite_an_unavailable_observation_freezes_the_target,
    postgres_an_unavailable_observation_freezes_the_target,
    an_unavailable_observation_freezes_the_target
);
case!(
    sqlite_steady_state_resyncs_one_revision,
    postgres_steady_state_resyncs_one_revision,
    steady_state_resyncs_one_revision
);
case!(
    sqlite_hold_down_delays_a_decrease,
    postgres_hold_down_delays_a_decrease,
    hold_down_delays_a_decrease
);
case!(
    sqlite_demand_counts_claimable_rows_up_to_max_running,
    postgres_demand_counts_claimable_rows_up_to_max_running,
    demand_counts_claimable_rows_up_to_max_running
);
case!(
    sqlite_undeliverable_rows_are_counted_and_never_demanded,
    postgres_undeliverable_rows_are_counted_and_never_demanded,
    undeliverable_rows_are_counted_and_never_demanded
);
case!(
    sqlite_a_partial_visit_raises_and_never_lowers,
    postgres_a_partial_visit_raises_and_never_lowers,
    a_partial_visit_raises_and_never_lowers
);
case!(
    sqlite_a_zone_larger_than_one_pass_is_measured_across_passes,
    postgres_a_zone_larger_than_one_pass_is_measured_across_passes,
    a_zone_larger_than_one_pass_is_measured_across_passes
);
case!(
    sqlite_an_idle_scope_costs_no_policy_observation,
    postgres_an_idle_scope_costs_no_policy_observation,
    an_idle_scope_costs_no_policy_observation
);
case!(
    sqlite_an_app_busy_behind_the_cycle_is_counted_by_the_next,
    postgres_an_app_busy_behind_the_cycle_is_counted_by_the_next,
    an_app_busy_behind_the_cycle_is_counted_by_the_next
);

/// A visit deadline no case reaches.
fn unbounded() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

async fn queue(fixture: &Fixture) -> Queue {
    Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap()
}

fn options(retry: Duration) -> driver::Options {
    driver::Options {
        recovery: recovery::Options {
            interval: Duration::from_secs(3600),
            ..recovery::Options::default()
        },
        capacity: zeroship_workflow_manager::capacity::Options {
            min_slots: 0,
            max_slots: 10,
            idle_hold_down: Duration::from_secs(3600),
            request_timeout: Duration::from_secs(10),
            retry_interval: retry,
        },
        ..driver::Options::default()
    }
}

fn advance(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: revision(1),
        },
        available_at: 0.try_into().unwrap(),
    }
}

fn revision(value: i64) -> Revision {
    value.try_into().unwrap()
}

fn capacity(
    queue: &Queue,
    provider: Rc<dyn CapacityProvider>,
    retry: Duration,
) -> Capacity {
    Capacity::new(queue.clone(), provider, options(retry).capacity).unwrap()
}

async fn seed(queue: &Queue, policies: &Rc<Policies>, zone: &ZoneId, app: &AppId) {
    policies.app(app, zone);
    queue.register_scope(app, zone).await.unwrap();
    queue.submit(&advance(app)).await.unwrap();
}

async fn target(fixture: &Fixture, zone: &ZoneId) -> Value {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("capacity_targets")
        .unwrap()
        .find(value!({"id":zone.as_str()}), value!({"limit":1}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    rows.into_iter().next().expect("a zone target")
}

async fn patch(fixture: &Fixture, zone: &ZoneId, patch: Value) {
    let updated = fixture
        .database()
        .await
        .collection("capacity_targets")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":zone.as_str()}),
            patch,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

/// A static pool refuses exactly when the target exceeds its configured slots,
/// and the durable target records the refusal without losing the target.
async fn static_pool_refuses_exactly_above_its_slots(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let app = AppId::mint();
    seed(&queue, &policies, &zone, &app).await;

    let refused = capacity(&queue, Rc::new(StaticPool { pool_slots: 0 }), Duration::from_secs(3600));
    assert_eq!(
        refused.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap(),
        Exchange::Applied
    );
    let row = target(fixture, &zone).await;
    assert_eq!(row["state"].as_str(), Some("refused"));
    assert_eq!(row["refusal"].as_str(), Some("pool_exhausted"));
    assert_eq!(row["desired"].as_i64(), Some(1));

    patch(fixture, &zone, value!({"retry_at":0})).await;
    let accepted = capacity(&queue, Rc::new(StaticPool { pool_slots: 1 }), Duration::from_secs(3600));
    assert_eq!(
        accepted.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap(),
        Exchange::Applied
    );
    let row = target(fixture, &zone).await;
    assert_eq!(row["state"].as_str(), Some("steady"));
    assert!(row["refusal"].is_null());
}

/// A dispatch-disabled app contributes no demand; the same app with dispatch on
/// does. The control differs only in that one policy field.
async fn a_disabled_app_has_no_demand(fixture: &Fixture) {
    let queue = queue(fixture).await;
    for dispatch in [false, true] {
        let policies = Policies::new();
        let zone = ZoneId::mint();
        let app = AppId::mint();
        policies.with_policy(
            &app,
            &zone,
            AppPolicy {
                dispatch,
                ..AppPolicy::default()
            },
        );
        queue.register_scope(&app, &zone).await.unwrap();
        queue.submit(&advance(&app)).await.unwrap();
        let target = capacity(
            &queue,
            Rc::new(StaticPool { pool_slots: 10 }),
            Duration::from_secs(3600),
        );
        target.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
        let row = target_row(fixture, &zone).await;
        let expected = if dispatch { 1 } else { 0 };
        assert_eq!(row["backlog_depth"].as_i64(), Some(expected), "dispatch={dispatch}");
    }
}

async fn target_row(fixture: &Fixture, zone: &ZoneId) -> Value {
    target(fixture, zone).await
}

/// An observation that fails freezes the target: the revision and desired do
/// not move, and no provider is asked.
async fn an_unavailable_observation_freezes_the_target(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let app = AppId::mint();
    seed(&queue, &policies, &zone, &app).await;
    let lane = capacity(&queue, Recorder::new(), Duration::from_secs(3600));
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    let before = target(fixture, &zone).await;

    policies.unavailable();
    assert_eq!(
        lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap(),
        Exchange::Idle
    );
    let after = target(fixture, &zone).await;
    assert_eq!(after["revision"], before["revision"]);
    assert_eq!(after["desired"], before["desired"]);
}

/// A steady target resends its current revision after the retry interval, and
/// the re-sent request names the same revision and desired slots.
async fn steady_state_resyncs_one_revision(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let app = AppId::mint();
    seed(&queue, &policies, &zone, &app).await;
    let recorder = Recorder::new();
    let lane = capacity(&queue, recorder.clone(), Duration::from_secs(3600));
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    assert_eq!(recorder.calls(), 1);
    // The retry interval has not elapsed, so a second pass does not re-request.
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    assert_eq!(recorder.calls(), 1);
    patch(fixture, &zone, value!({"retry_at":0})).await;
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    assert_eq!(recorder.calls(), 2);
    let calls = recorder.requests.borrow();
    assert_eq!(calls[0].revision, calls[1].revision);
    assert_eq!(calls[0].desired_slots, calls[1].desired_slots);
}

/// A decrease waits out the hold-down: the target keeps its higher desired
/// until the below-since mark is older than the hold-down, then falls.
async fn hold_down_delays_a_decrease(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let app = AppId::mint();
    seed(&queue, &policies, &zone, &app).await;
    let lane = capacity(&queue, Recorder::new(), Duration::from_secs(3600));
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    assert_eq!(target(fixture, &zone).await["desired"].as_i64(), Some(1));

    // Remove the work; demand falls to zero but the hold-down holds the target.
    policies.delete(&app);
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    let held = target(fixture, &zone).await;
    assert_eq!(held["desired"].as_i64(), Some(1));
    assert!(held["below_since"].as_i64().is_some());

    // Move the below-since mark past the hold-down and the decrease applies.
    patch(fixture, &zone, value!({"below_since":0})).await;
    lane.reconcile(&zone, policies.as_ref(), unbounded()).await.unwrap();
    assert_eq!(target(fixture, &zone).await["desired"].as_i64(), Some(0));
}

/// A provider that records every request and accepts.
#[derive(Debug, Default)]
struct Recorder {
    requests: std::cell::RefCell<Vec<zeroship_workflow_manager::capacity::CapacityRequest>>,
}

impl Recorder {
    fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    fn calls(&self) -> usize {
        self.requests.borrow().len()
    }
}

impl CapacityProvider for Recorder {
    fn ensure<'a>(
        &'a self,
        request: &'a zeroship_workflow_manager::capacity::CapacityRequest,
    ) -> CapacityFuture<'a> {
        self.requests.borrow_mut().push(request.clone());
        Box::pin(async { Ok(CapacityReply::Accepted) })
    }
}

/// Patch every job row `filter` matches, standing in for queue history a case
/// would otherwise have to drive through many deliveries.
async fn patch_jobs(fixture: &Fixture, filter: Value, patch: Value) {
    let updated = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .execute(Operation::Update {
            filter,
            patch,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

fn policy(max_running: i64) -> AppPolicy {
    AppPolicy {
        max_running,
        ..AppPolicy::default()
    }
}

/// Demand counts every claimable row up to the app's `max_running`, and the
/// backlog depth counts them all. The control lifts `max_running` above the
/// backlog, so demand follows the rows instead of the cap.
async fn demand_counts_claimable_rows_up_to_max_running(fixture: &Fixture) {
    let queue = queue(fixture).await;
    for (max_running, expected) in [(2, 2), (4, 3)] {
        let policies = Policies::new();
        let zone = ZoneId::mint();
        let app = AppId::mint();
        policies.with_policy(&app, &zone, policy(max_running));
        queue.register_scope(&app, &zone).await.unwrap();
        for _ in 0..3 {
            queue.submit(&advance(&app)).await.unwrap();
        }
        let lane = capacity(&queue, Recorder::new(), Duration::from_secs(3600));
        lane.reconcile(&zone, policies.as_ref(), unbounded())
            .await
            .unwrap();
        let row = target(fixture, &zone).await;
        assert_eq!(row["desired"].as_i64(), Some(expected), "max_running={max_running}");
        assert_eq!(row["backlog_depth"].as_i64(), Some(3), "max_running={max_running}");
    }
}

/// Creator work that cannot deliver now never drives demand, and the target
/// row counts it by reason: a row past its delivery budget, a row inside a
/// give-back back-off, and the rows of an app whose policy withholds dispatch.
/// The one deliverable row is the control, and the oldest claimable row is
/// the one recorded.
async fn undeliverable_rows_are_counted_and_never_demanded(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let app = AppId::mint();
    policies.app(&app, &zone);
    queue.register_scope(&app, &zone).await.unwrap();
    let mut deliverable = advance(&app);
    deliverable.available_at = 7.try_into().unwrap();
    let exhausted = advance(&app);
    let backed_off = advance(&app);
    for job in [&deliverable, &exhausted, &backed_off] {
        queue.submit(job).await.unwrap();
    }
    patch_jobs(
        fixture,
        value!({"id":exhausted.id.as_str()}),
        value!({"execution_attempts":AppPolicy::default().max_delivery_attempts}),
    )
    .await;
    patch_jobs(
        fixture,
        value!({"id":backed_off.id.as_str()}),
        value!({"deferred_until":i64::MAX / 2,"deferrals":1}),
    )
    .await;
    let withheld = AppId::mint();
    policies.with_policy(
        &withheld,
        &zone,
        AppPolicy {
            dispatch: false,
            ..AppPolicy::default()
        },
    );
    queue.register_scope(&withheld, &zone).await.unwrap();
    for _ in 0..2 {
        queue.submit(&advance(&withheld)).await.unwrap();
    }

    let lane = capacity(&queue, Recorder::new(), Duration::from_secs(3600));
    lane.reconcile(&zone, policies.as_ref(), unbounded())
        .await
        .unwrap();
    let row = target(fixture, &zone).await;
    assert_eq!(row["desired"].as_i64(), Some(1), "{row:?}");
    assert_eq!(row["backlog_depth"].as_i64(), Some(1), "{row:?}");
    assert_eq!(row["oldest_available_at"].as_i64(), Some(7), "{row:?}");
    assert_eq!(row["exhausted_jobs"].as_i64(), Some(1), "{row:?}");
    assert_eq!(row["backed_off_jobs"].as_i64(), Some(1), "{row:?}");
    assert_eq!(row["withheld_jobs"].as_i64(), Some(2), "{row:?}");
}

/// A visit cut by its deadline measures the apps it reached: it may raise the
/// target, never lowers it, and leaves the recorded census as it was. The
/// complete visit between the two partial ones is the control.
async fn a_partial_visit_raises_and_never_lowers(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let first = AppId::mint();
    let second = AppId::mint();
    for app in [&first, &second] {
        seed(&queue, &policies, &zone, app).await;
    }
    let lane = capacity(&queue, Recorder::new(), Duration::from_secs(3600));

    lane.reconcile(&zone, policies.as_ref(), Instant::now())
        .await
        .unwrap();
    let raised = target(fixture, &zone).await;
    assert_eq!(raised["desired"].as_i64(), Some(1), "{raised:?}");
    assert_eq!(raised["backlog_depth"].as_i64(), Some(0), "{raised:?}");

    lane.reconcile(&zone, policies.as_ref(), unbounded())
        .await
        .unwrap();
    let complete = target(fixture, &zone).await;
    assert_eq!(complete["desired"].as_i64(), Some(2), "{complete:?}");
    assert_eq!(complete["backlog_depth"].as_i64(), Some(2), "{complete:?}");

    lane.reconcile(&zone, policies.as_ref(), Instant::now())
        .await
        .unwrap();
    let kept = target(fixture, &zone).await;
    assert_eq!(kept["desired"].as_i64(), Some(2), "{kept:?}");
    assert!(kept["below_since"].is_null(), "{kept:?}");
    assert_eq!(kept["backlog_depth"].as_i64(), Some(2), "{kept:?}");
}

/// A zone too large for one pass is measured over successive passes. Each pass
/// its deadline cuts continues the zone's cycle after the last app it measured,
/// and may raise the target but never lowers it; the pass that reaches the end
/// completes the cycle, whose census has measured every app once, and that is
/// what lets the target fall. The control is the same zone measured in one
/// pass, which completes at once.
async fn a_zone_larger_than_one_pass_is_measured_across_passes(fixture: &Fixture) {
    let queue = queue(fixture).await;
    for one_pass in [false, true] {
        let policies = Policies::new();
        let zone = ZoneId::mint();
        for _ in 0..3 {
            seed(&queue, &policies, &zone, &AppId::mint()).await;
        }
        let lane = Capacity::new(
            queue.clone(),
            Recorder::new(),
            zeroship_workflow_manager::capacity::Options {
                idle_hold_down: Duration::from_millis(1),
                ..options(Duration::from_secs(3600)).capacity
            },
        )
        .unwrap();
        patch(fixture, &zone, value!({"desired":10})).await;
        let pass = || if one_pass { unbounded() } else { Instant::now() };
        if !one_pass {
            for _ in 0..2 {
                lane.reconcile(&zone, policies.as_ref(), pass()).await.unwrap();
                let row = target(fixture, &zone).await;
                assert_eq!(row["desired"].as_i64(), Some(10), "a cut pass lowered: {row:?}");
                assert!(row["below_since"].is_null(), "{row:?}");
            }
        }
        lane.reconcile(&zone, policies.as_ref(), pass()).await.unwrap();
        let row = target(fixture, &zone).await;
        assert_eq!(row["backlog_depth"].as_i64(), Some(3), "one_pass={one_pass}: {row:?}");
        assert!(
            row["below_since"].as_i64().is_some(),
            "a complete cycle below the target starts its decrease: {row:?}"
        );

        patch(fixture, &zone, value!({"below_since":0})).await;
        for _ in 0..if one_pass { 1 } else { 3 } {
            lane.reconcile(&zone, policies.as_ref(), pass()).await.unwrap();
        }
        let row = target(fixture, &zone).await;
        assert_eq!(row["desired"].as_i64(), Some(3), "one_pass={one_pass}: {row:?}");
    }
}

/// An app that goes from idle to busy behind a cycle's position is left to the
/// next cycle. The cycle it went busy in completes on everything it measured
/// across its passes, so the completion keeps the target at the demand those
/// passes found instead of falling to the last pass's share; the next cycle
/// starts again at the zone's first app, counts the newly busy one and raises
/// the target to cover it.
///
/// The hold-down has already run out when the cycle completes, so a completion
/// that measured less than the target would lower it at once.
async fn an_app_busy_behind_the_cycle_is_counted_by_the_next(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let mut apps = [AppId::mint(), AppId::mint(), AppId::mint()];
    apps.sort();
    let [behind, measured, last] = apps;
    // The first app of the zone has a scope and nothing to run yet.
    policies.app(&behind, &zone);
    queue.register_scope(&behind, &zone).await.unwrap();
    for app in [&measured, &last] {
        seed(&queue, &policies, &zone, app).await;
    }
    let lane = Capacity::new(
        queue.clone(),
        Recorder::new(),
        zeroship_workflow_manager::capacity::Options {
            idle_hold_down: Duration::from_millis(1),
            ..options(Duration::from_secs(3600)).capacity
        },
    )
    .unwrap();
    lane.reconcile(&zone, policies.as_ref(), unbounded())
        .await
        .unwrap();
    assert_eq!(target(fixture, &zone).await["desired"].as_i64(), Some(2));

    // A pass cut after its first app leaves the cycle standing after it.
    lane.reconcile(&zone, policies.as_ref(), Instant::now())
        .await
        .unwrap();
    assert_eq!(target(fixture, &zone).await["desired"].as_i64(), Some(2));
    // The app behind that position goes busy, and the decrease's hold-down is
    // already served.
    queue.submit(&advance(&behind)).await.unwrap();
    patch(fixture, &zone, value!({"below_since":0})).await;

    // The pass that completes the cycle measures the last app alone.
    lane.reconcile(&zone, policies.as_ref(), Instant::now())
        .await
        .unwrap();
    let completed = target(fixture, &zone).await;
    assert_eq!(completed["desired"].as_i64(), Some(2), "{completed:?}");
    assert_eq!(completed["backlog_depth"].as_i64(), Some(2), "{completed:?}");
    assert_eq!(
        policies.observations(&behind),
        0,
        "the cycle went back for an app behind its position"
    );

    lane.reconcile(&zone, policies.as_ref(), unbounded())
        .await
        .unwrap();
    let next = target(fixture, &zone).await;
    assert_eq!(next["desired"].as_i64(), Some(3), "{next:?}");
    assert_eq!(next["backlog_depth"].as_i64(), Some(3), "{next:?}");
    assert_eq!(policies.observations(&behind), 1);
}

/// A scope with no creator work is not visited: neither an app that never had
/// a job nor one whose every creator row has settled has its policy observed.
/// The control is an app of the same zone with a queued job, which is observed
/// once per visit.
async fn an_idle_scope_costs_no_policy_observation(fixture: &Fixture) {
    let queue = queue(fixture).await;
    let policies = Policies::new();
    let zone = ZoneId::mint();
    let idle = AppId::mint();
    policies.app(&idle, &zone);
    queue.register_scope(&idle, &zone).await.unwrap();
    let settled = AppId::mint();
    seed(&queue, &policies, &zone, &settled).await;
    patch_jobs(
        fixture,
        value!({"app_id":settled.as_str()}),
        value!({"state":"settled","outcome":"{\"kind\":\"completed\"}"}),
    )
    .await;
    let busy = AppId::mint();
    seed(&queue, &policies, &zone, &busy).await;

    let lane = capacity(&queue, Recorder::new(), Duration::from_secs(3600));
    lane.reconcile(&zone, policies.as_ref(), unbounded())
        .await
        .unwrap();
    assert_eq!(policies.observations(&idle), 0);
    assert_eq!(policies.observations(&settled), 0);
    assert_eq!(policies.observations(&busy), 1);
    assert_eq!(target(fixture, &zone).await["backlog_depth"].as_i64(), Some(1));
}
