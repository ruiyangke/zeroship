//! SQLite catalog contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use zeroship_data_orm::protection::Catalog;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

#[test]
fn estimate_row_count_missing_table_returns_zero() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            let rows = backend
                .estimate_row_count("app_demo", "missing_table")
                .await
                .expect("estimate_row_count for missing table");
            assert_eq!(rows, 0, "missing table must classify as empty");
        });
    })
}

#[test]
fn introspect_empty_schema_yields_empty_live_schema() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            let live = backend
                .introspect_schema(
                    "app_demo",
                    &crate::sql::SchemaName::new("app_demo").unwrap(),
                    None,
                )
                .await
                .expect("introspect_schema on empty namespace");
            // No user tables → empty `tables` / `indexes` / `foreign_keys`.
            // The `LiveSchema` shape uses HashMap so "empty" is `is_empty()`.
            assert!(
                live.tables.is_empty(),
                "empty namespace must yield empty `tables`; got {:?}",
                live.tables.keys().collect::<Vec<_>>()
            );
            assert!(
                live.indexes.is_empty(),
                "empty namespace must yield empty `indexes`"
            );
            assert!(
                live.foreign_keys.is_empty(),
                "empty namespace must yield empty `foreign_keys`"
            );
        });
    })
}

#[test]
fn introspect_after_create_table_round_trip() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            // Create a small table with a mix of NULL and NOT NULL
            // columns, plus a non-PK index, so the introspect output
            // exercises every PRAGMA branch.
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            backend
                .execute_fixture_on(
                    &client,
                    "CREATE TABLE \"app_demo\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL, \
                     payload TEXT\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");
            backend
                .execute_fixture_on(
                    &client,
                    "CREATE INDEX \"app_demo\".\"items_name_idx\" ON \"items\"(name)",
                    &[],
                )
                .await
                .expect("CREATE INDEX items_name_idx");

            let live = backend
                .introspect_schema(
                    "app_demo",
                    &crate::sql::SchemaName::new("app_demo").unwrap(),
                    None,
                )
                .await
                .expect("introspect_schema after CREATE TABLE");

            // Table must be observed.
            let cols = live
                .tables
                .get("items")
                .expect("items table must be present in LiveSchema");
            assert_eq!(
                cols.len(),
                3,
                "items has 3 columns, got {:?}",
                cols.keys().collect::<Vec<_>>()
            );

            // SQLite reports declared affinity names where PostgreSQL reports
            // `format_type(...)`; the shared catalog model stores either form.
            let id_col = cols.get("id").expect("id column");
            assert_eq!(id_col.pg_type, "INTEGER");
            // `id INTEGER PRIMARY KEY` is a special SQLite case — it's an
            // alias for ROWID, NOT-NULL-implicit only when the row has a
            // value. PRAGMA `table_info.notnull` returns 0 here even
            // though the column is effectively NOT NULL; we faithfully
            // report what the engine surfaces.
            assert!(
                !id_col.not_null,
                "PRAGMA table_info reports notnull=0 for INTEGER PRIMARY KEY (ROWID alias)"
            );

            let name_col = cols.get("name").expect("name column");
            assert_eq!(name_col.pg_type, "TEXT");
            assert!(name_col.not_null, "name was declared NOT NULL");

            let payload_col = cols.get("payload").expect("payload column");
            assert_eq!(payload_col.pg_type, "TEXT");
            assert!(!payload_col.not_null, "payload was declared nullable");

            // Index must be observed — the non-PK `items_name_idx` should
            // appear; the implicit `sqlite_autoindex_*` for `INTEGER
            // PRIMARY KEY` does NOT appear because INTEGER PRIMARY KEY
            // uses the ROWID and doesn't create an autoindex entry.
            let idxs = live
                .indexes
                .get("items")
                .expect("items must have an index map");
            let idx_info = idxs
                .get("items_name_idx")
                .expect("items_name_idx must be present");
            assert!(!idx_info.is_unique, "items_name_idx is non-unique");
            assert_eq!(idx_info.columns, vec!["name".to_string()]);

            // No FKs declared → no FK entries.
            assert!(
                !live.foreign_keys.contains_key("items"),
                "items has no declared FKs"
            );

            // estimate_row_count on the empty table is 0.
            let n = backend
                .estimate_row_count("app_demo", "items")
                .await
                .expect("estimate_row_count");
            assert_eq!(n, 0, "freshly-created table has 0 rows");
        });
    })
}

#[test]
fn introspection_includes_prefixed_creator_schema_tables() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("attach creator database");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"__zs_workflow_state\" (\
                     id TEXT PRIMARY KEY, \
                     \"secret\" TEXT /* zero-migrate:mask:kind=full,classification=pii */\
                     )",
                    &[],
                )
                .await
                .expect("create prefixed table");

            let live = backend
                .introspect_schema(
                    "app_demo",
                    &crate::sql::SchemaName::new("app_demo").unwrap(),
                    None,
                )
                .await
                .expect("introspect prefixed table");
            let secret = live
                .tables
                .get("__zs_workflow_state")
                .and_then(|columns| columns.get("secret"))
                .expect("prefixed table protection metadata");
            assert!(secret.mask.is_some(), "stored mask metadata was skipped");
        });
    })
}
