use crate::tests::{database::Database, membership::Membership};
use crate::{enforce, load_platform_policies, Action, AuthzContext, AuthzDecision};
use zeroship_id::UserId;

#[compio::test]
async fn the_organization_seat_decides_what_the_app_allows() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();

        // Read and deploy decisions distinguish each role from adjacent authority bands.
        for (role, read, deploy) in [
            ("viewer", AuthzDecision::Allow, AuthzDecision::Deny),
            ("developer", AuthzDecision::Allow, AuthzDecision::Allow),
            ("admin", AuthzDecision::Allow, AuthzDecision::Allow),
            ("owner", AuthzDecision::Allow, AuthzDecision::Allow),
            // `billing` sits at viewer rank on the app axis, with all the money
            // authority. Its rank is what decides the app questions.
            ("billing", AuthzDecision::Allow, AuthzDecision::Deny),
        ] {
            let fixture =
                Membership::seated(&database.admin, &format!("ladder-{role}"), role).await;
            assert_eq!(
                enforce(pg, &policies, &fixture.ctx(Action::AppsRead, fixture.app()))
                    .await
                    .unwrap(),
                read,
                "{role} apps:read"
            );
            assert_eq!(
                enforce(
                    pg,
                    &policies,
                    &fixture.ctx(Action::AppsDeploy, fixture.app())
                )
                .await
                .unwrap(),
                deploy,
                "{role} apps:deploy"
            );
        }
    })
    .await;
}

#[compio::test]
async fn money_authority_follows_billing_rank_not_rank() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();

        for (role, read, write) in [
            ("viewer", AuthzDecision::Deny, AuthzDecision::Deny),
            ("developer", AuthzDecision::Deny, AuthzDecision::Deny),
            ("admin", AuthzDecision::Allow, AuthzDecision::Deny),
            ("billing", AuthzDecision::Allow, AuthzDecision::Allow),
            ("owner", AuthzDecision::Allow, AuthzDecision::Allow),
        ] {
            let fixture = Membership::new(&database.admin, &format!("money-{role}"), role).await;
            assert_eq!(
                enforce(
                    pg,
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
                    pg,
                    &policies,
                    &fixture.ctx(Action::BillingWrite, fixture.organization())
                )
                .await
                .unwrap(),
                write,
                "{role} billing:write"
            );
        }
    })
    .await;
}

#[compio::test]
async fn below_admin_a_project_row_is_required() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();
        let fixture = Membership::new(&database.admin, "narrow-no-row", "developer").await;

        // No project_members row: a developer's organization seat reaches
        // nothing inside the project.
        for action in [Action::AppsRead, Action::AppsDeploy] {
            assert_eq!(
                enforce(pg, &policies, &fixture.ctx(action, fixture.app()))
                    .await
                    .unwrap(),
                AuthzDecision::Deny,
                "{action:?} must be denied without a project membership"
            );
        }

        // Grant project membership to the same principal.
        fixture.seat_on_project(&database.admin, "developer").await;
        for action in [Action::AppsRead, Action::AppsDeploy] {
            assert_eq!(
                enforce(pg, &policies, &fixture.ctx(action, fixture.app()))
                    .await
                    .unwrap(),
                AuthzDecision::Allow,
                "{action:?} must be allowed once the project membership exists"
            );
        }
    })
    .await;
}

#[compio::test]
async fn the_effective_rank_is_the_minimum_of_the_two_seats() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();

        // Organization developer, project OWNER: the project cannot widen, so
        // organization-admin authority over the project stays refused while
        // ordinary developer authority works.
        let ceiling = Membership::new(&database.admin, "narrow-ceiling", "developer").await;
        ceiling.seat_on_project(&database.admin, "owner").await;
        assert_eq!(
            enforce(
                pg,
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
                pg,
                &policies,
                &ceiling.ctx(Action::ProjectMembersWrite, ceiling.project())
            )
            .await
            .unwrap(),
            AuthzDecision::Deny,
            "a project owner seat must NOT lift a developer to project administration"
        );

        // Organization developer, project VIEWER: the project CEILINGS, so the
        // organization seat is cut down to reads inside it - the other half of
        // the same rule. The organization seat is deliberately below admin,
        // because at admin and above the narrowing stops applying at all
        // (exercised separately below).
        let floor = Membership::new(&database.admin, "narrow-floor", "developer").await;
        floor.seat_on_project(&database.admin, "viewer").await;
        assert_eq!(
            enforce(pg, &policies, &floor.ctx(Action::AppsRead, floor.app()))
                .await
                .unwrap(),
            AuthzDecision::Allow,
            "a viewer project seat still reads"
        );
        assert_eq!(
            enforce(pg, &policies, &floor.ctx(Action::AppsDeploy, floor.app()))
                .await
                .unwrap(),
            AuthzDecision::Deny,
            "a viewer project seat must ceiling an organization developer"
        );
    })
    .await;
}

#[compio::test]
async fn admin_and_above_reach_every_project_without_a_row() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();

        for (role, expected) in [
            ("developer", AuthzDecision::Deny),
            ("admin", AuthzDecision::Allow),
            ("owner", AuthzDecision::Allow),
        ] {
            let fixture = Membership::new(&database.admin, &format!("wide-{role}"), role).await;
            assert_eq!(
                enforce(
                    pg,
                    &policies,
                    &fixture.ctx(Action::AppsDeploy, fixture.app())
                )
                .await
                .unwrap(),
                expected,
                "{role} deploying into a project it holds no row on"
            );
        }
    })
    .await;
}

#[compio::test]
async fn a_deleted_membership_denies_the_very_next_request() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::new(&database.admin, "revocation", "owner").await;
        let policies = load_platform_policies().unwrap();

        assert_eq!(
            enforce(
                pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, fixture.app())
            )
            .await
            .unwrap(),
            AuthzDecision::Allow,
        );

        database
            .admin
            .execute(
                "DELETE FROM zeroship.organization_members \
             WHERE organization_id = $1 AND user_id = $2",
                &[&fixture.organization_id, &fixture.user_id.as_str()],
            )
            .await
            .expect("revoke the membership");

        assert_eq!(
            enforce(
                pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, fixture.app())
            )
            .await
            .unwrap(),
            AuthzDecision::Deny,
            "a committed revocation must take effect on the next request, with no cache to bust",
        );
    })
    .await;
}

#[compio::test]
async fn an_unseated_creator_is_denied_cross_tenant_reads() {
    Database::run(async |database| {
        let pg = &database.service;
        let victim = Membership::new(&database.admin, "c1-victim", "owner").await;

        let attacker_id = UserId::mint();
        database
            .admin
            .execute(
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
                enforce(pg, &policies, &ctx).await.unwrap(),
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
                enforce(pg, &policies, &ctx).await.unwrap(),
                AuthzDecision::Deny,
                "an unseated creator must not read {action:?} on another organization",
            );
        }

        assert_eq!(
            enforce(
                pg,
                &policies,
                &victim.ctx(Action::SecretsRead, victim.app())
            )
            .await
            .unwrap(),
            AuthzDecision::Allow,
            "the owner must still read secrets on their own app",
        );
    })
    .await;
}
