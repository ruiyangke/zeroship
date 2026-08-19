//! **What a `t.decimal(p, s)` column is worth after a SQLite 12-step rebuild.**
//!
//! Two carriers describe the same column and they do not agree.
//!
//! * `render/lower.rs::author_type_override` maps `ColType::Decimal { .. }` on SQLite to
//!   `data_type = "text"` / `ddl_type = "TEXT"`, with the reason written next to it:
//!   "NUMERIC/REAL affinity converts a sufficiently wide decimal string through a binary
//!   float, so retain authored decimal text byte-for-byte." That override rides in the
//!   catalog SNAPSHOT.
//! * `render/lower.rs::col_type_to_token` maps `ColType::Decimal { .. }` AND
//!   `ColType::Double` to the same token, `"number"`, discarding the precision and scale
//!   because the `FieldDef` vocabulary has no slot for them. That token rides in the
//!   per-table field-def MAP (`LiveSchema::sqlite_schemas`), and `schema/query.rs`'s
//!   SQLite `column_type` answers `REAL` for it.
//!
//! The 12-step rebuild picks ONE of those carriers per rebuild
//! (`render_create_table_sqlite_rebuild` has three arms) and copies every existing row
//! into whatever shape the winner rendered. So the disagreement is not a cosmetic one:
//! on the arm the map wins, a column the snapshot declared TEXT is re-declared REAL, and
//! `INSERT INTO tmp SELECT … FROM old` pushes every stored decimal string through a
//! binary double on the way in.
//!
//! # The value
//!
//! [`WIDE`] is 18 significant digits. It is exactly representable as a decimal and NOT
//! as an IEEE-754 double, which is the entire reason `t.decimal(20, 4)` exists rather
//! than `t.number()`. If the rebuild routes it through REAL affinity the digits are gone
//! and no later migration can recover them.
//!
//! # The oracle is the server
//!
//! Every assertion reads `PRAGMA table_info` for the DECLARED type and
//! `typeof(...)` / the value itself for what SQLite decided to STORE. Nothing here
//! compares `column_type` against `col_type_to_token`: those two agreeing would prove
//! only that two functions in this repo agree, which is precisely the circle this defect
//! lives inside.
//!
//! Each case asserts the PRE-rebuild state first, out of the same `PRAGMA`. The initial
//! `CREATE TABLE` is rendered by the ordinary lowering path, which reads the SNAPSHOT
//! carrier, so the before/after pair isolates the rebuild's re-render as the thing that
//! changes the column. A fixture that silently set nothing up reports as a setup failure
//! rather than as a defect.
//!
//! # Which carrier decides, measured rather than assumed
//!
//! `preserve_stored_shape = pure_rename.is_some() && dt.stored_create_sql.is_some()`,
//! and the three cases below are three different answers to it:
//!
//! * [`a_decimal_column_keeps_its_digits_through_a_fold_seeded_rebuild`] takes the
//!   `stored_create_sql.is_none()` route - the shape `engine::refresh_historical_live`
//!   builds when it re-derives live state from the fold, and the shape the six
//!   `*_sqlite.rs` fold suites drive. THE FIELD-DEF CARRIER DECIDES, and this is where
//!   the digits were measured going away.
//! * [`an_unchanged_decimal_table_does_not_phantom_diff_into_a_rebuild`] never gets as
//!   far as a rebuild arm: it shows the shipped
//!   [`zero_migrate::MigrationEngine::plan_declarative`] AUTHORING a destructive
//!   rebuild of a table nobody changed, because the live `text` it introspects and the
//!   `real` it derives from the field-def carrier canonicalise apart.
//! * [`the_deploy_path_rename_replays_the_stored_create_and_leaves_the_decimal_alone`]
//!   holds BOTH conjuncts and so takes the stored-shape arm. THE SNAPSHOT CARRIER
//!   DECIDES, and it was already right. That case was green before the fix; it is
//!   pinned as the tripwire for a future change that flips a conjunct, not as evidence
//!   of a bug.
//!
//! A stand-alone `setColumnNotNull` was tried as a fourth route onto the map-reading arm
//! and is NOT one: SQLite refuses the op before lowering ("SQLite has no ALTER COLUMN"),
//! so it never reaches a rebuild at all.
//!
//! # No skip
//!
//! SQLite is an embedded temp file. Every test here always runs for real.

use crate::support;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::ir::Op;
use zero_migrate::render::fold::single_fold;
use zero_migrate::{
    desired_snapshot_for_dialect, fold_ops, resolve_create_table_policy, Approval,
    CollectionDescriptor, DeclarativeAuthor, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, MigrationIr, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_decimal_rebuild";
const APP: &str = "app_decimal_rebuild";

/// A `decimal(20, 4)` value with 18 significant digits: exactly representable as a
/// decimal, NOT as an IEEE-754 double. The nearest double is `12345678901234.567383…`,
/// so a value that has been through REAL affinity reads back with different digits and a
/// `typeof` of `real`. This is the whole point of `t.decimal()` over `t.number()`.
const WIDE: &str = "12345678901234.5678";

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

fn registry(tables: &[&str]) -> BTreeMap<String, String> {
    tables
        .iter()
        .map(|table| ((*table).to_string(), APP.to_string()))
        .collect()
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
/// SQLite stores the declared type verbatim and derives the column's AFFINITY from it by
/// the rules in <https://sqlite.org/datatype3.html#determination_of_column_affinity>: a
/// declared type containing "REAL" gets REAL affinity, a bare "TEXT" gets TEXT affinity.
/// Reading it back out of `PRAGMA table_info` is reading what the database committed to,
/// not what any emitter claimed.
async fn declared_types(backend: &SqliteBackend, table: &str) -> Vec<(String, String)> {
    query(backend, &format!("PRAGMA main.table_info({table})"))
        .await
        .into_iter()
        // cid, name, type, notnull, dflt_value, pk
        .map(|row| {
            (
                row[1].clone().unwrap_or_default(),
                row[2].clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// What SQLite decided to STORE in `column`: the storage class, and the value as the
/// database itself renders it back.
///
/// This is the consequence layer. A declared type is a claim; `typeof()` is the database
/// reporting which of NULL/integer/real/text/blob the value became after affinity was
/// applied on the way in, and the value beside it is the digits that survived.
async fn stored(backend: &SqliteBackend, table: &str, column: &str) -> Vec<(String, String)> {
    query(
        backend,
        &format!("SELECT typeof({column}), CAST({column} AS TEXT) FROM main.{table} ORDER BY id"),
    )
    .await
    .into_iter()
    .map(|row| {
        (
            row[0].clone().unwrap_or_default(),
            row[1].clone().unwrap_or_default(),
        )
    })
    .collect()
}

/// The live schema `engine::refresh_historical_live` builds: table snapshots from
/// `fold_ops`, field maps from the projection, no `stored_create_sql`.
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
            "decimal-rebuild",
            LockMode::Acquire,
        )
        .await
        .expect("the plan applies");
    resolved.ops
}

/// Deploy an ordered envelope set through the shipped engine - the same entry point the
/// CLI uses, and the only place that seeds `live.sqlite_schemas` from the fold while the
/// live snapshot itself is genuinely INTROSPECTED.
async fn deploy(backend: &SqliteBackend, tables: &[&str], sources: &[&str]) -> Result<(), String> {
    let policy = support::no_inject(PROJECT);
    let exec_cfg = ExecutorConfig::new(PROJECT, PROJECT, policy.clone());
    let envelopes: Vec<MigrationIr> = sources
        .iter()
        .map(|source| serde_json::from_str(source).expect("test envelope parses"))
        .collect();
    MigrationEngine::new()
        .deploy_envelopes(
            &envelopes,
            backend,
            &policy,
            SqlDialect::Sqlite,
            PROJECT,
            APP,
            &registry(tables),
            Approval::Approved,
            &exec_cfg,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

// ---------------------------------------------------------------------------
// The migrations
// ---------------------------------------------------------------------------

/// A `decimal(20, 4)` column declared DIRECTLY (`amount`) and one declared through a
/// NAMED DOMAIN over the same type (`fee`), beside a `text` column that exists only to
/// be disturbed and an `int` column as a live control. The control proves the rebuild
/// re-rendered the table at all rather than passing everything through untouched.
///
/// The two decimals reach the descriptor by DIFFERENT routes and that is why both are
/// here: `amount` through `ir_column_to_field` reading `ColType::Decimal`, `fee` through
/// `lift_named_domain_base_type` resolving `ColType::Domain` to its base and re-deriving
/// the storage facets from it. Only the second route runs
/// `apply_col_type_to_field_descriptor`, so a fix applied to one and not the other is a
/// column that still loses its digits.
const CREATE: &str = r#"{"ir_version":1,"name":"create_ledger","owner_app":"app_decimal_rebuild","ops":[
  {"op":"createDomain","name":"money_t","as":{"decimal":{"precision":20,"scale":4}}},
  {"op":"createTable","name":"ledger","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"note","type":"text"},
    {"name":"tally","type":"int"},
    {"name":"amount","type":{"decimal":{"precision":20,"scale":4}},"nullable":true},
    {"name":"fee","type":{"domain":{"name":"money_t"}},"nullable":true}
  ],"primaryKey":["id"]}
]}"#;

/// A rename of a column that has nothing to do with the decimal. That is the point: the
/// damage is collateral.
const RENAME: &str = r#"{"ir_version":1,"name":"rename_note","owner_app":"app_decimal_rebuild","ops":[
  {"op":"renameColumn","table":"ledger","from":"note","to":"memo","type":"text"}
]}"#;

async fn seed(backend: &SqliteBackend) {
    exec(
        backend,
        &format!(
            "INSERT INTO main.ledger (id, note, tally, amount, fee) VALUES \
             ('row_1', 'alpha', 3, '{WIDE}', '{WIDE}')"
        ),
    )
    .await
    .expect("insert the row");
}

/// The pre-rebuild shape the ORDINARY `createTable` emitter wrote, read out of the
/// server. `amount` is TEXT because that emitter reads the SNAPSHOT carrier, where
/// `author_type_override` put it. This is the shape a rebuild is supposed to preserve.
fn created_shape() -> Vec<(String, String)> {
    vec![
        ("id".to_string(), "TEXT".to_string()),
        ("note".to_string(), "TEXT".to_string()),
        ("tally".to_string(), "INTEGER".to_string()),
        ("amount".to_string(), "TEXT".to_string()),
        ("fee".to_string(), "TEXT".to_string()),
    ]
}

/// The row as the server reports it before any rebuild: exact decimal text.
fn seeded_row() -> Vec<(String, String)> {
    vec![("text".to_string(), WIDE.to_string())]
}

// ---------------------------------------------------------------------------
// The observations
// ---------------------------------------------------------------------------

/// **A `t.decimal(20, 4)` column keeps TEXT affinity, and every digit, across a rebuild
/// driven from the fold-seeded live schema.**
///
/// This is the arm that reads the field-def map's CONTENT. `col_type_to_token` hands it
/// the token `"number"` - the same token `ColType::Double` gets - and the SQLite
/// `column_type` arm answers `REAL` for `"number"`. REAL affinity converts the stored
/// decimal string to a binary double on the way through
/// `INSERT INTO tmp SELECT … FROM old`, which is a lossy data migration over real rows.
#[compio::test]
async fn a_decimal_column_keeps_its_digits_through_a_fold_seeded_rebuild() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");

    let history = apply(&backend, CREATE, &LiveSchema::default()).await;
    seed(&backend).await;

    // THE ORACLE, before the rebuild.
    let before = declared_types(&backend, "ledger").await;
    assert_eq!(
        before,
        created_shape(),
        "the SERVER's pre-rebuild declared types. `amount` is TEXT because the ordinary \
         createTable emitter reads the SNAPSHOT carrier, where `author_type_override` \
         put `text`/`TEXT` with the no-binary-float reason written beside it. If this \
         line fails the fixture never set that shape up and nothing below is about the \
         rebuild."
    );
    assert_eq!(
        stored(&backend, "ledger", "amount").await,
        seeded_row(),
        "and SQLite stored the wide decimal as exact text, digit for digit"
    );
    assert_eq!(
        stored(&backend, "ledger", "fee").await,
        seeded_row(),
        "and the same for the one declared through the domain"
    );

    // The map's own claim, asserted before the server's, so a difference below is
    // attributable to the EMITTER rather than to the fold.
    let live = folded_live_schema(&history);
    assert!(
        live.table_snapshots["ledger"].stored_create_sql.is_none(),
        "the fold-seeded snapshot must carry NO stored CREATE, or the rebuild replays \
         SQLite's own text and this test observes nothing about the map"
    );
    assert_eq!(
        live.sqlite_schemas["ledger"]["amount"].get("type"),
        Some(&serde_json::json!("number")),
        "the folded map spells the decimal column `number` - the SAME token \
         `ColType::Double` gets, with the precision and scale discarded. That collision \
         is the defect under test; if this line changes, re-read what the fix did."
    );
    assert_eq!(
        live.sqlite_schemas["ledger"]["fee"].get("type"),
        Some(&serde_json::json!("number")),
        "and the domain column collapses to the SAME token: the domain lift resolves \
         `money_t` to its `Decimal` base and then spells it through the same map"
    );

    apply(&backend, RENAME, &live).await;

    // Nothing about a rename of `note` may change any other column, so the expectation
    // is the server's OWN pre-rebuild answer with the one name substituted - not a
    // hand-written list that could drift into agreeing with a defect.
    // The VALUE first, because it is the irreversible half: a declared type can be
    // corrected by a later migration, digits that went through a binary double cannot.
    assert_eq!(
        stored(&backend, "ledger", "amount").await,
        seeded_row(),
        "the wide decimal is STILL exact text after the rebuild's row copy. A `real` \
         storage class here means the copy pushed it through a binary double: \
         12345678901234.5678 is not representable as an IEEE-754 double, so those \
         digits are gone and no later migration can recover them."
    );
    assert_eq!(
        stored(&backend, "ledger", "fee").await,
        seeded_row(),
        "and so is the one that reached the descriptor through the DOMAIN lift. This is \
         a second code path to the same facet - `apply_col_type_to_field_descriptor` \
         rather than `ir_column_to_field` directly - and a fix applied to one and not \
         the other loses this column's digits while `amount` keeps its own."
    );
    let mut expected = before.clone();
    expected[1].0 = "memo".to_string();
    assert_eq!(
        declared_types(&backend, "ledger").await,
        expected,
        "and after a rebuild driven by a rename of an UNRELATED column, every column \
         keeps the declared type it had. A `REAL` on the `amount` line is the defect, \
         and the rebuild has just copied real rows into it."
    );
    assert_eq!(
        query(&backend, "SELECT tally FROM main.ledger ORDER BY id").await,
        vec![vec![Some("3".to_string())]],
        "and the control column came through the copy untouched"
    );
}

/// **The declarative differ, looking at a `t.numeric(20, 4)` table nobody changed, must
/// not author a destructive rebuild of it.**
///
/// The second consequence of the collision, and the one that reaches the shipped
/// [`zero_migrate::MigrationEngine::plan_declarative`] without any rebuild having to
/// happen first.
///
/// The differ compares two type spellings through `sqlite_canonical_type`: the LIVE one
/// it introspects out of the running database, and the DESIRED one it derives from the
/// descriptor set. For this column the live spelling is `text` - the ordinary
/// `createTable` wrote `TEXT`, from the snapshot carrier. The desired spelling comes
/// from `def_to_column_type_for_dialect` over the FIELD-DEF carrier, so while that
/// carrier could only say `number`, the desired spelling was `double precision`, whose
/// canonical affinity is `real`.
///
/// `text` != `real` is a same-name column TYPE change, one of
/// `sqlite_existing_table_needs_rebuild`'s triggers. So an idle re-deploy of an
/// UNCHANGED schema authored a 12-step rebuild of the table: destructive,
/// approval-gated, and copying every row into a `REAL` column - which is the digit loss
/// [`a_decimal_column_keeps_its_digits_through_a_fold_seeded_rebuild`] measures. The two
/// halves compound. The collision both INVENTS the rebuild and then corrupts the rows
/// that rebuild copies.
///
/// Nothing here is a string comparison against a prediction: the live side is
/// `SqliteBackend::snapshot_schema` reading the database the migration actually created,
/// and the claim under test is the differ's own answer about it.
#[compio::test]
async fn an_unchanged_decimal_table_does_not_phantom_diff_into_a_rebuild() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");
    let policy = support::no_inject(PROJECT);
    let exec_cfg = ExecutorConfig::new(PROJECT, PROJECT, policy.clone());

    let history = apply(&backend, CREATE, &LiveSchema::default()).await;
    seed(&backend).await;

    // The SERVER's account of the table, out of the database the migration just wrote.
    // This is the live side of the diff below, introspected rather than asserted into
    // existence.
    assert_eq!(
        declared_types(&backend, "ledger").await,
        created_shape(),
        "the live table the differ is about to read. If this fails the fixture never \
         created the shape and the diff below is about nothing."
    );

    let live = backend
        .snapshot_schema(&exec_cfg)
        .await
        .expect("the live SQLite schema introspects");
    let ownership: HashMap<String, String> = live
        .tables
        .keys()
        .map(|table| (table.clone(), APP.to_string()))
        .collect();

    // The desired side: the SAME history, folded to the descriptor set a re-deploy of
    // the unchanged schema presents. Nothing about the schema differs between the two
    // sides - only the carrier each is spelled through.
    let descriptors: Vec<CollectionDescriptor> =
        single_fold::fold(&history, SqlDialect::Sqlite, PROJECT, &policy)
            .expect("the history folds")
            .project_collection_descriptors()
            .into_values()
            // The projection stamps a synthetic `__fold__` owner; a re-deploy presents
            // the descriptors under the app that owns them, and the differ's
            // cross-app guard refuses a structural change to a table it does not.
            .map(|descriptor| CollectionDescriptor {
                owner_app: APP.to_string(),
                ..descriptor
            })
            .collect();
    let desired = desired_snapshot_for_dialect(PROJECT, &descriptors, SqlDialect::Sqlite, &policy)
        .expect("the descriptor set resolves to a desired snapshot");

    let plan = MigrationEngine::new()
        .plan_declarative(
            &desired,
            &live,
            &ownership,
            &DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite),
            &[],
            &GuardConfig::from_policy(policy.clone(), SqlDialect::Sqlite),
            &policy,
        )
        .expect("the declarative plan is authored");

    assert_eq!(
        plan.rebuilds
            .iter()
            .map(|rebuild| rebuild.spec.reason.clone())
            .collect::<Vec<_>>(),
        Vec::<String>::new(),
        "an UNCHANGED schema must author no 12-step rebuild. A reason naming `amount` \
         here is the phantom: the live column is TEXT and the desired one was derived \
         from a field-def carrier that could not say `decimal`, so the differ saw a \
         type change on a column nobody touched - and the rebuild it authors is the \
         destructive copy that loses the digits."
    );
    assert!(
        plan.plain.items.is_empty() && plan.renames.is_empty(),
        "and no other migration either: {:?} / {} renames",
        plan.plain
            .items
            .iter()
            .map(|item| item.migration.name.clone())
            .collect::<Vec<_>>(),
        plan.renames.len()
    );
}

/// **Which carrier decides on the shipped deploy path - PINNED, not fixed.**
///
/// This case was green before the fix and is green after it, and that is the finding.
/// `preserve_stored_shape = pure_rename.is_some() && dt.stored_create_sql.is_some()`,
/// and on [`zero_migrate::MigrationEngine::deploy_envelopes`] BOTH conjuncts hold for a
/// `renameColumn`: the live snapshot is introspected out of the running database so it
/// carries `stored_create_sql`, and `sqlite_rename_rebuild` builds its desired snapshot
/// by CLONING that live one, so the rename is pure by construction. The rebuild
/// therefore replays SQLite's OWN `CREATE TABLE` text and never reads the field-def
/// map's content.
///
/// So the deploy path was not the route by which the collision destroyed data: the
/// SNAPSHOT carrier decides there, and it was already right. Recording that is half the
/// answer to "which carrier decides for a real deploy"; the other half is the two cases
/// above, which reach the field-def carrier through routes that do NOT hold both
/// conjuncts.
///
/// Keep this pinned. If a future change flips either conjunct off, the deploy path
/// starts reading the map and this test is the tripwire.
#[compio::test]
async fn the_deploy_path_rename_replays_the_stored_create_and_leaves_the_decimal_alone() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");

    deploy(&backend, &["ledger"], &[CREATE])
        .await
        .expect("the createTable envelope deploys");
    seed(&backend).await;

    let before = declared_types(&backend, "ledger").await;
    assert_eq!(
        before,
        created_shape(),
        "the SERVER's pre-rebuild declared types on the DEPLOY path"
    );

    // The WHOLE ordered envelope set, the way the CLI re-presents it on every deploy:
    // `deploy_envelopes` seeds `live.sqlite_schemas` from the CUMULATIVE ops, so the
    // already-applied `CREATE` has to be in view or the rename fails closed with
    // `SqliteRenameNeedsLiveTable`. Its steps are journaled, so it is skipped, not
    // re-applied.
    deploy(&backend, &["ledger"], &[CREATE, RENAME])
        .await
        .expect("the renameColumn envelope deploys");

    let mut expected = before.clone();
    expected[1].0 = "memo".to_string();
    assert_eq!(
        declared_types(&backend, "ledger").await,
        expected,
        "the rename replayed the stored CREATE, so every other column is byte-identical"
    );
    assert_eq!(
        stored(&backend, "ledger", "amount").await,
        seeded_row(),
        "and the decimal is untouched - as it already was before the fix"
    );
}
