//! SQLite codecs contracts.
use super::fixtures::*;

use crate::tests::fixtures::schema::fixture_table_sql_sqlite;
use crate::tests::fixtures::Host;

use zeroship_migrate::schema::query::FkEmission;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// SQLite defaults and runtime timestamp binds must use the same UTC text form
/// so lexical ordering agrees with instant ordering. The fixture drives the
/// migration emitter for a system default and the runtime compiler for a
/// caller-provided instant, then inspects what each actually stored.
#[test]
fn dbbind134_sqlite_timestamp_spellings_invert_same_day_ordering() {
    Host::test(|host| {
        host.run(async {
            let app = "t134_spelling";
            let coll = "events";
            let binding = crate::tests::fixtures::harness_binding(app);
            let alias = binding.schema().as_str();
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding_for_alias(alias))
                .await
                .expect("attach app file");

            // Build the DDL through the EMITTER, not by hand. An earlier draft of
            // this test wrote `DEFAULT CURRENT_TIMESTAMP` as a literal, which meant
            // it could never observe a change to the emitter it claimed to test -
            // the comment asserted a mechanism the code did not drive.
            let schema = crate::value!({ "occurred_at": { "type": "timestamp" } });
            let ddl =
                fixture_table_sql_sqlite(binding.schema(), coll, &schema, &FkEmission::Inline)
                    .expect("build DDL");
            for stmt in ddl.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .expect("DDL exec");
            }

            // The DDL default itself must already carry the ISO-T spelling. This
            // catches a regression in the emitter without needing a row at all.
            assert!(
                ddl.contains("strftime('%Y-%m-%dT%H:%M:%fZ','now')"),
                "the emitted timestamp default must use the ISO-T spelling, got: {ddl}"
            );

            // Row A: id only, so `created_at` is written BY THE EMITTED DEFAULT.
            backend
                .execute_fixture(
                    &format!("INSERT INTO \"{alias}\".\"{coll}\" (id) VALUES ('a_default')"),
                    &[],
                )
                .await
                .expect("insert row A via the emitted column default");

            let client = backend.fixture_session(&crate::tests::fixtures::harness_alias(app)).await.expect("acquire client");

            // Row B: through the RUNTIME's builder, which converts a Unix-ms bind
            // for a declared timestamp column.
            let doc = crate::value!({
                "id": "b_bind",
                "occurred_at": 1_756_700_000_000_i64,
            });
            let runtime_schema = crate::tests::fixtures::generated_schema(schema.clone());
            let bq = compile_insert(binding.schema(), coll, &runtime_schema, &doc)
                .expect("compile insert");
            assert_eq!(
                bq.params[1],
                crate::value!("2025-09-01T04:13:20.000Z"),
                "the builder must bind canonical UTC text",
            );
            // `build_insert` emits a RETURNING clause, so this goes through `query`
            // rather than `execute_fixture` - the latter refuses a statement that yields
            // rows ("Execute returned results - did you mean to call query?").
            let params = &bq.params;
            client
                .query_values(&bq.sql, params)
                .await
                .expect("insert row B through the runtime builder");

            let a_stamp = client
                .query(
                    &format!(
                        "SELECT created_at FROM \"{alias}\".\"{coll}\" WHERE id = 'a_default'"
                    ),
                    &[],
                )
                .await
                .expect("read the emitter-defaulted stamp")[0][0]
                .clone()
                .expect("created_at is NOT NULL");
            let b_stamp = client
                .query(
                    &format!("SELECT occurred_at FROM \"{alias}\".\"{coll}\" WHERE id = 'b_bind'"),
                    &[],
                )
                .await
                .expect("read the bind-written stamp")[0][0]
                .clone()
                .expect("occurred_at was written");

            // Defaults and bound values must have the same separator; SQLite
            // compares this storage as text.
            let a_sep = a_stamp.as_bytes()[10] as char;
            let b_sep = b_stamp.as_bytes()[10] as char;
            assert_eq!(
                a_sep, b_sep,
                "the two emitters disagree on the separator: default wrote {a_stamp:?} \
             (sep {a_sep:?}), runtime bind wrote {b_stamp:?} (sep {b_sep:?})"
            );
            assert_eq!(
                a_sep, 'T',
                "both must settle on the ISO-T spelling the data plane already binds; \
             got {a_stamp:?}"
            );
        });
    })
}
