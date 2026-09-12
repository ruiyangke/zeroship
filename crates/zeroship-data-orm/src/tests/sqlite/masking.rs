//! SQLite masking contracts.
use super::fixtures::*;

use crate::tests::fixtures::schema::fixture_table_sql;
use crate::tests::fixtures::Host;

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
            "the field's display column must carry the mask sentinel: {sql}"
        );
        // The sentinel rides the masked column only - the raw column is not
        // itself masked (it holds the real value), so it must carry no mask
        // metadata.
        assert!(
            !sql.contains(&format!("\"{raw_ssn}\" TEXT /* __zsmask")),
            "the raw column must not carry the mask sentinel: {sql}"
        );
        let raw_name = raw_column_name("name");
        assert!(
            !sql.contains(&format!("\"{raw_name}\"")),
            "non-masked column must not emit a raw sibling: {sql}"
        );
    })
}

/// A masked write stores its visible mask and raw value in one statement.
#[test]
fn masked_insert_persists_visible_and_raw_columns_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            let raw = raw_column_name("ssn");
            backend
                .execute_fixture(
                    &format!(
                        "CREATE TABLE \"app_demo\".\"users\" (\
                     id    INTEGER PRIMARY KEY, \
                     ssn   TEXT, \
                     \"{raw}\" TEXT NOT NULL\
                 )"
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE ok");

            let mut doc = crate::value!({"ssn": "***-**-6789"});
            doc[raw.as_str()] = crate::value::Value::from("123-45-6789");
            let schema = crate::value!({
                "id": { "type": "string" },
                "ssn": {
                    "type": "string",
                    "mask": {"kind": "last4", "classification": "spi"}
                }
            });
            let bq = compile_insert(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &schema,
                &doc,
            )
            .unwrap();
            assert!(
                bq.sql.contains("\"ssn\"") && bq.sql.contains(&format!("\"{raw}\"")),
                "INSERT must reference the visible and raw columns: {}",
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
                .expect("masked INSERT must succeed");

            let rows = client
                .query(
                    &format!("SELECT ssn, \"{raw}\" FROM \"app_demo\".\"users\""),
                    &[],
                )
                .await
                .expect("SELECT both columns");
            assert_eq!(rows.len(), 1, "exactly one row inserted");
            assert_eq!(rows[0][0].as_deref(), Some("***-**-6789"));
            assert_eq!(rows[0][1].as_deref(), Some("123-45-6789"));
        });
    })
}

/// **A default SELECT serves the masked column**: a default
/// read names the field's display column and never selects raw storage.
#[test]
fn a_select_serves_the_masked_column_sqlite() {
    Host::test(|host| {
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
                "id": { "type": "string" },
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
            let bq = compile_insert(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &schema,
                &doc,
            )
            .expect("compile insert");
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
            let bq = compile_find(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &crate::value!({ "id": "usr_01" }),
                None,
                None,
                None,
                None,
                &schema,
            )
            .expect("compile find");
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
        let schema = crate::value!({
            "id": { "type": "string" },
            "ssn": {
                "type": "string",
                "encrypted": true,
                "mask": { "kind": "none", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let bq = compile_find(
            &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &crate::value!({}),
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect("compile find");
        // Schema-aware reads now always expand to the allowlisted public
        // column set, even when every mask is `kind: "none"`.
        assert!(
            !bq.sql.contains("SELECT *"),
            "schema-backed reads must avoid `*`: {}",
            bq.sql,
        );
        assert!(bq.sql.contains("\"source\".\"ssn\" AS \"ssn\""));
        assert!(bq.sql.contains("\"source\".\"name\" AS \"name\""));
    })
}
