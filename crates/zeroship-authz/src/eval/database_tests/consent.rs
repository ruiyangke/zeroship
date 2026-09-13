use crate::tests::{database::Database, membership::Membership};
use crate::{is_authorized_anywhere, Resource};
use crate::{load_platform_policies, Action};

#[compio::test]
async fn the_consent_probe_answers_organization_actions_truthfully() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();

        for (role, members_write, deploy) in [
            ("viewer", false, false),
            ("developer", false, false),
            ("admin", true, true),
            ("owner", true, true),
        ] {
            let fixture = Membership::new(&database.admin, &format!("probe-{role}"), role).await;
            assert_eq!(
                is_authorized_anywhere(
                    pg,
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
                    pg,
                    &policies,
                    &fixture.ctx(Action::AppsDeploy, Resource::Any)
                )
                .await
                .unwrap(),
                deploy,
                "{role} delegating apps:deploy"
            );
        }
    })
    .await;
}

#[compio::test]
async fn the_consent_probe_finds_an_explicit_project_seat() {
    Database::run(async |database| {
        let pg = &database.service;
        let policies = load_platform_policies().unwrap();
        let fixture = Membership::new(&database.admin, "probe-project", "developer").await;

        assert!(
            !is_authorized_anywhere(
                pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, Resource::Any)
            )
            .await
            .unwrap(),
            "a developer with no project seat can delegate apps:deploy nowhere"
        );

        fixture.seat_on_project(&database.admin, "developer").await;

        assert!(
            is_authorized_anywhere(
                pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, Resource::Any)
            )
            .await
            .unwrap(),
            "the probe must find the project the developer was just seated on"
        );
    })
    .await;
}
