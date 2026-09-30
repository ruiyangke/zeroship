#![expect(
    clippy::future_not_send,
    reason = "completion batch cases own compio-local creator transactions"
)]

//! How wide one completion may be.
//!
//! An executor reports a whole frontier in a single call, so the batch, not the
//! step, is the unit the app's limits bound. The ceiling counts outcomes; what
//! they encode to is bounded separately by `max_input_bytes`. The floor is the
//! other end of that same count: a completion reports something, or it is not a
//! completion. A case here therefore varies the count alone, and reads the
//! consequence off the journal the next dispatch replays rather than off the
//! rows behind it.

use super::{job_door, *};
use crate::{service::AppWorkflows, WorkflowExecution};

#[compio::test]
async fn sqlite_a_completion_past_the_frontier_ceiling_is_refused_whole() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    Box::pin(width_contract(Rc::new(store))).await;
}

#[compio::test]
async fn postgres_a_completion_past_the_frontier_ceiling_is_refused_whole() {
    let fixture = PostgresFixture::start().await;
    Box::pin(width_contract(Rc::new(fixture.store.clone()))).await;
}

#[compio::test]
async fn sqlite_a_completion_carrying_no_outcome_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    Box::pin(floor_contract(Rc::new(store))).await;
}

#[compio::test]
async fn postgres_a_completion_carrying_no_outcome_is_refused() {
    let fixture = PostgresFixture::start().await;
    Box::pin(floor_contract(Rc::new(fixture.store.clone()))).await;
}

/// The app's ceiling for these cases.
///
/// Lowered so a batch can cross it with outcomes a case can enumerate and a
/// reader can count. The production default is untouched: what a test varies is
/// the policy the app was granted, never the limit the engine enforces.
const CEILING: usize = 2;

/// A batch of completed steps, one per ordinal.
fn steps(count: usize) -> crate::WorkflowExecution {
    let outcomes: Vec<serde_json::Value> = (0..count)
        .map(|ordinal| {
            json!({
                "kind":"StepCompleted", "ordinal":ordinal, "name":format!("step-{ordinal}"),
                "nameOccurrence":0, "output":ordinal,
            })
        })
        .collect();
    execution(json!(outcomes))
}

/// The ordinals the next dispatch of `run` would replay.
///
/// Read through the same `invocation` an executor receives, so the assertion is
/// on what a replayed body would see. The dispatch is released rather than
/// completed, leaving the run where it was found.
async fn replayed_ordinals(
    scope: &AppWorkflows,
    worker: &job_door::Worker,
    run: &str,
) -> Vec<i32> {
    let claimed = worker.claim(scope).await;
    assert_eq!(claimed.assignment().invocation.run_id, run);
    let ordinals = claimed
        .assignment()
        .invocation
        .journal
        .iter()
        .map(|step| step.ordinal)
        .collect();
    claimed.give_back(scope, worker).await.unwrap();
    ordinals
}

/// A batch past the ceiling is refused, and one exactly at it is accepted.
///
/// The two halves are the same run, reported by the same worker under the same
/// policy; the one variable is how many outcomes the batch carries. Without the
/// accepted half the refusal would not distinguish a ceiling that binds from an
/// engine that refuses every batch of completed steps.
async fn width_contract(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = job_door::Worker::new(&app_id).await;
    service
        .policies
        .fixture_install(
            &app_id,
            leased_policy(
                2,
                AppPolicy {
                    max_frontier: CEILING,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();

    let claimed = worker.claim(&scope).await;
    assert_eq!(claimed.assignment().invocation.run_id, run.id);
    assert!(claimed.assignment().invocation.journal.is_empty());
    let refused = claimed.finish(&scope, steps(CEILING + 1)).await;
    assert!(
        matches!(&refused, Err(WorkflowServiceError::ResourceExhausted(message))
            if message == "workflow frontier exceeds the configured limit"),
        "a batch past the ceiling must be refused: {refused:?}"
    );
    claimed.give_back(&scope, &worker).await.unwrap();
    assert_eq!(
        replayed_ordinals(&scope, &worker, &run.id).await,
        Vec::<i32>::new(),
        "a refused batch settles none of the ordinals it carried"
    );

    // The same run, reported one outcome narrower.
    let claimed = worker.claim(&scope).await;
    assert_eq!(claimed.assignment().invocation.run_id, run.id);
    claimed
        .finish(&scope, steps(CEILING))
        .await
        .expect("a batch exactly at the ceiling is accepted");
    assert_eq!(
        replayed_ordinals(&scope, &worker, &run.id).await,
        (0..i32::try_from(CEILING).unwrap()).collect::<Vec<_>>(),
        "an accepted batch settles every ordinal it carried"
    );
}

/// A completion carrying nothing is refused, and one carrying a single outcome
/// is accepted.
///
/// The empty batch is assembled here rather than through `execution`, because
/// the runtime decoder refuses an empty array before a `WorkflowExecution`
/// exists. `complete` takes the batch itself, so this floor is what a host that
/// builds its own frontier - as the runner does when it trims a batch around a
/// rejected upload - is held to.
///
/// The two halves are the same run, reported by the same worker under the same
/// policy; the one variable is whether the batch carries an outcome. Without
/// the accepted half the refusal would not distinguish a floor that binds from
/// an engine that refuses whatever narrow batch it is handed.
async fn floor_contract(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = job_door::Worker::new(&app_id).await;
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();

    let claimed = worker.claim(&scope).await;
    assert_eq!(claimed.assignment().invocation.run_id, run.id);
    assert!(claimed.assignment().invocation.journal.is_empty());
    let refused = claimed
        .finish(
            &scope,
            WorkflowExecution {
                outcomes: Vec::new(),
            },
        )
        .await;
    assert!(
        matches!(&refused, Err(WorkflowServiceError::InvalidRequest(message))
            if message == "workflow completion requires an outcome"),
        "a completion carrying no outcome must be refused: {refused:?}"
    );
    claimed.give_back(&scope, &worker).await.unwrap();
    assert_eq!(
        replayed_ordinals(&scope, &worker, &run.id).await,
        Vec::<i32>::new(),
        "a refused completion settles nothing"
    );

    // The same run, reported one outcome wider.
    let claimed = worker.claim(&scope).await;
    assert_eq!(claimed.assignment().invocation.run_id, run.id);
    claimed
        .finish(&scope, steps(1))
        .await
        .expect("a completion carrying one outcome is accepted");
    assert_eq!(
        replayed_ordinals(&scope, &worker, &run.id).await,
        vec![0],
        "an accepted completion settles the ordinal it carried"
    );
}
