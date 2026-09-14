//! DB facade preparation through the native plugin.

use super::fixtures::{drain_pg, require_pg};
use crate::tests::fixtures::parity;

#[compio::test]
async fn db_plugin_prepares_the_creator_facade_before_module_evaluation() {
    let (_postgres, url) = require_pg().await;
    let app = crate::tests::fixtures::test_app_id!();
    let descriptor = parity::runtime_descriptor(
        "posts",
        &zeroship_data_orm::value!({
            "id": { "type": "id", "idPrefix": "post" },
            "title": { "type": "string", "required": true },
        }),
    );
    let source = r#"
        import { env } from "zeroship";
        const queryAtEvaluation = typeof env.db.posts.find({}).sort === 'function';
        export default { rpc: { inspect: async () => {
            return {
                queryAtEvaluation,
                collection: typeof env.db.posts.insert === 'function',
                transaction: typeof env.db.transaction === 'function',
                live: typeof env.db.live === 'function',
                policy: typeof env.db.declareMaskPolicy === 'function',
            };
        } } };
    "#;
    let (status, value) =
        parity::dispatch_zs_with_descriptor(&url, source, "inspect", &app, &descriptor);
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        value["json"],
        zeroship_data_orm::value!({
            "queryAtEvaluation": true, "collection": true,
            "transaction": true, "live": true, "policy": true,
        })
    );
    drain_pg().await;
}

#[compio::test]
async fn startup_database_work_and_unmask_refusal_match_postgres_and_sqlite() {
    let (_postgres, url) = require_pg().await;
    let pg_app = crate::tests::fixtures::test_app_id!();
    let sqlite_app = crate::tests::fixtures::test_app_id!();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = parity::run_startup_database_probe(&parity::sqlite_url(&dir), &sqlite_app);
    let postgres = parity::run_startup_database_probe(&url, &pg_app);
    assert_eq!(postgres, sqlite);
    drain_pg().await;
}
