#![expect(
    clippy::future_not_send,
    reason = "management composition tests own compio-local journal transactions"
)]

use super::*;
use crate::service::{policy::PolicyAuthority, types::digest, AppWorkflows};
use futures::channel::oneshot;
use std::collections::BTreeMap;
use zeroship_core::workflow_jobs::JobOperation;
use zeroship_data_orm::Value;

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
            Box::pin($contract(Rc::new(store))).await;
        }

        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_management_outer_transaction_owns_commit_and_abort,
    postgres_management_outer_transaction_owns_commit_and_abort,
    outer_transaction
);
case!(
    sqlite_management_authority_loss_cannot_commit_a_refusal_or_lifecycle_change,
    postgres_management_authority_loss_cannot_commit_a_refusal_or_lifecycle_change,
    authority_loss
);
case!(
    sqlite_management_composition_retains_original_expiry_after_refresh,
    postgres_management_composition_retains_original_expiry_after_refresh,
    original_expiry
);
case!(
    sqlite_management_composition_rejects_foreign_authority_and_transaction,
    postgres_management_composition_rejects_foreign_authority_and_transaction,
    foreign_binding
);

type Snapshot = BTreeMap<&'static str, Vec<Value>>;

async fn snapshot(tx: &Transaction, app: &AppId) -> Snapshot {
    let mut state = BTreeMap::new();
    for table in [
        "runs",
        "generations",
        "job_publications",
        "outbox",
        "management_receipts",
        "requests",
    ] {
        state.insert(
            table,
            journal_rows(tx, table, json!({"app_id":app.as_str()}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    state
}

async fn persisted(service: &WorkflowService, app: &AppId) -> Snapshot {
    let tx = service.begin().await.unwrap();
    let state = snapshot(&tx, app).await;
    tx.commit().await.unwrap();
    state
}

fn attempt(scope: &AppWorkflows) -> (AppWorkflows, PolicyAuthority) {
    let authority = scope.capture_policy().authority().unwrap().clone();
    let retained = scope.clone().with_authority(authority.clone()).unwrap();
    (retained, authority)
}

async fn apply(
    scope: &AppWorkflows,
    tx: &mut Transaction,
    request: &ManageRun,
    authority: &PolicyAuthority,
) -> ManagementOutcome {
    scope
        .apply_management_authorized(tx, request, &digest(request).unwrap(), authority)
        .await
        .unwrap()
}

async fn assert_staged_restart(tx: &Transaction, request: &ManageRun) {
    let app = request.app_id.as_str();
    let run = request.run_id.as_str();
    let rows = journal_rows(tx, "runs", json!({"app_id":app, "id":run})).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].integer("generation").unwrap(), 1);
    assert_eq!(rows[0].text("state").unwrap(), "queued");
    assert_eq!(
        journal_count(
            tx,
            "generations",
            json!({"app_id":app, "run_id":run, "generation":1, "state":"queued"}),
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(
            tx,
            "management_receipts",
            json!({"app_id":app, "request_id":request.request_id.as_str()}),
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(
            tx,
            "outbox",
            json!({"app_id":app, "kind":"workflow.restart"}),
        )
        .await,
        1
    );
    let publications = journal_rows(
        tx,
        "job_publications",
        json!({"app_id":app, "run_id":run, "generation":1}),
    )
    .await;
    assert_eq!(publications.len(), 1);
    let published: zeroship_core::workflow_jobs::JobSpec =
        serde_json::from_str(&publications[0].text("specification").unwrap()).unwrap();
    assert_eq!(published.app_id, request.app_id);
    assert!(matches!(
        published.operation,
        JobOperation::Advance { run_id, generation:1, .. } if run_id == request.run_id
    ));
}

async fn outer_transaction(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let run = start(&service, &app).await;
    let scope = service.fixture_app(app.clone());
    let request = restart(&app, &run);
    let before = persisted(&service, &app).await;
    assert!(!before["runs"].is_empty());
    assert!(!before["job_publications"].is_empty());

    let (retained, authority) = attempt(&scope);
    let mut tx = retained.service.begin().await.unwrap();
    app::lock_app(&mut tx, &app).await.unwrap();
    assert_eq!(
        apply(&retained, &mut tx, &request, &authority).await,
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_staged_restart(&tx, &request).await;
    assert_ne!(snapshot(&tx, &app).await, before);
    // A delivered caller can still abandon its transaction before recording
    // its queue receipt; the lifecycle helper has not committed anything.
    drop(tx);
    assert_eq!(persisted(&service, &app).await, before);

    let (retained, authority) = attempt(&scope);
    let mut tx = retained.service.begin().await.unwrap();
    app::lock_app(&mut tx, &app).await.unwrap();
    let outcome = apply(&retained, &mut tx, &request, &authority).await;
    assert_staged_restart(&tx, &request).await;
    let staged = snapshot(&tx, &app).await;
    authority.check().unwrap();
    tx.commit().await.unwrap();
    assert_eq!(persisted(&service, &app).await, staged);
    assert_eq!(scope.apply_management(&request).await.unwrap(), outcome);
    assert_eq!(persisted(&service, &app).await, staged);
    let pending = scope.pending_jobs(None, 10).await.unwrap();
    assert!(pending.iter().any(|job| matches!(
        &job.operation,
        JobOperation::Advance { run_id, generation:1, .. } if run_id == &request.run_id
    )));
}

async fn authority_loss(store: Rc<OrmStore>) {
    for after_application in [false, true] {
        let (service, app, _, _deployments) = registered_service(store.clone()).await;
        let run = start(&service, &app).await;
        let scope = service.fixture_app(app.clone());
        let request = restart(&app, &run);
        let before = persisted(&service, &app).await;
        let (retained, authority) = attempt(&scope);
        let mut tx = retained.service.begin().await.unwrap();
        app::lock_app(&mut tx, &app).await.unwrap();
        if after_application {
            assert_eq!(
                apply(&retained, &mut tx, &request, &authority).await,
                ManagementOutcome::Applied {
                    state: RunState::Queued
                }
            );
            assert_staged_restart(&tx, &request).await;
        }
        service
            .policies
            .fixture_install(
                &app,
                PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                    .unwrap(),
            )
            .unwrap();
        assert!(authority.check().is_err());
        if !after_application {
            assert!(matches!(
                retained
                    .apply_management_authorized(
                        &mut tx,
                        &request,
                        &digest(&request).unwrap(),
                        &authority,
                    )
                    .await,
                Err(WorkflowServiceError::Unavailable(_))
            ));
            assert_eq!(snapshot(&tx, &app).await, before);
        }
        assert!(matches!(
            tx.commit().await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
        assert_eq!(persisted(&service, &app).await, before);
        assert_eq!(receipt_count(&service, &request).await, 0);
        service
            .policies
            .fixture_install(&app, leased_policy(3, AppPolicy::default()))
            .unwrap();
        assert_eq!(
            scope.apply_management(&request).await.unwrap(),
            ManagementOutcome::Applied {
                state: RunState::Queued
            }
        );
        assert_eq!(head(&service, &app, &run).await, (1, "queued".into()));
        assert_eq!(receipt_count(&service, &request).await, 1);
    }
}

async fn original_expiry(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let run = start(&service, &app).await;
    let scope = service.fixture_app(app.clone());
    let request = restart(&app, &run);
    let before = persisted(&service, &app).await;
    let expires = Instant::now() + Duration::from_secs(5);
    service
        .policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), expires).unwrap(),
        )
        .unwrap();
    let (retained, authority) = attempt(&scope);
    service
        .policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                expires + Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap();
    let (entered, staged) = oneshot::channel();
    let (release, paused) = oneshot::channel();
    let mut operation = authority.run(async {
        let mut tx = retained.service.begin().await?;
        app::lock_app(&mut tx, &app).await?;
        let outcome = retained
            .apply_management_authorized(&mut tx, &request, &digest(&request).unwrap(), &authority)
            .await?;
        assert_staged_restart(&tx, &request).await;
        entered.send(()).unwrap();
        paused.await.unwrap();
        tx.commit().await?;
        Ok(outcome)
    });
    assert!(matches!(
        select(staged, operation.as_mut()).await,
        Either::Left((Ok(()), _))
    ));
    assert!(matches!(
        operation.await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(
        release.send(()).is_err(),
        "original expiry must cancel outer receipt bookkeeping"
    );
    assert!(authority.check().is_err());
    scope.capture_policy().authority().unwrap().check().unwrap();
    assert_eq!(persisted(&service, &app).await, before);
    assert_eq!(receipt_count(&service, &request).await, 0);
    assert_eq!(
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(head(&service, &app, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);
}

async fn foreign_binding(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store).await;
    let run = start(&service, &app).await;
    start(&service, &foreign).await;
    let scope = service.fixture_app(app.clone());
    let foreign_scope = service.fixture_app(foreign.clone());
    let request = restart(&app, &run);
    let before = persisted(&service, &app).await;
    let foreign_before = persisted(&service, &foreign).await;
    for foreign_transaction in [false, true] {
        let (retained, authority) = attempt(&scope);
        let (foreign_retained, foreign_authority) = attempt(&foreign_scope);
        let owner = if foreign_transaction {
            &foreign_retained
        } else {
            &retained
        };
        let supplied_authority = if foreign_transaction {
            &authority
        } else {
            &foreign_authority
        };
        let mut tx = owner.service.begin().await.unwrap();
        app::lock_app(&mut tx, owner.app_id()).await.unwrap();
        assert!(matches!(
            retained
                .apply_management_authorized(
                    &mut tx,
                    &request,
                    &digest(&request).unwrap(),
                    supplied_authority,
                )
                .await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
        drop(tx);
        assert_eq!(persisted(&service, &app).await, before);
        assert_eq!(persisted(&service, &foreign).await, foreign_before);
    }
    assert_eq!(
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
    assert_eq!(persisted(&service, &foreign).await, foreign_before);
}
