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
        "control.apps",
        "control.usage",
        "control.usage_history",
        "control.app_vars",
        "control.app_secrets",
        "control.app_env_expose",
        "control.creator_accounts",
        "control.creator_account_history",
        "control.payouts",
        "control.app_audit",
    ] {
        let rows = pg
            .query("SELECT to_regclass($1)::text AS rel", &[&table])
            .await
            .unwrap_or_else(|e| panic!("to_regclass({table}): {e}"));
        let rel: Option<String> = rows[0].get("rel");
        assert_eq!(rel.as_deref(), Some(table), "{table} should exist");
    }
}
