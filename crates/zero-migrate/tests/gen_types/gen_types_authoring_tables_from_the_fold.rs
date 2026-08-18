//! **Step 4, consumer 2: the authoring tables come from the single fold.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 4 moves the artifact
//! consumers off their private walkers one at a time.
//! `authoring_tables_from_ops` is the second, and it has the SMALLEST blast radius of
//! the four: measured, it is produced in exactly one place in `render_artifacts` and
//! read in exactly one place, `render_env_db_ts`. So this move can move bytes in
//! `env.db.ts` and in no other artifact, where consumer 1 could move two.
//!
//! # What the move can actually change, measured rather than assumed
//!
//! [`zero_migrate::render_artifacts`] emits two files. `schema.runtime.json` is
//! rendered from the `FieldDef` map plus the runtime-metadata projection, and consumer 2
//! touches neither - so its content hash is a CONTROL here, pinned in the corpus golden
//! beside `env.db.ts`'s and expected not to move for THIS consumer's reasons.
//!
//! STEP 4 CONSUMER 3 MOVED SIX OF THOSE CONTROL LINES, and the control working is the
//! reason they are worth reading rather than a reason to loosen it. That consumer
//! replaced `fold_to_field_defs` - the walker that produced the `fields` block - and its
//! own golden records five families in which the walker described a database the catalog
//! does not have. Two of those families are reachable from carriers in THIS file:
//! `carrier:attached_partition_dropped` (a dropped partition stayed in the map) and
//! `carrier:unique_constraint_lifecycle` (a dropped `UNIQUE` outlived its constraint).
//! Six `sha|runtime.json` lines moved, on those two carriers, on three dialects each.
//!
//! What did NOT move is the claim this file is actually about: ZERO `sha|env.db.ts`
//! lines and ZERO per-field lines changed, so consumer 3 is confined to the artifact it
//! owns. The two goldens agree on the new hashes independently, having been reduced by
//! different code from the same `render_artifacts` call.
//!
//! `env.db.ts` is rendered from `AuthoringTable`, whose six fields reach it like this:
//!
//! | field | where it lands in `env.db.ts` |
//! |---|---|
//! | `columns` | the `columns: { … }` block, one rendered expression per column |
//! | `primary_key` | `.primaryKey()` on the column when the key is single, the table-level `primaryKey: [ … ]` clause when it is composite, and `primaryKey: null` when there is none |
//! | `constraints` | `uniques:`, `checks:`, `foreignKeys:`, `exclusions:`, plus the `.references(…)` lift onto a column |
//! | `indexes` | the `indexes: [ … ]` block |
//! | `partition_by` | `partitionBy: …` |
//! | `schema` | `schema: …` |
//!
//! The `options: { … }` line is the ONE thing in a table block that does not come
//! from this map - it is the runtime-metadata projection consumer 1 moved - and it is
//! carried in the golden as a second control.
//!
//! Every one of those six is probed by field below, and
//! [`the_recorded_corpus_renders_the_same_artifacts_through_the_fold`] pins the whole
//! of both artifacts by content hash so a byte moving anywhere else cannot pass
//! unnoticed either.
//!
//! # The defect this move FIXES, and why it is a fix rather than a change
//!
//! `authoring_tables_from_ops` has no `Op::AlterPrimaryKey` arm at all - measured,
//! zero occurrences of `AlterPrimaryKey` in `render/gen_types.rs` against 29 in
//! `render/fold.rs` - so the op fell through its `_ => {}` and `env.db.ts` kept
//! declaring the primary key the migration replaced, dropped or added. The step 3
//! gate recorded it as `ATO_IGNORES_ALTER_PRIMARY_KEY` on one stream and one action;
//! measured through the artifact, it is FIVE distinct wrong artifacts, one per
//! `AlterPrimaryKeyAction` shape plus the identity facet the same op clears:
//!
//! * `replace` to a single column - the key stays on the old column,
//! * `replace` to a composite - the table-level `primaryKey:` clause never appears,
//! * `drop` - `primaryKey: null` never appears,
//! * `add` - `primaryKey: null` survives an op that installed a key,
//! * `replace` with `dropIdentityFrom` - `.autoIncrement()` outlives the identity the
//!   same op removed.
//!
//! Which side is right is NOT decided here by preference. It is decided by a live
//! PostgreSQL server in `tests/env_db_ts_primary_key_matches_the_server_pg.rs`, which
//! applies the migration for real, reads the key out of `pg_catalog`, and asserts
//! `env.db.ts` declares THAT key. This file's offline arms pin the same answers so
//! a DB-free run still fails when the artifact regresses.
//!
//! # The second divergence, which is a CHOICE and is adjudicated as one
//!
//! `AuthoredState::advance` removes a table on `Op::DropPartition`; the walker has no
//! arm for it. The pair is only reachable when a table is created by `createTable`
//! and then made a partition by `attachPartition`, which is the case
//! `single_fold.rs` recorded as UNMEASURED by the step 1 corpus. It stops being
//! unmeasured here: [`dropping_an_attached_partition_removes_it_from_env_db_ts`]
//! measures it, and `attachPartition is PostgreSQL-only` at lowering
//! (`render/lower.rs`), so PostgreSQL is the only dialect on which the stream is
//! applicable at all - and there `dropPartition` lowers to `DROP TABLE`, already
//! live-anchored by `tests/pg_scenarios.rs` scenario 12 and by
//! `tests/partition_claims_the_relation_namespace_pg.rs::dropping_a_partition_frees_its_name`.
//! `detachPartition`, whose child survives as a standalone table under the same name,
//! is the control that stops "remove it" from being applied to the wrong op.
//!
//! Offline throughout, except where a test name says `_pg`: the oracle here is the
//! emitted artifact, so there is no skip that could read as a pass.

use crate::support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use zero_migrate::manifest_entry::sha256_hex;
use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::{render_artifacts, EffectivePolicy, SqlDialect};

const SCHEMA: &str = "public";

const DIALECTS: [SqlDialect; 3] = [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql];

fn parse(ops: &str) -> Vec<Op> {
    serde_json::from_str(ops).expect("the stream parses")
}

fn env_db_ts(ops: &[Op], dialect: SqlDialect, policy: &EffectivePolicy) -> String {
    render_artifacts(ops, dialect, SCHEMA, policy)
        .expect("the stream renders artifacts")
        .env_db_ts
}

// ---------------------------------------------------------------------------
// Reading `env.db.ts` per FIELD
// ---------------------------------------------------------------------------

/// The lines of one table's block in `env.db.ts`, split into the SIX fields
/// `AuthoringTable` owns plus the two controls that share the block.
///
/// Per-field on purpose, and read out of the ARTIFACT rather than out of the map that
/// produced it. A probe that compared two `AuthoringTable` values would be asserting
/// at an internal boundary; the thing a regenerated app is built from is this text.
#[derive(Debug, Default, PartialEq, Eq)]
struct TableBlock {
    /// FIELD `columns`: one rendered expression per column, in declaration order.
    columns: Vec<String>,
    /// FIELD `primary_key`, as the artifact spells it. `single:<column>` when the key
    /// rides on a column as `.primaryKey()`, the literal `primaryKey:` clause when it
    /// is composite or absent, and `<implicit>` when the table block states nothing -
    /// which is what a single-column key renders as.
    primary_key: String,
    /// FIELD `constraints`, across all four rendered kinds.
    constraints: Vec<String>,
    /// FIELD `indexes`.
    indexes: Vec<String>,
    /// FIELD `partition_by`.
    partition_by: Option<String>,
    /// FIELD `schema`.
    schema: Option<String>,
    /// CONTROL: the runtime options line, which consumer 1 moved and this move must
    /// leave exactly where it is.
    options: Option<String>,
    /// CONTROL: anything the classifier did not recognise. Non-empty means the
    /// emitter grew a section this probe cannot see, and every assertion below would
    /// be silently narrower than it reads.
    unclassified: Vec<String>,
}

/// Split a rendered `env.db.ts` into its table blocks, each split per field.
///
/// The emitter indents a table key by two spaces and everything inside it by four, so
/// the split is structural rather than a regex over names.
fn table_blocks(env_db_ts: &str) -> std::collections::BTreeMap<String, TableBlock> {
    let mut out = std::collections::BTreeMap::new();
    let mut table: Option<(String, TableBlock)> = None;
    // Which multi-line section the reader is inside, if any.
    let mut section: Option<&'static str> = None;
    for line in env_db_ts.lines() {
        let Some((name, block)) = table.as_mut() else {
            if let Some(rest) = line.strip_prefix("  ") {
                if let Some(key) = rest.strip_suffix(": {") {
                    if !key.starts_with(' ') {
                        table = Some((unquote_js_key(key), TableBlock::default()));
                    }
                }
            }
            continue;
        };
        if line == "  }," {
            let (name, block) = table.take().expect("inside a table block");
            out.insert(name, block);
            continue;
        }
        if let Some(open) = section {
            if line == "    }," || line == "    ]," {
                section = None;
                continue;
            }
            let entry = line.trim().to_string();
            match open {
                "columns" => block.columns.push(entry),
                "constraints" => block.constraints.push(entry),
                "indexes" => block.indexes.push(entry),
                _ => block.unclassified.push(entry),
            }
            continue;
        }
        let trimmed = line.trim();
        match trimmed {
            "columns: {" => section = Some("columns"),
            "uniques: [" | "checks: [" | "foreignKeys: [" | "exclusions: [" => {
                section = Some("constraints");
            }
            "indexes: [" => section = Some("indexes"),
            _ if trimmed.starts_with("primaryKey: ") => {
                block.primary_key = trimmed.trim_end_matches(',').to_string();
            }
            _ if trimmed.starts_with("partitionBy: ") => {
                block.partition_by = Some(trimmed.trim_end_matches(',').to_string());
            }
            _ if trimmed.starts_with("schema: ") => {
                block.schema = Some(trimmed.trim_end_matches(',').to_string());
            }
            _ if trimmed.starts_with("options: ") => {
                block.options = Some(trimmed.trim_end_matches(',').to_string());
            }
            _ => block.unclassified.push(trimmed.to_string()),
        }
        let _ = name;
    }
    // A single-column primary key renders as a COLUMN modifier and leaves no
    // table-level clause, so the field is read off the columns instead of being
    // reported absent.
    for block in out.values_mut() {
        if block.primary_key.is_empty() {
            let single = block
                .columns
                .iter()
                .find(|column| column.contains(".primaryKey()"))
                .and_then(|column| column.split(':').next())
                .map(str::trim);
            block.primary_key = match single {
                Some(column) => format!("single:{}", unquote_js_key(column)),
                None => "<implicit>".to_string(),
            };
        }
    }
    out
}

fn unquote_js_key(key: &str) -> String {
    key.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .replace("\\\"", "\"")
}

/// One table's block, or a failure that names the tables the artifact DOES carry.
#[track_caller]
fn block(env_db_ts: &str, table: &str) -> TableBlock {
    let mut blocks = table_blocks(env_db_ts);
    let names: Vec<String> = blocks.keys().cloned().collect();
    blocks.remove(table).unwrap_or_else(|| {
        panic!("`env.db.ts` must carry a block for `{table}`; it carries {names:?}:\n{env_db_ts}")
    })
}

/// The probe is only a probe if it can actually see the emitter's output. A
/// classifier that silently bucketed everything into `unclassified` would make every
/// per-field assertion below vacuous in the direction that reads as green.
#[test]
fn the_per_field_reader_is_not_a_broken_instrument() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","schema":"reporting","columns":[
    {"name":"tenant","type":"text","nullable":false},
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text","nullable":false},
    {"name":"bucket","type":"int","nullable":false}],
   "primaryKey":["tenant","id"],
   "constraints":[{"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}}],
   "indexes":[{"name":"users_bucket_idx","columns":[{"kind":"column","name":"bucket"}]}],
   "partitionBy":{"kind":"range","columns":["bucket"],"collapse":true},
   "runtimeOptions":{"softDelete":true,"versioning":false}}
]"#,
    );
    let text = env_db_ts(&ops, SqlDialect::Postgres, &support::no_inject(SCHEMA));
    let block = block(&text, "users");
    assert_eq!(
        block.columns.len(),
        4,
        "the reader must see every column line: {block:#?}\n{text}"
    );
    assert_eq!(
        block.primary_key, "primaryKey: [\"tenant\", \"id\"]",
        "the reader must see a composite primary key: {block:#?}"
    );
    assert_eq!(
        block.constraints.len(),
        1,
        "the reader must see a constraint line: {block:#?}"
    );
    assert_eq!(
        block.indexes.len(),
        1,
        "the reader must see an index line: {block:#?}"
    );
    assert!(
        block.partition_by.is_some(),
        "the reader must see `partitionBy`: {block:#?}"
    );
    assert!(
        block.schema.is_some(),
        "the reader must see `schema`: {block:#?}"
    );
    assert!(
        block.options.is_some(),
        "the reader must see the runtime `options` control: {block:#?}"
    );
    assert!(
        block.unclassified.is_empty(),
        "the emitter produced a line this reader cannot classify, so every per-field \
         assertion in this file is narrower than it reads: {:?}\n{text}",
        block.unclassified
    );
}

// ---------------------------------------------------------------------------
// The op the walker never had an arm for, per action shape
// ---------------------------------------------------------------------------

/// The stream shapes the five `alterPrimaryKey` arms below share, so the golden and
/// the probes drive the SAME text rather than two hand-written lookalikes.
const PK_REPLACE_SINGLE: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"]}}
]"#;

const PK_REPLACE_COMPOSITE: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"tenant","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"orders_pair_uq","kind":{"kind":"unique","columns":["tenant","legacy_id"]}}]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["tenant","legacy_id"]}}
]"#;

const PK_DROP: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"drop","expectedColumns":["id"]}}
]"#;

const PK_ADD: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"add","columns":["legacy_id"]}}
]"#;

const PK_REPLACE_DROPS_IDENTITY: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"int","nullable":false,"identity":{"always":false}},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"],"dropIdentityFrom":["id"]}}
]"#;

const PK_REPLACE_THEN_RENAMED: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"]}},
  {"op":"renameColumn","table":"orders","from":"legacy_id","to":"external_id","type":"text"}
]"#;

const PK_REPLACE_THEN_DROP: &str = r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"]}},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"drop","expectedColumns":["legacy_id"]}}
]"#;

/// `replace` onto a single column moves `.primaryKey()` off the old column and onto
/// the new one. FIELD `primary_key`, and FIELD `columns` beside it because the key
/// rides on a column expression rather than on a clause of its own.
#[test]
fn a_replaced_single_column_primary_key_moves_in_env_db_ts() {
    for dialect in DIALECTS {
        let text = env_db_ts(
            &parse(PK_REPLACE_SINGLE),
            dialect,
            &support::no_inject(SCHEMA),
        );
        let block = block(&text, "orders");
        assert_eq!(
            block.primary_key, "single:legacy_id",
            "{dialect:?}: FIELD `primary_key` -- the replace installed `legacy_id`, so \
             regenerating an app from this artifact must not reinstate `id`:\n{text}"
        );
        assert!(
            block
                .columns
                .iter()
                .any(|c| c.starts_with("id: ") && !c.contains(".primaryKey()")),
            "{dialect:?}: FIELD `columns` -- `id` is no longer the key:\n{text}"
        );
    }
}

/// `replace` onto a COMPOSITE key renders through a different code path in
/// `render_table` - the table-level clause rather than the column modifier - so it is
/// asserted separately rather than assumed to follow from the single-column arm.
#[test]
fn a_replaced_composite_primary_key_reaches_the_table_level_clause() {
    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
        let text = env_db_ts(
            &parse(PK_REPLACE_COMPOSITE),
            dialect,
            &support::no_inject(SCHEMA),
        );
        let block = block(&text, "orders");
        assert_eq!(
            block.primary_key, "primaryKey: [\"tenant\", \"legacy_id\"]",
            "{dialect:?}: FIELD `primary_key` -- a composite key renders as the \
             table-level clause, in the ORDER the op installed:\n{text}"
        );
        assert!(
            block.columns.iter().all(|c| !c.contains(".primaryKey()")),
            "{dialect:?}: FIELD `columns` -- no column carries the single-key modifier \
             once the key is composite:\n{text}"
        );
    }
    // SQLite refuses a `createTable` table-level UNIQUE, so this stream has no SQLite
    // leg to assert. Stated rather than silently dropped from the loop.
    assert!(
        render_artifacts(
            &parse(PK_REPLACE_COMPOSITE),
            SqlDialect::Sqlite,
            SCHEMA,
            &support::no_inject(SCHEMA)
        )
        .is_err(),
        "the composite stream is expected to be refused on SQLite for an unrelated \
         reason (table-level UNIQUE); if it starts rendering, this arm owes it a case"
    );
}

/// `drop` leaves the table with no key at all, which the emitter spells
/// `primaryKey: null`.
#[test]
fn a_dropped_primary_key_leaves_env_db_ts_declaring_none() {
    for dialect in DIALECTS {
        let text = env_db_ts(&parse(PK_DROP), dialect, &support::no_inject(SCHEMA));
        let block = block(&text, "orders");
        assert_eq!(
            block.primary_key, "primaryKey: null",
            "{dialect:?}: FIELD `primary_key` -- the op dropped the key:\n{text}"
        );
    }
}

/// `add` installs a key on a table that had none, so `primaryKey: null` must go.
#[test]
fn an_added_primary_key_replaces_the_null_in_env_db_ts() {
    for dialect in DIALECTS {
        let text = env_db_ts(&parse(PK_ADD), dialect, &support::no_inject(SCHEMA));
        let block = block(&text, "orders");
        assert_eq!(
            block.primary_key, "single:legacy_id",
            "{dialect:?}: FIELD `primary_key` -- the op ADDED a key, and an artifact \
             still saying `null` describes a table the database does not have:\n{text}"
        );
    }
}

/// The same op clears the identity facet it names in `dropIdentityFrom`, and
/// `env.db.ts` spells identity as `.autoIncrement()`. FIELD `columns`.
#[test]
fn a_primary_key_replace_clears_the_identity_it_drops() {
    for dialect in DIALECTS {
        let text = env_db_ts(
            &parse(PK_REPLACE_DROPS_IDENTITY),
            dialect,
            &support::no_inject(SCHEMA),
        );
        let block = block(&text, "orders");
        assert!(
            block
                .columns
                .iter()
                .any(|c| c.starts_with("id: ") && !c.contains(".autoIncrement()")),
            "{dialect:?}: FIELD `columns` -- `dropIdentityFrom` removed the identity, \
             so the artifact must not still generate values for it:\n{text}"
        );
        assert_eq!(
            block.primary_key, "single:legacy_id",
            "{dialect:?}: FIELD `primary_key` -- and the key moved in the same op:\n{text}"
        );
    }
}

/// The installed key is STATE, so a later `renameColumn` has to follow it. A model
/// that merely echoed the op's column list would keep the pre-rename name.
#[test]
fn a_rename_after_a_primary_key_replace_follows_the_new_key() {
    for dialect in DIALECTS {
        let text = env_db_ts(
            &parse(PK_REPLACE_THEN_RENAMED),
            dialect,
            &support::no_inject(SCHEMA),
        );
        let block = block(&text, "orders");
        assert_eq!(
            block.primary_key, "single:external_id",
            "{dialect:?}: FIELD `primary_key` -- the key the replace installed follows \
             the rename that came after it:\n{text}"
        );
    }
}

/// Two `alterPrimaryKey` ops compose: the second sees the key the first installed.
/// This is the corpus stream `v_primary_key`, which is where the step 3 gate recorded
/// the divergence, driven end to end through the artifact.
#[test]
fn two_primary_key_ops_compose_in_env_db_ts() {
    for dialect in DIALECTS {
        let text = env_db_ts(
            &parse(PK_REPLACE_THEN_DROP),
            dialect,
            &support::no_inject(SCHEMA),
        );
        let block = block(&text, "orders");
        assert_eq!(
            block.primary_key, "primaryKey: null",
            "{dialect:?}: FIELD `primary_key` -- replace then drop leaves no key:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// The other divergence: a dropped partition
// ---------------------------------------------------------------------------

const ATTACHED_PARTITION: &str = r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}}
]"#;

fn attached_then(tail: &str) -> Vec<Op> {
    let mut ops = parse(ATTACHED_PARTITION);
    ops.extend(parse(tail));
    ops
}

/// `dropPartition` on a child that was authored as a `createTable` removes it.
///
/// On PostgreSQL - the only dialect where `attachPartition` lowers at all
/// (`render/lower.rs`: "attachPartition is PostgreSQL-only") - `dropPartition` lowers
/// to `DROP TABLE`, and that the relation genuinely goes is already measured against
/// a live server by `tests/pg_scenarios.rs` scenario 12 and by
/// `tests/partition_claims_the_relation_namespace_pg.rs::dropping_a_partition_frees_its_name`.
/// An artifact that kept the table would tell a regenerated app to recreate a
/// relation the migration removed.
#[test]
fn dropping_an_attached_partition_removes_it_from_env_db_ts() {
    let ops = attached_then(r#"[{"op":"dropPartition","parent":"par","name":"p1"}]"#);
    for dialect in DIALECTS {
        let text = env_db_ts(&ops, dialect, &support::no_inject(SCHEMA));
        let blocks = table_blocks(&text);
        assert!(
            !blocks.contains_key("p1"),
            "{dialect:?}: the drop took the relation, so `env.db.ts` must not still \
             describe it:\n{text}"
        );
        // The PARENT survives its child. Without this the arm above would pass if the
        // whole artifact went empty.
        assert!(
            blocks.contains_key("par"),
            "{dialect:?}: the parent is untouched:\n{text}"
        );
    }
}

/// **The control that shapes the arm above.** A DETACHED partition becomes a
/// standalone table under the same name - the rule
/// `tests/partition_claims_the_relation_namespace_pg.rs::detaching_a_partition_does_not_free_its_name`
/// states and enforces - so `detachPartition` must NOT remove it. Without this arm,
/// "a partition op removes the table" would look equally justified and would be wrong.
#[test]
fn detaching_a_partition_keeps_it_in_env_db_ts() {
    let ops = attached_then(r#"[{"op":"detachPartition","parent":"par","name":"p1"}]"#);
    for dialect in DIALECTS {
        let text = env_db_ts(&ops, dialect, &support::no_inject(SCHEMA));
        assert!(
            table_blocks(&text).contains_key("p1"),
            "{dialect:?}: a detached partition is still a table under the same name, \
             so the artifact keeps it:\n{text}"
        );
    }
}

/// The second control: while the child is merely ATTACHED, it stays. Only the drop
/// removes it, so the arm above cannot be passing because `attachPartition` removed
/// the table early.
#[test]
fn an_attached_partition_is_still_a_table_in_env_db_ts() {
    let ops = parse(ATTACHED_PARTITION);
    for dialect in DIALECTS {
        let text = env_db_ts(&ops, dialect, &support::no_inject(SCHEMA));
        assert!(
            table_blocks(&text).contains_key("p1"),
            "{dialect:?}: attaching does not remove the relation:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// The five fields that must NOT move
// ---------------------------------------------------------------------------

/// A stream that exercises `columns`, `constraints`, `indexes`, `partition_by` and
/// `schema` at once, across the rename and drop carriers that rewrite them.
///
/// This is the "nothing else moved" probe at field resolution. The corpus golden says
/// the same thing by hash; this one says WHICH field would have moved.
const EVERY_OTHER_FIELD: &str = r#"[
  {"op":"createSchema","name":"reporting","ifNotExists":true},
  {"op":"createTable","name":"accounts","schema":"reporting","columns":[{"name":"id","type":"text","nullable":false},{"name":"bucket","type":"int","nullable":false}],"primaryKey":["id"],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":true}},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"qty","type":"int"},{"name":"note","type":"text"},{"name":"owner_id","type":"text"},{"name":"doubled","type":"int","generated":{"expr":{"node":"binOp","op":"mul","lhs":{"node":"colRef","name":"qty"},"rhs":{"node":"literal","value":2}},"stored":true}}],"primaryKey":["id"],"constraints":[{"name":"orders_qty_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"qty"},"rhs":{"node":"literal","value":0}}}},{"name":"orders_note_uq","kind":{"kind":"unique","columns":["note"]}}],"indexes":[{"name":"orders_qty_idx","columns":[{"kind":"column","name":"qty"}]}]},
  {"op":"renameColumn","table":"orders","from":"qty","to":"quantity","type":"int"},
  {"op":"setColumnType","table":"orders","column":"note","toType":{"string":{"length":40}}},
  {"op":"dropColumn","table":"orders","column":"owner_id"}
]"#;

/// PostgreSQL only, and the reason is the stream rather than the change: a
/// `createTable` table-level CHECK is a PostgreSQL-only capability
/// (`fold: createTable table-level CHECK is PostgreSQL-only`), so the other two
/// dialects refuse this stream outright. The corpus golden still drives all three and
/// records those refusals, so the off-Postgres legs are pinned even though this probe
/// cannot assert fields on them.
#[test]
fn every_other_field_of_the_authoring_map_reaches_env_db_ts_unchanged() {
    let dialect = SqlDialect::Postgres;
    {
        let text = env_db_ts(
            &parse(EVERY_OTHER_FIELD),
            dialect,
            &support::no_inject(SCHEMA),
        );
        let orders = block(&text, "orders");
        // FIELD `columns`: the rename moved the name AND the generated expression that
        // spells it, the retype re-derived the parameterised facet, and the drop
        // removed a column.
        assert_eq!(
            orders.columns.len(),
            4,
            "{dialect:?}: FIELD `columns` -- one column was dropped: {orders:#?}"
        );
        assert!(
            orders.columns.iter().any(|c| c.starts_with("quantity: ")),
            "{dialect:?}: FIELD `columns` -- the rename landed: {orders:#?}"
        );
        assert!(
            orders
                .columns
                .iter()
                .any(|c| c.starts_with("doubled: ") && c.contains("quantity")),
            "{dialect:?}: FIELD `columns` -- the generated expression follows the \
             rename: {orders:#?}"
        );
        assert!(
            orders
                .columns
                .iter()
                .any(|c| c.starts_with("note: ") && c.contains("40")),
            "{dialect:?}: FIELD `columns` -- the retype re-derived `maxLength`: \
             {orders:#?}"
        );
        // FIELD `constraints`: the CHECK body follows the rename; the UNIQUE survives.
        assert!(
            orders
                .constraints
                .iter()
                .any(|c| c.contains("orders_qty_ck") && c.contains("quantity")),
            "{dialect:?}: FIELD `constraints` -- the CHECK body follows the rename: \
             {orders:#?}"
        );
        assert!(
            orders
                .constraints
                .iter()
                .any(|c| c.contains("orders_note_uq")),
            "{dialect:?}: FIELD `constraints` -- the UNIQUE survives: {orders:#?}"
        );
        // FIELD `indexes`: the key column follows the rename.
        assert!(
            orders
                .indexes
                .iter()
                .any(|i| i.contains("orders_qty_idx") && i.contains("quantity")),
            "{dialect:?}: FIELD `indexes` -- the index key follows the rename: \
             {orders:#?}"
        );
        // FIELDS `partition_by` and `schema`, on the other table.
        let accounts = block(&text, "accounts");
        assert_eq!(
            accounts.partition_by.as_deref(),
            Some("partitionBy: { range: [\"bucket\"], whenUnsupported: \"collapse\" }"),
            "{dialect:?}: FIELD `partition_by`"
        );
        assert_eq!(
            accounts.schema.as_deref(),
            Some("schema: \"reporting\""),
            "{dialect:?}: FIELD `schema`"
        );
    }
}

// ---------------------------------------------------------------------------
// The corpus: both artifacts, whole, on real recorded streams
// ---------------------------------------------------------------------------

/// The recorded op fixtures, the same 27 `tests/op_fixture_goldens.rs` owns and the
/// same list consumer 1's gate drives. Real drained recorder envelopes, already
/// policy-resolved, so they fold under the confined charter that produced them.
const STEMS: [&str; 27] = [
    "alter_primary_key",
    "comments_indexes",
    "constraint_not_valid",
    "ddl_addcol_constraints",
    "ddl_alter",
    "ddl_create",
    "ddl_drop",
    "ddl_rename_table",
    "dialectal_ops",
    "dml",
    "dml_upsert",
    "edge_scalars",
    "enums_domains",
    "fluent_ddl",
    "fluent_dml",
    "fluent_scalars",
    "fluent_scalars_dml",
    "grouped_views",
    "in_list_scalars",
    "p2a_facets",
    "partition",
    "pg_aggregates",
    "pg_vendor",
    "runtime_options",
    "sequences_exclusion",
    "synchronize_identity",
    "views",
];

/// The carrier streams, folded into the SAME golden as the recorded fixtures.
///
/// They are here because the recorded corpus does not cover what moved.
/// `the_corpus_golden_actually_covers_the_map_that_moved` is the assertion that keeps
/// that statement true: it counts, in the golden itself, the primary-key spellings
/// and the partition rows this move is about.
///
/// The brief for this move warned that the step 1 corpus holds exactly ONE
/// `unique: true` column and that a gate blind to a carrier proves nothing about it.
/// The same warning applies to KEY naming, which is what this move touches, so the
/// carriers below cross a primary key with every shape that renders differently:
/// single, composite, absent, injected, renamed, dropped-with-its-column.
const CARRIERS: &[(&str, &str)] = &[
    ("pk_replace_single", PK_REPLACE_SINGLE),
    ("pk_replace_composite", PK_REPLACE_COMPOSITE),
    ("pk_drop", PK_DROP),
    ("pk_add", PK_ADD),
    ("pk_replace_drops_identity", PK_REPLACE_DROPS_IDENTITY),
    ("pk_replace_then_renamed", PK_REPLACE_THEN_RENAMED),
    ("pk_replace_then_drop", PK_REPLACE_THEN_DROP),
    (
        "pk_column_dropped",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"dropColumn","table":"orders","column":"id"}
]"#,
    ),
    (
        "pk_column_renamed",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"renameColumn","table":"orders","from":"id","to":"order_id","type":"text"}
]"#,
    ),
    (
        "pk_composite_from_create_table",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"tenant","type":"text","nullable":false},{"name":"id","type":"text","nullable":false}],"primaryKey":["tenant","id"]},
  {"op":"renameColumn","table":"orders","from":"tenant","to":"account","type":"text"}
]"#,
    ),
    ("attached_partition", ATTACHED_PARTITION),
    (
        "attached_partition_dropped",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"dropPartition","parent":"par","name":"p1"}
]"#,
    ),
    (
        "attached_partition_detached",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"detachPartition","parent":"par","name":"p1"}
]"#,
    ),
    (
        "created_partition_dropped",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createPartition","name":"p1","of":"par","bounds":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"dropPartition","parent":"par","name":"p1"}
]"#,
    ),
    ("every_other_field", EVERY_OTHER_FIELD),
    // The `constraints` field renders through four blocks and the recorded fixtures
    // reach almost none of them: measured on the first capture of this golden, the 27
    // fixtures plus the carriers above produced only TEN `constraint` rows, because a
    // single-column FK is LIFTED onto the column as `.references(…)` and never
    // reaches a `foreignKeys:` block. A field this move could break with ten rows of
    // evidence is the corpus narrowing that `the_corpus_golden_actually_covers_the_map_that_moved`
    // exists to refuse, so the three streams below were added to cover it rather than
    // the floor being lowered to match.
    (
        "composite_foreign_key",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"tenant","type":"text","nullable":false},{"name":"id","type":"text","nullable":false}],"primaryKey":["tenant","id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"tenant","type":"text","nullable":false},{"name":"account_id","type":"text","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"orders_account_fk","kind":{"kind":"fk","columns":["tenant","account_id"],"referencesTable":"accounts","referencesColumns":["tenant","id"],"onDelete":"cascade"}}]}
]"#,
    ),
    (
        "composite_foreign_key_after_renames",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"tenant","type":"text","nullable":false},{"name":"id","type":"text","nullable":false}],"primaryKey":["tenant","id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"tenant","type":"text","nullable":false},{"name":"account_id","type":"text","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"orders_account_fk","kind":{"kind":"fk","columns":["tenant","account_id"],"referencesTable":"accounts","referencesColumns":["tenant","id"],"onDelete":"cascade"}}]},
  {"op":"renameColumn","table":"accounts","from":"id","to":"account_key","type":"text"},
  {"op":"renameTable","table":"accounts","to":"tenants"}
]"#,
    ),
    (
        "unique_constraint_lifecycle",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false},{"name":"handle","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}}},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_handle_uq","kind":{"kind":"unique","columns":["handle"]}}},
  {"op":"dropConstraint","table":"users","name":"users_handle_uq"}
]"#,
    ),
    (
        "table_renamed_with_fk_and_expressions",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"],"constraints":[{"name":"orders_owner_fk","kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}]},
  {"op":"renameTable","table":"accounts","to":"tenants"}
]"#,
    ),
    (
        "table_dropped_and_recreated",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"dropTable","table":"users"},
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"handle","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]}
]"#,
    ),
];

const CORPUS_GOLDEN: &str = "tests/goldens/authoring_tables_artifacts.txt";

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read_stem(stem: &str) -> Vec<Op> {
    let path = manifest_path(&format!("tests/op_fixtures/{stem}.golden.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str::<MigrationIr>(&text)
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
        .ops
}

/// One stream under one dialect, reduced to lines.
///
/// Three kinds of line, and all three are needed:
///
/// * a `sha` line per artifact - the WHOLE artifact. `env.db.ts`'s is the thing under
///   test; `runtime.json`'s is the CONTROL, because this move must not touch it at
///   all and a hash is the only way to say "nothing else";
/// * one line per FIELD of `AuthoringTable` as `env.db.ts` spells it, so when a `sha`
///   line moves the lines beside it name the field instead of printing two hashes;
/// * an `unclassified` line whenever the reader could not bucket something, so a
///   golden captured through a blind reader cannot look complete.
fn corpus_lines(
    stem: &str,
    ops: &[Op],
    policy: &EffectivePolicy,
    dialect: SqlDialect,
    out: &mut Vec<String>,
) {
    let d = format!("{dialect:?}");
    let rendered = match render_artifacts(ops, dialect, SCHEMA, policy) {
        Ok(rendered) => rendered,
        Err(error) => {
            out.push(format!("{stem}|{d}|refused|{error}"));
            return;
        }
    };
    out.push(format!(
        "{stem}|{d}|sha|env.db.ts|{}",
        sha256_hex(rendered.env_db_ts.as_bytes())
    ));
    out.push(format!(
        "{stem}|{d}|sha|runtime.json|{}",
        sha256_hex(rendered.runtime_json.as_bytes())
    ));
    for (table, block) in table_blocks(&rendered.env_db_ts) {
        out.push(format!(
            "{stem}|{d}|primary_key|{table}|{}",
            block.primary_key
        ));
        for (position, column) in block.columns.iter().enumerate() {
            out.push(format!("{stem}|{d}|column|{table}|{position}|{column}"));
        }
        for (position, constraint) in block.constraints.iter().enumerate() {
            out.push(format!(
                "{stem}|{d}|constraint|{table}|{position}|{constraint}"
            ));
        }
        for (position, index) in block.indexes.iter().enumerate() {
            out.push(format!("{stem}|{d}|index|{table}|{position}|{index}"));
        }
        if let Some(partition_by) = &block.partition_by {
            out.push(format!("{stem}|{d}|partition_by|{table}|{partition_by}"));
        }
        if let Some(schema) = &block.schema {
            out.push(format!("{stem}|{d}|schema|{table}|{schema}"));
        }
        if let Some(options) = &block.options {
            out.push(format!("{stem}|{d}|options|{table}|{options}"));
        }
        for line in &block.unclassified {
            out.push(format!("{stem}|{d}|unclassified|{table}|{line}"));
        }
    }
}

/// Drive both sources through the real entry point and reduce them to lines.
fn measure_corpus() -> Vec<String> {
    let mut measured = Vec::new();
    let confined = support::confined_charter();
    for stem in STEMS {
        let ops = read_stem(stem);
        for dialect in DIALECTS {
            corpus_lines(
                &format!("fixture:{stem}"),
                &ops,
                &confined,
                dialect,
                &mut measured,
            );
        }
    }
    let open = support::no_inject(SCHEMA);
    for (name, source) in CARRIERS {
        let ops = parse(source);
        for dialect in DIALECTS {
            corpus_lines(
                &format!("carrier:{name}"),
                &ops,
                &open,
                dialect,
                &mut measured,
            );
        }
    }
    measured
}

/// **The behaviour-preservation gate for the move.**
///
/// The golden was captured from the OLD path - `render_artifacts` driven by
/// `authoring_tables_from_ops` - BEFORE the consumer was switched, and committed with
/// exactly the rows that the walker's defects made wrong edited by hand, each of them
/// recorded in `docs/review-log.md` with the measurement that settles which side is
/// right. So this test compares what the new path emits against what the walker
/// emitted, on 27 real recorded streams and 20 carriers under 3 dialects, and it is
/// not circular: the side that produced the expectation is not the side under test.
///
/// There is deliberately NO re-bless environment variable, matching
/// `tests/op_fixture_goldens.rs` and consumer 1's gate: an easy update affordance is
/// what turns a corpus into a mirror of whatever the code emits today.
#[test]
fn the_recorded_corpus_renders_the_same_artifacts_through_the_fold() {
    let measured = measure_corpus();
    let path = manifest_path(CORPUS_GOLDEN);
    let recorded =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let expected: Vec<&str> = recorded
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let measured_set: BTreeSet<&str> = measured.iter().map(String::as_str).collect();
    let expected_set: BTreeSet<&str> = expected.iter().copied().collect();
    let unexpected: Vec<&&str> = measured_set.difference(&expected_set).collect();
    let missing: Vec<&&str> = expected_set.difference(&measured_set).collect();
    assert!(
        unexpected.is_empty() && missing.is_empty(),
        "the artifacts moved.\n  MISSING (the recorded path emitted these, the fold \
         does not):\n    {}\n  UNEXPECTED (the fold emits these, the recorded path did \
         not):\n    {}",
        missing
            .iter()
            .map(|l| (**l).to_string())
            .collect::<Vec<_>>()
            .join("\n    "),
        unexpected
            .iter()
            .map(|l| (**l).to_string())
            .collect::<Vec<_>>()
            .join("\n    "),
    );
    assert_eq!(
        measured.len(),
        expected.len(),
        "the corpus must not quietly lose or gain rows"
    );
}

/// The corpus is only evidence if it covers the thing that moved. A golden with no
/// composite primary key, no `primaryKey: null`, and no partition rows would pass the
/// test above while measuring nothing about the map this change replaced.
///
/// Every floor below names the shape it protects, and every one of them counts rows
/// in the GOLDEN FILE rather than in the measurement, so a run in which the code
/// stopped emitting a shape fails here as well as above.
#[test]
fn the_corpus_golden_actually_covers_the_map_that_moved() {
    let path = manifest_path(CORPUS_GOLDEN);
    let recorded =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let lines: Vec<&str> = recorded
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let count = |pred: &dyn Fn(&&str) -> bool| lines.iter().filter(|l| pred(l)).count();

    let single = count(&|l| l.contains("|primary_key|") && l.contains("|single:"));
    let composite = count(&|l| l.contains("|primary_key|") && l.contains("primaryKey: ["));
    let none = count(&|l| l.contains("|primary_key|") && l.ends_with("primaryKey: null"));
    let implicit = count(&|l| l.contains("|primary_key|") && l.ends_with("<implicit>"));
    let constraints = count(&|l| l.contains("|constraint|"));
    let indexes = count(&|l| l.contains("|index|"));
    let partition_by = count(&|l| l.contains("|partition_by|"));
    let schema = count(&|l| l.contains("|schema|"));
    let unclassified = count(&|l| l.contains("|unclassified|"));

    // Each floor sits below its MEASURED value, quoted in the message so a reader can
    // see how much slack there is rather than guessing. Measured on the golden this
    // change commits: single 59, composite 9, null 27, constraint 19, index 66,
    // partition_by 13, schema 1.
    assert!(
        single >= 40,
        "the golden must cover single-column primary keys, which render as a COLUMN \
         modifier (measured 59): {single}"
    );
    assert!(
        composite >= 6,
        "the golden must cover COMPOSITE primary keys, which render through a \
         different branch of `render_table` (measured 9): {composite}"
    );
    assert!(
        none >= 15,
        "the golden must cover `primaryKey: null`, the spelling an `alterPrimaryKey \
         drop` produces (measured 27): {none}"
    );
    // Not a floor but its opposite, and it was written as a floor first and MEASURED
    // to zero. `render_table` has exactly three spellings for the key - the column
    // modifier, the table-level clause, and `primaryKey: null` - so a block that
    // states none of them would mean the emitter grew a fourth path this file's
    // reader is blind to. Recorded as the invariant it turned out to be rather than
    // left as a floor that can never be met.
    assert_eq!(
        implicit, 0,
        "every table block states its key in one of the emitter's three spellings; a \
         block stating none means `render_table` grew a path this reader cannot see"
    );
    assert!(
        constraints >= 15,
        "the golden must cover the `constraints` field (measured 19): {constraints}"
    );
    assert!(
        indexes >= 50,
        "the golden must cover the `indexes` field (measured 66): {indexes}"
    );
    assert!(
        partition_by >= 9,
        "the golden must cover the `partition_by` field (measured 13): {partition_by}"
    );
    assert!(
        schema >= 1,
        "the golden must cover the `schema` field: {schema}"
    );
    assert_eq!(
        unclassified, 0,
        "a golden row the reader could not classify means the per-field halves of \
         every assertion in this file are narrower than they read"
    );
}

// ---------------------------------------------------------------------------
// The over-refusal control
// ---------------------------------------------------------------------------

/// Streams whose refusal is the interesting part, plus the ordinary ones, so the
/// control below is exercised on the arms that could plausibly refuse.
///
/// The walker being deleted had exactly ONE fallible call, `flatten_dialectal_ops`.
/// `single_fold::fold` makes the same call and then runs the whole structural catalog
/// replay on top of it, so the deletion should be able to REMOVE a refusal and never
/// add one. These probes put both directions under load: streams a dialect leg
/// refuses, streams the catalog refuses, and streams the named-type registry refuses.
const REFUSAL_PROBES: &[(&str, &str)] = &[
    (
        "dialect_leg_selection",
        r#"[
  {"op":"dialectal","pg":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"pg_only","type":"text"}],"primaryKey":["id"]}],"sqlite":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"sqlite_only","type":"text"}],"primaryKey":["id"]}],"mysql":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"mysql_only","type":"text"}],"primaryKey":["id"]}]}
]"#,
    ),
    (
        "alter_primary_key_without_a_candidate",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"]}}
]"#,
    ),
    (
        "alter_primary_key_wrong_expectation",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["legacy_id"],"columns":["legacy_id"]}}
]"#,
    ),
    (
        "alter_primary_key_on_a_missing_table",
        r#"[
  {"op":"alterPrimaryKey","table":"ghosts","action":{"kind":"drop","expectedColumns":["id"]}}
]"#,
    ),
    (
        "drop_partition_that_is_not_a_partition",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false}],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":true}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false}]},
  {"op":"dropPartition","parent":"par","name":"p1"}
]"#,
    ),
    (
        "attach_a_partition_to_an_unpartitioned_parent",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false}],"primaryKey":["bucket"]},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}}
]"#,
    ),
    (
        "table_level_check_off_postgres",
        r#"[
  {"op":"createTable","name":"issues","columns":[{"name":"id","type":"text","nullable":false},{"name":"status","type":"text"}],"primaryKey":["id"],"constraints":[{"name":"issues_status_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"ne","lhs":{"node":"colRef","name":"status"},"rhs":{"node":"literal","value":"x"}}}}]}
]"#,
    ),
    (
        "duplicate_enum",
        r#"[
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createEnum","name":"tier","values":["free","paid","pro"]}
]"#,
    ),
    (
        "duplicate_domain",
        r#"[
  {"op":"createDomain","name":"positive_number","as":"int"},
  {"op":"createDomain","name":"positive_number","as":"int"}
]"#,
    ),
    (
        "column_names_a_dropped_enum",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"dropEnum","name":"tier"},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}}],"primaryKey":["id"]}
]"#,
    ),
    (
        "table_created_twice",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]}
]"#,
    ),
    (
        "enum_recreated_after_drop",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"dropEnum","name":"tier"},
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}}],"primaryKey":["id"]}
]"#,
    ),
];

/// Every case the control below drives, once, so the two tests that read it cannot
/// drift apart.
fn control_cases() -> Vec<(String, Vec<Op>, &'static str)> {
    let mut cases: Vec<(String, Vec<Op>, &'static str)> = Vec::new();
    for stem in STEMS {
        cases.push((format!("fixture:{stem}"), read_stem(stem), "confined"));
    }
    for (name, source) in CARRIERS {
        cases.push((format!("carrier:{name}"), parse(source), "open"));
    }
    for (name, source) in REFUSAL_PROBES {
        cases.push((format!("probe:{name}"), parse(source), "open"));
    }
    cases
}

/// **The over-refusal control: this move added no refusal, and removed none either.**
///
/// An equality gate is STRUCTURALLY BLIND to a refusal change, because it only
/// compares streams that produced an answer on both sides: a stream that starts
/// erroring simply leaves the sample and the count falls, which reads as green
/// everywhere except in a pinned total. So the property is asserted as a
/// BICONDITIONAL against `fold_ops` - the surviving half of the coherence gate
/// `render_artifacts` applies beside the fold - and both directions panic.
///
/// Be precise about how much this proves FOR THIS MOVE, because it is less than it
/// proved for consumer 1. Consumer 1 introduced the `single_fold::fold` call into
/// `render_artifacts`, so its control was measuring a genuinely new failure surface.
/// That call is already there; this move only changes which value is READ from it, and
/// `project_authoring_tables` is infallible. So the expected refusal delta is zero BY
/// CONSTRUCTION, and this control is a regression guard rather than a discovery
/// instrument. What it does still catch, and what no other gate in this change can:
/// the deletion removing the walker's own `flatten_dialectal_ops` refusal in a stream
/// where the fold does not make the same call, which would show up as UNDER-REFUSAL.
#[test]
fn the_move_changed_no_refusal_that_the_old_path_already_made() {
    let confined = support::confined_charter();
    let open = support::no_inject(SCHEMA);

    let mut refused = 0_usize;
    let mut accepted = 0_usize;
    for (label, ops, charter) in control_cases() {
        let policy: &EffectivePolicy = if charter == "confined" {
            &confined
        } else {
            &open
        };
        for dialect in DIALECTS {
            let resolved = zero_migrate::resolve_create_table_policy(
                &MigrationIr {
                    inverse_ops: None,
                    irreversible: None,
                    ir_version: zero_migrate::CURRENT_IR_VERSION,
                    name: "over_refusal_control".to_string(),
                    owner_app: String::new(),
                    ops: ops.clone(),
                    flags: Default::default(),
                    depends_on: Vec::new(),
                    supersedes: Vec::new(),
                    preconditions: Vec::new(),
                    checksum: None,
                },
                policy,
                SCHEMA,
            );
            let Ok(resolved) = resolved else {
                // The resolve step is BEFORE the fold and this move did not touch it;
                // a stream it rejects reaches neither side.
                continue;
            };
            // The independent oracle is `fold_ops`, NOT the fold. This comparison was
            // written against `fold_to_field_defs`, which ran `fold_ops` itself and was
            // therefore a second opinion; step 4 consumer 3 deleted it, and rewriting
            // this line to `single_fold::fold(…).project_field_defs()` would have made
            // the biconditional compare `render_artifacts` to the very call it makes
            // first - a control that can only ever agree with itself. `fold_ops` is the
            // half of the old gate that still exists independently.
            let old_gate = zero_migrate::fold_ops(&resolved.ops, dialect, SCHEMA, policy);
            let now = render_artifacts(&ops, dialect, SCHEMA, policy);

            match (&old_gate, &now) {
                (Ok(_), Ok(_)) => accepted += 1,
                (Err(old), Err(new)) => {
                    refused += 1;
                    assert!(
                        new.to_string().contains(&old.to_string()),
                        "{label}/{dialect:?}: both refuse, but with DIFFERENT messages. \
                         The coherence gate said:\n  {old}\nand `render_artifacts` \
                         now says:\n  {new}"
                    );
                }
                (Ok(_), Err(new)) => panic!(
                    "{label}/{dialect:?}: OVER-REFUSAL. `fold_ops` accepts this \
                     stream and `render_artifacts` now refuses it:\n  {new}\nThat is a \
                     stream that used to produce artifacts and now produces none."
                ),
                (Err(old), Ok(_)) => panic!(
                    "{label}/{dialect:?}: UNDER-REFUSAL. `fold_ops` refuses this \
                     stream and `render_artifacts` renders it anyway:\n  {old}"
                ),
            }
        }
    }

    // The control is only a control if it exercised BOTH outcomes. All-accept would
    // pass while proving nothing about refusals, and all-refuse would prove nothing
    // about the streams that still render. Floored on BOTH sides for that reason.
    assert_eq!(
        (refused, accepted),
        (CONTROL_REFUSALS, CONTROL_ACCEPTANCES),
        "(refused, accepted). PINNED as well as floored: the floors below stop the \
         control passing vacuously, and the pin stops it drifting - a case that quietly \
         stopped being a refusal case would still clear a floor"
    );
    assert!(
        refused >= 20,
        "the control must actually drive refusals, or it says nothing about \
         over-refusal: {refused}"
    );
    assert!(
        accepted >= 20,
        "the control must also drive acceptances: {accepted}"
    );
}

/// Stream/dialect cases in which BOTH sides refuse.
///
/// 73 of 177 (59 streams x 3 dialects, less the 4 the policy resolution rejects before
/// either side is reached).
const CONTROL_REFUSALS: usize = 73;
/// Stream/dialect cases in which BOTH sides accept. 104 of the same 177.
const CONTROL_ACCEPTANCES: usize = 104;

/// The control above compares two Rust functions. This one checks that the probe
/// streams reach the arms they were written for, by asserting the exact refusals
/// they produce - so a probe that silently stopped being a refusal stream (a typo in
/// the JSON, a schema change) fails here rather than passing the biconditional
/// trivially on both sides being `Ok`.
#[test]
fn the_refusal_probes_still_exercise_the_arms_they_name() {
    let open = support::no_inject(SCHEMA);
    let outcome = |name: &str, dialect: SqlDialect| {
        let (_, source) = REFUSAL_PROBES
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("probe `{name}` exists"));
        render_artifacts(&parse(source), dialect, SCHEMA, &open)
            .map(|_| "rendered".to_string())
            .unwrap_or_else(|e| e.to_string())
    };
    let pg = SqlDialect::Postgres;
    assert!(
        outcome("alter_primary_key_without_a_candidate", pg).contains("UNIQUE candidate"),
        "the no-candidate probe must be refused for that reason: {}",
        outcome("alter_primary_key_without_a_candidate", pg)
    );
    assert!(
        outcome("alter_primary_key_on_a_missing_table", pg).contains("ghosts"),
        "the missing-table probe must name the table: {}",
        outcome("alter_primary_key_on_a_missing_table", pg)
    );
    assert!(
        outcome("drop_partition_that_is_not_a_partition", pg).contains("p1"),
        "the not-a-partition probe must name the relation: {}",
        outcome("drop_partition_that_is_not_a_partition", pg)
    );
    assert!(
        outcome("duplicate_enum", pg).contains("tier"),
        "the duplicate-enum probe must be refused as a duplicate enum: {}",
        outcome("duplicate_enum", pg)
    );
    assert!(
        outcome("table_level_check_off_postgres", SqlDialect::Sqlite).contains("CHECK"),
        "the table-level CHECK probe must be refused off Postgres: {}",
        outcome("table_level_check_off_postgres", SqlDialect::Sqlite)
    );
    // And the controls that stop the four above from passing for the wrong reason.
    assert_eq!(
        outcome("enum_recreated_after_drop", pg),
        "rendered",
        "a drop-and-recreate is NOT a duplicate, and a control that refused it would \
         make the duplicate probe pass for the wrong reason"
    );
    assert_eq!(
        outcome("table_level_check_off_postgres", pg),
        "rendered",
        "a table-level CHECK is fine ON Postgres, which is what makes the SQLite \
         refusal above dialect-specific rather than universal"
    );
    assert!(
        outcome("alter_primary_key_wrong_expectation", pg)
            .contains("replace columns must differ from expectedColumns"),
        "the wrong-expectation probe must be refused by the catalog replay's own \
         lifecycle rule, which is a refusal path `render_artifacts` has had since \
         consumer 1 put the fold in front of it: {}",
        outcome("alter_primary_key_wrong_expectation", pg)
    );
}

/// One more control on the golden, in the direction the over-refusal test cannot
/// reach: the corpus must contain BOTH refused and rendered rows, or the golden is
/// pinning only half of what `render_artifacts` does.
#[test]
fn the_corpus_golden_records_both_refusals_and_renders() {
    let path = manifest_path(CORPUS_GOLDEN);
    let recorded =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let refused = recorded.lines().filter(|l| l.contains("|refused|")).count();
    let rendered = recorded
        .lines()
        .filter(|l| l.contains("|sha|env.db.ts|"))
        .count();
    assert!(
        refused >= 5,
        "the golden must record refusals, or it says nothing about the fail-closed \
         half of `render_artifacts`: {refused}"
    );
    assert!(rendered >= 80, "the golden must record renders: {rendered}");
}

/// The two artifacts out of ONE `render_artifacts` call must describe the same
/// database. Section B row 2 of the proposal is a case where they did not, and this
/// move is the last chance to check it for the primary key specifically: the runtime
/// descriptor's `fields` come from the `FieldDef` projection, which DOES handle
/// `alterPrimaryKey`, and `env.db.ts` came from a walker that did not.
#[test]
fn both_artifacts_agree_about_the_key_the_op_installed() {
    for dialect in DIALECTS {
        let rendered = render_artifacts(
            &parse(PK_REPLACE_DROPS_IDENTITY),
            dialect,
            SCHEMA,
            &support::no_inject(SCHEMA),
        )
        .expect("the stream renders");
        let runtime: Value =
            serde_json::from_str(&rendered.runtime_json).expect("`schema.runtime.json` parses");
        // the `FieldDef` projection clears the identity facet on `dropIdentityFrom`; this is
        // the half that was ALREADY right, asserted so the fix cannot be "make them
        // agree by breaking the other one".
        let id = runtime
            .pointer("/collections/orders/fields/id")
            .unwrap_or_else(|| panic!("{dialect:?}: the descriptor carries `orders.id`"));
        assert_eq!(
            id.get("autoIncrement"),
            None,
            "{dialect:?}: `schema.runtime.json` already dropped the identity: {runtime:#}"
        );
        let block = block(&rendered.env_db_ts, "orders");
        assert!(
            block
                .columns
                .iter()
                .any(|c| c.starts_with("id: ") && !c.contains(".autoIncrement()")),
            "{dialect:?}: and now `env.db.ts` agrees, out of the same call:\n{}",
            rendered.env_db_ts
        );
    }
}
