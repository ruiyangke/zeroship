//! Lifecycle publication through the manager's signed schedule routes.
//!
//! Control commits intents in its catalog transaction; the publisher delivers
//! them in per-app revision order and records only the manager's exact
//! receipts. These cases run the real manager routes over HTTP against the
//! canonical platform database, with the manager's queue holds written to the
//! same Control deployment ledger the collector reads.

use super::*;
use std::cell::RefCell;
use zeroship_bundle::Manifest;
use zeroship_control::{
    cron::deploy_retention::{Collector, DeployRetentionConfig},
    publication::{
        AcceptanceResult, CatalogError, Transition,
        catalog::{self, ACKNOWLEDGED, PENDING},
        publisher::{Exchange, Publisher, PublisherConfig, ScheduleManager},
    },
};
use zeroship_core::{
    UserId,
    workflow_jobs::{DeploymentId, JobOperation, JobSpec},
    workflow_schedules::{ActivateSchedules, DisableSchedules, RegisterSchedules},
};
use zeroship_workflow_manager::deployments::DeploymentHolds;

use super::deployment_commands::{command, deploy, labelled, sealed, verified};

/// One intent row as `(revision, action, deploy_id, state, receipt)`.
type IntentRow = (i64, String, Option<String>, String, Option<String>);

async fn app(fixture: &Fixture, name: &str) -> AppId {
    let organization = OrganizationId::mint();
    let project = ProjectId::mint();
    let app = AppId::mint();
    let email = format!("{name}@zeroship.test");
    let admin = &fixture.platform.admin;
    admin
        .execute(
            "INSERT INTO zeroship.organizations(id,slug,name,billing_email) \
             VALUES($1,$2::citext,$2,$3::citext)",
            &[&organization.as_str(), &name, &email],
        )
        .await
        .unwrap();
    admin
        .execute(
            "INSERT INTO zeroship.projects(id,organization_id,slug,name) \
             VALUES($1,$2,'default','Publication Project')",
            &[&project.as_str(), &organization.as_str()],
        )
        .await
        .unwrap();
    admin
        .execute(
            "INSERT INTO zeroship.apps(id,name,plan_id,project_id,organization_id) \
             VALUES($1,$2,$3,$4,$5)",
            &[
                &app.as_str(),
                &name,
                &zeroship_control::plan_catalog::free_plan_id(),
                &project.as_str(),
                &organization.as_str(),
            ],
        )
        .await
        .unwrap();
    app
}

/// A passthrough manifest declaring `schedules` over one workflow.
fn scheduled(label: &str, schedules: &[&str]) -> Manifest {
    let mut manifest = labelled(label);
    manifest.workflows = Some(json!(["Nightly"]));
    manifest.schedules = schedules
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "workflowName": "Nightly",
                "input": {"private": "stays in the artifact"},
                "schedule": {"kind": "cron", "cron_expr": "0 0 1 1 *", "tz": "UTC"},
            })
        })
        .collect();
    manifest
}

async fn accept(fixture: &Fixture, app: &AppId, actor: &UserId, manifest: Manifest) -> AcceptanceResult {
    deploy(&fixture.state.registry, app, actor, manifest)
        .await
        .expect("deploy accepted")
        .result()
        .clone()
}

async fn intents(fixture: &Fixture, app: &AppId) -> Vec<IntentRow> {
    fixture
        .platform
        .admin
        .query(
            "SELECT revision, action, deploy_id, state, receipt \
               FROM zeroship.app_lifecycle_intents WHERE app_id=$1 ORDER BY revision",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
        .collect()
}

/// The manager's current selection for `app`: scope revision, whether
/// calendars run, and the selected activation's deployment and revision.
async fn selection(fixture: &Fixture, app: &AppId) -> Option<(i64, bool, Option<(String, i64)>)> {
    let admin = &fixture.platform.admin;
    let scope = admin
        .query(
            "SELECT revision, enabled, activation_id FROM workflow_manager.schedule_scopes \
              WHERE id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let scope = scope.first()?;
    let activation = match scope.get::<_, Option<String>>(2) {
        Some(id) => {
            let row = admin
                .query_one(
                    "SELECT deployment_id, revision FROM workflow_manager.schedule_activations \
                      WHERE id=$1",
                    &[&id],
                )
                .await
                .unwrap();
            Some((row.get(0), row.get(1)))
        }
        None => None,
    };
    Some((scope.get(0), scope.get(1), activation))
}

async fn manager_publisher(
    fixture: &Fixture,
    manager: &test::TestServer,
) -> Publisher<ControlCoordinator> {
    Publisher::new(
        catalog::connect(&fixture.control_url).await.unwrap(),
        ControlCoordinator::new(
            &origin(manager),
            fixture.state.service_auth.clone(),
            Options::default(),
        )
        .unwrap(),
        PublisherConfig::default(),
    )
    .unwrap()
}

/// Publish until nothing is pending, failing on any pass that leaves work.
async fn publish_all(publisher: &mut Publisher<ControlCoordinator>) -> usize {
    let mut acknowledged = 0;
    loop {
        let stats = publisher.tick().await.expect("publication pass");
        assert_eq!(stats.failed, 0, "{stats:?}");
        acknowledged += stats.acknowledged;
        if stats.attempted == 0 {
            return acknowledged;
        }
    }
}

fn activation_receipt(receipt: Option<&str>) -> JobSpec {
    serde_json::from_str(receipt.expect("acknowledged intents keep a receipt"))
        .expect("an activation receipt is the manager's job")
}

/// Deploy, archive, stage while archived, restore and roll back: every
/// transition reaches the manager in revision order, and every recorded
/// receipt is the one the manager returned for that exact request.
#[ntex::test]
async fn lifecycle_intents_reach_the_manager_in_revision_order() {
    let fixture = Fixture::new().await;
    let manager = fixture.coordinator().await;
    let actor = fixture.actor().await;
    let app = app(&fixture, "publication-order").await;

    let first = accept(&fixture, &app, &actor, scheduled("first", &["nightly"])).await;
    assert_eq!(first.lifecycle_revision.map(|r| r.get()), Some(1));
    let archived = fixture.state.registry.archive_app(&app).await.unwrap();
    assert!(archived.unwrap().archived_at.is_some());
    // Archiving again invents no transition.
    fixture.state.registry.archive_app(&app).await.unwrap();
    let staged = accept(&fixture, &app, &actor, scheduled("staged", &["nightly"])).await;
    assert_eq!(staged.lifecycle_revision, None, "an archived app only stages code");
    let restored = fixture.state.registry.unarchive_app(&app).await.unwrap();
    assert!(restored.unwrap().archived_at.is_none());
    fixture.state.registry.unarchive_app(&app).await.unwrap();
    let second = accept(&fixture, &app, &actor, scheduled("second", &["nightly"])).await;
    // An intentional rollback redeploys the first artifact as a new command.
    let rollback = accept(&fixture, &app, &actor, scheduled("first", &["nightly"])).await;
    assert_eq!(rollback.deploy_id, first.deploy_id);
    assert_eq!(rollback.lifecycle_revision.map(|r| r.get()), Some(5));

    let expected = [
        (1, "activate", Some(first.deploy_id.as_str())),
        (2, "disable", None),
        (3, "activate", Some(staged.deploy_id.as_str())),
        (4, "activate", Some(second.deploy_id.as_str())),
        (5, "activate", Some(first.deploy_id.as_str())),
    ];
    let pending = intents(&fixture, &app).await;
    assert_eq!(
        pending
            .iter()
            .map(|row| (row.0, row.1.as_str(), row.2.as_deref()))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(pending.iter().all(|row| row.3 == PENDING && row.4.is_none()));
    assert_eq!(selection(&fixture, &app).await, None, "nothing reached the manager");

    let mut publisher = manager_publisher(&fixture, &manager).await;
    assert_eq!(publish_all(&mut publisher).await, expected.len());
    let published = intents(&fixture, &app).await;
    for (row, (revision, action, deployment)) in published.iter().zip(expected) {
        assert_eq!(row.3, ACKNOWLEDGED, "revision {revision}");
        match action {
            "activate" => {
                let job = activation_receipt(row.4.as_deref());
                assert_eq!(job.app_id, app);
                assert_eq!(
                    job.operation,
                    JobOperation::Activate {
                        deployment_id: DeploymentId::parse(deployment.unwrap()).unwrap(),
                        revision: revision.try_into().unwrap(),
                    }
                );
            }
            _ => assert_eq!(
                serde_json::from_str::<DisableSchedules>(row.4.as_deref().unwrap()).unwrap(),
                DisableSchedules {
                    app_id: app.clone(),
                    revision: revision.try_into().unwrap(),
                }
            ),
        }
    }
    assert_eq!(
        selection(&fixture, &app).await,
        Some((5, true, Some((first.deploy_id.as_str().to_owned(), 5))))
    );
    // Static input never crossed into manager metadata.
    let definitions: Vec<String> = fixture
        .platform
        .admin
        .query(
            "SELECT definition FROM workflow_manager.schedule_deployments WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(definitions.len(), 3);
    assert!(definitions.iter().all(|definition| !definition.contains("private")));
}

/// Removing every schedule is still a normal activation: the manager selects
/// the new deployment and fences the calendar the previous one installed.
#[ntex::test]
async fn an_empty_schedule_list_fences_the_previous_calendar() {
    let fixture = Fixture::new().await;
    let manager = fixture.coordinator().await;
    let actor = fixture.actor().await;
    let app = app(&fixture, "publication-empty").await;
    let mut publisher = manager_publisher(&fixture, &manager).await;

    accept(&fixture, &app, &actor, scheduled("with-schedule", &["nightly"])).await;
    assert_eq!(publish_all(&mut publisher).await, 1);
    let calendar = || async {
        fixture
            .platform
            .admin
            .query_one(
                "SELECT next_at FROM workflow_manager.schedules WHERE app_id=$1 AND name='nightly'",
                &[&app.as_str()],
            )
            .await
            .unwrap()
            .get::<_, Option<i64>>(0)
    };
    assert!(calendar().await.is_some(), "the declared schedule runs");

    let without = accept(&fixture, &app, &actor, labelled("without-schedule")).await;
    let registration: RegisterSchedules = serde_json::from_str(
        &fixture
            .platform
            .admin
            .query_one(
                "SELECT registration FROM zeroship.app_lifecycle_intents WHERE app_id=$1 AND revision=2",
                &[&app.as_str()],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
    )
    .unwrap();
    assert!(registration.schedules.is_empty());
    assert_eq!(publish_all(&mut publisher).await, 1);
    assert_eq!(calendar().await, None, "the removed schedule no longer generates");
    assert_eq!(
        selection(&fixture, &app).await,
        Some((2, true, Some((without.deploy_id.as_str().to_owned(), 2))))
    );
}

/// How the scripted manager answers one app.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// Forward to the real manager.
    Forward,
    /// Forward, then drop the reply as if the connection died.
    LoseReply,
    /// Never answer.
    Block,
}

struct Scripted {
    inner: ControlCoordinator,
    scripts: RefCell<Vec<(AppId, Script)>>,
    calls: RefCell<Vec<(AppId, &'static str, i64)>>,
    lost: RefCell<Vec<JobSpec>>,
}

impl Scripted {
    fn script(&self, app: &AppId) -> Script {
        self.scripts
            .borrow()
            .iter()
            .find(|(scripted, _)| scripted == app)
            .map_or(Script::Forward, |(_, script)| *script)
    }
}

struct Manager(Rc<Scripted>);

impl ScheduleManager for Manager {
    fn register<'a>(&'a self, request: &'a RegisterSchedules) -> Exchange<'a, RegisterSchedules> {
        Box::pin(async move {
            self.0
                .calls
                .borrow_mut()
                .push((request.app_id.clone(), "register", 0));
            if self.0.script(&request.app_id) == Script::Block {
                std::future::pending::<()>().await;
            }
            self.0.inner.register_schedules(request).await
        })
    }
    fn activate<'a>(&'a self, request: &'a ActivateSchedules) -> Exchange<'a, JobSpec> {
        Box::pin(async move {
            self.0
                .calls
                .borrow_mut()
                .push((request.app_id.clone(), "activate", request.revision.get()));
            let job = self.0.inner.activate_schedules(request).await?;
            if self.0.script(&request.app_id) == Script::LoseReply {
                self.0.lost.borrow_mut().push(job);
                return Err(CoordinationError::Unavailable);
            }
            Ok(job)
        })
    }
    fn disable<'a>(&'a self, request: &'a DisableSchedules) -> Exchange<'a, DisableSchedules> {
        Box::pin(async move {
            self.0
                .calls
                .borrow_mut()
                .push((request.app_id.clone(), "disable", request.revision.get()));
            self.0.inner.disable_schedules(request).await
        })
    }
}

fn scripted(fixture: &Fixture, manager: &test::TestServer) -> Rc<Scripted> {
    Rc::new(Scripted {
        inner: ControlCoordinator::new(
            &origin(manager),
            fixture.state.service_auth.clone(),
            Options::default(),
        )
        .unwrap(),
        scripts: RefCell::new(Vec::new()),
        calls: RefCell::new(Vec::new()),
        lost: RefCell::new(Vec::new()),
    })
}

/// A publisher that dies after the catalog commit, a manager reply lost after
/// remote activation, and a confirmation that fails after the reply all leave
/// the intent pending; the next publisher resends the same revision and
/// records the job the manager created the first time.
#[ntex::test]
async fn lost_replies_and_restarts_confirm_only_the_original_receipt() {
    let fixture = Fixture::new().await;
    let manager = fixture.coordinator().await;
    let actor = fixture.actor().await;
    let app = app(&fixture, "publication-restart").await;
    let accepted = accept(&fixture, &app, &actor, scheduled("restart", &["nightly"])).await;

    // Killed after the catalog commit, mid-exchange.
    let script = scripted(&fixture, &manager);
    script.scripts.borrow_mut().push((app.clone(), Script::Block));
    let database = catalog::connect(&fixture.control_url).await.unwrap();
    let mut killed = Publisher::new(
        database,
        Manager(script.clone()),
        PublisherConfig::default(),
    )
    .unwrap();
    assert!(
        compio::time::timeout(Duration::from_millis(500), killed.tick())
            .await
            .is_err(),
        "the blocked exchange holds the pass until it is killed"
    );
    drop(killed);
    assert_eq!(intents(&fixture, &app).await[0].3, PENDING);

    // The manager activates, and the reply is lost.
    script.scripts.borrow_mut().clear();
    script
        .scripts
        .borrow_mut()
        .push((app.clone(), Script::LoseReply));
    let mut losing = Publisher::new(
        catalog::connect(&fixture.control_url).await.unwrap(),
        Manager(script.clone()),
        PublisherConfig::default(),
    )
    .unwrap();
    let stats = losing.tick().await.unwrap();
    assert_eq!((stats.attempted, stats.failed), (1, 1));
    let original = script.lost.borrow()[0].clone();
    assert_eq!(
        original.operation,
        JobOperation::Activate {
            deployment_id: accepted.deploy_id.clone(),
            revision: 1.try_into().unwrap(),
        }
    );
    assert_eq!(intents(&fixture, &app).await[0].3, PENDING);
    assert_eq!(
        selection(&fixture, &app).await.and_then(|s| s.2),
        Some((accepted.deploy_id.as_str().to_owned(), 1)),
        "the manager did activate"
    );

    // The reply arrives but the local confirmation fails.
    fixture
        .platform
        .admin
        .batch_execute(
            "CREATE FUNCTION zeroship.fail_publication_confirm() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN IF NEW.state='acknowledged' THEN RAISE EXCEPTION 'injected confirmation failure'; \
             END IF; RETURN NEW; END $$; \
             CREATE TRIGGER fail_publication_confirm BEFORE UPDATE ON zeroship.app_lifecycle_intents \
             FOR EACH ROW EXECUTE FUNCTION zeroship.fail_publication_confirm();",
        )
        .await
        .unwrap();
    let mut failing = manager_publisher(&fixture, &manager).await;
    let stats = failing.tick().await.unwrap();
    assert_eq!((stats.attempted, stats.failed), (1, 1));
    assert_eq!(intents(&fixture, &app).await[0].3, PENDING);
    fixture
        .platform
        .admin
        .batch_execute("DROP TRIGGER fail_publication_confirm ON zeroship.app_lifecycle_intents")
        .await
        .unwrap();

    // A fresh publisher confirms the job created by the first activation.
    let mut restarted = manager_publisher(&fixture, &manager).await;
    assert_eq!(publish_all(&mut restarted).await, 1);
    let confirmed = intents(&fixture, &app).await;
    assert_eq!(confirmed[0].3, ACKNOWLEDGED);
    assert_eq!(activation_receipt(confirmed[0].4.as_deref()), original);
    let jobs: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT COUNT(*)::bigint FROM workflow_manager.schedule_activations WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(jobs, 1, "retries replayed the one activation");
}

/// One app whose manager exchange never answers does not stop the others,
/// and its later revision is never sent ahead of the blocked one.
#[ntex::test]
async fn a_blocked_app_does_not_starve_other_apps() {
    let fixture = Fixture::new().await;
    let manager = fixture.coordinator().await;
    let actor = fixture.actor().await;
    let mut apps = Vec::new();
    for name in ["publication-a", "publication-b", "publication-c"] {
        let app = app(&fixture, name).await;
        accept(&fixture, &app, &actor, labelled(name)).await;
        apps.push(app);
    }
    apps.sort();
    let blocked = apps[0].clone();
    fixture.state.registry.archive_app(&blocked).await.unwrap();

    let script = scripted(&fixture, &manager);
    script
        .scripts
        .borrow_mut()
        .push((blocked.clone(), Script::Block));
    let mut publisher = Publisher::new(
        catalog::connect(&fixture.control_url).await.unwrap(),
        Manager(script.clone()),
        PublisherConfig {
            attempt_timeout: Duration::from_secs(2),
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let stats = publisher.tick().await.unwrap();
    assert_eq!(
        (stats.attempted, stats.acknowledged, stats.failed, stats.deferred),
        (3, 2, 1, 1),
        "{stats:?}"
    );
    for app in &apps[1..] {
        assert_eq!(intents(&fixture, app).await[0].3, ACKNOWLEDGED);
    }
    let held = intents(&fixture, &blocked).await;
    assert!(held.iter().all(|row| row.3 == PENDING));
    assert!(
        !script
            .calls
            .borrow()
            .iter()
            .any(|(app, kind, _)| app == &blocked && *kind == "disable"),
        "the blocked app's later disable was sent ahead of its activation"
    );

    let mut released = manager_publisher(&fixture, &manager).await;
    assert_eq!(publish_all(&mut released).await, 2);
    assert_eq!(selection(&fixture, &blocked).await.map(|s| (s.0, s.1)), Some((2, false)));
}

/// A refusal from the manager is never recorded as an acknowledgement.
#[ntex::test]
async fn a_manager_conflict_leaves_the_intent_pending() {
    let fixture = Fixture::new().await;
    let manager = fixture.coordinator().await;
    let actor = fixture.actor().await;
    let app = app(&fixture, "publication-conflict").await;
    let accepted = accept(&fixture, &app, &actor, labelled("conflict")).await;
    // Another authority already moved the manager past revision 1.
    let direct = ControlCoordinator::new(
        &origin(&manager),
        fixture.state.service_auth.clone(),
        Options::default(),
    )
    .unwrap();
    direct
        .register_schedules(&RegisterSchedules {
            app_id: app.clone(),
            deployment_id: accepted.deploy_id.clone(),
            schedules: Vec::new(),
        })
        .await
        .unwrap();
    direct
        .disable_schedules(&DisableSchedules {
            app_id: app.clone(),
            revision: 7.try_into().unwrap(),
        })
        .await
        .unwrap();

    let mut publisher = manager_publisher(&fixture, &manager).await;
    let stats = publisher.tick().await.unwrap();
    assert_eq!((stats.attempted, stats.failed), (1, 1));
    let rows = intents(&fixture, &app).await;
    assert_eq!((rows[0].3.as_str(), rows[0].4.as_deref()), (PENDING, None));
}

/// An activation the manager has not acknowledged keeps its bundle even after
/// newer deployments overtake it; once the manager's queue hold exists the
/// hold protects it, and only releasing that hold lets the collector reclaim.
#[ntex::test]
async fn a_pending_activation_keeps_its_bundle_until_the_queue_hold_takes_over() {
    let fixture = Fixture::new().await;
    let manager = fixture.coordinator().await;
    let actor = fixture.actor().await;
    let app = app(&fixture, "publication-retention").await;
    let mut accepted = Vec::new();
    for label in ["retained-a", "retained-b", "retained-c"] {
        let (hash, manifest) = sealed(labelled(label));
        fixture
            .state
            .blob_store
            .put_manifest(&app, &hash, manifest.as_bytes())
            .await
            .unwrap();
        accepted.push(accept(&fixture, &app, &actor, labelled(label)).await);
    }
    // A historical deployment with no pending intent, eligible by every other rule.
    let (control_hash, control_manifest) = sealed(labelled("retained-control"));
    let control = typed_id::generate("dep");
    fixture
        .state
        .blob_store
        .put_manifest(&app, &control_hash, control_manifest.as_bytes())
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.app_deploys(id,app_id,deploy_hash,manifest_json,activated_at) \
             VALUES($1,$2,$3,$4,now()-interval '2 days')",
            &[&control, &app.as_str(), &control_hash, &control_manifest],
        )
        .await
        .unwrap();
    for older in &accepted[..2] {
        fixture
            .platform
            .admin
            .execute(
                "UPDATE zeroship.app_deploys SET activated_at=now()-interval '1 day' WHERE id=$1",
                &[&older.deploy_id.as_str()],
            )
            .await
            .unwrap();
    }
    let unsent = collect_once(&fixture).await;
    assert_eq!(unsent.failed, 0);
    assert_eq!(
        retention(&fixture, &control).await,
        "deleted",
        "the control is reclaimed"
    );
    for older in &accepted[..2] {
        assert_eq!(
            retention(&fixture, older.deploy_id.as_str()).await,
            "available",
            "an unsent activation lost its bundle"
        );
        fixture
            .state
            .blob_store
            .get_manifest(&app, &older.deploy_hash)
            .await
            .unwrap();
    }

    let mut publisher = manager_publisher(&fixture, &manager).await;
    assert_eq!(publish_all(&mut publisher).await, 3);
    assert!(intents(&fixture, &app).await.iter().all(|row| row.3 == ACKNOWLEDGED));
    collect_once(&fixture).await;
    for older in &accepted[..2] {
        assert_eq!(
            retention(&fixture, older.deploy_id.as_str()).await,
            "available",
            "the queue hold must protect an acknowledged activation"
        );
    }

    let ledger = DeploymentHolds::new(database(&fixture.control_url).await).unwrap();
    ledger
        .release(
            &HoldScope::for_queue(app.clone()),
            accepted[0].deploy_id.as_str(),
            generation(1),
        )
        .await
        .unwrap();
    collect_once(&fixture).await;
    assert_eq!(retention(&fixture, accepted[0].deploy_id.as_str()).await, "deleted");
    assert_eq!(retention(&fixture, accepted[1].deploy_id.as_str()).await, "available");
    assert!(matches!(
        fixture
            .state
            .blob_store
            .get_manifest(&app, &accepted[0].deploy_hash)
            .await,
        Err(zeroship_bundle::BlobError::NotFound(_))
    ));
}

/// A fault raised inside the catalog callback after every write is staged
/// rolls back the pointer, the deployment row, the revision, the intent and
/// the receipt together; archive and restore behave the same way.
#[ntex::test]
async fn a_callback_fault_rolls_back_every_staged_catalog_write() {
    let fixture = Fixture::new().await;
    let actor = fixture.actor().await;
    let app = app(&fixture, "publication-rollback").await;
    let database = catalog::connect(&fixture.control_url).await.unwrap();
    let empty = snapshot(&fixture, &app).await;
    let faulted = command(&app, &actor, verified(labelled("faulted")));
    let outcome = catalog::transact(&database, |tx| async move {
        let accepted = catalog::accept(&tx, &faulted, zeroship_control::publication::now_millis())
            .await?;
        assert!(!accepted.replayed());
        Err::<(), _>(CatalogError::Storage("injected fault after staged writes"))
    })
    .await;
    assert!(matches!(outcome, Err(CatalogError::Storage(_))));
    assert_eq!(snapshot(&fixture, &app).await, empty);

    accept(&fixture, &app, &actor, labelled("committed")).await;
    let committed = snapshot(&fixture, &app).await;
    let target = &app;
    for restore in [false, true] {
        if restore {
            fixture.state.registry.archive_app(target).await.unwrap();
        }
        let before = snapshot(&fixture, target).await;
        let outcome = catalog::transact(&database, |tx| async move {
            let now = zeroship_control::publication::now_millis();
            let transition = if restore {
                catalog::restore(&tx, target, now).await?
            } else {
                catalog::archive(&tx, target, now).await?
            };
            assert!(matches!(transition, Some(Transition::Published(_))));
            Err::<(), _>(CatalogError::Storage("injected fault after staged writes"))
        })
        .await;
        assert!(matches!(outcome, Err(CatalogError::Storage(_))));
        assert_eq!(snapshot(&fixture, &app).await, before, "restore={restore}");
    }
    assert_eq!(committed.1 + 1, snapshot(&fixture, &app).await.1, "only the real archive advanced");
}

async fn retention(fixture: &Fixture, id: &str) -> String {
    fixture
        .platform
        .admin
        .query_one(
            "SELECT retention_state FROM zeroship.app_deploys WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

async fn collect_once(
    fixture: &Fixture,
) -> zeroship_control::cron::deploy_retention::DeployRetentionStats {
    Collector::connect(
        &fixture.control_url,
        fixture.state.blob_store.clone(),
        DeployRetentionConfig {
            grace_window_ms: 0,
            batch_size: 128,
            attempt_timeout: Duration::from_secs(10),
        },
    )
    .await
    .unwrap()
    .tick()
    .await
    .unwrap()
}

/// The app pointer, lifecycle revision and archive marker, with the app's
/// deployment, receipt and intent row counts.
async fn snapshot(fixture: &Fixture, app: &AppId) -> (Option<String>, i64, bool, i64, i64, i64) {
    let admin = &fixture.platform.admin;
    let row = admin
        .query_one(
            "SELECT deploy_hash, lifecycle_revision, archived_at IS NOT NULL, \
                    (SELECT COUNT(*)::bigint FROM zeroship.app_deploys WHERE app_id=$1), \
                    (SELECT COUNT(*)::bigint FROM zeroship.app_deploy_commands WHERE app_id=$1), \
                    (SELECT COUNT(*)::bigint FROM zeroship.app_lifecycle_intents WHERE app_id=$1) \
               FROM zeroship.apps WHERE id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    (
        row.get(0),
        row.get(1),
        row.get(2),
        row.get(3),
        row.get(4),
        row.get(5),
    )
}
