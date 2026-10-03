use super::*;
use zeroship_core::workflow_jobs::PropagationId;
use zeroship_workflow_manager::Error as ManagerError;

fn page(fixture: &Fixture, obligation: &PropagationId, revision: i64) -> JobSpec {
    JobSpec {
        operation: JobOperation::Propagate {
            propagation_id: obligation.clone(),
            revision: revision.try_into().unwrap(),
        },
        ..fixture.job()
    }
}

/// A propagation page is queued without a deployment, run or hold, and a worker
/// it is delivered to can neither page it forward nor settle it with an outcome
/// of its own: the next page is the journal's to publish, and this journal
/// committed none.
#[ntex::test]
async fn propagation_delivery_preserves_scope_without_holds_and_publishes_nothing() {
    let fixture = Fixture::new().await;
    let obligation = PropagationId::mint();
    let job = page(&fixture, &obligation, 1);
    fixture.submit(&job).await;
    fixture.submit(&job).await;
    let before = fixture.job_snapshot(&job).await;
    for operation in [
        page(&fixture, &PropagationId::mint(), 1).operation,
        page(&fixture, &obligation, 2).operation,
    ] {
        let changed = JobSpec {
            operation,
            ..job.clone()
        };
        assert_eq!(
            fixture.queue.submit(&changed).await,
            Err(ManagerError::Conflict)
        );
        assert_eq!(fixture.job_snapshot(&job).await, before);
    }
    let stored: Value = serde_json::from_str(&before[0]).unwrap();
    assert_eq!(stored["operation_kind"], "propagate");
    assert!(stored["deployment_id"].is_null() && stored["run_id"].is_null());
    let delivery = fixture.sweep(&job).await;
    let successor = page(&fixture, &obligation, 2);
    let before = fixture.job_snapshot(&job).await;
    let forged = json!({
        "delivery": delivery,
        "outcome": {"kind":"waiting"},
        "successors": [successor],
    });
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &forged).await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
            .await,
        (StatusCode::CONFLICT, json!({"code":"conflict"}))
    );
    assert_eq!(fixture.job_snapshot(&job).await, before);
    assert!(fixture.job_snapshot(&successor).await.is_empty());
    let held = fixture
        .platform
        .admin
        .query_one(
            "SELECT count(*) FROM workflow_manager.deployment_holds WHERE app_id=$1",
            &[&job.app_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(held.get::<_, i64>(0), 0);
    assert_foreign_worker_denied(&fixture, &delivery).await;
}
