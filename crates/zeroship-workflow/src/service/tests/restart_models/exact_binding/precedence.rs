use super::*;
use crate::{
    operations::{RestartDeploy, RestartTarget},
    service::WorkerIdentity,
};
use zeroship_data_orm::value;

async fn check(
    fixture: &Fixture,
    run: &str,
    options: &RestartOptions,
    policy: &AppPolicy,
) -> Result<(), WorkflowServiceError> {
    let (scope, authority) = attempt(&fixture.scope());
    authority
        .run(async {
            let mut tx = scope.service.begin().await?;
            app::lock_app(&mut tx, &fixture.owner).await?;
            let now = tx.now().await?;
            let plan =
                ready(restart::prepare(&mut tx, &fixture.owner, run, options, policy, now).await?)?;
            drop(plan);
            drop(tx);
            Ok(())
        })
        .await
}

fn partial(deploy: RestartDeploy) -> RestartOptions {
    RestartOptions {
        from: Some(RestartTarget {
            name: "missing".into(),
            occurrence: None,
        }),
        deploy: Some(deploy),
    }
}

pub(super) async fn lifecycle(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let worker = WorkerIdentity::new("restart-precedence".into()).unwrap();
    let task = fixture
        .scope()
        .service
        .poll(&worker)
        .await
        .unwrap()
        .unwrap();
    let deployment = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.replacement.id}),
        )
        .await;
    fixture
        .patch(
            "deploys",
            &fixture.replacement.id,
            value!({"state":"unavailable"}),
        )
        .await;
    let before = fixture.snapshot().await;
    let policy = AppPolicy::default();
    let denied = AppPolicy {
        admission: false,
        ..policy.clone()
    };
    assert!(matches!(
        check(
            &fixture,
            "missing",
            &partial(RestartDeploy::Latest),
            &denied
        )
        .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(matches!(
        check(
            &fixture,
            "missing",
            &partial(RestartDeploy::Latest),
            &policy
        )
        .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(matches!(
        check(&fixture, "missing", &RestartOptions::default(), &policy).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(
        matches!(check(&fixture, &fixture.run, &partial(RestartDeploy::Started), &policy).await,
        Err(WorkflowServiceError::InvalidRequest(message)) if message.contains("missing or ambiguous"))
    );
    assert!(
        matches!(check(&fixture, &fixture.run, &RestartOptions::default(), &policy).await,
        Err(WorkflowServiceError::Conflict(message)) if message.contains("execution lease is live"))
    );
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("deploys", deployment.0).await;
    fixture
        .scope()
        .service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    capacity_before_target(&fixture).await;
    source_before_target(&fixture).await;
}

async fn capacity_before_target(fixture: &Fixture) {
    fixture
        .scope()
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let deployment = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.replacement.id}),
        )
        .await;
    fixture
        .patch(
            "deploys",
            &fixture.replacement.id,
            value!({"state":"unavailable"}),
        )
        .await;
    let before = fixture.snapshot().await;
    let policy = AppPolicy {
        max_live_runs: 1,
        ..AppPolicy::default()
    };
    assert!(matches!(
        check(
            fixture,
            &fixture.run,
            &partial(RestartDeploy::Started),
            &policy
        )
        .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert!(
        matches!(check(fixture, &fixture.run, &RestartOptions::default(), &policy).await,
        Err(WorkflowServiceError::ResourceExhausted(message)) if message.contains("live-run limit"))
    );
    assert!(matches!(
        check(
            fixture,
            &fixture.run,
            &RestartOptions::default(),
            &AppPolicy::default()
        )
        .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("deploys", deployment.0).await;
}

async fn source_before_target(fixture: &Fixture) {
    let generation = fixture
        .row(
            "generations",
            json!({"app_id":fixture.owner.as_str(),"run_id":fixture.run,"generation":0}),
        )
        .await;
    let deployment = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.replacement.id}),
        )
        .await;
    let mut missing = fixture.replacement.clone();
    missing.workflows.remove("Example");
    fixture
        .patch(
            "deploys",
            &fixture.replacement.id,
            value!({"manifest":serde_json::to_string(&missing).unwrap()}),
        )
        .await;
    fixture
        .patch(
            "generations",
            &generation.text("id").unwrap(),
            value!({"deploy_id":fixture.replacement.id}),
        )
        .await;
    let before = fixture.snapshot().await;
    assert!(matches!(
        check(
            fixture,
            &fixture.run,
            &RestartOptions::default(),
            &AppPolicy::default()
        )
        .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("generations", generation.0).await;
    let before = fixture.snapshot().await;
    assert!(
        matches!(check(fixture, &fixture.run, &RestartOptions::default(), &AppPolicy::default()).await,
        Err(WorkflowServiceError::Conflict(message)) if message.contains("workflow is absent"))
    );
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("deploys", deployment.0).await;
    fixture.apply_exact(&fixture.original).await.unwrap();
    fixture.assert_generation(1, &fixture.original.id).await;
}

pub(super) async fn counters(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let head = fixture
        .row(
            "runs",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.run}),
        )
        .await;
    let generation = fixture
        .row(
            "generations",
            json!({"app_id":fixture.owner.as_str(),"run_id":fixture.run,"generation":0}),
        )
        .await;
    let deployment = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.replacement.id}),
        )
        .await;
    fixture
        .patch(
            "runs",
            &fixture.run,
            value!({"generation":i64::MAX,"signal_epoch":i64::MAX}),
        )
        .await;
    fixture
        .patch(
            "generations",
            &generation.text("id").unwrap(),
            value!({"generation":i64::MAX}),
        )
        .await;
    target_before_overflow(&fixture, &deployment.0).await;
    let before = fixture.snapshot().await;
    assert!(
        matches!(check(&fixture, &fixture.run, &RestartOptions::default(), &AppPolicy::default()).await,
        Err(WorkflowServiceError::ResourceExhausted(message)) if message.contains("generation exhausted"))
    );
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("generations", generation.0).await;
    fixture
        .patch("runs", &fixture.run, value!({"generation":0}))
        .await;
    target_before_overflow(&fixture, &deployment.0).await;
    let before = fixture.snapshot().await;
    assert!(
        matches!(check(&fixture, &fixture.run, &RestartOptions::default(), &AppPolicy::default()).await,
        Err(WorkflowServiceError::ResourceExhausted(message)) if message.contains("signal epoch exhausted"))
    );
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("runs", head.0).await;
    fixture.apply_exact(&fixture.original).await.unwrap();
    fixture.assert_generation(1, &fixture.original.id).await;
}

async fn target_before_overflow(fixture: &Fixture, deployment: &zeroship_data_orm::Value) {
    fixture
        .patch(
            "deploys",
            &fixture.replacement.id,
            value!({"state":"unavailable"}),
        )
        .await;
    let before = fixture.snapshot().await;
    assert!(matches!(
        check(
            fixture,
            &fixture.run,
            &RestartOptions::default(),
            &AppPolicy::default()
        )
        .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("deploys", deployment.clone()).await;
    let mut missing = fixture.replacement.clone();
    missing.workflows.remove("Example");
    fixture
        .patch(
            "deploys",
            &fixture.replacement.id,
            value!({"manifest":serde_json::to_string(&missing).unwrap()}),
        )
        .await;
    let before = fixture.snapshot().await;
    assert!(
        matches!(check(fixture, &fixture.run, &RestartOptions::default(), &AppPolicy::default()).await,
        Err(WorkflowServiceError::Conflict(message)) if message.contains("workflow is absent"))
    );
    assert_eq!(fixture.snapshot().await, before);
    fixture.restore("deploys", deployment.clone()).await;
}
