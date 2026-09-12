#![allow(
    clippy::future_not_send,
    reason = "fixtures use compio pools on their owning test runtime"
)]

use compio_postgres::{Client, NoTls};
use std::{collections::BTreeMap, num::NonZeroU32, time::Duration};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    workflow_coordination::*,
};
use zeroship_workflow_server::coordinator::{Coordinator, Error, Options, SCHEMA_SQL};

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
             CREATE SCHEMA workflow_coordination;
             CREATE SCHEMA customer;
             CREATE TABLE customer.__zeroship_workflow_history(secret text);
             INSERT INTO customer.__zeroship_workflow_history VALUES('customer-private-history');
             REVOKE ALL ON SCHEMA customer FROM PUBLIC;",
            )
            .await
            .unwrap();
        admin.batch_execute(SCHEMA_SQL).await.unwrap();
        admin.batch_execute(
            "GRANT USAGE ON SCHEMA workflow_coordination TO coordinator_test;
             GRANT SELECT ON workflow_coordination.schema_version TO coordinator_test;
             GRANT SELECT,INSERT,UPDATE,DELETE ON workflow_coordination.workers,
               workflow_coordination.scopes,workflow_coordination.assignments,
               workflow_coordination.placement_receipts,workflow_coordination.management TO coordinator_test;"
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
    let (first, second) = futures::join!(a.assign(&request), b.assign(&request));
    let assignment = first.unwrap();
    assert_eq!(assignment, second.unwrap());
    let mut conflict = request.clone();
    conflict.expected_revision = Some(assignment.revision);
    assert_eq!(b.assign(&conflict).await, Err(Error::Conflict));
    assert_eq!(
        b.assign(&assignment_request(&app, &worker)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        b.assign(&assignment_request(&AppId::mint(), &worker)).await,
        Err(Error::Capacity)
    );
    let next_request = AssignScope {
        request_id: RequestId::mint(),
        expected_revision: Some(assignment.revision),
        ..request.clone()
    };
    let replacement = b.assign(&next_request).await.unwrap();
    assert!(replacement.revision > assignment.revision);
    assert_eq!(
        a.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Denied)
    );
    assert_eq!(a.assign(&request).await.unwrap(), assignment);
    assert_eq!(
        a.assignments(&worker, None).await.unwrap(),
        vec![replacement]
    );

    let spare = register_worker(&a, 1).await;
    let left = assignment_request(&AppId::mint(), &spare);
    let right = assignment_request(&AppId::mint(), &spare);
    let (left, right) = futures::join!(a.assign(&left), b.assign(&right));
    assert!(matches!(
        (&left, &right),
        (Ok(_), Err(Error::Capacity)) | (Err(Error::Capacity), Ok(_))
    ));
    assert_eq!(a.assignments(&spare, None).await.unwrap().len(), 1);
}
#[compio::test]
async fn wake_hints_and_release_require_current_ownership_and_a_responsible_peer() {
    let fixture = Fixture::new().await;
    let a = fixture.service().await;
    let b = fixture.service().await;
    let w1 = register_worker(&a, 2).await;
    let w2 = register_worker(&a, 2).await;
    let app = AppId::mint();
    let first = a.assign(&assignment_request(&app, &w1)).await.unwrap();
    let first_release = release(&first, 1);
    assert_eq!(a.release(&w1, &first_release).await, Err(Error::Conflict));
    assert_eq!(
        a.publish_wake(&w2, &wake(&first, 1)).await,
        Err(Error::Denied)
    );
    let hint = a.publish_wake(&w1, &wake(&first, 1)).await.unwrap();
    assert_eq!(b.publish_wake(&w1, &wake(&first, 1)).await.unwrap(), hint);
    assert_eq!(b.release(&w1, &first_release).await, Err(Error::Conflict));
    let mut changed = wake(&first, 1);
    changed.next_due_at = Some(10.try_into().unwrap());
    assert_eq!(b.publish_wake(&w1, &changed).await, Err(Error::Conflict));
    a.publish_wake(&w1, &wake(&first, 2)).await.unwrap();
    assert_eq!(
        b.publish_wake(&w1, &wake(&first, 1)).await,
        Err(Error::Conflict)
    );
    let second = b.assign(&assignment_request(&app, &w2)).await.unwrap();
    b.publish_wake(&w2, &wake(&second, 1)).await.unwrap();
    assert_eq!(a.release(&w1, &first_release).await, Err(Error::Conflict));
    let r1 = release(&first, 2);
    let r2 = release(&second, 1);
    let (released1, released2) = futures::join!(a.release(&w1, &r1), b.release(&w2, &r2));
    assert!(matches!(
        (&released1, &released2),
        (Ok(()), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(()))
    ));
    let (old, worker, receipt) = if released1.is_ok() {
        (&first, &w1, &r1)
    } else {
        (&second, &w2, &r2)
    };
    assert_eq!(b.release(worker, receipt).await, Ok(()));
    assert_eq!(a.renew(worker, &assigned(old)).await, Err(Error::Denied));
    assert_eq!(
        a.publish_wake(worker, &wake(old, 3)).await,
        Err(Error::Denied)
    );
    let request = AssignScope {
        expected_revision: Some(old.revision),
        ..assignment_request(&app, worker)
    };
    let replacement = a.assign(&request).await.unwrap();
    assert!(replacement.revision > old.revision);
    assert_eq!(b.release(worker, receipt).await, Ok(()));
    assert_eq!(
        b.assignments(worker, None).await.unwrap(),
        vec![replacement.clone()]
    );
    assert_eq!(
        b.publish_wake(worker, &wake(old, 4)).await,
        Err(Error::Denied)
    );
    a.publish_wake(worker, &wake(&replacement, 1))
        .await
        .unwrap();

    let foreign = AppId::mint();
    let foreign_assignment = a.assign(&assignment_request(&foreign, &w2)).await.unwrap();
    assert_eq!(
        a.pending_management(&w1, &assigned(&foreign_assignment))
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
        .assign(&assignment_request(&app, &worker))
        .await
        .unwrap();
    assert!(service.recovery_scopes(None).await.unwrap().is_empty());
    fixture
        .admin
        .execute(
            "UPDATE workflow_coordination.assignments SET expires_at=0 WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        service.recovery_scopes(None).await.unwrap(),
        vec![app.clone()]
    );
    service
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
        service.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Denied)
    );
    assert!(service.assignments(&worker, None).await.unwrap().is_empty());
    let replacement = service
        .assign(&AssignScope {
            expected_revision: Some(assignment.revision),
            ..assignment_request(&app, &worker)
        })
        .await
        .unwrap();
    assert!(service.recovery_scopes(None).await.unwrap().is_empty());
    fixture
        .admin
        .execute(
            "UPDATE workflow_coordination.workers SET expires_at=0 WHERE worker_id=$1",
            &[&worker.as_str()],
        )
        .await
        .unwrap();
    assert!(service.ready_workers(None).await.unwrap().is_empty());
    assert_eq!(
        service.publish_wake(&worker, &wake(&replacement, 1)).await,
        Err(Error::Denied)
    );
    assert_eq!(service.recovery_scopes(None).await.unwrap(), vec![app]);
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
        a.manage(&service_issuer(WORKER_SERVICE_NAME).unwrap(), &one)
            .await,
        Err(Error::Denied)
    );
    let (left, right) = futures::join!(a.manage(&actor, &one), b.manage(&actor, &one));
    let receipt = left.unwrap();
    assert_eq!(receipt, right.unwrap());
    assert_eq!(a.recovery_scopes(None).await.unwrap(), vec![app.clone()]);
    let mut changed = one.clone();
    changed.run_id = RunId::mint();
    assert_eq!(b.manage(&actor, &changed).await, Err(Error::Conflict));
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
    a.manage(&actor, &two).await.unwrap();
    assert_eq!(a.manage(&actor, &command(&app)).await, Err(Error::Capacity));
    let worker = register_worker(&a, 1).await;
    let assigned_request = assignment_request(&app, &worker);
    let assignment = a.assign(&assigned_request).await.unwrap();
    let scope = assigned(&assignment);
    assert_eq!(
        b.pending_management(&worker, &scope).await.unwrap(),
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
        a.acknowledge_management(&WorkerId::mint(), &ack).await,
        Err(Error::Denied)
    );
    let receipt = a.acknowledge_management(&worker, &ack).await.unwrap();
    assert_eq!(
        b.acknowledge_management(&worker, &ack).await.unwrap(),
        receipt
    );
    assert_eq!(b.manage(&actor, &one).await.unwrap(), receipt);
    assert_eq!(
        b.management_receipt(&app, &one.request_id).await.unwrap(),
        Some(receipt)
    );
    assert_eq!(
        b.pending_management(&worker, &scope).await.unwrap(),
        vec![two.clone()]
    );
    let mut conflicting_ack = ack.clone();
    conflicting_ack.outcome = ManagementOutcome::NotFound {};
    assert_eq!(
        b.acknowledge_management(&worker, &conflicting_ack).await,
        Err(Error::Conflict)
    );
    let mut foreign = ack.clone();
    foreign.app_id = AppId::mint();
    assert_eq!(
        a.acknowledge_management(&worker, &foreign).await,
        Err(Error::Denied)
    );
    let renewed = a
        .assign(&AssignScope {
            request_id: RequestId::mint(),
            expected_revision: Some(assignment.revision),
            ..assigned_request
        })
        .await
        .unwrap();
    assert_eq!(
        b.acknowledge_management(&worker, &ack).await,
        Err(Error::Denied)
    );
    drop(a);
    drop(b);
    let reopened = fixture.options(options).await;
    assert_eq!(
        reopened
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
    assert_eq!(reopened.manage(&actor, &invalid).await, Err(Error::Invalid));
}

#[compio::test]
async fn lock_waits_cannot_extend_authority_and_timeout_sessions_are_reusable() {
    let mut fixture = Fixture::new().await;
    let normal = fixture.service().await;
    let worker = register_worker(&normal, 1).await;
    let app = AppId::mint();
    let assignment = normal
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
        "SELECT app_id FROM workflow_coordination.scopes WHERE app_id=$1 FOR UPDATE",
        &[&app.as_str()],
    )
    .await
    .unwrap();
    assert_eq!(
        bounded.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Unavailable)
    );
    lock.rollback().await.unwrap();
    bounded.verify().await.unwrap();
    bounded
        .renew(&worker, &assigned(&assignment))
        .await
        .unwrap();

    fixture.admin.execute(
        "UPDATE workflow_coordination.assignments SET expires_at=floor(extract(epoch FROM clock_timestamp())*1000)::bigint+200 WHERE app_id=$1",
        &[&app.as_str()],
    ).await.unwrap();

    let lock = fixture.admin.transaction().await.unwrap();
    lock.query(
        "SELECT app_id FROM workflow_coordination.scopes WHERE app_id=$1 FOR UPDATE",
        &[&app.as_str()],
    )
    .await
    .unwrap();
    let request = assigned(&assignment);
    let (attempt, ()) = futures::join!(normal.renew(&worker, &request), async {
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
    assert_eq!(service.verify().await, Err(Error::Unavailable));
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
        .batch_execute("CREATE TABLE workflow_coordination.injected(data jsonb)")
        .await
        .is_err());
    assert!(runtime
        .batch_execute("UPDATE workflow_coordination.schema_version SET fingerprint='forged'")
        .await
        .is_err());
    let columns: BTreeMap<String, Vec<String>> =
        serde_json::from_str(include_str!("../schema/identity-columns.json")).unwrap();
    assert!(!columns.is_empty());
    for (table, columns) in columns {
        assert!(!columns.is_empty());
        for column in columns {
            let row = fixture.admin.query_one(
                "SELECT c.collname FROM pg_attribute a JOIN pg_class t ON t.oid=a.attrelid
                 JOIN pg_namespace n ON n.oid=t.relnamespace JOIN pg_collation c ON c.oid=a.attcollation
                 WHERE n.nspname='workflow_coordination' AND t.relname=$1 AND a.attname=$2", &[&table,&column],
            ).await.unwrap();
            assert_eq!(row.get::<_, &str>(0), "C", "{table}.{column}");
        }
    }
    let rows = fixture.admin.query(
        "SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='workflow_coordination'
         AND (data_type IN ('json','jsonb','bytea') OR column_name IN ('input','output','history','payload_url','database_url','task_token'))", &[],
    ).await.unwrap();
    assert!(rows.is_empty());
    fixture
        .admin
        .batch_execute("UPDATE workflow_coordination.schema_version SET fingerprint='stale'")
        .await
        .unwrap();
    assert_eq!(service.verify().await, Err(Error::Unavailable));
    assert!(matches!(
        Coordinator::connect(&fixture.runtime_url, Options::default()).await,
        Err(Error::Unavailable)
    ));
}
