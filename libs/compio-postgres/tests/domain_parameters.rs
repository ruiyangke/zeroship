//! A domain-typed parameter binds with its BASE Rust type.
//!
//! PostgreSQL reports the declared parameter type in `Describe`, so for
//! `INSERT INTO t (v) VALUES ($1)` where `v` is a domain, the driver is handed
//! the DOMAIN's oid. `ToSql for i32` accepts only `Type::INT4`, so the bind was
//! refused with `error serializing parameter 0` and the statement could not be
//! executed at all -- while psql does it trivially, because libpq performs no
//! client-side type check.
//!
//! The RESULT direction never had this problem and still must not regress:
//! PostgreSQL reports the BASE type in `RowDescription`, so `SELECT 7::d` comes
//! back as plain `int4`. Only parameters see a domain oid.
//!
//! This is not a theoretical shape. `sdks/migrate` can create domains, and
//! `crates/zeroship-migrate-adapter` already carries a `resolve_domain()`
//! helper for `information_schema`'s `cardinal_number`, `sql_identifier` and
//! `yes_or_no` -- all domains -- because it hit the decode half of the same
//! problem one layer up.
//!
//! The domain's CHECK must keep firing. Unwrapping happens on the CLIENT only,
//! to choose an encoding; the server still applies every constraint, and the
//! second test below is what proves the constraint was not bypassed along with
//! the type check.

use compio_postgres::{Client, NoTls};

#[allow(dead_code)]
mod common;

fn test_url() -> Option<String> {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// Create a domain over `int4` and a temp table using it. Returns both names.
async fn domain_fixture(client: &Client, suffix: &str) -> (String, String) {
    let domain = common::test_object_name(&format!("cpg_domain_{suffix}"));
    let table = common::test_object_name(&format!("cpg_domtbl_{suffix}"));
    client
        .batch_execute(&format!("CREATE DOMAIN {domain} AS int4 CHECK (VALUE > 0)"))
        .await
        .expect("create the domain");
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v {domain})"))
        .await
        .expect("create the probe table");
    (domain, table)
}

/// Binding an `i32` to a domain-over-int4 parameter works.
#[compio::test]
async fn a_domain_parameter_accepts_its_base_rust_type() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;
    let (domain, table) = domain_fixture(&client, "bind").await;

    let statement = client
        .prepare(&format!("INSERT INTO {table} (v) VALUES ($1)"))
        .await
        .expect("prepare against a domain column");

    // The fixture is only meaningful if Describe really reported the domain.
    // Without this the test could pass because the server resolved the type
    // for us, which is exactly what happens on the result side.
    assert!(
        matches!(
            statement.params()[0].kind(),
            compio_postgres::types::Kind::Domain(_)
        ),
        "the parameter must be reported as a domain, or this tests nothing: {:?}",
        statement.params()[0]
    );

    let affected = client
        .execute(&statement, &[&7i32])
        .await
        .expect("an i32 must bind to a domain over int4");
    assert_eq!(affected, 1);

    let stored: i32 = client
        .query_one(&format!("SELECT v FROM {table}"), &[])
        .await
        .expect("read the row back")
        .get(0);
    assert_eq!(stored, 7, "the value must round-trip through the domain");

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .ok();
    client
        .batch_execute(&format!("DROP DOMAIN IF EXISTS {domain}"))
        .await
        .ok();
}

/// The domain's CHECK still fires. Relaxing the client-side type check must not
/// relax the server-side constraint.
///
/// This is the one-variable partner: same statement, same binding path, a value
/// the domain forbids. If the fix had somehow routed around the domain rather
/// than merely choosing an encoding for it, this would insert -1 happily.
#[compio::test]
async fn a_domain_check_constraint_still_rejects_a_bad_value() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;
    let (domain, table) = domain_fixture(&client, "check").await;

    let statement = client
        .prepare(&format!("INSERT INTO {table} (v) VALUES ($1)"))
        .await
        .expect("prepare against a domain column");

    let error = client
        .execute(&statement, &[&-1i32])
        .await
        .expect_err("the domain forbids values <= 0");
    assert_eq!(
        error
            .as_db_error()
            .expect("the refusal must come from the server")
            .code()
            .code(),
        "23514",
        "a domain CHECK violation is SQLSTATE 23514"
    );

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .ok();
    client
        .batch_execute(&format!("DROP DOMAIN IF EXISTS {domain}"))
        .await
        .ok();
}
