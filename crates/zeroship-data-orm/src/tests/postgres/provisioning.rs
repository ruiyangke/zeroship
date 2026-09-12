use crate::tests::fixtures::roles::*;
use zeroship_core::database_role::per_app_role_name;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_local_role_sql_shape_is_quoted_and_correct() {
        assert_eq!(
            set_local_role_sql("app_demo").unwrap(),
            r#"SET LOCAL ROLE "app_app_demo_role""#
        );
    }

    #[test]
    fn the_role_statement_is_transaction_scoped_not_session_scoped() {
        let sql = set_local_role_sql("app_demo").unwrap();
        assert!(
            sql.starts_with("SET LOCAL ROLE "),
            "the role fence must be transaction-scoped: {sql}"
        );
    }

    #[test]
    fn set_role_sql_doubles_embedded_quote_via_quote_ident() {
        assert_eq!(
            set_local_role_sql(r#"a"b"#).unwrap(),
            r#"SET LOCAL ROLE "app_a""b_role""#
        );
    }

    #[test]
    fn role_creation_attributes_exclude_platform_authority() {
        let attrs = format!(
            "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"{APP_ROLE_TEMPLATE}\""
        );
        assert!(attrs.contains("NOLOGIN"));
        assert!(attrs.contains("NOREPLICATION"));
        assert!(attrs.contains("NOCREATEDB"));
        assert!(attrs.contains("NOCREATEROLE"));
        assert!(!attrs.contains(" REPLICATION"));
    }

    #[test]
    fn app_role_template_has_the_shared_name() {
        assert_eq!(APP_ROLE_TEMPLATE, "__zeroship_app_role_template");
    }

    #[test]
    fn per_app_role_composition_remains_shared() {
        assert_eq!(per_app_role_name("app_demo").unwrap(), "app_app_demo_role");
    }
}

#[cfg(test)]
mod live_role_grant_tests {
    use super::*;
    use compio_postgres::{Client, NoTls, Pool};

    use crate::backend::pg_session_sql::tx_session_setup_sql;

    const TABLES: &[&str] = &[
        "widgets",
        "__zeroship_schema_migrations",
        "__zeroship_audit_unmask",
    ];

    async fn admin_client() -> (crate::tests::fixtures::postgres::Postgres, Client) {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let (client, connection) = compio_postgres::connect(&postgres.url(), NoTls)
            .await
            .expect("connect to the data ORM test database");
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        (postgres, client)
    }

    fn scratch_app(label: &str) -> String {
        format!("zs{label}_{}", uuid::Uuid::new_v4().simple())
    }

    fn table_ref(app: &str, table: &str) -> String {
        format!(
            "{}.{}",
            crate::sql::mapping::quote_ident(app),
            crate::sql::mapping::quote_ident(table)
        )
    }

    async fn create_schema_with_tables(admin: &Client, app: &str, tables: &[&str]) {
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA {}",
                crate::sql::mapping::quote_ident(app)
            ))
            .await
            .expect("create scratch app schema");
        create_tables(admin, app, tables).await;
    }

    async fn create_tables(admin: &Client, app: &str, tables: &[&str]) {
        for table in tables {
            admin
                .batch_execute(&format!(
                    "CREATE TABLE {} (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
                    table_ref(app, table)
                ))
                .await
                .expect("create scratch app table");
        }
    }

    async fn teardown(admin: &Client, app: &str) {
        let role = crate::sql::mapping::quote_ident(
            &per_app_role_name(app).expect("scratch app role name"),
        );
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                crate::sql::mapping::quote_ident(app)
            ))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP OWNED BY {role} CASCADE"))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP ROLE IF EXISTS {role}"))
            .await;
    }

    async fn as_runtime_role(
        admin: &Client,
        app: &str,
        sql: &str,
    ) -> Result<(), compio_postgres::Error> {
        let schema = crate::sql::SchemaName::new(app).expect("scratch app id is a schema");
        admin.batch_execute("BEGIN").await?;
        let scoped = async {
            admin
                .batch_execute(
                    &tx_session_setup_sql(&schema, crate::connection::SessionAuthority::PerAppRole)
                        .expect("scratch app role name"),
                )
                .await?;
            admin.batch_execute(sql).await
        }
        .await;
        let _ = admin
            .batch_execute(if scoped.is_ok() { "COMMIT" } else { "ROLLBACK" })
            .await;
        scoped
    }

    fn denied(error: &compio_postgres::Error) -> bool {
        error.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    }

    async fn assert_runtime_dml(admin: &Client, app: &str, table: &str) {
        let relation = table_ref(app, table);
        for sql in [
            format!("INSERT INTO {relation} (name) VALUES ('created')"),
            format!("UPDATE {relation} SET name = 'updated' WHERE name = 'created'"),
            format!("SELECT name FROM {relation}"),
            format!("DELETE FROM {relation} WHERE name = 'updated'"),
        ] {
            as_runtime_role(admin, app, &sql)
                .await
                .unwrap_or_else(|error| panic!("runtime DML failed for {relation}: {error:?}"));
        }
    }

    async fn assert_ordinary_dml_grants(admin: &Client, app: &str, role: &str, table: &str) {
        let privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 AND table_schema = $2 AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role, app, table],
            )
            .await
            .expect("query runtime table grants")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        assert_eq!(
            privileges,
            ["DELETE", "INSERT", "SELECT", "UPDATE"],
            "unexpected runtime grants for {}",
            table_ref(app, table)
        );
    }

    #[compio::test]
    async fn every_bound_schema_table_receives_runtime_dml() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app("grants");
        teardown(&admin, &app).await;
        create_schema_with_tables(&admin, &app, TABLES).await;

        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for role provisioning");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision runtime role");

        for table in TABLES {
            assert_runtime_dml(&admin, &app, table).await;
        }

        let future_table = "created_after_provisioning";
        create_tables(&admin, &app, &[future_table]).await;
        assert_runtime_dml(&admin, &app, future_table).await;

        let role = per_app_role_name(&app).expect("scratch app role name");
        let catalog_tables = admin
            .query_text_params(
                "SELECT table_name FROM information_schema.tables \
                  WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
                  ORDER BY table_name",
                &[app.as_str()],
            )
            .await
            .expect("enumerate bound-schema tables")
            .into_iter()
            .map(|row| row.get::<_, String>("table_name"))
            .collect::<Vec<_>>();
        assert!(!catalog_tables.is_empty());
        for table in catalog_tables {
            assert_ordinary_dml_grants(&admin, &app, &role, &table).await;
        }

        teardown(&admin, &app).await;
    }

    #[compio::test]
    async fn runtime_role_keeps_ddl_and_sibling_schema_out_of_reach() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app("owner");
        let sibling = scratch_app("sibling");
        teardown(&admin, &app).await;
        teardown(&admin, &sibling).await;
        create_schema_with_tables(&admin, &app, &["widgets"]).await;
        create_schema_with_tables(&admin, &sibling, &["secrets"]).await;

        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for role provisioning");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision runtime role");

        for (operation, sql) in [
            (
                "CREATE TABLE",
                format!("CREATE TABLE {} (id BIGINT)", table_ref(&app, "forbidden")),
            ),
            (
                "ALTER TABLE",
                format!(
                    "ALTER TABLE {} ADD COLUMN extra TEXT",
                    table_ref(&app, "widgets")
                ),
            ),
            (
                "DROP TABLE",
                format!("DROP TABLE {}", table_ref(&app, "widgets")),
            ),
            (
                "TRUNCATE",
                format!("TRUNCATE {}", table_ref(&app, "widgets")),
            ),
            (
                "sibling SELECT",
                format!("SELECT * FROM {}", table_ref(&sibling, "secrets")),
            ),
        ] {
            let error = match as_runtime_role(&admin, &app, &sql).await {
                Ok(()) => panic!("runtime role unexpectedly performed {operation}"),
                Err(error) => error,
            };
            assert!(
                denied(&error),
                "expected insufficient privilege for {operation}, got {error:?}"
            );
        }

        teardown(&admin, &app).await;
        teardown(&admin, &sibling).await;
    }

    #[compio::test]
    async fn role_lifecycle_is_idempotent_and_transaction_scoped() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app("lifecycle");
        teardown(&admin, &app).await;
        create_schema_with_tables(&admin, &app, &["widgets"]).await;

        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for role provisioning");
        let first = ensure_per_app_role(&pool, &app)
            .await
            .expect("create runtime role");
        let second = ensure_per_app_role(&pool, &app)
            .await
            .expect("re-provision runtime role");
        assert!(first.created_role);
        assert!(!second.created_role);

        let session_user: String = admin
            .query_one_scalar("SELECT current_user", &[])
            .await
            .expect("read admin identity");
        as_runtime_role(&admin, &app, "SELECT 1")
            .await
            .expect("run transaction as runtime role");
        let restored_user: String = admin
            .query_one_scalar("SELECT current_user", &[])
            .await
            .expect("read identity after commit");
        assert_eq!(restored_user, session_user);

        admin
            .batch_execute(&format!(
                "DROP SCHEMA {} CASCADE",
                crate::sql::mapping::quote_ident(&app)
            ))
            .await
            .expect("drop scratch app schema");
        drop_per_app_role(&pool, &app)
            .await
            .expect("drop runtime role");
        let role = per_app_role_name(&app).expect("scratch app role name");
        let remaining = admin
            .query_text_params(
                "SELECT 1 FROM pg_roles WHERE rolname = $1",
                &[role.as_str()],
            )
            .await
            .expect("query dropped role");
        assert!(remaining.is_empty());
    }
}
