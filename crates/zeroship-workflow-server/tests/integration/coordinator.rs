#![allow(
    clippy::future_not_send,
    reason = "fixtures use compio pools on their owning test runtime"
)]

use compio_postgres::{Client, NoTls};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    typed_id,
    workflow_coordination::*,
    workflow_jobs::{
        ClaimJobs, Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobReceipt, JobSpec,
        JournalSettlement, ManagementCommand,
    },
    workflow_policy::AppPolicy,
    ZoneId,
};
use zeroship_workflow::WorkflowServiceError;
use zeroship_workflow_manager::{
    app_facts::{AppFactsFuture, AppFactsSource},
    coordinator::{Admission, ZoneClaim},
    maintenance::MaintenanceAuthority,
    policy::control::PolicyObservations,
    recovery::Options as RecoveryOptions,
    DeliveryGrant, Error,
};
use zeroship_workflow_server::{
    coordinator::{Coordinator, Error as HostError, Options, SCHEMA_SQL},
    runs::RunService,
    server::connect_policies,
};

type StoredIds = BTreeMap<(String, String, String), String>;

use crate::support::{holds, platform, policies::GrantedPolicies};

struct Fixture {
    platform: platform::Platform,
    admin: Client,
    admin_url: String,
    runtime_url: String,
}
impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::fresh_database().await;
        let admin = connect(platform.admin_url.as_str()).await;
        // The clone carries the migrated platform schema. This fixture's
        // subject is the coordinator's own generated schema, so it replaces
        // those schemas before installing it. The manager's deployment catalog
        // lives in `zeroship`, and nothing the coordinator reads does, so the
        // schema exists and the coordinator holds no grant on it.
        admin
            .batch_execute(
                "DROP SCHEMA IF EXISTS workflow_manager CASCADE;
             DROP SCHEMA IF EXISTS zeroship CASCADE;
             DROP SCHEMA IF EXISTS customer CASCADE;
             CREATE SCHEMA workflow_manager;
             CREATE SCHEMA zeroship;
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
        grant_runtime(&admin, "coordinator_test").await;
        let runtime_url = platform.role_url("coordinator_test").to_string();
        let admin_url = platform.admin_url.to_string();
        Self {
            platform,
            admin,
            admin_url,
            runtime_url,
        }
    }
    /// A login of this case's own, carrying the runtime grants in this clone.
    ///
    /// For a case that mutates a role membership: the minted login is
    /// cluster-global but unique, so the membership reaches no sibling.
    async fn dedicated_role(&self) -> String {
        let role = typed_id::generate("wcr");
        self.admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"
            ))
            .await
            .unwrap();
        grant_runtime(&self.admin, &role).await;
        role
    }
    /// The database URL of a login this case created.
    fn role_url(&self, role: &str) -> String {
        self.platform.role_url(role).to_string()
    }
    async fn service(&self) -> Coordinator {
        self.options(Options::default()).await
    }
    async fn connect_as(&self, url: &str, options: Options) -> Coordinator {
        Coordinator::connect(url, options, holds::client())
            .await
            .unwrap()
    }
    async fn options(&self, options: Options) -> Coordinator {
        Coordinator::connect(&self.runtime_url, options, holds::client())
            .await
            .unwrap()
    }
    async fn stored_ids(&self) -> StoredIds {
        let rows = self.admin.query(
            "SELECT 'queue_scopes' AS kind,id AS scope,'' AS subject,id FROM workflow_manager.queue_scopes
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
                "queue_scopes" => assert_eq!(id, scope),
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
/// Grant `role` the database-local authority a coordinator runtime holds in
/// this clone. Shared by the fixture's login and any case's dedicated one.
async fn grant_runtime(admin: &Client, role: &str) {
    admin
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA workflow_manager TO \"{role}\";
             GRANT SELECT ON workflow_manager.schema_version TO \"{role}\";
             GRANT SELECT,INSERT,UPDATE,DELETE ON
               workflow_manager.queue_scopes,workflow_manager.deployment_holds,
               workflow_manager.management,workflow_manager.management_scopes,workflow_manager.jobs,
               workflow_manager.schedule_deployments,workflow_manager.schedule_activations,
               workflow_manager.schedule_disables,workflow_manager.schedule_scopes,
               workflow_manager.schedules,workflow_manager.schedule_occurrences,
               workflow_manager.recovery_scopes,workflow_manager.recovery_duties,
               workflow_manager.capacity_targets TO \"{role}\";"
        ))
        .await
        .unwrap();
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

/// Register `app`'s queue scope in `zone` and submit one advance job for it,
/// the way the service's own publication submits committed intent.
async fn submitted(service: &Coordinator, app: &AppId, zone: &ZoneId) -> JobSpec {
    let queue = service.manager.queue();
    queue.register_scope(app, zone).await.unwrap();
    let job = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 1.try_into().unwrap(),
    };
    assert_eq!(queue.submit(&job).await.unwrap(), job);
    job
}

/// One zone claim for one job, by an enrolled `worker` the authorization
/// callback keeps confirming.
async fn claim(
    service: &Coordinator,
    worker: &WorkerId,
    zone: &ZoneId,
    policies: &GrantedPolicies,
) -> Vec<DeliveryGrant> {
    let request = ClaimJobs {
        max: NonZeroU32::MIN,
        wait_ms: NonZeroU64::new(5_000).unwrap(),
        after: None,
        exclude: Vec::new(),
    };
    let claim = ZoneClaim {
        worker,
        zone,
        request: &request,
        deadline: service
            .manager
            .claim_deadline(Instant::now(), &request)
            .unwrap(),
    };
    service
        .manager
        .claim_in_zone(
            &claim,
            policies,
            || async { Ok(worker.clone()) },
            |_| async { Admission::Deliver(()) },
        )
        .await
        .unwrap()
        .0
        .grants
        .into_iter()
        .map(|(grant, ())| grant)
        .collect()
}

async fn heartbeat(
    service: &Coordinator,
    worker: &WorkerId,
    delivery: &Delivery,
) -> Result<DeliveryGrant, Error> {
    service
        .manager
        .heartbeat_job(worker, delivery, || async { Ok(worker.clone()) })
        .await
}

/// Two replicas over one queue hand one job to exactly one of two workers
/// claiming at once, and the claim leaves the scope's stored identity where it
/// was. The holder's lease then answers to either replica, and to no other
/// worker.
#[compio::test]
async fn replicas_deliver_a_job_once_and_keep_its_scope_identity() {
    let fixture = Fixture::new().await;
    let a = fixture.service().await;
    let b = fixture.service().await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    let policies = GrantedPolicies::default();
    policies.grant(&app, &zone, AppPolicy::default());
    let job = submitted(&a, &app, &zone).await;
    let initial_ids = fixture.stored_ids().await;
    let (first, second) = (WorkerId::mint(), WorkerId::mint());
    let (left, right) = futures::join!(
        claim(&a, &first, &zone, &policies),
        claim(&b, &second, &zone, &policies)
    );
    let mut granted = left.into_iter().chain(right).collect::<Vec<_>>();
    assert_eq!(granted.len(), 1, "racing claims must deliver the job once");
    let delivery = granted.pop().unwrap().delivery().clone();
    assert_eq!(delivery.job, job);
    assert_eq!(delivery.attempt.get(), 1);
    assert_ids_retained(&initial_ids, &fixture.stored_ids().await);
    let (holder, other) = if delivery.worker_id == first {
        (first, second)
    } else {
        (second, first)
    };
    for replica in [&a, &b] {
        assert!(
            claim(replica, &other, &zone, &policies).await.is_empty(),
            "a leased job is offered to nobody else"
        );
        let renewed = heartbeat(replica, &holder, &delivery).await.unwrap();
        assert_eq!(renewed.delivery().attempt, delivery.attempt);
    }
    assert!(heartbeat(&a, &other, &delivery).await.is_err());
    assert_eq!(
        heartbeat(
            &b,
            &other,
            &Delivery {
                worker_id: other.clone(),
                ..delivery.clone()
            }
        )
        .await
        .unwrap_err(),
        Error::Conflict,
        "a delivery naming a worker the queue never handed it to is stale"
    );
}

#[compio::test]
async fn management_is_durable_bounded_typed_and_held_by_the_lane() {
    let fixture = Fixture::new().await;
    let options = Options {
        batch_limit: 1,
        max_pending_management: 2,
        ..Options::default()
    };
    let a = fixture.options(options).await;
    let b = fixture.options(options).await;
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let zone = ZoneId::mint();
    let app = AppId::mint();
    a.manager.queue().register_scope(&app, &zone).await.unwrap();
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
    // A lifecycle command is a maintenance row. A zone claim claims as
    // `Claimant::Worker`, which admits the creator operation alone, so a worker
    // of the app's own zone is offered nothing here and the row belongs to the
    // authority the owning process asserts. The refusal is the control for the
    // lane's claim below: one variable differs, and it is the claimant.
    let worker = WorkerId::mint();
    let policies = GrantedPolicies::default();
    policies.grant(&app, &zone, AppPolicy::default());
    assert!(claim(&b, &worker, &zone, &policies).await.is_empty());
    // The lane carries the worker's identity here so the settlement below is
    // made by the holder the lease names.
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
    drop(a);
    drop(b);
    let reopened = fixture.options(options).await;
    // The exact committed settlement stays readable over a reopened queue.
    assert_eq!(
        reopened
            .manager
            .settle_job(&worker, &settlement, || async { Ok(worker.clone()) })
            .await
            .unwrap(),
        receipt
    );
    // The same authority over the reopened queue. It is asserted rather than
    // read, so nothing about the earlier processes grants or withdraws it.
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

/// A heartbeat waiting on the app lock is bounded by the transaction budget,
/// the session it timed out on is reusable, and a lease that lapsed while the
/// heartbeat waited is refused after the wait rather than extended.
#[compio::test]
async fn lock_waits_cannot_extend_authority_and_timeout_sessions_are_reusable() {
    let mut fixture = Fixture::new().await;
    let normal = fixture.service().await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    let policies = GrantedPolicies::default();
    policies.grant(&app, &zone, AppPolicy::default());
    submitted(&normal, &app, &zone).await;
    let worker = WorkerId::mint();
    let delivery = claim(&normal, &worker, &zone, &policies)
        .await
        .pop()
        .expect("the zone's worker claims the job")
        .delivery()
        .clone();
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
        heartbeat(&bounded, &worker, &delivery).await.unwrap_err(),
        Error::Timeout
    );
    lock.rollback().await.unwrap();
    // The pool session survives the cancelled command: `verify` runs several
    // queries on the same one-connection pool that just timed out.
    bounded.verify().await.unwrap();
    // The delivery the wait contended for is live, read through a coordinator
    // whose budget is not the 80ms that produced the timeout, so the outcome
    // does not turn on a second query fitting that budget.
    heartbeat(&normal, &worker, &delivery).await.unwrap();

    let blocker_pid: i32 = fixture
        .admin
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let expirer = connect(&fixture.admin_url).await;
    let lock = fixture.admin.transaction().await.unwrap();
    lock.query(
        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
        &[&app.as_str()],
    )
    .await
    .unwrap();
    let lapse = async {
        compio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = expirer
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE usename='coordinator_test' AND $1=ANY(pg_blocking_pids(pid)))",
                        &[&blocker_pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the heartbeat must reach the held app lock");
        // The lease lapses while the heartbeat waits. The job row is not under
        // the lock, so this lands before the heartbeat reads it.
        assert_eq!(
            expirer
                .execute(
                    "UPDATE workflow_manager.jobs SET lease_deadline=0 WHERE app_id=$1 AND id=$2",
                    &[&app.as_str(), &delivery.job.id.as_str()],
                )
                .await
                .unwrap(),
            1
        );
        lock.commit().await.unwrap();
    };
    let (attempt, ()) = futures::join!(heartbeat(&normal, &worker, &delivery), lapse);
    assert_eq!(attempt.unwrap_err(), Error::Conflict);
}

#[compio::test]
async fn metadata_schema_has_no_customer_authority_and_ids_are_bytewise() {
    let fixture = Fixture::new().await;
    // The reader AND the login that receives it are cluster-global, so this
    // case connects through a login of its own. A membership granted to the
    // shared `coordinator_test` would reach every sibling case on the server,
    // whose `Coordinator::connect` calls `verify`.
    let runtime_role = fixture.dedicated_role().await;
    let runtime_url = fixture.role_url(&runtime_role);
    let service = fixture.connect_as(&runtime_url, Options::default()).await;
    let runtime = connect(&runtime_url).await;
    let reader = typed_id::generate("wcr");
    fixture
        .admin
        .batch_execute(&format!(
            "CREATE ROLE \"{reader}\";
         GRANT USAGE ON SCHEMA customer TO \"{reader}\";
         GRANT SELECT ON customer.__zeroship_workflow_history TO \"{reader}\";
         GRANT \"{reader}\" TO \"{runtime_role}\" WITH INHERIT FALSE;"
        ))
        .await
        .unwrap();
    let privileges = runtime
        .query_one(
            &format!(
                "SELECT pg_has_role(current_user,'{reader}','USAGE'),
                pg_has_role(current_user,'{reader}','SET')"
            ),
            &[],
        )
        .await
        .unwrap();
    assert!(!privileges.get::<_, bool>(0));
    assert!(privileges.get::<_, bool>(1));
    assert_eq!(service.verify().await, Err(HostError::Unavailable));
    fixture
        .admin
        .batch_execute(&format!(
            "REVOKE \"{reader}\" FROM \"{runtime_role}\"; \
             DROP OWNED BY \"{reader}\"; DROP ROLE \"{reader}\""
        ))
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
           AND constraint_row.conname IN ('management_job','management_order','management_order_app','jobs_app_id_fkey') \
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
            ("jobs", "queue_scopes", vec!["id"]),
            ("management", "jobs", vec!["app_id", "id"]),
            ("management", "management_scopes", vec!["app_id", "run_id"]),
            ("management_scopes", "queue_scopes", vec!["id"]),
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
        Coordinator::connect(&fixture.runtime_url, Options::default(), holds::client()).await,
        Err(HostError::Unavailable)
    ));
    // Retire the login this case minted; it owns nothing outside this clone.
    fixture
        .admin
        .batch_execute(&format!(
            "DROP OWNED BY \"{runtime_role}\"; DROP ROLE \"{runtime_role}\""
        ))
        .await
        .unwrap();
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
/// startup within the one startup budget: constructing the pool stops at
/// `Options::startup_timeout` rather than at a default of the pool's.
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

    let accepted = database.stop();
    assert_eq!(accepted.answered, 0);
    assert!(
        accepted.silent >= 1,
        "the stand-in accepted no connection, so the step that ran never reached it: {accepted:?}"
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
