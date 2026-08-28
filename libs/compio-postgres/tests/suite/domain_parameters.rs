//! A domain-typed parameter binds with its BASE Rust type.
//!
//! PostgreSQL reports the declared parameter type in `Describe`, so for
//! `INSERT INTO t (v) VALUES ($1)` where `v` is a domain, the driver is handed
//! the DOMAIN's oid. `ToSql for i32` accepts only `Type::INT4`, so the bind was
//! refused with `error serializing parameter 0` and the statement could not be
//! executed at all -- while psql does it trivially, because libpq performs no
//! client-side type check.
//!
//! The RESULT direction is problem-free ONLY FOR A SCALAR domain, and this
//! header used to claim it was problem-free full stop. PostgreSQL reports the
//! BASE type in `RowDescription` for `SELECT 7::d`, which is why that always
//! worked - but for `d[]` it reports the domain ARRAY, so reading failed in
//! exactly the way binding did. Both are covered below.
//!
//! This is not a theoretical shape. `sdks/migrate` can create domains, and
//! `crates/zeroship-migrate-server/src/session.rs` already carries a `resolve_domain()`
//! helper for `information_schema`'s `cardinal_number`, `sql_identifier` and
//! `yes_or_no` -- all domains -- because it hit the decode half of the same
//! problem one layer up.
//!
//! The domain's CHECK must keep firing. Unwrapping happens on the CLIENT only,
//! to choose an encoding; the server still applies every constraint, and the
//! second test below is what proves the constraint was not bypassed along with
//! the type check.

use compio_postgres::Client;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
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
    let url = test_url();
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
    let url = test_url();
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

/// A binary COPY into a domain column accepts the base Rust type too.
///
/// `BinaryCopyInWriter` takes its column types from the CALLER, and the natural
/// way to obtain them is from the catalog or from a prepared statement's
/// `params()` -- both of which hand back the DOMAIN, not its base. So the same
/// rejection that blocked `execute` blocked binary COPY, on a path where the
/// driver cannot see a server-reported type to fall back on.
///
/// This shares `encode_parameter` with the bind path rather than repeating the
/// fallback, so the two cannot drift.
#[compio::test]
async fn a_binary_copy_into_a_domain_column_accepts_its_base_type() {
    use compio_postgres::binary_copy::BinaryCopyInWriter;
    use futures_util::pin_mut;

    let url = test_url();
    let client = connect_client(&url).await;
    let (domain, table) = domain_fixture(&client, "copy").await;

    // Obtain the column type the way a caller would: from the statement the
    // server described. This is what yields the domain rather than int4.
    let statement = client
        .prepare(&format!("INSERT INTO {table} (v) VALUES ($1)"))
        .await
        .expect("prepare against a domain column");
    let column_type = statement.params()[0].clone();
    assert!(
        matches!(column_type.kind(), compio_postgres::types::Kind::Domain(_)),
        "the fixture must supply a domain type, or this tests nothing"
    );

    let sink = client
        .copy_in(&format!("COPY {table} (v) FROM STDIN BINARY"))
        .await
        .expect("start a binary COPY");
    let writer = BinaryCopyInWriter::new(sink, &[column_type]);
    pin_mut!(writer);
    writer
        .as_mut()
        .write(&[&11i32])
        .await
        .expect("an i32 must encode for a domain over int4");
    let rows = writer.finish().await.expect("finish the COPY");
    assert_eq!(rows, 1);

    let stored: i32 = client
        .query_one(&format!("SELECT v FROM {table}"), &[])
        .await
        .expect("read the copied row")
        .get(0);
    assert_eq!(stored, 11);

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .ok();
    client
        .batch_execute(&format!("DROP DOMAIN IF EXISTS {domain}"))
        .await
        .ok();
}

/// An ARRAY OF DOMAIN binds and decodes with the base element type.
///
/// `d[]` is reported as `Kind::Array(Domain(base))`, so the scalar unwrap never
/// reaches the domain - the OUTER kind is `Array` - and `Vec<i32>` was refused
/// in BOTH directions against a column it can perfectly well fill.
///
/// This is also the case that corrects the header above: the result direction
/// is NOT problem-free. `RowDescription` reports the base for a scalar domain,
/// which is why `SELECT 7::d` always worked, but for `d[]` it reports the
/// domain ARRAY, so reading failed exactly as binding did. Measured before the
/// fix, both with the same message: "cannot convert between the Rust type
/// `alloc::vec::Vec<i32>` and the Postgres type `_d`".
#[compio::test]
async fn a_domain_array_binds_and_reads_with_its_base_element() {
    let url = test_url();
    let client = connect_client(&url).await;
    let domain = common::test_object_name("cpg_domarr_d");
    let table = common::test_object_name("cpg_domarr_t");
    client
        .batch_execute(&format!("CREATE DOMAIN {domain} AS int4 CHECK (VALUE > 0)"))
        .await
        .expect("create the domain");
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v {domain}[])"))
        .await
        .expect("create the probe table");

    let statement = client
        .prepare(&format!("INSERT INTO {table} (v) VALUES ($1)"))
        .await
        .expect("prepare against a domain-array column");
    // The fixture is only meaningful if the parameter really is an array whose
    // ELEMENT is a domain; otherwise this tests nothing.
    assert!(
        matches!(
            statement.params()[0].kind(),
            compio_postgres::types::Kind::Array(element)
                if matches!(element.kind(), compio_postgres::types::Kind::Domain(_))
        ),
        "the parameter must be an array of domain: {:?}",
        statement.params()[0]
    );

    let affected = client
        .execute(&statement, &[&vec![1i32, 2i32]])
        .await
        .expect("a Vec<i32> must bind to an array of domain over int4");
    assert_eq!(affected, 1);

    let stored: Vec<i32> = client
        .query_one(&format!("SELECT v FROM {table}"), &[])
        .await
        .expect("read the row back")
        .get(0);
    assert_eq!(stored, vec![1, 2], "the array must round-trip");

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .ok();
    client
        .batch_execute(&format!("DROP DOMAIN IF EXISTS {domain}"))
        .await
        .ok();
}

/// THE CONTROL: every element still passes the domain's CHECK.
///
/// Substituting the element type is a CLIENT-side relaxation of what `accepts`
/// will look at. If it had instead routed around the domain, this would insert
/// -5 happily. It does not: SQLSTATE 23514, per element, from the server.
#[compio::test]
async fn a_domain_array_still_enforces_the_element_check() {
    let url = test_url();
    let client = connect_client(&url).await;
    let domain = common::test_object_name("cpg_domarr_cd");
    let table = common::test_object_name("cpg_domarr_ct");
    client
        .batch_execute(&format!("CREATE DOMAIN {domain} AS int4 CHECK (VALUE > 0)"))
        .await
        .expect("create the domain");
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v {domain}[])"))
        .await
        .expect("create the probe table");

    let statement = client
        .prepare(&format!("INSERT INTO {table} (v) VALUES ($1)"))
        .await
        .expect("prepare against a domain-array column");
    let error = client
        .execute(&statement, &[&vec![1i32, -5i32]])
        .await
        .expect_err("the domain forbids elements <= 0");
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
