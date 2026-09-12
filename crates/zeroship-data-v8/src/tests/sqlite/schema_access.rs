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
