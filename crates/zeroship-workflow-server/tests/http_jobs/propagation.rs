use super::*;
use zeroship_core::{workflow_coordination::ManagementOutcome, workflow_jobs::PropagationId};

fn page(fixture: &Fixture, obligation: &PropagationId, revision: i64) -> JobSpec {
    JobSpec {
        operation: JobOperation::Propagate {
            propagation_id: obligation.clone(),
            revision: revision.try_into().unwrap(),
        },
        ..fixture.job()
    }
}

#[ntex::test]
async fn propagation_delivery_and_receipts_preserve_scope_without_holds() {
    let mut fixture = Fixture::new().await;
    let obligation = PropagationId::mint();
    let job = page(&fixture, &obligation, 1);
    fixture.submit(&job).await;
    fixture.submit(&job).await;
    let before = fixture.job_snapshot(&job).await;
    for operation in [
        page(&fixture, &PropagationId::mint(), 1).operation,
        page(&fixture, &obligation, 2).operation,
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
        assert_eq!(fixture.job_snapshot(&job).await, before);
    }
    let delivery = fixture.claim(&job).await;
    let successor = page(&fixture, &obligation, 2);
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
    assert_eq!(stored["operation_kind"], "propagate");
    assert!(stored["deployment_id"].is_null() && stored["run_id"].is_null());
    assert_foreign_worker_denied(&fixture, &command).await;
    assert_receipt_replay(&mut fixture, &command, &successor, body).await;
}

#[ntex::test]
async fn propagation_http_rejects_customer_routing_state() {
    let fixture = Fixture::new().await;
    let obligation = PropagationId::mint();
    let job = page(&fixture, &obligation, 1);
    let submit = serde_json::to_value(SubmitJob {
        scope: fixture.scope(),
        job: job.clone(),
    })
    .unwrap();
    for (field, value) in [
        ("cursor", json!("private")),
        ("runId", json!(RunId::mint())),
        ("generation", json!(0)),
        ("parents", json!(["private"])),
        ("deploymentId", json!(DeploymentId::mint())),
        ("propagationId", json!(RunId::mint())),
        ("propagationId", Value::Null),
        ("revision", json!(0)),
        ("revision", Value::Null),
    ] {
        let mut invalid = submit.clone();
        invalid.pointer_mut("/job/operation").unwrap()[field] = value;
        assert_eq!(
            fixture
                .post(endpoints::WORKFLOW_JOB_SUBMIT, &invalid)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{field}: {invalid}"
        );
    }
    assert!(fixture.job_snapshot(&job).await.is_empty());
    fixture.submit(&job).await;
}
