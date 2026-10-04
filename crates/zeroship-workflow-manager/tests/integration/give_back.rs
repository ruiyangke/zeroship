//! A delivery returned without a settlement: the deferrals a claim's host or a
//! worker that could not prepare the app gives back, and an attempt that began
//! and was interrupted.
//!
//! A deferral counts no attempt and keeps the row unclaimable for a while; an
//! interrupted attempt is claimable at once and counts, exactly once.
#![expect(
    clippy::future_not_send,
    reason = "native queue fixtures stay on their compio runtime"
)]

use crate::support::{self, policies::Policies, Admin, Backend, Fixture, Owner, QueueCalls};

use std::{
    future::ready,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RunId, WorkerId},
    workflow_jobs::{Delivery, DeploymentId, JobId, JobOperation, JobSpec},
    workflow_policy::AppPolicy,
    zone_id::ZoneId,
};
use zeroship_data_orm::{orm::Output, value, Value};
use zeroship_workflow_manager::{
    capacity::{self, Capacity, StaticPool},
    Claimant, DeliveryGrant, GiveBack, Options, Queue,
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
    sqlite_each_deferral_sets_when_its_row_is_claimable_again,
    postgres_each_deferral_sets_when_its_row_is_claimable_again,
    each_deferral
);
case!(
    sqlite_the_back_off_grows_with_each_give_back_and_resets_on_a_counted_attempt,
    postgres_the_back_off_grows_with_each_give_back_and_resets_on_a_counted_attempt,
    back_off_grows_and_resets
);
case!(
    sqlite_a_given_back_row_is_not_claimable_until_its_deferral_ends,
    postgres_a_given_back_row_is_not_claimable_until_its_deferral_ends,
    waits_out_its_deferral
);
case!(
    sqlite_no_deferral_counts_toward_the_delivery_budget,
    postgres_no_deferral_counts_toward_the_delivery_budget,
    deferrals_are_not_attempts
);
case!(
    sqlite_an_interrupted_attempt_counts_once_and_is_claimable_at_once,
    postgres_an_interrupted_attempt_counts_once_and_is_claimable_at_once,
    interrupted_counts_once
);
case!(
    sqlite_a_renewed_attempt_is_not_counted_again_when_interrupted,
    postgres_a_renewed_attempt_is_not_counted_again_when_interrupted,
    renewed_then_interrupted
);
case!(
    sqlite_interrupted_attempts_exhaust_the_row_and_deferrals_never_do,
    postgres_interrupted_attempts_exhaust_the_row_and_deferrals_never_do,
    interrupted_attempts_exhaust
);
case!(
    sqlite_a_pause_of_known_length_does_not_lengthen_the_back_off,
    postgres_a_pause_of_known_length_does_not_lengthen_the_back_off,
    known_pauses_leave_the_back_off
);
case!(
    sqlite_a_deferral_that_already_ended_leaves_the_row_claimable_now,
    postgres_a_deferral_that_already_ended_leaves_the_row_claimable_now,
    elapsed_deferral
);

/// The back-off every case configures, so a pause can be named.
const BACKOFF: Duration = Duration::from_millis(100);

/// One app of a minted zone, on a queue whose back-off starts at [`BACKOFF`].
struct App {
    queue: Queue,
    id: AppId,
    zone: ZoneId,
}

impl App {
    async fn new(fixture: &Fixture) -> Self {
        let queue = Queue::connect(
            fixture.binding(),
            fixture.url(),
            Options {
                defer_backoff: BACKOFF,
                ..Options::default()
            },
            support::synthetic_holds(),
        )
        .await
        .unwrap();
        Self::on(&queue, ZoneId::mint()).await
    }

    async fn on(queue: &Queue, zone: ZoneId) -> Self {
        let id = AppId::mint();
        queue.register_scope(&id, &zone).await.unwrap();
        Self {
            queue: queue.clone(),
            id,
            zone,
        }
    }

    async fn job(&self) -> JobSpec {
        let job = JobSpec {
            id: JobId::mint(),
            app_id: self.id.clone(),
            operation: JobOperation::Advance {
                deployment_id: DeploymentId::mint(),
                run_id: RunId::mint(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            available_at: 0.try_into().unwrap(),
        };
        assert_eq!(self.queue.submit(&job).await.unwrap(), job);
        job
    }

    /// A worker's claim of this app under the delivery budget `ceiling`.
    async fn claim(&self, worker: &WorkerId, ceiling: i64) -> Option<DeliveryGrant> {
        self.queue
            .claim_authorized(
                &self.id,
                worker,
                Claimant::Worker,
                Ok(ceiling),
                |_| ready(Ok(worker.clone())),
            )
            .await
            .unwrap()
    }

    async fn give_back(&self, delivery: &Delivery, defer: GiveBack) {
        let worker = delivery.worker_id.clone();
        self.queue
            .give_back(&worker, delivery, defer, |_| ready(Ok(worker.clone())))
            .await
            .unwrap();
    }
}

async fn now(fixture: &Fixture) -> i64 {
    match &fixture.admin {
        Admin::Postgres(admin) => admin
            .query(
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0),
        Admin::Sqlite(admin) => admin
            .query_row(
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)",
                [],
                |row| row.get(0),
            )
            .unwrap(),
    }
}

/// Wait, on the database clock, until `instant` has passed.
async fn until_past(fixture: &Fixture, instant: i64) {
    compio::time::timeout(Duration::from_secs(30), async {
        while now(fixture).await <= instant {
            compio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the database clock passes the instant");
}

async fn stored(fixture: &Fixture, job: &JobSpec) -> Value {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .find(
            value!({"id":job.id.as_str(),"app_id":job.app_id.as_str()}),
            value!({"limit":1}),
        )
        .await
        .unwrap()
    else {
        panic!("job query returned a count");
    };
    rows.into_iter().next().expect("the job was submitted")
}

/// Give `delivery` back and return the pause its row was given, read against
/// the database clock on both sides of the call: the queue stamped the row
/// somewhere between the two reads, so the pause is pinned to that window.
async fn paused(fixture: &Fixture, app: &App, delivery: &Delivery, defer: GiveBack) -> (i64, i64) {
    let before = now(fixture).await;
    app.give_back(delivery, defer).await;
    let after = now(fixture).await;
    let until = stored(fixture, &delivery.job).await["deferred_until"]
        .as_i64()
        .expect("a deferred row waits");
    (until - after, until - before)
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap()
}

/// Each deferral returns the row to `ready`, held by nobody, with its attempt
/// uncounted, and sets when it is claimable again: `Exact` at exactly its
/// instant, `After` its delay from the give-back, and the first `Backoff` the
/// configured base. Only the back-off moves the count the next back-off
/// doubles by.
async fn each_deferral(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let worker = WorkerId::mint();
    let ceiling = support::delivery_ceiling();
    let exact = app.job().await;
    let after = app.job().await;
    let backoff = app.job().await;

    let claimed = app.claim(&worker, ceiling).await.unwrap();
    assert_eq!(claimed.delivery().job, exact);
    let instant = now(fixture).await + 3_600_000;
    app.give_back(claimed.delivery(), GiveBack::Exact(instant)).await;
    assert_eq!(stored(fixture, &exact).await["deferred_until"], value!(instant));

    let delay = Duration::from_mins(10);
    let claimed = app.claim(&worker, ceiling).await.unwrap();
    assert_eq!(claimed.delivery().job, after);
    let (low, high) = paused(fixture, &app, claimed.delivery(), GiveBack::After(delay)).await;
    assert!(low <= millis(delay) && millis(delay) <= high, "{low}..{high}");

    let claimed = app.claim(&worker, ceiling).await.unwrap();
    assert_eq!(claimed.delivery().job, backoff);
    let (low, high) = paused(fixture, &app, claimed.delivery(), GiveBack::Backoff).await;
    assert!(low <= millis(BACKOFF) && millis(BACKOFF) <= high, "{low}..{high}");

    for (job, deferrals) in [(&exact, 0), (&after, 0), (&backoff, 1)] {
        let row = stored(fixture, job).await;
        assert_eq!(row["state"], value!("ready"));
        assert_eq!(row["worker_id"], Value::Null);
        assert_eq!(row["lease_deadline"], Value::Null);
        assert_eq!(row["attempt"], value!(1));
        assert_eq!(row["execution_attempts"], value!(0));
        assert_eq!(row["deferrals"], value!(deferrals), "only a back-off counts itself");
    }
}

/// Consecutive back-offs double, and the first counted attempt ends the run:
/// the back-off after it starts from the base again.
async fn back_off_grows_and_resets(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let owner = Owner::new(app.id.clone(), WorkerId::mint());
    let job = app.job().await;
    let ceiling = support::delivery_ceiling();

    for (deferrals, pause) in [(1, BACKOFF), (2, BACKOFF * 2)] {
        let claimed = app.claim(&owner.worker_id, ceiling).await.unwrap();
        let (low, high) = paused(fixture, &app, claimed.delivery(), GiveBack::Backoff).await;
        assert!(low <= millis(pause) && millis(pause) <= high, "{low}..{high}");
        let row = stored(fixture, &job).await;
        assert_eq!(row["deferrals"], value!(deferrals));
        until_past(fixture, row["deferred_until"].as_i64().unwrap()).await;
    }

    let claimed = app.claim(&owner.worker_id, ceiling).await.unwrap();
    let renewed = app.queue.heartbeat(&owner, claimed.delivery()).await.unwrap();
    assert_eq!(stored(fixture, &job).await["deferrals"], value!(0));
    let (low, high) = paused(fixture, &app, renewed.delivery(), GiveBack::Backoff).await;
    assert!(low <= millis(BACKOFF) && millis(BACKOFF) <= high, "{low}..{high}");
    assert_eq!(stored(fixture, &job).await["deferrals"], value!(1));
}

/// A given-back row is not offered to a claim before its deferral ends, and is
/// once it has. The control is a claim the host kept: its row stays leased to
/// the worker that took it and is not offered either.
async fn waits_out_its_deferral(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let ceiling = support::delivery_ceiling();
    let worker = WorkerId::mint();

    let claimed = app.claim(&worker, ceiling).await.unwrap();
    let instant = now(fixture).await + 2_000;
    app.give_back(claimed.delivery(), GiveBack::Exact(instant)).await;
    assert!(
        app.claim(&WorkerId::mint(), ceiling).await.is_none(),
        "a deferred row was offered before its deferral ended"
    );
    until_past(fixture, instant).await;
    let again = app.claim(&WorkerId::mint(), ceiling).await.unwrap();
    assert_eq!(again.delivery().job, job);
    assert_eq!(again.delivery().attempt.get(), 2);

    let kept = App::on(&app.queue, app.zone.clone()).await;
    let held = kept.job().await;
    let claimed = kept.claim(&worker, ceiling).await.unwrap();
    assert_eq!(claimed.delivery().job, held);
    assert!(kept.claim(&WorkerId::mint(), ceiling).await.is_none());
    let row = stored(fixture, &held).await;
    assert_eq!(row["state"], value!("leased"));
    assert_eq!(row["worker_id"], value!(worker.as_str()));
}

/// No deferral counts toward the delivery budget: a job given back by every
/// deferral, each time under a budget of one attempt, is still claimable after
/// all of them. The control is one interrupted attempt under the same budget,
/// after which the job is not claimable.
async fn deferrals_are_not_attempts(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let worker = WorkerId::mint();

    for defer in [GiveBack::After(Duration::from_millis(1)), GiveBack::Backoff] {
        let claimed = app
            .claim(&worker, 1)
            .await
            .expect("a deferred job keeps its delivery budget");
        app.give_back(claimed.delivery(), defer).await;
        let row = stored(fixture, &job).await;
        assert_eq!(row["execution_attempts"], value!(0));
        until_past(fixture, row["deferred_until"].as_i64().unwrap()).await;
    }
    let claimed = app.claim(&worker, 1).await.unwrap();
    let instant = now(fixture).await + 500;
    app.give_back(claimed.delivery(), GiveBack::Exact(instant)).await;
    until_past(fixture, instant).await;
    assert_eq!(stored(fixture, &job).await["execution_attempts"], value!(0));

    let claimed = app
        .claim(&worker, 1)
        .await
        .expect("three deferrals left the budget of one attempt unspent");
    app.give_back(claimed.delivery(), GiveBack::Interrupted).await;
    assert_eq!(stored(fixture, &job).await["execution_attempts"], value!(1));
    assert!(app.claim(&worker, 1).await.is_none(), "an interrupted attempt is counted");
}

/// An interrupted attempt returns the row ready at once, held by nobody,
/// counts that attempt and ends the run of consecutive back-offs before it.
async fn interrupted_counts_once(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let ceiling = support::delivery_ceiling();
    let worker = WorkerId::mint();

    let deferred = app.claim(&worker, ceiling).await.unwrap();
    app.give_back(deferred.delivery(), GiveBack::After(Duration::from_millis(1)))
        .await;
    until_past(
        fixture,
        stored(fixture, &job).await["deferred_until"].as_i64().unwrap(),
    )
    .await;
    let claimed = app.claim(&worker, ceiling).await.unwrap();
    assert_eq!(claimed.delivery().attempt.get(), 2);
    app.give_back(claimed.delivery(), GiveBack::Interrupted).await;

    let row = stored(fixture, &job).await;
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["worker_id"], Value::Null);
    assert_eq!(row["lease_deadline"], Value::Null);
    assert_eq!(row["deferred_until"], Value::Null);
    assert_eq!(row["execution_attempts"], value!(1));
    assert_eq!(row["executed_attempt"], value!(2));
    assert_eq!(row["deferrals"], value!(0));
    let again = app
        .claim(&WorkerId::mint(), ceiling)
        .await
        .expect("an interrupted attempt is claimable at once");
    assert_eq!(again.delivery().job, job);
    assert_eq!(again.delivery().attempt.get(), 3);
}

/// An attempt its first renewal already counted is not counted again when it
/// is interrupted.
async fn renewed_then_interrupted(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let owner = Owner::new(app.id.clone(), WorkerId::mint());

    let claimed = app
        .claim(&owner.worker_id, support::delivery_ceiling())
        .await
        .unwrap();
    let renewed = app.queue.heartbeat(&owner, claimed.delivery()).await.unwrap();
    assert_eq!(stored(fixture, &job).await["execution_attempts"], value!(1));
    app.give_back(renewed.delivery(), GiveBack::Interrupted).await;
    let row = stored(fixture, &job).await;
    assert_eq!(row["execution_attempts"], value!(1));
    assert_eq!(row["executed_attempt"], value!(1));
    assert_eq!(row["state"], value!("ready"));
}

/// As many interrupted attempts as the delivery budget allows exhaust the row:
/// it is not claimable, and the zone's capacity census counts it as exhausted.
/// The control is a job given back as unpreparable as many times, which is
/// still claimable once its back-off ends and is counted as backed off.
async fn interrupted_attempts_exhaust(fixture: &Fixture) {
    let spent = App::new(fixture).await;
    let unprepared = App::on(&spent.queue, spent.zone.clone()).await;
    let budget = AppPolicy {
        max_delivery_attempts: 2,
        max_stuck_dispatches: 1,
        ..AppPolicy::default()
    };
    let policies = Policies::new();
    for app in [&spent, &unprepared] {
        policies.with_policy(&app.id, &app.zone, budget.clone());
    }
    let interrupted = spent.job().await;
    let backed_off = unprepared.job().await;
    let worker = WorkerId::mint();
    let ceiling = budget.max_delivery_attempts;

    for _ in 0..ceiling {
        let claimed = spent.claim(&worker, ceiling).await.unwrap();
        spent.give_back(claimed.delivery(), GiveBack::Interrupted).await;
    }
    assert!(spent.claim(&worker, ceiling).await.is_none());
    assert_eq!(stored(fixture, &interrupted).await["state"], value!("ready"));

    for round in 1..=ceiling {
        let claimed = unprepared.claim(&worker, ceiling).await.unwrap();
        unprepared.give_back(claimed.delivery(), GiveBack::Backoff).await;
        if round < ceiling {
            until_past(
                fixture,
                stored(fixture, &backed_off).await["deferred_until"]
                    .as_i64()
                    .unwrap(),
            )
            .await;
        }
    }

    let census = Capacity::new(
        spent.queue.clone(),
        Rc::new(StaticPool { pool_slots: 16 }),
        capacity::Options::default(),
    )
    .unwrap();
    census
        .reconcile(
            &spent.zone,
            policies.as_ref(),
            Instant::now() + Duration::from_hours(1),
        )
        .await
        .unwrap();
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("capacity_targets")
        .unwrap()
        .find(value!({"id":spent.zone.as_str()}), value!({"limit":1}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    let target = rows.into_iter().next().expect("a zone target");
    assert_eq!(target["exhausted_jobs"].as_i64(), Some(1), "{target:?}");
    assert_eq!(target["backed_off_jobs"].as_i64(), Some(1), "{target:?}");

    until_past(
        fixture,
        stored(fixture, &backed_off).await["deferred_until"]
            .as_i64()
            .unwrap(),
    )
    .await;
    let claimed = unprepared
        .claim(&worker, ceiling)
        .await
        .expect("give-backs of an unprepared app never exhaust it");
    assert_eq!(claimed.delivery().job, backed_off);
    assert_eq!(stored(fixture, &backed_off).await["execution_attempts"], value!(0));
}

case!(
    sqlite_an_unsent_delivery_is_claimable_at_once_and_counts_nothing,
    postgres_an_unsent_delivery_is_claimable_at_once_and_counts_nothing,
    unsent_counts_nothing
);

/// A delivery its host could not send returns the row ready at once, counts no
/// attempt, and leaves the run of consecutive back-offs as it was. The control
/// is the back-off before it, which set that run.
async fn unsent_counts_nothing(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let ceiling = support::delivery_ceiling();
    let worker = WorkerId::mint();

    let deferred = app.claim(&worker, ceiling).await.unwrap();
    app.give_back(deferred.delivery(), GiveBack::Backoff).await;
    until_past(
        fixture,
        stored(fixture, &job).await["deferred_until"].as_i64().unwrap(),
    )
    .await;
    let claimed = app.claim(&worker, ceiling).await.unwrap();
    app.give_back(claimed.delivery(), GiveBack::Unsent).await;

    let row = stored(fixture, &job).await;
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["worker_id"], Value::Null);
    assert_eq!(row["deferred_until"], Value::Null);
    assert_eq!(row["execution_attempts"], value!(0));
    assert_eq!(row["deferrals"], value!(1), "the run of give-backs is unchanged");
    let again = app
        .claim(&WorkerId::mint(), ceiling)
        .await
        .expect("an unsent delivery is claimable at once");
    assert_eq!(again.delivery().attempt.get(), 3);
}

/// A give-back whose pause is already known - an exact instant, a fixed delay -
/// does not lengthen the next back-off: after two of them the first back-off is
/// still the base. The control is a back-off after a back-off, which doubles.
async fn known_pauses_leave_the_back_off(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let worker = WorkerId::mint();
    let ceiling = support::delivery_ceiling();

    for defer in [GiveBack::After(Duration::from_millis(1)), GiveBack::Exact(0)] {
        let claimed = app.claim(&worker, ceiling).await.unwrap();
        app.give_back(claimed.delivery(), defer).await;
        let row = stored(fixture, &job).await;
        if let Some(until) = row["deferred_until"].as_i64() {
            until_past(fixture, until).await;
        }
    }
    assert_eq!(stored(fixture, &job).await["deferrals"], value!(0));
    for pause in [BACKOFF, BACKOFF * 2] {
        let claimed = app.claim(&worker, ceiling).await.unwrap();
        let (low, high) = paused(fixture, &app, claimed.delivery(), GiveBack::Backoff).await;
        assert!(low <= millis(pause) && millis(pause) <= high, "{low}..{high}");
        until_past(fixture, stored(fixture, &job).await["deferred_until"].as_i64().unwrap()).await;
    }
}

/// A give-back to an instant that has already passed - the occurrence a host
/// deferred to fell due before the give-back ran - leaves the row claimable at
/// once rather than refusing the give-back and leaving the row leased. The
/// control is the same give-back to an instant still ahead, which waits.
async fn elapsed_deferral(fixture: &Fixture) {
    let app = App::new(fixture).await;
    let job = app.job().await;
    let worker = WorkerId::mint();
    let ceiling = support::delivery_ceiling();

    let claimed = app.claim(&worker, ceiling).await.unwrap();
    let passed = now(fixture).await - 1_000;
    app.give_back(claimed.delivery(), GiveBack::Exact(passed)).await;
    let row = stored(fixture, &job).await;
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["worker_id"], Value::Null);
    assert_eq!(row["deferred_until"], Value::Null);
    assert_eq!(row["deferrals"], value!(0));
    let again = app.claim(&WorkerId::mint(), ceiling).await.unwrap();
    assert_eq!(again.delivery().job, job);

    let ahead = now(fixture).await + 3_600_000;
    app.give_back(again.delivery(), GiveBack::Exact(ahead)).await;
    assert_eq!(stored(fixture, &job).await["deferred_until"], value!(ahead));
    assert!(app.claim(&WorkerId::mint(), ceiling).await.is_none());
}
