use super::fixtures::*;
use zeroship_data_orm::value;

#[test]
fn creator_queries_can_access_prefixed_tables_in_the_bound_schema() {
    run(async {
        let directory = tempfile::tempdir().unwrap();
        for table in ["__zeroship_workflow_records", "__zeroship_app_metadata"] {
            apply_schema_ahead_of_runtime(&directory, &format!(
                "CREATE TABLE \"default\".\"{table}\" ({SYSTEM_COLUMNS_SQLITE}, label TEXT NOT NULL);"
            ));
            let source = sqlite_runtime_source(
                table,
                &value!({"label":{"type":"string","required":true}}),
                r#"
const _procedures = {
  async exercise() {
    const table = env.db.collection(COLLECTION);
    const row = await table.insert({label:"created"});
    await table.update({id:row.id}, {label:"updated"});
    const rows = await table.find({id:row.id}, {});
    const removed = await table.purgeMany({id:row.id});
    let escaped = false;
    try { await env.db.collection("another_schema." + COLLECTION).find({}, {}); }
    catch (_) { escaped = true; }
    return {label:rows[0].label, removed, remaining:await table.count({}), escaped};
  }
};
"#,
            );
            assert_eq!(
                dispatch_sqlite_runtime(&directory, &source, "exercise"),
                value!({
                    "json":{"label":"updated", "removed":1, "remaining":0, "escaped":true}
                })
            );
        }
    });
}

#[test]
fn creator_operations_preserve_composite_keys_through_v8() {
    run(async {
        let _keys = with_project_key(&["default"], &"6".repeat(64));
        let directory = tempfile::tempdir().unwrap();
        let policy = zeroship_migrate::effective_policy_from_charter_toml(
            zeroship_migrate_server::policy::PLATFORM_CEILING_TOML,
        )
        .unwrap();
        let (_, statements) = zeroship_migrate::render_ir_envelope_sql_statements(
            zeroship_migrate::shipping_vendors(),
            include_str!("../../../../zeroship-data-orm/tests/fixtures/composite-migration.json"),
            &zeroship_migrate_sqlite::DIALECT,
            &zeroship_migrate::PreviewOpts {
                default_schema: "default".into(),
                owner_app: "default".into(),
                effective_policy: policy,
            },
        )
        .unwrap();
        rusqlite::Connection::open(directory.path().join("zs-default.sqlite"))
            .unwrap()
            .execute_batch(&statements.join(";"))
            .unwrap();
        let source = SqliteRuntimeSource {
            descriptor: include_str!(
                "../../../../zeroship-data-orm/tests/fixtures/composite.runtime.json"
            )
            .into(),
            source: format!(
                r#"
import {{ env }} from "zeroship";
const _procedures = {{
  async exercise() {{
    const table = env.db.collection("records");
    await table.insert({{app_key:"a", run_key:"shared", generation:1, label:"first", secret:"first"}});
    await table.insert({{app_key:"a", run_key:"shared", generation:2, label:"next", secret:"next"}});
    const key = {{app_key:"a", run_key:"shared", generation:1}};
    await table.update(key, {{secret:"updated"}});
    let immutable = false;
    try {{ await table.update(key, {{generation:3}}); }}
    catch (error) {{ immutable = error.code === "immutable_primary_key"; }}
    const first = await table.find(key, {{select:["secret"]}});
    const next = await table.find({{app_key:"a", run_key:"shared", generation:2}}, {{select:["secret"]}});
    return {{first, next, immutable}};
  }}
}};
{SQLITE_RUNTIME_RPC_SHIM}
"#
            ),
        };
        assert_eq!(
            dispatch_sqlite_runtime(&directory, &source, "exercise"),
            value!({
                "json":{"first":[{"secret":"updated"}], "next":[{"secret":"next"}], "immutable":true}
            })
        );
    });
}
