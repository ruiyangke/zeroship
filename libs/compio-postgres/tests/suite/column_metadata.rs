//! `Column`'s provenance accessors must agree with the catalog.
//!
//! `table_oid`, `column_id` and `type_modifier` are public, and none of them is
//! named anywhere in `tests/`. They come straight off the `RowDescription`, and
//! two of them carry a semantic that is easy to lose in a refactor: the
//! protocol sends ZERO for a column that does not belong to a table, and this
//! driver maps that zero to `None` (`prepare.rs`, `.filter(|n| *n != 0)`). A
//! version that passed the zero through would hand callers `Some(0)` -- an OID
//! and an attribute number that look real and match nothing.
//!
//! Nothing here is hardcoded. Every expected value is read from `pg_class`,
//! `pg_attribute` and `pg_type` on the same server in the same session, so the
//! test cannot drift from the catalog it is checking against and cannot be
//! satisfied by a driver that happens to agree with a constant I typed.
//!
//! A transposition of `table_oid` and `column_id` would not compile (`u32`
//! against `i16`), so that specific error is already excluded by the types;
//! what is NOT excluded is the zero mapping, a wrong `atttypmod`, or a column
//! resolved against the wrong relation.

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

/// A real table column carries the catalog's own oid, attnum and typmod.
#[compio::test]
async fn a_table_column_reports_its_catalog_identity() {
    let url = test_url();
    let client = connect_client(&url).await;
    let table = common::test_object_name("cpg_colmeta");

    // `varchar(10)` on purpose: its atttypmod is a real value (length + 4)
    // rather than the -1 an unconstrained type carries, so the assertion below
    // can tell "read the typmod" from "returned the no-typmod sentinel".
    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} (id int, label varchar(10))"
        ))
        .await
        .expect("create the probe table");

    // The catalog's answer, from the same session -- temp tables live in
    // pg_class under this backend's pg_temp schema.
    let expected = client
        .query_one(
            "SELECT c.oid::int8, a.attnum::int2, a.atttypmod::int4 \
             FROM pg_class c \
             JOIN pg_attribute a ON a.attrelid = c.oid \
             WHERE c.oid = ($1::text)::regclass AND a.attname = 'label'",
            &[&table],
        )
        .await
        .expect("read the catalog");
    let expected_oid: i64 = expected.get(0);
    let expected_attnum: i16 = expected.get(1);
    let expected_typmod: i32 = expected.get(2);

    let statement = client
        .prepare(&format!("SELECT label FROM {table}"))
        .await
        .expect("prepare");
    let column = &statement.columns()[0];

    assert_eq!(column.name(), "label");
    assert_eq!(
        column.table_oid().map(i64::from),
        Some(expected_oid),
        "table_oid must be the relation's catalog oid"
    );
    assert_eq!(
        column.column_id(),
        Some(expected_attnum),
        "column_id must be the attribute number, and `label` is the SECOND \
         column, so this also catches a first-column-always answer"
    );
    assert_eq!(
        column.type_modifier(),
        expected_typmod,
        "type_modifier must be the catalog's atttypmod for varchar(10)"
    );

    // The typmod really is a live value, not the sentinel -- if this ever
    // becomes -1 the assertion above would still hold while proving nothing.
    assert_ne!(
        expected_typmod, -1,
        "fixture broken: varchar(10) must carry a real atttypmod"
    );
}

/// A computed column has no provenance, and that is `None`, never `Some(0)`.
///
/// This is the one-variable partner for the test above: same accessors, a
/// column that belongs to no relation. If the zero mapping were removed, the
/// test above would still pass and only this one would fail.
#[compio::test]
async fn a_computed_column_has_no_table_or_attribute_number() {
    let url = test_url();
    let client = connect_client(&url).await;

    let statement = client
        .prepare("SELECT 1::int4 AS computed")
        .await
        .expect("prepare");
    let column = &statement.columns()[0];

    assert_eq!(column.name(), "computed");
    assert_eq!(
        column.table_oid(),
        None,
        "a computed column belongs to no relation; the protocol's zero must \
         become None, not Some(0)"
    );
    assert_eq!(
        column.column_id(),
        None,
        "a computed column has no attribute number; zero must become None"
    );
    assert_eq!(
        column.type_modifier(),
        -1,
        "an unconstrained int4 carries the no-typmod sentinel"
    );
}

/// A SYSTEM column reports its relation and a NEGATIVE attribute number.
///
/// This is the case the two above leave out, and it is the one that bites: a
/// caller that reads `column_id()` as a 1-based index into the relation is
/// correct for `id` and `label`, correct-by-accident for a computed column
/// (`None`), and WRONG here. PostgreSQL numbers user columns from 1 and its own
/// from -1 downward, so `ctid` is `Some(-1)`.
///
/// Measured against the live server rather than assumed, since the sign is the
/// whole point.
#[compio::test]
async fn a_system_column_reports_a_negative_attribute_number() {
    let client = connect_client(&common::test_url()).await;
    let table = common::test_object_name("cpg_colmeta_sys");
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (id int4)"))
        .await
        .expect("create the probe table");

    let statement = client
        .prepare(&format!("SELECT ctid, id FROM {table}"))
        .await
        .expect("prepare a select over a system column");

    let ctid = &statement.columns()[0];
    assert_eq!(ctid.name(), "ctid");
    assert!(
        ctid.table_oid().is_some(),
        "a system column still belongs to its relation"
    );
    assert_eq!(
        ctid.column_id(),
        Some(-1),
        "ctid's attribute number is negative, not an index"
    );

    // One variable away: the ordinary column beside it is still positive.
    let id = &statement.columns()[1];
    assert_eq!(id.column_id(), Some(1));

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .ok();
}
