//! What a session narrowed to a binding role may and may not do.
use crate::tests::fixtures::roles::*;
use crate::tests::fixtures::{harness_binding, harness_capability_role};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_local_role_sql_names_the_binding_role_quoted() {
        let binding = harness_binding("app_demo");
        assert_eq!(
            set_local_role_sql(&binding).unwrap(),
            format!(
                r#"SET LOCAL ROLE "{}""#,
                binding.session_role().expect("a creator binding narrows")
            )
        );
    }

    #[test]
    fn the_role_statement_is_transaction_scoped_not_session_scoped() {
        let sql = set_local_role_sql(&harness_binding("app_demo")).unwrap();
        assert!(
            sql.starts_with("SET LOCAL ROLE "),
            "the role fence must be transaction-scoped: {sql}"
        );
    }

    /// A binding that narrows to nothing has no role statement to compose.
    ///
    /// Its control is a creator binding, which must compose one: without it
    /// this would pass for a helper that refused everything.
    #[test]
    fn a_platform_binding_composes_no_role_statement() {
        let platform = zeroship_data_orm::binding::DbBinding::platform(
            "platform",
            "fixture",
            crate::sql::SchemaName::new("zeroship").expect("fixture schema"),
        );
        assert!(set_local_role_sql(&platform).is_err());
        assert!(set_local_role_sql(&harness_binding("app_demo")).is_ok());
    }

    #[test]
    fn role_creation_attributes_exclude_platform_authority() {
        let attrs = "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT";
        assert!(attrs.contains("NOLOGIN"));
        assert!(attrs.contains("NOREPLICATION"));
        assert!(attrs.contains("NOCREATEDB"));
        assert!(attrs.contains("NOCREATEROLE"));
        assert!(!attrs.contains(" REPLICATION"));
    }

    /// The binding role and the capability role are different objects.
    ///
    /// The fence rests on that: the capability role carries the privileges and
    /// the binding role inherits exactly one of them, so a ladder that composed
    /// one name for both would have nothing to revoke.
    #[test]
    fn the_binding_role_is_not_the_capability_role() {
        let binding = harness_binding("app_demo");
        assert_ne!(
            binding.session_role().expect("narrows"),
            harness_capability_role(&binding)
        );
    }
}

#[cfg(test)]
mod live_role_grant_tests {
    use super::*;
    use compio_postgres::{Client, NoTls, Pool};

    use crate::backend::pg_session_sql::tx_session_setup_sql;
    use zeroship_data_orm::binding::DbBinding;

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

    fn table_ref(binding: &DbBinding, table: &str) -> String {
        format!(
            "{}.{}",
            crate::sql::mapping::quote_ident(binding.schema().as_str()),
            crate::sql::mapping::quote_ident(table)
        )
    }

    async fn create_tables(admin: &Client, binding: &DbBinding, tables: &[&str]) {
        for table in tables {
            admin
                .batch_execute(&format!(
                    "CREATE TABLE {} (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
                    table_ref(binding, table)
                ))
                .await
                .expect("create scratch app table");
        }
    }

    async fn teardown(admin: &Client, binding: &DbBinding) {
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                crate::sql::mapping::quote_ident(binding.schema().as_str())
            ))
            .await;
        for role in [
            binding.session_role().expect("narrows").to_owned(),
            harness_capability_role(binding),
        ] {
            let quoted = crate::sql::mapping::quote_ident(&role);
            let _ = admin
                .batch_execute(&format!("DROP OWNED BY {quoted} CASCADE"))
                .await;
            let _ = admin
                .batch_execute(&format!("DROP ROLE IF EXISTS {quoted}"))
                .await;
        }
    }

    /// Run one statement the way the data plane does: the binding's own setup
    /// batch first, inside an explicit transaction that reverts it.
    async fn as_binding_role(
        admin: &Client,
        binding: &DbBinding,
        sql: &str,
    ) -> Result<(), compio_postgres::Error> {
        admin.batch_execute("BEGIN").await?;
        let scoped = async {
            admin
                .batch_execute(
                    &tx_session_setup_sql(
                        binding,
                        crate::connection::SessionAuthority::PerBindingRole,
                    )
                    .expect("the binding composes its setup batch"),
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

    async fn assert_runtime_dml(admin: &Client, binding: &DbBinding, table: &str) {
        let relation = table_ref(binding, table);
        for sql in [
            format!("INSERT INTO {relation} (name) VALUES ('created')"),
            format!("UPDATE {relation} SET name = 'updated' WHERE name = 'created'"),
            format!("SELECT name FROM {relation}"),
            format!("DELETE FROM {relation} WHERE name = 'updated'"),
        ] {
            as_binding_role(admin, binding, &sql)
                .await
                .unwrap_or_else(|error| panic!("runtime DML failed for {relation}: {error:?}"));
        }
    }

    async fn assert_ordinary_dml_grants(
        admin: &Client,
        binding: &DbBinding,
        role: &str,
        table: &str,
    ) {
        let privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 AND table_schema = $2 AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role, binding.schema().as_str(), table],
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
            table_ref(binding, table)
        );
    }

    #[compio::test]
    async fn every_bound_schema_table_receives_runtime_dml() {
        let (postgres, admin) = admin_client().await;
        let binding = harness_binding(&scratch_app("grants"));
        teardown(&admin, &binding).await;

        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for role provisioning");
        ensure_binding_ladder(&pool, &binding)
            .await
            .expect("provision the binding ladder");
        create_tables(&admin, &binding, TABLES).await;

        for table in TABLES {
            assert_runtime_dml(&admin, &binding, table).await;
        }

        let future_table = "created_after_provisioning";
        create_tables(&admin, &binding, &[future_table]).await;
        assert_runtime_dml(&admin, &binding, future_table).await;

        let role = harness_capability_role(&binding);
        let catalog_tables = admin
            .query_text_params(
                "SELECT table_name FROM information_schema.tables \
                  WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
                  ORDER BY table_name",
                &[binding.schema().as_str()],
            )
            .await
            .expect("enumerate bound-schema tables")
            .into_iter()
            .map(|row| row.get::<_, String>("table_name"))
            .collect::<Vec<_>>();
        assert!(!catalog_tables.is_empty());
        for table in catalog_tables {
            assert_ordinary_dml_grants(&admin, &binding, &role, &table).await;
        }

        teardown(&admin, &binding).await;
    }

    #[compio::test]
    async fn a_binding_keeps_ddl_and_a_neighbours_schema_out_of_reach() {
        let (postgres, admin) = admin_client().await;
        let binding = harness_binding(&scratch_app("owner"));
        let neighbour = harness_binding(&scratch_app("neighbour"));
        teardown(&admin, &binding).await;
        teardown(&admin, &neighbour).await;

        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for role provisioning");
        for ladder in [&binding, &neighbour] {
            ensure_binding_ladder(&pool, ladder)
                .await
                .expect("provision the binding ladder");
        }
        create_tables(&admin, &binding, &["widgets"]).await;
        create_tables(&admin, &neighbour, &["secrets"]).await;

        // Control: the permitted case succeeds under the same narrowing, so the
        // refusals below are about WHAT was asked rather than about a ladder
        // that granted nothing.
        assert_runtime_dml(&admin, &binding, "widgets").await;

        for (operation, sql) in [
            (
                "CREATE TABLE",
                format!(
                    "CREATE TABLE {} (id BIGINT)",
                    table_ref(&binding, "forbidden")
                ),
            ),
            (
                "ALTER TABLE",
                format!(
                    "ALTER TABLE {} ADD COLUMN extra TEXT",
                    table_ref(&binding, "widgets")
                ),
            ),
            (
                "DROP TABLE",
                format!("DROP TABLE {}", table_ref(&binding, "widgets")),
            ),
            (
                "TRUNCATE",
                format!("TRUNCATE {}", table_ref(&binding, "widgets")),
            ),
            (
                "neighbour SELECT",
                format!("SELECT * FROM {}", table_ref(&neighbour, "secrets")),
            ),
        ] {
            let error = match as_binding_role(&admin, &binding, &sql).await {
                Ok(()) => panic!("the binding role unexpectedly performed {operation}"),
                Err(error) => error,
            };
            assert!(
                denied(&error),
                "expected insufficient privilege for {operation}, got {error:?}"
            );
        }

        teardown(&admin, &binding).await;
        teardown(&admin, &neighbour).await;
    }

    #[compio::test]
    async fn ladder_lifecycle_is_idempotent_and_transaction_scoped() {
        let (postgres, admin) = admin_client().await;
        let binding = harness_binding(&scratch_app("lifecycle"));
        teardown(&admin, &binding).await;

        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for role provisioning");
        let first = ensure_binding_ladder(&pool, &binding)
            .await
            .expect("create the binding ladder");
        let second = ensure_binding_ladder(&pool, &binding)
            .await
            .expect("re-provision the binding ladder");
        assert!(first.created_role);
        assert!(!second.created_role);
        create_tables(&admin, &binding, &["widgets"]).await;

        let session_user: String = admin
            .query_one_scalar("SELECT current_user", &[])
            .await
            .expect("read admin identity");
        as_binding_role(&admin, &binding, "SELECT 1")
            .await
            .expect("run a transaction narrowed to the binding role");
        let restored_user: String = admin
            .query_one_scalar("SELECT current_user", &[])
            .await
            .expect("read identity after commit");
        assert_eq!(restored_user, session_user);

        admin
            .batch_execute(&format!(
                "DROP SCHEMA {} CASCADE",
                crate::sql::mapping::quote_ident(binding.schema().as_str())
            ))
            .await
            .expect("drop scratch app schema");
        drop_binding_ladder(&pool, &binding)
            .await
            .expect("drop the binding ladder");
        let role = binding.session_role().expect("narrows");
        let remaining = admin
            .query_text_params("SELECT 1 FROM pg_roles WHERE rolname = $1", &[role])
            .await
            .expect("query dropped role");
        assert!(remaining.is_empty());
    }
}
