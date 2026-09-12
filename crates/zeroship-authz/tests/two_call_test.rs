//! `enforce` end to end, against a live `PostgreSQL` carrying the real schema.
//!
//! `platform_policies_test.rs` pins what each band decides for a given rank.
//! This file pins the other half: that the rank the database produces for a
//! (principal, resource) pair is the right one - including the per-project
//! narrowing, which is the whole reason projects exist in this change - and
//! that the two-call token intersection still holds on top of it.
//!
//! Requires a test database with the committed migration corpus applied
//! (`PG_TEST_URL`). Without one this target REFUSES rather than skipping; the
//! reasoning is in `crates/zeroship-authz/tests/common/mod.rs`.

use common::live_dsn;
use compio_postgres::{connect, Client, NoTls};
use std::future::Future;
use zeroship_authz::{
    enforce, is_authorized_anywhere, load_platform_policies, Action, AuthzContext, AuthzDecision,
    Condition, Effect, Policy, Resource, Statement,
};
use zeroship_id::{AppId, UserId};

mod common;

// ---------------------------------------------------------------------------
// The two-call token intersection: TOKEN is a subset of USER
// ---------------------------------------------------------------------------

#[test]
fn principal_authorized_token_authorized_returns_allow() {
    run_db_test(|pg| async move {
        let fixture = Fixture::seated(&pg, "developer-allow", "developer").await;
        let wrapper = Policy {
            name: "deploy token".to_owned(),
            statements: vec![allow(vec![Action::AppsDeploy], vec![fixture.app()])],
        };

        let decision = enforce(
            &pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx_with_policy(Action::AppsDeploy, fixture.app(), 12 * 60 * 60, wrapper),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Allow);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn principal_unauthorized_returns_deny_even_if_token_grants() {
    run_db_test(|pg| async move {
        let fixture = Fixture::seated(&pg, "viewer-token-grants", "viewer").await;
        let wrapper = Policy {
            name: "overbroad token".to_owned(),
            statements: vec![allow(vec![Action::AppsDeploy], vec![fixture.app()])],
        };

        let decision = enforce(
            &pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx_with_policy(Action::AppsDeploy, fixture.app(), 12 * 60 * 60, wrapper),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn token_denies_returns_deny_even_if_principal_allowed() {
    run_db_test(|pg| async move {
        let fixture = Fixture::seated(&pg, "developer-token-denies", "developer").await;
        let wrapper = Policy {
            name: "read env token".to_owned(),
            statements: vec![allow(vec![Action::EnvRead], vec![fixture.app()])],
        };

        let decision = enforce(
            &pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx_with_policy(Action::AppsDeploy, fixture.app(), 12 * 60 * 60, wrapper),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn no_token_uses_the_static_bands_only() {
    run_db_test(|pg| async move {
        let fixture = Fixture::seated(&pg, "developer-no-token", "developer").await;

        let decision = enforce(
            &pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx(Action::AppsDeploy, fixture.app()),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Allow);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn time_window_policy_enforces_utc_hours() {
    run_db_test(|pg| async move {
        let fixture = Fixture::seated(&pg, "time-window", "developer").await;
        let token_policy = Policy {
            name: "business hours".to_owned(),
            statements: vec![Statement {
                effect: Effect::Allow,
                actions: vec![Action::AppsRead],
                resources: vec![fixture.app()],
                conditions: vec![Condition::TimeWindow {
                    start: "09:00".to_owned(),
                    end: "17:00".to_owned(),
                    tz: "UTC".to_owned(),
                }],
            }],
        };
        let policies = load_platform_policies().unwrap();

        let denied = enforce(
            &pg,
            &policies,
            &fixture.ctx_with_policy(
                Action::AppsRead,
                fixture.app(),
                3 * 60 * 60,
                token_policy.clone(),
            ),
        )
        .await
        .unwrap();
        let allowed = enforce(
            &pg,
            &policies,
            &fixture.ctx_with_policy(Action::AppsRead, fixture.app(), 12 * 60 * 60, token_policy),
        )
        .await
        .unwrap();

        assert_eq!(denied, AuthzDecision::Deny);
        assert_eq!(allowed, AuthzDecision::Allow);
        fixture.cleanup(&pg).await;
    });
}

// ---------------------------------------------------------------------------
// The organization ladder, resolved from real rows
// ---------------------------------------------------------------------------

#[test]
fn the_organization_seat_decides_what_the_app_allows() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();

        // One fixture per seat, each asked the SAME two questions. The pair is
        // what makes each row a boundary rather than an assertion.
        for (role, read, deploy) in [
            ("viewer", AuthzDecision::Allow, AuthzDecision::Deny),
            ("developer", AuthzDecision::Allow, AuthzDecision::Allow),
            ("admin", AuthzDecision::Allow, AuthzDecision::Allow),
            ("owner", AuthzDecision::Allow, AuthzDecision::Allow),
            // `billing` sits at viewer rank on the app axis, with all the money
            // authority. Its rank is what decides the app questions.
            ("billing", AuthzDecision::Allow, AuthzDecision::Deny),
        ] {
            let fixture = Fixture::seated(&pg, &format!("ladder-{role}"), role).await;
            assert_eq!(
                enforce(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::AppsRead, fixture.app())
                )
                .await
                .unwrap(),
                read,
                "{role} apps:read"
            );
            assert_eq!(
                enforce(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::AppsDeploy, fixture.app())
                )
                .await
                .unwrap(),
                deploy,
                "{role} apps:deploy"
            );
            fixture.cleanup(&pg).await;
        }
    });
}

/// The money axis, resolved from the ladder's second integer. `billing` holds
/// rank 10 and `billing_rank` 20; `admin` holds rank 30 and `billing_rank` 10.
#[test]
fn money_authority_follows_billing_rank_not_rank() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();

        for (role, read, write) in [
            ("viewer", AuthzDecision::Deny, AuthzDecision::Deny),
            ("developer", AuthzDecision::Deny, AuthzDecision::Deny),
            ("admin", AuthzDecision::Allow, AuthzDecision::Deny),
            ("billing", AuthzDecision::Allow, AuthzDecision::Allow),
            ("owner", AuthzDecision::Allow, AuthzDecision::Allow),
        ] {
            let fixture = Fixture::new(&pg, &format!("money-{role}"), role).await;
            assert_eq!(
                enforce(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::BillingRead, fixture.organization())
                )
                .await
                .unwrap(),
                read,
                "{role} billing:read"
            );
            assert_eq!(
                enforce(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::BillingWrite, fixture.organization())
                )
                .await
                .unwrap(),
                write,
                "{role} billing:write"
            );
            fixture.cleanup(&pg).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Per-project narrowing - the point of the change
// ---------------------------------------------------------------------------

/// A member below admin holds authority ONLY where a `project_members` row
/// exists. This is the case that had no equivalent under `app_members`, and it
/// is asserted with its control: the same seat, one row added, opposite answer.
#[test]
fn below_admin_a_project_row_is_required() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();
        let fixture = Fixture::new(&pg, "narrow-no-row", "developer").await;

        // No project_members row: a developer's organization seat reaches
        // nothing inside the project.
        for action in [Action::AppsRead, Action::AppsDeploy] {
            assert_eq!(
                enforce(&pg, &policies, &fixture.ctx(action, fixture.app()))
                    .await
                    .unwrap(),
                AuthzDecision::Deny,
                "{action:?} must be denied without a project membership"
            );
        }

        // ONE row added, nothing else changed.
        fixture.seat_on_project(&pg, "developer").await;
        for action in [Action::AppsRead, Action::AppsDeploy] {
            assert_eq!(
                enforce(&pg, &policies, &fixture.ctx(action, fixture.app()))
                    .await
                    .unwrap(),
                AuthzDecision::Allow,
                "{action:?} must be allowed once the project membership exists"
            );
        }

        fixture.cleanup(&pg).await;
    });
}

/// The narrowing is a MINIMUM in both directions. A project seat cannot widen
/// an organization seat, and an organization seat cannot widen a project seat.
#[test]
fn the_effective_rank_is_the_minimum_of_the_two_seats() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();

        // Organization developer, project OWNER: the project cannot widen, so
        // organization-admin authority over the project stays refused while
        // ordinary developer authority works.
        let ceiling = Fixture::new(&pg, "narrow-ceiling", "developer").await;
        ceiling.seat_on_project(&pg, "owner").await;
        assert_eq!(
            enforce(
                &pg,
                &policies,
                &ceiling.ctx(Action::AppsDeploy, ceiling.project())
            )
            .await
            .unwrap(),
            AuthzDecision::Allow,
            "a developer seated on the project may deploy in it"
        );
        assert_eq!(
            enforce(
                &pg,
                &policies,
                &ceiling.ctx(Action::ProjectMembersWrite, ceiling.project())
            )
            .await
            .unwrap(),
            AuthzDecision::Deny,
            "a project owner seat must NOT lift a developer to project administration"
        );
        ceiling.cleanup(&pg).await;

        // Organization developer, project VIEWER: the project CEILINGS, so the
        // organization seat is cut down to reads inside it - the other half of
        // the same rule. The organization seat is deliberately below admin,
        // because at admin and above the narrowing stops applying at all
        // (exercised separately below).
        let floor = Fixture::new(&pg, "narrow-floor", "developer").await;
        floor.seat_on_project(&pg, "viewer").await;
        assert_eq!(
            enforce(&pg, &policies, &floor.ctx(Action::AppsRead, floor.app()))
                .await
                .unwrap(),
            AuthzDecision::Allow,
            "a viewer project seat still reads"
        );
        assert_eq!(
            enforce(&pg, &policies, &floor.ctx(Action::AppsDeploy, floor.app()))
                .await
                .unwrap(),
            AuthzDecision::Deny,
            "a viewer project seat must ceiling an organization developer"
        );
        floor.cleanup(&pg).await;
    });
}

/// At admin and above the narrowing stops: the seat reaches every project in
/// the organization with no `project_members` row at all. The control is the
/// rank immediately below, which reaches nothing on the identical fixture.
#[test]
fn admin_and_above_reach_every_project_without_a_row() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();

        for (role, expected) in [
            ("developer", AuthzDecision::Deny),
            ("admin", AuthzDecision::Allow),
            ("owner", AuthzDecision::Allow),
        ] {
            let fixture = Fixture::new(&pg, &format!("wide-{role}"), role).await;
            assert_eq!(
                enforce(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::AppsDeploy, fixture.app())
                )
                .await
                .unwrap(),
                expected,
                "{role} deploying into a project it holds no row on"
            );
            fixture.cleanup(&pg).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Revocation, cross-tenant, audit
// ---------------------------------------------------------------------------

/// **Revocation is immediate, in every process, with nothing to invalidate.**
///
/// This test replaces one that promoted a member and then busted the entity
/// cache by hand to make the promotion visible. That cache is deleted, so the
/// interesting direction is the dangerous one: a membership DELETED by a
/// committed transaction must be invisible to the very next call, and the test
/// makes no invalidation call because there is no longer one to make.
///
/// The first Allow is the control: without it the Deny would prove only that
/// non-members are denied, not that anything was re-derived.
#[test]
fn a_deleted_membership_denies_the_very_next_request() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "revocation", "owner").await;
        let policies = load_platform_policies().unwrap();

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

        pg.execute(
            "DELETE FROM zeroship.organization_members \
             WHERE organization_id = $1 AND user_id = $2",
            &[&fixture.organization_id, &fixture.user_id.as_str()],
        )
        .await
        .expect("revoke the membership");

        assert_eq!(
            enforce(
                &pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, fixture.app())
            )
            .await
            .unwrap(),
            AuthzDecision::Deny,
            "a committed revocation must take effect on the next request, with no cache to bust",
        );

        fixture.cleanup(&pg).await;
    });
}

/// Regression for the cross-tenant read IDOR, driven through the real
/// `enforce`. A creator with no seat in the owning organization must be denied
/// every read on another organization's app, and the owner must still be
/// allowed - the control against over-restricting the fix.
#[test]
fn an_unseated_creator_is_denied_cross_tenant_reads() {
    run_db_test(|pg| async move {
        let victim = Fixture::new(&pg, "c1-victim", "owner").await;

        let attacker_id = UserId::mint();
        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[
                &attacker_id.as_str(),
                &format!("c1-attacker-{}@example.com", attacker_id.as_str()),
                &"c1-attacker",
            ],
        )
        .await
        .expect("insert attacker");

        let policies = load_platform_policies().unwrap();
        for action in [
            Action::AppsRead,
            Action::EnvRead,
            Action::SecretsRead,
            Action::DeploymentsRead,
        ] {
            let ctx = AuthzContext {
                principal_id: attacker_id.clone(),
                token_policy: None,
                action,
                resource: victim.app(),
                now: 12 * 60 * 60,
                request_ip: None,
                request_id: None,
            };
            assert_eq!(
                enforce(&pg, &policies, &ctx).await.unwrap(),
                AuthzDecision::Deny,
                "an unseated creator must not read {action:?} on another organization's app",
            );
        }
        // The same attacker must not reach the organization itself either.
        for action in [Action::OrganizationRead, Action::OrganizationMembersRead] {
            let ctx = AuthzContext {
                principal_id: attacker_id.clone(),
                token_policy: None,
                action,
                resource: victim.organization(),
                now: 12 * 60 * 60,
                request_ip: None,
                request_id: None,
            };
            assert_eq!(
                enforce(&pg, &policies, &ctx).await.unwrap(),
                AuthzDecision::Deny,
                "an unseated creator must not read {action:?} on another organization",
            );
        }

        assert_eq!(
            enforce(
                &pg,
                &policies,
                &victim.ctx(Action::SecretsRead, victim.app())
            )
            .await
            .unwrap(),
            AuthzDecision::Allow,
            "the owner must still read secrets on their own app",
        );

        let _ = pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&attacker_id.as_str()],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&attacker_id.as_str()],
            )
            .await;
        victim.cleanup(&pg).await;
    });
}

/// The audit row records the resource TYPE, so an organization decision and an
/// app decision are distinguishable afterwards.
/// `zeroship.authz_decisions.resource_type` is unconstrained `text` - nothing
/// in the database notices a mis-tagged row, so this is where it is noticed.
#[test]
fn audit_records_the_resource_type_and_matched_bands() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "audit", "owner").await;
        let policies = load_platform_policies().unwrap();

        for (resource, action, expected_type, expected_id) in [
            (
                fixture.app(),
                Action::AppsDeploy,
                "app",
                fixture.app_id.as_str().to_owned(),
            ),
            (
                fixture.project(),
                Action::ProjectRead,
                "project",
                fixture.project_id.clone(),
            ),
            (
                fixture.organization(),
                Action::OrganizationMembersRead,
                "organization",
                fixture.organization_id.clone(),
            ),
        ] {
            assert_eq!(
                enforce(&pg, &policies, &fixture.ctx(action, resource))
                    .await
                    .unwrap(),
                AuthzDecision::Allow,
            );

            let rows = pg
                .query(
                    "SELECT resource_type, resource_id, decision, matched_policies \
                     FROM zeroship.authz_decisions \
                     WHERE actor_user_id = $1 AND action = $2 \
                     ORDER BY occurred_at DESC LIMIT 1",
                    &[&fixture.user_id.as_str(), &action.cedar_id()],
                )
                .await
                .expect("select audit row");
            let row = rows.first().expect("audit row exists");
            assert_eq!(row.get::<_, String>("resource_type"), expected_type);
            assert_eq!(
                row.get::<_, Option<String>>("resource_id"),
                Some(expected_id)
            );
            assert_eq!(row.get::<_, String>("decision"), "allow");
            let matched: Vec<String> = row.get("matched_policies");
            assert!(
                !matched.is_empty(),
                "audit row must name the band that matched"
            );
        }

        fixture.cleanup(&pg).await;
    });
}

// ---------------------------------------------------------------------------
// The consent probe
// ---------------------------------------------------------------------------

/// `is_authorized_anywhere` is the consent question, and its answer must be the
/// true one or the screen over-claims. An organization action is grantable only
/// by someone who actually holds it somewhere.
#[test]
fn the_consent_probe_answers_organization_actions_truthfully() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();

        for (role, members_write, deploy) in [
            ("viewer", false, false),
            ("developer", false, false),
            ("admin", true, true),
            ("owner", true, true),
        ] {
            let fixture = Fixture::new(&pg, &format!("probe-{role}"), role).await;
            assert_eq!(
                is_authorized_anywhere(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::OrganizationMembersWrite, Resource::Any)
                )
                .await
                .unwrap(),
                members_write,
                "{role} delegating organization:members:write"
            );
            assert_eq!(
                is_authorized_anywhere(
                    &pg,
                    &policies,
                    &fixture.ctx(Action::AppsDeploy, Resource::Any)
                )
                .await
                .unwrap(),
                deploy,
                "{role} delegating apps:deploy"
            );
            fixture.cleanup(&pg).await;
        }
    });
}

/// The probe's project arm. A developer holds `apps:deploy` nowhere until a
/// project row exists, and the probe must find it once it does - which is what
/// `authority::project_probe_resources`' explicit-membership arm is for.
#[test]
fn the_consent_probe_finds_an_explicit_project_seat() {
    run_db_test(|pg| async move {
        let policies = load_platform_policies().unwrap();
        let fixture = Fixture::new(&pg, "probe-project", "developer").await;

        assert!(
            !is_authorized_anywhere(
                &pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, Resource::Any)
            )
            .await
            .unwrap(),
            "a developer with no project seat can delegate apps:deploy nowhere"
        );

        fixture.seat_on_project(&pg, "developer").await;

        assert!(
            is_authorized_anywhere(
                &pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, Resource::Any)
            )
            .await
            .unwrap(),
            "the probe must find the project the developer was just seated on"
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

#[derive(Debug)]
pub struct Fixture {
    pub user_id: UserId,
    pub organization_id: String,
    pub project_id: String,
    /// ONE field, where there used to be a `Uuid` and a `String` rendering of
    /// it. Two fields meant every use had to pick, and picking wrong is silent:
    /// the resolve reaches `zeroship.apps` by LEFT JOIN, so the wrong rendering
    /// matches no row and reports the seat as absent rather than failing.
    pub app_id: AppId,
}

impl Fixture {
    /// Seed one user, one organization, one project, one app, and one
    /// organization membership at `organization_role`.
    ///
    /// A project seat, where a test wants one, is added afterwards with
    /// [`Fixture::seat_on_project`], so the "before" state of every narrowing
    /// test is an organization seat and nothing else.
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
                &organization_slug(&organization_id),
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
                &format!("authz-{label}-{}", app_id.as_str()),
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

    /// [`Fixture::new`] plus a project seat at the SAME role, so
    /// `min(organization rank, project rank)` is the organization rank and the
    /// narrowing is out of the way.
    ///
    /// Every test about something OTHER than narrowing uses this. Building the
    /// fixture without a project row would make each of those tests silently
    /// depend on the narrowing rule as well, and a change to that rule would
    /// then break them all with no signal about which property actually moved.
    async fn seated(pg: &Client, label: &str, role: &str) -> Self {
        let fixture = Self::new(pg, label, role).await;
        fixture.seat_on_project(pg, role).await;
        fixture
    }

    async fn seat_on_project(&self, pg: &Client, role: &str) {
        pg.execute(
            "INSERT INTO zeroship.project_members \
                (project_id, organization_id, user_id, role) \
             VALUES ($1, $2, $3, $4)",
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

    fn project(&self) -> Resource {
        Resource::Project {
            id: self.project_id.clone(),
        }
    }

    fn organization(&self) -> Resource {
        Resource::Organization {
            id: self.organization_id.clone(),
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

    fn ctx_with_policy(
        &self,
        action: Action,
        resource: Resource,
        now: i64,
        policy: Policy,
    ) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id.clone(),
            token_policy: Some(policy),
            action,
            resource,
            now,
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
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
    }
}

/// A canonical typed id, minted by the one minter.
///
/// Composing a body by hand pins BOTH the width and the alphabet, so it stops
/// satisfying the schema's shape CHECK the moment either moves - and it fails at
/// insert time, not at compile time.
fn typed_id(prefix: &str) -> String {
    zeroship_id::typed_id::generate(prefix)
}

/// A slug matching the schema's grammar `^[a-z0-9][a-z0-9-]*$`, derived from
/// the id so it is unique per fixture (organization slugs are globally unique).
fn organization_slug(organization_id: &str) -> String {
    organization_id.replace('_', "-")
}

const fn allow(actions: Vec<Action>, resources: Vec<Resource>) -> Statement {
    Statement {
        effect: Effect::Allow,
        actions,
        resources,
        conditions: Vec::new(),
    }
}
