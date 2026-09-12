//! SQLite schema contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;
use crate::tests::fixtures::schema::{fixture_table_sql, fixture_table_sql_for};

use zeroship_migrate::schema::query::FkEmission;

use zeroship_data_orm::protection::Catalog;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

#[test]
fn p55_pr1_build_create_table_refuses_masked_suffix_field_sqlite() {
    Host::test(|_| {
        let schema = crate::value!({
            "name": {"type": "string"},
            // `_masked` is reserved for Path B sibling columns.
            "card_pan_masked": {"type": "string"},
        });
        let result = fixture_table_sql(
            &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "cards",
            &schema,
            &FkEmission::Inline,
        );
        let err = result.expect_err("schema with `_masked` suffix should be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("reserved field name") && msg.contains("_masked"),
            "expected reserved-suffix message, got: {msg}"
        );
    })
}

#[test]
fn p55_pr1_build_create_table_refuses_classification_name_field_sqlite() {
    Host::test(|_| {
        let schema = crate::value!({
            "name": {"type": "string"},
            // `phi` collides with the platform classification taxonomy.
            "phi": {"type": "string"},
        });
        let result = fixture_table_sql(
            &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "patients",
            &schema,
            &FkEmission::Inline,
        );
        let err = result.expect_err("schema with reserved classification name should be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("reserved field name"),
            "expected reserved-name message, got: {msg}"
        );
    })
}

/// `fixture_table_sql_for(Sqlite)` produces DDL the
/// SQLite engine accepts, and PRAGMA `table_info` reports all 7 system
/// fields after execution.
#[test]
fn sqlite_ddl_has_seven_assigned_field_columns_end_to_end() {
    Host::test(|host| {
        use crate::sql::compile::SqlDialect;

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            let schema = crate::value!({
                "title": { "type": "string", "required": true },
            });
            let sql = fixture_table_sql_for(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build sqlite DDL");

            // Execute the multi-statement payload through the session
            // actor — `execute_fixture` routes through `sqlite3_exec` which
            // accepts multi-statement SQL.
            // SQLite's `Connection::execute` runs ONE statement per call
            // (unlike PG's libpq simple-query), so this test splits the
            // multi-statement payload and executes each piece individually.
            for stmt in sql.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .unwrap_or_else(|e| panic!("engine must accept statement: {trimmed}\n{e:?}"));
            }

            // Confirm via introspection: all 7 system field columns
            // present, plus the 1 user column.
            let live = backend
                .introspect_schema("app_demo")
                .await
                .expect("introspect_schema");
            let cols = live
                .tables
                .get("posts")
                .expect("posts table must be present");
            for name in &[
                "id",
                "created_at",
                "updated_at",
                "created_by",
                "updated_by",
                "version",
                "deleted_at",
            ] {
                assert!(
                    cols.contains_key(*name),
                    "system field {name:?} missing from introspected cols: {:?}",
                    cols.keys().collect::<Vec<_>>()
                );
            }
            assert!(
                cols.contains_key("title"),
                "user-declared `title` must coexist: {:?}",
                cols.keys().collect::<Vec<_>>()
            );
        });
    })
}

/// All three auto-indexes (`deleted_at`, `updated_at`, `created_by`)
/// land in `sqlite_master` after the CREATE TABLE payload executes.
/// The `id` PK uses ROWID (no autoindex entry) and `version` is
/// intentionally unindexed (see `create_table_does_not_emit_index_for_version`).
#[test]
fn freshly_created_table_has_three_indexes_end_to_end() {
    Host::test(|host| {
        use crate::sql::compile::SqlDialect;
        use zeroship_migrate::schema::query::index_name;

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            let sql = fixture_table_sql_for(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &crate::value!({}),
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build sqlite DDL");
            // SQLite's `Connection::execute` runs ONE statement per call
            // (unlike PG's libpq simple-query), so this test splits the
            // multi-statement payload and executes each piece individually.
            for stmt in sql.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .unwrap_or_else(|e| panic!("engine must accept statement: {trimmed}\n{e:?}"));
            }

            let live = backend
                .introspect_schema("app_demo")
                .await
                .expect("introspect_schema");
            let idx_map = live
                .indexes
                .get("posts")
                .expect("posts must have an index map");

            for col in &["deleted_at", "updated_at", "created_by"] {
                let expected = index_name("posts", &[col], /* unique = */ false);
                assert!(
                    idx_map.contains_key(&expected),
                    "expected auto-index {expected} for column {col}; have: {:?}",
                    idx_map.keys().collect::<Vec<_>>()
                );
            }
        });
    })
}
