use super::*;
use zeroship_core::{workflow_coordination::ManagementOutcome, workflow_jobs::BroadcastId};

fn fanout(fixture: &Fixture, broadcast: &BroadcastId, revision: i64) -> JobSpec {
    JobSpec {
        operation: JobOperation::Fanout {
            broadcast_id: broadcast.clone(),
            revision: revision.try_into().unwrap(),
        },
        ..fixture.job()
    }
}

#[ntex::test]
async fn fanout_delivery_and_receipts_preserve_scope_without_holds() {
    let mut fixture = Fixture::new().await;
    let broadcast = BroadcastId::mint();
    let job = fanout(&fixture, &broadcast, 1);
    fixture.submit(&job).await;
    fixture.submit(&job).await;
    assert_fanout_substitution_refused(&fixture, &job, &broadcast).await;
    let delivery = fixture.claim(&job).await;
    let successor = fanout(&fixture, &broadcast, 2);
    let mut command = settlement(&delivery, vec![successor.clone()]);
    command.outcome = JobOutcome::Management {
        outcome: ManagementOutcome::Denied {},
    };
    let before = fixture.job_snapshot(&job).await;
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &command)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.job_snapshot(&job).await, before);
    assert!(fixture.job_snapshot(&successor).await.is_empty());
    command.outcome = JobOutcome::Waiting {};
    let (status, body) = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &command).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], json!({"kind":"waiting"}));
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
    let stored: Value = serde_json::from_str(&fixture.job_snapshot(&successor).await[0]).unwrap();
    assert_eq!(stored["operation_kind"], "fanout");
    assert!(stored["deployment_id"].is_null() && stored["run_id"].is_null());
    assert_foreign_worker_denied(&fixture, &command).await;
    assert_receipt_replay(&mut fixture, &command, &successor, body).await;
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
        let changed = SubmitJob {
            scope: fixture.scope(),
            job: JobSpec {
                operation,
                ..job.clone()
            },
        };
        assert_eq!(
            fixture
                .post(endpoints::WORKFLOW_JOB_SUBMIT, &changed)
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(fixture.job_snapshot(job).await, before);
    }
    let foreign = SubmitJob {
        scope: fixture.scope(),
        job: JobSpec {
            app_id: AppId::mint(),
            ..job.clone()
        },
    };
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SUBMIT, &foreign)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(fixture.job_snapshot(&foreign.job).await.is_empty());
}

#[ntex::test]
async fn fanout_http_rejects_open_nested_metadata_and_invalid_identity() {
    let fixture = Fixture::new().await;
    let broadcast = BroadcastId::mint();
    let job = fanout(&fixture, &broadcast, 1);
    fixture.submit(&job).await;
    let delivery = fixture.claim(&job).await;
    let successor = fanout(&fixture, &broadcast, 2);
    let submit = serde_json::to_value(SubmitJob {
        scope: fixture.scope(),
        job: successor.clone(),
    })
    .unwrap();
    let mut command = settlement(&delivery, vec![successor.clone()]);
    command.outcome = JobOutcome::Waiting {};
    let settled = serde_json::to_value(&command).unwrap();
    let before = fixture.job_snapshot(&job).await;
    for (endpoint, wire, path) in [
        (endpoints::WORKFLOW_JOB_SUBMIT, &submit, "/job/operation"),
        (
            endpoints::WORKFLOW_JOB_SETTLE,
            &settled,
            "/delivery/job/operation",
        ),
        (
            endpoints::WORKFLOW_JOB_SETTLE,
            &settled,
            "/successors/0/operation",
        ),
    ] {
        assert_invalid_fanout_fields(&fixture, endpoint, wire, path).await;
    }
    assert_eq!(fixture.job_snapshot(&job).await, before);
    assert!(fixture.job_snapshot(&successor).await.is_empty());
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &command)
            .await
            .0,
        StatusCode::OK
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
