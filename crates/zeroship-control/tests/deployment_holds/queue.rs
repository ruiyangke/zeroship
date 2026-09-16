use super::*;
use zeroship_core::{workflow_deployments::QueueHoldRequest, workflow_jobs::DeploymentId};
use zeroship_workflow_client::QueueDeploymentHolds;
use zeroship_workflow_manager::deployments::{self, DeploymentHolds};

#[ntex::test]
async fn queue_holds_work_without_workers_and_cannot_release_journal_holds() {
    let fixture = Fixture::new().await;
    let (app, deployment, hash) = fixture.deployment("queue-retention").await;
    // No coordinator is listening: queue authority must not depend on placement.
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let client = QueueDeploymentHolds::new(
        &origin(&control_server),
        fixture.workflow_role.clone(),
        Options::default(),
    )
    .unwrap();
    let mut request = QueueHoldRequest {
        app_id: app.clone(),
        deploy_id: DeploymentId::parse(&deployment).unwrap(),
        generation: generation(1),
    };
    let workers: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT COUNT(*)::bigint FROM zeroship.worker_instances",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(workers, 0);
    let http = Client::new().await;
    let reply = compio::time::timeout(
        Duration::from_secs(10),
        http.post(format!(
            "{}{}",
            origin(&control_server).trim_end_matches('/'),
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE.path_template()
        ))
        .header("authorization", control_header(&fixture.workflow_role))
        .send_json(&request),
    )
    .await
    .expect("lost reply request timed out")
    .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    drop(reply);
    let first = client.acquire(&request).await.unwrap();
    assert_eq!(first.deploy_hash, hash);
    assert_eq!(first.holder_id, HoldScope::for_queue(app.clone()).holder());
    assert_eq!(first.state, HoldState::Held);
    let rows = fixture.rows().await;
    assert_eq!(rows.len(), 1);
    let queue_storage_id = rows[0].0.clone();
    assert_eq!(client.clone().acquire(&request).await.unwrap(), first);

    let catalog_database = database(&fixture.control_url).await;
    let ledger = DeploymentHolds::new(catalog_database.clone()).unwrap();
    let journal = HoldScope::for_app(app.clone());
    let journal_receipt = ledger
        .acquire(&journal, &deployment, generation(1))
        .await
        .unwrap();
    assert_ne!(journal_receipt.holder_id, first.holder_id);
    let released = client.release(&request).await.unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(client.release(&request).await.unwrap(), released);
    assert_eq!(
        ledger
            .acquire(&journal, &deployment, generation(1))
            .await
            .unwrap(),
        journal_receipt
    );
    assert!(matches!(
        deployments::fence_reclamation(&catalog_database, &app, &deployment).await,
        Err(deployments::Error::Conflict(_))
    ));
    request.generation = generation(2);
    let reacquired = client.acquire(&request).await.unwrap();
    assert_eq!(reacquired.holder_id, first.holder_id);
    let stale = QueueHoldRequest {
        generation: generation(1),
        ..request.clone()
    };
    assert_eq!(
        client.release(&stale).await,
        Err(CoordinationError::Refused(FailureCode::Conflict))
    );
    assert_eq!(client.acquire(&request).await.unwrap(), reacquired);
    let rows = fixture.rows().await;
    let queue = rows.iter().find(|row| row.1 == first.holder_id).unwrap();
    assert_eq!(queue.0, queue_storage_id);
    assert_eq!(queue.2, request.generation.get());
    assert_eq!(queue.3, "held");
    assert_eq!(
        rows.iter().find(|row| row.1 == journal.holder()).unwrap().3,
        "held"
    );
    let workers: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT COUNT(*)::bigint FROM zeroship.worker_instances",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(workers, 0);
}

#[ntex::test]
async fn queue_hold_http_authenticates_before_decoding_and_closes_scope() {
    let fixture = Fixture::new().await;
    let (app, deployment, _) = fixture.deployment("queue-hold-auth").await;
    let (foreign_app, foreign_deployment, _) = fixture.deployment("queue-hold-foreign").await;
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let origin = origin(&control_server);
    let http = Client::new().await;
    let (worker, worker_auth) = fixture.joined_worker(&http, &origin).await;
    let workflow_instance = signer(
        ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/workflow/{}",
            worker.as_str()
        ))
        .unwrap(),
        ServiceSigningKey::generate(),
    );
    let wrong_key = signer(
        service_issuer(WORKFLOW_SERVICE_NAME).unwrap(),
        ServiceSigningKey::generate(),
    );
    for endpoint in [
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
    ] {
        for token in [
            None,
            Some(control_header(&fixture.worker_role)),
            Some(control_header(&worker_auth)),
            Some(control_header(&fixture.state.service_auth)),
            Some(control_header(&workflow_instance)),
            Some(control_header(&wrong_key)),
            Some(
                fixture
                    .workflow_role
                    .authorization_for(&ServiceIssuer::parse(AUDIENCE).unwrap())
                    .unwrap(),
            ),
        ] {
            let (status, failure) = post(
                &http,
                &origin,
                endpoint,
                token.as_deref(),
                &json!({"holderId":"forged"}),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(failure, json!({"code":"unauthenticated"}));
        }
    }
    for endpoint in [
        endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
        endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
    ] {
        let token = control_header(&fixture.workflow_role);
        let (status, failure) = post(
            &http,
            &origin,
            endpoint,
            Some(&token),
            &json!({"holderId":"forged"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(failure, json!({"code":"unauthenticated"}));
    }
    let request = QueueHoldRequest {
        app_id: app.clone(),
        deploy_id: DeploymentId::parse(&deployment).unwrap(),
        generation: generation(1),
    };
    for endpoint in [
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
    ] {
        for (field, value) in [
            ("holderId", json!(HoldScope::for_app(app.clone()).holder())),
            ("assignmentRevision", json!(1)),
            ("workerId", json!(worker)),
            ("generation", json!(0)),
            ("deployId", json!("dep_invalid")),
            ("input", json!({"private":"body"})),
        ] {
            let mut invalid = json!(request);
            invalid[field] = value;
            let token = control_header(&fixture.workflow_role);
            let (status, failure) = post(&http, &origin, endpoint, Some(&token), &invalid).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{field}");
            assert_eq!(failure, json!({"code":"invalid"}));
        }
        for mismatch in [
            QueueHoldRequest {
                app_id: foreign_app.clone(),
                ..request.clone()
            },
            QueueHoldRequest {
                deploy_id: DeploymentId::parse(&foreign_deployment).unwrap(),
                ..request.clone()
            },
        ] {
            let token = control_header(&fixture.workflow_role);
            let (status, failure) =
                post(&http, &origin, endpoint, Some(&token), &json!(mismatch)).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(failure, json!({"code":"denied"}));
        }
    }
    assert!(fixture.rows().await.is_empty());
    let token = control_header(&fixture.workflow_role);
    let (status, receipt) = post(
        &http,
        &origin,
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
        Some(&token),
        &json!(request),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["holderId"], HoldScope::for_queue(app).holder());
    let (status, failure) = post(
        &http,
        &origin,
        endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
        Some(&token),
        &json!(request),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(failure, json!({"code":"unauthenticated"}));
    assert_eq!(fixture.rows().await.len(), 1);
}
