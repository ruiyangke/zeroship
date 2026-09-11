use super::*;
use crate::operations::RunOperation;
use crate::service::{app::lock_app, PlatformPolicy, WorkerIdentity};

struct PlatformFixture {
    pg: PostgresFixture,
    admin: compio_postgres::Client,
    service: WorkflowService,
    app: AppId,
    other: AppId,
    plan: String,
    organization: String,
}

impl PlatformFixture {
    async fn start() -> Self {
        let pg = PostgresFixture::start().await;
        let admin = connect(&pg.admin_url).await;
        admin.batch_execute(
            "CREATE ROLE zeroship_control LOGIN; CREATE SCHEMA zeroship; \
             CREATE TABLE zeroship.plans (id text PRIMARY KEY, workflows_allowed bool NOT NULL, archived bool NOT NULL, runtime_limits_json jsonb NOT NULL); \
             CREATE TABLE zeroship.apps (id uuid PRIMARY KEY, plan_id text NOT NULL REFERENCES zeroship.plans, organization_id text NOT NULL, workflows_enabled bool NOT NULL, archived_at timestamptz, deleted_at timestamptz); \
             CREATE TABLE zeroship.app_spend_state (app_id uuid PRIMARY KEY REFERENCES zeroship.apps, state text NOT NULL); \
             CREATE TABLE zeroship.organization_billing_status (organization_id text PRIMARY KEY, state text NOT NULL); \
             CREATE TABLE zeroship.workflow_rollout_config (id text PRIMARY KEY, dispatch_paused bool NOT NULL, ingress_disabled bool NOT NULL); \
             CREATE TABLE zeroship.worker_instances (id text PRIMARY KEY, status text NOT NULL, public_key bytea NOT NULL); \
             GRANT USAGE ON SCHEMA zeroship TO zeroship_control; \
             GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA zeroship TO zeroship_control;"
        ).await.unwrap();
        admin
            .batch_execute(include_str!("../../../schema/platform-policy.sql"))
            .await
            .unwrap();
        let app = AppId::mint();
        let other = AppId::mint();
        let plan = typed_id::generate("pln");
        let organization = typed_id::generate("org");
        admin
            .execute(
                "INSERT INTO zeroship.plans VALUES ($1,true,false,'{}')",
                &[&plan],
            )
            .await
            .unwrap();
        for id in [&app, &other] {
            admin
                .execute(
                    "INSERT INTO zeroship.apps VALUES ($1,$2,$3,true,NULL,NULL)",
                    &[&id.uuid(), &plan, &organization],
                )
                .await
                .unwrap();
        }
        let url = pg.admin_url.replace("postgres@", "zeroship_workflow@");
        let service = WorkflowService::open(Arc::new(PostgresStore::platform(
            url,
            PlatformPolicy::new(AppPolicy::default()).unwrap(),
        )))
        .await
        .unwrap();
        for id in [&app, &other] {
            service
                .activate_deploy(
                    id,
                    &DeployRegistration {
                        id: typed_id::generate("dep"),
                        hash: "a".repeat(64),
                        workflows: ["Example".into()].into(),
                        schedules: Vec::new(),
                    },
                )
                .await
                .unwrap();
        }
        Self {
            pg,
            admin,
            service,
            app,
            other,
            plan,
            organization,
        }
    }

    async fn start_run(&self) -> Result<crate::operations::StartedRun, WorkflowServiceError> {
        self.service
            .for_app(self.app.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
    }
}

#[compio::test]
async fn platform_admission_reads_live_authority_and_keeps_management_available() {
    let fixture = PlatformFixture::start().await;
    let app = fixture.service.for_app(fixture.app.clone());
    assert!(fixture
        .service
        .register_app(&fixture.app, &AppPolicy::default())
        .await
        .is_err());
    let run = fixture.start_run().await.unwrap();
    let uuid = fixture.app.uuid();
    let admin = &fixture.admin;
    for column in [
        "workflows_enabled=false",
        "archived_at=now()",
        "deleted_at=now()",
    ] {
        admin
            .execute(
                &format!("UPDATE zeroship.apps SET {column} WHERE id=$1"),
                &[&uuid],
            )
            .await
            .unwrap();
        assert!(matches!(
            fixture.start_run().await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
        assert!(app.status(&run.id).await.is_ok());
        admin.execute("UPDATE zeroship.apps SET workflows_enabled=true,archived_at=NULL,deleted_at=NULL WHERE id=$1", &[&uuid]).await.unwrap();
    }
    for column in ["workflows_allowed=false", "archived=true"] {
        admin
            .execute(
                &format!("UPDATE zeroship.plans SET {column} WHERE id=$1"),
                &[&fixture.plan],
            )
            .await
            .unwrap();
        assert!(matches!(
            fixture.start_run().await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
        admin
            .execute(
                "UPDATE zeroship.plans SET workflows_allowed=true,archived=false WHERE id=$1",
                &[&fixture.plan],
            )
            .await
            .unwrap();
    }
    admin
        .execute(
            "INSERT INTO zeroship.app_spend_state VALUES ($1,'block')",
            &[&uuid],
        )
        .await
        .unwrap();
    // A workflow-owned row is not a cache of platform admission authority.
    admin
        .execute(
            "UPDATE workflow.apps SET policy=$2 WHERE app_id=$1",
            &[
                &fixture.app.as_str(),
                &serde_json::to_string(&AppPolicy::default()).unwrap(),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture.start_run().await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    app.transition(&RequestId::mint(), &run.id, RunOperation::Cancel)
        .await
        .unwrap();
    admin
        .execute(
            "DELETE FROM zeroship.app_spend_state WHERE app_id=$1",
            &[&uuid],
        )
        .await
        .unwrap();
    for state in ["suspended", "unknown"] {
        admin.execute("INSERT INTO zeroship.organization_billing_status VALUES ($1,$2) ON CONFLICT (organization_id) DO UPDATE SET state=excluded.state", &[&fixture.organization, &state]).await.unwrap();
        assert!(matches!(
            fixture.start_run().await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
    }
    admin
        .batch_execute("DELETE FROM zeroship.organization_billing_status")
        .await
        .unwrap();
    admin
        .execute(
            "UPDATE zeroship.plans SET runtime_limits_json=$2 WHERE id=$1",
            &[&fixture.plan, &json!({"workflow":{"maxLiveRuns":0}})],
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture.start_run().await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    admin
        .execute(
            "UPDATE zeroship.plans SET runtime_limits_json=$2 WHERE id=$1",
            &[&fixture.plan, &json!({"workflow":{"maxRuning":1}})],
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture.start_run().await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    admin
        .execute(
            "UPDATE zeroship.plans SET runtime_limits_json='{}' WHERE id=$1",
            &[&fixture.plan],
        )
        .await
        .unwrap();
    admin
        .batch_execute("UPDATE zeroship.workflow_rollout_config SET dispatch_paused=true")
        .await
        .unwrap();
    fixture.start_run().await.unwrap();
    let worker = WorkerIdentity::new("policy-worker".into()).unwrap();
    assert!(fixture.service.poll(&worker).await.unwrap().is_none());
    admin
        .batch_execute("UPDATE zeroship.workflow_rollout_config SET dispatch_paused=false")
        .await
        .unwrap();
    assert!(fixture.service.poll(&worker).await.unwrap().is_some());
    admin
        .batch_execute("DELETE FROM zeroship.workflow_rollout_config")
        .await
        .unwrap();
    assert!(matches!(
        fixture.start_run().await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
}

#[compio::test]
async fn policy_read_privileges_do_not_grant_control_writes_or_ddl() {
    let fixture = PlatformFixture::start().await;
    let runtime = connect(
        &fixture
            .pg
            .admin_url
            .replace("postgres@", "zeroship_workflow@"),
    )
    .await;
    for sql in [
        "UPDATE zeroship.apps SET workflows_enabled=true",
        "DELETE FROM zeroship.plans",
        "INSERT INTO zeroship.workflow_rollout_config VALUES ('rogue',false,false)",
        "CREATE TABLE workflow.rogue (id text)",
        "ALTER TABLE workflow.apps ADD COLUMN rogue text",
        "SET ROLE zeroship_workflow_owner",
    ] {
        let error = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(
            error.code(),
            Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{sql}: {error}"
        );
    }
    for role in ["zeroship_worker", "zeroship_gateway", "zeroship_app"] {
        let client = connect(
            &fixture
                .pg
                .admin_url
                .replace("postgres@", &format!("{role}@")),
        )
        .await;
        assert!(client
            .batch_execute("SELECT * FROM workflow.apps")
            .await
            .is_err());
        assert!(client
            .batch_execute("SELECT zeroship.workflow_policy_lock('app','x',true)")
            .await
            .is_err());
    }
}

async fn wait_for_advisory_wait(admin: &compio_postgres::Client, pid: i32) {
    compio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let row = admin
                .query_one(
                    "SELECT wait_event FROM pg_stat_activity WHERE pid=$1",
                    &[&pid],
                )
                .await
                .unwrap();
            if row.get::<_, Option<String>>(0).as_deref() == Some("advisory") {
                break;
            }
            compio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("policy writer must wait on the reader's advisory lock");
}

#[compio::test]
async fn policy_writes_serialize_with_admission_including_absent_billing_rows() {
    let fixture = PlatformFixture::start().await;
    let cases = [
        format!(
            "UPDATE zeroship.apps SET workflows_enabled=false WHERE id='{}'",
            fixture.app.uuid()
        ),
        format!(
            "UPDATE zeroship.plans SET workflows_allowed=false WHERE id='{}'",
            fixture.plan
        ),
        format!(
            "INSERT INTO zeroship.app_spend_state VALUES ('{}','block')",
            fixture.app.uuid()
        ),
        format!(
            "INSERT INTO zeroship.organization_billing_status VALUES ('{}','suspended')",
            fixture.organization
        ),
        "UPDATE zeroship.workflow_rollout_config SET dispatch_paused=true".into(),
    ];
    for sql in cases {
        let mut reader = fixture.service.store.begin().await.unwrap();
        lock_app(&mut reader, &fixture.app).await.unwrap();
        let writer = connect(
            &fixture
                .pg
                .admin_url
                .replace("postgres@", "zeroship_control@"),
        )
        .await;
        let pid: i32 = writer
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let write = compio::runtime::spawn(async move { writer.batch_execute(&sql).await });
        wait_for_advisory_wait(&fixture.admin, pid).await;
        reader.commit().await.unwrap();
        write.await.unwrap().unwrap();
        let mut after = fixture.service.store.begin().await.unwrap();
        let policy = lock_app(&mut after, &fixture.app).await.unwrap();
        assert!(!policy.admission || !policy.dispatch);
        after.commit().await.unwrap();
        fixture.admin.batch_execute(
            "UPDATE zeroship.apps SET workflows_enabled=true; \
             UPDATE zeroship.plans SET workflows_allowed=true; \
             DELETE FROM zeroship.app_spend_state; DELETE FROM zeroship.organization_billing_status; \
             UPDATE zeroship.workflow_rollout_config SET dispatch_paused=false;"
        ).await.unwrap();
    }
    // App policy writes do not block admissions belonging to an unrelated app.
    let writer = connect(&fixture.pg.admin_url).await;
    writer.batch_execute("BEGIN").await.unwrap();
    writer
        .execute(
            "UPDATE zeroship.apps SET workflows_enabled=false WHERE id=$1",
            &[&fixture.app.uuid()],
        )
        .await
        .unwrap();
    compio::time::timeout(
        std::time::Duration::from_secs(10),
        fixture.service.for_app(fixture.other.clone()).start(
            &RequestId::mint(),
            "Example",
            StartOptions::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    writer.batch_execute("COMMIT").await.unwrap();
    assert!(matches!(
        fixture.start_run().await,
        Err(WorkflowServiceError::PermissionDenied)
    ));

    // A reader that arrives after the writer must wait for its decision and
    // then read the committed disable, never an earlier MVCC snapshot.
    writer.batch_execute("BEGIN").await.unwrap();
    writer
        .execute(
            "UPDATE zeroship.apps SET workflows_enabled=true WHERE id=$1",
            &[&fixture.app.uuid()],
        )
        .await
        .unwrap();
    let service = fixture.service.clone();
    let app = fixture.app.clone();
    let (ready, waiting) = futures::channel::oneshot::channel();
    let reader = compio::runtime::spawn(async move {
        let mut tx = service.store.begin().await.unwrap();
        let pid = tx
            .query("SELECT pg_backend_pid()::bigint AS pid", &[])
            .await
            .unwrap()[0]
            .integer("pid")
            .unwrap();
        ready.send(pid as i32).unwrap();
        let policy = lock_app(&mut tx, &app).await.unwrap();
        tx.commit().await.unwrap();
        policy
    });
    wait_for_advisory_wait(&fixture.admin, waiting.await.unwrap()).await;
    writer.batch_execute("ROLLBACK").await.unwrap();
    assert!(!reader.await.unwrap().admission);
}
