//! SQLite system fields contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;
use crate::tests::fixtures::schema::fixture_table_sql_for;

use zeroship_migrate::schema::query::FkEmission;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// **Deferred**: the SDK INSERT auto-populate path (which
/// supplies `id` + `created_at` etc. from the runtime) is out of
/// scope here; only the DDL is exercised, so this end-to-end test
/// supplies the system fields manually via a raw-SQL INSERT to confirm
/// the emitted columns accept the canonical value shapes (TEXT id,
/// CURRENT_TIMESTAMP defaults firing on omitted columns).
#[test]
fn inserting_a_row_without_user_fields_succeeds_via_system_fields_only() {
    Host::test(|host| {
        use zeroship_data_sql::compile::SqlDialect;

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            let sql = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &zeroship_data_sql::value!({}),
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build sqlite DDL");
            // Split on `;\n` — SQLite's `Connection::execute` runs one
            // statement per call (see sibling test's note).
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

            // Raw INSERT: supply only `id` (no SDK auto-populate here).
            // The 3 NULL-able columns + 3 DEFAULT'd columns fill in from
            // the engine.
            backend
                .execute_fixture(
                    "INSERT INTO \"app_demo\".\"posts\" (id) VALUES ('post_01')",
                    &[],
                )
                .await
                .expect("INSERT with only id must succeed");

            // Round-trip: confirm `version = 1`, `deleted_at IS NULL`,
            // `created_at IS NOT NULL`. Pin the canonical shape the DDL
            // promises.
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let rows = client
                .query(
                    "SELECT id, version, deleted_at IS NULL AS dn, \
                        created_at IS NOT NULL AS cn \
                 FROM \"app_demo\".\"posts\"",
                    &[],
                )
                .await
                .expect("SELECT ok");
            assert_eq!(rows.len(), 1, "expected one row");
            let row = &rows[0];
            assert_eq!(row[0].as_deref(), Some("post_01"), "id round-trip");
            assert_eq!(row[1].as_deref(), Some("1"), "version default = 1");
            assert_eq!(row[2].as_deref(), Some("1"), "deleted_at IS NULL default");
            assert_eq!(
                row[3].as_deref(),
                Some("1"),
                "created_at IS NOT NULL default"
            );
        });
    })
}

/// End-to-end: the `apply_system_fields_on_insert` pass mints a typed_id
/// and the subsequent `build_insert_with_dialect` INSERT lands a row
/// with the canonical 7 system fields populated. Mirrors what the
/// `dispatch_insert` hot path does at request time but without standing
/// up V8 — exercises the SQL builder + SQLite engine round-trip.
#[test]
fn insert_end_to_end_populates_system_fields_sqlite() {
    Host::test(|host| {
        use zeroship_data_orm::crud::system_fields_pass::apply_system_fields_on_insert;
        use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            // 1. Stand up the table with the 7 system-field columns.
            let schema = zeroship_data_sql::value!({
                "title": {"type": "string", "required": true},
            });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build sqlite DDL");
            for stmt in ddl.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .unwrap_or_else(|e| panic!("DDL: {trimmed}\n{e:?}"));
            }

            // 2. Build the inbound doc — creator passes ONLY the user
            // field. The auto-mint pass injects `id`, `created_by`,
            // `updated_by`; the DB fires its DEFAULT for the timestamps +
            // version.
            let mut doc = zeroship_data_sql::value!({ "title": "PR 3 hello" });
            // The pass takes the collection's descriptor entry (the write pipeline
            // resolves it once per op and hands it down); the only thing it reads
            // out of it is a declared `t.id(prefix)`, and this one declares none.
            apply_system_fields_on_insert(
                &mut doc,
                &zeroship_data_sql::value!({ "title": { "type": "string", "required": true } }),
                "posts",
                Some("usr_actor_e2e"),
            )
            .expect("derived prefix must be accepted");

            // The minted id must carry the `post_` prefix (collection-name
            // derived since the schema didn't declare an `idPrefix`).
            let minted_id = doc
                .get("id")
                .and_then(|v| v.as_str())
                .expect("id minted by auto-mint pass")
                .to_string();
            assert!(
                minted_id.starts_with("post_"),
                "expected post_ prefix, got: {minted_id}"
            );

            // 3. Build + execute the INSERT. `RETURNING *` returns rows,
            // so route through the dedicated client's `query` path (the
            // pool's `execute_fixture` rejects result-bearing statements).
            let built = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .expect("build_insert");
            let params = &built.params;
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client (insert)");
            let returning_rows = client
                .query_values(&built.sql, params)
                .await
                .unwrap_or_else(|e| panic!("INSERT: {}\n{e:?}", built.sql));
            assert_eq!(returning_rows.len(), 1, "INSERT RETURNING * gives one row");

            // 4. Round-trip via SELECT: every system field must be the
            // canonical shape.
            let rows = client
                .query(
                    "SELECT id, title, created_by, updated_by, version, \
                        deleted_at IS NULL AS dn, \
                        created_at IS NOT NULL AS cn, \
                        updated_at IS NOT NULL AS un \
                 FROM \"app_demo\".\"posts\"",
                    &[],
                )
                .await
                .expect("SELECT ok");
            assert_eq!(rows.len(), 1, "exactly one row");
            let row = &rows[0];
            assert_eq!(row[0].as_deref(), Some(minted_id.as_str()), "id round-trip");
            assert_eq!(row[1].as_deref(), Some("PR 3 hello"), "title preserved");
            assert_eq!(
                row[2].as_deref(),
                Some("usr_actor_e2e"),
                "created_by from actor"
            );
            assert_eq!(
                row[3].as_deref(),
                Some("usr_actor_e2e"),
                "updated_by from actor (== created_by on INSERT)"
            );
            assert_eq!(row[4].as_deref(), Some("1"), "version default = 1");
            assert_eq!(row[5].as_deref(), Some("1"), "deleted_at IS NULL");
            assert_eq!(row[6].as_deref(), Some("1"), "created_at NOT NULL");
            assert_eq!(row[7].as_deref(), Some("1"), "updated_at NOT NULL");
        });
    })
}

/// FK type cascade end-to-end: a `t.ref(...)` column now emits TEXT
/// (not INTEGER) so the column accepts typed_id string
/// values without storage-class mismatch.
///
/// **Scope note**: the actual `FOREIGN KEY ... REFERENCES "app"."tbl"`
/// constraint clause uses a schema-qualified target name that SQLite's
/// CREATE TABLE parser refuses (a pre-existing PG-only path). This
/// test stands up the posts table WITHOUT the FK clause (skipping the
/// constraint with `FkEmission::Deferred` + empty existing set) and
/// asserts the column TYPE is TEXT - the FK type-cascade surface this
/// test pins. End-to-end FK constraint validation on SQLite remains a
/// PG-only path until the cross-app FK rework lands.
#[test]
fn insert_with_fk_uses_text_keys_end_to_end_sqlite() {
    Host::test(|host| {
        use zeroship_data_orm::crud::system_fields_pass::apply_system_fields_on_insert;
        use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            // Stand up the posts table with an `authorId` ref column. The
            // FK type cascade emits TEXT for the column type. We use
            // `FkEmission::Deferred(empty)` so the FK clause is omitted -
            // SQLite refuses schema-qualified REFERENCES targets, a
            // pre-existing PG-only path this test does not attempt to fix.
            let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
            // One binding for the table's shape and for the write's projection: the
            // DDL emitter and the INSERT builder must not read two literals.
            let schema = zeroship_data_sql::value!({
                "title": {"type": "string", "required": true},
                "authorId": {"type": "ref", "refTarget": "users"},
            });
            let posts_ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Deferred(&empty),
                SqlDialect::Sqlite,
            )
            .expect("build posts DDL");
            // Pin the FK column type to TEXT (was INTEGER before the
            // FK type cascade).
            assert!(
                posts_ddl.contains("\"authorId\" TEXT"),
                "expected TEXT FK column, got DDL: {posts_ddl}"
            );
            for stmt in posts_ddl.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .unwrap_or_else(|e| panic!("posts DDL: {trimmed}\n{e:?}"));
            }

            // Insert a post whose authorId is a typed_id string. Before
            // the FK type cascade the column was INTEGER and a typed_id
            // string would round-trip as the literal string under
            // SQLite's permissive storage model but assert against the
            // declared INTEGER affinity at introspection. With the
            // cascade applied the affinity is TEXT - no surprise on
            // read-back.
            let mut post_doc = zeroship_data_sql::value!({
                "title": "fk-ok",
                "authorId": "usr_01HXY3Z9PQR2STUV4WXY5Z6789",
            });
            apply_system_fields_on_insert(
                &mut post_doc,
                &zeroship_data_sql::value!({
                    "title": {"type": "string", "required": true},
                    "authorId": {"type": "ref", "refTarget": "users"},
                }),
                "posts",
                None,
            )
            .expect("derived prefix must be accepted");
            let built = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &post_doc,
                SqlDialect::Sqlite,
            )
            .expect("build posts insert");
            let params = &built.params;
            let client = backend.fixture_session("app_demo").await.expect("client");
            client
                .query_values(&built.sql, params)
                .await
                .unwrap_or_else(|e| panic!("post INSERT: {}\n{e:?}", built.sql));

            // Round-trip: the authorId on the row equals the typed_id we
            // inserted. Confirms TEXT storage preserves the typed_id
            // verbatim (no integer-coercion).
            let rows = client
                .query(
                    "SELECT authorId FROM \"app_demo\".\"posts\" WHERE title = 'fk-ok'",
                    &[],
                )
                .await
                .expect("SELECT");
            assert_eq!(rows.len(), 1, "exactly one row");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("usr_01HXY3Z9PQR2STUV4WXY5Z6789"),
                "FK round-trip preserves typed_id string"
            );
        });
    })
}

/// End-to-end: an UPDATE built via `build_update_one_with_system_fields`
/// on SQLite bumps `version` by exactly 1 and rewrites `updated_at`.
/// Mirrors what `dispatch_update_one` does at request time but bypasses
/// V8 / the per-isolate schema cache (we drive the SQL builder
/// directly).
#[test]
fn update_end_to_end_bumps_version_by_one_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_update_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");

            let schema = zeroship_data_sql::value!({
                "title": {"type": "string", "required": true},
            });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
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

            // INSERT row at version 1 (DDL default).
            let doc = zeroship_data_sql::value!({
                "id": "post_v1bump",
                "title": "original",
            });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let ins_params = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client
                .query_values(&ins.sql, ins_params)
                .await
                .expect("INSERT");

            // UPDATE via the system-fields-aware builder.
            let filter = zeroship_data_sql::value!({ "id": "post_v1bump" });
            let update = zeroship_data_sql::value!({ "title": "v2" });
            let autobump = SystemFieldAutoBump {
                dispatch_write: true,
                actor_id: Some("usr_e2e_updater"),
                ..Default::default()
            };
            let upd = build_update_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter,
                &update,
                SqlDialect::Sqlite,
                &autobump,
            )
            .unwrap();
            let upd_params = &upd.params;
            let returning = client
                .query_values(&upd.sql, upd_params)
                .await
                .expect("UPDATE");
            assert_eq!(returning.len(), 1, "UPDATE returned 1 row");

            // SELECT and confirm version bumped to 2 and updated_by was set.
            let rows = client
            .query(
                "SELECT title, version, updated_by FROM \"app_demo\".\"posts\" WHERE id = 'post_v1bump'",
                &[],
            )
            .await
            .expect("SELECT");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some("v2"), "title updated");
            assert_eq!(
                rows[0][1].as_deref(),
                Some("2"),
                "version bumped from 1 to 2"
            );
            assert_eq!(
                rows[0][2].as_deref(),
                Some("usr_e2e_updater"),
                "updated_by stamped from actor",
            );
        });
    })
}

/// End-to-end CAS success: an UPDATE that filters by the correct
/// `version` bumps the row.
#[test]
fn update_end_to_end_with_correct_version_succeeds_and_bumps_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_update_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();

            let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_cas_ok", "title": "v1" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let ins_params = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, ins_params).await.unwrap();

            // CAS at the correct version (1).
            let filter = zeroship_data_sql::value!({ "id": "post_cas_ok", "version": 1 });
            let update = zeroship_data_sql::value!({ "title": "v2" });
            let autobump = SystemFieldAutoBump {
                dispatch_write: true,
                actor_id: Some("usr_cas_ok"),
                ..Default::default()
            };
            let upd = build_update_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter,
                &update,
                SqlDialect::Sqlite,
                &autobump,
            )
            .unwrap();
            let upd_params = &upd.params;
            let returning = client.query_values(&upd.sql, upd_params).await.unwrap();
            assert_eq!(returning.len(), 1, "CAS matched: 1 affected row");

            let rows = client
                .query(
                    "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_cas_ok'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                rows[0][0].as_deref(),
                Some("2"),
                "version bumped on CAS hit"
            );
        });
    })
}

/// End-to-end CAS failure: an UPDATE that filters by a stale `version`
/// affects zero rows. The dispatch layer (not exercised here) converts
/// the empty RETURNING into a typed `version_mismatch` — at the SQL
/// layer we just confirm the affected-rows = 0 contract.
#[test]
fn update_end_to_end_with_stale_version_affects_zero_rows_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_update_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();

            let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_cas_stale", "title": "v1" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let ins_params = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, ins_params).await.unwrap();

            // CAS at the wrong version (row is at 1; we expect 99).
            let filter = zeroship_data_sql::value!({ "id": "post_cas_stale", "version": 99 });
            let update = zeroship_data_sql::value!({ "title": "v_nope" });
            let autobump = SystemFieldAutoBump {
                dispatch_write: true,
                actor_id: Some("usr_cas_stale"),
                ..Default::default()
            };
            let upd = build_update_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter,
                &update,
                SqlDialect::Sqlite,
                &autobump,
            )
            .unwrap();
            let upd_params = &upd.params;
            let returning = client.query_values(&upd.sql, upd_params).await.unwrap();
            assert!(returning.is_empty(), "stale CAS: 0 affected rows");

            // Row stays at version 1 and original title.
            let rows = client
                .query(
                    "SELECT version, title FROM \"app_demo\".\"posts\" WHERE id = 'post_cas_stale'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows[0][0].as_deref(), Some("1"));
            assert_eq!(rows[0][1].as_deref(), Some("v1"));
        });
    })
}

/// End-to-end concurrent CAS: two UPDATEs at the same version — one
/// wins, one loses. Confirms the WHERE version-check is atomic with
/// the SET.
#[test]
fn update_end_to_end_concurrent_two_updates_one_wins_one_loses_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_update_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();

            let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_race", "title": "v0" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let ins_params = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, ins_params).await.unwrap();

            // First UPDATE at version=1 wins.
            let filter1 = zeroship_data_sql::value!({ "id": "post_race", "version": 1 });
            let update1 = zeroship_data_sql::value!({ "title": "v_winner" });
            let ab = SystemFieldAutoBump {
                dispatch_write: true,
                actor_id: Some("usr_a"),
                ..Default::default()
            };
            let upd1 = build_update_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter1,
                &update1,
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p1 = &upd1.params;
            let r1 = client.query_values(&upd1.sql, p1).await.unwrap();
            assert_eq!(r1.len(), 1, "first CAS wins");

            // Second UPDATE at version=1 loses (row is now at version=2).
            let filter2 = zeroship_data_sql::value!({ "id": "post_race", "version": 1 });
            let update2 = zeroship_data_sql::value!({ "title": "v_loser" });
            let upd2 = build_update_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter2,
                &update2,
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p2 = &upd2.params;
            let r2 = client.query_values(&upd2.sql, p2).await.unwrap();
            assert!(r2.is_empty(), "second CAS loses");

            // Final state: winner's title, version=2.
            let rows = client
                .query(
                    "SELECT title, version FROM \"app_demo\".\"posts\" WHERE id = 'post_race'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows[0][0].as_deref(), Some("v_winner"));
            assert_eq!(rows[0][1].as_deref(), Some("2"));
        });
    })
}

/// End-to-end: UPDATE without a `version` filter blindly succeeds and
/// bumps version. Confirms the non-CAS path stays last-writer-wins.
#[test]
fn update_end_to_end_without_version_filter_succeeds_blindly_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_update_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();

            let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_blind", "title": "v0" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let ins_params = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, ins_params).await.unwrap();

            // No version in filter — last-writer-wins. Three consecutive
            // updates land in order; version is bumped each time.
            for new_title in ["v1", "v2", "v3"] {
                let filter = zeroship_data_sql::value!({ "id": "post_blind" });
                let update = zeroship_data_sql::value!({ "title": new_title });
                let ab = SystemFieldAutoBump {
                    dispatch_write: true,
                    actor_id: Some("usr_blind"),
                    ..Default::default()
                };
                let upd = build_update_many_with_system_fields(
                    &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                    "posts",
                    &schema,
                    &filter,
                    &update,
                    SqlDialect::Sqlite,
                    &ab,
                )
                .unwrap();
                let p = &upd.params;
                let r = client.query_values(&upd.sql, p).await.unwrap();
                assert_eq!(r.len(), 1, "blind UPDATE succeeds");
            }

            let rows = client
                .query(
                    "SELECT title, version FROM \"app_demo\".\"posts\" WHERE id = 'post_blind'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows[0][0].as_deref(), Some("v3"));
            assert_eq!(
                rows[0][1].as_deref(),
                Some("4"),
                "version bumped 1→2→3→4 across three updates"
            );
        });
    })
}

#[test]
fn soft_delete_end_to_end_sets_deleted_at_and_bumps_version_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_soft_delete_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_sd1", "title": "to be deleted" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();

            let filter = zeroship_data_sql::value!({ "id": "post_sd1" });
            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_deleter"),
                ..Default::default()
            };
            let sd = build_soft_delete_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter,
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &sd.params;
            let returning = client.query_values(&sd.sql, p).await.unwrap();
            assert_eq!(returning.len(), 1, "soft-delete returned 1 row");

            let rows = client
            .query(
                "SELECT deleted_at IS NOT NULL AS dn, version, updated_by FROM \"app_demo\".\"posts\" WHERE id = 'post_sd1'",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some("1"), "deleted_at IS NOT NULL");
            assert_eq!(rows[0][1].as_deref(), Some("2"), "version bumped from 1");
            assert_eq!(
                rows[0][2].as_deref(),
                Some("usr_deleter"),
                "updated_by stamped from actor"
            );
        });
    })
}

#[test]
fn soft_delete_on_already_soft_deleted_row_affects_zero_rows_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_soft_delete_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_idem", "title": "x" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();

            let filter = zeroship_data_sql::value!({ "id": "post_idem" });
            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_x"),
                ..Default::default()
            };
            let sd = build_soft_delete_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter,
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p1 = &sd.params;
            let r1 = client.query_values(&sd.sql, p1).await.unwrap();
            assert_eq!(r1.len(), 1, "first soft-delete hits");
            let r2 = client.query_values(&sd.sql, p1).await.unwrap();
            assert!(r2.is_empty(), "re-soft-deleting is a no-op");
        });
    })
}

#[test]
fn find_with_soft_delete_filter_hides_soft_deleted_rows_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_find_with_schema_and_unmask_and_soft_delete,
            build_insert_with_dialect, build_soft_delete_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            for id in &["post_alive_a", "post_alive_b", "post_dead"] {
                let doc = zeroship_data_sql::value!({ "id": id, "title": id });
                let ins = build_insert_with_dialect(
                    &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                    "posts",
                    &schema,
                    &doc,
                    SqlDialect::Sqlite,
                )
                .unwrap();
                let p = &ins.params;
                let client = backend.fixture_session("app_demo").await.unwrap();
                client.query_values(&ins.sql, p).await.unwrap();
            }
            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_actor"),
                ..Default::default()
            };
            let sd = build_soft_delete_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "id": "post_dead" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &sd.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&sd.sql, p).await.unwrap();

            let q = build_find_with_schema_and_unmask_and_soft_delete(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &zeroship_data_sql::value!({}),
                None,
                None,
                None,
                None,
                &zeroship_data_sql::value!({ "title": { "type": "string" } }),
                &[],
                true,
            )
            .unwrap();
            let rows = client.query(&q.sql, &[]).await.unwrap();
            assert_eq!(rows.len(), 2, "soft-deleted row hidden by auto-filter");

            let q2 = build_find_with_schema_and_unmask_and_soft_delete(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &zeroship_data_sql::value!({}),
                None,
                None,
                None,
                None,
                &zeroship_data_sql::value!({ "title": { "type": "string" } }),
                &[],
                false,
            )
            .unwrap();
            let rows2 = client.query(&q2.sql, &[]).await.unwrap();
            assert_eq!(rows2.len(), 3, "include_deleted: all rows visible");
        });
    })
}

#[test]
fn restore_clears_deleted_at_and_bumps_version_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_restore_many_with_system_fields, build_soft_delete_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_rs", "title": "x" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();

            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_x"),
                ..Default::default()
            };
            let sd = build_soft_delete_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "id": "post_rs" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &sd.params;
            client.query_values(&sd.sql, p).await.unwrap();

            let rs = build_restore_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "id": "post_rs" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &rs.params;
            let returning = client.query_values(&rs.sql, p).await.unwrap();
            assert_eq!(returning.len(), 1, "restore hit the soft-deleted row");

            let rows = client
            .query(
                "SELECT deleted_at IS NULL AS dn, version FROM \"app_demo\".\"posts\" WHERE id = 'post_rs'",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(rows[0][0].as_deref(), Some("1"), "deleted_at IS NULL");
            assert_eq!(rows[0][1].as_deref(), Some("3"), "version bumped twice");
        });
    })
}

#[test]
fn restore_on_already_live_row_affects_zero_rows_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_restore_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_live", "title": "x" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();

            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_x"),
                ..Default::default()
            };
            let rs = build_restore_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "id": "post_live" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &rs.params;
            let returning = client.query_values(&rs.sql, p).await.unwrap();
            assert!(returning.is_empty(), "restoring a live row is a no-op");
            let rows = client
                .query(
                    "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_live'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows[0][0].as_deref(), Some("1"), "version untouched");
        });
    })
}

#[test]
fn soft_delete_then_restore_full_lifecycle_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_find_with_schema_and_unmask_and_soft_delete,
            build_insert_with_dialect, build_restore_many_with_system_fields,
            build_soft_delete_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            let doc = zeroship_data_sql::value!({ "id": "post_lc", "title": "lifecycle" });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();

            let find_default = build_find_with_schema_and_unmask_and_soft_delete(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &zeroship_data_sql::value!({}),
                None,
                None,
                None,
                None,
                &zeroship_data_sql::value!({ "title": { "type": "string" } }),
                &[],
                true,
            )
            .unwrap();
            let r = client.query(&find_default.sql, &[]).await.unwrap();
            assert_eq!(r.len(), 1, "live row visible pre-delete");

            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_x"),
                ..Default::default()
            };
            let sd = build_soft_delete_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "id": "post_lc" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &sd.params;
            client.query_values(&sd.sql, p).await.unwrap();

            let r = client.query(&find_default.sql, &[]).await.unwrap();
            assert!(r.is_empty(), "soft-deleted row hidden");

            let find_inc = build_find_with_schema_and_unmask_and_soft_delete(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &zeroship_data_sql::value!({}),
                None,
                None,
                None,
                None,
                &zeroship_data_sql::value!({ "title": { "type": "string" } }),
                &[],
                false,
            )
            .unwrap();
            let r = client.query(&find_inc.sql, &[]).await.unwrap();
            assert_eq!(r.len(), 1, "include_deleted reveals it");

            let rs = build_restore_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "id": "post_lc" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &rs.params;
            client.query_values(&rs.sql, p).await.unwrap();

            let r = client.query(&find_default.sql, &[]).await.unwrap();
            assert_eq!(r.len(), 1, "restored row visible to default find");

            let rows = client
                .query(
                    "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_lc'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows[0][0].as_deref(), Some("3"));
        });
    })
}

#[test]
fn soft_delete_many_sets_deleted_at_on_all_matching_live_rows_sqlite() {
    Host::test(|host| {
        use zeroship_data_sql::compile::{
            SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
            build_soft_delete_many_with_system_fields,
        };

        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("app_demo").await.unwrap();
            let schema = zeroship_data_sql::value!({ "author": { "type": "string" }, "title": { "type": "string" } });
            let ddl = fixture_table_sql_for(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .unwrap();
            for stmt in ddl.split(";\n") {
                let t = stmt.trim();
                if t.is_empty() {
                    continue;
                }
                backend.execute_fixture(t, &[]).await.unwrap();
            }

            for (id, author) in &[
                ("post_a1", "usr_a"),
                ("post_a2", "usr_a"),
                ("post_a3_dead", "usr_a"),
                ("post_b1", "usr_b"),
                ("post_b2", "usr_b"),
            ] {
                let doc = zeroship_data_sql::value!({ "id": id, "author": author, "title": id });
                let ins = build_insert_with_dialect(
                    &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                    "posts",
                    &schema,
                    &doc,
                    SqlDialect::Sqlite,
                )
                .unwrap();
                let p = &ins.params;
                let client = backend.fixture_session("app_demo").await.unwrap();
                client.query_values(&ins.sql, p).await.unwrap();
            }
            backend
            .execute_fixture(
                "UPDATE \"app_demo\".\"posts\" SET deleted_at = CURRENT_TIMESTAMP WHERE id = 'post_a3_dead'",
                &[],
            )
            .await
            .unwrap();

            let ab = SystemFieldAutoBump {
                actor_id: Some("usr_admin"),
                ..Default::default()
            };
            let sd = build_soft_delete_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &zeroship_data_sql::value!({ "author": "usr_a" }),
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &sd.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            let returning = client.query_values(&sd.sql, p).await.unwrap();
            assert_eq!(
                returning.len(),
                2,
                "only 2 live usr_a rows affected; already-deleted excluded"
            );

            let dead_a = client
            .query(
                "SELECT COUNT(*) FROM \"app_demo\".\"posts\" WHERE author = 'usr_a' AND deleted_at IS NOT NULL",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(dead_a[0][0].as_deref(), Some("3"));
            let live_b = client
            .query(
                "SELECT COUNT(*) FROM \"app_demo\".\"posts\" WHERE author = 'usr_b' AND deleted_at IS NULL",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(live_b[0][0].as_deref(), Some("2"));
        });
    })
}

#[test]
fn purge_path_uses_hard_delete_sql_unchanged_sqlite() {
    Host::test(|_| {
        use zeroship_data_sql::compile::build_delete_one;

        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let q = build_delete_one(
            &zeroship_data_sql::SchemaName::new("app1").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "x" }),
        )
        .unwrap();
        assert!(q.sql.starts_with("DELETE FROM"));
        // A purge is a HARD delete: `deleted_at` must not appear in the SET or the
        // WHERE. It IS a system field, so the projection names it - scope the
        // assertion to the statement before `RETURNING`, which is what the test
        // means and what `contains` over the whole string used to imply only
        // because that clause was `*`.
        let body = q
            .sql
            .split_once(" RETURNING ")
            .expect("a RETURNING clause")
            .0;
        assert!(!body.contains("deleted_at"));
        assert!(!q.sql.contains("RETURNING *"));
        assert!(q.sql.contains(r#"RETURNING "id""#));
    })
}
