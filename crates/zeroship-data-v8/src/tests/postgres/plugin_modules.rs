//! DB adapter source delivery through the native plugin.

use super::fixtures::{drain_pg, require_pg};
use crate::tests::fixtures::parity;

#[compio::test]
async fn db_plugin_supplies_the_installer_and_shares_its_module_instance() {
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
        import * as sdk from "zeroship:db/internal";
        const collectionAtEvaluation = env.db.posts instanceof sdk.Collection;
        const initial = await import("zeroship:db/internal");
        export default { rpc: { inspect: async () => {
            const later = await import("zeroship:db/internal");
            return {
                same: initial === sdk && later === sdk,
                collectionAtEvaluation,
                installer: typeof sdk.installSchema === 'function',
                collection: env.db.posts instanceof sdk.Collection,
                naming: typeof sdk.naming === 'object',
                policy: typeof env.db.declareMaskPolicy === 'function',
                policyInstaller: typeof sdk._flushPendingMaskPolicy,
            };
        } } };
    "#;
    let (status, value) =
        parity::dispatch_zs_with_descriptor(&url, source, "inspect", &app, &descriptor);
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        value["json"],
        zeroship_data_orm::value!({
            "same": true, "installer": true, "collection": true,
            "collectionAtEvaluation": true,
            "naming": true, "policy": true, "policyInstaller": "undefined",
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
