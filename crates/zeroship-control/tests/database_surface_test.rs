//! The control-plane database surface, against a live PostgreSQL.
//!
//! Every case here is about a REFUSAL, and every refusal is paired with a
//! CONTROL that differs in exactly one variable - otherwise a green says only
//! that the operation is hard to reach, not that the guard is the thing
//! reaching it.
//!
//! # What each group binds
//!
//! - **Placement.** `zeroship_control::databases::place` chooses the active
//!   datastore in the project's zone carrying the fewest databases, ties broken
//!   by id. The refusals are a zone with no active cluster and a cluster that
//!   is active in ANOTHER zone; the control in both cases is one active cluster
//!   in the right zone.
//! - **The status ceiling.** No reconciler exists, so no schema and no role is
//!   ever created. Every row this surface can produce is asserted to stop at
//!   `provisioning` / `pending` - read back from PostgreSQL rather than from
//!   the returned record, because the record is what the module says and the
//!   row is what it did.
//! - **Authority.** The Cedar band is `developer` for writes. The rank
//!   predicate rides in each effect statement, so what these bind is that a row
//!   did not appear, not that a handler returned a status.
//! - **The project fence.** A binding may only join an app and a database in
//!   one project. The two composite foreign keys enforce it; these bind that a
//!   creator gets a refusal naming the mismatch rather than a 500.
//!
//! # Every test owns its own execution zone, and cleans it up BEFORE asserting
//!
//! `Registry::create_app` refuses to choose when a deployment declares more
//! than one active zone, so a leaked zone row breaks every sibling module in
//! this binary. Placement filters on the zone, so a private zone also makes one
//! test's datastores invisible to the next. Results are therefore collected,
//! [`World::cleanup`] runs, and only then are the assertions made - the idiom
//! `worker_join_test` already uses for its probe zone.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_control::databases::{
    self, BindDatabaseBody, CreateDatabaseBody, DatabaseError, CAPABILITY_READONLY,
    CAPABILITY_READWRITE,
};
use zeroship_control::organizations::{self, CreateOrganizationBody};
use zeroship_control::Registry;
use zeroship_core::{AppId, DatabaseId, UserId};

use crate::common;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fx {
    registry: Registry,
    pg: Client,
}

impl Fx {
    async fn new() -> Self {
        let url = common::require_control_db();
        let (pg, conn) = connect(&url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        let registry = Registry::new(&url).await.expect("registry");
        common::ensure_builtin_plans(&registry).await;
        Self { registry, pg }
    }
}

/// One organization, one project, and one execution zone owned by this test.
///
/// The organization is minted through the production path so the fixture and
/// the product cannot disagree about what a fresh organization looks like. The
/// PROJECT is inserted directly, because `organizations::create_project` takes
/// the zone from the column default and this fixture's whole point is to place
/// the project in a zone of its own.
struct World {
    zone_id: String,
    organization_id: String,
    project_id: String,
    owner: UserId,
    users: Vec<UserId>,
    apps: Vec<AppId>,
    projects: Vec<String>,
}

impl World {
    async fn new(fx: &Fx, label: &str) -> Self {
        let zone_id = zeroship_core::typed_id::generate("ezn");
        let zone_name = format!("{label}-zone-{}", Uuid::new_v4().simple());
        fx.pg
            .execute(
                "INSERT INTO zeroship.execution_zones (id, name, status) \
                 VALUES ($1, $2, 'active')",
                &[&zone_id, &zone_name],
            )
            .await
            .expect("declare this test's execution zone");

        let owner = seed_user(&fx.pg, label).await;
        let organization = organizations::create_organization(
            &fx.registry,
            &owner,
            &CreateOrganizationBody {
                name: format!("{label} {}", Uuid::new_v4().simple()),
                slug: Some(format!("{label}-{}", Uuid::new_v4().simple())),
                billing_email: None,
            },
            None,
        )
        .await
        .expect("create organization");

        let mut world = Self {
            zone_id,
            organization_id: organization.id,
            project_id: String::new(),
            owner: owner.clone(),
            users: vec![owner],
            apps: Vec::new(),
            projects: Vec::new(),
        };
        world.project_id = world.project(fx, "main").await;
        world
    }

    /// A project inside this world's organization AND this world's zone.
    async fn project(&mut self, fx: &Fx, label: &str) -> String {
        let project_id = zeroship_core::typed_id::generate("prj");
        fx.pg
            .execute(
                "INSERT INTO zeroship.projects \
                     (id, organization_id, slug, name, execution_zone_id) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &project_id,
                    &self.organization_id,
                    &format!("{label}-{}", Uuid::new_v4().simple()),
                    &label,
                    &self.zone_id,
                ],
            )
            .await
            .expect("seed project in this test's zone");
        self.projects.push(project_id.clone());
        project_id
    }

    /// Register a cluster in a zone, the way the service holding its credential
    /// would. Control never inserts one, so this is the operator's side of the
    /// world and it is spelled in the fixture rather than reached through the
    /// module under test.
    async fn datastore(&self, fx: &Fx, zone_id: &str, status: &str) -> String {
        let id = zeroship_core::typed_id::generate("dst");
        self.datastore_with_id(fx, &id, zone_id, status).await;
        id
    }

    async fn datastore_with_id(&self, fx: &Fx, id: &str, zone_id: &str, status: &str) {
        // `datastores_system_identifier_key` is the natural key, so every
        // fixture cluster needs a distinct one.
        let system_identifier: i64 = i64::from(u32::from_le_bytes(
            Uuid::new_v4().as_bytes()[..4]
                .try_into()
                .expect("four bytes"),
        )) + i64::from(std::process::id());
        fx.pg
            .execute(
                "INSERT INTO zeroship.datastores \
                     (id, system_identifier, execution_zone_id, status) \
                 VALUES ($1, $2, $3, $4)",
                &[&id, &system_identifier, &zone_id, &status],
            )
            .await
            .expect("register a fixture datastore");
    }

    async fn set_datastore_status(&self, fx: &Fx, datastore_id: &str, status: &str) {
        let updated = fx
            .pg
            .execute(
                "UPDATE zeroship.datastores SET status = $2 WHERE id = $1",
                &[&datastore_id, &status],
            )
            .await
            .expect("flip a fixture datastore status");
        assert_eq!(updated, 1, "the fixture datastore must exist to be flipped");
    }

    /// An app inside one of this world's projects.
    ///
    /// Written directly rather than through `Registry::create_app`, which
    /// would resolve a zone by name: this world declares a second active zone
    /// for the duration of the test, and naming this one here keeps the app's
    /// zone the project's zone whatever else is declared.
    async fn app(&mut self, fx: &Fx, project_id: &str, label: &str) -> AppId {
        let app_id = AppId::mint();
        fx.pg
            .execute(
                "INSERT INTO zeroship.apps \
                     (id, name, plan_id, project_id, organization_id, execution_zone_id) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &app_id.as_str(),
                    &format!("{label}-{}", Uuid::new_v4().simple()),
                    &zeroship_control::plan_catalog::free_plan_id(),
                    &project_id,
                    &self.organization_id,
                    &self.zone_id,
                ],
            )
            .await
            .expect("seed app in this test's project");
        self.apps.push(app_id.clone());
        app_id
    }

    /// Seat a fresh user in this world's organization at `role`, through the
    /// owner's authority so the seating is never the thing under test.
    async fn seat(&mut self, fx: &Fx, label: &str, role: &str) -> UserId {
        let user = seed_user(&fx.pg, label).await;
        organizations::add_member(
            &fx.registry,
            &self.owner,
            &self.organization_id,
            &organizations::AddMemberBody {
                user_id: user.clone(),
                role: role.to_string(),
            },
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("seat {label} as {role}: {err:?}"));
        self.users.push(user.clone());
        user
    }

    /// Seat an already-seated member on one PROJECT at `role`.
    ///
    /// Without this the member's effective project rank is ZERO, not their
    /// organization rank: `effective_project_rank` narrows to
    /// `min(organization, project)` below admin, and a NULL project rank
    /// narrows to nothing. That is the whole point of per-project narrowing,
    /// so a fixture that skipped it would be testing a caller with no reach at
    /// all and reading the refusal as the rank guard.
    async fn seat_on_project(&self, fx: &Fx, user: &UserId, project_id: &str, role: &str) {
        organizations::add_project_member(
            &fx.registry,
            &self.owner,
            project_id,
            &organizations::AddProjectMemberBody {
                user_id: user.clone(),
                role: role.to_string(),
            },
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("seat {} on {project_id} as {role}: {err:?}", user.as_str()));
    }

    async fn database_status(&self, fx: &Fx, database_id: &DatabaseId) -> Option<String> {
        fx.pg
            .query(
                "SELECT status FROM zeroship.databases WHERE id = $1",
                &[&database_id.as_str()],
            )
            .await
            .expect("read database status")
            .first()
            .map(|row| row.get("status"))
    }

    async fn binding_status(&self, fx: &Fx, database_id: &DatabaseId, app: &AppId) -> Option<String> {
        fx.pg
            .query(
                "SELECT status FROM zeroship.database_bindings \
                  WHERE database_id = $1 AND app_id = $2",
                &[&database_id.as_str(), &app.as_str()],
            )
            .await
            .expect("read binding status")
            .first()
            .map(|row| row.get("status"))
    }

    async fn database_count(&self, fx: &Fx, project_id: &str) -> i64 {
        fx.pg
            .query_one(
                "SELECT count(*) AS n FROM zeroship.databases WHERE project_id = $1",
                &[&project_id],
            )
            .await
            .expect("count databases")
            .get("n")
    }

    /// Remove every row this world wrote, innermost first: the RESTRICT keys
    /// between bindings, databases, datastores, apps, projects and the zone are
    /// the design, so the order here is the ladder read upwards.
    async fn cleanup(&self, fx: &Fx) {
        let pg = &fx.pg;
        for project in self.projects.iter().chain(std::iter::once(&self.project_id)) {
            let _ = pg
                .execute(
                    "DELETE FROM zeroship.database_bindings WHERE project_id = $1",
                    &[project],
                )
                .await;
            let _ = pg
                .execute(
                    "DELETE FROM zeroship.databases WHERE project_id = $1",
                    &[project],
                )
                .await;
        }
        let _ = pg
            .execute(
                "DELETE FROM zeroship.datastores WHERE execution_zone_id = $1",
                &[&self.zone_id],
            )
            .await;
        for app in &self.apps {
            let _ = pg
                .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app.as_str()])
                .await;
        }
        for project in &self.projects {
            let _ = pg
                .execute("DELETE FROM zeroship.projects WHERE id = $1", &[project])
                .await;
        }
        let _ = pg
            .execute(
                "DELETE FROM zeroship.app_audit WHERE resource = $1",
                &[&self.organization_id],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.organizations WHERE id = $1",
                &[&self.organization_id],
            )
            .await;
        for user in &self.users {
            let _ = pg
                .execute(
                    "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                    &[&user.as_str()],
                )
                .await;
            let _ = pg
                .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.as_str()])
                .await;
        }
        let removed = pg
            .execute(
                "DELETE FROM zeroship.execution_zones WHERE id = $1",
                &[&self.zone_id],
            )
            .await
            .expect("remove this test's execution zone");
        assert_eq!(
            removed, 1,
            "the probe zone {} survived cleanup. Every sibling test in this binary that creates \
             an app without naming a zone now fails, because a deployment with more than one \
             active zone refuses to choose.",
            self.zone_id
        );
    }

    /// Delete an extra zone this test declared beyond its own.
    async fn drop_zone(&self, fx: &Fx, zone_id: &str) {
        let _ = fx
            .pg
            .execute(
                "DELETE FROM zeroship.datastores WHERE execution_zone_id = $1",
                &[&zone_id],
            )
            .await;
        let removed = fx
            .pg
            .execute(
                "DELETE FROM zeroship.execution_zones WHERE id = $1",
                &[&zone_id],
            )
            .await
            .expect("remove a probe zone");
        assert_eq!(removed, 1, "probe zone {zone_id} survived cleanup");
    }
}

async fn seed_user(pg: &Client, label: &str) -> UserId {
    let id = UserId::mint();
    let email = format!("{label}-{}@zeroship.test", id.as_str());
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[&id.as_str(), &email, &label],
    )
    .await
    .expect("insert user");
    id
}

fn create_body(name: &str) -> CreateDatabaseBody {
    CreateDatabaseBody {
        name: name.to_string(),
    }
}

fn bind_body(app: &AppId, capability: &str) -> BindDatabaseBody {
    BindDatabaseBody {
        app_id: app.as_str().to_string(),
        capability: capability.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Placement picks the emptiest active cluster, and breaks a tie by id.
///
/// The two datastores are registered LARGER ID FIRST, so insertion order is the
/// opposite of id order. Without `ORDER BY ..., d.id` the first create would
/// land wherever the scan happened to reach first; with it, the first create is
/// pinned to the smaller id. The second create then proves the COUNT arm - one
/// cluster now holds a database, so the other must win - and the third returns
/// to the smaller id with the counts level again, which no single arm produces
/// on its own.
#[compio::test]
async fn placement_fills_the_emptiest_cluster_and_breaks_ties_by_id() {
    let fx = Fx::new().await;
    let world = World::new(&fx, "place-balance").await;

    let mut ids = [
        zeroship_core::typed_id::generate("dst"),
        zeroship_core::typed_id::generate("dst"),
    ];
    ids.sort();
    let (smaller, larger) = (ids[0].clone(), ids[1].clone());
    world
        .datastore_with_id(&fx, &larger, &world.zone_id, "active")
        .await;
    world
        .datastore_with_id(&fx, &smaller, &world.zone_id, "active")
        .await;

    let mut landed = Vec::new();
    for name in ["one", "two", "three"] {
        let record = databases::create_database(
            &fx.registry,
            &world.owner,
            &world.project_id,
            &create_body(name),
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("create {name}: {err:?}"));
        landed.push(record.datastore_id);
    }

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    assert_eq!(
        landed,
        vec![smaller.clone(), larger.clone(), smaller.clone()],
        "placement must fill the emptiest cluster and break a tie by id"
    );
}

/// A zone whose clusters are all `pending` or `draining` refuses, and the
/// control is the SAME call once one of them is `active`.
#[compio::test]
async fn placement_refuses_a_zone_with_no_active_cluster() {
    let fx = Fx::new().await;
    let world = World::new(&fx, "place-nocap").await;

    let bootstrapping = world.datastore(&fx, &world.zone_id, "pending").await;
    world.datastore(&fx, &world.zone_id, "draining").await;

    let refused = databases::create_database(
        &fx.registry,
        &world.owner,
        &world.project_id,
        &create_body("main"),
        None,
    )
    .await;
    let rows_after_refusal = world.database_count(&fx, &world.project_id).await;

    // CONTROL: one variable moves - the bootstrapping cluster becomes active.
    world
        .set_datastore_status(&fx, &bootstrapping, "active")
        .await;
    let admitted = databases::create_database(
        &fx.registry,
        &world.owner,
        &world.project_id,
        &create_body("main"),
        None,
    )
    .await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    match refused {
        Err(DatabaseError::NoDatastoreInZone { execution_zone_id }) => {
            assert_eq!(execution_zone_id, world.zone_id);
        }
        other => panic!("a zone with no active cluster must refuse, got {other:?}"),
    }
    assert_eq!(
        rows_after_refusal, 0,
        "a refused placement must write no database row"
    );
    let record = admitted.expect("the control must be admitted");
    assert_eq!(
        record.datastore_id, bootstrapping,
        "the control must land on the cluster that became active"
    );
}

/// An active cluster in ANOTHER zone is never chosen, and the control is one
/// active cluster in the project's own zone.
///
/// This is the fence the composite key `databases_placement_fkey` exists for:
/// a database is placed on a cluster in its project's zone, structurally. The
/// refusal here proves placement never offers the key a pair it would have to
/// reject.
#[compio::test]
async fn placement_never_reaches_a_cluster_in_another_zone() {
    let fx = Fx::new().await;
    let world = World::new(&fx, "place-zone").await;

    let elsewhere_id = zeroship_core::typed_id::generate("ezn");
    let elsewhere_name = format!("elsewhere-{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.execution_zones (id, name, status) VALUES ($1, $2, 'active')",
            &[&elsewhere_id, &elsewhere_name],
        )
        .await
        .expect("declare a second zone");
    world.datastore(&fx, &elsewhere_id, "active").await;

    let refused = databases::create_database(
        &fx.registry,
        &world.owner,
        &world.project_id,
        &create_body("main"),
        None,
    )
    .await;

    // CONTROL: one variable moves - an active cluster appears in the
    // project's OWN zone.
    let here = world.datastore(&fx, &world.zone_id, "active").await;
    let admitted = databases::create_database(
        &fx.registry,
        &world.owner,
        &world.project_id,
        &create_body("main"),
        None,
    )
    .await;

    world.cleanup(&fx).await;
    world.drop_zone(&fx, &elsewhere_id).await;
    drop(fx);
    common::drain_pg().await;

    assert!(
        matches!(refused, Err(DatabaseError::NoDatastoreInZone { .. })),
        "an active cluster in another zone must not be reachable, got {refused:?}"
    );
    let record = admitted.expect("the control must be admitted");
    assert_eq!(record.datastore_id, here);
    assert_eq!(
        record.execution_zone_id, world.zone_id,
        "the database's zone must be its project's zone"
    );
}

// ---------------------------------------------------------------------------
// The status ceiling
// ---------------------------------------------------------------------------

/// Nothing this surface writes reaches a converged state.
///
/// Read back from PostgreSQL rather than from the returned records: the record
/// is what the module says, the row is what it did. `schema_epoch` is asserted
/// at zero for the same reason - it is a role-name input the cluster owns, and
/// Control minting one would be a claim about roles that do not exist.
#[compio::test]
async fn a_created_database_and_its_binding_stop_short_of_convergence() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "ceiling").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let app = world.app(&fx, &world.project_id.clone(), "ceiling-app").await;

    let created = databases::create_database(
        &fx.registry,
        &world.owner,
        &world.project_id,
        &create_body("main"),
        None,
    )
    .await
    .expect("create database");
    let database_id = DatabaseId::parse(&created.id).expect("a minted database id");
    let bound = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&app, CAPABILITY_READWRITE),
        None,
    )
    .await
    .expect("bind app");

    let stored_database = world.database_status(&fx, &database_id).await;
    let stored_binding = world.binding_status(&fx, &database_id, &app).await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    assert_eq!(stored_database.as_deref(), Some("provisioning"));
    assert_eq!(stored_binding.as_deref(), Some("pending"));
    assert_eq!(created.schema_epoch, 0, "Control mints no schema epoch");
    assert_eq!(bound.generation, 1);
    assert_eq!(
        bound.observed_generation, 0,
        "no reconciler has converged this binding"
    );
    assert_eq!(bound.capability, CAPABILITY_READWRITE);
}

// ---------------------------------------------------------------------------
// Authority
// ---------------------------------------------------------------------------

/// Every mutation refuses a viewer and admits a developer.
///
/// One test for all four verbs, each arm a refusal paired with the SAME call by
/// a developer. What it binds is that the rank predicate is inside each effect
/// statement: after every refusal the world is read back, so an arm that
/// "refused" while writing its row fails here rather than passing on the error
/// type alone.
#[compio::test]
async fn every_mutation_refuses_a_viewer_and_admits_a_developer() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "authority").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let project_id = world.project_id.clone();
    let app = world.app(&fx, &project_id, "authority-app").await;
    let viewer = world.seat(&fx, "viewer", "viewer").await;
    let developer = world.seat(&fx, "developer", "developer").await;
    // BOTH seats, or neither caller reaches the project at all: below admin the
    // effective rank is `min(organization, project)` and a missing project row
    // narrows to zero. Seating both at their own role makes the ONE variable
    // between these two callers their rank, which is what this test is about.
    world
        .seat_on_project(&fx, &viewer, &project_id, "viewer")
        .await;
    world
        .seat_on_project(&fx, &developer, &project_id, "developer")
        .await;

    let create_refused = databases::create_database(
        &fx.registry,
        &viewer,
        &project_id,
        &create_body("main"),
        None,
    )
    .await;
    let after_create_refusal = world.database_count(&fx, &project_id).await;

    let created = databases::create_database(
        &fx.registry,
        &developer,
        &project_id,
        &create_body("main"),
        None,
    )
    .await
    .expect("a developer may create a database");
    let database_id = DatabaseId::parse(&created.id).expect("a minted database id");

    let bind_refused = databases::bind_database(
        &fx.registry,
        &viewer,
        &database_id,
        &bind_body(&app, CAPABILITY_READONLY),
        None,
    )
    .await;
    let after_bind_refusal = world.binding_status(&fx, &database_id, &app).await;

    databases::bind_database(
        &fx.registry,
        &developer,
        &database_id,
        &bind_body(&app, CAPABILITY_READONLY),
        None,
    )
    .await
    .expect("a developer may bind an app");

    let unbind_refused =
        databases::unbind_database(&fx.registry, &viewer, &database_id, &app, None).await;
    let after_unbind_refusal = world.binding_status(&fx, &database_id, &app).await;

    databases::unbind_database(&fx.registry, &developer, &database_id, &app, None)
        .await
        .expect("a developer may unbind an app");

    let delete_refused =
        databases::delete_database(&fx.registry, &viewer, &database_id, None).await;
    let after_delete_refusal = world.database_status(&fx, &database_id).await;

    let deleted = databases::delete_database(&fx.registry, &developer, &database_id, None).await;
    let after_delete = world.database_status(&fx, &database_id).await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    for (label, outcome) in [
        ("create", &create_refused.map(|record| record.id)),
        ("bind", &bind_refused.map(|record| record.id)),
    ] {
        assert!(
            matches!(outcome, Err(DatabaseError::Insufficient(_))),
            "a viewer must not {label} a database, got {outcome:?}"
        );
    }
    for (label, outcome) in [
        ("unbind", &unbind_refused),
        ("delete", &delete_refused),
    ] {
        assert!(
            matches!(outcome, Err(DatabaseError::Insufficient(_))),
            "a viewer must not {label}, got {outcome:?}"
        );
    }
    assert_eq!(
        after_create_refusal, 0,
        "a refused create must write no database row"
    );
    assert_eq!(
        after_bind_refusal, None,
        "a refused bind must write no binding row"
    );
    assert_eq!(
        after_unbind_refusal.as_deref(),
        Some("pending"),
        "a refused unbind must leave the binding in place"
    );
    assert_eq!(
        after_delete_refusal.as_deref(),
        Some("provisioning"),
        "a refused delete must leave the database in place"
    );
    deleted.expect("a developer may delete an unbound database");
    assert_eq!(after_delete, None, "the admitted delete removed the row");
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// A bound database refuses deletion, and the refusal names every app to
/// unbind. The control is the SAME call once the bindings are gone.
#[compio::test]
async fn deleting_a_bound_database_is_refused_and_names_the_apps() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "delete-bound").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let project_id = world.project_id.clone();
    let writer = world.app(&fx, &project_id, "writer").await;
    let reader = world.app(&fx, &project_id, "reader").await;

    let created = databases::create_database(
        &fx.registry,
        &world.owner,
        &project_id,
        &create_body("main"),
        None,
    )
    .await
    .expect("create database");
    let database_id = DatabaseId::parse(&created.id).expect("a minted database id");
    for (app, capability) in [
        (&writer, CAPABILITY_READWRITE),
        (&reader, CAPABILITY_READONLY),
    ] {
        databases::bind_database(
            &fx.registry,
            &world.owner,
            &database_id,
            &bind_body(app, capability),
            None,
        )
        .await
        .expect("bind app");
    }

    let refused =
        databases::delete_database(&fx.registry, &world.owner, &database_id, None).await;
    let survived = world.database_status(&fx, &database_id).await;

    // CONTROL: one variable moves - the bindings go.
    for app in [&writer, &reader] {
        databases::unbind_database(&fx.registry, &world.owner, &database_id, app, None)
            .await
            .expect("unbind app");
    }
    let admitted =
        databases::delete_database(&fx.registry, &world.owner, &database_id, None).await;
    let after_delete = world.database_status(&fx, &database_id).await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    let Err(DatabaseError::DatabaseHasBindings(bound)) = refused else {
        panic!("a bound database must refuse deletion, got {refused:?}");
    };
    let mut named: Vec<(String, String)> = bound
        .iter()
        .map(|app| (app.app_id.clone(), app.capability.clone()))
        .collect();
    named.sort();
    let mut expected = vec![
        (writer.as_str().to_owned(), CAPABILITY_READWRITE.to_owned()),
        (reader.as_str().to_owned(), CAPABILITY_READONLY.to_owned()),
    ];
    expected.sort();
    assert_eq!(
        named, expected,
        "the refusal must name every bound app and the capability it holds"
    );
    assert!(
        bound.iter().all(|app| !app.app_name.is_empty()),
        "the refusal must carry a readable app name, not only an id"
    );
    assert_eq!(
        survived.as_deref(),
        Some("provisioning"),
        "a refused delete must leave the row"
    );
    admitted.expect("an unbound database deletes");
    assert_eq!(after_delete, None);
}

// ---------------------------------------------------------------------------
// The project fence
// ---------------------------------------------------------------------------

/// A binding may only join an app and a database in ONE project.
///
/// Three refusals share one control. The app in a sibling project and the
/// DELETED app are both `AppNotInProject`, because a deleted app has left its
/// project and there is nothing left to tell "gone" from "elsewhere" apart. The
/// control is an app in the database's own project, which binds.
#[compio::test]
async fn binding_refuses_an_app_outside_the_databases_project() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "fence").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let project_id = world.project_id.clone();
    let sibling_project = world.project(&fx, "sibling").await;

    let here = world.app(&fx, &project_id, "here").await;
    let elsewhere = world.app(&fx, &sibling_project, "elsewhere").await;
    let deleted = world.app(&fx, &project_id, "deleted").await;
    fx.pg
        .execute(
            "UPDATE zeroship.apps SET project_id = NULL, archived_at = NOW(), \
             deleted_at = NOW() WHERE id = $1",
            &[&deleted.as_str()],
        )
        .await
        .expect("detach an app from its project the way delete_app does");

    let created = databases::create_database(
        &fx.registry,
        &world.owner,
        &project_id,
        &create_body("main"),
        None,
    )
    .await
    .expect("create database");
    let database_id = DatabaseId::parse(&created.id).expect("a minted database id");

    let cross_project = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&elsewhere, CAPABILITY_READWRITE),
        None,
    )
    .await;
    let detached = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&deleted, CAPABILITY_READWRITE),
        None,
    )
    .await;
    let unknown = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&AppId::mint(), CAPABILITY_READWRITE),
        None,
    )
    .await;
    let bindings_after_refusals = fx
        .pg
        .query_one(
            "SELECT count(*) AS n FROM zeroship.database_bindings WHERE database_id = $1",
            &[&database_id.as_str()],
        )
        .await
        .expect("count bindings")
        .get::<_, i64>("n");

    // CONTROL: one variable moves - the app is in the database's own project.
    let admitted = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&here, CAPABILITY_READWRITE),
        None,
    )
    .await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    for (label, outcome) in [
        ("an app in a sibling project", &cross_project),
        ("a deleted app", &detached),
        ("an app that never existed", &unknown),
    ] {
        match outcome {
            Err(DatabaseError::AppNotInProject {
                project_id: named, ..
            }) => assert_eq!(
                named, &project_id,
                "{label} must be refused naming the database's project"
            ),
            other => panic!("{label} must be refused, got {other:?}"),
        }
    }
    assert_eq!(
        bindings_after_refusals, 0,
        "a refused bind must write no binding row"
    );
    let record = admitted.expect("an app in the same project binds");
    assert_eq!(record.app_id, here.as_str());
    assert_eq!(record.project_id, project_id);
}

/// A second binding of the same app is refused and names the live capability,
/// because a grant is explicit and is never silently replaced. The control is a
/// different app, which binds.
#[compio::test]
async fn a_second_binding_of_one_app_is_refused_with_its_live_capability() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "rebind").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let project_id = world.project_id.clone();
    let first = world.app(&fx, &project_id, "first").await;
    let second = world.app(&fx, &project_id, "second").await;

    let created = databases::create_database(
        &fx.registry,
        &world.owner,
        &project_id,
        &create_body("main"),
        None,
    )
    .await
    .expect("create database");
    let database_id = DatabaseId::parse(&created.id).expect("a minted database id");
    databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&first, CAPABILITY_READWRITE),
        None,
    )
    .await
    .expect("the first bind");

    let refused = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&first, CAPABILITY_READONLY),
        None,
    )
    .await;
    let unchanged = fx
        .pg
        .query_one(
            "SELECT capability FROM zeroship.database_bindings \
              WHERE database_id = $1 AND app_id = $2",
            &[&database_id.as_str(), &first.as_str()],
        )
        .await
        .expect("read the live binding")
        .get::<_, String>("capability");

    // CONTROL: one variable moves - a different app.
    let admitted = databases::bind_database(
        &fx.registry,
        &world.owner,
        &database_id,
        &bind_body(&second, CAPABILITY_READONLY),
        None,
    )
    .await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    match refused {
        Err(DatabaseError::AlreadyBound { app_id, capability }) => {
            assert_eq!(app_id, first.as_str());
            assert_eq!(
                capability, CAPABILITY_READWRITE,
                "the refusal must name the capability the LIVE binding holds"
            );
        }
        other => panic!("a second bind must be refused, got {other:?}"),
    }
    assert_eq!(
        unchanged, CAPABILITY_READWRITE,
        "a refused re-bind must not rewrite the live capability"
    );
    admitted.expect("a different app binds");
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// A display name is unique inside a project and free outside it, because
/// `databases_project_name_key` is the whole claim a name makes - a database is
/// addressed by its id everywhere else.
#[compio::test]
async fn a_name_is_taken_in_one_project_and_free_in_the_next() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "names").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let project_id = world.project_id.clone();
    let sibling_project = world.project(&fx, "sibling").await;

    databases::create_database(
        &fx.registry,
        &world.owner,
        &project_id,
        &create_body("main"),
        None,
    )
    .await
    .expect("the first main");

    let refused = databases::create_database(
        &fx.registry,
        &world.owner,
        &project_id,
        &create_body("main"),
        None,
    )
    .await;

    // CONTROL: one variable moves - a different project.
    let admitted = databases::create_database(
        &fx.registry,
        &world.owner,
        &sibling_project,
        &create_body("main"),
        None,
    )
    .await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    match refused {
        Err(DatabaseError::NameTaken(name)) => assert_eq!(name, "main"),
        other => panic!("a duplicate name in one project must refuse, got {other:?}"),
    }
    admitted.expect("the same name in a sibling project is free");
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The two listings: a project's databases, and a database's bindings with
/// their capability. Each is checked against a row that must NOT appear, so
/// neither can pass by returning everything.
#[compio::test]
async fn the_listings_are_scoped_to_their_project_and_their_database() {
    let fx = Fx::new().await;
    let mut world = World::new(&fx, "listing").await;
    world.datastore(&fx, &world.zone_id, "active").await;
    let project_id = world.project_id.clone();
    let sibling_project = world.project(&fx, "sibling").await;
    let writer = world.app(&fx, &project_id, "writer").await;
    let reader = world.app(&fx, &project_id, "reader").await;
    let unbound = world.app(&fx, &project_id, "unbound").await;

    let mut here = Vec::new();
    for name in ["analytics", "main"] {
        here.push(
            databases::create_database(
                &fx.registry,
                &world.owner,
                &project_id,
                &create_body(name),
                None,
            )
            .await
            .expect("create database"),
        );
    }
    let elsewhere = databases::create_database(
        &fx.registry,
        &world.owner,
        &sibling_project,
        &create_body("main"),
        None,
    )
    .await
    .expect("create the sibling database");

    let subject = DatabaseId::parse(&here[1].id).expect("a minted database id");
    let neighbour = DatabaseId::parse(&here[0].id).expect("a minted database id");
    for (app, capability) in [
        (&writer, CAPABILITY_READWRITE),
        (&reader, CAPABILITY_READONLY),
    ] {
        databases::bind_database(
            &fx.registry,
            &world.owner,
            &subject,
            &bind_body(app, capability),
            None,
        )
        .await
        .expect("bind app");
    }
    databases::bind_database(
        &fx.registry,
        &world.owner,
        &neighbour,
        &bind_body(&unbound, CAPABILITY_READWRITE),
        None,
    )
    .await
    .expect("bind the neighbour's app");

    let listed = databases::list_databases(&fx.pg, &project_id).await;
    let bindings = databases::list_bindings(&fx.pg, &subject).await;

    world.cleanup(&fx).await;
    drop(fx);
    common::drain_pg().await;

    let listed = listed.expect("list databases");
    assert_eq!(
        listed.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        vec!["analytics", "main"],
        "the listing is this project's databases, ordered by name"
    );
    assert!(
        listed.iter().all(|d| d.id != elsewhere.id),
        "the sibling project's database must not be listed"
    );

    let bindings = bindings.expect("list bindings");
    let mut seen: Vec<(String, String)> = bindings
        .iter()
        .map(|b| (b.app_id.clone(), b.capability.clone()))
        .collect();
    seen.sort();
    let mut expected = vec![
        (writer.as_str().to_owned(), CAPABILITY_READWRITE.to_owned()),
        (reader.as_str().to_owned(), CAPABILITY_READONLY.to_owned()),
    ];
    expected.sort();
    assert_eq!(
        seen, expected,
        "every binding on this database, with the capability it holds"
    );
    assert!(
        bindings.iter().all(|b| b.app_id != unbound.as_str()),
        "the neighbouring database's binding must not be listed"
    );
}
