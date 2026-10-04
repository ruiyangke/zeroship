use super::*;

async fn memberships(client: &compio_postgres::Client) -> (i64, Option<String>) {
    let row = client
        .query_one(INHERITED_MEMBERSHIPS_SQL, &[])
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
        let tenant_role = database.mint_role("tenant_role");
        let fixture_login = database.mint_role("fixture_login");
        database
            .admin
            .batch_execute(&format!(
                "CREATE ROLE \"{tenant_role}\"; \
                 CREATE ROLE \"{fixture_login}\" LOGIN PASSWORD '{fixture_login}'"
            ))
            .await
            .unwrap();
        let login = database.connect_as(&fixture_login).await;
        assert_eq!(memberships(&login).await, (0, None));

        database
            .admin
            .batch_execute(&format!(
                "GRANT \"{tenant_role}\" TO \"{fixture_login}\" WITH INHERIT TRUE"
            ))
            .await
            .unwrap();
        assert_eq!(
            memberships(&login).await,
            (1, Some(tenant_role.clone()))
        );

        // Changing the login default does not fence an existing membership.
        database
            .admin
            .batch_execute(&format!("ALTER ROLE \"{fixture_login}\" NOINHERIT"))
            .await
            .unwrap();
        assert_eq!(
            memberships(&login).await,
            (1, Some(tenant_role.clone()))
        );

        database
            .admin
            .batch_execute(&format!(
                "GRANT \"{tenant_role}\" TO \"{fixture_login}\" WITH INHERIT FALSE"
            ))
            .await
            .unwrap();
        assert_eq!(memberships(&login).await, (0, None));
    })
    .await;
}

#[compio::test]
async fn any_inherited_role_is_reported_and_refuses_boot() {
    // Isolated: this grants and revokes a membership on zeroship_worker itself,
    // which every worker test process of the shared server would then also see
    // (see `Database::isolated`'s doc).
    Database::isolated(async |database| {
        let worker = database.connect_as(WORKER_DATABASE_ROLE).await;
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
        let tenant_role = database.mint_role("tenant_role");
        let alternate_grantor = database.mint_role("alternate_grantor");
        let fixture_login = database.mint_role("fixture_login");
        database.admin.batch_execute(&format!(
            "CREATE ROLE \"{tenant_role}\"; CREATE ROLE \"{alternate_grantor}\"; \
             CREATE ROLE \"{fixture_login}\" LOGIN PASSWORD '{fixture_login}'; \
             GRANT \"{tenant_role}\" TO \"{alternate_grantor}\" WITH ADMIN TRUE; \
             GRANT \"{tenant_role}\" TO \"{fixture_login}\" WITH INHERIT FALSE; \
             SET ROLE \"{alternate_grantor}\"; \
             GRANT \"{tenant_role}\" TO \"{fixture_login}\" WITH INHERIT TRUE; RESET ROLE;"
        )).await.unwrap();
        let login = database.connect_as(&fixture_login).await;
        assert_eq!(memberships(&login).await, (1, Some(tenant_role.clone())));
        database.admin.batch_execute(&format!(
            "REVOKE \"{tenant_role}\" FROM \"{fixture_login}\""
        )).await.unwrap();
        assert_eq!(memberships(&login).await, (1, Some(tenant_role.clone())));
        database.admin.batch_execute(&format!(
            "SET ROLE \"{alternate_grantor}\"; \
             GRANT \"{tenant_role}\" TO \"{fixture_login}\" WITH INHERIT FALSE; RESET ROLE",
        )).await.unwrap();
        assert_eq!(memberships(&login).await, (0, None));
    }).await;
}

/// A tenant schema holding one row, reachable only through a binding role in
/// the shape the cluster reconciler grants it: a database capability role
/// carrying the privilege, inherited by the binding and never settable
/// (`WITH SET FALSE`), and the binding assumable by the worker and never
/// inherited (`WITH INHERIT FALSE`). Roles are named by the derivation the
/// reconciler and the data plane share, so the fixture is the current model
/// rather than a stand-in for it.
struct Tenant {
    schema: String,
    binding: String,
}

#[expect(
    clippy::future_not_send,
    reason = "the worker database fixture stays on its compio runtime"
)]
impl Tenant {
    async fn provision(database: &Database) -> Self {
        let tenant = zeroship_core::DatabaseId::mint();
        let schema = zeroship_core::database_derivation::schema_name(&tenant);
        let capability = zeroship_core::database_derivation::capability_role_name(
            &tenant,
            zeroship_core::database_role::DatabaseCapability::ReadWrite,
        )
        .unwrap();
        let binding = zeroship_core::database_derivation::binding_role_name(
            &zeroship_core::BindingId::mint(),
        )
        .unwrap();
        database
            .admin
            .batch_execute(&format!(
                "CREATE SCHEMA \"{schema}\";
                 CREATE TABLE \"{schema}\".secrets (v text);
                 INSERT INTO \"{schema}\".secrets VALUES ('tenant-secret');
                 CREATE ROLE \"{capability}\" NOLOGIN;
                 GRANT USAGE ON SCHEMA \"{schema}\" TO \"{capability}\";
                 GRANT SELECT ON \"{schema}\".secrets TO \"{capability}\";
                 CREATE ROLE \"{binding}\" NOLOGIN INHERIT;
                 GRANT \"{capability}\" TO \"{binding}\" WITH SET FALSE;
                 GRANT \"{binding}\" TO {WORKER_DATABASE_ROLE} WITH INHERIT FALSE;"
            ))
            .await
            .expect("provision the tenant schema and its binding ladder");
        Self { schema, binding }
    }

    /// The secret read with no narrowing, on a fresh worker session. `Ok` is
    /// the row; `Err` is a privilege refusal and nothing else, because a typo,
    /// a missing table or the wrong protocol would also be an error and every
    /// one of them would read as a fence that held.
    async fn bare_read(&self, database: &Database) -> Result<String, String> {
        let worker = database.connect_as(WORKER_DATABASE_ROLE).await;
        worker
            .query_one(&format!("SELECT v FROM \"{}\".secrets", self.schema), &[])
            .await
            .map(|row| row.get::<_, String>(0))
            .map_err(|error| {
                let refused = error
                    .as_db_error()
                    .unwrap_or_else(|| panic!("expected a server error, got {error}"));
                assert_eq!(
                    refused.code(),
                    &compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE,
                    "expected a privilege refusal, got {} {}",
                    refused.code().code(),
                    refused.message()
                );
                refused.message().to_owned()
            })
    }

    /// The secret read the way the data plane reads it: `SET LOCAL ROLE` to
    /// the binding inside an explicit transaction.
    async fn narrowed_read(&self, database: &Database) -> String {
        let worker = database.connect_as(WORKER_DATABASE_ROLE).await;
        worker
            .simple_query(&format!(
                "BEGIN; SET LOCAL ROLE \"{}\"; SELECT v FROM \"{}\".secrets; COMMIT",
                self.binding, self.schema
            ))
            .await
            .expect("a session narrowed to the binding reaches the tenant schema")
            .iter()
            .find_map(|message| match message {
                compio_postgres::SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                _ => None,
            })
            .expect("the narrowed read returns the row")
    }
}

/// The `PostgreSQL` behaviour the row-counting shape of
/// [`INHERITED_MEMBERSHIPS_SQL`] rests on, exhibited on the secret itself
/// rather than on the catalog.
///
/// `pg_auth_members` is unique on `(roleid, member, grantor)`, so a second
/// grantor adds a second row, and membership takes the union across rows: one
/// inheriting row makes the binding's privileges ambient on the worker login
/// whatever the reconciler's own row says. A `REVOKE`, even by a superuser,
/// removes only the issuing grantor's row. A posture check that asked whether
/// "the" membership is fenced would pass on both states below while a bare
/// statement reads across the tenant boundary, so the check counts rows and
/// refuses boot on either.
#[compio::test]
async fn a_second_grantors_inheriting_grant_re_opens_the_fence_and_a_plain_revoke_leaves_it_open() {
    // Isolated: this grants and revokes zeroship_worker's own memberships
    // through a second grantor, which every worker test process of the shared
    // server would then also see (see `Database::isolated`'s doc).
    Database::isolated(async |database| {
        let tenant = Tenant::provision(database).await;
        let worker_url = database.url_as(WORKER_DATABASE_ROLE);

        // THE FENCE, and its controls. The narrowed read proves the ladder
        // grants something real, so the bare refusal is the fence and not an
        // unprovisioned role; the boot check accepts this state.
        assert_eq!(tenant.narrowed_read(database).await, "tenant-secret");
        let fenced = tenant
            .bare_read(database)
            .await
            .expect_err("the reconciler's grant alone must not make the binding ambient");
        assert!(fenced.contains("permission denied"), "{fenced}");
        validate_database_url(worker_url.as_str())
            .await
            .expect("the fenced ladder is the accepted boot posture");

        // A second grantor issues the inheriting grant of the same pair.
        let binding = &tenant.binding;
        database
            .admin
            .batch_execute(&format!(
                "CREATE ROLE alternate_grantor NOLOGIN;
                 GRANT \"{binding}\" TO alternate_grantor WITH ADMIN TRUE;
                 SET ROLE alternate_grantor;
                 GRANT \"{binding}\" TO {WORKER_DATABASE_ROLE} WITH INHERIT TRUE;
                 RESET ROLE;"
            ))
            .await
            .expect("a second grantor's inheriting grant");
        let rows: Vec<bool> = database
            .admin
            .query(
                "SELECT membership.inherit_option \
                   FROM pg_auth_members membership \
                   JOIN pg_roles granted ON granted.oid = membership.roleid \
                   JOIN pg_roles member ON member.oid = membership.member \
                  WHERE granted.rolname = $1 AND member.rolname = $2 \
                  ORDER BY membership.inherit_option",
                &[binding, &WORKER_DATABASE_ROLE],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            rows,
            [false, true],
            "the pair holds one fenced row and one inheriting row"
        );
        assert_eq!(
            tenant.bare_read(database).await.as_deref(),
            Ok("tenant-secret"),
            "the inheriting row re-opens the fence beside the fenced one"
        );
        let refused = validate_database_url(worker_url.as_str())
            .await
            .expect_err("boot must refuse the re-opened fence");
        assert!(refused.contains(binding.as_str()), "{refused}");

        // A plain REVOKE by the superuser removes its own row and no other.
        database
            .admin
            .batch_execute(&format!("REVOKE \"{binding}\" FROM {WORKER_DATABASE_ROLE}"))
            .await
            .expect("a plain revoke");
        assert_eq!(
            tenant.bare_read(database).await.as_deref(),
            Ok("tenant-secret"),
            "the other grantor's inheriting row survives a superuser REVOKE"
        );
        let refused = validate_database_url(worker_url.as_str())
            .await
            .expect_err("boot must still refuse after the plain revoke");
        assert!(refused.contains(binding.as_str()), "{refused}");

        // The grantor that issued the row is the one that can fence it.
        database
            .admin
            .batch_execute(&format!(
                "SET ROLE alternate_grantor;
                 GRANT \"{binding}\" TO {WORKER_DATABASE_ROLE} WITH INHERIT FALSE;
                 RESET ROLE;"
            ))
            .await
            .expect("the issuing grantor fences its own row");
        tenant
            .bare_read(database)
            .await
            .expect_err("a fenced row leaves nothing ambient");
        assert_eq!(tenant.narrowed_read(database).await, "tenant-secret");
        validate_database_url(worker_url.as_str())
            .await
            .expect("the fenced row is the accepted boot posture again");
    })
    .await;
}
