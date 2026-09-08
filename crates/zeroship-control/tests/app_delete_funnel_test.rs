//! The account-closure funnel, END TO END, against a live PostgreSQL.
//!
//! Every refusal in this chain names the next step:
//!
//!   erase account   -> sole owner of a live organization -> dissolve it
//!   dissolve        -> it still owns projects            -> delete them
//!   delete project  -> it still owns apps                -> delete them
//!   delete app      -> THE STEP THAT DID NOT EXIST
//!
//! Until `organizations::delete_app` landed, the last arrow pointed at nothing:
//! there was no route, no statement and no `DELETE FROM zeroship.apps` anywhere
//! in the control plane, and the refusal text told the creator to "archive and
//! delete them first, or move them to another project" - two remedies, neither
//! performable. A sole creator who had ever deployed could not close their
//! account, and that is the ORDINARY zero-config case: one `create_app` with no
//! project named mints the personal organization, its default project and the
//! app in one call, which is exactly how every case below starts.
//!
//! What each group binds:
//!
//! - **The ordering.** Archive is reversible and delete is terminal, so delete
//!   refuses an app that is not archived, and restore refuses one that is
//!   deleted. Each refusal is paired with a control differing in ONE variable.
//! - **The chain.** App, then project, then organization, then the human. It is
//!   one test rather than four, because the claim is that the chain TERMINATES
//!   and no subset of it can say that.
//! - **What deletion must not take with it.** The billing rows outlive the app
//!   on purpose - this repo keeps invoices outliving erased humans - so the
//!   retained evidence is asserted at the rows AND through
//!   `billing_read::outstanding_billing`, the predicate that actually reads
//!   them. The environment, which is capability rather than evidence, must be
//!   gone in the same breath.

#![allow(clippy::future_not_send)]

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_control::billing_read::{outstanding_billing, LocalInvoicing};
use zeroship_control::erasure::{preflight, ErasureRemedy};
use zeroship_control::organizations::{self, AddMemberBody, OrganizationError};
use zeroship_control::Registry;

use crate::common;

const FX_SCALE: i64 = 1_000_000_000_000;

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
        Self { registry, pg }
    }

    async fn seed_user(&self, label: &str) -> Uuid {
        let id = Uuid::new_v4();
        let email = format!("{label}-{}@zeroship.test", id.simple());
        self.pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, $3, NOW())",
                &[&id, &email, &label],
            )
            .await
            .expect("insert user");
        id
    }

    /// A plan of this file's own, so a shared catalog row cannot make one case
    /// depend on another's cleanup.
    async fn seed_plan(&self, label: &str) -> String {
        let plan_id = format!("pln_appdel_{}", Uuid::new_v4().simple());
        self.pg
            .execute(
                "INSERT INTO zeroship.plans \
                   (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                    runtime_limits_json, spend_limit_default_cents) \
                 VALUES ($1, $2, 0, 0, $3, \
                         '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 0)",
                &[&plan_id, &label, &FX_SCALE],
            )
            .await
            .expect("seed plan");
        plan_id
    }

    /// The zero-config first deploy: no project named, so control mints the
    /// creator's personal organization and its default project around the app.
    /// This is the shape the defect was reported against, so it is the shape
    /// every case here is built from rather than a hand-assembled fixture.
    async fn first_deploy(&self, label: &str) -> Deployed {
        let owner = self.seed_user(label).await;
        let plan_id = self.seed_plan(label).await;
        let app = self
            .registry
            .create_app(
                &format!("{label}-{}", Uuid::new_v4().simple()),
                &plan_id,
                &owner,
                None,
            )
            .await
            .expect("zero-config create_app");
        let row = self
            .pg
            .query(
                "SELECT a.organization_id, a.project_id \
                   FROM zeroship.apps a WHERE a.id = $1",
                &[&app.id],
            )
            .await
            .expect("read the minted ownership")
            .first()
            .map(|row| {
                (
                    row.get::<_, String>("organization_id"),
                    row.get::<_, Option<String>>("project_id")
                        .expect("a fresh app names a project"),
                )
            })
            .expect("the app exists");
        Deployed {
            owner,
            app: app.id,
            organization: row.0,
            project: row.1,
        }
    }

    async fn count(&self, sql: &str, app: Uuid) -> i64 {
        self.pg
            .query(sql, &[&app])
            .await
            .expect("count")
            .first()
            .map_or(0, |row| row.get::<_, i64>("n"))
    }

    async fn app_state(&self, app: Uuid) -> (bool, bool, Option<String>) {
        let rows = self
            .pg
            .query(
                "SELECT archived_at IS NOT NULL AS archived, \
                        deleted_at IS NOT NULL AS deleted, \
                        project_id \
                   FROM zeroship.apps WHERE id = $1",
                &[&app],
            )
            .await
            .expect("read app state");
        let row = rows.first().expect("the app row is retained");
        (
            row.get("archived"),
            row.get("deleted"),
            row.get("project_id"),
        )
    }
}

struct Deployed {
    owner: Uuid,
    app: Uuid,
    organization: String,
    project: String,
}

fn first_of_this_month() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("first of this month")
}

/// Archive is the reversible step, delete is the terminal one, and the order is
/// enforced rather than advised.
///
/// The CONTROL is the second half: the SAME call on the SAME app succeeds once
/// it is archived. Without it an implementation that refused every deletion
/// would pass the first assertion.
#[compio::test]
async fn a_live_app_is_not_deleted_and_an_archived_one_is() {
    let fx = Fx::new().await;
    let d = fx.first_deploy("livedel").await;

    let err = organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect_err("a live app must not be deleted");
    assert!(
        matches!(err, OrganizationError::AppNotArchived),
        "the refusal must name the missing step, got {err:?}"
    );
    let (archived, deleted, project) = fx.app_state(d.app).await;
    assert!(!archived && !deleted, "the refusal changed nothing");
    assert_eq!(project.as_deref(), Some(d.project.as_str()));

    fx.registry
        .archive_app(&d.app)
        .await
        .expect("archive")
        .expect("the app exists");
    organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect("an archived app is deleted");

    let (archived, deleted, project) = fx.app_state(d.app).await;
    assert!(archived && deleted, "the marker is set and archive survives");
    assert!(
        project.is_none(),
        "a deleted app leaves its project, which is what lets the project go"
    );

    common::drain_pg().await;
}

/// Restore must not undo the terminal step.
///
/// `zeroship_authz` already denies every app-scoped action on a deleted app,
/// because it reaches an app's organization through the project the delete
/// detaches. This binds the registry statement's OWN guard, which is the one
/// that holds if that ever stops being true.
#[compio::test]
async fn a_deleted_app_is_not_restored() {
    let fx = Fx::new().await;
    let d = fx.first_deploy("restoredel").await;

    fx.registry
        .archive_app(&d.app)
        .await
        .expect("archive")
        .expect("the app exists");
    // The CONTROL: restore works right up until the delete.
    fx.registry
        .unarchive_app(&d.app)
        .await
        .expect("restore an archived app")
        .expect("the app exists");
    fx.registry
        .archive_app(&d.app)
        .await
        .expect("archive again")
        .expect("the app exists");

    organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect("delete");

    assert!(
        fx.registry
            .unarchive_app(&d.app)
            .await
            .expect("restore must not error")
            .is_none(),
        "a deleted app is absent to restore, not restorable"
    );
    let (_, deleted, _) = fx.app_state(d.app).await;
    assert!(deleted, "the marker did not move");

    common::drain_pg().await;
}

/// Ending an app needs `admin` in the organization, the same floor
/// `delete_project` carries, and the floor rides in the effect statement rather
/// than in Cedar.
#[compio::test]
async fn deleting_an_app_needs_admin_authority() {
    let fx = Fx::new().await;
    let d = fx.first_deploy("rankdel").await;
    let developer = fx.seed_user("rankdel-dev").await;
    organizations::add_member(
        &fx.registry,
        d.owner,
        &d.organization,
        &AddMemberBody {
            user_id: developer,
            role: "developer".to_string(),
        },
        None,
    )
    .await
    .expect("seat a developer");
    fx.registry
        .archive_app(&d.app)
        .await
        .expect("archive")
        .expect("the app exists");

    let err = organizations::delete_app(&fx.registry, developer, d.app, None)
        .await
        .expect_err("a developer must not delete an app");
    assert!(
        matches!(err, OrganizationError::Insufficient(_)),
        "{err:?}"
    );
    let (_, deleted, _) = fx.app_state(d.app).await;
    assert!(!deleted, "the refused delete wrote nothing");

    // The CONTROL: the owner, differing only in rank, succeeds.
    organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect("the owner deletes it");

    common::drain_pg().await;
}

/// Deletion keeps the ledger and destroys the capability.
///
/// The usage row is asserted twice on purpose: once at the table, and once
/// through `outstanding_billing`, which is the predicate `dissolve` and the
/// erasure preflight actually refuse on. A row that survived but had become
/// unreachable to that read would be evidence nobody can bill from, and only
/// the second assertion can tell the difference.
#[compio::test]
async fn deletion_keeps_the_billing_evidence_and_destroys_the_environment() {
    let fx = Fx::new().await;
    let d = fx.first_deploy("evidence").await;
    let metric = format!("custom_{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.billing_metrics (metric, kind, unit, owner_app) \
             VALUES ($1, 'custom', 'unit', $2)",
            &[&metric, &d.app],
        )
        .await
        .expect("seed a metric");
    fx.pg
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 4242)",
            &[&d.app, &first_of_this_month(), &metric],
        )
        .await
        .expect("seed usage");
    fx.pg
        .execute(
            "INSERT INTO zeroship.app_vars (app_id, key_name, value) VALUES ($1, 'API_BASE', 'x')",
            &[&d.app],
        )
        .await
        .expect("seed a var");
    fx.pg
        .execute(
            "INSERT INTO zeroship.app_secrets (app_id, key_name, ciphertext) \
             VALUES ($1, 'TOKEN', $2)",
            &[&d.app, &vec![1u8, 2, 3]],
        )
        .await
        .expect("seed a secret");

    fx.registry
        .archive_app(&d.app)
        .await
        .expect("archive")
        .expect("the app exists");
    organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect("delete");

    assert_eq!(
        fx.count(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.usage_aggregates WHERE app_id = $1",
            d.app
        )
        .await,
        1,
        "the usage row must outlive the app; the FK would have CASCADED it away"
    );
    assert_eq!(
        fx.count(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.apps WHERE id = $1",
            d.app
        )
        .await,
        1,
        "the app row is the billing subject and is retained"
    );
    assert_eq!(
        fx.count(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.app_vars WHERE app_id = $1",
            d.app
        )
        .await,
        0,
        "the environment is capability, not evidence"
    );
    assert_eq!(
        fx.count(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.app_secrets WHERE app_id = $1",
            d.app
        )
        .await,
        0,
        "the secrets go with the app"
    );
    assert_eq!(
        fx.count(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.app_audit \
              WHERE app_id = $1 AND action = 'app_deleted'",
            d.app
        )
        .await,
        1,
        "the terminal act is audited"
    );

    // The billing read still reaches the retained usage through
    // `apps.organization_id`, which is the column deletion deliberately keeps.
    let seen = fx
        .pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.usage_aggregates u \
               JOIN zeroship.apps a ON a.id = u.app_id \
              WHERE a.organization_id = $1",
            &[&d.organization],
        )
        .await
        .expect("read usage through the organization")[0]
        .get::<_, i64>("n");
    assert_eq!(seen, 1, "a detached app is still the organization's to bill");
    outstanding_billing(&fx.pg, &d.organization, LocalInvoicing::Yes)
        .await
        .expect("the debt predicate still runs over a deleted app");

    common::drain_pg().await;
}

/// THE WHOLE FUNNEL, in order, on one creator.
///
/// This is the test the defect is about. Each step is asserted to REFUSE with
/// the reason that names the next one, and then to SUCCEED once that step has
/// been taken - so a green here is "the chain terminates", not "four calls
/// returned Ok".
#[compio::test]
async fn the_closure_funnel_terminates_for_a_sole_creator_who_deployed() {
    let fx = Fx::new().await;
    let d = fx.first_deploy("funnel").await;

    // 1. Erase the account: refused, sole owner of a live organization, and the
    //    remedy names the projects.
    let report = preflight(&fx.pg, d.owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    let blocker = report
        .blockers
        .iter()
        .find(|b| b.organization_id == d.organization)
        .expect("the personal organization blocks erasure");
    assert!(blocker.personal);
    assert_eq!(blocker.remedy, ErasureRemedy::DeleteProjects);

    // 2. Dissolve: refused, it still owns projects.
    let err = organizations::dissolve_organization(
        &fx.registry,
        d.owner,
        &d.organization,
        LocalInvoicing::Yes,
        None,
    )
        .await
        .expect_err("an organization owning a project is not dissolved");
    assert!(
        matches!(err, OrganizationError::OrganizationHasProjects(n) if n == 1),
        "{err:?}"
    );

    // 3. Delete the project: refused, it still owns an app. THIS is the refusal
    //    whose remedy did not exist.
    let err = organizations::delete_project(&fx.registry, d.owner, &d.project, None)
        .await
        .expect_err("a project owning an app is not deleted");
    assert!(
        matches!(err, OrganizationError::ProjectHasApps(n) if n == 1),
        "{err:?}"
    );

    // 4. Delete the app: refused until archived, then done.
    let err = organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect_err("a live app is not deleted");
    assert!(matches!(err, OrganizationError::AppNotArchived), "{err:?}");
    fx.registry
        .archive_app(&d.app)
        .await
        .expect("archive")
        .expect("the app exists");
    organizations::delete_app(&fx.registry, d.owner, d.app, None)
        .await
        .expect("the funnel's last step exists");

    // 5. The project now goes.
    organizations::delete_project(&fx.registry, d.owner, &d.project, None)
        .await
        .expect("a project whose last app is deleted can be deleted");

    // 6. The organization now closes.
    organizations::dissolve_organization(
        &fx.registry,
        d.owner,
        &d.organization,
        LocalInvoicing::Yes,
        None,
    )
        .await
        .expect("an organization whose last project is gone can be dissolved");

    // 7. And the human is erasable: the dissolved organization is no longer an
    //    ownership blocker, and the retained usage owes nothing (its period is
    //    the current month, which the unbilled arm excludes by design).
    let report = preflight(&fx.pg, d.owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    assert!(
        report.is_clear(),
        "the funnel must terminate: {:?} / {:?}",
        report.blockers,
        report.billing_blockers
    );

    common::drain_pg().await;
}
