//! Control's terminal deletion, read over the app-facts capability, makes the
//! driver abandon an app's recovery responsibility instead of closing it.
#![expect(
    clippy::future_not_send,
    reason = "platform fixtures use their compio runtime"
)]

#[path = "support/app_facts.rs"]
mod app_facts;
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
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    capacity::LocalCapacity,
    coordinator::{Coordinator, Options as CoordinatorOptions},
    driver::{Driver, Options as DriverOptions},
    lifecycle::{AppLifecycle, FactsLifecycle},
    recovery::{Options as RecoveryOptions, Recovery, ScopeState},
    Options, Queue,
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

/// The facts source stands in for Control, so it reads with a credential that
/// can. The workflow role holds no grant on the policy inputs, and the closing
/// lane's deletion check crosses the same endpoint they do.
async fn connect_lifecycle(url: &str) -> FactsLifecycle {
    let control = url.replacen("zeroship_workflow@", "postgres@", 1);
    FactsLifecycle::new(app_facts::DatabaseAppFacts::connect(&control).await)
}

async fn queue(platform: &platform::Platform) -> Queue {
    Queue::connect(
        DbBinding::platform(
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

/// Only a RECORDED deletion abandons. An app Control has no row for is absent
/// from the answer, and absence is not deletion: the closing lane must not
/// abandon an app because a facts read did not mention it.
///
/// That distinction is the whole reason the wire contract omits unknown apps
/// rather than reporting them, so this drives all three states at once - live,
/// deleted, and unknown to Control - against one answer.
#[compio::test]
async fn only_a_recorded_deletion_abandons_and_an_unknown_app_does_not() {
    let platform = platform::Platform::new().await;
    let live = AppId::mint();
    let gone = AppId::mint();
    seed(&platform, &live, "lifecycle-live").await;
    seed(&platform, &gone, "lifecycle-gone").await;
    let source = connect_lifecycle(&platform.runtime_url).await;
    let unknown = AppId::mint();
    let apps = [live.clone(), gone.clone(), unknown.clone()];
    assert!(
        source.deleted(&apps).await.unwrap().is_empty(),
        "no app is deleted yet, and the unknown one is not deleted either"
    );
    delete(&platform, &gone).await;
    assert_eq!(
        source.deleted(&apps).await.unwrap(),
        BTreeSet::from([gone.clone()]),
        "exactly the recorded deletion"
    );
    // The unknown app stays out of the answer however many times it is asked
    // about, including when it is the ONLY app asked about. A source that
    // reported an absence as a deletion would abandon here.
    assert!(source.deleted(&[unknown]).await.unwrap().is_empty());
    // An empty request asks nothing and answers nothing, without an exchange.
    assert!(source.deleted(&[]).await.unwrap().is_empty());
    // Archival is not deletion: the marker is `deleted_at`, and an app that is
    // merely archived stays placeable for maintenance jobs.
    platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET archived_at = now() WHERE id = $1",
            &[&live.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        source.deleted(&apps).await.unwrap(),
        BTreeSet::from([gone]),
        "an archived app is not abandoned"
    );
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
        Rc::new(LocalCapacity),
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
