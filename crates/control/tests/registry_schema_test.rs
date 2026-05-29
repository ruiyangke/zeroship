//! Live-PG schema regression tests for control-plane registry tables.
//!
//! Set `CONTROL_TEST_DB` or `PG_TEST_URL` to run; tests skip otherwise.

use compio_postgres::{connect, Client, NoTls};
use zeroship_control::Registry;

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
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
async fn registry_core_tables_live_in_control_schema() {
    let Some(url) = db_url() else {
        eprintln!("[registry_schema_test] CONTROL_TEST_DB/PG_TEST_URL not set - skipping");
        return;
    };
    Registry::new(&url).await.expect("registry");
    let pg = pg(&url).await;

    for table in [
        "apps",
        "usage",
        "usage_history",
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
        // qualifier whenever the schema is on the connection's `search_path`
        // (the deployed default is `control, auth, platform, public`), so a
        // literal `control.apps` comparison is unreliable. Joining
        // `pg_class`→`pg_namespace` reports the schema unconditionally and
        // also asserts the table is not silently shadowed in `public`.
        let qualified = format!("control.{table}");
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
        assert_eq!(schema, "control", "{qualified} should live in the control schema");
    }
}
