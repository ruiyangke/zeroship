use super::*;
use crate::service::{app, capability::SignalTarget, models, SignalAuthority, SignalTokenRequest};
use futures::{stream::FuturesUnordered, StreamExt};
use std::time::Duration;
use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};
use zeroship_data_orm::{
    orm::{Entity, Output},
    value,
};

#[compio::test]
async fn sqlite_signal_revocation_preserves_foreign_and_other_target_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    revocation_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_signal_revocation_preserves_foreign_and_other_target_authority() {
    let fixture = PostgresFixture::start().await;
    revocation_contract(Rc::new(fixture.store.clone())).await;
}

async fn revocation_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let service = service.with_signal_authority(Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    ));
    let run_id = typed_id::new_workflow_run_id();
    let foreign_id = typed_id::new_workflow_run_id();
    let mut tx = service.begin().await.unwrap();
    for (app_id, run_id) in [(&local, &run_id), (&foreign, &foreign_id)] {
        app::lock_app(&mut tx, app_id).await.unwrap();
        let deploy = app::active_deploy(&mut tx, app_id).await.unwrap();
        let now = tx.now().await.unwrap();
        app::insert_root_run(
            &mut tx,
            app_id,
            &app::NewRun {
                id: run_id,
                name: "Example",
                deploy: &deploy.id,
                options: &StartOptions::default(),
                input_source: None,
                max_input_bytes: AppPolicy::default().max_input_bytes,
            },
            now,
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let run = SignalTarget::Run {
        run_id: run_id.clone(),
    };
    let foreign_run = SignalTarget::Run { run_id: foreign_id };
    let topic = SignalTarget::Topic {
        topic: "updates".into(),
    };
    let local_scope = service.fixture_app(local.clone());
    let message = SignalOptions {
        signal_type: "ready".into(),
        payload: json!("accepted"),
    };
    for revoked_target in [Some(run.clone()), Some(topic.clone()), None] {
        let mut issued = Vec::new();
        for (app_id, run_target) in [(&local, &run), (&foreign, &foreign_run)] {
            for target in [run_target, &topic] {
                let options = SignalTokenRequest {
                    target: target.clone(),
                    types: ["ready".into()].into(),
                    lifetime_seconds: 60,
                };
                let token = service
                    .fixture_app(app_id.clone())
                    .issue_signal_token(&RequestId::mint(), options.clone())
                    .await
                    .unwrap();
                issued.push((app_id, target, options, token));
            }
        }
        let request = RequestId::mint();
        let revoked = local_scope
            .revoke_signal_tokens(&request, revoked_target.clone())
            .await
            .unwrap();
        assert_eq!(revoked.epoch, 1);
        assert_eq!(
            local_scope
                .revoke_signal_tokens(&request, revoked_target.clone())
                .await
                .unwrap(),
            revoked
        );
        for (app_id, target, options, token) in issued {
            let other_app = if app_id == &local { &foreign } else { &local };
            assert!(matches!(
                service
                    .fixture_app((other_app).clone())
                    .ingest_signal(&RequestId::mint(), token.as_str(), target, message.clone())
                    .await,
                Err(WorkflowServiceError::NotFound(_)),
            ));
            let denied = app_id == &local
                && revoked_target
                    .as_ref()
                    .is_none_or(|revoked| revoked == target);
            let result = service
                .fixture_app((app_id).clone())
                .ingest_signal(&RequestId::mint(), token.as_str(), target, message.clone())
                .await;
            if denied {
                assert_eq!(result, Err(WorkflowServiceError::Unauthenticated));
                let replacement = local_scope
                    .issue_signal_token(&RequestId::mint(), options)
                    .await
                    .unwrap();
                service
                    .fixture_app((app_id).clone())
                    .ingest_signal(
                        &RequestId::mint(),
                        replacement.as_str(),
                        target,
                        message.clone(),
                    )
                    .await
                    .unwrap();
                // Issuing another token must not reset the persisted revocation epoch.
                assert_eq!(
                    service
                        .fixture_app((app_id).clone())
                        .ingest_signal(&RequestId::mint(), token.as_str(), target, message.clone())
                        .await,
                    Err(WorkflowServiceError::Unauthenticated)
                );
            } else {
                result.unwrap();
            }
        }
    }

    for target in [Some(run), Some(topic), None] {
        let mut tx = service.begin().await.unwrap();
        app::lock_app(&mut tx, &local).await.unwrap();
        let (table, filter) = match &target {
            Some(SignalTarget::Run { run_id }) => (
                models::runs::Entity::COLLECTION,
                value!({"app_id":local.as_str(), "id":run_id.clone()}),
            ),
            Some(SignalTarget::Topic { topic }) => (
                models::topics::Entity::COLLECTION,
                value!({"app_id":local.as_str(), "topic":topic.clone()}),
            ),
            None => (
                models::app_state::Entity::COLLECTION,
                value!({"app_id":local.as_str()}),
            ),
        };
        tx.database()
            .collection(table)
            .unwrap()
            .update(filter, value!({"signal_epoch":i64::MAX}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let request = RequestId::mint();
        assert!(matches!(
            local_scope.revoke_signal_tokens(&request, target).await,
            Err(WorkflowServiceError::ResourceExhausted(_))
        ));
        let tx = service.begin().await.unwrap();
        let receipts = tx
            .database()
            .collection(models::requests::Entity::COLLECTION)
            .unwrap()
            .count(
                value!({"app_id":local.as_str(), "request_id":request.as_str()}),
                value!({}),
            )
            .await
            .unwrap();
        assert!(matches!(receipts, Output::Count(0)));
        tx.commit().await.unwrap();
    }
}

/// Two independent hosts revoking one app's signal authority at the same time.
///
/// Each revocation reads the epoch it raises and names that reading in its own
/// write filter, so the host that commits second cannot store a successor of a
/// reading the first has already replaced. Both revocations land and every
/// token minted before them stays refused.
///
/// The contention is PostgreSQL's. A SQLite journal reserves its writer as a
/// transaction opens, so two hosts never hold one app's state row at once.
#[compio::test]
async fn postgres_concurrent_app_revocations_each_advance_the_signal_epoch() {
    let fixture = PostgresFixture::start().await;
    let worker_url = fixture
        .admin_url
        .replacen("postgres@", "customer_worker@", 1);
    let schema = || crate::service::store::SchemaName::new("customer").unwrap();
    let policies = Arc::new(HostPolicies::default());
    let authority = Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    );
    let mut hosts = Vec::new();
    for store in [
        fixture.store.clone(),
        orm_store(&worker_url, schema()).await,
        orm_store(&worker_url, schema()).await,
    ] {
        hosts.push(
            WorkflowService::open(Rc::new(store), policies.clone())
                .await
                .unwrap()
                .with_signal_authority(authority.clone()),
        );
    }
    let holder = hosts.pop().unwrap();
    let second = hosts.pop().unwrap();
    let first = hosts.pop().unwrap();

    let app = AppId::mint();
    first
        .fixture_register(&app, leased_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    let target = SignalTarget::Topic {
        topic: "updates".into(),
    };
    let token = first
        .fixture_app(app.clone())
        .issue_signal_token(
            &RequestId::mint(),
            SignalTokenRequest {
                target: target.clone(),
                types: ["ready".into()].into(),
                lifetime_seconds: 60,
            },
        )
        .await
        .unwrap();
    let opening = app_signal_epoch(&first, &app).await;

    // Hold the app's state row so both hosts reach their revocation write
    // before either of them commits one.
    let observer = connect(&fixture.admin_url).await;
    let mut blocker = holder.begin().await.unwrap();
    app::lock_app(&mut blocker, &app).await.unwrap();
    let pending = [&first, &second]
        .into_iter()
        .map(|service| {
            let scope = service.fixture_app(app.clone());
            async move {
                scope
                    .revoke_signal_tokens(&RequestId::mint(), None)
                    .await
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>();
    futures::pin_mut!(pending);
    let contended = async {
        loop {
            let waiting: i64 = observer
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity \
                     WHERE usename='customer_worker' AND wait_event_type='Lock'",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if waiting == 2 {
                return;
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    futures::pin_mut!(contended);
    // The journal's lock budget bounds how long the held row may block a host,
    // so the blocker releases as soon as both are demonstrably waiting on it.
    let pending = match compio::time::timeout(
        Duration::from_millis(u64::from(
            zeroship_data_orm::budgets::DB_LOCK_TIMEOUT_MS / 2,
        )),
        futures::future::select(pending, contended),
    )
    .await
    .expect("both revocations must reach the held app state row")
    {
        futures::future::Either::Left(_) => {
            panic!("a revocation settled before the app state row was released")
        }
        futures::future::Either::Right(((), pending)) => pending,
    };
    blocker.commit().await.unwrap();
    let mut epochs = compio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("both revocations must settle once the app state row is released")
        .into_iter()
        .map(|revoked| revoked.expect("a revocation refused its own epoch").epoch)
        .collect::<Vec<_>>();
    epochs.sort_unstable();
    assert_eq!(epochs, [opening + 1, opening + 2]);
    assert_eq!(app_signal_epoch(&first, &app).await, opening + 2);
    assert_eq!(
        second
            .fixture_app(app.clone())
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &target,
                SignalOptions {
                    signal_type: "ready".into(),
                    payload: json!("accepted"),
                },
            )
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );
}

async fn app_signal_epoch(service: &WorkflowService, app: &AppId) -> i64 {
    let tx = service.begin().await.unwrap();
    let epoch = tx
        .database()
        .entity::<models::app_state::Entity>()
        .unwrap()
        .find::<models::AppSignalEpoch>(
            models::app_state::app_id.eq(app.as_str()).unwrap(),
            zeroship_data_orm::orm::FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the registered app keeps a state row")
        .signal_epoch;
    tx.commit().await.unwrap();
    epoch
}
