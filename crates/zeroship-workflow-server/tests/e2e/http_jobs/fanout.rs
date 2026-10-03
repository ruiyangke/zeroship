use super::*;
use zeroship_core::workflow_jobs::BroadcastId;
use zeroship_workflow_manager::Error as ManagerError;

fn fanout(fixture: &Fixture, broadcast: &BroadcastId, revision: i64) -> JobSpec {
    JobSpec {
        operation: JobOperation::Fanout {
            broadcast_id: broadcast.clone(),
            revision: revision.try_into().unwrap(),
        },
        ..fixture.job()
    }
}

/// A fanout job is queued without a deployment, run or hold, and a worker it is
/// delivered to can neither page it forward nor settle it with an outcome of its
/// own: the next page is the journal's to publish, and this journal committed
/// none.
#[ntex::test]
async fn fanout_delivery_preserves_scope_without_holds_and_publishes_nothing() {
    let fixture = Fixture::new().await;
    let broadcast = BroadcastId::mint();
    let job = fanout(&fixture, &broadcast, 1);
    fixture.submit(&job).await;
    fixture.submit(&job).await;
    assert_fanout_substitution_refused(&fixture, &job, &broadcast).await;
    let stored: Value = serde_json::from_str(&fixture.job_snapshot(&job).await[0]).unwrap();
    assert_eq!(stored["operation_kind"], "fanout");
    assert!(stored["deployment_id"].is_null() && stored["run_id"].is_null());
    let delivery = fixture.sweep(&job).await;
    let successor = fanout(&fixture, &broadcast, 2);
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

async fn assert_fanout_substitution_refused(
    fixture: &Fixture,
    job: &JobSpec,
    broadcast: &BroadcastId,
) {
    let before = fixture.job_snapshot(job).await;
    for operation in [
        fanout(fixture, &BroadcastId::mint(), 1).operation,
        fanout(fixture, broadcast, 2).operation,
    ] {
        let changed = JobSpec {
            operation,
            ..job.clone()
        };
        assert_eq!(
            fixture.queue.submit(&changed).await,
            Err(ManagerError::Conflict)
        );
        assert_eq!(fixture.job_snapshot(job).await, before);
    }
}

#[ntex::test]
async fn fanout_settlement_rejects_open_nested_metadata() {
    let fixture = Fixture::new().await;
    let broadcast = BroadcastId::mint();
    let job = fanout(&fixture, &broadcast, 1);
    fixture.submit(&job).await;
    let delivery = fixture.sweep(&job).await;
    let successor = fanout(&fixture, &broadcast, 2);
    let settled = committed(&delivery);
    let before = fixture.job_snapshot(&job).await;
    assert_invalid_fanout_fields(
        &fixture,
        endpoints::WORKFLOW_JOB_SETTLE,
        &settled,
        "/delivery/job/operation",
    )
    .await;
    assert_eq!(fixture.job_snapshot(&job).await, before);
    assert!(fixture.job_snapshot(&successor).await.is_empty());
    // The control: the unaltered settlement parses and is decided on what it
    // says, which is a delivery whose job the journal holds no receipt for.
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &settled).await,
        (StatusCode::CONFLICT, json!({"code":"conflict"}))
    );
}

async fn assert_invalid_fanout_fields(
    fixture: &Fixture,
    endpoint: ServiceEndpoint,
    wire: &Value,
    path: &str,
) {
    for (field, value) in [
        ("topic", json!("private")),
        ("cursor", json!("private")),
        ("body", json!({"private":true})),
        ("cutoff", json!(1)),
        ("deploymentId", json!(DeploymentId::mint())),
        ("broadcastId", json!(RunId::mint())),
        ("broadcastId", Value::Null),
        ("revision", json!(0)),
        ("revision", json!(-1)),
        ("revision", Value::Null),
    ] {
        let mut invalid = wire.clone();
        invalid.pointer_mut(path).unwrap()[field] = value;
        assert_eq!(
            fixture.post(endpoint, &invalid).await.0,
            StatusCode::BAD_REQUEST,
            "{field}: {invalid}"
        );
    }
}
