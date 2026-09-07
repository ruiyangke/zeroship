//! The app -> project -> organization resolution, across the uuid/text seam.
//!
//! **This file replaces `anywhere_uuid_regression_test.rs`, and the premise is
//! re-read rather than ported.** That test existed because
//! `zeroship.app_members.app_id` was a `uuid` column read into a Rust `String`,
//! which makes compio-postgres fail on the type, and because
//! `is_authorized_anywhere` reached that read unconditionally. `app_members` is
//! deleted, so the defect it pinned cannot recur in that form.
//!
//! The seam it was really about is still here and has moved: an app is reached
//! by `uuid`, and everything above it - `projects.id`,
//! `organization_members.organization_id`, `project_members.project_id` - is
//! `text COLLATE "C"`. One resolve query now spans both, binding a parsed
//! `Uuid` against `apps.id` and text against the project chain. Getting either
//! side's type wrong fails the query outright, so the test that matters is
//! whether the whole chain resolves and produces the right authority.
//!
//! Requires a test database with the committed migration corpus applied
//! (`PG_TEST_URL`).

use compio_postgres::{connect, Client, NoTls};
use std::future::Future;
use uuid::Uuid;
use zeroship_authz::{
    authority, enforce, is_authorized_anywhere, load_platform_policies, Action, AuthzContext,
    AuthzDecision, Resource,
};

/// The whole chain, in one request: `Resource::App` carries a uuid in string
/// form, the resolve parses it, joins `apps -> projects -> organization_members
/// -> organization_roles`, and produces the seat's rank.
#[test]
fn an_app_resolves_its_authority_through_its_project() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "resolve-chain", "developer").await;
        fixture.seat_on_project(&pg, "developer").await;
        let policies = load_platform_policies().unwrap();

        let resolved = authority::resolve(&pg, fixture.user_id, &fixture.app())
            .await
            .expect("resolve must not error on the uuid/text seam");
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
#[test]
fn an_unknown_app_denies_without_erroring() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "resolve-missing", "owner").await;
        let policies = load_platform_policies().unwrap();

        let missing = Resource::App {
            id: Uuid::new_v4().to_string(),
        };
        let resolved = authority::resolve(&pg, fixture.user_id, &missing)
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

/// An app id that is not a uuid at all must also deny quietly. Sending it
/// straight into `apps.id = $1::uuid` would raise an invalid-input error and
/// surface as a 500 on a request that should simply be refused.
#[test]
fn a_malformed_app_id_denies_without_erroring() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "resolve-malformed", "owner").await;
        let policies = load_platform_policies().unwrap();

        let malformed = Resource::App {
            id: "not-a-uuid".to_owned(),
        };
        let resolved = authority::resolve(&pg, fixture.user_id, &malformed)
            .await
            .expect("a malformed app id must not raise a database error");
        assert_eq!(resolved.effective_rank, 0);

        assert_eq!(
            enforce(&pg, &policies, &fixture.ctx(Action::AppsRead, malformed))
                .await
                .unwrap(),
            AuthzDecision::Deny,
        );

        fixture.cleanup(&pg).await;
    });
}

/// A principal with no `zeroship.users` row is a VALIDATION failure, never rank
/// zero. Degrading it to a Deny would file an unknown principal as "an ordinary
/// member with no seat" in the audit trail.
#[test]
fn an_unknown_principal_is_a_validation_error_not_rank_zero() {
    run_db_test(|pg| async move {
        for resource in [
            Resource::Any,
            Resource::App {
                id: Uuid::new_v4().to_string(),
            },
            Resource::Project {
                id: "prj_0000000000000000000001".to_owned(),
            },
            Resource::Organization {
                id: "org_0000000000000000000001".to_owned(),
            },
        ] {
            let err = authority::resolve(&pg, Uuid::new_v4(), &resource)
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

        let organizations = authority::organization_resources(&pg, fixture.user_id)
            .await
            .expect("organization memberships must read as text");
        assert_eq!(
            organizations,
            vec![Resource::Organization {
                id: fixture.organization_id.clone()
            }]
        );

        let projects = authority::project_probe_resources(&pg, fixture.user_id)
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
    let Some(dsn) = zeroship_core::config::test_database_url_opt() else {
        zeroship_test_support::skip("skipping (no test database; set PG_TEST_URL)");
        return;
    };
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

struct Fixture {
    user_id: Uuid,
    organization_id: String,
    project_id: String,
    app_uuid: Uuid,
}

impl Fixture {
    async fn new(pg: &Client, label: &str, organization_role: &str) -> Self {
        let user_id = Uuid::new_v4();
        let organization_id = typed_id("org");
        let project_id = typed_id("prj");
        let app_uuid = Uuid::new_v4();

        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[&user_id, &format!("{label}-{user_id}@example.com"), &label],
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
            "INSERT INTO zeroship.apps (id, name, project_id) VALUES ($1, $2, $3)",
            &[
                &app_uuid,
                &format!("authz-{label}-{}", Uuid::new_v4().simple()),
                &project_id,
            ],
        )
        .await
        .expect("insert app");
        pg.execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
             VALUES ($1, $2, $3)",
            &[&organization_id, &user_id, &organization_role],
        )
        .await
        .expect("insert organization membership");

        Self {
            user_id,
            organization_id,
            project_id,
            app_uuid,
        }
    }

    async fn seat_on_project(&self, pg: &Client, role: &str) {
        pg.execute(
            "INSERT INTO zeroship.project_members \
                (project_id, organization_id, user_id, role) VALUES ($1, $2, $3, $4)",
            &[
                &self.project_id,
                &self.organization_id,
                &self.user_id,
                &role,
            ],
        )
        .await
        .expect("insert project membership");
    }

    fn app(&self) -> Resource {
        Resource::App {
            id: self.app_uuid.to_string(),
        }
    }

    const fn ctx(&self, action: Action, resource: Resource) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id,
            token_policy: None,
            action,
            resource,
            now: 12 * 60 * 60,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        }
    }

    async fn cleanup(&self, pg: &Client) {
        for sql in [
            "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
            "DELETE FROM zeroship.project_members WHERE user_id = $1",
            "DELETE FROM zeroship.organization_members WHERE user_id = $1",
        ] {
            let _ = pg.execute(sql, &[&self.user_id]).await;
        }
        let _ = pg
            .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&self.app_uuid])
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
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

fn typed_id(prefix: &str) -> String {
    let hex = Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &hex[..22])
}
