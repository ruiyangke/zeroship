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

/// The journal clock, which is the one a durable wait is resolved against.
///
/// A wait is compared to the database's clock rather than the host process's,
/// so a case that schedules one has to schedule it in those terms.
async fn clock(service: &WorkflowService) -> i64 {
    let mut tx = service.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    tx.commit().await.unwrap();
    now
}

/// The two revisions one dispatch was minted at, as its task row carries them.
///
/// Both come off the same row, so a case cannot compare one dispatch's frontier
/// against another dispatch's journal. `frontier_revision` is what authorizes
/// the dispatch; `journal_revision` names the journal state it was handed.
async fn task_revisions(service: &WorkflowService, task: &str) -> (i64, i64) {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "tasks", json!({"id":task})).await;
    let revisions = (
        rows[0].integer("frontier_revision").unwrap(),
        rows[0].integer("journal_revision").unwrap(),
    );
    tx.commit().await.unwrap();
    revisions
}

/// The `state` column of one dispatch's task row.
async fn task_state(service: &WorkflowService, task: &str) -> String {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "tasks", json!({"id":task})).await;
    let state = rows[0].text("state").unwrap();
    tx.commit().await.unwrap();
    state
}

/// How many of a run's dispatches were reclaimed with nothing reported.
///
/// Counted without the journal state they died on, so a case can say that two
/// halves reached a verdict having spent the same dispatches and differ only in
/// where those dispatches' strikes are counted.
async fn expired_dispatches(service: &WorkflowService, run: &str) -> usize {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "tasks", json!({"run_id":run, "state":"expired"})).await;
    tx.commit().await.unwrap();
    rows.len()
}

/// The state of one ordinal in the journal a dispatch was handed.
fn replayed_state(task: &TaskAssignment, ordinal: i32) -> Option<&str> {
    task.invocation
        .journal
        .iter()
        .find(|step| step.ordinal == ordinal)
        .map(|step| step.state.as_str())
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
paired!(
    sqlite_a_resolved_wait_moves_the_journal_revision_with_the_journal,
    postgres_a_resolved_wait_moves_the_journal_revision_with_the_journal,
    resolving_moves_both
);
paired!(
    sqlite_an_outstanding_wait_moves_neither_the_journal_revision_nor_the_journal,
    postgres_an_outstanding_wait_moves_neither_the_journal_revision_nor_the_journal,
    outstanding_moves_neither
);
paired!(
    sqlite_journal_progress_between_reclaimed_dispatches_clears_the_older_strike,
    postgres_journal_progress_between_reclaimed_dispatches_clears_the_older_strike,
    journal_progress_clears_a_strike
);
paired!(
    sqlite_a_committed_transition_between_reclaimed_dispatches_clears_the_older_strike,
    postgres_a_committed_transition_between_reclaimed_dispatches_clears_the_older_strike,
    frontier_progress_clears_a_strike
);
paired!(
    sqlite_reclaimed_dispatches_with_nothing_in_between_spend_the_strike_budget,
    postgres_reclaimed_dispatches_with_nothing_in_between_spend_the_strike_budget,
    stalls_without_progress
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

/// One run parked on a due attempt beside a sleep, ready to be dispatched.
///
/// The service is held here because the deployment fixture has to outlive every
/// case that reads through it; destructuring this away would drop it.
struct HeldFrontier {
    service: WorkflowService,
    app: AppId,
    worker: WorkerIdentity,
    run: String,
    /// The journal clock's reading the sleep was scheduled for.
    wake_at: i64,
    _deployments: Deployments,
}

/// Bring one run to a due attempt beside an outstanding sleep.
///
/// This is the frontier every case below starts from, and it is the shape that
/// puts pressure on what a dispatch is authorized against: the attempt is what
/// makes `prepare` hand the run out at all while the sleep is still
/// outstanding, and the sleep is the one wait whose resolution nothing
/// publishes - it settles against the database clock rather than against an
/// event, so no frontier transition commits when `resolve` settles it.
///
/// `resolving` chooses whether the sleep comes due shortly or an hour out, which
/// is the single variable every pair here differs in. `strikes` is the budget
/// `prepare` spends on dispatches reclaimed with nothing reported. The lease is
/// short enough to expire inside a case, so a dispatch is reclaimed rather than
/// released: nothing reports an outcome, exactly as a worker that died would not.
async fn attempt_beside_a_sleep(
    store: Rc<OrmStore>,
    resolving: bool,
    strikes: i64,
) -> HeldFrontier {
    let (service, app, _, deployments) = registered_service(store).await;
    let worker = WorkerIdentity::new("step-retries".into()).unwrap();
    let run = started(&service, &app, 1).await;
    service
        .fixture_install(
            &app,
            leased_policy(
                3,
                AppPolicy {
                    retry_delay_ms: 1,
                    lease_ms: 1_000,
                    max_stuck_dispatches: strikes,
                    ..Default::default()
                },
            ),
        )
        .unwrap();

    // Scheduled against the journal clock so the wait is commensurate with what
    // resolves it, and far enough out that the first dispatch is minted while it
    // is still outstanding.
    let wake_at = clock(&service).await + if resolving { 2_500 } else { 3_600_000 };
    let wake_at_text = chrono::DateTime::from_timestamp_millis(wake_at)
        .unwrap()
        .to_rfc3339();

    // A suspension has to trail its batch, so the frontier under test is
    // reached over two dispatches: the attempt is held first, and the sleep is
    // opened beside it while it still has executions left.
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(failed_step(3, None)),
        )
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"Sleep", "ordinal":1, "name":"settle", "nameOccurrence":0,
                 "wakeAt":wake_at_text},
            ])),
        )
        .await
        .unwrap();
    assert_eq!(
        step_state(&service, &app, &run, 0).await.as_deref(),
        Some("retrying"),
        "the attempt must be held"
    );
    assert_eq!(
        step_state(&service, &app, &run, 1).await.as_deref(),
        Some("running"),
        "the sleep must be outstanding"
    );
    assert_eq!(run_state(&service, &app, &run).await, "queued");

    HeldFrontier {
        service,
        app,
        worker,
        run,
        wake_at,
        _deployments: deployments,
    }
}

/// A wait that comes due between two dispatches moves the journal revision.
///
/// `frontier_revision` cannot carry this, and the case says so rather than
/// asserting around it. That revision authorizes one dispatch and is pinned for
/// the whole life of that authorization: the advance job is published at it,
/// `publication_id` hashes it into an immutable operation, `assign` stamps it on
/// the task inside the transaction that consumes the job, and `authorize_task`
/// refuses every later claim whose task disagrees with the run. So it stands
/// still here, across both dispatches, in both halves of the pair - and a
/// journal that moved underneath it would leave two dispatches disagreeing
/// about one revision.
///
/// The shape that puts pressure on it is a due attempt beside an unresolved
/// sleep. The attempt is what makes `prepare` hand the run out at all while the
/// sleep is still outstanding, and the sleep is the one wait whose resolution
/// nothing publishes: it settles against the clock rather than against an event,
/// so no frontier transition commits when `resolve` settles it. The lease then
/// expires with nothing reported, which is the other half - reclaiming a
/// dispatch does not move a revision either.
async fn resolving_moves_both(store: Rc<OrmStore>) {
    journal_revision_tracks_the_journal(store, true).await;
}

/// The same two dispatches with the wait left outstanding, moving neither.
///
/// This is what says the comparison discriminates. Both halves reclaim a
/// dispatch and mint a second one, and they differ in one thing: whether the
/// sleep comes due in between. A journal comparison that failed on the pair
/// regardless - on an identity, a timestamp, or anything else a second dispatch
/// mints fresh - would fail here too, and a revision that moved on every
/// dispatch rather than on every rewrite would move here too.
async fn outstanding_moves_neither(store: Rc<OrmStore>) {
    journal_revision_tracks_the_journal(store, false).await;
}

/// The journal revision moves exactly when the replayed journal does.
///
/// `resolving` chooses whether the sleep comes due between the two dispatches,
/// and the assertions at the end are stated against it on both sides: the
/// journals differ exactly when the case resolved something, and so do the
/// journal revisions. Pinning each side to the case's own intent is what keeps
/// the biconditional from being satisfied by two quantities that are both
/// constant.
async fn journal_revision_tracks_the_journal(store: Rc<OrmStore>, resolving: bool) {
    // A budget no half of this pair reaches: the strikes are the other pair's
    // subject, and a stall here would end the run before the second dispatch.
    let held = attempt_beside_a_sleep(store, resolving, 4).await;
    let (service, app, worker, run, wake_at) = (
        &held.service,
        &held.app,
        &held.worker,
        held.run.as_str(),
        held.wake_at,
    );

    let first = service
        .poll(worker)
        .await
        .unwrap()
        .expect("the due attempt must be dispatched past the unresolved sleep");
    assert_eq!(first.invocation.run_id, run);
    assert_eq!(
        replayed_state(&first, 1),
        Some("running"),
        "the first dispatch must be minted while the sleep is still outstanding"
    );
    let (first_frontier, first_journal_revision) = task_revisions(service, &first.id).await;
    let first_journal = serde_json::to_value(&first.invocation.journal).unwrap();

    // The worker dies without reporting. Nothing ends the lease but the clock,
    // and nothing announces the sleep's expiry, so the next poll reclaims a
    // dispatch whose wait has silently come due.
    compio::time::sleep(std::time::Duration::from_secs(4)).await;
    let now = clock(service).await;
    assert_eq!(
        now >= wake_at,
        resolving,
        "the sleep must be past due exactly when the case intends it: \
         now={now} wake_at={wake_at}"
    );

    let second = service
        .poll(worker)
        .await
        .unwrap()
        .expect("the reclaimed run must be dispatched again");
    assert_eq!(second.invocation.run_id, run);
    assert_ne!(
        second.id, first.id,
        "the reclaimed run must be handed a new dispatch"
    );
    assert_eq!(
        task_state(service, &first.id).await,
        "expired",
        "the second dispatch must follow a lease reclaimed with nothing reported"
    );
    assert_eq!(
        step_state(service, app, run, 1).await.as_deref(),
        Some(if resolving { "completed" } else { "running" }),
        "the resolve ahead of the second dispatch must have rewritten the sleep \
         exactly when the case intends it"
    );
    assert_eq!(
        replayed_state(&second, 1),
        Some(if resolving { "completed" } else { "running" }),
        "the second dispatch must be handed the sleep as the journal now holds it"
    );
    let (second_frontier, second_journal_revision) = task_revisions(service, &second.id).await;
    let second_journal = serde_json::to_value(&second.invocation.journal).unwrap();

    // The authorization the dispatch carries is pinned, so it is the same under
    // both halves and cannot be the quantity that tracks a rewrite.
    assert_eq!(
        second_frontier, first_frontier,
        "a reclaimed dispatch is authorized at the frontier revision its \
         predecessor was, so nothing here may move it"
    );

    let journals_differ = second_journal != first_journal;
    let revisions_differ = second_journal_revision != first_journal_revision;
    assert_eq!(
        journals_differ, resolving,
        "the second dispatch's journal must differ from the first's exactly when \
         the sleep came due between them: first={first_journal} \
         second={second_journal}"
    );
    assert_eq!(
        revisions_differ, resolving,
        "the journal revision must move exactly when the sleep came due between \
         the two dispatches: first={first_journal_revision} \
         second={second_journal_revision}"
    );
    assert_eq!(
        revisions_differ, journals_differ,
        "the journal revision and the journal a dispatch replays must move \
         together: revisions {first_journal_revision} -> \
         {second_journal_revision}, journals {first_journal} -> {second_journal}"
    );
    assert!(
        second_journal_revision >= first_journal_revision,
        "the journal revision must not go backwards within one generation: \
         {first_journal_revision} -> {second_journal_revision}"
    );
}

/// What happens to one run between the two dispatches a strike case reclaims.
///
/// `prepare` counts a strike against the journal state the run is at, and that
/// state takes two revisions to name because each covers commits the other does
/// not. These are the three arms that says so: one where neither moves, and one
/// for each revision moving alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Interlude {
    /// Nothing. The run is genuinely stuck, so the budget must be spent.
    Nothing,
    /// The sleep comes due, rewriting a step in place. Nothing publishes that,
    /// so the frontier revision stands still and the journal revision moves.
    JournalRewrite,
    /// A dispatch reports a batch that only opens a new ordinal. It rewrites no
    /// existing step, so the journal revision stands still and the committed
    /// transition moves the frontier revision.
    FrontierTransition,
}

/// Journal progress between two reclaimed dispatches clears the older strike.
///
/// `prepare` brings a run to rest at `stalled` once `max_stuck_dispatches`
/// dispatches have been reclaimed with nothing reported. Progress the creator
/// never reported is still progress: a sleep that came due rewrote the journal,
/// so a dispatch that died against the state before it was not a strike against
/// the state after it, and carrying it forward stalls a run that is moving.
///
/// The control is [`stalls_without_progress`], which differs only in whether the
/// sleep comes due.
async fn journal_progress_clears_a_strike(store: Rc<OrmStore>) {
    strikes_are_keyed_to_both_revisions(store, Interlude::JournalRewrite).await;
}

/// A committed frontier transition between them clears the older strike too.
///
/// This is the other half of the key, and it is the half the journal revision
/// cannot carry: a batch that only opens a new ordinal inserts a step rather
/// than rewriting one, so nothing goes through the rewrite funnel while the
/// transition still supersedes the frontier the older dispatch died on. Keying
/// the strikes on the journal revision alone would stall this run.
///
/// The control is [`stalls_without_progress`], which differs only in whether a
/// dispatch in between reported anything.
async fn frontier_progress_clears_a_strike(store: Rc<OrmStore>) {
    strikes_are_keyed_to_both_revisions(store, Interlude::FrontierTransition).await;
}

/// The same two reclaimed dispatches with nothing in between, stalling.
///
/// This is what says the two cases above discriminate. All three arms reclaim
/// exactly two dispatches against a two-strike budget; the other two each move
/// one revision in between and this one moves neither. A key that dropped every
/// strike, or counted them against nothing, would dispatch this run too.
async fn stalls_without_progress(store: Rc<OrmStore>) {
    strikes_are_keyed_to_both_revisions(store, Interlude::Nothing).await;
}

/// Let one dispatch report a batch that only opens a new ordinal.
///
/// The steps already in the journal keep their rows, so nothing reaches the
/// rewrite funnel, while the committed transition still supersedes the frontier
/// an earlier dispatch died at. The new wait is scheduled far enough out that it
/// never resolves inside a case, and the attempt still due beside it is what
/// keeps the run dispatchable rather than suspended.
async fn open_a_new_ordinal(held: &HeldFrontier) {
    let (service, app, worker, run) = (&held.service, &held.app, &held.worker, held.run.as_str());
    let reporting = service
        .poll(worker)
        .await
        .unwrap()
        .expect("one strike of a two-strike budget must not stall the run");
    assert_eq!(reporting.invocation.run_id, run);
    let far = chrono::DateTime::from_timestamp_millis(clock(service).await + 3_600_000)
        .unwrap()
        .to_rfc3339();
    service
        .complete(
            worker,
            &reporting.id,
            &reporting.token,
            execution(json!([
                {"kind":"Sleep", "ordinal":2, "name":"second", "nameOccurrence":0,
                 "wakeAt":far},
            ])),
        )
        .await
        .unwrap();
    assert_eq!(
        step_state(service, app, run, 2).await.as_deref(),
        Some("running"),
        "the reporting dispatch must have opened a new ordinal"
    );
}

/// Two dispatches reclaimed with nothing reported against a two-strike budget.
///
/// The verdict is read after the second reclaim, when the budget is exactly
/// spent if both strikes still count. `interlude` is the single intervention
/// that separates the arms, and every arm reaches that verdict having reclaimed
/// the same dispatches, which is what makes the stall a statement about where
/// the strikes are counted rather than how many there are.
async fn strikes_are_keyed_to_both_revisions(store: Rc<OrmStore>, interlude: Interlude) {
    let resolving = interlude == Interlude::JournalRewrite;
    let held = attempt_beside_a_sleep(store, resolving, 2).await;
    let (service, app, worker, run, wake_at) = (
        &held.service,
        &held.app,
        &held.worker,
        held.run.as_str(),
        held.wake_at,
    );

    let first = service
        .poll(worker)
        .await
        .unwrap()
        .expect("the due attempt must be dispatched past the unresolved sleep");
    assert_eq!(first.invocation.run_id, run);

    // The first worker dies. The reclaim that follows is the poll that resolves
    // the sleep when the case intends it to.
    compio::time::sleep(std::time::Duration::from_secs(4)).await;
    let now = clock(service).await;
    assert_eq!(
        now >= wake_at,
        resolving,
        "the sleep must be past due exactly when the case intends it: \
         now={now} wake_at={wake_at}"
    );
    assert_eq!(
        step_state(service, app, run, 1).await.as_deref(),
        Some("running"),
        "nothing may have resolved the sleep before the reclaim that is meant to"
    );

    if interlude == Interlude::FrontierTransition {
        open_a_new_ordinal(&held).await;
    }

    let second = service
        .poll(worker)
        .await
        .unwrap()
        .expect("one strike of a two-strike budget must not stall the run");
    assert_eq!(second.invocation.run_id, run);
    assert_eq!(
        step_state(service, app, run, 1).await.as_deref(),
        Some(if resolving { "completed" } else { "running" }),
        "the sleep must have been rewritten exactly when the case intends it"
    );

    // The second worker dies as well, so every arm arrives at the verdict having
    // spent two dispatches on nothing. What differs is only which journal state
    // each of those two strikes sits at.
    compio::time::sleep(std::time::Duration::from_secs(2)).await;
    let third = service.poll(worker).await.unwrap();
    assert_eq!(
        expired_dispatches(service, run).await,
        2,
        "every arm must reach the verdict with the same dispatches reclaimed, or \
         the stall is not about where the strikes are counted"
    );
    assert_eq!(
        run_state(service, app, run).await,
        if interlude == Interlude::Nothing {
            "stalled"
        } else {
            "running"
        },
        "a run whose journal or frontier moved between its reclaimed dispatches \
         must be dispatched again, and one where neither moved must stall: \
         {interlude:?}"
    );
    assert_eq!(
        third.is_some(),
        interlude != Interlude::Nothing,
        "the third dispatch exists exactly when the older strike was counted \
         against a state the run has left behind: {interlude:?}"
    );
    if let Some(third) = third {
        assert_eq!(third.invocation.run_id, run);
        assert_eq!(
            replayed_state(&third, 1),
            Some(if resolving { "completed" } else { "running" }),
            "the dispatch that outlived the strikes must replay the journal as it \
             now stands"
        );
    }
}
