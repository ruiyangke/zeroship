#![allow(
    clippy::future_not_send,
    reason = "fixtures use compio pools on their owning test runtime"
)]

use compio_postgres::{Client, NoTls};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroUsize},
    rc::Rc,
    time::Duration,
};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    typed_id,
    workflow_coordination::*,
    workflow_jobs::{
        JobOperation, JobOutcome, JobReceipt, JournalSettlement, ManagementCommand,
    },
    workflow_policy::AppPolicy,
};
use zeroship_workflow::WorkflowServiceError;
use zeroship_workflow_manager::{
    app_facts::{AppFactsFuture, AppFactsSource},
    coordinator::Placed,
    maintenance::MaintenanceAuthority,
    policy::control::PolicyObservations,
    recovery::Options as RecoveryOptions,
    Error,
};
use zeroship_workflow_server::{
    coordinator::{connect_eligibility, Coordinator, Error as HostError, Options, SCHEMA_SQL},
    runs::RunService,
    server::connect_policies,
};

type StoredIds = BTreeMap<(String, String, String), String>;

use crate::support::{holds, platform, zone};

struct Fixture {
    _platform: platform::Platform,
    admin: Client,
    runtime_url: String,
}
impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::fresh_database().await;
        let admin = connect(platform.admin_url.as_str()).await;
        // The clone carries the migrated platform schema. This fixture's
        // subject is the coordinator's own generated schema, so it replaces
        // those schemas before installing it.
        admin
            .batch_execute(
                "DROP SCHEMA IF EXISTS workflow_manager CASCADE;
             DROP SCHEMA IF EXISTS zeroship CASCADE;
             DROP SCHEMA IF EXISTS customer CASCADE;
             CREATE SCHEMA workflow_manager;
             CREATE SCHEMA zeroship;
             CREATE TABLE zeroship.apps(id text PRIMARY KEY,execution_zone_id text,deleted_at timestamptz);
             CREATE SCHEMA customer;
             CREATE TABLE customer.__zeroship_workflow_history(id text PRIMARY KEY,secret text);
             REVOKE ALL ON SCHEMA customer FROM PUBLIC;",
            )
            .await
            .unwrap();
        // The coordinator login is cluster-global and every case creates the
        // same one, so the losers of the creation race absorb "already exists".
        let _ = admin
            .batch_execute("CREATE ROLE coordinator_test LOGIN PASSWORD 'coordinator_test'")
            .await;
        admin
            .execute(
                "INSERT INTO customer.__zeroship_workflow_history(id,secret) VALUES($1,'customer-private-history')",
                &[&typed_id::generate("wfh")],
            )
            .await
            .unwrap();
        admin.batch_execute(SCHEMA_SQL).await.unwrap();
        admin
            .batch_execute(zeroship_workflow_manager::deployments::POSTGRES_SCHEMA)
            .await
            .unwrap();
        admin.batch_execute(
            "GRANT USAGE ON SCHEMA workflow_manager,zeroship TO coordinator_test;
             GRANT SELECT(id,execution_zone_id,deleted_at) ON zeroship.apps TO coordinator_test;
             GRANT SELECT ON workflow_manager.schema_version TO coordinator_test;
             GRANT SELECT,INSERT,UPDATE,DELETE ON workflow_manager.workers,
               workflow_manager.queue_scopes,workflow_manager.deployment_holds,workflow_manager.assignments,
               workflow_manager.placement_receipts,workflow_manager.management,workflow_manager.management_scopes,workflow_manager.jobs,
               workflow_manager.schedule_deployments,workflow_manager.schedule_activations,
               workflow_manager.schedule_disables,workflow_manager.schedule_scopes,
               workflow_manager.schedules,workflow_manager.schedule_occurrences,
               workflow_manager.recovery_scopes,workflow_manager.recovery_duties,
               workflow_manager.capacity_demands,
               workflow_manager.capacity_targets TO coordinator_test;"
        ).await.unwrap();
        let runtime_url = platform.role_url("coordinator_test").to_string();
        Self {
            _platform: platform,
            admin,
            runtime_url,
        }
    }
    async fn service(&self) -> Coordinator {
        self.options(Options::default()).await
    }
    async fn options(&self, options: Options) -> Coordinator {
        Coordinator::connect(&self.runtime_url, options, holds::client(), zone::trusted())
            .await
            .unwrap()
    }
    async fn stored_ids(&self) -> StoredIds {
        let rows = self.admin.query(
            "SELECT 'workers' AS kind,id AS scope,'' AS subject,id FROM workflow_manager.workers
             UNION ALL SELECT 'queue_scopes',id,'',id FROM workflow_manager.queue_scopes
             UNION ALL SELECT 'assignments',app_id,worker_id,id FROM workflow_manager.assignments
             UNION ALL SELECT 'placement_receipts',app_id,request_id,id FROM workflow_manager.placement_receipts
             UNION ALL SELECT 'management',app_id,request_id,id FROM workflow_manager.management
             UNION ALL SELECT 'management_scopes',app_id,run_id,id FROM workflow_manager.management_scopes",
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
                "management" => assert!(typed_id::parse_with_prefix(&id, "wjb").is_ok()),
                "management_scopes" => assert!(typed_id::parse_with_prefix(&id, "wmo").is_ok()),
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
/// The manager selects an eligible worker for the app.
async fn place(service: &Coordinator, app: &AppId) -> Assignment {
    match service.manager.place(app).await.unwrap() {
        Placed::Assigned(assignment) => assignment,
        other => panic!("expected a placement: {other:?}"),
    }
}

fn assigned(assignment: &Assignment) -> AssignedScope {
    AssignedScope {
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
    }
}
fn release(assignment: &Assignment, reason: ReleaseReason) -> ReleaseScope {
    ReleaseScope {
        request_id: RequestId::mint(),
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
        reason,
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

/// Two replicas placing the same app converge on one placement, a second
/// visit reports it owned, and a full instance admits nothing more. Giving the
/// placement up advances its revision and retires the old authority.
#[compio::test]
async fn replicas_fence_placement_retries_and_capacity() {
    let fixture = Fixture::new().await;
    let a = fixture.service().await;
    let b = fixture.service().await;
    let worker = register_worker(&a, 1).await;
    let app = AppId::mint();
    let (first, second) = futures::join!(a.manager.place(&app), b.manager.place(&app));
    let assignment = match (first.unwrap(), second.unwrap()) {
        (Placed::Assigned(assignment), Placed::Owned)
        | (Placed::Owned, Placed::Assigned(assignment)) => assignment,
        results => panic!("racing visits must converge on one placement: {results:?}"),
    };
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
    assert_eq!(b.manager.place(&app).await.unwrap(), Placed::Owned);
    // The instance's one slot is taken, so another app finds no capacity.
    assert!(matches!(
        b.manager.place(&AppId::mint()).await.unwrap(),
        Placed::Unplaced(_)
    ));
    b.manager
        .release(&worker, &release(&assignment, ReleaseReason::Relinquished))
        .await
        .unwrap();
    let replacement = place(&b, &app).await;
    assert!(replacement.revision > assignment.revision);
    assert_ids_retained(&initial_ids, &fixture.stored_ids().await);
    // The placement was re-admitted, so the row is live at a HIGHER revision:
    // this instance is still the placed one and its revision moved, which is the
    // retryable case. The release-without-replacement case above stays `Denied`,
    // because there the grant is gone rather than superseded.
    assert_eq!(
        a.manager.renew(&worker, &assigned(&assignment)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        a.manager.assignments(&worker, None).await.unwrap(),
        vec![replacement]
    );

    let spare = register_worker(&a, 1).await;
    let (left, right) = (AppId::mint(), AppId::mint());
    let (left, right) = futures::join!(a.manager.place(&left), b.manager.place(&right));
    assert!(matches!(
        (&left, &right),
        (Ok(Placed::Assigned(_)), Ok(Placed::Unplaced(_)))
            | (Ok(Placed::Unplaced(_)), Ok(Placed::Assigned(_)))
    ));
    assert_eq!(a.manager.assignments(&spare, None).await.unwrap().len(), 1);
}
#[compio::test]
async fn release_needs_neither_a_wake_hint_nor_a_responsible_peer() {
    let fixture = Fixture::new().await;
    let a = fixture.service().await;
    let b = fixture.service().await;
    register_worker(&a, 1).await;
    register_worker(&a, 1).await;
    let (app, other) = (AppId::mint(), AppId::mint());
    // One slot per worker, so the two apps land on different instances.
    let first = place(&a, &app).await;
    let second = place(&b, &other).await;
    let (w1, w2) = (first.worker_id.clone(), second.worker_id.clone());
    assert_ne!(w1, w2);
    // A worker holding no placement of the app releases nothing.
    let stranger = register_worker(&a, 2).await;
    assert_eq!(
        a.manager
            .release(&stranger, &release(&first, ReleaseReason::Relinquished))
            .await,
        Err(Error::Denied)
    );
    let r1 = release(&first, ReleaseReason::Relinquished);
    let r2 = release(&second, ReleaseReason::Relinquished);
    // Every owner may release at once: recovery responsibility stays with the
    // manager, so no responsible peer has to remain.
    let (released1, released2) =
        futures::join!(a.manager.release(&w1, &r1), b.manager.release(&w2, &r2));
    assert_eq!((released1, released2), (Ok(()), Ok(())));
    let released_ids = fixture.stored_ids().await;
    assert_eq!(b.manager.release(&w1, &r1).await, Ok(()));
    assert_eq!(fixture.stored_ids().await, released_ids);
    assert_eq!(
        b.manager
            .release(
                &w1,
                &ReleaseScope {
                    reason: ReleaseReason::Refused,
                    ..r1.clone()
                }
            )
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        a.manager.renew(&w1, &assigned(&first)).await,
        Err(Error::Denied)
    );
    let replacement = place(&a, &app).await;
    assert!(replacement.revision > first.revision);
    assert_ids_retained(&released_ids, &fixture.stored_ids().await);
    assert_eq!(
        b.manager
            .assignments(&replacement.worker_id, None)
            .await
            .unwrap(),
        vec![replacement.clone()]
    );
    // A refused release tombstones the pair for this instance's life, so the
    // next selection never offers that instance the app again.
    let refused = replacement.worker_id.clone();
    a.manager
        .release(&refused, &release(&replacement, ReleaseReason::Refused))
        .await
        .unwrap();
    let next = place(&a, &app).await;
    assert_ne!(next.worker_id, refused);

    // An instance that holds no placement of an app cannot claim its work,
    // however live its own registration is.
    let foreign = place(&a, &AppId::mint()).await;
    let outsider = register_worker(&a, 1).await;
    assert_eq!(
        a.manager
            .claim_job(&outsider, &assigned(&foreign), Ok(AppPolicy::default().max_delivery_attempts), || async {
                Ok(outsider.clone())
            })
            .await
            .unwrap_err(),
        Error::Denied
    );
}

/// An expired placement leaves its app unowned, so the manager places it
/// again under a higher revision while the stale authority stays refused.
#[compio::test]
async fn expired_placements_leave_the_app_unowned_and_replaceable() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 1).await;
    let app = AppId::mint();
    let assignment = place(&service, &app).await;
    assert!(service.manager.owned(&app).await.unwrap());
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET expires_at=0 WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert!(!service.manager.owned(&app).await.unwrap());
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
    let replacement = place(&service, &app).await;
    assert!(replacement.revision > assignment.revision);
    assert!(service.manager.owned(&app).await.unwrap());
    // An expired registration is neither a candidate nor an owner.
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
    assert!(!service.manager.owned(&app).await.unwrap());
    assert!(matches!(
        service.manager.place(&app).await.unwrap(),
        Placed::Unplaced(_)
    ));
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
    // Enqueued management is a maintenance row, which the lane claims without a
    // placement, so accepting it leaves the app with no owner.
    assert!(!a.manager.owned(&app).await.unwrap());
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
        deployment: None,
    };
    a.manager.manage(&actor, &two).await.unwrap();
    assert_eq!(
        a.manager.manage(&actor, &command(&app)).await,
        Err(Error::Capacity)
    );
    let worker = register_worker(&a, 1).await;
    let assignment = place(&a, &app).await;
    assert_eq!(assignment.worker_id, worker);
    let scope = assigned(&assignment);
    // A lifecycle command is a maintenance row. `claim_job` claims as
    // `Claimant::Placed`, which admits the creator operation alone, so the placed
    // worker is offered nothing here and the row belongs to the authority the
    // owning process asserts. The refusal is the control for the claim below: one
    // variable differs, and it is the claimant.
    assert!(b
        .manager
        .claim_job(&worker, &scope, Ok(AppPolicy::default().max_delivery_attempts), || async { Ok(worker.clone()) })
        .await
        .unwrap()
        .is_none());
    // The authority carries this app's own placed identity rather than a fresh
    // one, because `Queue::settle` authorizes on `settlement.delivery.worker_id`
    // and the settlements below are made under `worker`. Its asserted revision is
    // `ASSERTED`, which is the revision this first placement holds, so the
    // placement read behind `settle_job` resolves the authority this lease names.
    let lane = MaintenanceAuthority::new(app.clone(), worker.clone());
    let grant = lane
        .claim(
            b.manager.queue(),
            Ok(AppPolicy::default().max_delivery_attempts),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(grant.delivery().worker_id, worker);
    assert_eq!(grant.delivery().assignment_revision, assignment.revision);
    assert_eq!(
        grant.delivery().job.operation,
        JobOperation::Management {
            request_id: one.request_id.clone(),
            run_id: one.run_id.clone(),
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::Transition {
                operation: RunOperation::Pause
            },
        }
    );
    let outcome = ManagementOutcome::Applied {
        state: RunState::Paused,
    };
    let settlement = JournalSettlement::from_receipt(
        &JobReceipt {
            job: grant.delivery().job.clone(),
            outcome: JobOutcome::Management {
                outcome: outcome.clone(),
            },
        },
        grant.delivery(),
    )
    .unwrap();
    let foreign_worker = WorkerId::mint();
    assert_eq!(
        a.manager
            .settle_job(&foreign_worker, &settlement, || async {
                Ok(foreign_worker.clone())
            })
            .await,
        Err(Error::Denied)
    );
    let receipt = a
        .manager
        .settle_job(&worker, &settlement, || async { Ok(worker.clone()) })
        .await
        .unwrap();
    assert_eq!(
        b.manager
            .settle_job(&worker, &settlement, || async { Ok(worker.clone()) })
            .await
            .unwrap(),
        receipt
    );
    let management_receipt = ManagementReceipt {
        app_id: app.clone(),
        request_id: one.request_id.clone(),
        outcome: Some(outcome),
    };
    assert_eq!(
        b.manager.manage(&actor, &one).await.unwrap(),
        management_receipt
    );
    assert_ids_retained(&initial_ids, &fixture.stored_ids().await);
    assert_eq!(
        b.manager
            .management_receipt(&app, &one.request_id)
            .await
            .unwrap(),
        Some(management_receipt)
    );
    let conflicting = JournalSettlement::from_receipt(
        &JobReceipt {
            job: grant.delivery().job.clone(),
            outcome: JobOutcome::Management {
                outcome: ManagementOutcome::NotFound {},
            },
        },
        grant.delivery(),
    )
    .unwrap();
    assert_eq!(
        b.manager
            .settle_job(&worker, &conflicting, || async { Ok(worker.clone()) })
            .await,
        Err(Error::Conflict)
    );
    let mut foreign_delivery = grant.delivery().clone();
    foreign_delivery.job.app_id = AppId::mint();
    let foreign = JournalSettlement::from_receipt(
        &JobReceipt {
            job: foreign_delivery.job.clone(),
            outcome: settlement.outcome().clone(),
        },
        &foreign_delivery,
    )
    .unwrap();
    assert_eq!(
        a.manager
            .settle_job(&worker, &foreign, || async { Ok(worker.clone()) })
            .await,
        Err(Error::Denied)
    );
    a.manager
        .release(&worker, &release(&assignment, ReleaseReason::Relinquished))
        .await
        .unwrap();
    let renewed = place(&a, &app).await;
    assert_ne!(
        renewed.revision, assignment.revision,
        "a re-placement must supersede the revision the settlement above names"
    );
    // Exact committed settlement remains readable after placement replacement.
    assert_eq!(
        b.manager
            .settle_job(&worker, &settlement, || async { Ok(worker.clone()) })
            .await
            .unwrap(),
        receipt
    );
    drop(a);
    drop(b);
    let reopened = fixture.options(options).await;
    // The same authority over a reopened queue. It is asserted rather than read,
    // so the replaced placement above neither grants nor withdraws it.
    let grant = lane
        .claim(
            reopened.manager.queue(),
            Ok(AppPolicy::default().max_delivery_attempts),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        grant.delivery().job.operation,
        JobOperation::Management {
            request_id: two.request_id.clone(),
            run_id: two.run_id.clone(),
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::RestartStarted {
                from: Some(RestartTarget {
                    name: "checkpoint".into(),
                    occurrence: Some(0),
                })
            },
        }
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
        deployment: None,
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
    let assignment = place(&normal, &app).await;
    assert_eq!(assignment.worker_id, worker);
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
            "DO $$ BEGIN
                 IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='customer_reader') THEN
                     CREATE ROLE customer_reader;
                 END IF;
             END $$;
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
        "../../../zeroship-workflow-manager/schema/identity-columns.json"
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
           AND constraint_row.conname IN ('assignment_scope','assignment_worker','receipt_scope','management_job','management_order','management_order_app','jobs_app_id_fkey') \
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
            ("assignments", "queue_scopes", vec!["id"]),
            ("assignments", "workers", vec!["id"]),
            ("jobs", "queue_scopes", vec!["id"]),
            ("management", "jobs", vec!["app_id", "id"]),
            ("management", "management_scopes", vec!["app_id", "run_id"]),
            ("management_scopes", "queue_scopes", vec!["id"]),
            ("placement_receipts", "queue_scopes", vec!["id"]),
        ]
        .map(|(table, target, columns)| (
            table.to_owned(),
            target.to_owned(),
            columns.into_iter().map(str::to_owned).collect::<Vec<_>>()
        ))
    );
    fixture
        .admin
        .batch_execute("UPDATE workflow_manager.schema_version SET fingerprint='stale'")
        .await
        .unwrap();
    assert_eq!(service.verify().await, Err(HostError::Unavailable));
    assert!(matches!(
        Coordinator::connect(
            &fixture.runtime_url,
            Options::default(),
            holds::client(),
            zone::trusted()
        )
        .await,
        Err(HostError::Unavailable)
    ));
}

#[compio::test]
async fn assignment_verification_preserves_leases_and_fences_app_authority() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 2).await;
    let app = AppId::mint();
    let assignment = place(&service, &app).await;
    assert_eq!(assignment.worker_id, worker);
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
    // An app or an instance with no placement of its own is not the placed
    // one, and no retry changes that.
    for foreign in [
        VerifyAssignment {
            app_id: AppId::mint(),
            ..request.clone()
        },
        VerifyAssignment {
            worker_id: WorkerId::mint(),
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

/// A revision that moved is not a refusal.
///
/// `refresh_entry` in `zeroship-workflow-runner` releases a placement as REFUSED
/// on `PermissionDenied`, and the manager then stops offering that pair to that
/// instance. A placement whose revision advanced under a caller that WAS
/// legitimately placed is the opposite case: the next scan installs the revision
/// that now holds, so it must not arrive as the permanent one. It reaches
/// creator code through `api/runs.rs` too, where the same distinction decides
/// whether a caller retries.
#[compio::test]
async fn a_moved_assignment_revision_conflicts_rather_than_denying() {
    let fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 2).await;
    let app = AppId::mint();
    let first = place(&service, &app).await;
    assert_eq!(first.worker_id, worker);
    let granted = VerifyAssignment {
        app_id: app.clone(),
        worker_id: worker.clone(),
        assignment_revision: first.revision,
    };
    // The control. Without it the refusal below would also pass over a fixture
    // that never placed anything.
    assert_eq!(
        service
            .manager
            .verify_assignment(&granted)
            .await
            .map(|verified| verified.revision),
        Ok(first.revision)
    );
    service
        .manager
        .release(&worker, &release(&first, ReleaseReason::Relinquished))
        .await
        .unwrap();
    let moved = place(&service, &app).await;
    assert!(moved.revision > first.revision);
    assert_eq!(
        service.manager.verify_assignment(&granted).await,
        Err(Error::Conflict)
    );
    // The revision that now holds verifies, so the refusal above is about the
    // revision rather than about the pair.
    assert_eq!(
        service
            .manager
            .verify_assignment(&VerifyAssignment {
                assignment_revision: moved.revision,
                ..granted.clone()
            })
            .await
            .map(|verified| verified.revision),
        Ok(moved.revision)
    );
}

#[compio::test]
async fn assignment_verification_checks_expiry_after_waiting_for_scope_lock() {
    let mut fixture = Fixture::new().await;
    let service = fixture.service().await;
    let worker = register_worker(&service, 1).await;
    let app = AppId::mint();
    let assignment = place(&service, &app).await;
    assert_eq!(assignment.worker_id, worker);
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

/// The startup budget these tests give the coordinator. The pool's own default
/// warm-up budget is far longer than [`STARTUP_WATCHDOG`], so a step bounded by
/// that default instead of by this budget trips the watchdog.
const STARTUP_BUDGET: Duration = Duration::from_millis(500);
const STARTUP_WATCHDOG: Duration = Duration::from_secs(5);

/// A `PostgreSQL` stand-in that completes the handshake of its first `answered`
/// connections and then accepts every later one without ever answering it.
struct StallingDatabase {
    url: String,
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<Accepted>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Accepted {
    answered: usize,
    silent: usize,
}
impl StallingDatabase {
    fn start(answered: usize) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the stand-in");
        listener
            .set_nonblocking(true)
            .expect("make the stand-in's accept pollable");
        let url = format!(
            "postgres://coordinator_test@{}/postgres?sslmode=disable",
            listener.local_addr().expect("stand-in address")
        );
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut held = Vec::new();
            let mut accepted = Accepted {
                answered: 0,
                silent: 0,
            };
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("make the accepted stream blocking");
                        if accepted.answered < answered {
                            answer_startup(&mut stream);
                            accepted.answered += 1;
                        } else {
                            accepted.silent += 1;
                        }
                        held.push(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if !matches!(
                            stopped.try_recv(),
                            Err(std::sync::mpsc::TryRecvError::Empty)
                        ) {
                            // Held until now so that every accepted session
                            // stays open and silent, never closed: a closed
                            // socket is a refusal, not a database that stalls.
                            drop(held);
                            return accepted;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("stand-in accept failed: {error}"),
                }
            }
        });
        Self { url, stop, thread }
    }

    fn stop(self) -> Accepted {
        let _ = self.stop.send(());
        self.thread.join().expect("the stand-in panicked")
    }
}

/// Read one startup packet and answer it with `AuthenticationOk`,
/// `BackendKeyData` and `ReadyForQuery`: a session that is open and idle.
fn answer_startup(stream: &mut std::net::TcpStream) {
    use std::io::{Read, Write};
    stream
        .set_read_timeout(Some(STARTUP_WATCHDOG))
        .expect("bound the stand-in's read");
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read the startup length");
    let mut startup = vec![0_u8; u32::from_be_bytes(length) as usize - length.len()];
    stream
        .read_exact(&mut startup)
        .expect("read the startup packet");
    stream
        .write_all(&[
            b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'K', 0, 0, 0, 12, 0, 0, 0, 7, 0, 0, 0, 46, b'Z', 0, 0,
            0, 5, b'I',
        ])
        .expect("answer the startup packet");
}

fn budgeted() -> Options {
    Options {
        acquire_timeout: STARTUP_BUDGET,
        ..Options::default()
    }
}

/// Run one startup step under the watchdog and return how long it took to
/// fail, with the error it produced. A step that is not bounded by the startup
/// budget trips the watchdog.
async fn fails_within_budget<T, E>(
    step: &str,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> (Duration, E) {
    let started = std::time::Instant::now();
    let result = compio::time::timeout(STARTUP_WATCHDOG, future)
        .await
        .unwrap_or_else(|_| panic!("{step} outlived the startup budget of {STARTUP_BUDGET:?}"));
    let elapsed = started.elapsed();
    let Err(error) = result else {
        panic!("{step} succeeded against a database that never answers");
    };
    (elapsed, error)
}

/// A database that accepts connections and never answers fails coordinator
/// startup within the one startup budget: constructing the pool, and binding
/// placement eligibility, each stop at `Options::startup_timeout` rather than
/// at a default of the pool's.
#[compio::test]
async fn a_silent_database_fails_startup_within_the_startup_budget() {
    assert!(
        compio_postgres::PoolConfig::default().get_warm_up_timeout() > STARTUP_WATCHDOG,
        "the pool's default warm-up budget fits inside the watchdog, so this test \
         cannot tell it from the startup budget"
    );
    assert_eq!(budgeted().startup_timeout(), STARTUP_BUDGET);
    let database = StallingDatabase::start(0);

    let (elapsed, error) = fails_within_budget(
        "coordinator startup",
        Box::pin(Coordinator::connect(
            &database.url,
            budgeted(),
            holds::client(),
            zone::trusted(),
        )),
    )
    .await;
    assert_eq!(
        error,
        HostError::Unavailable,
        "coordinator startup did not report the database unavailable"
    );
    assert!(
        elapsed >= STARTUP_BUDGET,
        "coordinator startup failed after {elapsed:?}, before its budget, so something \
         other than the budget stopped it"
    );

    let (elapsed, error) = fails_within_budget(
        "binding placement eligibility",
        connect_eligibility(&database.url, budgeted()),
    )
    .await;
    assert_eq!(
        error,
        HostError::Unavailable,
        "binding placement eligibility did not report the database unavailable"
    );
    assert!(
        elapsed >= STARTUP_BUDGET,
        "binding placement eligibility failed after {elapsed:?}, before its budget"
    );

    let accepted = database.stop();
    assert_eq!(accepted.answered, 0);
    assert!(
        accepted.silent >= 2,
        "the stand-in accepted fewer connections than the steps that ran: {accepted:?}"
    );
}

/// The step after the pool is bounded by the same budget: a database that
/// answers the pool's warm-up connection and then stalls every later one fails
/// startup at the queue binding within `Options::startup_timeout`.
#[compio::test]
async fn a_database_that_stalls_after_the_pool_fails_startup_within_the_startup_budget() {
    let database = StallingDatabase::start(1);

    let (elapsed, error) = fails_within_budget(
        "coordinator startup",
        Box::pin(Coordinator::connect(
            &database.url,
            budgeted(),
            holds::client(),
            zone::trusted(),
        )),
    )
    .await;
    assert_eq!(
        error,
        HostError::Unavailable,
        "coordinator startup did not report the database unavailable"
    );
    assert!(
        elapsed >= STARTUP_BUDGET,
        "coordinator startup failed after {elapsed:?}, before its budget"
    );

    let accepted = database.stop();
    assert_eq!(
        accepted.answered, 1,
        "the pool's warm-up connection was not the answered one: {accepted:?}"
    );
    assert!(
        accepted.silent >= 1,
        "startup failed without the queue opening a connection, so the failure is \
         not the queue step's: {accepted:?}"
    );
}

/// A facts source that is never consulted: `connect_policies` opens the ledger
/// before it reads any observation, and a silent database fails there.
#[derive(Debug)]
struct SilentFacts;
impl AppFactsSource for SilentFacts {
    fn observe<'a>(&'a self, _apps: &'a [AppId]) -> AppFactsFuture<'a> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

/// Opening the journal is a startup step, so a database that accepts
/// connections and never answers fails it within `Options::startup_timeout`,
/// not at the construction default of the ORM pools it opens.
#[compio::test]
async fn a_silent_database_fails_the_journal_step_within_the_startup_budget() {
    assert!(
        compio_postgres::PoolConfig::default().get_warm_up_timeout() > STARTUP_WATCHDOG,
        "the pool's default warm-up budget fits inside the watchdog, so this test \
         cannot tell it from the startup budget"
    );
    let fixture = Fixture::new().await;
    let service = fixture.options(Options::default()).await;
    let recovery = service
        .recovery(RecoveryOptions::default())
        .expect("a live coordinator yields a recovery scope");
    let database = StallingDatabase::start(0);

    let (elapsed, error) = fails_within_budget(
        "journal startup",
        Box::pin(RunService::connect_within(
            &database.url,
            recovery,
            STARTUP_BUDGET,
        )),
    )
    .await;
    assert!(
        matches!(error, WorkflowServiceError::Unavailable(_)),
        "journal startup did not report the database unavailable: {error:?}"
    );
    assert!(
        elapsed >= STARTUP_BUDGET,
        "journal startup failed after {elapsed:?}, before its budget"
    );

    let accepted = database.stop();
    assert_eq!(accepted.answered, 0);
    assert!(
        accepted.silent >= 1,
        "the journal step never reached the silent database: {accepted:?}"
    );
}

/// Opening the policy ledger is a startup step too, so a database that accepts
/// connections and never answers fails it within `Options::startup_timeout`,
/// not within the ledger's transaction deadline.
#[compio::test]
async fn a_silent_database_fails_the_policy_step_within_the_startup_budget() {
    let database = StallingDatabase::start(0);

    let (elapsed, error) = fails_within_budget(
        "policy ledger startup",
        Box::pin(connect_policies(
            Rc::new(SilentFacts),
            &database.url,
            budgeted(),
            PolicyObservations::new(NonZeroUsize::new(16).expect("policy cache capacity")),
        )),
    )
    .await;
    assert_eq!(
        error,
        Error::Unavailable,
        "policy ledger startup did not report the database unavailable"
    );
    assert!(
        elapsed >= STARTUP_BUDGET,
        "policy ledger startup failed after {elapsed:?}, before its budget"
    );

    let accepted = database.stop();
    assert_eq!(accepted.answered, 0);
    assert!(
        accepted.silent >= 1,
        "the policy step never reached the silent database: {accepted:?}"
    );
}
