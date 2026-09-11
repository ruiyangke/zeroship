//! PostgreSQL workflow contracts.
use super::fixtures::*;

use compio_postgres::{NoTls, Pool};

use uuid::Uuid;

#[compio::test]
async fn workflow_journal_redeploy_grants_do_not_reopen_without_reprovision() {
    let (_postgres, url) = require_pg().await;
    let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("workflow provision pg client");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let app_id = Uuid::new_v4();
    let app_schema = zeroship_workflow::store::pg::app_schema_for(&app_id);
    let tables = zeroship_workflow::store::pg::WorkflowTables::for_app_id(&app_id);
    let schema_role = zeroship_core::database_role::per_app_role_name(&app_schema)
        .expect("workflow schema must produce a valid PostgreSQL role name");
    let uuid_role = zeroship_core::database_role::per_app_role_name(&app_id.to_string())
        .expect("workflow app id must produce a valid PostgreSQL role name");

    let _ = pool
        .execute(
            &format!("DROP SCHEMA IF EXISTS \"{app_schema}\" CASCADE"),
            &[],
        )
        .await;
    for role in [&schema_role, &uuid_role] {
        let _ = pool
            .execute(&format!("DROP OWNED BY \"{role}\""), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
            .await;
    }

    // Stand in for `db/migrations-ts/20260818000200_worker_database_authority.ts`.
    // This suite runs against a bare database with no platform migrations
    // applied, and since 2a44ea8ef nothing in the worker creates this role:
    // `PgStore::provision` opens with `SET ROLE zeroship_workflow_owner` and
    // fails outright if it is absent. The attribute list is copied from that
    // migration, so a test-created role cannot be wider than the deployed one.
    //
    // WHAT THIS DOES NOT CATCH: the migration ceasing to create the role, or
    // creating it wider. Creating it here makes this test green either way.
    // `platform_migrate.rs` is what rules on the deployed role.
    pool.execute(
        &format!(
            "DO $$ BEGIN \
               IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{owner}') THEN \
                 CREATE ROLE \"{owner}\" NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
                                         NOINHERIT NOREPLICATION NOBYPASSRLS; \
               END IF; \
             END $$",
            owner = zeroship_migrate_server::provisioning::WORKFLOW_OWNER_ROLE,
        ),
        &[],
    )
    .await
    .expect("precreate the narrow workflow journal owner role");
    // The journal SCHEMA is created by the deploy's migration apply, not by the
    // worker -- `PgStore::provision` holds no CREATE on the database. Call the
    // migration service's own statement rather than a CREATE SCHEMA of our own,
    // so the journal below is owned the way production owns it.
    zeroship_migrate_server::provisioning::provision_workflow_journal_schema(&client, &app_id)
        .await
        .expect("provision the app workflow journal schema");
    zeroship_workflow::store::pg::PgStore::provision(&client, &app_id)
        .await
        .expect("provision app-local workflow journal");
    crate::tests::fixtures::roles::ensure_per_app_role(&pool, &app_schema)
        .await
        .expect("redeploy plugin-db per-app role grants");

    for table in tables.all() {
        let rows = pool
            .query_text_params(
                "SELECT \
                    has_table_privilege($1, $2, 'SELECT') AS sel, \
                    has_table_privilege($1, $2, 'INSERT') AS ins, \
                    has_table_privilege($1, $2, 'UPDATE') AS upd, \
                    has_table_privilege($1, $2, 'DELETE') AS del",
                &[schema_role.as_str(), table],
            )
            .await
            .expect("check journal table privileges");
        let row = &rows[0];
        assert!(
            !row.get::<_, bool>("sel"),
            "app role must not SELECT {table}"
        );
        assert!(
            !row.get::<_, bool>("ins"),
            "app role must not INSERT {table}"
        );
        assert!(
            !row.get::<_, bool>("upd"),
            "app role must not UPDATE {table}"
        );
        assert!(
            !row.get::<_, bool>("del"),
            "app role must not DELETE {table}"
        );

        let owner_rows = pool
            .query_text_params(
                "SELECT pg_get_userbyid(c.relowner) AS owner \
                   FROM pg_class c \
                  WHERE c.oid = to_regclass($1)",
                &[table],
            )
            .await
            .expect("check journal table owner");
        let owner: String = owner_rows[0].get("owner");
        // Bound to `zeroship-migrate-server`'s copy of the owner-role name while the
        // writer is `plugin-workflow`'s private copy of it, so the two
        // duplicated constants disagreeing shows up here rather than as a
        // journal nobody can reach. Until 2026-08-20 this compared against
        // `__zeroship_platform_role`, the role the store created for itself
        // before 2a44ea8ef removed `provision_owner_sql`.
        assert_eq!(
            owner,
            zeroship_migrate_server::provisioning::WORKFLOW_OWNER_ROLE,
            "journal owner for {table}"
        );
        // The security property the name is a proxy for: no role an app's own
        // code runs as may own the journal, because an owner can re-GRANT
        // itself the DML the assertions above just proved it lacks.
        assert_ne!(
            owner, schema_role,
            "journal owner for {table} is an app role"
        );
        assert_ne!(owner, uuid_role, "journal owner for {table} is an app role");
    }

    let _ = pool
        .execute(
            &format!("DROP SCHEMA IF EXISTS \"{app_schema}\" CASCADE"),
            &[],
        )
        .await;
    for role in [&schema_role, &uuid_role] {
        let _ = pool
            .execute(&format!("DROP OWNED BY \"{role}\""), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
            .await;
    }
    drop(client);
    release_pg(pool).await;
}
