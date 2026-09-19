//! The real manager host advances durable metadata without an execution worker.
#![allow(
    clippy::future_not_send,
    reason = "process, HTTP and database fixtures share the ntex compio runtime"
)]

#[path = "support/holds.rs"]
mod holds;
#[path = "support/platform.rs"]
mod platform;
#[path = "support/policy.rs"]
mod policy_fixture;
#[allow(
    dead_code,
    reason = "the shared process fixture also exposes explicit connection-failure probes"
)]
#[path = "support/server_process.rs"]
mod server_process;

use ntex::{client::Client, http::StatusCode};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    service_assertion::ServiceSigningKey,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    typed_id,
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
    workflow_schedules::{
        ActivateSchedules, RegisterSchedules, ScheduleCatchUp, ScheduleDescriptor, ScheduleId,
        ScheduleOverlap, ScheduleTiming,
    },
};
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    recovery::{Options as RecoveryOptions, Recovery},
    scheduling::{Options as SchedulerOptions, Scheduler},
    Options, Queue,
};

#[path = "driver/collection.rs"]
mod collection;

struct Seed {
    app: AppId,
    deployment: DeploymentId,
    activation: JobSpec,
    broken: ScheduleId,
    healthy: ScheduleId,
    acquiring: DeploymentId,
    releasing: DeploymentId,
    held: DeploymentId,
    unselected: DeploymentId,
}

async fn seed(platform: &platform::Platform) -> Seed {
    let queue = Queue::connect(
        DbBinding::new(
            "workflow_manager",
            "workflow_manager",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        &platform.runtime_url,
        Options::default(),
        holds::client(),
    )
    .await
    .unwrap();
    let scheduler = Scheduler::new(queue.clone(), SchedulerOptions::default()).unwrap();
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    let registration = RegisterSchedules {
        app_id: app.clone(),
        deployment_id: deployment.clone(),
        schedules: ["calendar-a", "calendar-b"]
            .into_iter()
            .map(|name| ScheduleDescriptor {
                name: name.into(),
                workflow_name: "scheduled-work".into(),
                schedule: ScheduleTiming::Cron {
                    cron_expr: "0 0 1 1 *".into(),
                    tz: "UTC".into(),
                },
                overlap: ScheduleOverlap::Allow,
                catch_up: ScheduleCatchUp::Skip,
            })
            .collect(),
    };
    scheduler.prepare(&registration).await.unwrap();
    let activation = scheduler
        .activate(&ActivateSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            revision: 1.try_into().unwrap(),
        })
        .await
        .unwrap();
    Recovery::new(queue.clone(), RecoveryOptions::default())
        .unwrap()
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();

    let (broken, healthy) = overdue_calendars(platform, &app).await;
    let acquiring = DeploymentId::mint();
    let releasing = DeploymentId::mint();
    let held = DeploymentId::mint();
    let unselected = DeploymentId::mint();
    for deployment in [&acquiring, &releasing, &held, &unselected] {
        queue.ensure_deployment(&app, deployment).await.unwrap();
    }
    // Simulate a manager crash before acknowledgement persistence. Only the
    // real server's authenticated Control client may finish these transitions.
    set_hold(platform, &app, &acquiring, "acquiring").await;
    platform
        .admin
        .execute(
            "UPDATE workflow_manager.deployment_holds SET deploy_hash=NULL \
             WHERE app_id=$1 AND deployment_id=$2",
            &[&app.as_str(), &acquiring.as_str()],
        )
        .await
        .unwrap();
    set_hold(platform, &app, &releasing, "releasing").await;
    // A hold confirmed long ago for a deployment the app never selected and no
    // job uses: the retention lane releases it through the real Control client.
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.deployment_holds SET held_at=0 \
                 WHERE app_id=$1 AND deployment_id=$2 AND state='held'",
                &[&app.as_str(), &unselected.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    // Only the unselected deployment still owes a journal release here. The
    // creator engine already gave the others' journal holds back, so the queue
    // lane this test follows is the only reason any of them changes.
    platform
        .admin
        .execute(
            "UPDATE workflow_manager.deployment_holds SET journal_state='released' \
             WHERE app_id=$1 AND deployment_id <> $2",
            &[&app.as_str(), &unselected.as_str()],
        )
        .await
        .unwrap();
    Seed {
        app,
        deployment,
        activation,
        broken,
        healthy,
        acquiring,
        releasing,
        held,
        unselected,
    }
}

async fn overdue_calendars(platform: &platform::Platform, app: &AppId) -> (ScheduleId, ScheduleId) {
    let schedules = platform
        .admin
        .query(
            "SELECT id FROM workflow_manager.schedules WHERE app_id=$1 ORDER BY id",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(schedules.len(), 2);
    let broken = ScheduleId::parse(schedules[0].get::<_, &str>(0)).unwrap();
    let healthy = ScheduleId::parse(schedules[1].get::<_, &str>(0)).unwrap();
    assert!(broken.as_str() < healthy.as_str());
    // Move the calendar frontier to a valid past occurrence. A corrupt earlier
    // candidate must remain retryable without starving the valid later calendar.
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.schedules SET next_at=0 WHERE app_id=$1",
                &[&app.as_str()],
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.schedules SET definition='{' WHERE app_id=$1 AND id=$2",
                &[&app.as_str(), &broken.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    platform
        .admin
        .execute(
            "UPDATE workflow_manager.recovery_duties SET next_due_at=0 WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();

    (broken, healthy)
}

async fn set_hold(
    platform: &platform::Platform,
    app: &AppId,
    deployment: &DeploymentId,
    state: &str,
) {
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.deployment_holds SET state=$3 \
                 WHERE app_id=$1 AND deployment_id=$2 AND state='held'",
                &[&app.as_str(), &deployment.as_str(), &state],
            )
            .await
            .unwrap(),
        1
    );
}

async fn hold_state(
    platform: &platform::Platform,
    seed: &Seed,
    deployment: &DeploymentId,
) -> String {
    platform
        .admin
        .query_one(
            "SELECT state FROM workflow_manager.deployment_holds WHERE app_id=$1 AND deployment_id=$2",
            &[&seed.app.as_str(), &deployment.as_str()],
        )
        .await
        .unwrap()
        .get(0)
}

async fn journal_state(
    platform: &platform::Platform,
    seed: &Seed,
    deployment: &DeploymentId,
) -> String {
    platform
        .admin
        .query_one(
            "SELECT journal_state FROM workflow_manager.deployment_holds \
             WHERE app_id=$1 AND deployment_id=$2",
            &[&seed.app.as_str(), &deployment.as_str()],
        )
        .await
        .unwrap()
        .get(0)
}

async fn until<T>(description: &str, mut probe: impl std::ops::AsyncFnMut() -> Option<T>) -> T {
    compio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(result) = probe().await {
                return result;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("server driver did not {description}"))
}

async fn private_schema(platform: &platform::Platform) {
    platform
        .admin
        .batch_execute(
            "CREATE SCHEMA driver_customer;
             CREATE TABLE driver_customer.__zeroship_workflow_history \
               (id text PRIMARY KEY, secret text NOT NULL);
             REVOKE ALL ON SCHEMA driver_customer FROM PUBLIC;
             REVOKE ALL ON driver_customer.__zeroship_workflow_history FROM PUBLIC;",
        )
        .await
        .unwrap();
    platform
        .admin
        .execute(
            "INSERT INTO driver_customer.__zeroship_workflow_history VALUES($1,'private history')",
            &[&typed_id::generate("wfh")],
        )
        .await
        .unwrap();
    let runtime = platform::connect(&platform.runtime_url).await;
    let denied = runtime
        .query(
            "SELECT * FROM driver_customer.__zeroship_workflow_history",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(
        denied.as_db_error().unwrap().code().code(),
        "42501",
        "the manager must lack creator-schema privileges"
    );
}

fn peers(platform: &platform::Platform) -> PathBuf {
    let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let key = ServiceSigningKey::generate();
    let path = platform.work.path().join("driver-peers.json");
    platform::write_private(
        &path,
        serde_json::to_vec(&json!({"keys":[{
            "iss":control.as_str(),"x":key.public_jwk_x()
        }]}))
        .unwrap(),
    );
    path
}

async fn jobs(platform: &platform::Platform, app: &AppId) -> Vec<String> {
    platform
        .admin
        .query(
            "SELECT to_jsonb(j)::text FROM workflow_manager.jobs j WHERE app_id=$1 ORDER BY id",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn no_workers(platform: &platform::Platform) {
    let row = platform
        .admin
        .query_one(
            "SELECT (SELECT COUNT(*) FROM zeroship.worker_instances), \
                    (SELECT COUNT(*) FROM workflow_manager.workers), \
                    (SELECT COUNT(*) FROM workflow_manager.assignments)",
            &[],
        )
        .await
        .unwrap();
    for column in 0..3 {
        assert_eq!(row.get::<_, i64>(column), 0);
    }
}

async fn initial_progress(platform: &platform::Platform, seed: &Seed) {
    until(
        "publish calendars and maintenance and reconcile interrupted holds",
        async || {
            let row = platform
                .admin
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM workflow_manager.schedule_occurrences \
                     WHERE app_id=$1 AND schedule_id=$2), \
                    (SELECT pending_job_id FROM workflow_manager.recovery_duties WHERE app_id=$1 AND kind='reconcile'), \
                    (SELECT pending_job_id FROM workflow_manager.recovery_duties WHERE app_id=$1 AND kind='collect')",
                    &[&seed.app.as_str(), &seed.healthy.as_str()],
                )
                .await
                .unwrap();
            let published: bool = row.get(0);
            let pending: Option<String> = row.get(1);
            let collection: Option<String> = row.get(2);
            (published
                && pending.is_some()
                && collection.is_some()
                && hold_state(platform, seed, &seed.acquiring).await == "held"
                && hold_state(platform, seed, &seed.releasing).await == "released"
                && hold_state(platform, seed, &seed.unselected).await == "released"
                // Giving the queue hold back leaves the journal one, which the
                // lane asks the creator engine for through a durable job.
                && journal_state(platform, seed, &seed.unselected).await == "releasing")
                .then_some(())
        },
    )
    .await;
    // The unselected hold confirmed moments ago is still within its grace.
    assert_eq!(hold_state(platform, seed, &seed.held).await, "held");
    assert_eq!(hold_state(platform, seed, &seed.deployment).await, "held");
}

struct Snapshot {
    jobs: Vec<String>,
    occurrence: String,
    pending: String,
    collection: String,
    scheduled_at: i64,
}

async fn snapshot(platform: &platform::Platform, seed: &Seed) -> Snapshot {
    let stable = jobs(platform, &seed.app).await;
    assert_eq!(stable.len(), 5);
    let operations: Vec<JobOperation> = stable
        .iter()
        .map(|row| {
            let row: serde_json::Value = serde_json::from_str(row).unwrap();
            assert_eq!(row["state"], "ready");
            assert_eq!(row["attempt"], 0);
            assert!(row["worker_id"].is_null());
            let operation: JobOperation =
                serde_json::from_str(row["operation"].as_str().unwrap()).unwrap();
            match &operation {
                JobOperation::Activate { deployment_id, .. }
                | JobOperation::Cron { deployment_id, .. } => {
                    assert_eq!(deployment_id, &seed.deployment);
                    assert_eq!(row["deployment_id"], deployment_id.as_str());
                }
                // A release names its deployment but reports none: no hold is
                // left to confirm, which is the whole point of releasing it.
                JobOperation::ReleaseHold { deployment_id } => {
                    assert_eq!(deployment_id, &seed.unselected);
                    assert!(row["deployment_id"].is_null());
                }
                JobOperation::Reconcile {} | JobOperation::Collect {} => {
                    assert!(row["deployment_id"].is_null());
                }
                other => panic!("unexpected manager job: {other:?}"),
            }
            operation
        })
        .collect();
    assert!(operations.contains(&seed.activation.operation));
    assert!(operations.iter().any(|operation| matches!(operation,
        JobOperation::ReleaseHold { deployment_id } if deployment_id == &seed.unselected)));
    assert!(operations
        .iter()
        .any(|operation| matches!(operation, JobOperation::Reconcile {})));
    assert!(operations
        .iter()
        .any(|operation| matches!(operation, JobOperation::Collect {})));
    assert!(operations.iter().any(|operation| matches!(operation,
        JobOperation::Cron {schedule_id, ..} if schedule_id == &seed.healthy)));
    let occurrence = platform.admin.query_one(
        "SELECT to_jsonb(o)::text AS snapshot,job_id,scheduled_at FROM workflow_manager.schedule_occurrences o \
         WHERE app_id=$1 AND schedule_id=$2",
        &[&seed.app.as_str(), &seed.healthy.as_str()],
    ).await.unwrap();
    let occurrence_snapshot: String = occurrence.get("snapshot");
    let occurrence_job = JobId::parse(occurrence.get::<_, &str>("job_id")).unwrap();
    let scheduled_at: i64 = occurrence.get("scheduled_at");
    let pending: String = platform
        .admin
        .query_one(
            "SELECT pending_job_id FROM workflow_manager.recovery_duties WHERE app_id=$1 AND kind='reconcile'",
            &[&seed.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_ne!(occurrence_job.as_str(), pending);
    let collection: String = platform
        .admin
        .query_one(
            "SELECT pending_job_id FROM workflow_manager.recovery_duties WHERE app_id=$1 AND kind='collect'",
            &[&seed.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_ne!(collection, pending);
    assert_ne!(collection, occurrence_job.as_str());
    Snapshot {
        jobs: stable,
        occurrence: occurrence_snapshot,
        pending,
        collection,
        scheduled_at,
    }
}

async fn replay_frontiers(platform: &platform::Platform, seed: &Seed, snapshot: &Snapshot) {
    // Replay an already accepted calendar occurrence after process loss. Its
    // persisted identity and the pending recovery job must survive cursor replay.
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.schedules SET next_at=$3 WHERE app_id=$1 AND id=$2",
                &[
                    &seed.app.as_str(),
                    &seed.healthy.as_str(),
                    &snapshot.scheduled_at
                ],
            )
            .await
            .unwrap(),
        1
    );
    platform
        .admin
        .execute(
            "UPDATE workflow_manager.recovery_duties SET next_due_at=0 WHERE app_id=$1",
            &[&seed.app.as_str()],
        )
        .await
        .unwrap();
    set_hold(platform, &seed.app, &seed.held, "releasing").await;
}

async fn replay_progress(platform: &platform::Platform, seed: &Seed, snapshot: &Snapshot) {
    until(
        "resume the committed calendar and release intent after restart",
        async || {
            let advanced: bool = platform
                .admin
                .query_one(
                    "SELECT next_at > $3 FROM workflow_manager.schedules WHERE app_id=$1 AND id=$2",
                    &[
                        &seed.app.as_str(),
                        &seed.healthy.as_str(),
                        &snapshot.scheduled_at,
                    ],
                )
                .await
                .unwrap()
                .get(0);
            (advanced && hold_state(platform, seed, &seed.held).await == "released").then_some(())
        },
    )
    .await;
}

async fn assert_replayed(platform: &platform::Platform, seed: &Seed, snapshot: &Snapshot) {
    assert_eq!(jobs(platform, &seed.app).await, snapshot.jobs);
    let replayed: String = platform
        .admin
        .query_one(
            "SELECT to_jsonb(o)::text FROM workflow_manager.schedule_occurrences o \
         WHERE app_id=$1 AND schedule_id=$2",
            &[&seed.app.as_str(), &seed.healthy.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(replayed, snapshot.occurrence);
    let replayed_pending: String = platform
        .admin
        .query_one(
            "SELECT pending_job_id FROM workflow_manager.recovery_duties WHERE app_id=$1 AND kind='reconcile'",
            &[&seed.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(replayed_pending, snapshot.pending);
    let replayed_collection: String = platform
        .admin
        .query_one(
            "SELECT pending_job_id FROM workflow_manager.recovery_duties WHERE app_id=$1 AND kind='collect'",
            &[&seed.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(replayed_collection, snapshot.collection);
    let broken = platform
        .admin
        .query_one(
            "SELECT definition,next_at, \
           (SELECT COUNT(*) FROM workflow_manager.schedule_occurrences WHERE schedule_id=$2) \
         FROM workflow_manager.schedules WHERE app_id=$1 AND id=$2",
            &[&seed.app.as_str(), &seed.broken.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(broken.get::<_, &str>(0), "{");
    assert_eq!(broken.get::<_, i64>(1), 0);
    assert_eq!(broken.get::<_, i64>(2), 0);
    assert_eq!(hold_state(platform, seed, &seed.deployment).await, "held");
    assert_eq!(platform.admin.query_one(
        "SELECT deploy_hash FROM workflow_manager.deployment_holds WHERE app_id=$1 AND deployment_id=$2",
        &[&seed.app.as_str(), &seed.acquiring.as_str()],
    ).await.unwrap().get::<_, String>(0), "a".repeat(64));
    no_workers(platform).await;
    assert_eq!(
        platform
            .admin
            .query_one(
                "SELECT secret FROM driver_customer.__zeroship_workflow_history",
                &[],
            )
            .await
            .unwrap()
            .get::<_, &str>(0),
        "private history"
    );
}

async fn ready(http: &Client, url: &str) {
    assert_eq!(
        http.get(format!("{url}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[ntex::test]
async fn server_drives_metadata_without_workers_and_resumes_after_restart() {
    let platform = platform::Platform::new().await;
    private_schema(&platform).await;
    let seed = Box::pin(seed(&platform)).await;
    let peers = peers(&platform);
    let http = Client::new().await;
    no_workers(&platform).await;
    assert_eq!(jobs(&platform, &seed.app).await.len(), 1);

    let server = server_process::ServerProcess::start(
        &platform.runtime_url,
        &peers,
        platform.work.path(),
        "driver",
        &http,
    )
    .await;
    initial_progress(&platform, &seed).await;
    ready(&http, &server.url).await;
    drop(server);

    let snapshot = snapshot(&platform, &seed).await;
    replay_frontiers(&platform, &seed, &snapshot).await;
    let restarted = server_process::ServerProcess::start(
        &platform.runtime_url,
        &peers,
        platform.work.path(),
        "driver-restarted",
        &http,
    )
    .await;
    replay_progress(&platform, &seed, &snapshot).await;
    ready(&http, &restarted.url).await;
    drop(restarted);
    assert_replayed(&platform, &seed, &snapshot).await;
}
