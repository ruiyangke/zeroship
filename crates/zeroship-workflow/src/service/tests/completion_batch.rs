#![expect(
    clippy::future_not_send,
    reason = "completion batch cases own compio-local creator transactions"
)]

//! How wide one completion may be.
//!
//! An executor reports a whole frontier in a single call, so the batch, not the
//! step, is the unit the app's ceiling bounds. The ceiling counts outcomes; what
//! they encode to is bounded separately by `max_input_bytes`. A case here
//! therefore varies the count alone, and reads the consequence off the journal
//! the next dispatch replays rather than off the rows behind it.

use super::*;
use crate::service::WorkerIdentity;

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
    service: &WorkflowService,
    worker: &WorkerIdentity,
    run: &str,
) -> Vec<i32> {
    let task = service
        .poll(worker)
        .await
        .unwrap()
        .expect("the run under test must be dispatchable");
    assert_eq!(task.invocation.run_id, run);
    let ordinals = task
        .invocation
        .journal
        .iter()
        .map(|step| step.ordinal)
        .collect();
    service
        .release(worker, &task.id, &task.token)
        .await
        .unwrap();
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
    let worker = WorkerIdentity::new("completion-batch".into()).unwrap();
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

    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    assert!(task.invocation.journal.is_empty());
    let refused = service
        .complete(&worker, &task.id, &task.token, steps(CEILING + 1))
        .await;
    assert!(
        matches!(&refused, Err(WorkflowServiceError::ResourceExhausted(message))
            if message == "workflow frontier exceeds the configured limit"),
        "a batch past the ceiling must be refused: {refused:?}"
    );
    service
        .release(&worker, &task.id, &task.token)
        .await
        .unwrap();
    assert_eq!(
        replayed_ordinals(&service, &worker, &run.id).await,
        Vec::<i32>::new(),
        "a refused batch settles none of the ordinals it carried"
    );

    // The same run, reported one outcome narrower.
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    service
        .complete(&worker, &task.id, &task.token, steps(CEILING))
        .await
        .expect("a batch exactly at the ceiling is accepted");
    assert_eq!(
        replayed_ordinals(&service, &worker, &run.id).await,
        (0..i32::try_from(CEILING).unwrap()).collect::<Vec<_>>(),
        "an accepted batch settles every ordinal it carried"
    );
}
