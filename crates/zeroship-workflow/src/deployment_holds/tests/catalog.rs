use super::*;

fn manifest() -> (String, String) {
    let mut manifest = zeroship_bundle::Manifest::default();
    let hash =
        zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    manifest.deploy_hash = Some(hash.clone());
    (hash, serde_json::to_string(&manifest).unwrap())
}

#[test]
fn concurrent_local_hosts_share_the_normal_deployment_identity() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.sqlite");
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let peers: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let app = app.clone();
            let hash = hash.clone();
            let encoded = encoded.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                compio::runtime::Runtime::new().unwrap().block_on(async {
                    DeploymentHolds::open_local(&path)
                        .await
                        .unwrap()
                        .record_deployment(&app, &hash, &encoded)
                        .await
                        .unwrap()
                })
            })
        })
        .collect();
    let ids: Vec<_> = peers.into_iter().map(|peer| peer.join().unwrap()).collect();
    for id in &ids[1..] {
        assert_eq!(id, &ids[0]);
    }
}

#[compio::test]
async fn local_catalog_preserves_identity_holds_and_reclamation_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("deployments/index.sqlite");
    let catalog = DeploymentHolds::open_local(&path).await.unwrap();
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let deployment = catalog
        .record_deployment(&app, &hash, &encoded)
        .await
        .unwrap();
    let holder = scope(&app);
    let receipt = catalog
        .acquire(&holder, &deployment, generation(1))
        .await
        .unwrap();
    drop(catalog);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ANALYZE")
        .unwrap();
    let catalog = DeploymentHolds::open_local(&path).await.unwrap();
    assert_eq!(
        catalog
            .record_deployment(&app, &hash, &encoded)
            .await
            .unwrap(),
        deployment
    );
    assert_eq!(
        catalog
            .acquire(&holder, &deployment, generation(1))
            .await
            .unwrap(),
        receipt
    );
    let foreign = AppId::mint();
    let other = catalog
        .record_deployment(&foreign, &hash, &encoded)
        .await
        .unwrap();
    assert_ne!(other, deployment);
    assert!(matches!(
        catalog
            .acquire(&scope(&foreign), &deployment, generation(1))
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));

    let tx = catalog.database.begin_transaction().await.unwrap();
    assert_conflict(fence_reclamation(&tx, &app, &deployment).await);
    tx.rollback().await.unwrap();
    catalog
        .release(&holder, &deployment, generation(1))
        .await
        .unwrap();
    let tx = catalog.database.begin_transaction().await.unwrap();
    fence_reclamation(&tx, &app, &deployment).await.unwrap();
    tx.commit().await.unwrap();
    assert_conflict(catalog.record_deployment(&app, &hash, &encoded).await);
    let tx = catalog.database.begin_transaction().await.unwrap();
    finish_reclamation(&tx, &app, &deployment).await.unwrap();
    tx.commit().await.unwrap();
    let reopened = DeploymentHolds::open_local(&path).await.unwrap();
    assert_conflict(reopened.record_deployment(&app, &hash, &encoded).await);
    assert_conflict(reopened.acquire(&holder, &deployment, generation(2)).await);
}

#[compio::test]
async fn postgres_concurrent_registration_preserves_the_winning_identity() {
    let fixture = Postgres::start().await;
    let first = DeploymentHolds::new(database(&fixture.url).await).unwrap();
    let second = DeploymentHolds::new(database(&fixture.url).await).unwrap();
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let (left, right) = futures::join!(
        first.record_deployment(&app, &hash, &encoded),
        second.record_deployment(&app, &hash, &encoded),
    );
    let deployment = left.unwrap();
    assert_eq!(deployment, right.unwrap());
    let holder = scope(&app);
    first
        .acquire(&holder, &deployment, generation(1))
        .await
        .unwrap();
    assert_eq!(
        second
            .record_deployment(&app, &hash, &encoded)
            .await
            .unwrap(),
        deployment
    );
    let tx = first.database.begin_transaction().await.unwrap();
    assert_conflict(fence_reclamation(&tx, &app, &deployment).await);
    tx.rollback().await.unwrap();
}

#[compio::test]
async fn registration_rejects_invalid_manifests_and_corrupt_existing_metadata() {
    let root = tempfile::tempdir().unwrap();
    let catalog = DeploymentHolds::open_local(&root.path().join("index.sqlite"))
        .await
        .unwrap();
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    for (invalid_hash, invalid_manifest) in [
        (hash.as_str(), "{}"),
        ("invalid", encoded.as_str()),
        (hash.as_str(), "[null]"),
    ] {
        assert!(matches!(
            catalog
                .record_deployment(&app, invalid_hash, invalid_manifest)
                .await,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    let deployment = catalog
        .record_deployment(&app, &hash, &encoded)
        .await
        .unwrap();
    catalog
        .database
        .collection(deploys::Entity::COLLECTION)
        .unwrap()
        .update(value!({"id":deployment}), value!({"manifest_json":"{}"}))
        .await
        .unwrap();
    assert!(matches!(
        catalog.record_deployment(&app, &hash, &encoded).await,
        Err(WorkflowServiceError::Internal(_))
    ));
}

#[compio::test]
async fn local_catalog_rejects_incompatible_schema_without_rewriting_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.sqlite");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TABLE app_deploys (id TEXT PRIMARY KEY); INSERT INTO app_deploys VALUES ('keep')").unwrap();
    assert!(matches!(
        DeploymentHolds::open_local(&path).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        connection
            .query_row("SELECT id FROM app_deploys", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    assert!(connection
        .prepare("SELECT * FROM app_deploy_holds")
        .is_err());
}
