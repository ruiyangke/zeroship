// This test target is its own crate ROOT and overflows rustc's layout query.
// `recursion_limit` is per crate root, so the crate's lib and its sibling test
// targets do not cover this one. Caught by `tests/clippy_gate.sh`, which lints
// `--all-targets`; a bare `cargo test -p zeroship-plugin-db --lib` never builds
// this file, which is why it compiled clean until the gate ran.
#![recursion_limit = "256"]

//! The masking storage flip: the filter oracle, and the three silent
//! consequences of closing it.
//!
//! # The defect these tests exist for
//!
//! A masked column used to store PLAINTEXT under the field's own name and the
//! mask in a `<col>_masked` sibling. The projection substituted
//! `"ssn_masked" AS "ssn"`, but the filter builder previously ignored its
//! schema hint and so COULD NOT: `find({ ssn: { $gt: v } })`
//! rendered `WHERE "ssn" > $1` and compared against plaintext. The caller never
//! saw a value and did not need to - the set of matching rows IS the answer, and
//! repeated probes binary-search it, with no authorization check on the path and
//! no audit row written. Reading `__zeroship_audit_unmask` would show nothing
//! unusual while it happened.
//!
//! The fix is not a fence on the filter builder. It is that `ssn` now stores the
//! MASK and `__zs_raw__ssn` stores the real value, so the ignorant path is the
//! safe path: a builder that knows nothing about masking selects and filters the
//! masked column and leaks nothing.
//!
//! # Why these tests build their own tables
//!
//! Every fixture here creates its table with the REAL DDL emitter and writes
//! through the REAL write pipeline. The flip's whole risk is that the emitter
//! and the data plane disagree about which physical column holds what, and a
//! hand-written fixture agrees with whichever one its author had in mind.
//!
//! Requires a live PostgreSQL, named by `PG_TEST_URL` or by the overlay
//! (`deploy/ops/zeroship.test.toml`). Opt-in behind `required-features =
//! ["test-helpers"]`, so an unreachable server FAILS rather than skipping: a
//! skipping run of a security suite is indistinguishable from a passing one.
//!
//! ```text
//! PG_TEST_URL=postgres://... cargo test -p zeroship-plugin-db \
//!   --features test-helpers --test mask_flip -- --test-threads=1
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use compio_postgres::{NoTls, Pool};
use serde_json::{json, Value};
use zeroship_plugin_db::query::{
    build_aggregate, build_create_table_with_fks, build_distinct, build_find_with_schema,
    build_insert, build_where, raw_column_name, read_surface_columns, validate_field_name,
    FkEmission,
};

#[path = "support/mod.rs"]
mod support;

fn test_url() -> String {
    zeroship_core::config::test_database_url()
}

/// Connect, or fail the test.
///
/// Deliberately NOT a skip, for the reason the module doc gives.
async fn require_pg() -> String {
    let url = test_url();
    match compio_postgres::connect(&url, NoTls).await {
        Ok((client, connection)) => {
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            drop(client);
            url
        }
        Err(e) => panic!("the mask-flip suite requires a reachable server at PG_TEST_URL: {e}"),
    }
}

async fn release_pg(pool: Rc<Pool>) {
    drop(pool);
    zeroship_plugin_db::reset_context_for_tests();
    let _ = compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await;
}

/// The schema both oracle fixtures use: one masked column and one unmasked
/// control that differs in exactly one variable (the `mask` block).
fn flip_schema() -> Value {
    json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        // The control. Same type, same nullability, no mask. Every assertion
        // about `ssn` below has a twin about `nickname`, so a fixture that
        // simply returned no rows for everything cannot pass.
        "nickname": { "type": "string" },
    })
}

/// Create `<app>.<collection>` from the DDL the platform actually emits, and
/// install the descriptor entry the deploy would have installed.
async fn fixture(pool: &Rc<Pool>, url: &str, app: &str, collection: &str, schema: &Value) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
        .await
        .unwrap();
    let ddl = build_create_table_with_fks(app, collection, schema, &FkEmission::Inline)
        .expect("the platform's own CREATE TABLE emitter");
    pool.batch_execute(&ddl)
        .await
        .unwrap_or_else(|e| panic!("emitted DDL must apply: {e}\n{ddl}"));
    zeroship_plugin_db::set_postgres_pool_for_tests(Rc::clone(pool), url);
    zeroship_plugin_db::cache_schema_for_tests(app, collection, schema.clone());
}

/// What one write through the pipeline left behind.
///
/// `id` is PLATFORM-ASSIGNED: the write pipeline refuses a document that
/// carries one, so no fixture in this file may choose a row's identity. Every
/// test below learns it from the write that created the row, which makes each
/// assertion on an id a statement that identity round-trips through the
/// platform's own minting rather than a comparison against a literal the
/// fixture and the assertion both made up.
struct Inserted {
    /// The id the platform minted for this row.
    ///
    /// Read off the PREPARED DOCUMENT, before the SQL runs. That is the value
    /// the pipeline itself treated as the row's identity - the stage that binds
    /// per-row derivations to the primary key (encryption's `row_pk` AAD) reads
    /// it from exactly here - and taking it from before the round trip is what
    /// lets `rows[0]["id"] == id` be a real assertion instead of a tautology.
    id: String,
    /// The `RETURNING` row(s) the database handed back.
    rows: Vec<Value>,
}

/// Insert one document through the REAL write pipeline and the REAL insert
/// builder, and return the minted id with the `RETURNING` row.
///
/// This asserted `RETURNING *` and said "this suite is written against it".
/// The write path now names its columns, so the shape this suite is written
/// against is the projection - and the projection is what the assertion pins,
/// because a builder that quietly went back to `*` would put the raw column
/// back in every row below.
async fn insert_through_the_pipeline(
    pool: &Rc<Pool>,
    app: &str,
    collection: &str,
    schema: &Value,
    doc: Value,
) -> Inserted {
    let mut docs = json!([doc]);
    zeroship_plugin_db::crud::prepare_insert_many_docs_for_write(&mut docs, app, collection, None)
        .await
        .expect("write pipeline");
    let id = docs[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the write pipeline must mint an id: {}", docs[0]))
        .to_string();
    let bq = build_insert(app, collection, schema, &docs[0]).expect("insert builder");
    assert!(
        !bq.sql.contains("RETURNING *") && bq.sql.contains(r#"RETURNING "id""#),
        "the write path's shape is a named projection; this suite is written against it: {}",
        bq.sql,
    );
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool
        .query_text_params(&bq.sql, &param_refs)
        .await
        .unwrap_or_else(|e| panic!("insert must apply: {e}\n{}", bq.sql));
    Inserted {
        id,
        rows: rows.iter().map(row_to_json).collect(),
    }
}

/// Every column of a returned row as a JSON string value, keyed by column name.
fn row_to_json(row: &compio_postgres::Row) -> Value {
    let mut map = serde_json::Map::new();
    for (i, column) in row.columns().iter().enumerate() {
        let value: Option<String> = row.try_get(i).unwrap_or(None);
        map.insert(
            column.name().to_string(),
            value.map_or(Value::Null, Value::String),
        );
    }
    Value::Object(map)
}

async fn run_find(pool: &Rc<Pool>, app: &str, filter: &Value, schema: &Value) -> Vec<Value> {
    let bq = build_find_with_schema(
        app, "people", filter, Some(50), None, None, None, schema,
    )
    .expect("find builder");
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = pool.query_text_params(&bq.sql, &param_refs).await.unwrap();
    rows.iter().map(row_to_json).collect()
}

// ---------------------------------------------------------------------------
// 1. The oracle
// ---------------------------------------------------------------------------

/// **THE ORACLE.** A range filter on a masked column must not narrow the
/// plaintext.
///
/// Two rows whose real SSNs sit at opposite ends of the range. A sequence of
/// `$gt` probes sweeps between them. Before the flip the probe at
/// `500-00-0000` returned exactly the high row and nothing else, and repeating
/// the sweep at finer granularity recovers the digits one at a time. After the
/// flip both rows store `***`, every probe compares `'***'` against the probe
/// value, and the two rows fall on the SAME side of every one.
///
/// The assertion is INVARIANCE, not emptiness: no probe may separate the two
/// rows, and the whole sweep must return one constant answer. An implementation
/// that returned nothing at all would satisfy an emptiness assertion perfectly,
/// which is why the same sweep also runs against the unmasked control column,
/// where it MUST separate them.
#[compio::test]
async fn a_range_filter_on_a_masked_column_cannot_narrow_the_plaintext() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    let app = "flip_oracle";
    let schema = flip_schema();
    fixture(&pool, &url, app, "people", &schema).await;

    // The two rows are told apart by the ids the PLATFORM minted for them, not
    // by ids this fixture chose - it may not choose one. `low` is the row whose
    // real SSN sits at the bottom of the range, `high` the one at the top.
    let low = insert_through_the_pipeline(
        &pool,
        app,
        "people",
        &schema,
        json!({ "ssn": "111-11-1111", "nickname": "aaa" }),
    )
    .await
    .id;
    let high = insert_through_the_pipeline(
        &pool,
        app,
        "people",
        &schema,
        json!({ "ssn": "999-99-9999", "nickname": "zzz" }),
    )
    .await
    .id;

    // Control zero: both rows are there. A fixture that inserted nothing would
    // make every arm below vacuous. The set is compared against the two minted
    // ids rather than counted, so this also pins that each write's identity
    // survived the round trip - a row read back under some other id would
    // satisfy a bare count.
    let all = run_find(&pool, app, &json!({}), &schema).await;
    assert_eq!(all.len(), 2, "both rows must be present: {all:?}");
    let mut present: Vec<String> = all
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    present.sort();
    let mut minted = vec![low.clone(), high.clone()];
    minted.sort();
    assert_eq!(
        present, minted,
        "the rows read back must be the two the platform minted: {all:?}",
    );

    let probes = [
        "000-00-0000",
        "111-11-1111",
        "222-22-2222",
        "500-00-0000",
        "888-88-8888",
        "999-99-9999",
    ];

    let mut sweep: Vec<Vec<String>> = Vec::new();
    for probe in probes {
        let rows = run_find(&pool, app, &json!({ "ssn": { "$gt": probe } }), &schema).await;
        let mut ids: Vec<String> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        assert_ne!(
            ids.len(),
            1,
            "probe {probe:?} SEPARATED the two rows. That single bit is the oracle: \
             repeating the sweep recovers the whole SSN, with no authorization check \
             on the path and no audit row written. Matched: {ids:?}",
        );
        sweep.push(ids);
    }
    assert!(
        sweep.windows(2).all(|w| w[0] == w[1]),
        "the probe sweep over a masked column must be constant - it must carry no \
         information about the values at all; got {sweep:?}",
    );

    // THE CONTROL, differing in one variable: the same shape of query over the
    // unmasked `nickname` column MUST separate the rows. Without this arm an
    // implementation that refused every filter, or returned no rows at all,
    // would pass every assertion above.
    let rows = run_find(&pool, app, &json!({ "nickname": { "$gt": "mmm" } }), &schema).await;
    assert_eq!(
        rows.len(),
        1,
        "the unmasked control column must still be range-filterable: {rows:?}",
    );
    // And it selects the RIGHT one: the row inserted second, named by the id
    // the platform minted for it.
    assert_eq!(rows[0]["id"], json!(high));

    // And the ordering channel is closed the same way: `orderBy` on a masked
    // column sorts by the mask, so a `limit 1` cannot name the largest SSN.
    let ordered = {
        let bq = build_find_with_schema(
            app,
            "people",
            &json!({}),
            Some(1),
            None,
            Some(&json!({ "ssn": -1 })),
            None,
            &schema,
        )
        .unwrap();
        assert!(
            !bq.sql.contains(&raw_column_name("ssn")),
            "orderBy must never name the raw column: {}",
            bq.sql,
        );
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        pool.query_text_params(&bq.sql, &param_refs).await.unwrap()
    };
    assert_eq!(ordered.len(), 1, "the ordered query still returns a row");

    release_pg(pool).await;
}

/// The feature is SECURED, not CLOSED: the plaintext is still reachable, by the
/// one path that carries an authorization check and writes an audit row.
///
/// This is the granted-path control for the oracle above. Without it, an
/// implementation that simply destroyed the value on write would satisfy every
/// assertion in this file.
#[compio::test]
async fn the_real_value_is_still_stored_and_still_reachable_by_the_audited_path() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    let app = "flip_reachable";
    let schema = flip_schema();
    fixture(&pool, &url, app, "people", &schema).await;

    // The platform mints the id; the row is addressed by that value from here
    // on. `row_pk` below is the same value, which matters beyond addressing:
    // it is the identity the write pipeline binds per-row derivations to, so a
    // stand-in would not merely miss the row, it would fail to decrypt one.
    let person = insert_through_the_pipeline(
        &pool,
        app,
        "people",
        &schema,
        json!({ "ssn": "123-45-6789", "nickname": "ada" }),
    )
    .await;

    let raw_col = raw_column_name("ssn");
    let stored = pool
        .query_text_params(
            &format!(
                "SELECT \"ssn\" AS mask, \"{raw_col}\" AS raw \
                 FROM \"{app}\".\"people\" WHERE id = $1"
            ),
            &[person.id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        stored.len(),
        1,
        "the minted id must address the row the write created",
    );
    assert_eq!(
        stored[0].get::<_, String>("mask"),
        "***",
        "the field's own column holds the mask",
    );
    assert_eq!(
        stored[0].get::<_, String>("raw"),
        "123-45-6789",
        "the real value is stored, in a column no query surface can name",
    );

    // ---- and the audited path really does recover it, on Postgres ----
    //
    // Without this arm nothing binds `crud::unmask`'s PG SQL to the raw
    // column: pointing it back at the field's own column reddens no other test
    // in this file, because every other assertion here is about what a query
    // CANNOT reach. The unmask path is the one reader that must reach it.
    pool.batch_execute(
        &zeroship_migrate_server::provisioning::audit_unmask_table_sql(app),
    )
    .await
    .expect("the audit table the deploy provisions");
    // The unmask fetch runs `SET LOCAL ROLE app_<id>_role`, so the per-app role
    // and its grants have to exist - the deploy's `zeroship migrate` creates
    // them, and this stands in for it.
    zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app)
        .await
        .expect("per-app role, as the deploy would provision it");
    support::grant_runtime_select_columns(&pool, app, "people", &["id", &raw_col]).await;

    let result = zeroship_plugin_db::crud::unmask::dispatch_unmask(
        &zeroship_plugin_db::binding::DbBinding::cold_start(app),
        zeroship_plugin_db::crud::unmask::UnmaskFieldArgs {
            collection: "people".to_string(),
            row_pk: person.id.clone(),
            column: "ssn".to_string(),
            actor: Some(json!({ "kind": "auto", "id": null })),
            reason: Some("mask_flip integration test".to_string()),
        },
    )
    .await
    .expect("the audited unmask path must still return plaintext");
    assert_eq!(
        result.plaintext, "123-45-6789",
        "unmask must read the RAW column; reading the field's own column would \
         return the mask and the feature would be closed rather than secured",
    );

    // And the audit row the guarantee rests on was written.
    let audit = pool
        .query_text_params(
            &format!(
                "SELECT outcome, \"column\" FROM \"{app}\".\"__zeroship_audit_unmask\""
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(audit.len(), 1, "exactly one audit row");
    assert_eq!(audit[0].get::<_, String>("outcome"), "granted");
    assert_eq!(
        audit[0].get::<_, String>("column"),
        "ssn",
        "the audit row names the LOGICAL field, not the physical column",
    );

    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 2. Outward: the leak the flip would have created
// ---------------------------------------------------------------------------

/// **Outward.** No row-returning write verb may hand back the raw column.
///
/// The twelve write sites in `zeroship-schema` emitted `RETURNING *` - every
/// physical column, never passing through the projection allowlist, which was
/// SELECT-side only. Without the read pipeline's row-surface stage, `insert`
/// returned the real value under a key the generated `Row<S>` type does not
/// declare - invisible to any review written against the generated types, and
/// doubly silent because the mask still came back correctly beside it.
///
/// **Both boundaries are asserted here, in the order a row crosses them.** The
/// projection is first: the write's own SQL no longer names the raw column, so
/// the value never leaves the database. The surface stage is second, and still
/// necessary - the write is not the only producer of a row, and the second half
/// of this test feeds it the kind of row that has no projection in front of it.
///
/// The assertion is on the KEY SET, not on the absence of one name, so a
/// differently-named raw column cannot pass it.
#[compio::test]
async fn no_write_verb_hands_back_a_column_the_descriptor_does_not_declare() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    let app = "flip_returning";
    let schema = flip_schema();
    fixture(&pool, &url, app, "people", &schema).await;

    let Inserted {
        id: minted_id,
        rows: returned,
    } = insert_through_the_pipeline(
        &pool,
        app,
        "people",
        &schema,
        json!({ "ssn": "123-45-6789", "nickname": "ada" }),
    )
    .await;
    assert_eq!(returned.len(), 1);
    // BOUNDARY 1, the database's. The row PostgreSQL sent back does not contain
    // the raw column at all - not "contains it and we removed it".
    //
    // This assertion is the inverse of the one it replaced, which required the
    // raw column to be present "or this test proves nothing". That was true
    // while the write starred; the value it was guarding against is now
    // unreachable one layer earlier.
    assert!(
        returned[0].get(raw_column_name("ssn").as_str()).is_none(),
        "the write's projection must not name the raw column: {:?}",
        returned[0],
    );
    // Paired with the control that the row is a real row and not an empty one -
    // otherwise "no raw column" is satisfied by returning nothing. The id is
    // the one the pipeline minted BEFORE the statement ran, so this arm is also
    // the projection's round trip: `RETURNING "id"` hands back the identity the
    // write assigned.
    assert_eq!(returned[0]["id"], json!(minted_id));
    assert!(
        returned[0].get("ssn").is_some(),
        "the masked column must still come back: {:?}",
        returned[0],
    );
    // And the raw column IS in the table - so the assertion above is about the
    // projection, not about a write that failed to store the value.
    let stored = pool
        .query_text_params(
            &format!(
                r#"SELECT {} AS raw FROM "{app}"."people" WHERE "id" = $1"#,
                zeroship_plugin_db::query::quote_ident(&raw_column_name("ssn")),
            ),
            &[minted_id.as_str()],
        )
        .await
        .expect("read the raw column directly");
    assert_eq!(stored.len(), 1, "the row must exist");

    // BOUNDARY 2, the runtime's.
    let allowed: BTreeSet<String> = read_surface_columns(&schema);
    let finalized = zeroship_plugin_db::crud::finalize_rows_on_read_for_tests(
        app,
        "people",
        returned.clone(),
    )
    .await
    .expect("read pipeline");
    let keys: BTreeSet<String> = finalized[0].as_object().unwrap().keys().cloned().collect();
    assert!(
        keys.is_subset(&allowed),
        "a write's returned row carried columns the descriptor does not declare: {:?}",
        keys.difference(&allowed).collect::<Vec<_>>(),
    );
    // And the VALUE is gone, not merely re-keyed.
    let serialized = serde_json::to_string(&finalized[0]).unwrap();
    assert!(
        !serialized.contains("123-45-6789"),
        "the real value must not cross the JS boundary from a write: {serialized}",
    );
    // Paired with what must still come back, so this is not a green from
    // returning an empty row.
    assert_eq!(finalized[0]["ssn"]["masked"], json!("***"));
    assert_eq!(finalized[0]["nickname"], json!("ada"));
    assert_eq!(finalized[0]["id"], json!(minted_id));

    // ---- and the arm that binds the SURFACE stage specifically ----
    //
    // The arm above no longer exercises the surface stage AT ALL: the row it
    // finalizes came out of a named projection, so there is nothing off-surface
    // in it to remove. That is the point of the projection, and it is also why
    // this arm has to build its own row.
    //
    // A row with off-surface columns still reaches the pipeline from the
    // producer no projection sits in front of: the WAL consumer decodes
    // pgoutput with no schema in reach. This is that row's shape - the raw
    // column carrying the real value, plus columns the descriptor does not
    // declare at all (an auxiliary shadow-table key, a column added to the
    // table out of band). Nothing else on the read path removes an unknown key:
    // the only other key removal in the pipeline is the mask pass's, and it
    // removes exactly one name it derives itself.
    let mut smuggled = returned[0].clone();
    smuggled[raw_column_name("ssn")] = json!("123-45-6789");
    smuggled["__zs_shadow_key"] = json!("aux-42");
    smuggled["totally_undeclared"] = json!("leak-me");
    let finalized = zeroship_plugin_db::crud::finalize_rows_on_read_for_tests(
        app,
        "people",
        vec![smuggled],
    )
    .await
    .expect("read pipeline");
    let keys: BTreeSet<String> = finalized[0].as_object().unwrap().keys().cloned().collect();
    assert!(
        keys.is_subset(&allowed),
        "an undeclared physical column reached the JS boundary: {:?}",
        keys.difference(&allowed).collect::<Vec<_>>(),
    );
    assert!(
        !serde_json::to_string(&finalized[0]).unwrap().contains("leak-me"),
        "and neither did its value: {finalized:?}",
    );

    release_pg(pool).await;
}

// ---------------------------------------------------------------------------
// 3. Inward: the raw column is unnameable
// ---------------------------------------------------------------------------

/// **Inward.** The raw column is refused on every inbound surface.
///
/// Not because a fence was added to each of them - because the column is named
/// something `validate_field_name` already refused before the flip, and every
/// inbound surface already calls it. That is a stronger property than a fence:
/// a fence can be forgotten on a surface nobody has written yet.
///
/// The name is derived from `raw_column_name`, never spelled as a literal, so a
/// rename keeps the test pointed at the real column.
#[test]
fn the_raw_column_is_refused_on_every_inbound_surface() {
    let raw = raw_column_name("ssn");
    let schema = flip_schema();

    let refusals: Vec<(&str, bool)> = vec![
        (
            "filter key",
            build_where(
                &json!({ raw.clone(): "x" }),
                &mut Vec::new(),
                &schema,
            )
            .is_err(),
        ),
        (
            // The aggregate matcher and `$group.by` below both validate
            // against the same declared shape.
            "aggregate $match",
            build_aggregate(
                "app1",
                "people",
                &json!([{ "$match": { raw.clone(): "x" } }]),
                &schema,
            )
            .is_err(),
        ),
        (
            "select",
            build_find_with_schema(
                "app1",
                "people",
                &json!({}),
                Some(1),
                None,
                None,
                Some(&json!([raw.clone()])),
                &schema,
            )
            .is_err(),
        ),
        (
            "orderBy",
            build_find_with_schema(
                "app1",
                "people",
                &json!({}),
                Some(1),
                None,
                Some(&json!({ raw.clone(): 1 })),
                None,
                &schema,
            )
            .is_err(),
        ),
        (
            "$group.by",
            build_aggregate(
                "app1",
                "people",
                &json!([{ "$group": { "by": [raw.clone()] } }]),
                &schema,
            )
            .is_err(),
        ),
        (
            "distinct",
            build_distinct("app1", "people", &raw, &json!({}), &schema).is_err(),
        ),
        (
            // The exact function the write pipeline's document-key and
            // update-patch-key fences call, on every key including `$set`
            // nesting.
            "write document key",
            validate_field_name(&raw).is_err(),
        ),
    ];
    for (surface, refused) in &refusals {
        assert!(refused, "{surface} accepted the raw column {raw:?}");
    }
    assert_eq!(refusals.len(), 7, "seven inbound surfaces ruled on");

    // The control: the LOGICAL name is ACCEPTED on those same surfaces. Without
    // it, a validator that refused everything would pass all seven above.
    assert!(build_where(&json!({ "ssn": "x" }), &mut Vec::new(), &schema).is_ok());
    assert!(build_distinct("app1", "people", "ssn", &json!({}), &schema).is_ok());
    assert!(validate_field_name("ssn").is_ok());
    assert!(build_find_with_schema(
        "app1",
        "people",
        &json!({}),
        Some(1),
        None,
        Some(&json!({ "ssn": 1 })),
        Some(&json!(["ssn"])),
        &schema,
    )
    .is_ok());
}

// ---------------------------------------------------------------------------
// 4. Live-query subscriptions
// ---------------------------------------------------------------------------

/// **Live-query subscriptions.** A masked-column predicate must keep firing.
///
/// The WAL tuple carries the mask under the field's own name, so an unlowered
/// `find({ssn: "123-45-6789"})` would compare a plaintext operand against a
/// stored mask, never match, and the subscription would stop firing with no
/// error anywhere - the failure mode `read_set`'s own doc calls unacceptable.
///
/// Equality is lowered (the operand is masked the same way the stored value
/// is); a range is dropped to coarse-grained, because a range over a mask is not
/// a range over the value and no rewriting makes it one. Coarse-grained
/// over-delivers, which is the bias `read_set` already declares.
#[test]
fn a_masked_predicate_is_lowered_for_the_change_stream() {
    use zeroship_plugin_db::read_set::{normalise_filter, Predicate, PredicateOp};
    let schema = flip_schema();

    let Some(Predicate::All(conjuncts)) =
        normalise_filter(&json!({ "ssn": "123-45-6789" }), &schema)
    else {
        panic!("an equality predicate on a masked column must stay fine-grained");
    };
    assert_eq!(conjuncts.len(), 1);
    assert_eq!(conjuncts[0].column, "ssn");
    assert_eq!(conjuncts[0].op, PredicateOp::Eq);
    assert_eq!(
        conjuncts[0].value,
        json!("***"),
        "the operand must be masked the same way the stored value is, or the \
         predicate silently never matches",
    );

    assert!(
        normalise_filter(&json!({ "ssn": { "$gt": "500-00-0000" } }), &schema).is_none(),
        "a range over a mask must fall back to coarse-grained rather than \
         comparing masks as if they were values",
    );

    // The control: the unmasked column keeps BOTH shapes fine-grained and its
    // operand untouched. Without this arm an implementation that returned
    // `None` for everything would pass the range assertion.
    let Some(Predicate::All(conjuncts)) = normalise_filter(&json!({ "nickname": "ada" }), &schema)
    else {
        panic!("an unmasked equality predicate must stay fine-grained");
    };
    assert_eq!(conjuncts[0].value, json!("ada"));
    assert!(normalise_filter(&json!({ "nickname": { "$gt": "m" } }), &schema).is_some());
}

// ---------------------------------------------------------------------------
// 5. The DDL half - the piece that touches real column data
// ---------------------------------------------------------------------------

/// **The type-and-constraint swap, verified against a real server.**
///
/// The differ emits nothing for the flip: the column-additions branch is
/// name-only and the `RewriteColumnType` arm keys strictly off the `encrypted`
/// toggle, and the flip moves neither the name nor that toggle. So the emitted
/// CREATE TABLE is hand-authored with nothing verifying it. This test is that
/// verification - it applies the emitted DDL to a real server, reads the
/// server's own catalog back, and then writes through the real pipeline.
///
/// The specific thing it catches: `.mask()` is legal on string, number and
/// bytes, and every mask kind returns a String. Leaving the declared type and
/// constraints on the field's own column makes `'***'` a hard error under
/// `DOUBLE PRECISION` and a CHECK violation under an enum - every write to the
/// collection fails.
#[compio::test]
async fn the_declared_type_and_constraints_travel_to_the_raw_column() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    let app = "flip_ddl";
    let schema = json!({
        "score": {
            "type": "number",
            "required": true,
            "mask": { "kind": "full", "classification": "pii" }
        },
        "tier": {
            "type": "string",
            "enum": ["gold", "silver"],
            "required": true,
            "mask": { "kind": "full", "classification": "pii" }
        },
        "plain": { "type": "number" },
    });
    fixture(&pool, &url, app, "accounts", &schema).await;

    // The server's own catalog, not the emitted string.
    let cols = pool
        .query_text_params(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = 'accounts' ORDER BY column_name",
            &[app],
        )
        .await
        .unwrap();
    let types: BTreeMap<String, String> = cols
        .iter()
        .map(|r| {
            (
                r.get::<_, String>("column_name"),
                r.get::<_, String>("data_type"),
            )
        })
        .collect();
    assert_eq!(
        types.get("score").map(String::as_str),
        Some("text"),
        "the masked column must be TEXT so it can hold '***': {types:?}",
    );
    assert_eq!(
        types.get(&raw_column_name("score")).map(String::as_str),
        Some("double precision"),
        "the declared numeric type belongs to the value: {types:?}",
    );
    // The control: an unmasked numeric column keeps its type under its own
    // name, so this is not a green from making everything text.
    assert_eq!(
        types.get("plain").map(String::as_str),
        Some("double precision"),
        "an unmasked column is untouched by the flip: {types:?}",
    );

    // And it accepts a write. This is what a mistake in the swap breaks.
    let account = insert_through_the_pipeline(
        &pool,
        app,
        "accounts",
        &schema,
        json!({ "score": 42.5, "tier": "gold", "plain": 7.0 }),
    )
    .await;

    let stored = pool
        .query_text_params(
            &format!(
                // `::text` on the raw score because it is a real `float8` on
                // the server - which is the point of the test.
                "SELECT \"score\" AS mask, \"{}\"::text AS raw, \"tier\" AS tier_mask, \
                 \"{}\" AS tier_raw FROM \"{app}\".\"accounts\" WHERE id = $1",
                raw_column_name("score"),
                raw_column_name("tier"),
            ),
            &[account.id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        stored.len(),
        1,
        "the minted id must address the row the write created",
    );
    assert_eq!(stored[0].get::<_, String>("mask"), "***");
    assert_eq!(stored[0].get::<_, String>("raw"), "42.5");
    assert_eq!(stored[0].get::<_, String>("tier_mask"), "***");
    assert_eq!(stored[0].get::<_, String>("tier_raw"), "gold");

    release_pg(pool).await;
}

/// **Constraints follow the real value, and that is not an optimisation.**
///
/// `.unique()` on a masked field is semantically about the real value. Left on
/// the masked column it would enforce uniqueness over MASKS, where many rows
/// legitimately share `***-**-1234` - and for `kind: "full"` every mask is
/// `***`, so the table would cap at ONE ROW and the failure would present as a
/// duplicate-key error on perfectly valid data.
#[compio::test]
async fn a_unique_masked_field_admits_rows_that_share_a_mask() {
    let url = require_pg().await;
    let pool = Rc::new(Pool::connect(&url, 4).await.unwrap());
    let app = "flip_unique";
    let schema = json!({
        "ssn": {
            "type": "string",
            "unique": true,
            "mask": { "kind": "last4", "classification": "pci" }
        },
    });
    fixture(&pool, &url, app, "people", &schema).await;

    // `build_create_indexes` emits CONCURRENTLY, which cannot run inside the
    // implicit transaction `batch_execute` uses, so the fixture's DDL carries
    // the table alone. Apply the index the platform would build.
    for spec in zeroship_plugin_db::query::build_create_indexes(app, "people", &schema).unwrap() {
        pool.execute(&spec.sql.replace("CONCURRENTLY ", ""), &[])
            .await
            .unwrap_or_else(|e| panic!("index must build: {e}\n{}", spec.sql));
    }

    // Two rows whose real values differ but whose masks are identical. Neither
    // names its own id - the platform mints one per row, which is what makes
    // them two rows rather than one overwritten one.
    let first = insert_through_the_pipeline(
        &pool,
        app,
        "people",
        &schema,
        json!({ "ssn": "111-11-1234" }),
    )
    .await;
    let second = insert_through_the_pipeline(
        &pool,
        app,
        "people",
        &schema,
        json!({ "ssn": "999-99-1234" }),
    )
    .await;

    let count = pool
        .query_text_params(
            &format!("SELECT count(*)::text AS n FROM \"{app}\".\"people\""),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        count[0].get::<_, String>("n"),
        "2",
        "two rows sharing the mask ***-**-1234 must both insert",
    );
    // ...and they are the two the writes created, addressed by the ids the
    // platform minted for them.
    let named = pool
        .query_text_params(
            &format!(
                "SELECT count(*)::text AS n FROM \"{app}\".\"people\" \
                 WHERE \"id\" IN ($1, $2)"
            ),
            &[first.id.as_str(), second.id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        named[0].get::<_, String>("n"),
        "2",
        "both minted ids must address a stored row",
    );

    // The control: uniqueness over the REAL value is still enforced, so this is
    // not a green from dropping the constraint.
    // A third row whose REAL value collides with the first. Its id is minted
    // like every other, so the only thing that can be refused below is the
    // duplicate value on the raw column.
    let mut docs = json!([{ "ssn": "111-11-1234" }]);
    zeroship_plugin_db::crud::prepare_insert_many_docs_for_write(&mut docs, app, "people", None)
        .await
        .expect("write pipeline");
    let bq = build_insert(app, "people", &schema, &docs[0]).unwrap();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let err = pool
        .query_text_params(&bq.sql, &param_refs)
        .await
        .expect_err("a duplicate REAL value must still be refused");
    assert!(
        format!("{err:?}").contains("23505") || format!("{err:?}").contains("unique"),
        "expected a unique violation on the raw column, got {err:?}",
    );

    release_pg(pool).await;
}
