use crate::tests::fixtures::roles::*;
use zeroship_core::database_role::per_app_role_name;
#[cfg(test)]
mod reserved_table_revoke_tests {
    use super::*;

    /// The exemption is only meaningful if the table WOULD otherwise be swept.
    /// If the audit table were ever renamed out from under the reserved prefix
    /// this test says so, rather than leaving a dead exclusion behind.
    #[test]
    fn the_exempted_table_is_one_the_prefix_would_match() {
        assert!(
            WORKER_WRITABLE_RESERVED_TABLE.starts_with(RESERVED_SYSTEM_TABLE_PREFIX),
            "the exemption is dead unless the prefix matches the name"
        );
    }

    /// THE REGRESSION. Without the `relname <> ...` predicate the sweep strips
    /// the runtime role's INSERT on the audit table, and every `unmask()` dies
    /// with `permission denied for table __zeroship_audit_unmask` — because the
    /// worker stopped creating (and therefore owning) that table on 2026-08-28
    /// and is now an ordinary grantee.
    #[test]
    fn the_sweep_exempts_the_unmask_audit_table() {
        let sql = revoke_reserved_system_table_privileges_sql("app_x", "app_x_role");
        assert!(
            sql.contains("c.relname <> '__zeroship_audit_unmask'"),
            "the unmask audit table must be excluded from the sweep: {sql}"
        );
    }

    /// The sweep must still cover everything else under the prefix — above all
    /// the migration journal, which the worker must never be able to forge.
    #[test]
    fn the_sweep_still_matches_the_reserved_prefix() {
        let sql = revoke_reserved_system_table_privileges_sql("app_x", "app_x_role");
        assert!(
            sql.contains("left(c.relname, 11) = '__zeroship_'"),
            "the reserved-prefix predicate must survive the exemption: {sql}"
        );
        assert!(sql.contains("REVOKE ALL PRIVILEGES ON TABLE"), "{sql}");
    }
}

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

    /// The emitted statement must stay LOCAL.
    ///
    /// `SET ROLE` and `SET LOCAL ROLE` differ by one word and by whether the
    /// change survives the transaction. The session form leaves a pooled
    /// connection wearing the constrained role if the statement is cancelled
    /// before its `RESET`, so dropping `LOCAL` here would reintroduce the
    /// cancellation-unsafe pair deleted above - and every other assertion in
    /// this file would still pass.
    #[test]
    fn the_role_statement_is_transaction_scoped_not_session_scoped() {
        let sql = set_local_role_sql("app_demo").unwrap();
        assert!(
            sql.starts_with("SET LOCAL ROLE "),
            "the role fence must be transaction-scoped: {sql}",
        );
    }

    #[test]
    fn set_role_sql_doubles_embedded_quote_via_quote_ident() {
        // DB-15: the role name is the single statement enforcing per-tenant
        // role separation. It MUST flow through quote_ident (doubling any
        // embedded `"`), not a hand-written `"{}"` splice — even though app_id
        // is validated upstream, this boundary must not rely on that.
        assert_eq!(
            set_local_role_sql(r#"a"b"#).unwrap(),
            r#"SET LOCAL ROLE "app_a""b_role""#
        );
    }

    // The three session-setup SQL tests moved to
    // `crate::backend::pg_session_sql` with the functions they cover.

    #[test]
    fn create_role_attrs_assert_noreplication() {
        // §17.5 NON-NEGOTIABLE: the per-app CREATE ROLE attribute string
        // MUST contain NOREPLICATION. This is a source-level guard so a
        // future edit that drops the attribute (relying on the server
        // default) fails the test — the default is overridable per
        // cluster (`rolreplication` inheritance is subtle), so we assert
        // it explicitly. The literal lives in `ensure_per_app_role`;
        // mirror it here.
        let attrs = format!(
            "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"{APP_ROLE_TEMPLATE}\""
        );
        assert!(
            attrs.contains("NOREPLICATION"),
            "per-app role MUST be NOREPLICATION (§17.5 slot-ownership-stays-platform)"
        );
        assert!(
            !attrs.contains(" REPLICATION"),
            "per-app role MUST NOT carry the REPLICATION attribute"
        );
    }

    #[test]
    fn app_role_template_constant_uses_proposal_naming() {
        assert_eq!(APP_ROLE_TEMPLATE, "__zeroship_app_role_template");
    }
}

#[cfg(test)]
mod live_reserved_sweep_tests {
    use super::*;
    use compio_postgres::{Client, NoTls};

    // Named explicitly rather than reached through `super::*`: this module used
    // the re-export deleted above, and `cargo check --all-targets` did not
    // notice, because that command skips a target whose feature is off.
    use crate::backend::pg_session_sql::tx_session_setup_sql;

    async fn admin_client() -> (crate::tests::fixtures::postgres::Postgres, Client) {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let (client, conn) = compio_postgres::connect(&postgres.url(), NoTls)
            .await
            .expect("connect to the plugin-db test database");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        (postgres, client)
    }

    /// A scratch app id with a unique prefix - this server is shared.
    fn scratch_app() -> String {
        format!("zsaudit_{}", uuid::Uuid::new_v4().simple())
    }

    fn audit_ref(app: &str) -> String {
        format!(
            "{}.\"__zeroship_audit_unmask\"",
            crate::compile::quote_ident(app)
        )
    }

    fn journal_ref(app: &str) -> String {
        format!(
            "{}.\"__zeroship_schema_migrations\"",
            crate::compile::quote_ident(app)
        )
    }

    /// The eight columns `crud/unmask.rs`'s PG arm binds, with `id` and `ts`
    /// left to their defaults.
    ///
    /// A COPY of that column list, not the production statement - the producer
    /// is a private `async fn` that resolves its backend out of the isolate
    /// context, which this module has no reason to stand up. So this case cannot
    /// catch the column list drifting apart from the DDL; the live `unmask()`
    /// cases in `crates/zeroship-data-orm/src/tests/postgres/mod.rs` are what covers that. What it IS here to
    /// catch is the privilege, which those cases run as the owning superuser and
    /// therefore cannot see.
    fn audit_insert_sql(app: &str) -> String {
        format!(
            "INSERT INTO {} (actor_id, actor_role, collection, row_pk, \"column\", \
             classification, reason, outcome) \
             VALUES ('usr_1', 'admin', 'users', '1', 'ssn', 'pii', 'support', 'granted')",
            audit_ref(app)
        )
    }

    /// Provision one scratch app the way production does: the migration service
    /// (an ADMIN principal, not the worker) creates the reserved tables, then
    /// the per-app role is provisioned over them.
    ///
    /// `provision_audit_unmask_table` is `zeroship-migrate-server`'s production
    /// entry point, reached through the dev-dependency this crate already
    /// declares for exactly this reason - a fixture holding its own `CREATE
    /// TABLE` would prove the shape of the fixture.
    ///
    /// The journal table beside it is NOT from a production generator: the
    /// engine bootstraps `__zeroship_schema_migrations` from inside an apply,
    /// which is far more machinery than this case needs. It stands in for "any
    /// other reserved-prefix table", and all that is asserted about it is that
    /// the sweep still strips it.
    async fn provision_scratch_app(admin: &Client, app: &str) -> String {
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                crate::compile::quote_ident(app)
            ))
            .await
            .expect("create scratch app schema");
        zeroship_migrate_server::provisioning::provision_audit_unmask_table(admin, app)
            .await
            .expect("provision the unmask audit table as the migration service does");
        admin
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {} (id BIGSERIAL PRIMARY KEY, name TEXT); \
                 CREATE TABLE IF NOT EXISTS {}.\"widgets\" (id BIGSERIAL PRIMARY KEY);",
                journal_ref(app),
                crate::compile::quote_ident(app),
            ))
            .await
            .expect("seed the swept journal stand-in and a creator table");
        per_app_role_name(app).expect("scratch app role name")
    }

    /// Named-object teardown only. `__zeroship_app_role_template` is
    /// deliberately left alone: `ensure_per_app_role` creates it idempotently,
    /// it is shared by every app on the cluster, and dropping it would break
    /// concurrent work on this server.
    async fn teardown(admin: &Client, app: &str) {
        let role =
            crate::compile::quote_ident(&per_app_role_name(app).expect("scratch app role name"));
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                crate::compile::quote_ident(app)
            ))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP OWNED BY {role} CASCADE"))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP ROLE IF EXISTS {role}"))
            .await;
    }

    /// Run `sql` with the connection's role narrowed to the app's runtime role
    /// exactly the way the data plane narrows it - `tx_session_setup_sql`, the
    /// production statement, inside the transaction whose COMMIT/ROLLBACK is
    /// what reverts it.
    async fn as_runtime_role(
        admin: &Client,
        app: &str,
        sql: &str,
    ) -> Result<(), compio_postgres::Error> {
        // `tx_session_setup_sql` takes the typed schema name, so the scratch app
        // id is parsed rather than passed as text. Mirrors
        // `zeroship-migrate-server`'s `scratch_schema_name`, which took the same
        // signature change.
        let schema = zeroship_data_sql::SchemaName::new(app).expect("a scratch app id is a schema");
        admin.batch_execute("BEGIN").await?;
        let scoped = async {
            admin
                .batch_execute(&tx_session_setup_sql(&schema).expect("scratch app role name"))
                .await?;
            admin.batch_execute(sql).await
        }
        .await;
        let _ = admin
            .batch_execute(if scoped.is_ok() { "COMMIT" } else { "ROLLBACK" })
            .await;
        scoped
    }

    fn denied(err: &compio_postgres::Error) -> bool {
        err.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    }

    async fn assert_audit_verb_refused(what: &str, statement: impl FnOnce(&str) -> String) {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        provision_scratch_app(&admin, &app).await;

        let pool = compio_postgres::Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role (this is what runs the sweep)");

        let outcome = as_runtime_role(&admin, &app, &statement(&app)).await;
        teardown(&admin, &app).await;

        let err = outcome.expect_err(&format!(
            "the runtime role must not be able to {what} the unmask audit table"
        ));
        assert!(
            denied(&err),
            "expected insufficient_privilege for audit {what}, got {:?}: {err}",
            err.code()
        );
    }

    #[compio::test]
    async fn a_wrong_kind_audit_relation_keeps_no_runtime_privileges() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA {}; \
                 CREATE VIEW {} AS SELECT 1::bigint AS id",
                crate::compile::quote_ident(&app),
                audit_ref(&app),
            ))
            .await
            .expect("create a wrong-kind relation at the reserved audit name");

        let pool = compio_postgres::Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role over the wrong-kind relation");
        let role = per_app_role_name(&app).expect("scratch app role name");
        let privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 \
                    AND table_schema = $2 \
                    AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role.as_str(), app.as_str(), WORKER_WRITABLE_RESERVED_TABLE],
            )
            .await
            .expect("query wrong-kind audit privileges")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        teardown(&admin, &app).await;

        assert!(
            privileges.is_empty(),
            "a non-table at the audit name must keep no worker privileges: {privileges:?}"
        );
    }

    #[compio::test]
    async fn a_malformed_audit_table_fails_closed_without_runtime_privileges() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA {}; \
                 CREATE TABLE {} (id BIGINT PRIMARY KEY)",
                crate::compile::quote_ident(&app),
                audit_ref(&app),
            ))
            .await
            .expect("create an audit table without its required serial sequence");

        let pool = compio_postgres::Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        let outcome = ensure_per_app_role(&pool, &app).await;
        let role = per_app_role_name(&app).expect("scratch app role name");
        let privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 \
                    AND table_schema = $2 \
                    AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role.as_str(), app.as_str(), WORKER_WRITABLE_RESERVED_TABLE],
            )
            .await
            .expect("query malformed audit table privileges")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        teardown(&admin, &app).await;

        assert!(
            outcome.is_err(),
            "a malformed audit table must be rejected after its privileges are denied"
        );
        assert!(
            privileges.is_empty(),
            "a malformed audit table must keep no worker privileges: {privileges:?}"
        );
    }

    /// THE ONE THAT MATTERS. The worker must still be able to append to its own
    /// audit log after the reserved-prefix sweep has run over the schema.
    ///
    /// Positive half: a real `INSERT`, as the real runtime role, lands a row.
    /// Negative half: the same role cannot `TRUNCATE` or `DROP` that table, and
    /// cannot write the journal beside it. Losing ownership was claimed as a
    /// security GAIN; a test that only proved the append still works would have
    /// measured the loss and not the gain.
    #[compio::test]
    async fn the_runtime_role_can_append_to_the_audit_log_after_the_sweep() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        let role = provision_scratch_app(&admin, &app).await;

        let pool = compio_postgres::Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role (this is what runs the sweep)");

        // The table is NOT owned by the role that writes it - the premise the
        // exemption exists for. While the worker created it, this assertion
        // would have been false and the sweep would have been a no-op.
        let owner: String = admin
            .query_one_scalar(
                "SELECT tableowner FROM pg_tables \
                  WHERE schemaname = $1 AND tablename = '__zeroship_audit_unmask'",
                &[&app],
            )
            .await
            .expect("read audit table owner");
        assert_ne!(owner, role, "the worker must not own its own audit log");

        let table_privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 \
                    AND table_schema = $2 \
                    AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role.as_str(), app.as_str(), WORKER_WRITABLE_RESERVED_TABLE],
            )
            .await
            .expect("query the audit table's exact grants")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        println!("information_schema.table_privileges for {role}: {table_privileges:?}");
        assert_eq!(
            table_privileges,
            vec!["INSERT"],
            "the runtime role must hold exactly INSERT on its audit table"
        );

        let audit = audit_ref(&app);
        let sequence_privileges = admin
            .query_text_params(
                "SELECT \
                    has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'USAGE') AS usage, \
                    has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'SELECT') AS sel, \
                    has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'UPDATE') AS upd",
                &[role.as_str(), audit.as_str()],
            )
            .await
            .expect("query the audit serial sequence's exact grants");
        let sequence_privileges = &sequence_privileges[0];
        assert!(
            sequence_privileges.get::<_, bool>("usage"),
            "the audit serial sequence needs USAGE for nextval"
        );
        assert!(
            !sequence_privileges.get::<_, bool>("sel"),
            "the audit serial sequence must not grant SELECT"
        );
        assert!(
            !sequence_privileges.get::<_, bool>("upd"),
            "the audit serial sequence must not grant UPDATE"
        );

        // POSITIVE HALF - a real INSERT, as the real role.
        as_runtime_role(&admin, &app, &audit_insert_sql(&app))
            .await
            .expect("the runtime role must be able to append an unmask audit row");
        let rows: i64 = admin
            .query_one_scalar(&format!("SELECT count(*) FROM {}", audit_ref(&app)), &[])
            .await
            .expect("count audit rows");
        assert_eq!(rows, 1, "the append must actually have landed a row");

        // NEGATIVE HALF - the reach it must NOT have.
        for (what, sql) in [
            ("TRUNCATE", format!("TRUNCATE {}", audit_ref(&app))),
            ("DROP", format!("DROP TABLE {}", audit_ref(&app))),
            (
                "forge a journal row",
                format!("INSERT INTO {} (name) VALUES ('forged')", journal_ref(&app)),
            ),
        ] {
            let err = as_runtime_role(&admin, &app, &sql)
                .await
                .expect_err(&format!("the runtime role must not be able to {what}"));
            assert!(
                err.as_db_error().is_some(),
                "{what} must be refused by the server, not by the client: {err}"
            );
        }
        // Only the appended row survives all of that.
        let rows: i64 = admin
            .query_one_scalar(&format!("SELECT count(*) FROM {}", audit_ref(&app)), &[])
            .await
            .expect("re-count audit rows");
        assert_eq!(rows, 1, "no denied statement may have changed the log");

        teardown(&admin, &app).await;
    }

    #[compio::test]
    async fn the_runtime_role_cannot_select_the_unmask_audit_log() {
        assert_audit_verb_refused("SELECT", |app| {
            format!("SELECT 1 FROM {} LIMIT 0", audit_ref(app))
        })
        .await;
    }

    #[compio::test]
    async fn the_runtime_role_cannot_update_the_unmask_audit_log() {
        assert_audit_verb_refused("UPDATE", |app| {
            format!("UPDATE {} SET reason = NULL WHERE false", audit_ref(app))
        })
        .await;
    }

    #[compio::test]
    async fn the_runtime_role_cannot_delete_from_the_unmask_audit_log() {
        assert_audit_verb_refused("DELETE FROM", |app| {
            format!("DELETE FROM {} WHERE false", audit_ref(app))
        })
        .await;
    }

    /// THE CONTROL, differing in exactly one variable: the exemption clause.
    ///
    /// Same production provisioning, same production sweep statement - with the
    /// `AND c.relname <> '__zeroship_audit_unmask'` predicate deleted from the
    /// generated text. The INSERT above must now be refused with
    /// `insufficient_privilege`, which is the exact failure the exemption's
    /// docstring predicts. Without this arm the case above would pass whether or
    /// not the exemption did anything.
    ///
    /// The clause is rebuilt from the two constants rather than typed out, so a
    /// rename that made the strip a no-op fails the `assert_ne!` instead of
    /// quietly turning this control into a duplicate of the case above.
    #[compio::test]
    async fn without_the_exemption_the_sweep_takes_the_runtime_role_insert() {
        let (postgres, admin) = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        let role = provision_scratch_app(&admin, &app).await;

        let pool = compio_postgres::Pool::connect(&postgres.url(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role");
        admin
            .batch_execute(&format!(
                "GRANT INSERT ON TABLE {}.\"widgets\" TO {}",
                crate::compile::quote_ident(&app),
                crate::compile::quote_ident(&role),
            ))
            .await
            .expect("give the control table one explicit privilege");

        let shipped = revoke_reserved_system_table_privileges_sql(&app, &role);
        let exemption = format!(
            "AND c.relname <> {} ",
            sql_string_literal(WORKER_WRITABLE_RESERVED_TABLE)
        );
        let unexempted = shipped.replace(&exemption, "");
        assert_ne!(
            shipped, unexempted,
            "the exemption clause {exemption:?} was not found in the shipped sweep, \
             so this control would have re-run the shipped statement and proved \
             nothing:\n{shipped}"
        );

        admin
            .batch_execute(&unexempted)
            .await
            .expect("run the sweep without its exemption");

        let err = as_runtime_role(&admin, &app, &audit_insert_sql(&app))
            .await
            .expect_err("without the exemption the append MUST be refused");
        assert!(
            denied(&err),
            "expected insufficient_privilege, got {:?}: {err}",
            err.code()
        );
        // `compio_postgres::Error`'s Display is the bare string "db error"; the
        // server's text is on the wrapped `DbError`, which is where the
        // docstring's predicted `permission denied for table
        // __zeroship_audit_unmask` actually appears.
        let message = err
            .as_db_error()
            .map(|db| db.message().to_string())
            .unwrap_or_default();
        assert!(
            message.contains("__zeroship_audit_unmask"),
            "the refusal must name the audit table; got {message:?}"
        );

        // And the swept privilege is exactly the one the exemption protects:
        // the creator table beside it is untouched, so the sweep did not simply
        // strip everything.
        let creator_insert: bool = admin
            .query_one_scalar(
                &format!(
                    "SELECT has_table_privilege('{role}', '{}.\"widgets\"', 'INSERT')",
                    crate::compile::quote_ident(&app)
                ),
                &[],
            )
            .await
            .expect("probe creator-table privilege");
        assert!(
            creator_insert,
            "the sweep must leave ordinary creator tables alone"
        );

        teardown(&admin, &app).await;
    }
}
