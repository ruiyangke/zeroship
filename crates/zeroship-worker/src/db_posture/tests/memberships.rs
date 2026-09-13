use super::*;

async fn memberships(client: &compio_postgres::Client) -> (i64, Option<String>) {
    let row = client
        .query_one(INHERITED_MEMBERSHIPS_SQL, &[&AMBIENT_MEMBERSHIP_EXEMPTION])
        .await
        .expect("inspect the connected login's memberships");
    (
        row.get("inheriting_memberships"),
        row.get("inheriting_membership_example"),
    )
}

#[compio::test]
async fn membership_inheritance_changes_are_visible_on_the_same_login_connection() {
    Database::run(async |database| {
        database
            .admin
            .batch_execute(
                "CREATE ROLE tenant_role; CREATE ROLE fixture_login LOGIN PASSWORD 'fixture_login'",
            )
            .await
            .unwrap();
        let login = database.connect_as("fixture_login").await;
        assert_eq!(memberships(&login).await, (0, None));

        database
            .admin
            .batch_execute("GRANT tenant_role TO fixture_login WITH INHERIT TRUE")
            .await
            .unwrap();
        assert_eq!(memberships(&login).await, (1, Some("tenant_role".into())));

        // Changing the login default does not fence an existing membership.
        database
            .admin
            .batch_execute("ALTER ROLE fixture_login NOINHERIT")
            .await
            .unwrap();
        assert_eq!(memberships(&login).await, (1, Some("tenant_role".into())));

        database
            .admin
            .batch_execute("GRANT tenant_role TO fixture_login WITH INHERIT FALSE")
            .await
            .unwrap();
        assert_eq!(memberships(&login).await, (0, None));
    })
    .await;
}

#[compio::test]
async fn the_workflow_owner_is_exempt_while_other_inherited_roles_are_reported() {
    Database::migrated(async |database| {
        let worker = database.connect_as(WORKER_DATABASE_ROLE).await;
        let owns_journal: bool = worker.query_one(
            "SELECT pg_has_role(current_user, 'zeroship_workflow_owner', 'USAGE')", &[],
        ).await.unwrap().get(0);
        assert!(owns_journal, "the accepted role must actually inherit journal authority");
        assert_eq!(memberships(&worker).await, (0, None));

        database.admin.batch_execute("CREATE ROLE unexpected_role; GRANT unexpected_role TO zeroship_worker WITH INHERIT TRUE").await.unwrap();
        assert_eq!(memberships(&worker).await, (1, Some("unexpected_role".into())));
        let error = validate_database_url(database.url_as(WORKER_DATABASE_ROLE).as_str()).await.unwrap_err();
        assert!(error.contains("unexpected_role"), "{error}");

        database.admin.batch_execute("GRANT unexpected_role TO zeroship_worker WITH INHERIT FALSE").await.unwrap();
        validate_database_url(database.url_as(WORKER_DATABASE_ROLE).as_str()).await.unwrap();
    }).await;
}

#[compio::test]
async fn fencing_a_grant_does_not_hide_an_inheriting_sibling_from_another_grantor() {
    Database::run(async |database| {
        database.admin.batch_execute(
            "CREATE ROLE tenant_role; CREATE ROLE alternate_grantor; \
             CREATE ROLE fixture_login LOGIN PASSWORD 'fixture_login'; \
             GRANT tenant_role TO alternate_grantor WITH ADMIN TRUE; \
             GRANT tenant_role TO fixture_login WITH INHERIT FALSE; \
             SET ROLE alternate_grantor; \
             GRANT tenant_role TO fixture_login WITH INHERIT TRUE; RESET ROLE;",
        ).await.unwrap();
        let login = database.connect_as("fixture_login").await;
        assert_eq!(memberships(&login).await, (1, Some("tenant_role".into())));
        database.admin.batch_execute("REVOKE tenant_role FROM fixture_login").await.unwrap();
        assert_eq!(memberships(&login).await, (1, Some("tenant_role".into())));
        database.admin.batch_execute(
            "SET ROLE alternate_grantor; GRANT tenant_role TO fixture_login WITH INHERIT FALSE; RESET ROLE",
        ).await.unwrap();
        assert_eq!(memberships(&login).await, (0, None));
    }).await;
}
