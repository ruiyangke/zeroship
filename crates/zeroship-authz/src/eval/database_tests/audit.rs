use crate::tests::{database::Database, membership::Membership};
use crate::{enforce, load_platform_policies, Action, AuthzDecision};

#[compio::test]
async fn audit_records_the_resource_type_and_matched_bands() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::new(&database.admin, "audit", "owner").await;
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
                enforce(pg, &policies, &fixture.ctx(action, resource))
                    .await
                    .unwrap(),
                AuthzDecision::Allow,
            );

            let rows = database
                .admin
                .query(
                    "SELECT resource_type, resource_id, decision, matched_policies \
                     FROM zeroship.authz_decisions \
                     WHERE actor_user_id = $1 AND action = $2 \
                     ORDER BY occurred_at",
                    &[&fixture.user_id.as_str(), &action.cedar_id()],
                )
                .await
                .expect("select audit row");
            assert_eq!(rows.len(), 1, "each call writes its own audit decision");
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
    })
    .await;
}
