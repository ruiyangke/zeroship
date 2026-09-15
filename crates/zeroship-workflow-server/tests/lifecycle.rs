//! Control's terminal deletion, read under the manager's column grants, makes
//! the driver abandon an app's recovery responsibility instead of closing it.
#![expect(
    clippy::future_not_send,
    reason = "platform fixtures use their compio runtime"
)]

#[path = "support/holds.rs"]
mod holds;
#[allow(dead_code, reason = "the platform fixture also supports process tests")]
#[path = "support/platform.rs"]
mod platform;
#[path = "support/zone.rs"]
mod zone;

use std::{collections::BTreeSet, rc::Rc, time::Duration};
use zeroship_core::{
    schema_name::SchemaName, typed_id, workflow_jobs::DeploymentId, AppId, OrganizationId,
    ProjectId,
};
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
};
use zeroship_workflow_manager::{
    capacity::{Contract, LocalCapacity},
    coordinator::{Coordinator, Options as CoordinatorOptions},
    driver::{Driver, Options as DriverOptions},
    lifecycle::{self, AppLifecycle, ControlLifecycle},
    recovery::{Options as RecoveryOptions, Recovery, ScopeState},
    Error, Options, Queue,
};

async fn seed(platform: &platform::Platform, app: &AppId, name: &str) {
    let organization = OrganizationId::mint();
    let project = ProjectId::mint();
    let plan = typed_id::new_plan_id();
    // Case-insensitive identity columns take literals: the fixture names are
    // constants, and a text parameter does not bind to them.
    platform
        .admin
        .execute(
            &format!(
                "INSERT INTO zeroship.plans(id,name,runtime_limits_json,workflows_allowed) \
                 VALUES($1,'{name}','{{}}',true)"
            ),
            &[&plan],
        )
        .await
        .unwrap();
    platform
        .admin
        .execute(
            &format!(
                "INSERT INTO zeroship.organizations(id,slug,name,billing_email) \
                 VALUES($1,'{name}','{name}','lifecycle@zeroship.test')"
            ),
            &[&organization.as_str()],
        )
        .await
        .unwrap();
    platform
        .admin
        .execute(
            &format!(
                "INSERT INTO zeroship.projects(id,organization_id,slug,name) \
                 VALUES($1,$2,'default','{name}')"
            ),
            &[&project.as_str(), &organization.as_str()],
        )
        .await
        .unwrap();
    platform
        .admin
        .execute(
            &format!(
                "INSERT INTO zeroship.apps(id,name,plan_id,project_id,organization_id,workflows_enabled) \
                 VALUES($1,'{name}',$2,$3,$4,true)"
            ),
            &[&app.as_str(), &plan, &project.as_str(), &organization.as_str()],
        )
        .await
        .unwrap();
}

/// Control's terminal deletion: archive first, then the marker.
async fn delete(platform: &platform::Platform, app: &AppId) {
    platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET archived_at = now(), deleted_at = now(), project_id = NULL \
             WHERE id = $1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
}

async fn connect_lifecycle(url: &str) -> ControlLifecycle {
    let database = Database::connect(
        DbBinding::new(
            "platform",
            "workflow-lifecycle",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(url, ProjectKeySource::unavailable()).connection_authority(),
        lifecycle::collections().unwrap(),
    )
    .await
    .unwrap();
    ControlLifecycle::new(database).unwrap()
}

async fn queue(platform: &platform::Platform) -> Queue {
    Queue::connect(
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
    .unwrap()
}

/// The runtime role reads only the deletion marker it was granted, and its
/// readiness fails without that grant.
#[compio::test]
async fn deletion_marker_is_read_through_its_column_grant_alone() {
    let platform = platform::Platform::new().await;
    let live = AppId::mint();
    let gone = AppId::mint();
    seed(&platform, &live, "lifecycle-live").await;
    seed(&platform, &gone, "lifecycle-gone").await;
    let source = connect_lifecycle(&platform.runtime_url).await;
    source.ready().await.unwrap();
    let unknown = AppId::mint();
    let apps = [live.clone(), gone.clone(), unknown];
    assert!(source.deleted(&apps).await.unwrap().is_empty());
    delete(&platform, &gone).await;
    assert_eq!(
        source.deleted(&apps).await.unwrap(),
        BTreeSet::from([gone.clone()])
    );

    let runtime = platform::connect(&platform.runtime_url).await;
    assert!(
        runtime
            .query("SELECT project_id FROM zeroship.apps", &[])
            .await
            .is_err(),
        "the grant is column scoped"
    );
    platform
        .admin
        .batch_execute("REVOKE SELECT (deleted_at) ON zeroship.apps FROM zeroship_workflow")
        .await
        .unwrap();
    assert_eq!(source.ready().await, Err(Error::Unavailable));
    assert!(source.deleted(&apps).await.is_err());
    platform
        .admin
        .batch_execute("GRANT SELECT (deleted_at) ON zeroship.apps TO zeroship_workflow")
        .await
        .unwrap();
    source.ready().await.unwrap();
}

/// Over the canonical schema, one driver pass abandons the deleted app's
/// responsibility and begins closing the idle live one.
#[compio::test]
async fn the_driver_abandons_deleted_apps_over_the_canonical_schema() {
    let platform = platform::Platform::new().await;
    let live = AppId::mint();
    let gone = AppId::mint();
    seed(&platform, &live, "driver-live").await;
    seed(&platform, &gone, "driver-gone").await;
    let queue = queue(&platform).await;
    let recovery = RecoveryOptions {
        interval: Duration::from_secs(3600),
        idle_after: Duration::from_millis(1),
        ..RecoveryOptions::default()
    };
    let scopes = Recovery::new(queue.clone(), recovery).unwrap();
    for app in [&live, &gone] {
        scopes
            .ensure(app, &DeploymentId::mint(), 1.try_into().unwrap())
            .await
            .unwrap();
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.recovery_duties SET next_due_at = $2 WHERE app_id = $1",
                &[&app.as_str(), &(i64::MAX / 2)],
            )
            .await
            .unwrap();
    }
    delete(&platform, &gone).await;
    compio::time::sleep(Duration::from_millis(5)).await;
    let mut driver = Driver::new(
        Coordinator::new(queue, CoordinatorOptions::default(), zone::trusted()).unwrap(),
        DriverOptions {
            recovery,
            ..DriverOptions::default()
        },
        Rc::new(connect_lifecycle(&platform.runtime_url).await),
        Contract::declarative(Rc::new(LocalCapacity)),
    )
    .unwrap();
    let report = driver.tick().await.closing;
    assert!(
        report.failures.is_empty() && report.scan_error.is_none(),
        "{report:?}"
    );
    assert_eq!(report.completed, 2);
    let state = async |app: &AppId| scopes.responsibility(app).await.unwrap().unwrap().state;
    assert_eq!(state(&gone).await, ScopeState::Abandoned);
    assert_eq!(state(&live).await, ScopeState::Closing);
    let duties: i64 = platform
        .admin
        .query_one(
            "SELECT count(*) FROM workflow_manager.recovery_duties WHERE app_id = $1",
            &[&gone.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(duties, 0);
}
