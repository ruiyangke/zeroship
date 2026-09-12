use super::*;
use std::{path::Path, process::Command, time::Duration};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{binding::DbBinding, encryption::ProjectKeySource, ConnectOptions};

mod catalog;

async fn database(url: &str) -> Database {
    let schema = if url.starts_with("sqlite:") {
        "main"
    } else {
        "zeroship"
    };
    Database::connect(
        DbBinding::new(
            "platform",
            "deployment-holds-test",
            SchemaName::new(schema).unwrap(),
        ),
        ConnectOptions::new(url, ProjectKeySource::unavailable()).connection_authority(),
        collections().unwrap(),
    )
    .await
    .unwrap()
}

async fn seed(db: &Database, app: &AppId, hash: &str) -> String {
    let deployment = typed_id::generate("dep");
    let mut document = value!({
        "id":deployment, "app_id":app.uuid().to_string(), "deploy_hash":hash,
        "manifest_json":"{}", "activated_at":null,
        "retention_state":"available", "retention_lock":0
    });
    document["created_at"] = Value::Timestamp(0);
    db.collection(deploys::Entity::COLLECTION)
        .unwrap()
        .insert(document)
        .await
        .unwrap();
    deployment
}

fn scope(app: &AppId) -> HoldScope {
    HoldScope::new(app.clone(), typed_id::generate("dhl")).unwrap()
}
fn generation(value: i64) -> HoldGeneration {
    value.try_into().unwrap()
}
fn assert_conflict<T: std::fmt::Debug>(result: Result<T, WorkflowServiceError>) {
    assert!(
        matches!(result, Err(WorkflowServiceError::Conflict(_))),
        "{result:?}"
    );
}

#[derive(FromRow)]
#[orm(entity = holds)]
struct StoredHoldIdentity {
    id: String,
}

async fn stored_hold_identity(database: &Database, holder: &HoldScope, deployment: &str) -> String {
    let rows = database
        .entity::<holds::Entity>()
        .unwrap()
        .find::<StoredHoldIdentity>(
            holds::app_id
                .eq(holder.app.uuid().to_string())
                .unwrap()
                .and(holds::deploy_id.eq(deployment).unwrap())
                .and(holds::holder_id.eq(holder.holder.clone()).unwrap()),
            FindOptions::default(),
        )
        .await
        .unwrap();
    let [row] = rows.as_slice() else {
        panic!("holder scope must identify a stored receipt");
    };
    typed_id::parse_with_prefix(&row.id, "dhr").unwrap();
    row.id.clone()
}

#[compio::test]
async fn sqlite_holds_survive_reconnection_and_fence_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("platform.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(SQLITE_SCHEMA)
        .unwrap();
    contract(&format!("sqlite:{}", path.display())).await;
}

#[compio::test]
async fn postgres_holds_survive_reconnection_and_fence_reclamation() {
    let fixture = Postgres::start().await;
    contract(&fixture.url).await;
    let control = connect(&fixture.url).await;
    let error = control
        .query("SELECT * FROM customer.__zeroship_workflow_runs", &[])
        .await
        .unwrap_err();
    assert_eq!(error.as_db_error().unwrap().code().code(), "42501");
}

async fn contract(url: &str) {
    let db = database(url).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    let hash = "a".repeat(64);
    let deployment = seed(&db, &app, &hash).await;
    let other_deployment = seed(&db, &app, &"b".repeat(64)).await;
    let foreign_deployment = seed(&db, &foreign, &hash).await;
    let holder = scope(&app);
    let peer = scope(&app);
    let holds = DeploymentHolds::new(db.clone()).unwrap();
    let receipt = holds
        .acquire(&holder, &deployment, generation(1))
        .await
        .unwrap();
    assert_eq!(receipt.deploy_hash, hash);
    assert_eq!(receipt.app_id, app);
    assert_eq!(receipt.holder_id, holder.holder);
    assert_eq!(receipt.state, HoldState::Held);
    let stored_id = stored_hold_identity(&db, &holder, &deployment).await;
    assert_eq!(
        holds
            .acquire(&holder, &deployment, generation(1))
            .await
            .unwrap(),
        receipt
    );

    // Reconnect through a new ORM handle; no in-memory owner or placement lease
    // is needed to recover the acknowledgement.
    let restarted = DeploymentHolds::new(database(url).await).unwrap();
    assert_eq!(
        restarted
            .acquire(&holder, &deployment, generation(1))
            .await
            .unwrap(),
        receipt
    );
    for (wrong_scope, wrong_deployment) in [
        (scope(&foreign), deployment.as_str()),
        (holder.clone(), foreign_deployment.as_str()),
    ] {
        assert!(matches!(
            holds
                .acquire(&wrong_scope, wrong_deployment, generation(1))
                .await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
        assert!(matches!(
            holds
                .release(&wrong_scope, wrong_deployment, generation(1))
                .await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
    }
    assert_conflict(holds.release(&peer, &deployment, generation(1)).await);
    assert_conflict(
        holds
            .release(&holder, &other_deployment, generation(1))
            .await,
    );
    assert_conflict(holds.acquire(&peer, &deployment, generation(2)).await);

    holds
        .acquire(&peer, &deployment, generation(1))
        .await
        .unwrap();
    let released = holds
        .release(&holder, &deployment, generation(1))
        .await
        .unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(
        restarted
            .release(&holder, &deployment, generation(1))
            .await
            .unwrap(),
        released
    );
    assert_conflict(restarted.acquire(&holder, &deployment, generation(1)).await);
    assert_conflict(
        transact(&db, async |tx| {
            fence_reclamation(&tx, &app, &deployment).await
        })
        .await,
    );

    let renewed = restarted
        .acquire(&holder, &deployment, generation(2))
        .await
        .unwrap();
    assert_eq!(renewed.generation, generation(2));
    assert_eq!(
        stored_hold_identity(&db, &holder, &deployment).await,
        stored_id
    );
    assert_conflict(holds.release(&holder, &deployment, generation(1)).await);
    assert_conflict(holds.acquire(&holder, &deployment, generation(1)).await);
    holds
        .release(&peer, &deployment, generation(1))
        .await
        .unwrap();
    assert_conflict(
        transact(&db, async |tx| {
            fence_reclamation(&tx, &app, &deployment).await
        })
        .await,
    );
    holds
        .release(&holder, &deployment, generation(2))
        .await
        .unwrap();

    assert_conflict(
        transact(&db, async |tx| {
            finish_reclamation(&tx, &app, &deployment).await
        })
        .await,
    );
    assert_conflict(
        transact(&db, async |tx| {
            assert_eq!(fence_reclamation(&tx, &app, &deployment).await?, hash);
            Err::<(), _>(conflict("collector aborted"))
        })
        .await,
    );
    holds
        .acquire(&holder, &deployment, generation(3))
        .await
        .unwrap();
    holds
        .release(&holder, &deployment, generation(3))
        .await
        .unwrap();

    assert_eq!(
        transact(&db, async |tx| fence_reclamation(&tx, &app, &deployment)
            .await)
        .await
        .unwrap(),
        hash
    );
    assert_conflict(restarted.acquire(&holder, &deployment, generation(4)).await);
    assert_conflict(
        holds
            .acquire(&scope(&app), &deployment, generation(1))
            .await,
    );
    // A lost release acknowledgement is still recoverable after reclamation.
    restarted
        .release(&holder, &deployment, generation(3))
        .await
        .unwrap();
    for _ in 0..2 {
        transact(&db, async |tx| {
            assert_eq!(fence_reclamation(&tx, &app, &deployment).await?, hash);
            finish_reclamation(&tx, &app, &deployment).await
        })
        .await
        .unwrap();
    }
    assert_conflict(holds.acquire(&holder, &deployment, generation(4)).await);
    // The closed deployment has not closed admission for another deployment.
    holds
        .acquire(&holder, &other_deployment, generation(1))
        .await
        .unwrap();

    let suspect = seed(&db, &app, &"c".repeat(64)).await;
    holds
        .acquire(&holder, &suspect, generation(1))
        .await
        .unwrap();
    for patch in [
        value!({"state":"unknown"}),
        value!({"state":"released", "generation":0}),
    ] {
        db.collection(models::app_deploy_holds::Entity::COLLECTION).unwrap()
            .update(value!({"app_id":app.uuid().to_string(), "deploy_id":suspect, "holder_id":holder.holder}), patch)
            .await.unwrap();
        assert_conflict(
            transact(&db, async |tx| fence_reclamation(&tx, &app, &suspect).await).await,
        );
        assert!(matches!(
            holds.acquire(&holder, &suspect, generation(1)).await,
            Err(WorkflowServiceError::Internal(_))
        ));
    }

    callback_errors_and_cancellation_roll_back(&db, &holds, &app).await;
}

async fn callback_errors_and_cancellation_roll_back(
    database: &Database,
    holds: &DeploymentHolds,
    app: &AppId,
) {
    let deployment = seed(database, app, &"d".repeat(64)).await;
    for error in [
        WorkflowServiceError::InvalidRequest("invalid command".into()),
        WorkflowServiceError::Unauthenticated,
        WorkflowServiceError::PermissionDenied,
        WorkflowServiceError::NotFound("missing record".into()),
        WorkflowServiceError::Conflict("changed record".into()),
        WorkflowServiceError::ResourceExhausted("capacity".into()),
        WorkflowServiceError::PayloadTooLarge,
        WorkflowServiceError::Unavailable("peer unavailable".into()),
        WorkflowServiceError::Timeout,
        WorkflowServiceError::Internal("host failure".into()),
    ] {
        let result = transact(database, async |tx| {
            fence_reclamation(&tx, app, &deployment).await?;
            Err::<(), _>(error.clone())
        })
        .await;
        assert_eq!(result, Err(error));
    }

    let (fenced, ready) = futures::channel::oneshot::channel();
    compio::time::timeout(Duration::from_secs(10), async {
        let mut pending = Box::pin(transact(database, async |tx| {
            fence_reclamation(&tx, app, &deployment).await?;
            fenced.send(()).unwrap();
            std::future::pending::<Result<(), WorkflowServiceError>>().await
        }));
        match futures::future::select(&mut pending, ready).await {
            futures::future::Either::Left(_) => panic!("collector completed before cancellation"),
            futures::future::Either::Right((ready, _)) => ready.unwrap(),
        }
        drop(pending);
        // An aborted collector must release both its writes and ORM admission.
        holds
            .acquire(&scope(app), &deployment, generation(1))
            .await
            .unwrap();
    })
    .await
    .expect("cancelled collector must release deployment admission");
}

#[compio::test]
async fn postgres_reclamation_serializes_with_concurrent_hold_acquisition() {
    let fixture = Postgres::start().await;
    let db = database(&fixture.url).await;
    let holds = DeploymentHolds::new(database(&fixture.url).await).unwrap();
    let app = AppId::mint();
    let deployment = seed(&db, &app, &"a".repeat(64)).await;
    let holder = scope(&app);
    let attempt = transact(&db, async |tx| {
        fence_reclamation(&tx, &app, &deployment).await?;
        let attempt = compio::runtime::spawn(async move {
            holds.acquire(&holder, &deployment, generation(1)).await
        });
        // Observe the real competing database lock before committing the collector.
        // This proves the ordering without assuming a scheduler delay.
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = fixture.admin.query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE usename = 'zeroship_control' AND wait_event_type = 'Lock'",
                    &[],
                ).await.unwrap();
                if row.get::<_, i64>(0) > 0 { break; }
                compio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("hold acquisition must wait for the reclamation transaction");
        Ok(attempt)
    })
    .await
    .unwrap();
    assert_conflict(
        compio::time::timeout(Duration::from_secs(10), attempt)
            .await
            .unwrap()
            .unwrap(),
    );
}

#[test]
fn hold_identity_and_generation_are_validated() {
    assert!(HoldScope::new(AppId::mint(), typed_id::generate("wrk")).is_err());
    for invalid in [0, -1, i64::MIN] {
        assert!(HoldGeneration::try_from(invalid).is_err());
        assert!(serde_json::from_value::<HoldGeneration>(serde_json::json!(invalid)).is_err());
    }
    assert!(generation(i64::MAX).next().is_err());
    assert_eq!(generation(1).next().unwrap(), generation(2));
}

#[test]
fn deployment_models_match_the_migration_compiler() {
    let output = Command::new("node")
        .args([
            "crates/zeroship-workflow/schema/deployments/generate.mjs",
            "--check",
        ])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Postgres {
    _container: Container<GenericImage>,
    url: String,
    admin: compio_postgres::Client,
}
impl Postgres {
    async fn start() -> Self {
        let container = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("deployment hold tests require PostgreSQL");
        let host = container.get_host().unwrap();
        let port = container.get_host_port_ipv4(5432).unwrap();
        let admin = connect(&format!("postgres://postgres@{host}:{port}/postgres")).await;
        admin.batch_execute(
            "CREATE SCHEMA zeroship; \
             CREATE ROLE zeroship_control LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; \
             CREATE SCHEMA customer; CREATE TABLE customer.__zeroship_workflow_runs (id text PRIMARY KEY);"
        ).await.unwrap();
        admin.batch_execute(POSTGRES_SCHEMA).await.unwrap();
        admin.batch_execute(
            "GRANT USAGE ON SCHEMA zeroship TO zeroship_control; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA zeroship TO zeroship_control;"
        ).await.unwrap();
        Self {
            _container: container,
            url: format!("postgres://zeroship_control@{host}:{port}/postgres"),
            admin,
        }
    }
}
async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move {
        connection.run().await.unwrap();
    })
    .detach();
    client
}
