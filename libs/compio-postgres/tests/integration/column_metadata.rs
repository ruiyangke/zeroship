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
use compio_postgres::SimpleQueryMessage;
use compio_postgres::types::Type;
use std::num::NonZeroUsize;

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

async fn connect_client_with_probationary_statements(url: &str) -> Client {
    let mut config: compio_postgres::Config = url.parse().expect("parse the test URL");
    config.statement_cache_capacity(1);
    config.statement_cache_execution_threshold(
        NonZeroUsize::new(2).expect("the test threshold is nonzero"),
    );
    let (client, connection) = config
        .connect(common::suite_tls())
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

async fn create_probe_table(client: &Client, prefix: &str) -> (String, u32) {
    let table = common::test_object_name(prefix);
    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} (id int4); \
             INSERT INTO {table} VALUES (17)"
        ))
        .await
        .expect("create and populate the metadata probe table");

    let oid_row = client
        .query_one("SELECT (($1::text)::regclass)::oid::int8", &[&table])
        .await
        .expect("resolve the probe table's catalog oid");
    let oid = oid_row.get::<_, i64>(0);
    let oid = u32::try_from(oid).expect("PostgreSQL oids fit in u32");
    assert_ne!(
        oid, 0,
        "a live relation never uses the no-relation sentinel"
    );
    (table, oid)
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

/// A raw-string transaction bind describes its unnamed portal in `bind.rs`.
#[compio::test]
async fn bind_unnamed_portal_row_description_preserves_table_oid() {
    let url = test_url();
    let mut client = connect_client_with_probationary_statements(&url).await;
    let (table, expected_oid) = create_probe_table(&client, "cpg_colmeta_bind").await;
    let sql = format!("SELECT id FROM {table}");

    let transaction = client.transaction().await.expect("start a transaction");
    let portal = transaction
        .bind(sql.as_str(), &[])
        .await
        .expect("bind the first, still-unnamed execution");
    let rows = transaction
        .query_portal(&portal, 0)
        .await
        .expect("read the portal");

    assert_eq!(rows.len(), 1, "the probe table carries one row");
    assert_eq!(
        rows[0].columns()[0].table_oid(),
        Some(expected_oid),
        "the unnamed portal's descriptor must retain its source relation"
    );

    drop(portal);
    transaction.rollback().await.expect("roll back the probe");
}

/// `query_text_params` constructs its own statement from a portal descriptor.
#[compio::test]
async fn query_text_params_row_description_preserves_table_oid() {
    let client = connect_client(&test_url()).await;
    let (table, expected_oid) = create_probe_table(&client, "cpg_colmeta_text").await;
    let rows = client
        .query_text_params(&format!("SELECT id FROM {table} WHERE id = $1"), &["17"])
        .await
        .expect("run the text-parameter query");

    assert_eq!(rows.len(), 1, "the parameter must select the probe row");
    assert_eq!(
        rows[0].columns()[0].table_oid(),
        Some(expected_oid),
        "query_text_params must retain its RowDescription's source relation"
    );
}

/// `query_typed` has a separate duplicated RowDescription construction loop.
#[compio::test]
async fn query_typed_row_description_preserves_table_oid() {
    let client = connect_client(&test_url()).await;
    let (table, expected_oid) = create_probe_table(&client, "cpg_colmeta_typed").await;
    let rows = client
        .query_typed(
            &format!("SELECT id FROM {table} WHERE id = $1"),
            &[(&17_i32, Type::INT4)],
        )
        .await
        .expect("run the typed query");

    assert_eq!(rows.len(), 1, "the parameter must select the probe row");
    assert_eq!(
        rows[0].columns()[0].table_oid(),
        Some(expected_oid),
        "query_typed must retain its RowDescription's source relation"
    );
}

/// A row with no columns is a real shape, and `is_empty` is the only accessor
/// that reports it.
///
/// `Row::is_empty` and `SimpleQueryRow::is_empty` are both `self.len() == 0`
/// over two different column stores, and neither is named anywhere in
/// `tests/`. Every other test in this suite selects at least one column, so
/// both would return `false` for every row the suite has ever built and an
/// implementation hardcoding `false` would pass all of them.
///
/// `PostgreSQL` makes the zero-column case reachable: `CREATE TABLE t()` is
/// legal, a `DEFAULT VALUES` insert gives it a row, and selecting it sends a
/// `DataRow` with no fields. The non-empty arm beside it is the one-variable
/// control - same client, same session, differing only in how many columns the
/// query names.
#[compio::test]
async fn a_row_with_no_columns_reports_itself_empty() {
    let url = test_url();
    let client = connect_client(&url).await;
    let table = common::test_object_name("cpg_zero_column");
    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table}(); INSERT INTO {table} DEFAULT VALUES"
        ))
        .await
        .expect("create a zero-column table and give it a row");

    let rows = client
        .query(&format!("SELECT * FROM {table}"), &[])
        .await
        .expect("select the zero-column row");
    assert_eq!(rows.len(), 1, "the zero-column table must yield one row");
    assert!(
        rows[0].is_empty(),
        "a row with no columns must report itself empty"
    );
    assert_eq!(rows[0].len(), 0);
    assert!(rows[0].columns().is_empty());

    // Control: identical client and session, one column instead of none.
    let populated = client
        .query_one("SELECT 1", &[])
        .await
        .expect("select one column");
    assert!(
        !populated.is_empty(),
        "a row with a column must not report itself empty"
    );
    assert_eq!(populated.len(), 1);
    assert_eq!(populated.columns().len(), 1);

    // The simple-query path carries its own row type and its own column store.
    let mut simple_rows = 0;
    for message in client
        .simple_query(&format!("SELECT * FROM {table}"))
        .await
        .expect("simple_query the zero-column row")
    {
        if let SimpleQueryMessage::Row(row) = message {
            simple_rows += 1;
            assert!(
                row.is_empty(),
                "a simple-query row with no columns must report itself empty"
            );
            assert_eq!(row.len(), 0);
            assert!(row.columns().is_empty());
        }
    }
    assert_eq!(simple_rows, 1, "the simple query must yield one row");

    for message in client
        .simple_query("SELECT 1")
        .await
        .expect("simple_query one column")
    {
        if let SimpleQueryMessage::Row(row) = message {
            assert!(
                !row.is_empty(),
                "a simple-query row with a column must not report itself empty"
            );
            assert_eq!(row.len(), 1);
            assert_eq!(row.columns().len(), 1);
        }
    }
}
