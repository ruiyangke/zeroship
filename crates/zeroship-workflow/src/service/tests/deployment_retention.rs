#![expect(
    clippy::future_not_send,
    reason = "retention tests own compio-local journals and clients"
)]

use super::*;
use crate::deployment_holds::{
    DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldScope, HoldState,
};
use crate::operations::{RestartDeploy, RestartOptions, RunOperation};
use std::{cell::Cell, time::Duration};

struct LostReplies {
    inner: deployment_fixture::OwnedClient,
    acquire: Cell<bool>,
    release: Cell<bool>,
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for LostReplies {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let receipt = self.inner.acquire(deployment, generation).await?;
        if self.acquire.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost acquire reply".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let receipt = self.inner.release(deployment, generation).await?;
        if self.release.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost release reply".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
}

#[compio::test]
async fn sqlite_deployment_intents_recover_lost_replies_and_close_admission() {
    let dir = tempfile::tempdir().unwrap();
    recovery_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_deployment_intents_recover_lost_replies_and_close_admission() {
    let fixture = PostgresFixture::start().await;
    recovery_contract(Rc::new(fixture.store.clone())).await;
}

async fn recovery_contract(store: Rc<OrmStore>) {
    let (service, app, other, platform) = registered_service(store.clone()).await;
    let deploy = platform.deploy(&app).await;
    let client = LostReplies {
        inner: platform.client(&app),
        acquire: Cell::new(true),
        release: Cell::new(true),
    };
    assert!(matches!(
        service
            .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    platform.assert_held(&app, &deploy.id).await;
    assert_eq!(
        service
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap(),
        std::slice::from_ref(&deploy.id)
    );
    assert!(service
        .pending_deployment_holds(&other, None, 1)
        .await
        .unwrap()
        .is_empty());
    assert!(service
        .pending_deployment_holds(&app, Some(&deploy.id), 1)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        service
            .acquire_deployment_hold(&other, &deploy.id, &deploy.hash, &client)
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let foreign_holder =
        platform.client_for_scope(HoldScope::new(app.clone(), typed_id::generate("dhl")).unwrap());
    assert!(matches!(
        service
            .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &foreign_holder)
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(service
        .acquire_deployment_hold(&app, &deploy.id, &"c".repeat(64), &client)
        .await
        .is_err());
    let unbound = service.clone().with_deployments(
        super::super::AppDeployments::new(platform.source.clone(), 1024 * 1024).unwrap(),
    );
    assert!(matches!(
        unbound.activate_deploy(&app, &deploy).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let reopened = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap()
        .with_deployments(service.deployments.clone().unwrap());
    let held = reopened
        .reconcile_deployment_hold(&app, &deploy.id, &client)
        .await
        .unwrap();
    assert_eq!(held.state, HoldState::Held);
    assert_eq!(held.generation.get(), 1);
    assert!(reopened
        .pending_deployment_holds(&app, None, 1)
        .await
        .unwrap()
        .is_empty());
    let foreign_run = reopened
        .fixture_app(other.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    // Scoped foreign keys reject assigning another app's deployment even when
    // a caller knows its globally unique ID.
    for table in ["runs", "generations"] {
        let tx = reopened.begin().await.unwrap();
        let key = if table == "runs" { "id" } else { "run_id" };
        assert!(tx
            .database()
            .collection(&format!("__zeroship_workflow_{table}"))
            .unwrap()
            .update(
                json!({"app_id":other.as_str(), key:foreign_run.id}).into(),
                json!({"deploy_id":deploy.id}).into()
            )
            .await
            .is_err());
    }
    assert!(matches!(
        reopened
            .release_deployment_hold(&app, &deploy.id, &client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        reopened
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap(),
        std::slice::from_ref(&deploy.id)
    );
    assert!(reopened
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
        .await
        .is_err());
    assert!(reopened.activate_deploy(&app, &deploy).await.is_err());
    let released = reopened
        .reconcile_deployment_hold(&app, &deploy.id, &client)
        .await
        .unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(
        reopened
            .release_deployment_hold(&app, &deploy.id, &client)
            .await
            .unwrap(),
        released
    );

    reopened.activate_deploy(&app, &deploy).await.unwrap();
    let reacquired = reopened
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
        .await
        .unwrap();
    assert_eq!(reacquired.generation.get(), 2);
    assert!(client
        .inner
        .release(&deploy.id, held.generation)
        .await
        .is_err());
    platform.assert_held(&app, &deploy.id).await;
    reopened.activate_deploy(&app, &deploy).await.unwrap();
    assert!(
        reopened
            .release_deployment_hold(&app, &deploy.id, &client)
            .await
            .is_err(),
        "active deployment cannot be released"
    );
    let replacement = platform.deploy(&app).await;
    reopened.activate_deploy(&app, &replacement).await.unwrap();
    reopened
        .release_deployment_hold(&app, &deploy.id, &client)
        .await
        .unwrap();

    reopened
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
        .await
        .unwrap();
    reopened.activate_deploy(&app, &deploy).await.unwrap();
}

#[compio::test]
async fn sqlite_retained_generations_keep_their_deployment_hold() {
    let dir = tempfile::tempdir().unwrap();
    dependencies_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_retained_generations_keep_their_deployment_hold() {
    let fixture = PostgresFixture::start().await;
    dependencies_contract(Rc::new(fixture.store.clone())).await;
}
async fn dependencies_contract(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let first = platform.deploy(&app).await;
    let second = platform.deploy(&app).await;
    let client = platform.client(&app);
    service
        .acquire_deployment_hold(&app, &first.id, &first.hash, &client)
        .await
        .unwrap();
    service.activate_deploy(&app, &first).await.unwrap();
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service.activate_deploy(&app, &second).await.unwrap();
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    scope
        .transition(&RequestId::mint(), &run.id, RunOperation::Cancel)
        .await
        .unwrap();
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    scope
        .restart(
            &RequestId::mint(),
            &run.id,
            RestartOptions {
                from: None,
                deploy: Some(RestartDeploy::Latest),
            },
        )
        .await
        .unwrap();
    // The run head moved, but its retained previous generation still needs code.
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    platform.assert_held(&app, &first.id).await;
    assert!(service
        .pending_deployment_holds(&app, None, 1)
        .await
        .unwrap()
        .is_empty());
    // Failed release rolled back its admission fence.
    service.activate_deploy(&app, &first).await.unwrap();
}

struct GatedReply<C> {
    inner: C,
    ready: flume::Sender<()>,
    resume: flume::Receiver<()>,
}
impl<C> GatedReply<C> {
    async fn deliver(&self, receipt: HoldReceipt) -> Result<HoldReceipt, WorkflowServiceError> {
        self.ready.send_async(()).await.unwrap();
        self.resume.recv_async().await.unwrap();
        Ok(receipt)
    }
}
#[async_trait::async_trait(?Send)]
impl<C: DeploymentHoldClient> DeploymentHoldClient for GatedReply<C> {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.deliver(self.inner.acquire(deployment, generation).await?)
            .await
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.deliver(self.inner.release(deployment, generation).await?)
            .await
    }
}

#[compio::test]
async fn sqlite_old_acknowledgements_cannot_reopen_reacquired_holds() {
    let dir = tempfile::tempdir().unwrap();
    stale_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_old_acknowledgements_cannot_reopen_reacquired_holds() {
    let fixture = PostgresFixture::start().await;
    stale_contract(Rc::new(fixture.store.clone())).await;
}
async fn stale_contract(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = platform.client(&app);
    for (release, resolve_hash) in [(false, false), (false, true), (true, false)] {
        let deployment = platform.deploy(&app).await;
        if release {
            service
                .acquire_deployment_hold(&app, &deployment.id, &deployment.hash, &client)
                .await
                .unwrap();
        }
        let (ready, reached) = flume::bounded(1);
        let (resume, resumed) = flume::bounded(1);
        let gated = GatedReply {
            inner: client.clone(),
            ready,
            resume: resumed,
        };
        let running = service.clone();
        let running_app = app.clone();
        let deploy = deployment.clone();
        let attempt = compio::runtime::spawn(async move {
            if release {
                running
                    .release_deployment_hold(&running_app, &deploy.id, &gated)
                    .await
            } else if resolve_hash {
                running
                    .acquire_deployment_hold_checked(
                        &running_app,
                        &deploy.id,
                        None,
                        &gated,
                        &|| Ok(()),
                    )
                    .await
            } else {
                running
                    .acquire_deployment_hold(&running_app, &deploy.id, &deploy.hash, &gated)
                    .await
            }
        });
        compio::time::timeout(Duration::from_secs(10), reached.recv_async())
            .await
            .unwrap()
            .unwrap();
        // A different journal operation completes while the platform reply is
        // blocked, proving the outbound call has released the app transaction.
        compio::time::timeout(
            Duration::from_secs(10),
            service.pending_deployment_holds(&app, None, 1),
        )
        .await
        .unwrap()
        .unwrap();
        service
            .reconcile_deployment_hold(&app, &deployment.id, &client)
            .await
            .unwrap();
        if !release {
            service
                .release_deployment_hold(&app, &deployment.id, &client)
                .await
                .unwrap();
        }
        service
            .acquire_deployment_hold(&app, &deployment.id, &deployment.hash, &client)
            .await
            .unwrap();
        resume.send_async(()).await.unwrap();
        assert!(matches!(
            attempt.await.unwrap(),
            Err(WorkflowServiceError::Conflict(_))
        ));
        platform.assert_held(&app, &deployment.id).await;
        assert!(service
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap()
            .is_empty());
    }
    let deployment = platform.deploy(&app).await;
    service
        .acquire_deployment_hold(&app, &deployment.id, &deployment.hash, &client)
        .await
        .unwrap();
    service
        .release_deployment_hold(&app, &deployment.id, &client)
        .await
        .unwrap();
    let (ready, reached) = flume::bounded(1);
    let (_resume, resumed) = flume::bounded(1);
    let gated = GatedReply {
        inner: client.clone(),
        ready,
        resume: resumed,
    };
    let running = service.clone();
    let running_app = app.clone();
    let deploy = deployment.clone();
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let attempt = compio::runtime::spawn(futures::future::Abortable::new(
        async move {
            running
                .acquire_deployment_hold(&running_app, &deploy.id, &deploy.hash, &gated)
                .await
        },
        registration,
    ));
    compio::time::timeout(Duration::from_secs(10), reached.recv_async())
        .await
        .unwrap()
        .unwrap();
    abort.abort();
    assert!(attempt.await.unwrap().is_err());
    platform.assert_held(&app, &deployment.id).await;
    let reopened = WorkflowService::open(service.store.clone(), service.policies.clone())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap(),
        std::slice::from_ref(&deployment.id)
    );
    reopened
        .reconcile_deployment_hold(&app, &deployment.id, &client)
        .await
        .unwrap();
}

#[derive(Clone)]
struct MismatchedReceipt {
    inner: deployment_fixture::OwnedClient,
    corrupt: fn(&mut HoldReceipt),
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for MismatchedReceipt {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let mut receipt = self.inner.acquire(deployment, generation).await?;
        (self.corrupt)(&mut receipt);
        Ok(receipt)
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.inner.release(deployment, generation).await
    }
}

#[compio::test]
async fn sqlite_receipts_require_complete_intent_identity() {
    let dir = tempfile::tempdir().unwrap();
    receipt_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_receipts_require_complete_intent_identity() {
    let fixture = PostgresFixture::start().await;
    receipt_contract(Rc::new(fixture.store.clone())).await;
}
async fn receipt_contract(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = platform.client(&app);
    let corruptions: [fn(&mut HoldReceipt); 6] = [
        |r| r.app_id = AppId::mint(),
        |r| r.deploy_id = typed_id::generate("dep"),
        |r| r.holder_id = typed_id::generate("dhl"),
        |r| r.generation = r.generation.next().unwrap(),
        |r| r.deploy_hash = "f".repeat(64),
        |r| r.state = HoldState::Released,
    ];
    for corrupt in corruptions {
        let deploy = platform.deploy(&app).await;
        let bad = MismatchedReceipt {
            inner: client.clone(),
            corrupt,
        };
        assert!(matches!(
            service
                .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &bad)
                .await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        assert_eq!(
            service
                .pending_deployment_holds(&app, None, 1)
                .await
                .unwrap(),
            std::slice::from_ref(&deploy.id)
        );
        let bad_host = service.clone().with_deployments(
            platform
                .binding(&[&app])
                .with_hold_client(Rc::new(bad.clone())),
        );
        assert!(bad_host.activate_deploy(&app, &deploy).await.is_err());
        service
            .reconcile_deployment_hold(&app, &deploy.id, &client)
            .await
            .unwrap();
        service
            .release_deployment_hold(&app, &deploy.id, &client)
            .await
            .unwrap();
    }
}

struct NoPlatformIo(HoldScope);

#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for NoPlatformIo {
    fn scope(&self) -> &HoldScope {
        &self.0
    }

    async fn acquire(
        &self,
        _deployment: &str,
        _generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        panic!("journal validation must finish without a platform acquisition")
    }

    async fn release(
        &self,
        _deployment: &str,
        _generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        panic!("journal validation must finish without a platform release")
    }
}

#[compio::test]
async fn sqlite_only_acquiring_deployment_holds_allow_unresolved_hashes() {
    let dir = tempfile::tempdir().unwrap();
    unresolved_hash_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_only_acquiring_deployment_holds_allow_unresolved_hashes() {
    let fixture = PostgresFixture::start().await;
    unresolved_hash_contract(Rc::new(fixture.store.clone())).await;
}

async fn unresolved_hash_contract(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = platform.deploy(&app).await;
    let client = platform.client(&app);
    let lost = LostReplies {
        inner: client.clone(),
        acquire: Cell::new(true),
        release: Cell::new(false),
    };
    assert!(matches!(
        service
            .acquire_deployment_hold_checked(&app, &deployment.id, None, &lost, &|| Ok(()))
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let no_io = NoPlatformIo(client.scope().clone());
    let filter = json!({"app_id":app.as_str(), "deploy_id":deployment.id});
    for state in ["held", "releasing", "released"] {
        let tx = service.begin().await.unwrap();
        journal_update(
            &tx,
            "deployment_holds",
            filter.clone(),
            json!({"state":state, "deploy_hash":null}),
        )
        .await;
        tx.commit().await.unwrap();

        assert!(matches!(
            service
                .acquire_deployment_hold_checked(&app, &deployment.id, None, &no_io, &|| Ok(()))
                .await,
            Err(WorkflowServiceError::Internal(_))
        ));
        assert!(matches!(
            service
                .reconcile_deployment_hold(&app, &deployment.id, &no_io)
                .await,
            Err(WorkflowServiceError::Internal(_))
        ));
        assert!(matches!(
            service
                .release_deployment_hold(&app, &deployment.id, &no_io)
                .await,
            Err(WorkflowServiceError::Internal(_))
        ));
        let tx = service.begin().await.unwrap();
        assert!(matches!(
            crate::service::deployment_retention::admission_generation(
                &tx,
                &app,
                &deployment.id,
                &deployment.hash,
                client.scope(),
            )
            .await,
            Err(WorkflowServiceError::Internal(_))
        ));
        let rows = journal_rows(&tx, "deployment_holds", filter.clone()).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("state").unwrap(), state);
        assert_eq!(rows[0].optional_text("deploy_hash").unwrap(), None);
        tx.commit().await.unwrap();
        platform.assert_held(&app, &deployment.id).await;
    }

    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "deployment_holds",
        filter,
        json!({"state":"acquiring"}),
    )
    .await;
    tx.commit().await.unwrap();
    let held = service
        .reconcile_deployment_hold(&app, &deployment.id, &client)
        .await
        .unwrap();
    assert_eq!(held.state, HoldState::Held);
    assert_eq!(held.deploy_hash, deployment.hash);
    assert_eq!(
        service
            .acquire_deployment_hold_checked(&app, &deployment.id, None, &no_io, &|| Ok(()))
            .await
            .unwrap(),
        held
    );
}

#[compio::test]
async fn sqlite_concurrent_hold_resolution_preserves_the_bound_hash() {
    let dir = tempfile::tempdir().unwrap();
    concurrent_resolution_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_concurrent_hold_resolution_preserves_the_bound_hash() {
    let fixture = PostgresFixture::start().await;
    concurrent_resolution_contract(Rc::new(fixture.store.clone())).await;
}

async fn concurrent_resolution_contract(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = platform.client(&app);
    for conflicting in [false, true] {
        let deployment = platform.deploy(&app).await;
        let (ready, reached) = flume::bounded(1);
        let (resume, resumed) = flume::bounded(1);
        let corrupt: fn(&mut HoldReceipt) = if conflicting {
            |receipt| {
                receipt.deploy_hash = if receipt.deploy_hash.starts_with('a') {
                    "b".repeat(64)
                } else {
                    "a".repeat(64)
                };
            }
        } else {
            |_| {}
        };
        let gated = GatedReply {
            inner: MismatchedReceipt {
                inner: client.clone(),
                corrupt,
            },
            ready,
            resume: resumed,
        };
        let running = service.clone();
        let running_app = app.clone();
        let deploy_id = deployment.id.clone();
        let attempt = compio::runtime::spawn(async move {
            running
                .acquire_deployment_hold_checked(&running_app, &deploy_id, None, &gated, &|| Ok(()))
                .await
        });
        compio::time::timeout(Duration::from_secs(10), reached.recv_async())
            .await
            .unwrap()
            .unwrap();
        let filter = json!({"app_id":app.as_str(), "deploy_id":deployment.id});
        let tx = service.begin().await.unwrap();
        let rows = journal_rows(&tx, "deployment_holds", filter.clone()).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("state").unwrap(), "acquiring");
        assert_eq!(rows[0].optional_text("deploy_hash").unwrap(), None);
        tx.commit().await.unwrap();

        let held = compio::time::timeout(
            Duration::from_secs(10),
            service.acquire_deployment_hold(&app, &deployment.id, &deployment.hash, &client),
        )
        .await
        .expect("pending platform reply must not retain the journal lock")
        .unwrap();
        resume.send_async(()).await.unwrap();
        let delayed = attempt.await.unwrap();
        if conflicting {
            assert!(matches!(delayed, Err(WorkflowServiceError::Conflict(_))));
        } else {
            assert_eq!(delayed.unwrap(), held);
        }
        let tx = service.begin().await.unwrap();
        let rows = journal_rows(&tx, "deployment_holds", filter).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("state").unwrap(), "held");
        assert_eq!(rows[0].text("deploy_hash").unwrap(), held.deploy_hash);
        assert_eq!(
            rows[0].integer("generation").unwrap(),
            held.generation.get()
        );
        assert_eq!(
            crate::service::deployment_retention::admission_generation(
                &tx,
                &app,
                &deployment.id,
                &deployment.hash,
                client.scope(),
            )
            .await
            .unwrap(),
            held.generation.get()
        );
        tx.commit().await.unwrap();
        platform.assert_held(&app, &deployment.id).await;
        let reopened = WorkflowService::open(service.store.clone(), service.policies.clone())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .acquire_deployment_hold_checked(
                    &app,
                    &deployment.id,
                    None,
                    &NoPlatformIo(client.scope().clone()),
                    &|| Ok(()),
                )
                .await
                .unwrap(),
            held
        );
    }
}
