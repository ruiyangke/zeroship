//! **The RENAME CARRIER SWEEP, measured against real SQLite.**
//!
//! The SQLite leg of `rename_carrier_sweep_pg`, sharing the same inventory
//! (`support::carriers`) so both dialects are checked against ONE enumeration of what a
//! column name can hide in.
//!
//! ## The reason a separate leg exists at all
//!
//! A SQLite column rename is not PostgreSQL's. PostgreSQL renames the attribute in
//! place and every carrier follows for free, because the catalog holds each of them as
//! attribute NUMBERS. SQLite either rewrites its own stored `CREATE TABLE` text
//! (`ALTER TABLE ... RENAME COLUMN`) or REBUILDS the table through the 12-step
//! procedure - and the rebuild renders its new table FROM A SNAPSHOT. That is what
//! turns a stale carrier from a comparison artifact into a MIGRATION FAILURE here: the
//! rebuild's leading statement is the `CREATE TABLE`, so a body naming a dead column is
//! refused before any value copy, inside the transaction.
//!
//! ## The carrier set is SMALLER here, and that is measured rather than assumed
//!
//! Six of the sixteen carriers are unreachable on SQLite, each for a reason the engine
//! states out loud, and [`SQLITE_UNREACHABLE_CARRIER_FIELDS`] pins the list from BOTH
//! sides - a carrier that becomes reachable fails this test just as loudly as one that
//! goes stale. Without that second direction the exclusions would be a way to make the
//! sweep pass by shrinking it.
//!
//! No database gate is needed: SQLite is an embedded temp file, so this leg always
//! runs for real.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use support::carriers::{carriers_of_schema, REQUIRED_CARRIER_FIELDS};
use tempfile::TempDir;
use zero_migrate::{
    apply::executor::LockMode, fold_ops, model::ir::Op, resolve_create_table_policy, Approval,
    ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr, SchemaSnapshot, SqlDialect,
    SqliteBackend,
};

const PROJECT: &str = "prj_carrier_sweep";
const APP: &str = "app_carrier_sweep";

const TABLE: &str = "carrier_sweep_main";

/// The pre-rename column name, carrying every STRUCTURAL and every RENDERED-SQL carrier
/// SQLite can reach.
const OLD_COLUMN: &str = "qty_on_hand";

/// The post-rename column name.
const NEW_COLUMN: &str = "amount_on_hand";

/// The second renamed column: a TypeID-formatted TEXT column, which is the shape that
/// makes the fold emit an inline format CHECK naming its own column. It is the only
/// producer of `inline_checks`, so without it that carrier would sit unexercised.
const OLD_FORMAT_COLUMN: &str = "sku_code";

/// The post-rename name of [`OLD_FORMAT_COLUMN`].
const NEW_FORMAT_COLUMN: &str = "article_code";

/// Every rename this fixture performs, as `(before, after)`.
const RENAMES: &[(&str, &str)] = &[
    (OLD_COLUMN, NEW_COLUMN),
    (OLD_FORMAT_COLUMN, NEW_FORMAT_COLUMN),
];

/// The carriers the PostgreSQL leg sweeps and this one CANNOT, each with the engine
/// refusal that puts it out of reach. Asserted in BOTH directions below, which is the
/// point: a carrier that BECOMES reachable fails this test until it is swept, so the
/// list cannot be used to make the sweep pass by shrinking it. It already earned that -
/// `definition (PRIMARY KEY)` was on this list on the first run and the reachability
/// direction threw it back off.
///
/// * `definition (UNIQUE)` / `definition (FOREIGN KEY)` - the SQLite emitter does not
///   thread a table-level UNIQUE or FK off a `createTable`, and a stand-alone
///   `addConstraint` is `SqliteRebuildOnly`, so neither kind reaches the fold's SQLite
///   constraint bucket.
/// * `definition (CHECK)` and `cascade_columns` - a table-level CHECK is refused
///   outright ("createTable table-level CHECK is PostgreSQL-only"), and it is the only
///   kind that records `cascade_columns`, so the refusal takes both.
/// * `include` - "createIndex BRIN/INCLUDE/WITH/ONLY features are unsupported on
///   SQLite".
/// * `partition_by.columns` - "partitioned tables are PostgreSQL-only".
const SQLITE_UNREACHABLE_CARRIER_FIELDS: &[&str] = &[
    "TableSnapshot::constraints[].cascade_columns",
    "TableSnapshot::constraints[].definition (CHECK)",
    "TableSnapshot::constraints[].definition (FOREIGN KEY)",
    "TableSnapshot::constraints[].definition (UNIQUE)",
    "TableSnapshot::indexes[].include",
    "TableSnapshot::partition_by.columns",
];

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zm-{tag}.sqlite"));
    let journal = dir.path().join(format!("zm-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

/// The executor's policy is scoped to the PROJECT SCHEMA, while the authoring policy
/// below is the confined charter (scoped to the app). They are deliberately different
/// objects for different questions - the executor's decides whether an existence probe
/// may touch a schema, the author's decides what the app may write - and conflating
/// them makes the run refuse itself with `ExistenceGuardSchemaOutOfScope`.
fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT))
}

/// The CREATE half. Everything SQLite can carry: a generated column, a value-format
/// column (the inline CHECK), a plain index, a partial index (a rendered predicate) and
/// an expression-keyed index (a rendered expression).
const CREATE_IR: &str = r#"{
  "ir_version": 1,
  "name": "carrier_sweep_sqlite_create",
  "owner_app": "app_carrier_sweep",
  "ops": [
    {"op":"createTable","name":"carrier_sweep_main","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false},
      {"name":"note","type":"text","nullable":true},
      {"name":"total_cents","type":"int","nullable":true,
       "generated":{"expr":{"node":"binOp","op":"add",
         "lhs":{"node":"colRef","name":"qty_on_hand"},
         "rhs":{"node":"literal","value":1}},"stored":true}},
      {"name":"sku_code","type":"text","nullable":true,
       "valueFormat":{"typeId":{"prefix":"sku"}}}
    ],"primaryKey":["id","qty_on_hand"]},

    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_plain",
     "columns":[{"kind":"column","name":"qty_on_hand"}],"unique":false},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_partial",
     "columns":[{"kind":"column","name":"note"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty_on_hand"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}}}],
     "unique":false}
  ]
}"#;

/// The first rename, applied through the engine's own `renameColumn` - which on SQLite
/// is LIVE-RESOLVED and lowers to the 12-step rebuild, so this really is the rebuild
/// path and not a synthetic shortcut.
///
/// Two renames of the SAME table cannot share a document: the SQLite lower refuses with
/// `SqliteRepeatRenameTarget`, because each rename is a whole-table rebuild and stacking
/// two of them in one plan would have the second rebuild derive its shape from a table
/// the first has already replaced. So the fixture deploys them as two migrations, which
/// is also the more honest shape - it is what an operator would actually run.
const RENAME_IR: &str = r#"{
  "ir_version": 1,
  "name": "carrier_sweep_sqlite_rename",
  "owner_app": "app_carrier_sweep",
  "ops": [
    {"op":"renameColumn","table":"carrier_sweep_main","from":"qty_on_hand",
     "to":"amount_on_hand","type":"int"}
  ]
}"#;

/// The second rename, in its own migration for the reason [`RENAME_IR`] records.
const RENAME_FORMAT_IR: &str = r#"{
  "ir_version": 1,
  "name": "carrier_sweep_sqlite_rename_format",
  "owner_app": "app_carrier_sweep",
  "ops": [
    {"op":"renameColumn","table":"carrier_sweep_main","from":"sku_code",
     "to":"article_code","type":"text"}
  ]
}"#;

/// Apply one IR doc through the REAL SQLite pipeline and return its ordered ops so the
/// caller accumulates the stream the fold replays.
/// The SQLite live schema `engine::refresh_historical_live` builds: table snapshots from
/// `fold_ops`, SDK field maps from `fold_to_field_defs`, over the ops applied so far.
///
/// A SQLite `renameColumn` is LIVE-RESOLVED and needs the live COLUMN, not just the
/// table name, so `LiveSchema::from_tables` is not enough (`RenameNeedsLiveColumn`), and
/// `from_catalog_snapshot` alone leaves `sqlite_schemas` empty, which is the map the
/// 12-step rebuild renders its new `CREATE TABLE` from. This is the same pair
/// `rename_column_indexed_sqlite` assembles, and the leg with NO `stored_create_sql` -
/// so `preserve_stored_shape` is OFF and the rebuild really does render from the folded
/// snapshot, which is exactly the consumer this sweep is about.
fn folded_live_schema(history: &[Op]) -> LiveSchema {
    let effective = support::no_inject(APP);
    let snapshot =
        fold_ops(history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let sqlite_schemas =
        zero_migrate::fold_to_field_defs(history, SqlDialect::Sqlite, PROJECT, &effective)
            .expect("the history folds to field defs");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = sqlite_schemas;
    live
}

async fn apply_doc(
    be: &SqliteBackend,
    ir: &str,
    registry: &BTreeMap<String, String>,
    live: LiveSchema,
) -> Vec<Op> {
    let raw: MigrationIr = serde_json::from_str(ir).expect("test IR parses");
    let resolved = resolve_create_table_policy(&raw, &support::no_inject(APP), PROJECT)
        .expect("test IR resolves");
    let ir = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::no_inject(APP));
    let document = zero_migrate::model::load::load_ir_document(
        &ir,
        APP,
        zero_migrate::model::validate::Dialect::Sqlite,
        registry,
        None,
    )
    .expect("load gate (sqlite)");
    let ops = document.ops.clone();
    let plan = author
        .lower_plan(&document, &live)
        .expect("lower the doc plan on SQLite");
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            Approval::Approved,
            be,
            &exec_cfg(),
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored plan on SQLite");
    ops
}

/// What the run measured: the folded projection, and the live catalog after the real
/// rebuild.
struct Measured {
    folded: SchemaSnapshot,
    live: SchemaSnapshot,
}

/// Create everything through the real engine against a real temp-file SQLite, rename
/// both columns through the engine's own `renameColumn` (the 12-step rebuild), then
/// introspect and fold the same op stream offline.
async fn measure(tag: &str) -> Measured {
    let paths = paths(tag);
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open sqlite backend");

    let mut ops: Vec<Op> = Vec::new();
    ops.extend(apply_doc(&backend, CREATE_IR, &BTreeMap::new(), LiveSchema::default()).await);
    // A rename doc touches a table that already EXISTS, so the load gate needs the
    // ownership registry to answer "who owns it" with something other than
    // `<unregistered>`, and the lower needs the live COLUMNS. The create doc needs
    // neither: it is the op that establishes both.
    let registry: BTreeMap<String, String> = BTreeMap::from([(TABLE.to_string(), APP.to_string())]);
    let live = folded_live_schema(&ops);
    ops.extend(apply_doc(&backend, RENAME_IR, &registry, live).await);
    let live = folded_live_schema(&ops);
    ops.extend(apply_doc(&backend, RENAME_FORMAT_IR, &registry, live).await);

    let live = backend
        .snapshot_schema_sqlite()
        .await
        .expect("introspect live SQLite schema");
    let folded = fold_ops(&ops, SqlDialect::Sqlite, PROJECT, &support::no_inject(APP))
        .expect("fold the SQLite op stream offline");

    Measured { folded, live }
}

/// Which carrier fields actually HELD one of the pre-rename column names, by path.
///
/// Deliberately stronger than "is not empty". A carrier populated with some OTHER
/// column - a primary key over `id` alone, say - satisfies both the coverage check and
/// the sweep without any rename ever having touched it, which is a green that proves
/// nothing. Run over the NEVER-RENAMED baseline fold, this answers the question that
/// matters: did the fixture put a renamed column INTO this carrier in the first place?
fn carriers_holding_a_renamed_column(snapshot: &SchemaSnapshot) -> BTreeSet<&'static str> {
    carriers_of_schema(snapshot)
        .into_iter()
        .filter(|carrier| {
            carrier
                .spellings
                .iter()
                .any(|spelling| RENAMES.iter().any(|(from, _)| spelling.contains(from)))
        })
        .map(|carrier| carrier.field)
        .collect()
}

/// Fold the CREATE half ALONE - the same history with no rename applied. This is the
/// baseline [`carriers_holding_a_renamed_column`] interrogates.
fn baseline_fold() -> SchemaSnapshot {
    let raw: MigrationIr = serde_json::from_str(CREATE_IR).expect("the create IR parses");
    let resolved = resolve_create_table_policy(&raw, &support::no_inject(APP), PROJECT)
        .expect("the create IR resolves");
    fold_ops(
        &resolved.ops,
        SqlDialect::Sqlite,
        PROJECT,
        &support::no_inject(APP),
    )
    .expect("the create-only history folds")
}

/// The gate that keeps the SQLite exclusion list from being a way to shrink the sweep.
/// Every carrier is either SWEPT here or listed as unreachable, the two sets are
/// disjoint, and their union is the whole inventory - so a carrier that becomes
/// reachable on SQLite fails this test until it is swept, and one that stops being
/// reachable fails it until the reason is written down.
///
/// Coverage is measured on the NEVER-RENAMED baseline and demands that each swept
/// carrier HELD a to-be-renamed column, not merely that it was non-empty. That is the
/// difference between "this fixture built a primary key" and "this fixture built a
/// primary key the rename has to move", and only the second makes the sweep's green
/// mean anything.
#[compio::test]
async fn is_the_sqlite_carrier_set_exactly_the_inventory_minus_its_stated_refusals() {
    let held = carriers_holding_a_renamed_column(&baseline_fold());

    let required: BTreeSet<&str> = REQUIRED_CARRIER_FIELDS.iter().copied().collect();
    let unreachable: BTreeSet<&str> = SQLITE_UNREACHABLE_CARRIER_FIELDS.iter().copied().collect();

    let stray: Vec<&&str> = unreachable.difference(&required).collect();
    assert!(
        stray.is_empty(),
        "`SQLITE_UNREACHABLE_CARRIER_FIELDS` names paths that are not carriers at all: \
         {stray:?}"
    );

    let expected: BTreeSet<&str> = required.difference(&unreachable).copied().collect();

    let missed: Vec<&&str> = expected.difference(&held).collect();
    assert!(
        missed.is_empty(),
        "before any rename, these carriers do not hold a to-be-renamed column, so the \
         sweep below proves nothing about them: {missed:?}. Either extend `CREATE_IR` so \
         each one names {RENAMES:?}, or - if the engine refuses that shape on SQLite - \
         add it to `SQLITE_UNREACHABLE_CARRIER_FIELDS` with the refusal that puts it out \
         of reach"
    );

    let newly_reachable: Vec<&&str> = unreachable
        .iter()
        .filter(|field| held.contains(**field))
        .collect();
    assert!(
        newly_reachable.is_empty(),
        "these carriers are listed as UNREACHABLE on SQLite but the fold populated them \
         with a renamed column: {newly_reachable:?}. The refusal that excused them no \
         longer holds, so they need sweeping rather than excusing - an exclusion list is \
         a testable claim, not a licence"
    );
}

/// The sweep. After a rename applied through the REAL 12-step rebuild, no carrier of the
/// folded snapshot may still spell a pre-rename column name.
#[compio::test]
async fn does_every_folded_carrier_follow_a_column_rename_on_sqlite() {
    let measured = measure("sweep").await;

    let mut stale: Vec<String> = Vec::new();
    for carrier in carriers_of_schema(&measured.folded) {
        for spelling in &carrier.spellings {
            for (from, to) in RENAMES {
                if spelling.contains(from) {
                    stale.push(format!(
                        "  {}: {spelling}  (still names `{from}`, not `{to}`)",
                        carrier.field
                    ));
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "these folded carriers still spell a PRE-rename column name after the SQLite \
         rebuild:\n{}\n\nOn SQLite this is not a quiet inconsistency: \
         `render_create_table_sqlite_rebuild` renders the new table FROM THIS SNAPSHOT \
         and the `CREATE TABLE` leads the statement spec, so a body naming a dead column \
         fails the migration at its first statement",
        stale.join("\n")
    );
}

/// The oracle. SQLite really did move every carrier it reports, so the sweep above is
/// comparing against a moved target rather than passing on an empty one.
#[compio::test]
async fn does_live_sqlite_follow_the_rename_into_every_carrier_it_reports() {
    let measured = measure("oracle").await;

    let mut stale: Vec<String> = Vec::new();
    let mut unseen: Vec<&str> = Vec::new();
    for (from, to) in RENAMES {
        let mut saw_new_name = false;
        for carrier in carriers_of_schema(&measured.live) {
            for spelling in &carrier.spellings {
                if spelling.contains(from) {
                    stale.push(format!("  {}: {spelling}", carrier.field));
                }
                if spelling.contains(to) {
                    saw_new_name = true;
                }
            }
        }
        if !saw_new_name {
            unseen.push(to);
        }
    }
    assert!(
        stale.is_empty(),
        "live SQLite still reports a PRE-rename column name in these carriers:\n{}",
        stale.join("\n")
    );
    assert!(
        unseen.is_empty(),
        "no live SQLite carrier mentions {unseen:?}, so the introspection read something \
         other than the rebuilt table"
    );
}

/// The anti-corruption witness for the one rendered-SQL carrier SQLite does reach with a
/// string literal in it. A TypeID format CHECK is a membership/regex predicate over its
/// own column, and it is dense with literals; the rename must move the REFERENCE and
/// leave every literal byte-identical.
#[compio::test]
async fn does_the_sqlite_inline_check_keep_its_literals_while_its_reference_moves() {
    let measured = measure("literals").await;

    let column = measured
        .folded
        .tables
        .get(TABLE)
        .and_then(|table| {
            table
                .columns
                .iter()
                .find(|column| column.name == NEW_FORMAT_COLUMN)
        })
        .expect("the renamed value-format column survives the fold");
    assert!(
        !column.inline_checks.is_empty(),
        "the value-format column carries no inline CHECK, so this witness has no subject"
    );

    // Fold the SAME history with the column ALREADY named `article_code` and no rename
    // at all. The renamed body must equal the never-renamed one BYTE FOR BYTE: that is
    // a far stronger statement than "the literals survived", because it also catches a
    // rewrite that dropped, doubled or re-quoted anything else in the body.
    let never_renamed = fold_ops(
        &{
            let raw: MigrationIr =
                serde_json::from_str(&CREATE_IR.replace(OLD_FORMAT_COLUMN, NEW_FORMAT_COLUMN))
                    .expect("the never-renamed IR parses");
            resolve_create_table_policy(&raw, &support::no_inject(APP), PROJECT)
                .expect("the never-renamed IR resolves")
                .ops
        },
        SqlDialect::Sqlite,
        PROJECT,
        &support::no_inject(APP),
    )
    .expect("fold the never-renamed history");

    let expected = never_renamed
        .tables
        .get(TABLE)
        .and_then(|table| {
            table
                .columns
                .iter()
                .find(|column| column.name == NEW_FORMAT_COLUMN)
        })
        .expect("the never-renamed value-format column exists");

    assert_eq!(
        column.inline_checks, expected.inline_checks,
        "an inline CHECK reached by a RENAME must be byte-identical to the one the same \
         history produces with the column named `{NEW_FORMAT_COLUMN}` from the start. A \
         difference here is the rewrite corrupting the body - the failure mode that is \
         strictly worse than the staleness it repairs, because the hardened SQLite \
         connection would accept a mangled predicate that enforces something else"
    );
}
