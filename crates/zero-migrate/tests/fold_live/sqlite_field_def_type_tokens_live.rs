//! **What SQLite actually stores for the type tokens `col_type_to_token` emits.**
//!
//! `render/lower.rs::col_type_to_token` maps a closed [`ColType`] to the SDK `FieldDef`
//! type token that lands in `LiveSchema::sqlite_schemas`. `schema/query.rs`'s SQLite
//! `column_type` maps that token back to a declared SQL type when the 12-step rebuild
//! re-renders `CREATE TABLE` from the map. The two are a matched pair and nothing
//! checks that they agree.
//!
//! Taking the SET of tokens the first can emit against the SET the second names, the
//! difference is exactly two: `bigInt` and `bytes`. Both fell to `column_type`'s
//! `_ => "TEXT"` arm. The consequence is not a wrong string in a file - it is a wrong
//! COLUMN, because SQLite derives affinity from the declared type by documented rule
//! (a declared type containing "INT" gets INTEGER affinity; "BLOB" gets BLOB; a bare
//! TEXT gets TEXT), and the rebuild copies every existing row through it.
//!
//! So a `renameColumn` on some OTHER column of the table silently rewrote a
//! `t.bigInt()` column to TEXT affinity and converted its stored integers to strings,
//! and a `bytes` column to TEXT affinity, which is the one affinity that does not
//! round-trip arbitrary bytes.
//!
//! # The oracle is the server, not the emitted SQL
//!
//! Every assertion below reads `PRAGMA table_info` for the DECLARED type and
//! `typeof(...)` / `hex(...)` for what the database decided to STORE. No test here
//! inspects the DDL string, and none compares `column_type` against
//! `col_type_to_token` - that pairing agreeing proves only that two functions in this
//! repo agree, not that SQLite agrees with either.
//!
//! Each case asserts the PRE-rebuild state first, out of the same `PRAGMA`. The
//! initial `CREATE TABLE` is rendered by a different emitter (the ordinary lowering
//! path), so the before/after pair isolates the rebuild's re-render as the thing that
//! changes the column - a setup that silently did nothing reports as a setup failure
//! instead of as a defect.
//!
//! # The arm this drives
//!
//! `render_create_table_sqlite_rebuild` has two arms and only the SDK-value one reads
//! the map's CONTENT; the deploy path takes the other. `tests/fold_live/
//! sqlite_rebuild_field_defs_live.rs` records that finding at length. This file uses
//! the same fold-seeded `LiveSchema` it does, for the same reason: on the stored-shape
//! arm SQLite's own `CREATE TABLE` text is replayed and the map is never read, so a
//! test written there would observe nothing about these tokens.
//!
//! # No skip
//!
//! SQLite is an embedded temp file. Every test here always runs for real.

use crate::support;

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::ir::Op;
use zero_migrate::render::fold::single_fold;
use zero_migrate::{
    fold_ops, resolve_create_table_policy, Approval, ExecutorConfig, IrAuthor, LiveSchema,
    MigrationEngine, MigrationIr, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_field_def_type_tokens";
const APP: &str = "app_field_def_type_tokens";

/// Larger than 2^53, so a value that round-trips through a float is provably wrong and
/// a value that round-trips through TEXT is visible as a `typeof` of `text`. This is
/// the whole point of `t.bigInt()` over `t.number()`.
const WIDE: i64 = 9_007_199_254_740_993;

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths() -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join("app.sqlite");
    let journal = dir.path().join("app.migrations.sqlite");
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

async fn exec(backend: &SqliteBackend, sql: &str) -> Result<(), String> {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|error| error.to_string())?;
    backend
        .actor()
        .exec(sql)
        .await
        .map_err(|error| error.to_string())
}

async fn query(backend: &SqliteBackend, sql: &str) -> Vec<Vec<Option<String>>> {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine-journal mode");
    backend
        .actor()
        .query(sql)
        .await
        .unwrap_or_else(|error| panic!("query {sql}: {error}"))
}

/// The SERVER's declared type for every column of `table`, as `(name, declared type)`.
///
/// SQLite stores the declared type verbatim and derives the column's AFFINITY from it
/// by the rules in <https://sqlite.org/datatype3.html#determination_of_column_affinity>.
/// Reading it back out of `PRAGMA table_info` is therefore reading what the database
/// committed to, not what any emitter claimed.
async fn declared_types(backend: &SqliteBackend, table: &str) -> Vec<(String, String)> {
    query(backend, &format!("PRAGMA main.table_info({table})"))
        .await
        .into_iter()
        .map(|row| {
            // cid, name, type, notnull, dflt_value, pk
            (
                row[1].clone().unwrap_or_default(),
                row[2].clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// What SQLite decided to STORE in `column`, per row: the storage class it chose.
///
/// This is the consequence layer. A declared type is a claim; `typeof()` is the
/// database reporting which of NULL/integer/real/text/blob the value actually became
/// after affinity was applied on the way in.
async fn stored_types(backend: &SqliteBackend, table: &str, column: &str) -> Vec<String> {
    query(
        backend,
        &format!("SELECT typeof({column}) FROM main.{table} ORDER BY id"),
    )
    .await
    .into_iter()
    .map(|row| row[0].clone().unwrap_or_default())
    .collect()
}

/// The live schema `engine::refresh_historical_live` builds: table snapshots from
/// `fold_ops`, field maps from the projection. This is the shape that routes a rebuild
/// through the arm that reads the map's content.
fn folded_live_schema(history: &[Op]) -> LiveSchema {
    let policy = support::no_inject(PROJECT);
    let snapshot =
        fold_ops(history, SqlDialect::Sqlite, PROJECT, &policy).expect("the history folds");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = single_fold::fold(history, SqlDialect::Sqlite, PROJECT, &policy)
        .expect("the history folds")
        .project_field_defs();
    live
}

/// Apply one IR through the real lower + executor and return its resolved ops.
async fn apply(backend: &SqliteBackend, source: &str, live: &LiveSchema) -> Vec<Op> {
    let policy = support::no_inject(PROJECT);
    let exec_cfg = ExecutorConfig::new(PROJECT, PROJECT, policy.clone());
    let raw: MigrationIr = serde_json::from_str(source).expect("test IR parses");
    let resolved = resolve_create_table_policy(&raw, &policy, PROJECT).expect("the IR resolves");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &policy);
    let steps = author.lower_steps(&resolved, live).expect("the IR lowers");
    MigrationEngine::new()
        .apply_plan(
            &steps,
            Approval::Approved,
            backend,
            &exec_cfg,
            "field-def-type-tokens",
            LockMode::Acquire,
        )
        .await
        .expect("the plan applies");
    resolved.ops
}

// ---------------------------------------------------------------------------
// The migrations
// ---------------------------------------------------------------------------

/// One table carrying the two tokens in the difference set alongside an `int` column
/// that is already handled, so the `int` column is a live control: it proves the
/// rebuild re-rendered the table at all, and that the fixture is not simply passing
/// everything through untouched.
///
/// `note` exists only to be renamed - the rename is what forces the 12-step rebuild.
const CREATE: &str = r#"{"ir_version":1,"name":"create_ledger","owner_app":"app_field_def_type_tokens","ops":[
  {"op":"createTable","name":"ledger","columns":[{"name":"id","type":"text","nullable":false},{"name":"note","type":"text"},{"name":"tally","type":"int"},{"name":"amount","type":"bigInt"},{"name":"payload","type":"bytes"}],"primaryKey":["id"]}
]}"#;

/// A rename of a column that has nothing to do with the two under test. That is the
/// point: the damage below is collateral.
const RENAME: &str = r#"{"ir_version":1,"name":"rename_note","owner_app":"app_field_def_type_tokens","ops":[
  {"op":"renameColumn","table":"ledger","from":"note","to":"memo","type":"text"}
]}"#;

async fn seed(backend: &SqliteBackend) {
    exec(
        backend,
        &format!(
            "INSERT INTO main.ledger (id, note, tally, amount, payload) VALUES \
             ('row_1', 'alpha', 3, {WIDE}, x'00FF10')"
        ),
    )
    .await
    .expect("insert the row");
}

// ---------------------------------------------------------------------------
// The observations
// ---------------------------------------------------------------------------

/// **A `t.bigInt()` column keeps INTEGER affinity across a rebuild, and its values stay
/// integers.**
///
/// `col_type_to_token` emits the token `bigInt` for [`ColType::BigInt`]. The SQLite
/// `column_type` arm named `bigint`/`int8`/`int4` - SQL spellings - and not the DSL
/// token, so `bigInt` reached `_ => "TEXT"`. TEXT affinity converts an integer to its
/// decimal string on the way in, so the rebuild's `INSERT INTO tmp SELECT … FROM old`
/// rewrote every stored value.
///
/// The value is deliberately past 2^53: `typeof` reporting `text` for it is the exact
/// loss `t.bigInt()` exists to prevent.
#[compio::test]
async fn a_big_int_column_survives_a_rebuild_as_an_integer() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");

    let history = apply(&backend, CREATE, &LiveSchema::default()).await;
    seed(&backend).await;

    // THE ORACLE, before the rebuild: the server's own account of the column.
    let before = declared_types(&backend, "ledger").await;
    assert_eq!(
        before,
        vec![
            ("id".to_string(), "TEXT".to_string()),
            ("note".to_string(), "TEXT".to_string()),
            ("tally".to_string(), "INTEGER".to_string()),
            ("amount".to_string(), "INTEGER".to_string()),
            ("payload".to_string(), "BLOB".to_string()),
        ],
        "the SERVER's pre-rebuild declared types. These are what the ORDINARY \
         createTable emitter wrote - a different code path from the rebuild's \
         re-render - so this line establishes the shape the rebuild is supposed to \
         preserve. If it fails, the fixture never set that shape up and nothing below \
         is about the rebuild."
    );
    assert_eq!(
        stored_types(&backend, "ledger", "amount").await,
        vec!["integer".to_string()],
        "and SQLite stored the wide value as an integer, per BIGINT's INTEGER affinity"
    );
    assert_eq!(
        stored_types(&backend, "ledger", "payload").await,
        vec!["blob".to_string()],
        "and the bytes as a blob"
    );

    // The map's own claim, asserted before the server's, so a difference below is
    // attributable to the EMITTER rather than to the fold.
    let live = folded_live_schema(&history);
    assert!(
        live.table_snapshots["ledger"].stored_create_sql.is_none(),
        "the authored snapshot must carry NO stored CREATE, or the rebuild replays \
         SQLite's own text and this test observes nothing about the map"
    );
    assert_eq!(
        live.sqlite_schemas["ledger"]["amount"].get("type"),
        Some(&serde_json::json!("bigInt")),
        "the folded map spells the token `bigInt`, or the row below is not about the \
         SQLite type emitter"
    );
    assert_eq!(
        live.sqlite_schemas["ledger"]["payload"].get("type"),
        Some(&serde_json::json!("bytes")),
        "and `bytes` for the raw-bytes column"
    );

    apply(&backend, RENAME, &live).await;

    // Nothing about a rename of `note` may change any other column. The expectation is
    // therefore the server's OWN pre-rebuild answer with the one name substituted,
    // rather than a hand-written list that could drift into agreeing with a defect.
    let mut expected = before.clone();
    expected[1].0 = "memo".to_string();
    assert_eq!(
        declared_types(&backend, "ledger").await,
        expected,
        "after a rebuild driven by a rename of an UNRELATED column, every column keeps \
         the declared type it had. `amount` must stay INTEGER-affinity and `payload` \
         BLOB; a `TEXT` on either line is the defect, and the rebuild has just copied \
         real rows into it."
    );
    assert_eq!(
        stored_types(&backend, "ledger", "amount").await,
        vec!["integer".to_string()],
        "and the wide value is STILL an integer. `text` here means the copy converted \
         it to a decimal string - the loss `t.bigInt()` exists to prevent."
    );
    // MEASURED, and narrower than it looks: with `payload` declared TEXT the already
    // stored blob still read back as `blob`, because SQLite's TEXT affinity converts
    // INTEGER and REAL values to text and leaves a BLOB alone. So the `bytes` half of
    // this defect did NOT corrupt the row on the way through the copy the way the
    // `bigInt` half did. What it broke is the column's DECLARED type, which is what
    // every later write, comparison and index build is resolved against - and which
    // the IR's own `ColType::Bytes` doc-comment states is `BLOB` on SQLite. This
    // assertion is kept as the standing guarantee, not as the RED that found the bug;
    // the declared-type assertion above is that.
    assert_eq!(
        stored_types(&backend, "ledger", "payload").await,
        vec!["blob".to_string()],
        "and the raw bytes are still a blob"
    );
    assert_eq!(
        query(
            &backend,
            "SELECT amount, hex(payload), tally FROM main.ledger ORDER BY id"
        )
        .await,
        vec![vec![
            Some(WIDE.to_string()),
            Some("00FF10".to_string()),
            Some("3".to_string()),
        ]],
        "and the values themselves come through the copy byte for byte"
    );
}
