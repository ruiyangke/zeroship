//! SQLite masking contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;
use crate::tests::fixtures::schema::fixture_table_sql;

use zeroship_migrate::schema::query::FkEmission;

use crate::sql::compile::raw_column_name;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// **DDL shape on SQLite**: `build_create_table_with_fks`
/// emits the field's own column as a bare `TEXT` mask holder (carrying the
/// `zero-migrate:mask:...` sentinel) plus a RAW sibling - named via
/// [`raw_column_name`], never spelled out here - that carries the declared
/// type and constraints. The SQLite arm receives the SQL byte-identical to
/// PG for this schema (no encryption, so no dialect-specific BYTEA/BLOB
/// split); the SQLite engine accepts it once executed through the
/// SQLite-flavoured `CREATE TABLE` path.
#[test]
fn a_raw_column_is_emitted_for_a_masked_field_sqlite() {
    Host::test(|_| {
        let schema = crate::value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let sql = fixture_table_sql(
            &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &schema,
            &FkEmission::Inline,
        )
        .unwrap();
        let raw_ssn = raw_column_name("ssn");
        assert!(
            sql.contains(&format!("\"{raw_ssn}\" TEXT")),
            "raw column must be emitted to carry the real value: {sql}"
        );
        assert!(
            sql.contains("\"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */"),
            "the field's own column must be the masked sibling and carry the mask sentinel: {sql}"
        );
        // The sentinel rides the masked column only - the raw column is not
        // itself masked (it holds the real value), so it must carry no mask
        // metadata.
        assert!(
            !sql.contains(&format!("\"{raw_ssn}\" TEXT /* __zsmask")),
            "the raw column must not carry the mask sentinel: {sql}"
        );
        assert!(
            !sql.contains("\"name_masked\""),
            "non-masked column must not emit a sibling: {sql}"
        );
        let raw_name = raw_column_name("name");
        assert!(
            !sql.contains(&format!("\"{raw_name}\"")),
            "non-masked column must not emit a raw sibling: {sql}"
        );
    })
}

/// **Atomic dual-write on SQLite**: when a row carries
/// both the parent + sibling (mask pass already ran), the
/// SQLite-flavoured `build_insert_with_dialect` INSERT statement
/// includes both columns atomically. Then we execute the INSERT
/// against a hand-rolled SQLite-shaped table to confirm the engine
/// accepts the dual write end-to-end and persists the masked value
/// alongside the plaintext.
#[test]
fn dual_write_insert_persists_parent_and_sibling_sqlite() {
    Host::test(|host| {
        use crate::sql::compile::{SqlDialect, build_insert_with_dialect};

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            // Hand-rolled SQLite-flavoured CREATE TABLE — the SQLite
            // CREATE TABLE dialect doesn't speak PG's SERIAL /
            // TIMESTAMPTZ; the orchestrator emits SQLite-flavoured DDL
            // elsewhere. The sibling-column CLAUSE emission is standard
            // SQL; we exercise it inside a SQLite-valid table here.
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"users\" (\
                     id    INTEGER PRIMARY KEY, \
                     ssn   TEXT, \
                     ssn_masked TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE ok");

            // Simulate the dispatch_insert → apply_mask_on_write step:
            // the mask pass has populated `ssn_masked`. The SQL builder
            // walks the row map, so the sibling key naturally lands on
            // the INSERT column list (no special-casing needed).
            let doc = crate::value!({
                "ssn": "123-45-6789",
                "ssn_masked": "***-**-6789"
            });
            // The descriptor entry for the fixture table above. It declares `ssn`
            // only: `ssn_masked` is a PHYSICAL column the mask pass writes, never a
            // declared field, so it is on the INSERT column list and not on the
            // projection - which is the shape this test is about.
            let schema = crate::value!({ "ssn": { "type": "string" } });
            let bq = build_insert_with_dialect(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            assert!(
                bq.sql.contains("\"ssn\"") && bq.sql.contains("\"ssn_masked\""),
                "INSERT must reference both parent + sibling: {}",
                bq.sql,
            );

            let param_refs = &bq.params;
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let _ = client
                .query_values(&bq.sql, param_refs)
                .await
                .expect("dual-write INSERT must succeed");

            // Verify both columns landed atomically.
            let rows = client
                .query("SELECT ssn, ssn_masked FROM \"app_demo\".\"users\"", &[])
                .await
                .expect("SELECT both columns");
            assert_eq!(rows.len(), 1, "exactly one row inserted");
            assert_eq!(rows[0][0].as_deref(), Some("123-45-6789"));
            assert_eq!(rows[0][1].as_deref(), Some("***-**-6789"));
        });
    })
}

/// **A default SELECT serves the masked column**: a default
/// read against a masked-column DDL must name the field's own column
/// directly - no AS-rewrite; the sibling-alias scheme is gone since the
/// storage flip - and must NEVER reference the raw column. Since the
/// field's own column is now where a dual-write leaves the mask, a
/// schema-blind SELECT already reads the mask with no special casing.
/// End-to-end gate: drive a dual-write through the dialect-aware INSERT
/// builder (mirroring what `mask_pass::relocate_masked_columns`
/// produces), then build a `find` SQL via `build_find_with_schema` with
/// the cached schema, run it through the SQLite session, and assert the
/// engine returns the masked string under the field's own column - and
/// that the real value is nowhere in the row.
#[test]
fn a_select_serves_the_masked_column_sqlite() {
    Host::test(|host| {
        use crate::sql::compile::{
            SqlDialect, build_find_with_schema, build_insert_with_dialect,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            let raw_ssn = raw_column_name("ssn");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"app_demo\".\"users\" (\
                         id    TEXT PRIMARY KEY, \
                         \"{raw_ssn}\" TEXT, \
                         ssn   TEXT NOT NULL, \
                         name  TEXT\
                     )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE ok");

            // Dual-write a row the way `mask_pass::relocate_masked_columns`
            // produces one: the raw column stores the real value (plaintext
            // here - masking + encryption are orthogonal in
            // `apply_mask_on_write` design), the field's own column stores
            // the masked string.
            let schema = crate::value!({
                "ssn": {
                    "type": "string",
                    "mask": { "kind": "last4", "classification": "spi" }
                },
                "name": { "type": "string" }
            });
            let mut doc = crate::value!({
                "id": "usr_01",
                "ssn": "***-**-6789",
                "name": "alice"
            });
            doc.as_object_mut()
                .expect("doc object")
                .insert(raw_ssn.clone(), crate::value!("123-45-6789"));
            let bq = build_insert_with_dialect(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .expect("build_insert_with_dialect");
            let param_refs = &bq.params;
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            client
                .query_values(&bq.sql, param_refs)
                .await
                .expect("INSERT");

            // Build a default read with schema awareness: the SELECT must
            // name the field's own column directly AND must NOT reference the
            // raw column at all. Verify the SQL shape BEFORE running the
            // query - this is the load-bearing assertion this test pins.
            let bq = build_find_with_schema(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &crate::value!({ "id": "usr_01" }),
                None,
                None,
                None,
                None,
                &schema,
            )
            .expect("build_find_with_schema");
            let select_clause = bq
                .sql
                .split(" FROM ")
                .next()
                .expect("SELECT prefix")
                .to_string();
            assert!(
                select_clause.contains("\"ssn\""),
                "SELECT must project the field's own column directly: {select_clause}"
            );
            assert!(
                !select_clause.contains("AS \"ssn\""),
                "there is no more AS-rewrite onto ssn - the aliasing scheme is gone: {select_clause}"
            );
            // The raw column (real value) must NEVER appear in a default
            // read's SELECT clause - it is unqueryable outside the audited
            // unmask path (`protection::unmask`).
            assert!(
                !select_clause.contains(raw_ssn.as_str()),
                "SELECT must never reference the raw column: {select_clause}"
            );

            // Execute the SELECT and verify the row returns the masked
            // string under the field's own column, and the real value is
            // nowhere in the row.
            let param_refs = &bq.params;
            let rows = client
                .query_values(&bq.sql, param_refs)
                .await
                .expect("SELECT");
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            assert_eq!(
                row.iter()
                    .filter_map(|c| c.as_deref())
                    .find(|s| *s == "***-**-6789"),
                Some("***-**-6789"),
                "row must include the masked string: {row:?}"
            );
            // The real value must NOT appear anywhere in the row (we never
            // selected the raw column).
            assert!(
                !row.iter().any(|c| c.as_deref() == Some("123-45-6789")),
                "the real value must not surface on a default read: {row:?}"
            );
        });
    })
}

/// **`kind: none` preserves the encryption decrypt-on-read path**:
/// when a column declares `mask: { kind: "none" }`, the SELECT clause
/// must emit the parent column directly (no AS-rewrite), and the row
/// must surface the parent's value (the ciphertext / plaintext under
/// the parent column).
#[test]
fn aliased_select_skips_kind_none_sqlite() {
    Host::test(|_| {
        use crate::sql::compile::build_find_with_schema;

        let schema = crate::value!({
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "none", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let bq = build_find_with_schema(
            &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &crate::value!({}),
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect("build_find_with_schema");
        assert!(
            !bq.sql.contains("\"ssn_masked\""),
            "kind=none must NOT trigger the AS-rewrite: {}",
            bq.sql,
        );
        // Schema-aware reads now always expand to the allowlisted public
        // column set, even when every mask is `kind: "none"`.
        assert!(
            !bq.sql.contains("SELECT *"),
            "schema-backed reads must avoid `*`: {}",
            bq.sql,
        );
        assert!(
            bq.sql.contains("SELECT \"ssn\", \"name\"")
                && bq.sql.contains("\"ssn\"")
                && bq.sql.contains("\"name\""),
            "schema-backed reads must project the public column set: {}",
            bq.sql,
        );
    })
}

/// **NOT NULL contract on the sibling**: omitting the
/// sibling from an INSERT against a masked-column DDL must fail at the
/// engine level (the sibling is `TEXT NOT NULL`). This is the
/// load-bearing assertion that mask-pass must run before the SQL
/// builder - skip it and the engine rejects with a NOT NULL violation.
#[test]
fn missing_sibling_fails_not_null_constraint_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"users\" (\
                     id  INTEGER PRIMARY KEY, \
                     ssn TEXT, \
                     ssn_masked TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE ok");
            // Insert WITHOUT the sibling. The engine must refuse.
            let res = backend
                .execute_fixture(
                    "INSERT INTO \"app_demo\".\"users\" (\"ssn\") VALUES (?)",
                    &[("plaintext-no-mask").into()],
                )
                .await;
            assert!(
                res.is_err(),
                "INSERT without sibling MUST fail (sibling is NOT NULL); got Ok"
            );
        });
    })
}
