//! **The oracle that adjudicates step 4 consumer 3, live, with rows in the table.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 4 moves
//! `fold_to_field_defs` onto `FoldedSchema::project_field_defs`. Two of that walker's
//! consumers only produce a file a human reads. The third does not:
//!
//! ```text
//! fold_to_field_defs
//!   -> engine.rs, inside `deploy_envelopes_locked`      live.sqlite_schemas
//!   -> render/lower.rs, the SQLite `renameColumn` leg   live.sqlite_schemas.get(table)
//!   -> render/declarative.rs                            desired.sqlite_schemas.get(table)
//!   -> schema/query.rs                                  the rebuilt CREATE TABLE
//!   -> SQLite's 12-step table rebuild                   INSERT INTO tmp SELECT … FROM old
//! ```
//!
//! The last line is a DATA MIGRATION. Every row is copied into a table whose shape came
//! from that map, so a map that is wrong about a type, a nullability, a default or a
//! constraint does not produce a wrong file - it produces a wrong table, over real rows,
//! inside the migration transaction.
//!
//! # TWO legs, and only one of them reads the map's CONTENT
//!
//! This is the correction this file exists to record, and it was found by neutering
//! rather than by reading. `render_create_table_sqlite_rebuild` chooses between two arms
//! on `preserve_stored_shape = pure_rename.is_some() && dt.stored_create_sql.is_some()`:
//!
//! * the STORED-SHAPE arm replays SQLite's own `CREATE TABLE` text and defers the rename
//!   to SQLite's `ALTER TABLE … RENAME COLUMN`. It never reads `sqlite_schemas`;
//! * the SDK-VALUE arm renders a new `CREATE TABLE` from `sqlite_schemas` verbatim.
//!
//! On the `deploy_envelopes` path the live snapshot is INTROSPECTED, so it carries
//! `stored_create_sql`, and a `renameColumn` the engine accepts is a pure rename. So the
//! deploy path takes the STORED-SHAPE arm, and its dependency on `sqlite_schemas` is one
//! of PRESENCE, not of content: `render/lower.rs` fails closed when the table's entry is
//! MISSING (`SqliteRenameNeedsLiveTable`) and never looks inside it.
//!
//! MEASURED, twice, over the whole 229-binary suite:
//!
//! * emptying the map the engine builds fails exactly ONE test,
//!   `hr_sqlite::hr_migrations_apply_in_sequence_on_real_sqlite`, and it fails on the
//!   ABSENCE check with `… needs the table's full live structure … it is absent`;
//!   every test in this file still passed;
//!   * rewriting every column in that map to `{"type":"string"}` and stripping
//!   `required`, `default`, `unique`, `refTarget` and `onDelete` fails NOTHING AT ALL.
//!
//! That second number is the honest statement of the gap: on the deploy path a
//! present-but-WRONG descriptor was, and is, invisible. [`the_deploy_path_depends_on_the_maps_PRESENCE_not_its_content`]
//! pins it so the next reader does not have to rediscover it, and
//! [`a_fold_seeded_rebuild_renders_its_create_table_from_the_map`] covers the other arm -
//! the one that DOES read the content - against a real database with rows in the table.
//!
//! The second arm is not hypothetical: it is the leg `engine::refresh_historical_live`
//! builds (table snapshots from `fold_ops`, field maps from the projection, no
//! `stored_create_sql`), and six existing `*_sqlite.rs` files drive it. What none of
//! them does is assert the DESCRIPTOR'S OWN CLAIMS - column types, nullability, default
//! and the constraint set - against what the server independently reports afterwards,
//! with rows in the table across the copy.
//!
//! # What is asked of the server, and in what order
//!
//! Every case runs against a real SQLite file. The deploy-path cases go through the
//! shipped [`zero_migrate::MigrationEngine::deploy_envelopes`] - the same entry point the
//! CLI uses, and the one that populates `live.sqlite_schemas` from the fold; the
//! content-reading case lowers and applies through `IrAuthor` + `apply_plan` against the
//! fold-seeded live schema, which is the shape `engine::refresh_historical_live` builds.
//!
//! Each reads the SERVER's answer out of `PRAGMA table_info` / `foreign_key_list` /
//! `index_list` BEFORE the rebuild and asserts that first, so a case whose setup silently
//! did nothing reports a setup failure rather than a defect. Only then is the rebuilt
//! table compared to it.
//!
//! # No skip
//!
//! SQLite is an embedded temp file, so every test here always runs for real. There is
//! no `ZERO_MIGRATE_*` gate to forget and no skip banner that could read as a pass -
//! the one respect in which this file is stronger than consumer 2's
//! `tests/env_db_ts_matches_the_server_pg.rs`.

use crate::support;

use std::collections::BTreeMap;
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

const PROJECT: &str = "prj_rebuild_field_defs";
const APP: &str = "app_rebuild_field_defs";

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

/// Deploy an ordered envelope set through the shipped engine.
///
/// The REAL path on purpose: `deploy_envelopes_locked` is the only place that seeds
/// `live.sqlite_schemas` from the fold, so a test that assembled a `LiveSchema` by hand
/// would be measuring its own wiring rather than the engine's.
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

/// The SERVER's column list for `table`, as `(name, declared type, NOT NULL, default)`.
///
/// Every one of these four is a claim the folded `FieldDef` map makes and the rebuild
/// re-renders: `type`, `required`, and `default` are keys in the map, and the rebuild's
/// `CREATE TABLE` is built from them.
async fn columns(
    backend: &SqliteBackend,
    table: &str,
) -> Vec<(String, String, bool, Option<String>)> {
    query(backend, &format!("PRAGMA main.table_info({table})"))
        .await
        .into_iter()
        .map(|row| {
            // cid, name, type, notnull, dflt_value, pk
            (
                row[1].clone().unwrap_or_default(),
                row[2].clone().unwrap_or_default(),
                row[3].as_deref() == Some("1"),
                row[4].clone(),
            )
        })
        .collect()
}

/// The SERVER's own statement of every outgoing foreign key on `table`, as
/// `(child column, parent table, ON DELETE)`.
///
/// `NO ACTION` is SQLite's spelling for "no clause was written", so this reports the
/// referential ACTION the database stored rather than what any artifact claims.
async fn foreign_keys(backend: &SqliteBackend, table: &str) -> Vec<(String, String, String)> {
    query(backend, &format!("PRAGMA main.foreign_key_list({table})"))
        .await
        .into_iter()
        .map(|row| {
            // id, seq, table, from, to, on_update, on_delete, match
            (
                row[3].clone().unwrap_or_default(),
                row[2].clone().unwrap_or_default(),
                row[6].clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// Every index the server holds on `table`, as `(name, unique)`, sorted.
async fn indexes(backend: &SqliteBackend, table: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = query(backend, &format!("PRAGMA main.index_list({table})"))
        .await
        .into_iter()
        .map(|row| {
            // seq, name, unique, origin, partial
            (
                row[1].clone().unwrap_or_default(),
                row[2].as_deref() == Some("1"),
            )
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// The migrations
// ---------------------------------------------------------------------------

/// One table carrying, in five columns, five different claims the folded `FieldDef` map
/// makes about storage: a NOT NULL text primary key, a nullable text column, a UNIQUE
/// NOT NULL text column, a NOT NULL integer with a DEFAULT, and a reference with
/// `ON DELETE CASCADE`.
///
/// Deliberately no generated column, no inline CHECK and no case-insensitive text: those
/// three route the rebuild through the SNAPSHOT renderer instead
/// (`render_create_table_sqlite_rebuild`'s first arm), which does NOT read
/// `sqlite_schemas`. A fixture carrying one of them would exercise a different arm and
/// prove nothing about this leg. The charter is `no_inject` for the same reason: an
/// injected table cannot reach the SDK-value arm at all, which
/// `tests/rename_column_indexed_sqlite.rs` pins separately.
const CREATE: &str = r#"{"ir_version":1,"name":"create_orders","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"label","type":"text"}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"note","type":"text"},{"name":"code","type":"text","nullable":false,"unique":true},{"name":"qty","type":"int","nullable":false,"default":{"literal":{"value":1}}},{"name":"owner_id","type":"text","references":{"table":"accounts","column":"id","onDelete":"cascade"}}],"primaryKey":["id"]}
]}"#;

/// The op that forces the 12-step rebuild. A SQLite `renameColumn` is the ONE consumer
/// of `LiveSchema::sqlite_schemas` in the whole engine (`render/lower.rs`), so this is
/// what puts the folded map into a `CREATE TABLE` that rows are copied through.
const RENAME: &str = r#"{"ir_version":1,"name":"rename_note","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"renameColumn","table":"orders","from":"note","to":"memo","type":"text"}
]}"#;

const TABLES: [&str; 2] = ["accounts", "orders"];

async fn seed(backend: &SqliteBackend) {
    exec(
        backend,
        "INSERT INTO main.accounts (id, label) VALUES ('acct_1', 'first'), ('acct_2', 'second')",
    )
    .await
    .expect("insert the parent rows");
    exec(
        backend,
        "INSERT INTO main.orders (id, note, code, qty, owner_id) VALUES \
         ('ord_1', 'alpha', 'A-1', 7, 'acct_1'), \
         ('ord_2', NULL, 'A-2', 1, 'acct_2'), \
         ('ord_3', 'gamma', 'A-3', 3, NULL)",
    )
    .await
    .expect("insert the child rows");
}

// ---------------------------------------------------------------------------
// The observations the leg never had
// ---------------------------------------------------------------------------

/// **Every row and every column value survives the copy the rebuild performs.**
///
/// This drives the DEPLOY path, so it exercises the stored-shape arm: the rebuild
/// replays SQLite's own `CREATE TABLE`. It is therefore evidence about the rebuild and
/// about the rows, and NOT about the folded map's content - see the module docs and
/// [`the_deploy_path_depends_on_the_maps_PRESENCE_not_its_content`]. The content claim
/// is made by [`a_fold_seeded_rebuild_renders_its_create_table_from_the_map`], on the
/// other arm.
#[compio::test]
async fn every_row_and_every_column_survives_a_deployed_rebuild() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");

    deploy(&backend, &TABLES, &[CREATE])
        .await
        .expect("create the tables");
    seed(&backend).await;

    // THE ORACLE, read from the server before the rebuild and asserted first.
    let before = columns(&backend, "orders").await;
    assert_eq!(
        before,
        vec![
            ("id".to_string(), "TEXT".to_string(), true, None),
            ("note".to_string(), "TEXT".to_string(), false, None),
            ("code".to_string(), "TEXT".to_string(), true, None),
            (
                "qty".to_string(),
                "INTEGER".to_string(),
                true,
                Some("1".to_string())
            ),
            ("owner_id".to_string(), "TEXT".to_string(), false, None),
        ],
        "the SERVER's pre-rebuild column list, read first so a later difference is \
         attributable to the rebuild rather than to the create"
    );

    deploy(&backend, &TABLES, &[CREATE, RENAME])
        .await
        .expect("the rename rebuilds the table");

    let mut expected = before.clone();
    expected[1].0 = "memo".to_string();
    assert_eq!(
        columns(&backend, "orders").await,
        expected,
        "only the renamed column's NAME may move. A type, a nullability or a default \
         that moved here is the rebuild rewriting the table's storage contract from the \
         folded FieldDef map, over rows that were written under the old one"
    );
    assert_eq!(
        query(
            &backend,
            "SELECT id, memo, code, qty, owner_id FROM main.orders ORDER BY id"
        )
        .await,
        vec![
            vec![
                Some("ord_1".to_string()),
                Some("alpha".to_string()),
                Some("A-1".to_string()),
                Some("7".to_string()),
                Some("acct_1".to_string()),
            ],
            vec![
                Some("ord_2".to_string()),
                None,
                Some("A-2".to_string()),
                Some("1".to_string()),
                Some("acct_2".to_string()),
            ],
            vec![
                Some("ord_3".to_string()),
                Some("gamma".to_string()),
                Some("A-3".to_string()),
                Some("3".to_string()),
                None,
            ],
        ],
        "every row, every value and every NULL survives the copy"
    );
}

/// **The rebuilt table carries exactly the constraints the server had, and they still
/// bite.**
///
/// Structure first, then behaviour. A structural comparison alone would accept an index
/// that exists but is not enforced, and a behavioural check alone would not notice a
/// SECOND constraint appearing beside the right one.
#[compio::test]
async fn the_rebuilt_table_carries_exactly_the_constraints_the_server_had() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");

    deploy(&backend, &TABLES, &[CREATE])
        .await
        .expect("create the tables");
    seed(&backend).await;

    // THE ORACLE. Both are asserted against their expected content, not just captured,
    // so a create that silently produced no unique index and no foreign key would fail
    // here rather than making the post-rebuild comparison trivially true.
    let keys_before = foreign_keys(&backend, "orders").await;
    assert_eq!(
        keys_before,
        vec![(
            "owner_id".to_string(),
            "accounts".to_string(),
            "CASCADE".to_string()
        )],
        "the SERVER must hold the cascading reference before the rebuild"
    );
    let indexes_before = indexes(&backend, "orders").await;
    assert!(
        indexes_before.iter().any(|(_, unique)| *unique),
        "the SERVER must hold a unique index before the rebuild: {indexes_before:?}"
    );

    deploy(&backend, &TABLES, &[CREATE, RENAME])
        .await
        .expect("the rename rebuilds the table");

    assert_eq!(
        foreign_keys(&backend, "orders").await,
        keys_before,
        "the rebuild must neither drop the reference nor change its referential action. \
         A re-imposed `ON DELETE CASCADE` destroys child rows on a later parent delete; \
         a dropped one leaves orphans. Both are decided by the folded FieldDef map."
    );
    assert_eq!(
        indexes(&backend, "orders").await,
        indexes_before,
        "and the index set, uniqueness included, is the one the server already had"
    );

    // BEHAVIOUR, because a constraint that exists and is not enforced reads identical to
    // one that is.
    assert!(
        exec(
            &backend,
            "INSERT INTO main.orders (id, code, qty) VALUES ('ord_9', 'A-1', 1)",
        )
        .await
        .is_err(),
        "the unique index must still reject a duplicate after the rebuild"
    );
    assert!(
        exec(
            &backend,
            "INSERT INTO main.orders (id, code, qty) VALUES ('ord_9', NULL, 1)",
        )
        .await
        .is_err(),
        "and the NOT NULL must still reject a missing value"
    );
    exec(
        &backend,
        "INSERT INTO main.orders (id, code) VALUES ('ord_9', 'A-9')",
    )
    .await
    .expect("a row that violates nothing still inserts");
    assert_eq!(
        query(&backend, "SELECT qty FROM main.orders WHERE id = 'ord_9'").await,
        vec![vec![Some("1".to_string())]],
        "and the DEFAULT the map carries is still the one the server applies"
    );
    exec(&backend, "DELETE FROM main.accounts WHERE id = 'acct_1'")
        .await
        .expect("the parent delete cascades");
    assert_eq!(
        query(&backend, "SELECT id FROM main.orders WHERE id = 'ord_1'").await,
        Vec::<Vec<Option<String>>>::new(),
        "and the cascade the server had still reaches the child row after the rebuild"
    );
}

// ---------------------------------------------------------------------------
// The arm that DOES read the map, against a real database with rows in it
// ---------------------------------------------------------------------------

const RENAME_IR: &str = r#"{"ir_version":1,"name":"rename_note","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"renameColumn","table":"orders","from":"note","to":"memo","type":"text"}
]}"#;

/// The live schema `engine::refresh_historical_live` builds: table snapshots from
/// `fold_ops`, field maps from the projection, over the ops applied so far.
///
/// No `stored_create_sql`, because both halves are AUTHORED rather than introspected -
/// which is exactly what turns `preserve_stored_shape` off and routes the rebuild
/// through the SDK-value arm, the one that renders its `CREATE TABLE` from the map.
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
            "rebuild-field-defs",
            LockMode::Acquire,
        )
        .await
        .expect("the plan applies");
    resolved.ops
}

/// **The content claim: the rebuilt table is the one the folded map describes, and the
/// rows come through it intact.**
///
/// This is the arm the module docs identify as reading `sqlite_schemas` verbatim. Every
/// assertion below is a claim the MAP makes - the column set and order, the declared
/// types, the NOT NULLs, the DEFAULT, the unique index and the foreign key - read back
/// out of the SERVER after a real 12-step rebuild has copied three real rows through it.
///
/// A neuter that rewrites the map's `type` to `"string"` and strips `required`,
/// `default`, `unique`, `refTarget` and `onDelete` leaves the whole rest of the suite
/// green; it fails HERE, which is the only reason this file can claim to observe the
/// descriptor at all.
#[compio::test]
async fn a_fold_seeded_rebuild_renders_its_create_table_from_the_map() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");

    let history = apply(&backend, CREATE, &LiveSchema::default()).await;
    seed(&backend).await;

    // THE ORACLE, before the rebuild.
    let before_columns = columns(&backend, "orders").await;
    let before_keys = foreign_keys(&backend, "orders").await;
    assert_eq!(
        before_columns,
        vec![
            ("id".to_string(), "TEXT".to_string(), true, None),
            ("note".to_string(), "TEXT".to_string(), false, None),
            ("code".to_string(), "TEXT".to_string(), true, None),
            (
                "qty".to_string(),
                "INTEGER".to_string(),
                true,
                Some("1".to_string())
            ),
            ("owner_id".to_string(), "TEXT".to_string(), false, None),
        ],
        "the SERVER's pre-rebuild column list"
    );
    assert_eq!(
        before_keys,
        vec![(
            "owner_id".to_string(),
            "accounts".to_string(),
            "CASCADE".to_string()
        )],
        "and its foreign key, cascade included"
    );

    // The FOLD-seeded live schema routes this through the SDK-value arm.
    let live = folded_live_schema(&history);
    assert!(
        live.table_snapshots["orders"].stored_create_sql.is_none(),
        "the authored snapshot must carry NO stored CREATE, or the rebuild replays \
         SQLite's own text and this test observes nothing about the map"
    );
    apply(&backend, RENAME_IR, &live).await;

    // THE MAP's own claim about `qty`, asserted before the server's, so the difference
    // below is attributable to the emitter rather than to the fold.
    assert_eq!(
        live.sqlite_schemas["orders"]["qty"].get("default"),
        Some(&serde_json::json!(1)),
        "the folded map declares the DEFAULT, or the row below is not about the emitter"
    );

    let mut expected = before_columns.clone();
    expected[1].0 = "memo".to_string();
    // `qty`'s `DEFAULT 1` SURVIVES the rebuild, and the line that says so used to say
    // the opposite. This was a real defect, characterized here rather than expected
    // away: `schema::query::def_to_constraints_for_dialect` emitted a `DEFAULT` for the
    // `string`, `number` and `boolean` type tokens and had no arm for `int`, so an
    // integer column's default was dropped by the rebuilt `CREATE TABLE` while a text
    // column's survived. Both the walker and the projection put `"default": 1` in the
    // map (asserted above), so the loss was downstream of the map, in the emitter. The
    // emitter now renders the numeric and text-shaped token families through the same
    // renderer its `FieldDescriptor` sibling uses, and the server keeps the default -
    // so `before_columns` passes through unchanged apart from the rename, which is what
    // a rebuild is supposed to mean.
    assert_eq!(
        columns(&backend, "orders").await,
        expected,
        "the rebuilt table's columns, types and NOT NULLs are the ones the folded map \
         declares. Every one of these is a key in the map, rendered into the \
         `CREATE TABLE` that three rows were just copied into."
    );
    assert_eq!(
        foreign_keys(&backend, "orders").await,
        before_keys,
        "and the reference, with the referential action the map carries. A wrong \
         `onDelete` here silently deletes child rows on a later parent delete."
    );
    assert!(
        indexes(&backend, "orders").await.iter().any(|(_, u)| *u),
        "the unique index the map's `unique` key declares survives the rebuild"
    );
    assert_eq!(
        query(
            &backend,
            "SELECT id, memo, code, qty, owner_id FROM main.orders ORDER BY id"
        )
        .await,
        vec![
            vec![
                Some("ord_1".to_string()),
                Some("alpha".to_string()),
                Some("A-1".to_string()),
                Some("7".to_string()),
                Some("acct_1".to_string()),
            ],
            vec![
                Some("ord_2".to_string()),
                None,
                Some("A-2".to_string()),
                Some("1".to_string()),
                Some("acct_2".to_string()),
            ],
            vec![
                Some("ord_3".to_string()),
                Some("gamma".to_string()),
                Some("A-3".to_string()),
                Some("3".to_string()),
                None,
            ],
        ],
        "and every row, value and NULL survives the copy through it"
    );
}

/// **The finding this file was rewritten around, pinned so it is not rediscovered.**
///
/// On the DEPLOY path the rebuild takes the stored-shape arm, so `LiveSchema::sqlite_schemas`
/// is required to be PRESENT and is never read. This test states both halves: the entry
/// must exist, and the deploy is indifferent to what is in it.
///
/// It is written as a characterization, not an endorsement. A present-but-wrong
/// descriptor being invisible here is the reason
/// [`a_fold_seeded_rebuild_renders_its_create_table_from_the_map`] exists, and the
/// reason the offline golden carries the emitted `CREATE TABLE` for every SQLite table
/// in the corpus rather than only the JSON.
#[compio::test]
#[allow(non_snake_case)]
async fn the_deploy_path_depends_on_the_maps_PRESENCE_not_its_content() {
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");
    let history = apply(&backend, CREATE, &LiveSchema::default()).await;
    let policy = support::no_inject(PROJECT);
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &policy);
    let raw: MigrationIr = serde_json::from_str(RENAME_IR).expect("the rename IR parses");
    let resolved = resolve_create_table_policy(&raw, &policy, PROJECT).expect("it resolves");

    // PRESENCE is load-bearing: the catalog-sourced live schema with an EMPTY map is
    // refused before any DDL is emitted.
    let snapshot = backend
        .snapshot_schema_sqlite()
        .await
        .expect("the catalog snapshot reads");
    assert!(
        snapshot.tables["orders"].stored_create_sql.is_some(),
        "an INTROSPECTED snapshot carries the stored CREATE, which is what routes a \
         pure rename through the replay arm on the deploy path"
    );
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    let error = author
        .lower_steps(&resolved, &live)
        .expect_err("an absent field map is refused")
        .to_string();
    assert!(
        error.contains("sqlite_schemas"),
        "and the refusal names the map it needs: {error}"
    );

    // CONTENT is not: the same rename lowers identically whether the map is the real one
    // or a deliberately wrong one, because this arm replays SQLite's own stored text.
    live.sqlite_schemas = single_fold::fold(&history, SqlDialect::Sqlite, PROJECT, &policy)
        .expect("the history folds")
        .project_field_defs();
    let real = format!(
        "{:?}",
        author.lower_steps(&resolved, &live).expect("lowers")
    );

    let mut wrong = live;
    for schema in wrong.sqlite_schemas.values_mut() {
        if let Some(columns) = schema.as_object_mut() {
            for field in columns.values_mut() {
                if let Some(object) = field.as_object_mut() {
                    object.insert("type".into(), serde_json::json!("string"));
                    object.remove("required");
                    object.remove("default");
                    object.remove("unique");
                }
            }
        }
    }
    let corrupted = format!(
        "{:?}",
        author.lower_steps(&resolved, &wrong).expect("lowers")
    );
    assert_eq!(
        real, corrupted,
        "MEASURED, and recorded rather than celebrated: on the deploy path the rebuild \
         is byte-identical whether the folded map is right or deliberately wrong, so \
         nothing on that path can catch a present-but-wrong descriptor. If this ever \
         starts differing, the deploy path has begun reading the map's content and this \
         file's other cases become load-bearing for it."
    );
}

// ---------------------------------------------------------------------------
// The blast radius of the move, measured at the SQLite deploy path
// ---------------------------------------------------------------------------

/// Every stream on which the two answers differ, with the op that would put the
/// difference into a rebuilt `CREATE TABLE` appended.
///
/// The sweep behind this move reported FIVE divergence families between
/// `fold_to_field_defs` and `FoldedSchema::project_field_defs`, over every prefix of the
/// 27 recorded fixtures and 22 carriers on 3 dialects. Three of the five never appear on
/// SQLite at all because the fold refuses the op that creates them; the other two do
/// appear in the MAP on SQLite. Whether they reach the REBUILD is a different question -
/// the map is built offline, the rebuild only ever sees ops that a live deploy accepted
/// - and it is the question this table answers by asking the deploy path.
/// Each row carries the substring the engine's refusal must contain, so a stream that
/// stopped being refused FOR ITS OWN REASON - a typo in the JSON, a table renamed out
/// from under it, a charter grant that changed - fails here instead of passing the
/// emptiness assertion for free. Four streams, four DIFFERENT refusal paths.
const DIVERGENCE_STREAMS: &[(&str, &str, &str)] = &[
    (
        "a dropped UNIQUE constraint",
        "SQLite cannot add or drop a table constraint in place",
        r#"{"ir_version":1,"name":"d1","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}}},
  {"op":"dropConstraint","table":"users","name":"users_email_uq"},
  {"op":"renameColumn","table":"users","from":"email","to":"mail","type":"text"}
]}"#,
    ),
    (
        "a dropped CHECK bound",
        "addConstraint(check) expression rendering is PostgreSQL-only",
        r#"{"ir_version":1,"name":"d2","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}}}}},
  {"op":"dropConstraint","table":"users","name":"users_score_ck"},
  {"op":"renameColumn","table":"users","from":"score","to":"points","type":"int"}
]}"#,
    ),
    (
        "a re-added column inheriting a dropped foreign-key policy",
        "logical/storage type differs",
        r#"{"ir_version":1,"name":"d4","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"note","type":"text"},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"],"constraints":[{"name":"orders_owner_fk","kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}]},
  {"op":"dropColumn","table":"orders","column":"owner_id"},
  {"op":"addColumn","table":"orders","column":"owner_id","type":{"ref":{"references":"accounts"}}},
  {"op":"renameColumn","table":"orders","from":"note","to":"memo","type":"text"}
]}"#,
    ),
    (
        "a dropped partition still in the map",
        "native only on Postgres",
        r#"{"ir_version":1,"name":"d5","owner_app":"app_rebuild_field_defs","ops":[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"dropPartition","parent":"par","name":"p1"}
]}"#,
    ),
];

/// **The blast radius, measured rather than reasoned about: no divergence between the
/// walker and the projection can reach a SQLite table rebuild.**
///
/// This is the safety claim the whole move turns on, so it is asked of the engine rather
/// than argued from the lowering rules. Each stream above is one on which the two
/// answers provably differ; each is deployed for real on SQLite through the same
/// `deploy_envelopes` entry point the CLI uses; and each must be REFUSED before it can
/// put its difference into a rebuilt `CREATE TABLE`.
///
/// Read this as narrowly as it is written. It does NOT say the move changes nothing - it
/// changes five things, and `tests/gen_types_field_defs_from_the_fold.rs` pins all of
/// them offline. It says the changes are confined to `schema.runtime.json`, and that the
/// leg which copies rows is not among the consumers whose answer moves.
///
/// The CONTROL below is what stops that from being a claim about a broken harness: the
/// same helper, the same charter, the same `renameColumn`, on a stream with no
/// divergence in it, DEPLOYS. Without it, a `deploy` that refused everything would pass
/// this test perfectly.
#[compio::test]
async fn no_field_def_divergence_reaches_a_sqlite_rebuild() {
    let mut deployed = Vec::new();
    let mut wrong_reason = Vec::new();
    for (label, expected, source) in DIVERGENCE_STREAMS {
        let paths = paths();
        let backend =
            SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");
        let tables = ["users", "accounts", "orders", "par", "p1"];
        match deploy(&backend, &tables, &[source]).await {
            Err(error) if error.contains(expected) => {}
            Err(error) => wrong_reason.push(format!(
                "{label}: refused, but not for {expected:?} - {error}"
            )),
            Ok(()) => deployed.push((*label).to_string()),
        }
    }
    assert!(
        wrong_reason.is_empty(),
        "a divergence stream is refused for a DIFFERENT reason than the one it was \
         written to hit, so it no longer exercises the arm it names and its refusal is \
         not evidence about that arm:\n  {}",
        wrong_reason.join("\n  ")
    );
    assert!(
        deployed.is_empty(),
        "a stream on which the folded FieldDef map and the walker disagree DEPLOYED on \
         SQLite, so the difference can reach the 12-step rebuild's CREATE TABLE and be \
         copied over real rows. Each one needs its own live adjudication before this \
         move can be called safe on the DDL path: {deployed:?}"
    );

    // THE CONTROL. Same helper, same charter, same rename - and this one must deploy, or
    // the assertion above is satisfied by a harness that cannot deploy anything.
    let paths = paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open the SQLite backend");
    deploy(&backend, &TABLES, &[CREATE, RENAME]).await.expect(
        "the control stream must DEPLOY, or `no divergence reaches the rebuild` is a \
             statement about a broken harness rather than about the engine",
    );
    assert_eq!(
        columns(&backend, "orders")
            .await
            .iter()
            .map(|(name, ..)| name.clone())
            .collect::<Vec<_>>(),
        vec!["id", "memo", "code", "qty", "owner_id"],
        "and the control really did reach the rebuild, or it controls for nothing"
    );
}
