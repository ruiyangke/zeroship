//! The app -> project -> organization resolution, end to end.
//!
//! **This file replaces `anywhere_uuid_regression_test.rs`, and the premise is
//! re-read rather than ported.** That test existed because
//! `zeroship.app_members.app_id` was a `uuid` column read into a Rust `String`,
//! which makes compio-postgres fail on the type, and because
//! `is_authorized_anywhere` reached that read unconditionally. `app_members` is
//! deleted, so the defect it pinned cannot recur in that form.
//!
//! **The uuid/text seam this file was named for is GONE.** It ran through
//! `apps.id`: that column was `uuid` while every id above it - `projects.id`,
//! `organization_members.organization_id`, `project_members.project_id` - was
//! `text COLLATE "C"`, so one resolve query bound a parsed `Uuid` on one side of
//! a join and text on the other. An app id is a typed id now and `apps.id` holds
//! its printed form, so the whole chain is text and there is no cast to get
//! wrong.
//!
//! What remains worth testing is what the seam was standing in for: whether the
//! chain resolves at all, and whether it produces the RIGHT authority rather
//! than a quiet zero. A wrong id does not fail this query - `apps` is reached by
//! LEFT JOIN, so it contributes no row and the answer is "you hold no seat" -
//! which is why the cases below pair a resolve that must succeed with an unknown
//! app that must deny.
//!
//! Requires a test database with the committed migration corpus applied
//! (`PG_TEST_URL`). Without one this target REFUSES rather than skipping; the
//! reasoning is in `crates/zeroship-authz/tests/common/mod.rs`.
//!
//! **`zeroship.apps.id` MUST BE `text` for this file to pass.** The resolve
//! binds the printed app id, so a database still carrying the `uuid` column
//! fails these tests on the INSERT in [`Fixture::new`] - loudly, and before any
//! assertion.

mod common;

use zeroship_core::user_id::UserId;
use common::live_dsn;
use compio_postgres::{connect, Client, NoTls};
use std::future::Future;
use uuid::Uuid;
use zeroship_authz::{
    authority, enforce, is_authorized_anywhere, load_platform_policies, Action, AuthzContext,
    AuthzDecision, Resource,
};
use zeroship_core::app_id::AppId;

/// The whole chain, in one request: `Resource::App` carries the typed id, the
/// resolve binds its printed form, joins `apps -> projects ->
/// organization_members -> organization_roles`, and produces the seat's rank.
#[test]
fn an_app_resolves_its_authority_through_its_project() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "resolve-chain", "developer").await;
        fixture.seat_on_project(&pg, "developer").await;
        let policies = load_platform_policies().unwrap();

        let resolved = authority::resolve(&pg, &fixture.user_id, &fixture.app())
            .await
            .expect("resolve must not error across the app -> project chain");
        assert_eq!(
            resolved.effective_rank, 20,
            "the developer seat must survive the app -> project -> organization join"
        );

        assert_eq!(
            enforce(
                &pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, fixture.app())
            )
            .await
            .unwrap(),
            AuthzDecision::Allow,
        );

        fixture.cleanup(&pg).await;
    });
}

/// Naming an app that does not exist must DENY, not error. The resolve still
/// has to read the principal's own row, so "no such app" and "no such user" are
/// different outcomes and only the second is a validation failure.
///
/// This is also the control for the neighbour above: a WELL-FORMED app id that
/// names no row denies quietly, so the neighbour's Allow is evidence that the
/// join found the fixture's app and not merely that nothing errored.
#[test]
fn an_unknown_app_denies_without_erroring() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "resolve-missing", "owner").await;
        let policies = load_platform_policies().unwrap();

        let missing = Resource::App { id: AppId::mint() };
        let resolved = authority::resolve(&pg, &fixture.user_id, &missing)
            .await
            .expect("an unknown app is not a database failure");
        assert_eq!(resolved.effective_rank, 0);
        assert_eq!(resolved.billing_rank, 0);

        assert_eq!(
            enforce(&pg, &policies, &fixture.ctx(Action::AppsRead, missing))
                .await
                .unwrap(),
            AuthzDecision::Deny,
        );

        fixture.cleanup(&pg).await;
    });
}

// WHAT USED TO BE HERE: `a_malformed_app_id_is_refused_rather_than_denied`.
//
// The bug it pinned is worth keeping written down. The resolve parsed a uuid
// and, on failure, fell through to the UNRANKED read, so the canonical
// `app_<base62>` rendering resolved to rank zero and 403'd as "you hold no seat
// on this app". Two very different things - an id we cannot read, and an id
// naming an app the caller cannot reach - produced one indistinguishable
// answer, in the response and in `zeroship.authz_decisions` alike.
//
// It is deleted rather than ported because its INPUT cannot be built.
// `Resource::App` carries an `AppId`, so `"not-a-uuid"` is refused by
// `AppId::parse` at the crate boundary, before a database is involved. The
// refusal itself is bound where it now happens and where no database is needed
// to reach it: `resource::tests::a_non_canonical_app_id_does_not_deserialize`
// for the wire, and `zeroship_core::app_id` for the parse.
//
// It is NOT re-asserted here even though the assertion would be one line. This
// target calls `process::exit` when no database is configured, so a test that
// needs no database would race that exit and report whichever won - a green
// that means nothing, in the one file whose whole subject is answers that look
// like other answers.
//
// The half this file still owes the boundary is the other one - a well-formed
// id naming no app must deny QUIETLY - and
// `an_unknown_app_denies_without_erroring` above is that case.

/// A principal with no `zeroship.users` row is a VALIDATION failure, never rank
/// zero. Degrading it to a Deny would file an unknown principal as "an ordinary
/// member with no seat" in the audit trail.
#[test]
fn an_unknown_principal_is_a_validation_error_not_rank_zero() {
    run_db_test(|pg| async move {
        for resource in [
            Resource::Any,
            Resource::App { id: AppId::mint() },
            Resource::Project {
                id: "prj_0000000000000000000000001".to_owned(),
            },
            Resource::Organization {
                id: "org_0000000000000000000000001".to_owned(),
            },
        ] {
            let err = authority::resolve(&pg, &UserId::mint(), &resource)
                .await
                .expect_err("an unknown principal must not resolve");
            assert!(
                matches!(err, zeroship_authz::AuthzError::Validation(_)),
                "{resource:?} produced {err:?}"
            );
        }
    });
}

/// The probe reads `organization_members.organization_id` and
/// `project_members.project_id` as text and hands them straight back as
/// resource ids. If either read had the wrong type, or a stored id left the
/// closed resource alphabet, this is where it fails - and it is on the consent
/// path, which is where such a failure is least visible.
#[test]
fn the_probe_reads_the_text_id_columns_and_returns_valid_resources() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "probe-ids", "developer").await;
        fixture.seat_on_project(&pg, "viewer").await;

        let organizations = authority::organization_resources(&pg, &fixture.user_id)
            .await
            .expect("organization memberships must read as text");
        assert_eq!(
            organizations,
            vec![Resource::Organization {
                id: fixture.organization_id.clone()
            }]
        );

        let projects = authority::project_probe_resources(&pg, &fixture.user_id)
            .await
            .expect("project memberships must read as text");
        assert_eq!(
            projects,
            vec![Resource::Project {
                id: fixture.project_id.clone()
            }]
        );

        // And the probe that consumes them answers on the strength of the
        // ceilinged project seat: reads yes, deploys no.
        let policies = load_platform_policies().unwrap();
        assert!(is_authorized_anywhere(
            &pg,
            &policies,
            &fixture.ctx(Action::AppsRead, Resource::Any)
        )
        .await
        .unwrap());
        assert!(
            !is_authorized_anywhere(
                &pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, Resource::Any)
            )
            .await
            .unwrap(),
            "a viewer project seat ceilings an organization developer on the consent path too"
        );

        fixture.cleanup(&pg).await;
    });
}

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

fn run_db_test<F, Fut>(test: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = ()>,
{
    // The preflight, before the first fixture INSERT. On 2026-09-07 the
    // database this reads held every schema these tests need and had never seen
    // `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`,
    // so the resolve query below failed on a missing `apps.organization_id` and
    // this file reported a stale database as a code regression. It refuses now,
    // naming `deploy/ops/db-migrate.sh` - and refuses just as loudly when no DSN
    // was configured at all.
    let dsn = live_dsn();
    compio::runtime::Runtime::new()
        .expect("create compio runtime")
        .block_on(async move {
            let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            test(client).await;
        });
}

// Four ids and nothing else, so `struct_field_names` fires on the shared
// postfix. It started firing when `app_uuid` went: that field was the second
// rendering of `app_id`, and it is the only reason the names ever varied.
#[allow(clippy::struct_field_names)] // every field IS an id; naming them so is the point
struct Fixture {
    user_id: UserId,
    organization_id: String,
    project_id: String,
    app_id: AppId,
}

impl Fixture {
    async fn new(pg: &Client, label: &str, organization_role: &str) -> Self {
        let user_id = UserId::mint();
        let organization_id = typed_id("org");
        let project_id = typed_id("prj");
        let app_id = AppId::mint();

        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[
                &user_id.as_str(),
                &format!("{label}-{}@example.com", user_id.as_str()),
                &label,
            ],
        )
        .await
        .expect("insert user");
        pg.execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
             VALUES ('free', 'Free', '{}') ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .expect("seed the free plan");
        pg.execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2::citext, $3, $4::citext)",
            &[
                &organization_id,
                &organization_id.replace('_', "-"),
                &label,
                &format!("{label}@example.com"),
            ],
        )
        .await
        .expect("insert organization");
        pg.execute(
            "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
             VALUES ($1, $2, $3::citext, $4)",
            &[&project_id, &organization_id, &"default", &label],
        )
        .await
        .expect("insert project");
        pg.execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
             SELECT $1, $2, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $3",
            &[
                &app_id.as_str(),
                &format!("authz-{label}-{}", Uuid::new_v4().simple()),
                &project_id,
            ],
        )
        .await
        .expect("insert app");
        pg.execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
             VALUES ($1, $2, $3)",
            &[&organization_id, &user_id.as_str(), &organization_role],
        )
        .await
        .expect("insert organization membership");

        Self {
            user_id,
            organization_id,
            project_id,
            app_id,
        }
    }

    async fn seat_on_project(&self, pg: &Client, role: &str) {
        pg.execute(
            "INSERT INTO zeroship.project_members \
                (project_id, organization_id, user_id, role) VALUES ($1, $2, $3, $4)",
            &[
                &self.project_id,
                &self.organization_id,
                &self.user_id.as_str(),
                &role,
            ],
        )
        .await
        .expect("insert project membership");
    }

    fn app(&self) -> Resource {
        Resource::App {
            id: self.app_id.clone(),
        }
    }

    fn ctx(&self, action: Action, resource: Resource) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id.clone(),
            token_policy: None,
            action,
            resource,
            now: 12 * 60 * 60,
            request_ip: None,
            request_id: None,
        }
    }

    async fn cleanup(&self, pg: &Client) {
        for sql in [
            "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
            "DELETE FROM zeroship.project_members WHERE user_id = $1",
            "DELETE FROM zeroship.organization_members WHERE user_id = $1",
        ] {
            let _ = pg.execute(sql, &[&self.user_id.as_str()]).await;
        }
        let _ = pg
            .execute(
                "DELETE FROM zeroship.apps WHERE id = $1",
                &[&self.app_id.as_str()],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.projects WHERE id = $1",
                &[&self.project_id],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.organizations WHERE id = $1",
                &[&self.organization_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id.as_str()])
            .await;
    }
}

fn typed_id(prefix: &str) -> String {
    let hex = Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &hex[..22])
}
