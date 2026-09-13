use super::fixtures::*;
use crate::tests::fixtures::parity;

#[test]
fn prefixed_creator_table_delivers_change_through_the_v8_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let collection = "__zeroship_workflow_app_state";
    apply_schema_ahead_of_runtime(
        &dir,
        &format!(
            "CREATE TABLE \"{LOCAL_DEV_APP_ID}\".\"{collection}\" ({SYSTEM_COLUMNS_SQLITE}, \
             \"name\" TEXT NOT NULL)"
        ),
    );
    let source = sqlite_runtime_source(
        collection,
        &zeroship_data_orm::value!({"name": {"type": "string", "required": true}}),
        r#"
async function observe() {
    try {
        const table = env.db.collection(COLLECTION);
        const subscription = table.openSubscription();
        await table.insert({ name: "created" });
        const event = await subscription.next();
        subscription.close();
        return event;
    } catch (error) {
        return { caught: { message: error?.message, code: error?.code } };
    }
}
observe.config = { kind: "action" };
const _procedures = { observe };
"#,
    );

    let event = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "observe"));
    assert_eq!(event["kind"], "change", "{event}");
    assert_eq!(event["op"], "insert");
    assert_eq!(event["collection"], collection);
}
