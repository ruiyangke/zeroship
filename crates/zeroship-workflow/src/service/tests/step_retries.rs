#![expect(clippy::future_not_send, reason = "creator journal cases use compio")]

//! `StepConfig.retries` as the journal enforces it.
//!
//! The contract these cases bind is that a step with attempts left does not
//! settle its ordinal. The replay bridge re-executes an ordinal it holds no row
//! for, so the visible consequence of an attempt being left is a HOLE in the
//! journal the next dispatch is handed, and the visible consequence of the last
//! attempt being spent is that the hole closes with a `failed` row the run then
//! fails on.

use super::*;
use crate::{
    operations::RunState,
    service::{TaskAssignment, WorkerIdentity},
};

/// The journal ordinals the next dispatch would replay, in order.
///
/// Reads through the same `invocation` the executor receives, so a case asserts
/// on what the body will actually see rather than on rows behind it.
async fn replayed_ordinals(service: &WorkflowService, worker: &WorkerIdentity) -> Vec<i64> {
    let task = service
        .poll(worker)
        .await
        .unwrap()
        .expect("a queued run must be dispatched");
    let ordinals = task
        .invocation
        .journal
        .iter()
        .map(|step| i64::from(step.ordinal))
        .collect();
    service
        .release(worker, &task.id, &task.token)
        .await
        .unwrap();
    ordinals
}

/// The next dispatch of one named run.
///
/// Every other run the app has queued is leased and held rather than released,
/// so it leaves the queue without finishing and cannot be handed out again. A
/// case that simply polled would otherwise assert against whichever run the
/// dispatcher reached first, which is not the one under test.
async fn poll_for(
    service: &WorkflowService,
    worker: &WorkerIdentity,
    run: &str,
) -> TaskAssignment {
    let mut held = Vec::new();
    loop {
        let task = service
            .poll(worker)
            .await
            .unwrap()
            .expect("the run under test must be dispatchable");
        if task.invocation.run_id == run {
            return task;
        }
        assert!(held.len() < 4, "the run under test was never dispatched");
        held.push(task);
    }
}

/// The `state` column of one step row.
async fn step_state(
    service: &WorkflowService,
    app: &AppId,
    run: &str,
    ordinal: i64,
) -> Option<String> {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "steps",
        json!({"app_id":app.as_str(), "run_id":run, "ordinal":ordinal}),
    )
    .await;
    let state = rows.first().map(|row| row.text("state").unwrap());
    tx.commit().await.unwrap();
    state
}

/// The `state` column of the run row.
async fn run_state(service: &WorkflowService, app: &AppId, run: &str) -> String {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "runs", json!({"app_id":app.as_str(), "id":run})).await;
    let state = rows[0].text("state").unwrap();
    tx.commit().await.unwrap();
    state
}

/// When the run is next allowed to be dispatched.
async fn run_due(service: &WorkflowService, app: &AppId, run: &str) -> Option<i64> {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "runs", json!({"app_id":app.as_str(), "id":run})).await;
    let due = rows[0].optional_integer("due_at").unwrap();
    tx.commit().await.unwrap();
    due
}

/// A step failure, as the replay bridge reports one, declaring its ceiling.
fn failed_step(max_attempts: i64, retryable: Option<bool>) -> serde_json::Value {
    let mut error = json!({"type":"Error", "message":"intentional failure"});
    if let Some(retryable) = retryable {
        error["retryable"] = json!(retryable);
    }
    json!([{
        "kind":"RunFailed", "ordinal":0, "name":"charge", "nameOccurrence":0,
        "maxAttempts":max_attempts, "error":error,
    }])
}

/// Start a run under a policy whose inter-attempt delay the case chooses.
async fn started(service: &WorkflowService, app: &AppId, delay_ms: i64) -> String {
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .policies
        .fixture_install(
            app,
            leased_policy(
                2,
                AppPolicy {
                    retry_delay_ms: delay_ms,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    run.id
}

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&directory.path().join("retries.sqlite")).await,
            )))
            .await;
        }

        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

paired!(
    sqlite_a_step_with_attempts_left_is_re_executed_and_its_last_failure_is_final,
    postgres_a_step_with_attempts_left_is_re_executed_and_its_last_failure_is_final,
    spends_every_attempt
);
paired!(
    sqlite_a_step_with_one_attempt_fails_its_run_on_the_first_failure,
    postgres_a_step_with_one_attempt_fails_its_run_on_the_first_failure,
    single_attempt_is_final
);
paired!(
    sqlite_a_failure_declared_unretryable_spends_no_further_attempt,
    postgres_a_failure_declared_unretryable_spends_no_further_attempt,
    unretryable_is_final
);
paired!(
    sqlite_a_retried_step_that_succeeds_records_what_it_cost,
    postgres_a_retried_step_that_succeeds_records_what_it_cost,
    success_records_attempts
);
paired!(
    sqlite_a_declared_ceiling_above_the_app_limit_is_refused,
    postgres_a_declared_ceiling_above_the_app_limit_is_refused,
    ceiling_is_enforced
);
paired!(
    sqlite_a_pending_attempt_holds_the_run_until_its_delay_elapses,
    postgres_a_pending_attempt_holds_the_run_until_its_delay_elapses,
    delay_is_durable
);
paired!(
    sqlite_a_retried_outcome_is_validated_like_a_first_one,
    postgres_a_retried_outcome_is_validated_like_a_first_one,
    replacement_is_validated
);
paired!(
    sqlite_a_due_attempt_is_dispatched_past_an_unresolved_wait,
    postgres_a_due_attempt_is_dispatched_past_an_unresolved_wait,
    attempt_outruns_a_wait
);

/// Three declared attempts are three executions of the body, and only the third
/// failure settles the ordinal.
///
/// The paired control is [`single_attempt_is_final`], which differs in the
/// declared ceiling alone: without the hole in the replayed journal this case
/// asserts on, the body would never run a second time.
async fn spends_every_attempt(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 1).await;

    for attempt in 1..=2 {
        let task = service.poll(&worker).await.unwrap().expect("a run with attempts left stays dispatchable");
        service
            .complete(&worker, &task.id, &task.token, execution(failed_step(3, None)))
            .await
            .unwrap();
        assert_eq!(
            step_state(&service, &app, &run, 0).await.as_deref(),
            Some("retrying"),
            "attempt {attempt} left the ordinal settled"
        );
        assert_eq!(run_state(&service, &app, &run).await, "queued");
        assert_eq!(
            replayed_ordinals(&service, &worker).await,
            Vec::<i64>::new(),
            "attempt {attempt} left a row the bridge would replay instead of re-running the body"
        );
    }

    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(&worker, &task.id, &task.token, execution(failed_step(3, None)))
        .await
        .unwrap();
    assert_eq!(
        step_state(&service, &app, &run, 0).await.as_deref(),
        Some("failed"),
        "the last attempt must settle the ordinal"
    );
    let task = service.poll(&worker).await.unwrap().expect("the settled failure replays into the body");
    assert_eq!(
        task.invocation
            .journal
            .iter()
            .map(|step| (i64::from(step.ordinal), step.state.clone()))
            .collect::<Vec<_>>(),
        vec![(0, "failed".to_string())],
    );
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunFailed","error":{"type":"Error","message":"intentional failure"}}])),
        )
        .await
        .unwrap();
    let scope = service.fixture_app(app.clone());
    assert_eq!(scope.status(&run).await.unwrap().state, RunState::Failed);
}

/// The control for [`spends_every_attempt`]: one declared attempt, everything
/// else identical, and the first failure is the settled one.
async fn single_attempt_is_final(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 1).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(&worker, &task.id, &task.token, execution(failed_step(1, None)))
        .await
        .unwrap();
    assert_eq!(
        step_state(&service, &app, &run, 0).await.as_deref(),
        Some("failed")
    );
    assert_eq!(replayed_ordinals(&service, &worker).await, vec![0]);
}

/// The control for [`spends_every_attempt`] on the other variable: the same
/// three declared attempts, and an error that declares it cannot be cleared by
/// running the body again.
async fn unretryable_is_final(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 1).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(failed_step(3, Some(false))),
        )
        .await
        .unwrap();
    assert_eq!(
        step_state(&service, &app, &run, 0).await.as_deref(),
        Some("failed"),
        "a declared-unretryable failure must settle even with attempts left"
    );
    assert_eq!(replayed_ordinals(&service, &worker).await, vec![0]);
}

/// An attempt that succeeds closes the ordinal and keeps the count it cost, so a
/// completed step says how many executions it took rather than implying one.
async fn success_records_attempts(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 1).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(&worker, &task.id, &task.token, execution(failed_step(3, None)))
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"StepCompleted","ordinal":0,"name":"charge","output":"ok"}])),
        )
        .await
        .unwrap();

    assert_eq!(
        step_state(&service, &app, &run, 0).await.as_deref(),
        Some("completed")
    );
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "steps",
        json!({"app_id":app.as_str(), "run_id":run.clone(), "ordinal":0}),
    )
    .await;
    let record: serde_json::Value =
        serde_json::from_str(&rows[0].text("record").unwrap()).unwrap();
    tx.commit().await.unwrap();
    assert_eq!(record["step"]["attempts"], json!(2));
    assert_eq!(record["step"]["output"], json!("ok"));
    assert_eq!(replayed_ordinals(&service, &worker).await, vec![0]);
}

/// The declared ceiling is checked against the app's budget, and a declaration
/// above it is refused rather than clamped down to it.
async fn ceiling_is_enforced(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_step_attempts: 2,
                    ..Default::default()
                },
            ),
        )
        .unwrap();

    let task = service.poll(&worker).await.unwrap().unwrap();
    let refused = service
        .complete(&worker, &task.id, &task.token, execution(failed_step(3, None)))
        .await;
    assert!(
        matches!(refused, Err(WorkflowServiceError::InvalidRequest(_))),
        "a ceiling above the app budget must be refused: {refused:?}"
    );
    assert_eq!(
        step_state(&service, &app, &run.id, 0).await,
        None,
        "a refused ceiling must leave no attempt state behind"
    );

    // The same dispatch, reported again inside the budget, so the two halves
    // differ in the declared ceiling and nothing else.
    service
        .release(&worker, &task.id, &task.token)
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(&worker, &task.id, &task.token, execution(failed_step(2, None)))
        .await
        .expect("a ceiling inside the app budget is accepted");
    assert_eq!(
        step_state(&service, &app, &run.id, 0).await.as_deref(),
        Some("retrying")
    );
}

/// A frontier can hold an unresolved wait and a failed step at once, and the
/// attempt still has to run.
///
/// A child started beside a failing step is the shape that reaches this: the
/// outcome normalizer lets a `Child` sit ahead of another entry, so both land in
/// one batch. The child is what makes the run look idle to the dispatcher, and
/// the attempt is the only thing that can move it, so a rule reading only the
/// wait parks the run until the child it is not waiting on finishes.
///
/// It also pins the hole to one ordinal: the child beside the retried step keeps
/// its own row and replays, and only the step with attempts left is re-executed.
async fn attempt_outruns_a_wait(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 1).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"Child", "ordinal":0, "name":"Example", "nameOccurrence":0,
                 "childWorkflowName":"Example", "input":null, "options":{}},
                {"kind":"RunFailed", "ordinal":1, "name":"charge", "nameOccurrence":0,
                 "maxAttempts":3, "error":{"type":"Error","message":"intentional failure"}},
            ])),
        )
        .await
        .unwrap();
    assert_eq!(
        step_state(&service, &app, &run, 0).await.as_deref(),
        Some("running"),
        "the child must still be outstanding"
    );
    assert_eq!(
        step_state(&service, &app, &run, 1).await.as_deref(),
        Some("retrying")
    );
    assert_eq!(run_state(&service, &app, &run).await, "queued");

    // The child this run started is queued too, and `poll_for` holds its lease
    // rather than finishing it, so the child is still outstanding when the
    // parent's own dispatch arrives.
    let task = poll_for(&service, &worker, &run).await;
    assert_eq!(
        task.invocation
            .journal
            .iter()
            .map(|step| i64::from(step.ordinal))
            .collect::<Vec<_>>(),
        vec![0],
        "only the step with attempts left may be re-executed"
    );
}

/// An outcome replacing a held attempt goes through the same admission as one
/// creating a row.
///
/// A completion carrying a payload reference has to claim that payload, and the
/// replacement path is the one where that is easy to lose: it writes to a row
/// that already exists, so it can look like a plain update. The two arms differ
/// only in whether the step had already failed once, and the second must be
/// refused for the same reason the first is.
async fn replacement_is_validated(store: Rc<OrmStore>) {
    let unclaimed = json!([{
        "kind":"StepCompleted", "ordinal":0, "name":"charge", "nameOccurrence":0,
        "outputRef":{"hash":"a".repeat(64), "size":1},
    }]);
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let first = started(&service, &app, 1).await;
    let second = started(&service, &app, 1).await;

    let task = poll_for(&service, &worker, &first).await;
    let refused = service
        .complete(&worker, &task.id, &task.token, execution(unclaimed.clone()))
        .await;
    assert!(
        refused.is_err(),
        "a first completion may not claim a payload it never staged: {refused:?}"
    );
    assert_eq!(step_state(&service, &app, &first, 0).await, None);

    let task = poll_for(&service, &worker, &second).await;
    service
        .complete(&worker, &task.id, &task.token, execution(failed_step(2, None)))
        .await
        .unwrap();
    assert_eq!(
        step_state(&service, &app, &second, 0).await.as_deref(),
        Some("retrying")
    );

    let task = poll_for(&service, &worker, &second).await;
    let refused = service
        .complete(&worker, &task.id, &task.token, execution(unclaimed))
        .await;
    assert!(
        refused.is_err(),
        "a retried completion may not claim a payload it never staged: {refused:?}"
    );
    assert_eq!(
        step_state(&service, &app, &second, 0).await.as_deref(),
        Some("retrying"),
        "a refused replacement must leave the held attempt as it was"
    );
}

/// The next attempt is a durable fact on the run, not a timer in the isolate
/// that failed: the worker that ran the attempt is gone, so the only thing that
/// can hold the run back is `due_at`.
async fn delay_is_durable(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 3_600_000).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    let before = {
        let mut tx = service.begin().await.unwrap();
        let now = tx.now().await.unwrap();
        tx.commit().await.unwrap();
        now
    };
    service
        .complete(&worker, &task.id, &task.token, execution(failed_step(3, None)))
        .await
        .unwrap();

    assert_eq!(run_state(&service, &app, &run).await, "queued");
    let due = run_due(&service, &app, &run).await.expect("a pending attempt must be scheduled");
    assert!(
        due >= before + 3_600_000,
        "the attempt was scheduled without its delay: due={due} before={before}"
    );
    assert!(
        service.poll(&worker).await.unwrap().is_none(),
        "a run whose next attempt is not due must not be dispatched"
    );
}
