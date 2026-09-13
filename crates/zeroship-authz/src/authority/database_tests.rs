//! Resolve authority from the migrated organization and project model.

use crate::tests::{database::Database, membership::Membership};
use crate::{
    authority, enforce, is_authorized_anywhere, load_platform_policies, Action, AuthzDecision,
    Resource,
};
use zeroship_id::{AppId, UserId};

#[compio::test]
async fn an_app_resolves_its_authority_through_its_project() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::new(&database.admin, "resolve-chain", "developer").await;
        fixture.seat_on_project(&database.admin, "developer").await;
        let policies = load_platform_policies().unwrap();

        let resolved = authority::resolve(pg, &fixture.user_id, &fixture.app())
            .await
            .expect("resolve must not error across the app -> project chain");
        assert_eq!(
            resolved.effective_rank, 20,
            "the developer seat must survive the app -> project -> organization join"
        );

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
    })
    .await;
}

#[compio::test]
async fn an_unknown_app_denies_without_erroring() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::new(&database.admin, "resolve-missing", "owner").await;
        let policies = load_platform_policies().unwrap();

        let missing = Resource::App { id: AppId::mint() };
        let resolved = authority::resolve(pg, &fixture.user_id, &missing)
            .await
            .expect("an unknown app is not a database failure");
        assert_eq!(resolved.effective_rank, 0);
        assert_eq!(resolved.billing_rank, 0);

        assert_eq!(
            enforce(pg, &policies, &fixture.ctx(Action::AppsRead, missing))
                .await
                .unwrap(),
            AuthzDecision::Deny,
        );
    })
    .await;
}

#[compio::test]
async fn an_unknown_principal_is_a_validation_error_not_rank_zero() {
    Database::run(async |database| {
        let pg = &database.service;
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
            let unknown = UserId::mint();
            let err = authority::resolve(pg, &unknown, &resource)
                .await
                .expect_err("an unknown principal must not resolve");
            assert!(
                matches!(err, crate::AuthzError::Validation(_)),
                "{resource:?} produced {err:?}"
            );
        }
    })
    .await;
}

#[compio::test]
async fn the_probe_reads_the_text_id_columns_and_returns_valid_resources() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::new(&database.admin, "probe-ids", "developer").await;
        fixture.seat_on_project(&database.admin, "viewer").await;

        let organizations = authority::organization_resources(pg, &fixture.user_id)
            .await
            .expect("organization memberships must read as text");
        assert_eq!(
            organizations,
            vec![Resource::Organization {
                id: fixture.organization_id.clone()
            }]
        );

        let projects = authority::project_probe_resources(pg, &fixture.user_id)
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
            pg,
            &policies,
            &fixture.ctx(Action::AppsRead, Resource::Any)
        )
        .await
        .unwrap());
        assert!(
            !is_authorized_anywhere(
                pg,
                &policies,
                &fixture.ctx(Action::AppsDeploy, Resource::Any)
            )
            .await
            .unwrap(),
            "a viewer project seat ceilings an organization developer on the consent path too"
        );
    })
    .await;
}
