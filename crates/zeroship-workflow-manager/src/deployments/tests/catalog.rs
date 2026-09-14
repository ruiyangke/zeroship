use super::*;

/// The local host's catalog lives in its combined platform metadata file.
async fn local(path: &Path) -> Result<DeploymentHolds, Error> {
    crate::local::LocalPlatform::open(path)
        .await
        .map(|platform| platform.deployments().clone())
}

fn manifest() -> (String, String) {
    let mut manifest = zeroship_bundle::Manifest::default();
    let hash =
        zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    manifest.deploy_hash = Some(hash.clone());
    (hash, serde_json::to_string(&manifest).unwrap())
}

#[test]
fn opening_waits_for_a_host_that_still_holds_the_platform_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("platform/metadata.sqlite");
    compio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { local(&path).await.unwrap() });
    // A replaced process can hold the file while it shuts down.
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let (finished, done) = std::sync::mpsc::channel();
    let opener = {
        let path = path.clone();
        std::thread::spawn(move || {
            let opened = compio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { local(&path).await.map(|_| ()) });
            finished.send(()).unwrap();
            opened
        })
    };
    assert!(
        done.recv_timeout(std::time::Duration::from_secs(6))
            .is_err(),
        "opening must keep waiting through a restart handoff instead of failing"
    );
    holder.execute_batch("COMMIT").unwrap();
    opener.join().unwrap().unwrap();
}

#[test]
fn concurrent_local_hosts_share_the_normal_deployment_identity() {
    for _ in 0..8 {
        concurrent_local_registration();
    }
}

fn concurrent_local_registration() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.sqlite");
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    // Spawn every participant before joining any thread waiting at the barrier.
    let peers: [_; 4] = std::array::from_fn(|_| {
        let path = path.clone();
        let app = app.clone();
        let hash = hash.clone();
        let encoded = encoded.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            compio::runtime::Runtime::new().unwrap().block_on(async {
                local(&path)
                    .await
                    .unwrap()
                    .record_deployment(&app, &hash, &encoded)
                    .await
                    .unwrap()
            })
        })
    });
    let ids: Vec<_> = peers.into_iter().map(|peer| peer.join().unwrap()).collect();
    for id in &ids[1..] {
        assert_eq!(id, &ids[0]);
    }
}

#[compio::test]
async fn local_catalog_preserves_identity_holds_and_reclamation_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("deployments/index.sqlite");
    let catalog = local(&path).await.unwrap();
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
    let catalog = local(&path).await.unwrap();
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
        Err(Error::PermissionDenied)
    ));

    assert_conflict(fence_reclamation(&catalog.database, &app, &deployment).await);
    catalog
        .release(&holder, &deployment, generation(1))
        .await
        .unwrap();
    fence_reclamation(&catalog.database, &app, &deployment)
        .await
        .unwrap();
    assert_conflict(catalog.record_deployment(&app, &hash, &encoded).await);
    finish_reclamation(&catalog.database, &app, &deployment)
        .await
        .unwrap();
    let reopened = local(&path).await.unwrap();
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
    assert_conflict(
        transact(&first.database, async |tx| {
            fence_reclamation(&tx, &app, &deployment).await
        })
        .await,
    );
}

#[compio::test]
async fn postgres_collector_helpers_keep_the_lock_and_fence_in_one_transaction() {
    let fixture = Postgres::start().await;
    let db = database(&fixture.url).await;
    let ledger = DeploymentHolds::new(db.clone()).unwrap();
    let app = AppId::mint();
    let hash = "b".repeat(64);
    let deployment = seed(&db, &app, &hash).await;
    fixture.admin.batch_execute(
        "CREATE FUNCTION zeroship.require_collector_transaction() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.retention_state IS DISTINCT FROM OLD.retention_state
              AND current_setting('workflow.collector_lock', true) IS DISTINCT FROM OLD.id THEN
             RAISE EXCEPTION 'collector lock did not survive until its state change';
           END IF;
           PERFORM set_config('workflow.collector_lock', NEW.id, true);
           RETURN NEW;
         END $$;
         CREATE TRIGGER require_collector_transaction BEFORE UPDATE ON zeroship.app_deploys
         FOR EACH ROW EXECUTE FUNCTION zeroship.require_collector_transaction();",
    ).await.unwrap();

    // The marker is transaction-local: an autocommit lock cannot authorize a
    // later state change, even if the pool returns the same connection.
    assert!(
        db.collection(deploys::Entity::COLLECTION)
            .unwrap()
            .update(
                value!({"id":deployment}),
                value!({"retention_state":"reclaiming"}),
            )
            .await
            .is_err()
    );
    let holder = scope(&app);
    ledger
        .acquire(&holder, &deployment, generation(1))
        .await
        .unwrap();
    assert_conflict(fence_reclamation(&db, &app, &deployment).await);
    ledger
        .release(&holder, &deployment, generation(1))
        .await
        .unwrap();

    assert_eq!(
        fence_reclamation(&db, &app, &deployment).await.unwrap(),
        hash
    );
    assert_conflict(ledger.acquire(&holder, &deployment, generation(2)).await);
    finish_reclamation(&db, &app, &deployment).await.unwrap();
    let records = db
        .entity::<deploys::Entity>()
        .unwrap()
        .find::<DeploymentRecord>(deploys::id.eq(deployment).unwrap(), FindOptions::default())
        .await
        .unwrap();
    let [record] = records.as_slice() else {
        panic!("collector must preserve the deployment tombstone");
    };
    assert_eq!(record.retention_state, "deleted");
}

#[compio::test]
async fn registration_rejects_invalid_manifests_and_corrupt_existing_metadata() {
    let root = tempfile::tempdir().unwrap();
    let catalog = local(&root.path().join("index.sqlite")).await.unwrap();
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
            Err(Error::InvalidRequest(_))
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
        Err(Error::Internal(_))
    ));
}

#[compio::test]
async fn local_catalog_uses_the_exact_host_selected_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("normal-app-deployments.sqlite");
    let ledger = local(&path).await.unwrap();
    let app = AppId::mint();
    let (hash, encoded) = manifest();
    let deployment = ledger
        .record_deployment(&app, &hash, &encoded)
        .await
        .unwrap();
    let holder = scope(&app);
    ledger
        .acquire(&holder, &deployment, generation(1))
        .await
        .unwrap();
    let stored = rusqlite::Connection::open(&path).unwrap();
    let identity: String = stored
        .query_row(
            "SELECT id FROM app_deploys WHERE app_id = ?1 AND deploy_hash = ?2",
            rusqlite::params![app.as_str(), hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(identity, deployment);
    let state: String = stored.query_row(
        "SELECT state FROM app_deploy_holds WHERE app_id = ?1 AND deploy_id = ?2 AND holder_id = ?3",
        rusqlite::params![app.as_str(), deployment, holder.holder()],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(state, "held");
}

#[compio::test]
async fn local_catalog_rejects_incompatible_schema_without_rewriting_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.sqlite");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TABLE app_deploys (id TEXT PRIMARY KEY); INSERT INTO app_deploys VALUES ('keep')").unwrap();
    assert!(matches!(local(&path).await, Err(Error::Unavailable(_))));
    assert_eq!(
        connection
            .query_row("SELECT id FROM app_deploys", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    assert!(
        connection
            .prepare("SELECT * FROM app_deploy_holds")
            .is_err()
    );
}
