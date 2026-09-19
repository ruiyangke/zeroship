use super::fixtures::*;
use zeroship_data_orm::value;

#[test]
fn nested_query_refuses_writes_after_await_and_restores_mutation() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
        apply_schema_ahead_of_runtime(
            &dir,
            &format!(
                "CREATE TABLE \"{alias}\".notes ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);"
            ),
        );
        let source = sqlite_runtime_source(
            "notes",
            &value!({"title":{"type":"string", "required":true}}),
            r#"
            import { runQuery, runMutation } from 'zeroship';
            const _procedures = {
                async compose() {
                    const notes = env.db.collection(COLLECTION);
                    const rejected = await runMutation(async () => {
                        await notes.insert({title: 'before'});
                        const rejected = await runQuery(async () => {
                            await notes.find({}, {});
                            await new Promise(resolve => setTimeout(resolve, 5));
                            try { await notes.insert({title: 'blocked'}); }
                            catch (error) { return error.code; }
                            return 'unexpected success';
                        });
                        await notes.insert({title: 'after'});
                        return rejected;
                    });
                    const rows = await notes.find({}, {orderBy: {title: 1}});
                    return {rejected, titles: rows.map(row => row.title)};
                },
            };
        "#,
        );
        assert_eq!(
            dispatch_sqlite_runtime(&dir, &source, "compose"),
            value!({"json":{
                "rejected":"capability_violation", "titles":["after", "before"],
            }})
        );
    });
}
