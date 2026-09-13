#![allow(
    clippy::future_not_send,
    reason = "fixtures use compio pools on their owning test runtime"
)]

use compio_postgres::{Client, NoTls};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    time::Duration,
};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    typed_id,
    workflow_coordination::*,
};
use zeroship_workflow_manager::Error;
use zeroship_workflow_server::coordinator::{Coordinator, Error as HostError, Options, SCHEMA_SQL};

type StoredIds = BTreeMap<(String, String, String), String>;

struct Fixture {
    _postgres: Container<GenericImage>,
    admin: Client,
    runtime_url: String,
}
impl Fixture {
    async fn new() -> Self {
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("coordinator tests require Testcontainers PostgreSQL");
        let address = format!(
            "{}:{}",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let admin = connect(&format!("postgres://postgres@{address}/postgres")).await;
        admin
            .batch_execute(
                "CREATE ROLE coordinator_test LOGIN;
             CREATE SCHEMA workflow_manager;
             CREATE SCHEMA customer;
             CREATE TABLE customer.__zeroship_workflow_history(id text PRIMARY KEY,secret text);
             REVOKE ALL ON SCHEMA customer FROM PUBLIC;",
            )
            .await
            .unwrap();
        admin
            .execute(
                "INSERT INTO customer.__zeroship_workflow_history(id,secret) VALUES($1,'customer-private-history')",
                &[&typed_id::generate("wfh")],
            )
            .await
            .unwrap();
        admin.batch_execute(SCHEMA_SQL).await.unwrap();
        admin.batch_execute(
            "GRANT USAGE ON SCHEMA workflow_manager TO coordinator_test;
             GRANT SELECT ON workflow_manager.schema_version TO coordinator_test;
             GRANT SELECT,INSERT,UPDATE,DELETE ON workflow_manager.workers,
               workflow_manager.queue_scopes,workflow_manager.assignments,
               workflow_manager.placement_receipts,workflow_manager.management,workflow_manager.jobs TO coordinator_test;"
        ).await.unwrap();
        Self {
            _postgres: postgres,
            admin,
            runtime_url: format!("postgres://coordinator_test@{address}/postgres"),
        }
    }
    async fn service(&self) -> Coordinator {
        self.options(Options::default()).await
    }
    async fn options(&self, options: Options) -> Coordinator {
        Coordinator::connect(&self.runtime_url, options)
            .await
            .unwrap()
    }
    async fn stored_ids(&self) -> StoredIds {
        let rows = self.admin.query(
            "SELECT 'workers' AS kind,id AS scope,'' AS subject,id FROM workflow_manager.workers
             UNION ALL SELECT 'queue_scopes',id,'',id FROM workflow_manager.queue_scopes
             UNION ALL SELECT 'assignments',app_id,worker_id,id FROM workflow_manager.assignments
             UNION ALL SELECT 'placement_receipts',app_id,request_id,id FROM workflow_manager.placement_receipts
             UNION ALL SELECT 'management',app_id,request_id,id FROM workflow_manager.management",
            &[],
        ).await.unwrap();
        assert!(!rows.is_empty());
        let mut stored = BTreeMap::new();
        let mut unique = BTreeSet::new();
        for row in rows {
            let kind: String = row.get("kind");
            let scope: String = row.get("scope");
            let subject: String = row.get("subject");
            let id: String = row.get("id");
            match kind.as_str() {
                "workers" | "queue_scopes" => assert_eq!(id, scope),
                "assignments" => assert!(typed_id::parse_with_prefix(&id, "wca").is_ok()),
                "placement_receipts" => assert!(typed_id::parse_with_prefix(&id, "wcp").is_ok()),
                "management" => assert!(typed_id::parse_with_prefix(&id, "wcm").is_ok()),
                _ => panic!("unexpected metadata table"),
            }
            assert!(unique.insert(id.clone()));
            assert!(stored.insert((kind, scope, subject), id).is_none());
        }
        stored
    }
}
fn assert_ids_retained(before: &StoredIds, after: &StoredIds) {
    assert!(!before.is_empty());
    for (key, id) in before {
        assert_eq!(
            after.get(key),
            Some(id),
            "storage identity changed: {key:?}"
        );
    }
}
async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls).await.unwrap();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}
async fn register_worker(service: &Coordinator, capacity: u32) -> WorkerId {
    let worker = WorkerId::mint();
    service
        .manager
        .register(
            &worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(capacity).unwrap(),
                state: WorkerState::Ready,
            },
        )
        .await
        .unwrap();
    worker
}
fn assignment_request(app: &AppId, worker: &WorkerId) -> AssignScope {
    AssignScope {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        worker_id: worker.clone(),
        expected_revision: None,
    }
}
fn assigned(assignment: &Assignment) -> AssignedScope {
    AssignedScope {
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
    }
}
fn wake(assignment: &Assignment, revision: i64) -> PublishWakeHint {
    PublishWakeHint {
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
        revision: revision.try_into().unwrap(),
        next_due_at: None,
    }
}
fn release(assignment: &Assignment, revision: i64) -> ReleaseScope {
    ReleaseScope {
        request_id: RequestId::mint(),
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
        wake_revision: revision.try_into().unwrap(),
    }
}
fn command(app: &AppId) -> ManageRun {
    ManageRun {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        run_id: RunId::mint(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Pause,
        },
    }
}

#[compio::test]
async fn replicas_fence_placement_retries_and_capacity() {
    let fixture = Fixture::new().await;
    let a = fixture.service().await;
    let b = fixture.service().await;
    let worker = register_worker(&a, 1).await;
    let app = AppId::mint();
    let request = assignment_request(&app, &worker);
    let (first, second) = futures::join!(a.manager.assign(&request), b.manager.assign(&request));
    let assignment = first.unwrap();
    assert_eq!(assignment, second.unwrap());
    let initial_ids = fixture.stored_ids().await;
    b.manager
        .register(
            &worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(1).unwrap(),
                state: WorkerState::Ready,
            },
        )
        .await
        .unwrap();
    assert_eq!(fixture.stored_ids().await, initial_ids);
    let mut conflict = request.clone();
    conflict.expected_revision = Some(assignment.revision);
    assert_eq!(b.manager.assign(&conflict).await, Err(Error::Conflict));
    assert_eq!(
        b.manager.assign(&assignment_request(&app, &worker)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        b.manager
            .assign(&assignment_request(&AppId::mint(), &worker))
            .await,
        Err(Error::Capacity)
    );
    let next_request = AssignScope {
        request_id: RequestId::mint(),
        expected_revision: Some(assignment.revision),
        ..request.clone()
    };
    let replacement = b.manager.assign(&next_request).await.unwrap();
    assert!(replacement.revision > assignment.revision);
    assert_ids_retained(&initial_ids, &fixture.stored_ids().await);
    assert_eq!(
        a.manager.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Denied)
    );
    assert_eq!(a.manager.assign(&request).await.unwrap(), assignment);
    assert_eq!(
        a.manager.assignments(&worker, None).await.unwrap(),
        vec![replacement]
    );

    let spare = register_worker(&a, 1).await;
    let left = assignment_request(&AppId::mint(), &spare);
    let right = assignment_request(&AppId::mint(), &spare);
    let (left, right) = futures::join!(a.manager.assign(&left), b.manager.assign(&right));
    assert!(matches!(
        (&left, &right),
        (Ok(_), Err(Error::Capacity)) | (Err(Error::Capacity), Ok(_))
    ));
    assert_eq!(a.manager.assignments(&spare, None).await.unwrap().len(), 1);
}
#[compio::test]
async fn wake_hints_and_release_require_current_ownership_and_a_responsible_peer() {
    let fixture = Fixture::new().await;
    let a = fixture.service().await;
    let b = fixture.service().await;
    let w1 = register_worker(&a, 2).await;
    let w2 = register_worker(&a, 2).await;
    let app = AppId::mint();
    let first = a
        .manager
        .assign(&assignment_request(&app, &w1))
        .await
        .unwrap();
    let first_release = release(&first, 1);
    assert_eq!(
        a.manager.release(&w1, &first_release).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        a.manager.publish_wake(&w2, &wake(&first, 1)).await,
        Err(Error::Denied)
    );
    let hint = a.manager.publish_wake(&w1, &wake(&first, 1)).await.unwrap();
    assert_eq!(
        b.manager.publish_wake(&w1, &wake(&first, 1)).await.unwrap(),
        hint
    );
    assert_eq!(
        b.manager.release(&w1, &first_release).await,
        Err(Error::Conflict)
    );
    let mut changed = wake(&first, 1);
    changed.next_due_at = Some(10.try_into().unwrap());
    assert_eq!(
        b.manager.publish_wake(&w1, &changed).await,
        Err(Error::Conflict)
    );
    a.manager.publish_wake(&w1, &wake(&first, 2)).await.unwrap();
    assert_eq!(
        b.manager.publish_wake(&w1, &wake(&first, 1)).await,
        Err(Error::Conflict)
    );
    let second = b
        .manager
        .assign(&assignment_request(&app, &w2))
        .await
        .unwrap();
    b.manager
        .publish_wake(&w2, &wake(&second, 1))
        .await
        .unwrap();
    assert_eq!(
        a.manager.release(&w1, &first_release).await,
        Err(Error::Conflict)
    );
    let r1 = release(&first, 2);
    let r2 = release(&second, 1);
    let (released1, released2) =
        futures::join!(a.manager.release(&w1, &r1), b.manager.release(&w2, &r2));
    assert!(matches!(
        (&released1, &released2),
        (Ok(()), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(()))
    ));
    let (old, worker, receipt) = if released1.is_ok() {
        (&first, &w1, &r1)
    } else {
        (&second, &w2, &r2)
    };
    let released_ids = fixture.stored_ids().await;
    assert_eq!(b.manager.release(worker, receipt).await, Ok(()));
    assert_eq!(fixture.stored_ids().await, released_ids);
    assert_eq!(
        a.manager.renew(worker, &assigned(old)).await,
        Err(Error::Denied)
    );
    assert_eq!(
        a.manager.publish_wake(worker, &wake(old, 3)).await,
        Err(Error::Denied)
    );
    let request = AssignScope {
        expected_revision: Some(old.revision),
        ..assignment_request(&app, worker)
    };
    let replacement = a.manager.assign(&request).await.unwrap();
    assert!(replacement.revision > old.revision);
    assert_ids_retained(&released_ids, &fixture.stored_ids().await);
    assert_eq!(b.manager.release(worker, receipt).await, Ok(()));
    assert_eq!(
        b.manager.assignments(worker, None).await.unwrap(),
        vec![replacement.clone()]
    );
    assert_eq!(
        b.manager.publish_wake(worker, &wake(old, 4)).await,
        Err(Error::Denied)
    );
    a.manager
        .publish_wake(worker, &wake(&replacement, 1))
        .await
        .unwrap();

    let foreign = AppId::mint();
    let foreign_assignment = a
        .manager
        .assign(&assignment_request(&foreign, &w2))
        .await
        .unwrap();
    assert_eq!(
        a.manager
            .pending_management(&w1, &assigned(&foreign_assignment))
            .await,
        Err(Error::Denied)
    );
}

#[compio::test]
async fn lost_assignments_require_rescan_without_published_wake_hints() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 1).await;
    let app = AppId::mint();
    let assignment = service
        .manager
        .assign(&assignment_request(&app, &worker))
        .await
        .unwrap();
    assert!(service
        .manager
        .recovery_scopes(None)
        .await
        .unwrap()
        .is_empty());
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET expires_at=0 WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        service.manager.recovery_scopes(None).await.unwrap(),
        vec![app.clone()]
    );
    service
        .manager
        .register(
            &worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(1).unwrap(),
                state: WorkerState::Ready,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        service.manager.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Denied)
    );
    assert!(service
        .manager
        .assignments(&worker, None)
        .await
        .unwrap()
        .is_empty());
    let replacement = service
        .manager
        .assign(&AssignScope {
            expected_revision: Some(assignment.revision),
            ..assignment_request(&app, &worker)
        })
        .await
        .unwrap();
    assert!(service
        .manager
        .recovery_scopes(None)
        .await
        .unwrap()
        .is_empty());
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.workers SET expires_at=0 WHERE id=$1",
            &[&worker.as_str()],
        )
        .await
        .unwrap();
    assert!(service
        .manager
        .ready_workers(None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        service
            .manager
            .publish_wake(&worker, &wake(&replacement, 1))
            .await,
        Err(Error::Denied)
    );
    assert_eq!(
        service.manager.recovery_scopes(None).await.unwrap(),
        vec![app]
    );
}

#[compio::test]
async fn management_is_durable_bounded_typed_and_assignment_scoped() {
    let fixture = Fixture::new().await;
    let options = Options {
        batch_limit: 1,
        max_pending_management: 2,
        ..Options::default()
    };
    let a = fixture.options(options).await;
    let b = fixture.options(options).await;
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let app = AppId::mint();
    let one = command(&app);
    assert_eq!(
        a.manager
            .manage(&service_issuer(WORKER_SERVICE_NAME).unwrap(), &one)
            .await,
        Err(Error::Denied)
    );
    let (left, right) = futures::join!(
        a.manager.manage(&actor, &one),
        b.manager.manage(&actor, &one)
    );
    let receipt = left.unwrap();
    assert_eq!(receipt, right.unwrap());
    let initial_ids = fixture.stored_ids().await;
    assert_eq!(
        a.manager.recovery_scopes(None).await.unwrap(),
        vec![app.clone()]
    );
    let mut changed = one.clone();
    changed.run_id = RunId::mint();
    assert_eq!(
        b.manager.manage(&actor, &changed).await,
        Err(Error::Conflict)
    );
    let mut two = command(&app);
    two.command = ManagementOperation::Restart {
        options: RestartOptions {
            from: Some(RestartTarget {
                name: "checkpoint".into(),
                occurrence: Some(0),
            }),
            deploy: Some(RestartDeploy::Started),
        },
    };
    a.manager.manage(&actor, &two).await.unwrap();
    assert_eq!(
        a.manager.manage(&actor, &command(&app)).await,
        Err(Error::Capacity)
    );
    let worker = register_worker(&a, 1).await;
    let assigned_request = assignment_request(&app, &worker);
    let assignment = a.manager.assign(&assigned_request).await.unwrap();
    let scope = assigned(&assignment);
    assert_eq!(
        b.manager.pending_management(&worker, &scope).await.unwrap(),
        vec![one.clone()]
    );
    let ack = AcknowledgeManagement {
        request_id: one.request_id.clone(),
        app_id: app.clone(),
        assignment_revision: assignment.revision,
        outcome: ManagementOutcome::Applied {
            state: RunState::Paused,
        },
    };
    assert_eq!(
        a.manager
            .acknowledge_management(&WorkerId::mint(), &ack)
            .await,
        Err(Error::Denied)
    );
    let receipt = a
        .manager
        .acknowledge_management(&worker, &ack)
        .await
        .unwrap();
    assert_eq!(
        b.manager
            .acknowledge_management(&worker, &ack)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(b.manager.manage(&actor, &one).await.unwrap(), receipt);
    assert_ids_retained(&initial_ids, &fixture.stored_ids().await);
    assert_eq!(
        b.manager
            .management_receipt(&app, &one.request_id)
            .await
            .unwrap(),
        Some(receipt)
    );
    assert_eq!(
        b.manager.pending_management(&worker, &scope).await.unwrap(),
        vec![two.clone()]
    );
    let mut conflicting_ack = ack.clone();
    conflicting_ack.outcome = ManagementOutcome::NotFound {};
    assert_eq!(
        b.manager
            .acknowledge_management(&worker, &conflicting_ack)
            .await,
        Err(Error::Conflict)
    );
    let mut foreign = ack.clone();
    foreign.app_id = AppId::mint();
    assert_eq!(
        a.manager.acknowledge_management(&worker, &foreign).await,
        Err(Error::Denied)
    );
    let renewed = a
        .manager
        .assign(&AssignScope {
            request_id: RequestId::mint(),
            expected_revision: Some(assignment.revision),
            ..assigned_request
        })
        .await
        .unwrap();
    assert_eq!(
        b.manager.acknowledge_management(&worker, &ack).await,
        Err(Error::Denied)
    );
    drop(a);
    drop(b);
    let reopened = fixture.options(options).await;
    assert_eq!(
        reopened
            .manager
            .pending_management(&worker, &assigned(&renewed))
            .await
            .unwrap(),
        vec![two]
    );
    let mut invalid = command(&app);
    invalid.command = ManagementOperation::Restart {
        options: RestartOptions {
            from: Some(RestartTarget {
                name: "checkpoint".into(),
                occurrence: None,
            }),
            deploy: Some(RestartDeploy::Latest),
        },
    };
    assert_eq!(
        reopened.manager.manage(&actor, &invalid).await,
        Err(Error::Invalid)
    );
}

#[compio::test]
async fn lock_waits_cannot_extend_authority_and_timeout_sessions_are_reusable() {
    let mut fixture = Fixture::new().await;
    let normal = fixture.service().await;
    let worker = register_worker(&normal, 1).await;
    let app = AppId::mint();
    let assignment = normal
        .manager
        .assign(&assignment_request(&app, &worker))
        .await
        .unwrap();
    let options = Options {
        connections: 1,
        command_timeout: Duration::from_millis(80),
        ..Options::default()
    };
    let bounded = fixture.options(options).await;
    let lock = fixture.admin.transaction().await.unwrap();
    lock.query(
        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
        &[&app.as_str()],
    )
    .await
    .unwrap();
    assert_eq!(
        bounded.manager.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Timeout)
    );
    lock.rollback().await.unwrap();
    bounded.verify().await.unwrap();
    bounded
        .manager
        .renew(&worker, &assigned(&assignment))
        .await
        .unwrap();

    fixture.admin.execute(
        "UPDATE workflow_manager.assignments SET expires_at=floor(extract(epoch FROM clock_timestamp())*1000)::bigint+200 WHERE app_id=$1",
        &[&app.as_str()],
    ).await.unwrap();

    let lock = fixture.admin.transaction().await.unwrap();
    lock.query(
        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
        &[&app.as_str()],
    )
    .await
    .unwrap();
    let request = assigned(&assignment);
    let (attempt, ()) = futures::join!(normal.manager.renew(&worker, &request), async {
        compio::time::sleep(Duration::from_millis(300)).await;
        lock.commit().await.unwrap();
    });
    assert_eq!(attempt, Err(Error::Denied));
}

#[compio::test]
async fn metadata_schema_has_no_customer_authority_and_ids_are_bytewise() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let runtime = connect(&fixture.runtime_url).await;
    fixture
        .admin
        .batch_execute(
            "CREATE ROLE customer_reader;
         GRANT USAGE ON SCHEMA customer TO customer_reader;
         GRANT SELECT ON customer.__zeroship_workflow_history TO customer_reader;
         GRANT customer_reader TO coordinator_test WITH INHERIT FALSE;",
        )
        .await
        .unwrap();
    let privileges = runtime
        .query_one(
            "SELECT pg_has_role(current_user,'customer_reader','USAGE'),
                pg_has_role(current_user,'customer_reader','SET')",
            &[],
        )
        .await
        .unwrap();
    assert!(!privileges.get::<_, bool>(0));
    assert!(privileges.get::<_, bool>(1));
    assert_eq!(service.verify().await, Err(HostError::Unavailable));
    fixture
        .admin
        .batch_execute("REVOKE customer_reader FROM coordinator_test")
        .await
        .unwrap();
    service.verify().await.unwrap();
    assert!(runtime
        .query("SELECT * FROM customer.__zeroship_workflow_history", &[])
        .await
        .is_err());
    assert!(runtime
        .batch_execute("CREATE TABLE workflow_manager.injected(id text PRIMARY KEY,data jsonb)")
        .await
        .is_err());
    assert!(runtime
        .batch_execute("UPDATE workflow_manager.schema_version SET fingerprint='forged'")
        .await
        .is_err());
    let columns: BTreeMap<String, Vec<String>> = serde_json::from_str(include_str!(
        "../../zeroship-workflow-manager/schema/identity-columns.json"
    ))
    .unwrap();
    assert!(!columns.is_empty());
    for (table, columns) in columns {
        assert!(!columns.is_empty());
        let primary = fixture
            .admin
            .query_one(
                "SELECT ARRAY(
               SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY AS key(attnum,position)
               JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=key.attnum
               ORDER BY key.position
             ) AS columns FROM pg_constraint c
             JOIN pg_class t ON t.oid=c.conrelid JOIN pg_namespace n ON n.oid=t.relnamespace
             WHERE n.nspname='workflow_manager' AND t.relname=$1 AND c.contype='p'",
                &[&table],
            )
            .await
            .unwrap();
        assert_eq!(primary.get::<_, Vec<String>>("columns"), ["id"], "{table}");
        for column in columns {
            let row = fixture.admin.query_one(
                "SELECT c.collname FROM pg_attribute a JOIN pg_class t ON t.oid=a.attrelid
                 JOIN pg_namespace n ON n.oid=t.relnamespace JOIN pg_collation c ON c.oid=a.attcollation
                 WHERE n.nspname='workflow_manager' AND t.relname=$1 AND a.attname=$2", &[&table,&column],
            ).await.unwrap();
            assert_eq!(row.get::<_, &str>(0), "C", "{table}.{column}");
        }
    }
    let rows = fixture.admin.query(
        "SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='workflow_manager'
         AND (data_type IN ('json','jsonb','bytea') OR column_name IN ('input','output','history','payload_url','database_url','task_token'))", &[],
    ).await.unwrap();
    assert!(rows.is_empty());
    let scopes = fixture.admin.query(
        "SELECT relation.relname, target.relname, ARRAY( \
           SELECT attribute.attname::text FROM unnest(constraint_row.confkey) WITH ORDINALITY AS key(attnum,position) \
           JOIN pg_attribute attribute ON attribute.attrelid=target.oid AND attribute.attnum=key.attnum \
           ORDER BY key.position) AS target_columns FROM pg_constraint constraint_row \
         JOIN pg_class relation ON relation.oid=constraint_row.conrelid \
         JOIN pg_class target ON target.oid=constraint_row.confrelid \
         JOIN pg_namespace namespace ON namespace.oid=relation.relnamespace \
         WHERE namespace.nspname='workflow_manager' \
           AND constraint_row.conname IN ('assignment_scope','assignment_worker','receipt_scope','management_scope','jobs_app_id_fkey') \
         ORDER BY relation.relname,target.relname", &[],
    ).await.unwrap();
    assert_eq!(
        scopes
            .iter()
            .map(|row| (
                row.get::<_, String>(0),
                row.get::<_, String>(1),
                row.get::<_, Vec<String>>(2)
            ))
            .collect::<Vec<_>>(),
        [
            ("assignments", "queue_scopes"),
            ("assignments", "workers"),
            ("jobs", "queue_scopes"),
            ("management", "queue_scopes"),
            ("placement_receipts", "queue_scopes"),
        ]
        .map(|(table, target)| (
            table.to_owned(),
            target.to_owned(),
            vec!["id".to_owned()]
        ))
    );
    fixture
        .admin
        .batch_execute("UPDATE workflow_manager.schema_version SET fingerprint='stale'")
        .await
        .unwrap();
    assert_eq!(service.verify().await, Err(HostError::Unavailable));
    assert!(matches!(
        Coordinator::connect(&fixture.runtime_url, Options::default()).await,
        Err(HostError::Unavailable)
    ));
}

#[compio::test]
async fn assignment_verification_preserves_leases_and_fences_app_authority() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 2).await;
    let app = AppId::mint();
    let assignment = service
        .manager
        .assign(&assignment_request(&app, &worker))
        .await
        .unwrap();
    let request = VerifyAssignment {
        app_id: app.clone(),
        worker_id: worker.clone(),
        assignment_revision: assignment.revision,
    };
    for worker_is_shorter in [false, true] {
        let (assignment_ttl, worker_ttl) = if worker_is_shorter {
            (60_000_i64, 30_000_i64)
        } else {
            (30_000, 60_000)
        };
        fixture.admin.execute(
            "UPDATE workflow_manager.assignments SET expires_at=floor(extract(epoch FROM clock_timestamp())*1000)::bigint+$2 WHERE app_id=$1",
            &[&app.as_str(), &assignment_ttl],
        ).await.unwrap();
        fixture.admin.execute(
            "UPDATE workflow_manager.workers SET state='draining',expires_at=floor(extract(epoch FROM clock_timestamp())*1000)::bigint+$2 WHERE id=$1",
            &[&worker.as_str(), &worker_ttl],
        ).await.unwrap();
        let snapshot = || async {
            fixture.admin.query_one(
                "SELECT to_jsonb(a)::text AS assignment,to_jsonb(w)::text AS worker,
                 LEAST(a.expires_at,w.expires_at) AS deadline
                 FROM workflow_manager.assignments a JOIN workflow_manager.workers w ON w.id=a.worker_id
                 WHERE a.app_id=$1 AND a.worker_id=$2", &[&app.as_str(), &worker.as_str()],
            ).await.unwrap()
        };
        let before = snapshot().await;
        let result = service.manager.verify_assignment(&request).await.unwrap();
        assert_eq!(result.app_id, app);
        assert_eq!(result.worker_id, worker);
        assert_eq!(result.revision, assignment.revision);
        assert_eq!(result.expires_at.get(), before.get::<_, i64>("deadline"));
        assert_eq!(
            service.manager.verify_assignment(&request).await.unwrap(),
            result
        );
        let after = snapshot().await;
        for column in ["assignment", "worker"] {
            assert_eq!(
                before.get::<_, String>(column),
                after.get::<_, String>(column)
            );
        }
    }
    for foreign in [
        VerifyAssignment {
            app_id: AppId::mint(),
            ..request.clone()
        },
        VerifyAssignment {
            worker_id: WorkerId::mint(),
            ..request.clone()
        },
        VerifyAssignment {
            assignment_revision: (assignment.revision.get() + 1).try_into().unwrap(),
            ..request.clone()
        },
    ] {
        assert_eq!(
            service.manager.verify_assignment(&foreign).await,
            Err(Error::Denied)
        );
    }
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET released=true WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        service.manager.verify_assignment(&request).await,
        Err(Error::Denied)
    );
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET released=false,expires_at=0 WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        service.manager.verify_assignment(&request).await,
        Err(Error::Denied)
    );
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET expires_at=$2 WHERE app_id=$1",
            &[&app.as_str(), &i64::MAX],
        )
        .await
        .unwrap();
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.workers SET expires_at=0 WHERE id=$1",
            &[&worker.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        service.manager.verify_assignment(&request).await,
        Err(Error::Denied)
    );
}

#[compio::test]
async fn assignment_verification_checks_expiry_after_waiting_for_scope_lock() {
    let mut fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 1).await;
    let app = AppId::mint();
    let assignment = service
        .manager
        .assign(&assignment_request(&app, &worker))
        .await
        .unwrap();
    let request = VerifyAssignment {
        app_id: app.clone(),
        worker_id: worker.clone(),
        assignment_revision: assignment.revision,
    };
    fixture.admin.execute(
        "UPDATE workflow_manager.assignments SET expires_at=floor(extract(epoch FROM clock_timestamp())*1000)::bigint+200 WHERE app_id=$1",
        &[&app.as_str()],
    ).await.unwrap();
    let lock = fixture.admin.transaction().await.unwrap();
    lock.query(
        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
        &[&app.as_str()],
    )
    .await
    .unwrap();
    let (result, ()) = futures::join!(service.manager.verify_assignment(&request), async {
        compio::time::sleep(Duration::from_millis(300)).await;
        lock.commit().await.unwrap();
    });
    assert_eq!(result, Err(Error::Denied));
    service.verify().await.unwrap();
}
