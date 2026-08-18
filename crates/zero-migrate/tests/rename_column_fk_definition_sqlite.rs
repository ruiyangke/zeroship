//! A column rename must follow the FOREIGN KEY definitions whose LOCAL column list
//! names that column - and must leave the REFERENCED side of the same definition
//! alone.
//!
//! `ConstraintSnapshot::definition` is rendered SQL text, and the SQLite rebuild's
//! snapshot renderer splices it into the new table's `CREATE TABLE` verbatim. The
//! rebuild derives its post-rename table from the LIVE one by rewriting
//! `ColumnSnapshot::name`, so before this fix the rebuilt CREATE said
//!
//! ```sql
//! CONSTRAINT "issues_owner_id_fkey" FOREIGN KEY (owner_id) REFERENCES owners(owner_id)
//! ```
//!
//! over a table whose column is now `holder_id`. SQLite RESOLVES a foreign key's
//! CHILD column list at CREATE TABLE time (`unknown column "..." in foreign key
//! definition`), so that DDL is REFUSED at the rebuild's leading statement - a failed
//! migration inside the transaction, never a corrupted schema. That is measured here
//! rather than asserted from the SQLite documentation, because the PARENT side of the
//! very same definition behaves the OTHER way: a bad referenced column is accepted at
//! CREATE and only surfaces as `foreign key mismatch` on the first DML that fires the
//! constraint, and not at all while `PRAGMA foreign_keys` is off. A rewrite that
//! touched the referenced side would therefore degrade SILENTLY, which is exactly why
//! the fixture below points the FK at a PARENT COLUMN THAT SHARES THE RENAMED
//! COLUMN'S NAME.
//!
//! Why this is not the rewrite the inline-CHECK fix used: `rename_quoted_column_in_sql`
//! walks QUOTED runs and leaves a bare word alone, but a constraint `definition`
//! spells its columns through `constraintdef_cols`, which quotes CONDITIONALLY - so a
//! plain lowercase `owner_id` is BARE and the quoted-run walk would rewrite nothing,
//! while a reserved-word column is quoted on BOTH sides of `REFERENCES` and the walk
//! would rewrite both. Wrong in both regimes. The fold has already solved this shape
//! with `rename_constraint_definition_column`, which re-renders only the LEADING
//! parenthesized group, and that is what the rebuild now reuses.
//!
//! The seam matters as much as the rewrite. `ConstraintSnapshot`'s `PartialEq`
//! COMPARES `definition`, so rewriting it into the DESIRED snapshot would make the
//! desired table differ from the renamed live one, flip `pure_sqlite_column_rename` to
//! `None`, turn `preserve_stored_shape` OFF and stop the CATALOG path replaying
//! SQLite's own stored body. The rewrite therefore runs INSIDE
//! `render_create_table_sqlite_rebuild`, AFTER that decision has been taken and after
//! the stored-shape arm has already returned. Two tests below pin that: one asserts
//! the catalog leg still replays its stored body, and one asserts the shape DECISION
//! itself is unchanged through its only observable projection,
//! `SqliteRebuildSpec::column_renames` (populated only when `preserve_stored_shape`
//! holds).

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::ir::IrFlagsOverride;
use zero_migrate::render::fold::single_fold;
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::{
    fold_ops, resolve_create_table_policy, Approval, ColType, ExecutorConfig, MigrationEngine,
    MigrationIr, Op, PlanStep, RenameStep, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_fk_definition";
const APP: &str = "app_fk_definition";
const TABLE: &str = "issues";
const PARENT: &str = "owners";
/// The FK's local column, and ALSO the parent's referenced column. The rename must
/// move the first and leave the second exactly where it is.
const OLD_COLUMN: &str = "owner_id";
const NEW_COLUMN: &str = "holder_id";
const FK_NAME: &str = "issues_owner_id_fkey";

/// A parent whose UNIQUE candidate key is a column SHARING THE CHILD'S FK COLUMN NAME
/// (unique because SQLite requires the referenced columns to be a parent key), and a
/// child carrying BOTH an enum column - which is what routes this rebuild through the
/// SNAPSHOT renderer, the one arm that splices `definition` into the CREATE - and the
/// foreign key whose LOCAL column is about to be renamed. The shared name is the point:
/// the rename must move the local `owner_id` and leave the referenced `owner_id`
/// exactly where it is.
fn create_ir() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_issues",
        "owner_app": APP,
        "ops": [
            {
                "op": "createEnum",
                "name": "issue_status",
                "values": ["UNCONFIRMED", "CONFIRMED"],
            },
            {
                "op": "createTable",
                "name": PARENT,
                "columns": [
                    { "name": OLD_COLUMN, "type": "text", "nullable": false, "unique": true },
                    { "name": "label", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    {
                        "name": "status",
                        "type": { "enum": { "name": "issue_status" } },
                        "nullable": false,
                    },
                    {
                        "name": OLD_COLUMN,
                        "type": "text",
                        "nullable": false,
                        "references": {
                            "table": PARENT,
                            "column": OLD_COLUMN,
                            "name": FK_NAME,
                        },
                    },
                    { "name": "summary", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
        ],
    }))
    .expect("the create IR deserializes")
}

fn rename_ir() -> MigrationIr {
    rename_ir_named("rename_owner_id", OLD_COLUMN, NEW_COLUMN)
}

fn rename_ir_named(name: &str, from: &str, to: &str) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: name.to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::RenameColumn {
            table: TABLE.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            ty: ColType::Text,
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

/// `support::confined_charter` with the catalog-read scope widened to the project
/// schema. A column-level `references` lowers with an EXISTENCE GUARD that probes the
/// FK target, and the executor authorizes that probe against the `schema.cross_schema`
/// grant's scope; the shared fixture's `include = ["app"]` does not name this test's
/// project, so the guard would refuse the CREATE before the rename under test ever
/// ran. Everything else - the mandatory inject, the destructive-ops allowance - is the
/// shared fixture verbatim.
const CHARTER: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "version",    type = "integer",     nullable = false },
]
"#;

fn charter() -> zero_migrate::EffectivePolicy {
    zero_migrate::effective_policy_from_charter_toml(CHARTER)
        .expect("the fk-definition test charter composes")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, charter())
}

/// The SQLite live schema `engine::refresh_historical_live` builds: table snapshots
/// from `fold_ops`, SDK field maps from the `FieldDef` projection, over the same ops.
fn folded_live_schema(history: &[Op]) -> LiveSchema {
    let effective = charter();
    let snapshot =
        fold_ops(history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let sqlite_schemas = single_fold::fold(history, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = sqlite_schemas;
    live.unique_indexes = BTreeSet::new();
    live
}

fn seed_parent_sql(owner: &str) -> String {
    format!(
        "INSERT INTO main.{PARENT} (id, created_at, updated_at, version, {OLD_COLUMN}, label) \
         VALUES ('o-{owner}','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'{owner}','L')"
    )
}

fn child_sql(id: &str, column: &str, owner: &str) -> String {
    format!(
        "INSERT INTO main.{TABLE} (id, created_at, updated_at, version, status, {column}, summary) \
         VALUES ('{id}','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'CONFIRMED','{owner}','s')"
    )
}

async fn deploy_create(backend: &SqliteBackend, engine: &MigrationEngine) -> Vec<Op> {
    let effective = charter();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create = resolve_create_table_policy(&create_ir(), &effective, PROJECT)
        .expect("the create resolves under the charter");
    let steps = author
        .lower_steps(&create, &LiveSchema::default())
        .expect("the create lowers");
    engine
        .apply_plan(
            &steps,
            Approval::None,
            backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the create applies");
    create.ops
}

#[compio::test]
async fn a_sqlite_rename_rebuild_emits_a_foreign_key_over_the_new_local_column() {
    let effective = charter();
    let p = paths("fk_definition");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create_ops = deploy_create(&backend, &engine).await;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&seed_parent_sql("alice"))
        .await
        .expect("seed the parent row the child FK points at");
    backend
        .actor()
        .exec(&child_sql("i1", OLD_COLUMN, "alice"))
        .await
        .expect("seed a child row the rebuild has to carry across");

    // Control: the FK is live and ENFORCED on the pre-rename column.
    let rejected = backend
        .actor()
        .exec(&child_sql("bad", OLD_COLUMN, "nobody"))
        .await;
    assert!(
        rejected.is_err(),
        "the control: the foreign key rejects an orphan child before the rename"
    );

    let live = folded_live_schema(&create_ops);
    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the folded live schema");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    let create_sql = rebuild.spec.new_table_create.clone();

    // Asserted on the SQL before the apply, so a failure NAMES the defect instead of
    // reporting an opaque SQLite error.
    assert!(
        create_sql.contains(&format!("FOREIGN KEY ({NEW_COLUMN}) REFERENCES")),
        "the foreign key's LOCAL column list names the POST-rename column:\n{create_sql}"
    );
    assert!(
        !create_sql.contains(&format!("FOREIGN KEY ({OLD_COLUMN})")),
        "no foreign key may still declare the renamed-away column as its local key - \
         the new table does not have it:\n{create_sql}"
    );
    // The REFERENCED side spells the SAME name and belongs to the PARENT table, which
    // this rename did not touch. Rewriting it would point the child at a column
    // `owners` does not have - accepted by CREATE TABLE and only reported later.
    assert!(
        create_sql.contains(&format!("REFERENCES {PARENT}({OLD_COLUMN})")),
        "the REFERENCED table and column are untouched by a LOCAL column rename:\n{create_sql}"
    );
    // The constraint NAME is derived from the pre-rename column and stays put: SQLite
    // does not rename a constraint when a column is renamed, and the fold does not
    // either.
    assert!(
        create_sql.contains(&format!(r#"CONSTRAINT "{FK_NAME}""#)),
        "the constraint keeps the name it was created under:\n{create_sql}"
    );

    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the rebuild applies");

    // The row survived under the new column name.
    let after = backend
        .actor()
        .query(&format!(
            "SELECT {NEW_COLUMN} FROM main.{TABLE} WHERE id = 'i1'"
        ))
        .await
        .expect("read the renamed column");
    assert_eq!(
        after[0][0].as_deref(),
        Some("alice"),
        "the seeded value follows the rename"
    );

    // BEHAVIOUR, not spelling. Both directions, because an FK that names a column the
    // table does not have and an FK whose PARENT key does not exist fail in DIFFERENT
    // places, and only one of them is loud.
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let rejected = backend
        .actor()
        .exec(&child_sql("orphan", NEW_COLUMN, "nobody"))
        .await;
    assert!(
        rejected.is_err(),
        "the rebuilt foreign key REJECTS an orphan written to the POST-rename column"
    );
    backend
        .actor()
        .exec(&seed_parent_sql("bob"))
        .await
        .expect("a second parent");
    backend
        .actor()
        .exec(&child_sql("i2", NEW_COLUMN, "bob"))
        .await
        .expect("and ACCEPTS a child whose post-rename column matches a real parent key");

    // And the stored CREATE - SQLite's own reading of the rebuilt table.
    let stored = backend
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = '{TABLE}'"
        ))
        .await
        .expect("read the stored CREATE");
    let stored = stored[0][0].as_deref().expect("the stored CREATE text");
    assert!(
        stored.contains(&format!(
            "FOREIGN KEY ({NEW_COLUMN}) REFERENCES {PARENT}({OLD_COLUMN})"
        )),
        "the rebuilt table's stored CREATE carries the foreign key over the new local \
         column and the unchanged parent key: {stored}"
    );

    // `PRAGMA foreign_key_list` is SQLite's own structural answer, independent of the
    // text it stored.
    let fks = backend
        .actor()
        .query(&format!("PRAGMA main.foreign_key_list({TABLE})"))
        .await
        .expect("read the rebuilt table's foreign key list");
    let row = fks.first().expect("exactly one foreign key survives");
    assert!(
        row.iter().flatten().any(|cell| cell.as_str() == NEW_COLUMN),
        "SQLite reports the foreign key's local column as the POST-rename name: {fks:?}"
    );
    assert!(
        row.iter().flatten().any(|cell| cell.as_str() == PARENT),
        "and still targets the parent table: {fks:?}"
    );
}

/// What SQLite ACTUALLY does with the stale definition, measured on the hardened
/// connection this crate ships rather than reasoned about.
///
/// The CHILD column list is resolved at CREATE TABLE time, so a stale local column is
/// a hard parse error and the migration fails at its leading statement. The PARENT
/// column list is NOT: a referenced column that does not exist is accepted by CREATE
/// TABLE and only reported when a DML statement fires the constraint. That asymmetry
/// is the reason the rewrite is confined to the leading group.
#[compio::test]
async fn sqlite_refuses_a_stale_child_key_and_silently_accepts_a_stale_parent_key() {
    let p = paths("fk_definition_semantics");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&format!(
            "CREATE TABLE main.{PARENT} (id TEXT PRIMARY KEY, {OLD_COLUMN} TEXT NOT NULL UNIQUE)"
        ))
        .await
        .expect("a parent to point at");

    // (a) the STALE CHILD key - what the defect emitted. SQLite refuses the CREATE.
    let refused = backend
        .actor()
        .exec(&format!(
            "CREATE TABLE main.stale_child (id TEXT PRIMARY KEY, {NEW_COLUMN} TEXT NOT NULL, \
             CONSTRAINT \"c\" FOREIGN KEY ({OLD_COLUMN}) REFERENCES {PARENT}({OLD_COLUMN}))"
        ))
        .await
        .expect_err("a foreign key over a column the table does not have is refused at CREATE");
    let message = refused.to_string();
    assert!(
        message.contains("foreign key") && message.contains(OLD_COLUMN),
        "and the refusal NAMES the missing local column, which is what makes the \
         defect a failed migration rather than a corrupt schema: {message}"
    );
    let exists = backend
        .actor()
        .query("SELECT name FROM main.sqlite_schema WHERE name = 'stale_child'")
        .await
        .expect("read the catalog");
    assert!(
        exists.is_empty(),
        "nothing was created: the refusal lands before the table exists"
    );

    // (b) the STALE PARENT key - what a rewrite that overreached would emit. SQLite
    // ACCEPTS the CREATE and says nothing until a write fires the constraint.
    backend
        .actor()
        .exec(&format!(
            "CREATE TABLE main.stale_parent (id TEXT PRIMARY KEY, {NEW_COLUMN} TEXT NOT NULL, \
             CONSTRAINT \"c\" FOREIGN KEY ({NEW_COLUMN}) REFERENCES {PARENT}({NEW_COLUMN}))"
        ))
        .await
        .expect(
            "a foreign key whose PARENT column does not exist is ACCEPTED at CREATE - the \
             silent half, and the reason the referenced side is left alone",
        );
    let mismatch = backend
        .actor()
        .exec(&format!(
            "INSERT INTO main.stale_parent (id, {NEW_COLUMN}) VALUES ('x','anything')"
        ))
        .await;
    assert!(
        mismatch.is_err(),
        "the breakage surfaces only on the first write that fires the constraint"
    );

    // (c) and not even then, with enforcement off. `PRAGMA foreign_keys` is exactly what
    // the rebuild itself turns off around the table swap, so "silent" here is measured
    // rather than inferred.
    backend
        .actor()
        .exec("PRAGMA foreign_keys = OFF")
        .await
        .expect("enforcement off");
    backend
        .actor()
        .exec(&format!(
            "INSERT INTO main.stale_parent (id, {NEW_COLUMN}) VALUES ('y','anything')"
        ))
        .await
        .expect(
            "with foreign_keys off the broken constraint reports NOTHING - a rewrite that \
             moved the referenced column would leave no trace at all",
        );
    backend
        .actor()
        .exec("PRAGMA foreign_keys = ON")
        .await
        .expect("enforcement back on");
}

/// The SHAPE DECISION on its own, separated from what either arm then emits.
///
/// `SqliteRebuildSpec::column_renames` is populated as
/// `pure_rename.filter(|_| preserve_stored_shape)`, so it is a direct read of the two
/// facts this fix had to leave untouched: whether `pure_sqlite_column_rename` still
/// matched, and whether the stored shape is still preserved. Lowering the SAME rename
/// against the two live shapes gives the contrast in one place - a CATALOG snapshot
/// carries `stored_create_sql` and must come back carrying the rename for the executor
/// to replay, a FOLD-seeded one does not and must come back with none. A rewrite of
/// `definition` into the desired snapshot would collapse the first case onto the
/// second, silently, because `ConstraintSnapshot::eq` compares `definition`.
#[compio::test]
async fn the_stored_shape_decision_is_unchanged_by_the_constraint_rewrite() {
    let effective = charter();
    let p = paths("fk_definition_decision");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create_ops = deploy_create(&backend, &engine).await;

    let snapshot = backend
        .snapshot_schema_sqlite()
        .await
        .expect("the catalog snapshot reads");
    let mut catalog_live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    catalog_live.sqlite_schemas =
        single_fold::fold(&create_ops, SqlDialect::Sqlite, PROJECT, &effective)
            .map(|folded| folded.project_field_defs())
            .expect("the history folds to field defs");

    let decision = |live: &LiveSchema| {
        let steps = author
            .lower_steps(&rename_ir(), live)
            .expect("the rename lowers");
        let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
            panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
        };
        rebuild.spec.column_renames.clone()
    };

    assert_eq!(
        decision(&catalog_live),
        vec![(OLD_COLUMN.to_string(), NEW_COLUMN.to_string())],
        "the CATALOG leg is still a pure rename with the stored shape preserved - the \
         single fact this fix was deferred over"
    );
    assert!(
        decision(&folded_live_schema(&create_ops)).is_empty(),
        "and the FOLD-seeded leg still has no stored shape to preserve, so it renders \
         the new table outright and carries no deferred RENAME COLUMN"
    );
}

/// The OTHER leg, which must not change. A SQLite CATALOG read supplies
/// `stored_create_sql`, so a pure rename replays SQLite's own stored body byte for
/// byte and lets its `RENAME COLUMN` move the foreign key afterwards. Pinned so the
/// fix above cannot leak into it.
#[compio::test]
async fn a_catalog_sourced_rename_still_replays_the_stored_body() {
    let effective = charter();
    let p = paths("fk_definition_catalog");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create_ops = deploy_create(&backend, &engine).await;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&seed_parent_sql("alice"))
        .await
        .expect("seed the parent row");
    backend
        .actor()
        .exec(&child_sql("i1", OLD_COLUMN, "alice"))
        .await
        .expect("seed a child row");

    // The production `live` for a SQLite deploy: a CATALOG snapshot, supplemented
    // with the SDK field map (`MigrationEngine::deploy`).
    let snapshot = backend
        .snapshot_schema_sqlite()
        .await
        .expect("the catalog snapshot reads");
    let table = snapshot
        .tables
        .get(TABLE)
        .expect("the deployed table is in the catalog snapshot");
    assert!(
        table.stored_create_sql.is_some(),
        "the catalog carries the stored CREATE, which is what routes a pure rename \
         through the replay arm"
    );
    let stored_before = table
        .stored_create_sql
        .clone()
        .expect("the stored CREATE text");

    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = single_fold::fold(&create_ops, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");

    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the catalog live schema");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };

    // THE SHAPE DECISION ITSELF, through its only observable projection:
    // `column_renames` is populated EXACTLY when `preserve_stored_shape` holds
    // (`pure_rename.filter(|_| preserve_stored_shape)`), so a non-empty list here is a
    // direct assertion that `pure_sqlite_column_rename` still matched and the stored
    // shape is still preserved. This is the pin the whole fix is built around: a
    // rewrite of `definition` into the DESIRED snapshot would make it `[]`, because
    // `ConstraintSnapshot::eq` compares `definition`.
    assert_eq!(
        rebuild.spec.column_renames,
        vec![(OLD_COLUMN.to_string(), NEW_COLUMN.to_string())],
        "the catalog-sourced rename is still a PURE rename with the stored shape \
         preserved: {:?}",
        rebuild.spec
    );

    // And the emitted CREATE is the STORED body, still naming the pre-rename column,
    // with only the CREATE target re-pointed at the temporary table.
    let create_sql = &rebuild.spec.new_table_create;
    assert!(
        create_sql.contains(&format!("FOREIGN KEY ({OLD_COLUMN}) REFERENCES")),
        "the replay arm reproduces the STORED body verbatim, still naming the \
         pre-rename local column - SQLite's own RENAME COLUMN moves it after the swap, \
         and the rewrite must not touch this:\n{create_sql}"
    );
    // And it IS the stored body, not a re-render of the snapshot. Compared with
    // whitespace collapsed rather than byte for byte, because the replay arm
    // re-splices the FK clauses (`rewrite_sqlite_stored_foreign_keys`) and drops the
    // space after the preceding comma - a pre-existing detail of that splice, not
    // something this rename touches. Every TOKEN is the stored one.
    fn squeeze(sql: &str) -> String {
        sql.chars().filter(|c| !c.is_whitespace()).collect()
    }
    assert_eq!(
        create_sql.split_once('(').map(|(_, body)| squeeze(body)),
        stored_before.split_once('(').map(|(_, body)| squeeze(body)),
        "the stored body token for token, only the CREATE target differs"
    );

    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the catalog-sourced rebuild applies");

    let stored = backend
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = '{TABLE}'"
        ))
        .await
        .expect("read the stored CREATE");
    let stored = stored[0][0].as_deref().expect("the stored CREATE text");
    assert!(
        stored.contains(&format!("FOREIGN KEY (\"{NEW_COLUMN}\") REFERENCES"))
            || stored.contains(&format!("FOREIGN KEY ({NEW_COLUMN}) REFERENCES")),
        "SQLite's own RENAME COLUMN moved the foreign key on this leg: {stored}"
    );
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let rejected = backend
        .actor()
        .exec(&child_sql("orphan", NEW_COLUMN, "nobody"))
        .await;
    assert!(
        rejected.is_err(),
        "and the replayed foreign key enforces on the post-rename column"
    );
}

/// TWO renames in one history: the fold's `Op::RenameColumn` arm has to follow the
/// definition too, because its output is the NEXT rebuild's live source. It already
/// does (`rename_constraint_definition_column`, kind-gated to the three definitions
/// whose leading group is a local column list); this measures it end to end so the
/// second rebuild cannot silently start from a body the first rename already moved.
#[compio::test]
async fn a_second_rename_starts_from_a_folded_definition_the_first_rename_already_moved() {
    let effective = charter();
    let p = paths("fk_definition_twice");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create_ops = deploy_create(&backend, &engine).await;

    let first = rename_ir();
    let steps = author
        .lower_steps(&first, &folded_live_schema(&create_ops))
        .expect("the first rename lowers");
    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the first rebuild applies");

    let mut history = create_ops.clone();
    history.extend(first.ops.iter().cloned());

    // The FOLD's own output, before any rebuild touches it.
    let folded =
        fold_ops(&history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let fk = folded.tables[TABLE]
        .constraints
        .iter()
        .find(|constraint| constraint.kind == "FOREIGN KEY")
        .expect("the folded snapshot carries the foreign key");
    assert!(
        fk.definition.contains(&format!(
            "FOREIGN KEY ({NEW_COLUMN}) REFERENCES {PARENT}({OLD_COLUMN})"
        )),
        "the FOLD moved the LOCAL column and left the REFERENCED one alone: {}",
        fk.definition
    );

    let second = rename_ir_named("rename_holder_id", NEW_COLUMN, "keeper_id");
    let steps = author
        .lower_steps(&second, &folded_live_schema(&history))
        .expect("the second rename lowers");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    assert!(
        rebuild.spec.new_table_create.contains(&format!(
            "FOREIGN KEY (keeper_id) REFERENCES {PARENT}({OLD_COLUMN})"
        )),
        "the second rebuild's foreign key names the twice-renamed local column:\n{}",
        rebuild.spec.new_table_create
    );

    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the second rebuild applies");

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&seed_parent_sql("carol"))
        .await
        .expect("a parent");
    let rejected = backend
        .actor()
        .exec(&child_sql("i9", "keeper_id", "nobody"))
        .await;
    assert!(
        rejected.is_err(),
        "and the twice-moved foreign key still enforces"
    );
    backend
        .actor()
        .exec(&child_sql("i8", "keeper_id", "carol"))
        .await
        .expect("while a real parent key is still accepted");
}
