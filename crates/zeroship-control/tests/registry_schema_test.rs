//! Live-PG schema regression tests for control-plane registry tables.
//!
//! Configure a test database (`zeroship_core::config::test_database_url_opt`;
//! run `tests/provision_test_backends.sh` to provision one) to run; tests
//! skip otherwise.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_control::Registry;
use zeroship_core::UserId;

use crate::common;

fn db_url() -> String {
    crate::common::require_control_db()
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
        "organization_accounts",
        "organization_account_history",
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
        // crates/zeroship-control/src/registry.rs fully qualifies every reference as
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
        assert_eq!(
            schema, "zeroship",
            "{qualified} should live in the zeroship schema"
        );
    }

    // Teardown: `pg` holds this test's Postgres connection, and locals are
    // dropped only after the body returns - by which point the runtime is gone
    // and the socket can no longer be closed. Drop it explicitly, then wait for
    // the close to land.
    drop(pg);
    common::drain_pg().await;
}

/// There is no app-level API key, and the create path proves it in BOTH
/// directions: it must neither RETURN one to its caller nor STORE one.
///
/// Both halves live in one test because either alone is satisfied by the wrong
/// fix. A response with no key field is what a `#[serde(skip_serializing)]`
/// attribute over a live column produces - that was the state this replaces,
/// and it hid a minted secret rather than not minting one. A table with no key
/// column says nothing about what the handler hands back. Only the pair says
/// the key does not exist.
///
/// The round-trip is the second half's real assertion. A create response that
/// deserializes back into the very type that produced it cannot be withholding
/// a field: withholding is exactly what makes the round-trip fail. Asserting
/// only "no field named api_key" would pass against a struct that kept the
/// secret and renamed the field.
#[compio::test]
async fn create_app_neither_returns_nor_stores_an_app_level_key() {
    let url = db_url();
    let registry = Registry::new(&url).await.expect("registry");
    let pg = pg(&url).await;

    let owner_id = UserId::mint();
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
        &[
            &owner_id.as_str(),
            &format!("nokey-owner-{}@zeroship.test", owner_id.as_str()),
            &"nokey-owner",
        ],
    )
    .await
    .expect("seed owner user");
    zeroship_control::plan_catalog::seed_plans(&registry)
        .await
        .expect("seed built-in plans");

    let name = format!("nokey-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let record = registry
        // `None` project: let the registry mint the owner's personal
        // organization and default project, which is the one-step path a
        // creator's first deploy takes.
        .create_app(
            &name,
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner_id,
            None,
        )
        .await
        .expect("create_app");

    // STORES. Ask the catalog what the create path actually wrote into, not
    // the SQL text of the statement: a column dropped from the INSERT list but
    // left on the table is still a place a key can be put back.
    let columns = pg
        .query(
            "SELECT column_name::text AS column_name \
             FROM information_schema.columns \
             WHERE table_schema = 'zeroship' AND table_name = 'apps'",
            &[],
        )
        .await
        .expect("read zeroship.apps columns");
    let column_names: Vec<String> = columns
        .iter()
        .map(|row| row.get::<_, String>("column_name").to_ascii_lowercase())
        .collect();
    for column in &column_names {
        assert!(
            !column.contains("api_key") && !column.contains("apikey"),
            "zeroship.apps must carry no app-level key column; found {column:?}"
        );
    }
    // CONTROL for the sweep above: an empty or misspelled column list would
    // satisfy it vacuously.
    for expected in ["id", "name", "plan_id"] {
        assert!(
            column_names.iter().any(|column| column == expected),
            "zeroship.apps must still carry {expected:?}"
        );
    }

    // RETURNS. The create-app response body is exactly this serialized record
    // (`create_app_response_body` in crates/zeroship-control/src/api.rs).
    let body = serde_json::to_value(&record).expect("serialize the created record");
    let object = body.as_object().expect("the record is a JSON object");
    for field in object.keys() {
        assert!(
            !field.contains("api_key") && !field.contains("apiKey"),
            "the create-app response must carry no app-level key field; found {field:?}"
        );
    }
    // CONTROL: the identity a caller needs must survive.
    for expected in ["id", "name", "plan_id"] {
        assert!(
            object.contains_key(expected),
            "the create-app response must still carry {expected:?}"
        );
    }
    serde_json::from_value::<zeroship_core::types::AppRecord>(body)
        .expect("the create-app response must round-trip: nothing is withheld from it");

    drop(pg);
    common::drain_pg().await;
}
