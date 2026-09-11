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
             CREATE TABLE zeroship.apps (id uuid PRIMARY KEY, plan_id text NOT NULL REFERENCES zeroship.plans, organization_id text NOT NULL, workflows_enabled bool NOT NULL, archived_at timestamptz, deleted_at timestamptz, deploy_hash text, manifest_json text); \
             CREATE TABLE zeroship.app_deploys (id text PRIMARY KEY,app_id uuid NOT NULL REFERENCES zeroship.apps,deploy_hash text NOT NULL,manifest_json text NOT NULL,activated_at timestamptz,UNIQUE(app_id,deploy_hash)); \
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
        admin
            .batch_execute(
                "REVOKE INSERT,UPDATE,DELETE ON workflow.schema_version FROM zeroship_workflow",
            )
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
                    "INSERT INTO zeroship.apps (id,plan_id,organization_id,workflows_enabled) VALUES ($1,$2,$3,true)",
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
            publish_deploy(
                &pg.admin_url,
                id,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into()].into(),
                    schedules: Vec::new(),
                },
            )
            .await;
            service.reconcile_deploy(id).await.unwrap();
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

async fn publish_deploy(url: &str, app: &AppId, deploy: &DeployRegistration) {
    let mut conn = connect(url).await;
    let tx = conn.transaction().await.unwrap();
    let manifest = serde_json::to_string(&serde_json::json!({
        "workflows":deploy.workflows,"schedules":deploy.schedules
    }))
    .unwrap();
    tx.execute(
        "UPDATE zeroship.apps SET deploy_hash=$2,manifest_json=$3 WHERE id=$1",
        &[&app.uuid(), &deploy.hash, &manifest],
    )
    .await
    .unwrap();
    tx.execute("INSERT INTO zeroship.app_deploys (id,app_id,deploy_hash,manifest_json,activated_at) VALUES ($1,$2,$3,$4,now())", &[&deploy.id,&app.uuid(),&deploy.hash,&manifest]).await.unwrap();
    tx.commit().await.unwrap();
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
            "UPDATE workflow.app_state SET policy=$2 WHERE app_id=$1",
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
        "ALTER TABLE workflow.app_state ADD COLUMN rogue text",
        "SET ROLE zeroship_workflow_migrator",
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
            .batch_execute("SELECT * FROM workflow.app_state")
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

#[compio::test]
async fn platform_deploy_selection_is_current_while_live_runs_keep_their_snapshot() {
    use crate::service::{
        IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
    };
    let fixture = PlatformFixture::start().await;
    let first = fixture.start_run().await.unwrap();
    let original: String = fixture
        .admin
        .query_one(
            "SELECT deploy_id FROM workflow.runs WHERE id=$1",
            &[&first.id],
        )
        .await
        .unwrap()
        .get(0);
    let next = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: "b".repeat(64),
        workflows: ["Next".into()].into(),
        schedules: vec![ScheduleRegistration {
            name: "Recurring".into(),
            workflow_name: "Next".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 60_000,
                anchor: IntervalAnchor::Deploy,
            },
            input: json!(null),
            overlap: ScheduleOverlap::Allow,
            catch_up: ScheduleCatchUp::Skip,
        }],
    };
    publish_deploy(&fixture.pg.admin_url, &fixture.app, &next).await;
    // Start must observe the current selection even before a notification arrives.
    let run = fixture
        .service
        .for_app(fixture.app.clone())
        .start(&RequestId::mint(), "Next", StartOptions::default())
        .await
        .unwrap();
    let selected: String = fixture
        .admin
        .query_one(
            "SELECT deploy_id FROM workflow.runs WHERE id=$1",
            &[&run.id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(selected, next.id);
    assert!(matches!(
        fixture.start_run().await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(fixture
        .service
        .activate_deploy(&fixture.app, &next)
        .await
        .is_err());
    let before: i64 = fixture
        .admin
        .query_one(
            "SELECT next_at FROM workflow.schedules WHERE app_id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    fixture
        .service
        .reconcile_deploy(&fixture.app)
        .await
        .unwrap();
    fixture
        .service
        .reconcile_deploy(&fixture.app)
        .await
        .unwrap();
    let after: i64 = fixture
        .admin
        .query_one(
            "SELECT next_at FROM workflow.schedules WHERE app_id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        before, after,
        "delayed notifications must preserve the current frontier"
    );
    let task = fixture
        .service
        .poll(&WorkerIdentity::new("test-worker".into()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.invocation.run_id, first.id);
    assert_eq!(task.invocation.deploy_id, original);
    assert!(fixture
        .admin
        .execute(
            "UPDATE zeroship.app_deploys SET manifest_json='{}' WHERE id=$1",
            &[&next.id]
        )
        .await
        .is_err());

    fixture
        .admin
        .execute(
            "UPDATE workflow.schedules SET next_at=0 WHERE app_id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap();
    let removed = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: "c".repeat(64),
        workflows: ["Next".into()].into(),
        schedules: Vec::new(),
    };
    publish_deploy(&fixture.pg.admin_url, &fixture.app, &removed).await;
    assert_eq!(
        fixture.service.tick_schedules().await.unwrap(),
        0,
        "a removed schedule cannot fire while its notification is delayed"
    );
    let due: Option<i64> = fixture
        .admin
        .query_one(
            "SELECT next_at FROM workflow.schedules WHERE app_id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(due, None);
}

#[compio::test]
async fn platform_deploy_history_writes_wait_for_workflow_admission() {
    let fixture = PlatformFixture::start().await;
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
    let app = fixture.app.uuid();
    let write = compio::runtime::spawn(async move {
        writer
            .execute(
                "UPDATE zeroship.app_deploys SET activated_at=now() WHERE app_id=$1",
                &[&app],
            )
            .await
    });
    wait_for_advisory_wait(&fixture.admin, pid).await;
    reader.commit().await.unwrap();
    assert_eq!(write.await.unwrap().unwrap(), 1);
}

#[compio::test]
async fn platform_deploy_outbox_commits_with_activation_and_recovers_fairly() {
    let fixture = PlatformFixture::start().await;
    let mut apps = [&fixture.app, &fixture.other];
    apps.sort_by_key(|app| app.uuid());
    let [earlier, later] = apps;
    let revision: i64 = fixture
        .admin
        .query_one(
            "SELECT revision FROM zeroship.workflow_deploy_notifications WHERE app_id=$1",
            &[&earlier.uuid()],
        )
        .await
        .unwrap()
        .get(0);
    let writer = connect(&fixture.pg.admin_url).await;
    writer.batch_execute("BEGIN").await.unwrap();
    writer
        .execute(
            "UPDATE zeroship.apps SET deploy_hash=$2 WHERE id=$1",
            &[&earlier.uuid(), &"f".repeat(64)],
        )
        .await
        .unwrap();
    let uncommitted: i64 = writer
        .query_one(
            "SELECT revision FROM zeroship.workflow_deploy_notifications WHERE app_id=$1",
            &[&earlier.uuid()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(uncommitted > revision);
    writer.batch_execute("ROLLBACK").await.unwrap();
    let after: i64 = fixture
        .admin
        .query_one(
            "SELECT revision FROM zeroship.workflow_deploy_notifications WHERE app_id=$1",
            &[&earlier.uuid()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        after, revision,
        "rolled-back activation cannot leave a notification"
    );
    for app in apps {
        publish_deploy(
            &fixture.pg.admin_url,
            app,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "b".repeat(64),
                workflows: ["Next".into()].into(),
                schedules: Vec::new(),
            },
        )
        .await;
    }
    fixture
        .admin
        .execute(
            "UPDATE zeroship.apps SET manifest_json='{}' WHERE id=$1",
            &[&earlier.uuid()],
        )
        .await
        .unwrap();
    let first = fixture
        .service
        .reconcile_pending_deploys(None, 1)
        .await
        .unwrap();
    assert_eq!(first.reconciled, 0);
    assert!(first.next.is_some());

    // A newly created app belongs to the next pass, so it cannot keep the
    // failed earlier app waiting behind an expanding page boundary.
    let newcomer = AppId::mint();
    assert!(newcomer.uuid() > later.uuid());
    fixture.admin.execute("INSERT INTO zeroship.apps (id,plan_id,organization_id,workflows_enabled) VALUES ($1,$2,$3,true)", &[&newcomer.uuid(),&fixture.plan,&fixture.organization]).await.unwrap();
    publish_deploy(
        &fixture.pg.admin_url,
        &newcomer,
        &DeployRegistration {
            id: typed_id::generate("dep"),
            hash: "c".repeat(64),
            workflows: ["Next".into()].into(),
            schedules: Vec::new(),
        },
    )
    .await;
    let second = fixture
        .service
        .reconcile_pending_deploys(first.next, 1)
        .await
        .unwrap();
    assert_eq!(
        second.reconciled, 1,
        "a malformed earlier deployment must not block a later app"
    );
    let end = fixture
        .service
        .reconcile_pending_deploys(second.next, 1)
        .await
        .unwrap();
    assert_eq!(end.reconciled, 0);
    assert!(end.next.is_none());
    let unmapped: i64 = fixture
        .admin
        .query_one(
            "SELECT count(*) FROM workflow.app_state WHERE app_id=$1",
            &[&newcomer.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(unmapped, 0);
    fixture.admin.execute("UPDATE zeroship.apps a SET manifest_json=d.manifest_json FROM zeroship.app_deploys d WHERE a.id=$1 AND d.app_id=a.id AND d.deploy_hash=a.deploy_hash", &[&earlier.uuid()]).await.unwrap();
    let recovered = WorkflowService::open(fixture.service.store.clone())
        .await
        .unwrap();
    assert_eq!(
        recovered
            .reconcile_pending_deploys(None, 64)
            .await
            .unwrap()
            .reconciled,
        2
    );
    assert_eq!(
        recovered
            .reconcile_pending_deploys(None, 64)
            .await
            .unwrap()
            .reconciled,
        0
    );
    for app in [earlier, later, &newcomer] {
        let row = fixture.admin.query_one("SELECT n.revision,s.deploy_revision FROM zeroship.workflow_deploy_notifications n JOIN workflow.app_state s ON s.platform_app_id=n.app_id::text WHERE n.app_id=$1", &[&app.uuid()]).await.unwrap();
        assert_eq!(row.get::<_, i64>(0), row.get::<_, i64>(1));
    }
}
