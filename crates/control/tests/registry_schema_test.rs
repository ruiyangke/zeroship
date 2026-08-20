//! Live-PG schema regression tests for control-plane registry tables.
//!
//! Configure a test database (`zeroship_core::config::test_database_url_opt`;
//! run `tests/provision_test_backends.sh` to provision one) to run; tests
//! skip otherwise.

use compio_postgres::{connect, Client, NoTls};
use zeroship_control::Registry;

use crate::common;

fn db_url() -> String {
    zeroship_core::config::test_database_url_opt()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| {
            "postgresql://postgres:zeroship@localhost:5440/zeroship_billing_test".to_string()
        })
}

async fn pg(db_url: &str) -> Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

#[compio::test]
async fn registry_core_tables_live_in_zeroship_schema() {
    let url = db_url();
    Registry::new(&url).await.expect("registry");
    let pg = pg(&url).await;

    for table in [
        "apps",
        "app_usage",
        "app_usage_history",
        "app_vars",
        "app_secrets",
        "app_env_expose",
        "creator_accounts",
        "creator_account_history",
        "payouts",
        "app_audit",
    ] {
        // Resolve the table's *actual* namespace via the catalog rather than
        // `to_regclass(...)::text`. `regclass`'s text form strips the schema
        // qualifier whenever the schema is on the connection's `search_path`,
        // so a literal `zeroship.apps` comparison is unreliable. Joining
        // `pg_class`→`pg_namespace` reports the schema unconditionally and
        // also asserts the table is not silently shadowed in `public`.
        //
        // Every platform/system table lives in ONE `zeroship` schema
        // (db/migrations-ts/20260702000100_schema_roles_extensions.ts; the registry code in
        // crates/control/src/registry.rs fully qualifies every reference as
        // `zeroship.*`). There is no `control` schema.
        let qualified = format!("zeroship.{table}");
        let rows = pg
            .query(
                "SELECT n.nspname AS schema
                 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE c.oid = to_regclass($1)",
                &[&qualified],
            )
            .await
            .unwrap_or_else(|e| panic!("resolve namespace for {qualified}: {e}"));
        assert_eq!(rows.len(), 1, "{qualified} should exist");
        let schema: String = rows[0].get("schema");
        assert_eq!(schema, "zeroship", "{qualified} should live in the zeroship schema");
    }

    // Teardown: `pg` holds this test's Postgres connection, and locals are
    // dropped only after the body returns - by which point the runtime is gone
    // and the socket can no longer be closed. Drop it explicitly, then wait for
    // the close to land.
    drop(pg);
    common::drain_pg().await;
}
