//! **Step 4, consumer 1: the runtime collection metadata comes from the single fold.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 4 moves the artifact
//! consumers off their private walkers one at a time, and
//! `runtime_metadata_from_ops` is the first because it has the smallest blast radius.
//! This file is the gate on that move.
//!
//! # What the move can actually change, measured rather than assumed
//!
//! The metadata map reaches exactly three places in the two artifacts:
//!
//! * `schema.runtime.json` -> `collections[<t>].options` (the three runtime flags),
//! * `schema.runtime.json` -> `collections[<t>].indexes` (the plain index list),
//! * `env.db.ts` -> the `runtimeOptions` block (`render_table` passes the metadata
//!   entry to `render_runtime_options` and to nothing else - the `indexes:` block in
//!   `env.db.ts` is rendered from the AUTHORING table, not from here).
//!
//! Everything else in both artifacts comes from the other two walkers, which this
//! move does not touch. So the assertions below probe those three places per FIELD,
//! and [`the_recorded_corpus_renders_the_same_artifacts_through_the_fold`] pins the
//! whole of both artifacts by content hash so a change outside those three places
//! cannot pass unnoticed either.
//!
//! # Why these streams
//!
//! The step 3 gate (`render/gen_types/fold_projection_equality.rs`) proved
//! `project_runtime_metadata` equal to `runtime_metadata_from_ops` and found no
//! difference. It was nonetheless BLIND to the three divergences below, because no
//! stream in the step 1 corpus crosses a `unique` column with a rename or with a
//! `dropIndex`. Passing that gate was necessary and not sufficient, and these are the
//! streams that show why.
//!
//! Be precise about how much that gate established, because "2,720 byte-identical
//! comparisons" is the number that gets quoted and it is the wrong one: 2,720 was the
//! total across ALL FOUR projections. The runtime-metadata leg contributed **683** of
//! it - measured, as the fall in `EQUAL_COMPARISONS` when that leg retired and
//! nothing else about the corpus changed. Quoting the four-projection total for a
//! one-projection claim overstates the prior evidence fourfold, and an inflated
//! evidence claim is the same failure as a gate that passes for the wrong reason,
//! only in prose.
//!
//! # The rule the three rename arms encode
//!
//! An implicit unique index is named `<table>_<column>_key` ONCE, when `createTable`
//! creates it. PostgreSQL and SQLite store an index name independently of the table
//! and column it covers, so neither `ALTER TABLE ... RENAME TO` nor
//! `ALTER TABLE ... RENAME COLUMN` renames it. `render/fold.rs`'s `Op::RenameTable`
//! arm states the same rule for the catalog half and cites the live-server test that
//! measured it (`fold_roundtrip_pg.rs`: "`ALTER TABLE tags RENAME TO labels`
//! genuinely leaves `tags_pkey` named `tags_pkey`"). A projection that re-derived the
//! name from the CURRENT table and column would put an index name in
//! `schema.runtime.json` that no catalog anywhere has.
//!
//! # The one deliberate behaviour CHANGE
//!
//! [`dropping_an_included_column_drops_the_index_from_the_runtime_descriptor`] is the
//! single row where the old walker and the new projection disagree AND the old walker
//! is the one that is wrong. `runtime_metadata_from_ops` retained an index whose
//! `INCLUDE` payload named the dropped column, because it matched on its own `fields`
//! list and `INCLUDE` columns were never in it. `render/fold.rs` records the
//! measurement that settles it - on PG 18.4,
//! `CREATE INDEX i ON t (b) INCLUDE (a); ALTER TABLE t DROP COLUMN a` leaves no `i` in
//! `pg_indexes` - so the artifact named an index the database does not have. The fold
//! cascades it away, `authoring_tables_from_ops` already cascaded it away for
//! `env.db.ts`, and this arm pins the corrected answer.
//!
//! Offline throughout: the oracle is the emitted artifact, so there is no skip here
//! that could read as a pass.

mod support;

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

fn artifacts(ops: &[Op], dialect: SqlDialect, policy: &EffectivePolicy) -> (Value, String) {
    let rendered =
        render_artifacts(ops, dialect, SCHEMA, policy).expect("the stream renders artifacts");
    let runtime =
        serde_json::from_str(&rendered.runtime_json).expect("`schema.runtime.json` parses");
    (runtime, rendered.env_db_ts)
}

/// The `indexes` array one collection carries in `schema.runtime.json`.
fn indexes(runtime: &Value, collection: &str) -> Vec<Value> {
    runtime
        .pointer(&format!("/collections/{collection}/indexes"))
        .unwrap_or_else(|| {
            panic!("the descriptor should carry `{collection}`: {runtime:#}");
        })
        .as_array()
        .expect("`indexes` is an array")
        .clone()
}

/// One index entry, located by NAME rather than by position, so a test that is
/// looking for `users_email_key` fails with "absent" rather than silently reading
/// whatever sits at index 0.
fn index_named<'a>(entries: &'a [Value], name: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
}

/// Assert the three fields of one runtime index descriptor SEPARATELY.
///
/// Per-field on purpose. A whole-object `assert_eq!` on two distinct descriptors is
/// decided by whichever field differs first - in practice always `name` - so the
/// `fields` and `unique` terms would never be reached and a projection that dropped
/// one of them would still pass.
#[track_caller]
fn assert_index_fields(
    entries: &[Value],
    name: &str,
    expected_fields: &[&str],
    expected_unique: bool,
) {
    let entry = index_named(entries, name).unwrap_or_else(|| {
        panic!(
            "the runtime descriptor must carry an index named `{name}`; it carries {:?}",
            entries
                .iter()
                .map(|e| e.get("name").cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>()
        )
    });
    // FIELD 1: `fields`.
    let fields: Vec<&str> = entry
        .get("fields")
        .and_then(Value::as_array)
        .expect("an index descriptor carries `fields`")
        .iter()
        .map(|f| f.as_str().expect("a field name is a string"))
        .collect();
    assert_eq!(
        fields, expected_fields,
        "index `{name}`: FIELD `fields` -- the columns the index covers"
    );
    // FIELD 2: `unique`. Absent means false (`skip_serializing_if`), so the absence
    // is read explicitly rather than left to a missing-key comparison.
    let unique = entry
        .get("unique")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert_eq!(
        unique, expected_unique,
        "index `{name}`: FIELD `unique` -- whether the index enforces uniqueness"
    );
    // FIELD 3: `name` is what located the entry, asserted here so the probe states
    // all three rather than leaving one implicit in the lookup.
    assert_eq!(
        entry.get("name").and_then(Value::as_str),
        Some(name),
        "index `{name}`: FIELD `name` -- the catalog name"
    );
}

/// Assert the three runtime OPTION fields separately, for the same reason.
#[track_caller]
fn assert_options(
    runtime: &Value,
    collection: &str,
    soft_delete: bool,
    versioning: bool,
    strictness: &str,
) {
    let options = runtime
        .pointer(&format!("/collections/{collection}/options"))
        .unwrap_or_else(|| panic!("the descriptor should carry `{collection}`: {runtime:#}"));
    assert_eq!(
        options.get("softDelete").and_then(Value::as_bool),
        Some(soft_delete),
        "`{collection}`: FIELD `options.softDelete`"
    );
    assert_eq!(
        options.get("versioning").and_then(Value::as_bool),
        Some(versioning),
        "`{collection}`: FIELD `options.versioning`"
    );
    assert_eq!(
        options.get("strictness").and_then(Value::as_str),
        Some(strictness),
        "`{collection}`: FIELD `options.strictness`"
    );
}

// ---------------------------------------------------------------------------
// The rename carriers the step 3 corpus never crossed with a `unique` column
// ---------------------------------------------------------------------------

/// `createTable users(email unique)` then `renameTable users -> members`.
///
/// PostgreSQL does not rename an index when it renames a table, so the index the
/// server has is still `users_email_key`. A projection that re-derived the name from
/// the CURRENT table name answers `members_email_key`, which is an object no catalog
/// has, and the artifact would be describing a database that does not exist.
#[test]
fn a_table_rename_does_not_rename_the_implicit_unique_index() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text","nullable":false,"unique":true}],
   "primaryKey":["id"]},
  {"op":"renameTable","table":"users","to":"members"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, _) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        let entries = indexes(&runtime, "members");
        assert_index_fields(&entries, "users_email_key", &["email"], true);
        assert!(
            index_named(&entries, "members_email_key").is_none(),
            "{dialect:?}: `members_email_key` is a name the rename never created; \
             re-deriving it from the new table name invents an index the catalog \
             does not have: {entries:#?}"
        );
    }
}

/// `createTable users(email unique)` then `renameColumn email -> mail`.
///
/// The FIELD follows the rename and the NAME does not. Both halves are asserted, so
/// a projection that froze the whole descriptor instead of just its name fails here
/// too.
#[test]
fn a_column_rename_moves_the_index_field_and_not_the_index_name() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text","nullable":false,"unique":true}],
   "primaryKey":["id"]},
  {"op":"renameColumn","table":"users","from":"email","to":"mail","type":"text"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, _) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        let entries = indexes(&runtime, "users");
        assert_index_fields(&entries, "users_email_key", &["mail"], true);
        assert!(
            index_named(&entries, "users_mail_key").is_none(),
            "{dialect:?}: PostgreSQL does not rename an index when it renames a \
             column, so `users_mail_key` names nothing: {entries:#?}"
        );
    }
}

/// A `dropIndex` naming the implicit unique index removes it, and leaves the column.
///
/// Asserted with and without the optional `table` qualifier, because the walker had
/// separate paths for the two and a projection that only handled the qualified one
/// would keep a dropped index in the artifact for the unqualified spelling.
#[test]
fn dropping_the_implicit_unique_index_by_name_removes_it_from_the_descriptor() {
    for qualifier in ["\"table\":\"users\",", ""] {
        let ops = parse(&format!(
            r#"[
  {{"op":"createTable","name":"users","columns":[
    {{"name":"id","type":"text","nullable":false}},
    {{"name":"email","type":"text","nullable":false,"unique":true}}],
   "primaryKey":["id"]}},
  {{"op":"dropIndex",{qualifier}"name":"users_email_key"}}
]"#
        ));
        for dialect in DIALECTS {
            let (runtime, _) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
            let entries = indexes(&runtime, "users");
            assert!(
                index_named(&entries, "users_email_key").is_none(),
                "{dialect:?} (qualifier {qualifier:?}): the index was dropped, so the \
                 artifact must not still name it: {entries:#?}"
            );
            // The COLUMN survives its index. This is the control that stops the arm
            // above from passing because the whole collection vanished.
            assert!(
                runtime.pointer("/collections/users/fields/email").is_some(),
                "{dialect:?}: dropping an index does not drop its column: {runtime:#}"
            );
        }
    }
}

/// The rename rule has to survive the two renames COMPOSED, and the drop after them
/// still has to match the frozen name.
#[test]
fn a_frozen_index_name_survives_both_renames_and_is_still_droppable() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text","nullable":false,"unique":true}],
   "primaryKey":["id"]},
  {"op":"renameTable","table":"users","to":"members"},
  {"op":"renameColumn","table":"members","from":"email","to":"mail","type":"text"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, _) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        let entries = indexes(&runtime, "members");
        assert_index_fields(&entries, "users_email_key", &["mail"], true);
    }

    let mut dropped = ops;
    dropped.extend(parse(
        r#"[{"op":"dropIndex","table":"members","name":"users_email_key"}]"#,
    ));
    for dialect in DIALECTS {
        let (runtime, _) = artifacts(&dropped, dialect, &support::no_inject(SCHEMA));
        assert!(
            indexes(&runtime, "members").is_empty(),
            "{dialect:?}: the frozen name is the name that drops it"
        );
    }
}

/// Dropping the COLUMN takes its implicit index with it, under the post-rename name.
#[test]
fn dropping_a_renamed_unique_column_takes_its_implicit_index_with_it() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text","nullable":false,"unique":true}],
   "primaryKey":["id"]},
  {"op":"renameColumn","table":"users","from":"email","to":"mail","type":"text"},
  {"op":"dropColumn","table":"users","column":"mail"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, _) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        assert!(
            indexes(&runtime, "users").is_empty(),
            "{dialect:?}: the column is gone, so its implicit index is gone with it"
        );
    }
}

/// A named plain index and an implicit unique index coexist, in declaration order,
/// and the plain one keeps the name it was authored with.
#[test]
fn a_plain_index_and_an_implicit_unique_index_are_both_described() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text","nullable":false,"unique":true},
    {"name":"team","type":"text"}],
   "primaryKey":["id"],
   "indexes":[{"name":"users_team_idx","columns":[{"kind":"column","name":"team"}]}]}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, _) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        let entries = indexes(&runtime, "users");
        assert_index_fields(&entries, "users_email_key", &["email"], true);
        assert_index_fields(&entries, "users_team_idx", &["team"], false);
        // ORDER is part of the artifact's bytes: the implicit unique indexes come
        // first, in column declaration order, then the authored indexes.
        assert_eq!(
            entries
                .iter()
                .map(|e| e.get("name").and_then(Value::as_str).unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["users_email_key", "users_team_idx"],
            "{dialect:?}: implicit unique indexes precede authored ones"
        );
    }
}

// ---------------------------------------------------------------------------
// The one row where the old walker was the wrong one
// ---------------------------------------------------------------------------

/// **A deliberate behaviour change.** An index whose `INCLUDE` payload names a
/// dropped column is dropped by the server, so it must leave the artifact.
///
/// `runtime_metadata_from_ops` kept it: its `DropColumn` arm matched on the runtime
/// descriptor's own `fields` list, which never contained an `INCLUDE` column.
/// `render/fold.rs` cites the measurement that settles which side is right - on PG
/// 18.4, `CREATE INDEX i ON t (b) INCLUDE (a); ALTER TABLE t DROP COLUMN a` leaves no
/// `i` in `pg_indexes`. The artifact was naming an index the database does not have.
///
/// The `env.db.ts` half of the same artifact already agreed with the server, because
/// it renders indexes from `authoring_tables_from_ops`, whose `DropColumn` arm
/// cascades on `include`. So before this move the TWO artifacts out of one
/// `render_artifacts` call disagreed about the same index - which is exactly the
/// class of defect section B of the proposal is a list of.
#[test]
fn dropping_an_included_column_drops_the_index_from_the_runtime_descriptor() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"users","columns":[
    {"name":"id","type":"text","nullable":false},
    {"name":"email","type":"text"},
    {"name":"extra","type":"text"}],
   "primaryKey":["id"],
   "indexes":[{"name":"users_email_idx","columns":[{"kind":"column","name":"email"}],
               "include":["extra"]}]},
  {"op":"dropColumn","table":"users","column":"extra"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, env_db_ts) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        assert!(
            index_named(&indexes(&runtime, "users"), "users_email_idx").is_none(),
            "{dialect:?}: PG drops an index whose INCLUDE payload names the dropped \
             column, so `schema.runtime.json` must not still name it: {runtime:#}"
        );
        // The two artifacts now agree, which is the point of the row. This half was
        // already correct and is asserted so the fix cannot be "make them agree by
        // breaking the other one".
        assert!(
            !env_db_ts.contains("users_email_idx"),
            "{dialect:?}: `env.db.ts` already cascaded the index away; both artifacts \
             out of one call must describe the same database:\n{env_db_ts}"
        );
    }
}

// ---------------------------------------------------------------------------
// Runtime OPTIONS, the other half of the map
// ---------------------------------------------------------------------------

/// `createTable`'s `runtimeOptions` and a later `setTableOptions` both reach both
/// artifacts, per field, and a table rename carries them to the new name.
#[test]
fn runtime_options_reach_both_artifacts_per_field_and_survive_a_rename() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"boxes","columns":[{"name":"id","type":"text","nullable":false}],
   "primaryKey":["id"],"runtimeOptions":{"softDelete":true,"versioning":false}},
  {"op":"setTableOptions","table":"boxes","options":{"versioning":true,"strictness":"lenient"}},
  {"op":"renameTable","table":"boxes","to":"crates"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, env_db_ts) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        assert_options(&runtime, "crates", true, true, "lenient");
        // The `env.db.ts` half reads the SAME map through `render_runtime_options`,
        // and it is the only thing the map reaches in that artifact.
        assert!(
            env_db_ts.contains("softDelete: true"),
            "{dialect:?}: `env.db.ts` carries `softDelete`:\n{env_db_ts}"
        );
        assert!(
            env_db_ts.contains("versioning: true"),
            "{dialect:?}: `env.db.ts` carries `versioning`:\n{env_db_ts}"
        );
        assert!(
            env_db_ts.contains("strictness: \"lenient\""),
            "{dialect:?}: `env.db.ts` carries `strictness`:\n{env_db_ts}"
        );
    }
}

/// A dropped table takes its options with it: an entry that outlived its table would
/// give the descriptor a collection the database does not have.
#[test]
fn a_dropped_table_leaves_no_runtime_metadata_behind() {
    let ops = parse(
        r#"[
  {"op":"createTable","name":"boxes","columns":[{"name":"id","type":"text","nullable":false}],
   "primaryKey":["id"],"runtimeOptions":{"softDelete":true,"versioning":false}},
  {"op":"dropTable","table":"boxes"}
]"#,
    );
    for dialect in DIALECTS {
        let (runtime, env_db_ts) = artifacts(&ops, dialect, &support::no_inject(SCHEMA));
        assert!(
            runtime.pointer("/collections/boxes").is_none(),
            "{dialect:?}: the table is dropped: {runtime:#}"
        );
        assert!(
            !env_db_ts.contains("boxes"),
            "{dialect:?}: the table is dropped:\n{env_db_ts}"
        );
    }
}

// ---------------------------------------------------------------------------
// The corpus: both artifacts, whole, on real recorded streams
// ---------------------------------------------------------------------------

/// The recorded op fixtures, the same 27 `tests/op_fixture_goldens.rs` owns. These
/// are real drained recorder envelopes, already policy-resolved, so they fold under
/// the confined charter that produced them.
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
/// They are here because the recorded corpus turned out not to cover the thing that
/// moved: measured on the golden this file writes, the 27 fixtures produce 63 index
/// rows and NOT ONE of them is an implicit unique index, which is the single carrier
/// whose name the fold had to freeze. A behaviour-preservation gate that cannot see
/// the carrier under change proves nothing about it, and
/// [`the_corpus_golden_actually_covers_the_map_that_moved`] is the assertion that
/// keeps that true.
///
/// These fold under `no_inject` rather than the confined charter, because they are
/// written to state one rule each rather than to exercise the policy.
const CARRIERS: &[(&str, &str)] = &[
    (
        "unique_column",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true},{"name":"team","type":"text"}],"primaryKey":["id"],"indexes":[{"name":"users_team_idx","columns":[{"kind":"column","name":"team"}]}]}
]"#,
    ),
    (
        "unique_column_table_renamed",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"renameTable","table":"users","to":"members"}
]"#,
    ),
    (
        "unique_column_renamed",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"renameColumn","table":"users","from":"email","to":"mail","type":"text"}
]"#,
    ),
    (
        "unique_index_dropped_by_name",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"dropIndex","table":"users","name":"users_email_key"}
]"#,
    ),
    (
        "unique_index_dropped_unqualified",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"dropIndex","name":"users_email_key"}
]"#,
    ),
    (
        "unique_column_dropped",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"dropColumn","table":"users","column":"email"}
]"#,
    ),
    (
        "runtime_options_then_rename",
        r#"[
  {"op":"createTable","name":"boxes","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"],"runtimeOptions":{"softDelete":true,"versioning":false}},
  {"op":"setTableOptions","table":"boxes","options":{"versioning":true,"strictness":"lenient"}},
  {"op":"renameTable","table":"boxes","to":"crates"}
]"#,
    ),
    (
        "index_include_column_dropped",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text"},{"name":"extra","type":"text"}],"primaryKey":["id"],"indexes":[{"name":"users_email_idx","columns":[{"kind":"column","name":"email"}],"include":["extra"]}]},
  {"op":"dropColumn","table":"users","column":"extra"}
]"#,
    ),
    (
        "plain_index_created_then_column_renamed",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text"}],"primaryKey":["id"]},
  {"op":"createIndex","table":"users","columns":[{"kind":"column","name":"email"}]},
  {"op":"renameColumn","table":"users","from":"email","to":"mail","type":"text"}
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

const CORPUS_GOLDEN: &str = "tests/goldens/runtime_metadata_artifacts.txt";

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
/// Two kinds of line, and both are needed:
///
/// * a `sha` line per artifact - the WHOLE artifact, so a byte moving anywhere in
///   either file is caught even though this move should not be able to move one;
/// * one line per FIELD the metadata map owns - so when a `sha` line moves, the
///   lines beside it say which field moved and the failure names it instead of
///   printing two hashes.
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
        "{stem}|{d}|sha|runtime.json|{}",
        sha256_hex(rendered.runtime_json.as_bytes())
    ));
    out.push(format!(
        "{stem}|{d}|sha|env.db.ts|{}",
        sha256_hex(rendered.env_db_ts.as_bytes())
    ));
    let runtime: Value =
        serde_json::from_str(&rendered.runtime_json).expect("`schema.runtime.json` parses");
    let collections = runtime
        .get("collections")
        .and_then(Value::as_object)
        .expect("the v1 descriptor carries `collections`");
    for (name, collection) in collections {
        let options = collection.get("options").expect("a collection has options");
        out.push(format!(
            "{stem}|{d}|options|{name}|softDelete={}|versioning={}|strictness={}",
            options
                .get("softDelete")
                .and_then(Value::as_bool)
                .expect("softDelete"),
            options
                .get("versioning")
                .and_then(Value::as_bool)
                .expect("versioning"),
            options
                .get("strictness")
                .and_then(Value::as_str)
                .expect("strictness"),
        ));
        for (position, index) in collection
            .get("indexes")
            .and_then(Value::as_array)
            .expect("a collection has indexes")
            .iter()
            .enumerate()
        {
            out.push(format!(
                "{stem}|{d}|index|{name}|{position}|name={}|fields={}|unique={}",
                index.get("name").and_then(Value::as_str).expect("name"),
                index
                    .get("fields")
                    .and_then(Value::as_array)
                    .expect("fields")
                    .iter()
                    .map(|f| f.as_str().expect("a field name is a string"))
                    .collect::<Vec<_>>()
                    .join(","),
                index
                    .get("unique")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ));
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
/// `runtime_metadata_from_ops` - BEFORE the consumer was switched, and committed
/// unchanged. So this test compares what the new path emits against what the walker
/// emitted, on 27 real recorded streams under 3 dialects, and it is not circular: the
/// side that produced the expectation is not the side under test.
///
/// The one row that had to be edited by hand when the walker was deleted is recorded
/// in `docs/review-log.md` with the measurement that justified it. There is
/// deliberately NO re-bless environment variable, matching
/// `tests/op_fixture_goldens.rs`: an easy update affordance is what turns a corpus
/// into a mirror of whatever the code emits today.
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

    // Named, per-line, so a failure says WHICH stream / dialect / field moved rather
    // than printing two multi-thousand-line blobs.
    let measured_set: BTreeSet<&str> = measured.iter().map(String::as_str).collect();
    let expected_set: BTreeSet<&str> = expected.iter().copied().collect();
    let unexpected: Vec<&&str> = measured_set.difference(&expected_set).collect();
    let missing: Vec<&&str> = expected_set.difference(&measured_set).collect();
    assert!(
        unexpected.is_empty() && missing.is_empty(),
        "the artifacts moved.\n  MISSING (the old walker emitted these, the fold does \
         not):\n    {}\n  UNEXPECTED (the fold emits these, the old walker did \
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
/// `index` lines and no non-default `options` lines would pass the test above while
/// measuring nothing about the map this change replaced.
#[test]
fn the_corpus_golden_actually_covers_the_map_that_moved() {
    let path = manifest_path(CORPUS_GOLDEN);
    let recorded =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let index_lines = recorded
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| l.contains("|index|"))
        .count();
    let unique_index_lines = recorded
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| l.contains("|index|") && l.ends_with("unique=true"))
        .count();
    let non_default_option_lines = recorded
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| {
            l.contains("|options|")
                && (l.contains("softDelete=true")
                    || l.contains("versioning=true")
                    || !l.ends_with("strictness=strict"))
        })
        .count();
    assert!(
        index_lines >= 50,
        "the golden must describe real indexes, not an empty map: {index_lines}"
    );
    assert!(
        unique_index_lines >= 1,
        "the golden must cover at least one IMPLICIT unique index - the one carrier \
         whose name the fold had to freeze: {unique_index_lines}"
    );
    assert!(
        non_default_option_lines >= 1,
        "the golden must cover at least one non-default runtime option, or the \
         options half of the map is untested: {non_default_option_lines}"
    );
}

// ---------------------------------------------------------------------------
// The over-refusal control
// ---------------------------------------------------------------------------

/// Streams whose NAMED-TYPE resolution or table-shape resolution is the interesting
/// part, plus the ordinary ones, so the control below is exercised on the arms that
/// could plausibly refuse.
///
/// `AuthoredState::advance` has exactly three fallible sites - `ResolvedInject::
/// for_table` in the `createTable` arm, and `NamedTypeRegistry::create_enum` /
/// `create_domain` in the two named-type arms - and none of them existed on the path
/// `render_artifacts` took before this move. These streams put each of them under
/// load.
const REFUSAL_PROBES: &[(&str, &str)] = &[
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
        "enum_recreated_after_drop",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"dropEnum","name":"tier"},
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}}],"primaryKey":["id"]}
]"#,
    ),
    (
        "domain_recreated_after_drop",
        r#"[
  {"op":"createDomain","name":"positive_number","as":"int"},
  {"op":"dropDomain","name":"positive_number"},
  {"op":"createDomain","name":"positive_number","as":"int"}
]"#,
    ),
    (
        "enum_and_domain_share_a_name",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"createDomain","name":"tier","as":"int"}
]"#,
    ),
    (
        "column_names_an_undefined_enum",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"missing_tier"}}}],"primaryKey":["id"]}
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
        "duplicate_enum_inside_a_dialect_leg",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"dialectal","pg":[{"op":"createEnum","name":"tier","values":["free","paid"]}],"sqlite":[],"mysql":[]}
]"#,
    ),
    (
        "duplicate_enum_only_in_an_inactive_leg",
        r#"[
  {"op":"dialectal","pg":[{"op":"createEnum","name":"tier","values":["free"]}],"sqlite":[{"op":"createEnum","name":"other","values":["a"]}],"mysql":[{"op":"createEnum","name":"other","values":["a"]}]},
  {"op":"createEnum","name":"tier","values":["free","paid"]}
]"#,
    ),
    (
        "table_in_a_named_schema",
        r#"[
  {"op":"createSchema","name":"reporting","ifNotExists":true},
  {"op":"createTable","name":"hits","schema":"reporting","columns":[{"name":"id","type":"text","nullable":false},{"name":"path","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]}
]"#,
    ),
    (
        "table_created_twice",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]}
]"#,
    ),
];

/// **The over-refusal control: this move added no refusal.**
///
/// Before step 4, the ONLY thing that could make `render_artifacts` refuse a stream
/// (after the policy resolution it still performs first) was
/// `fold_to_field_defs`: `runtime_metadata_from_ops` and `authoring_tables_from_ops`
/// applied no coherence gate at all, and the one fallible call they shared -
/// `flatten_dialectal_ops` - `fold_to_field_defs` makes too. After the move,
/// `single_fold::fold` runs FIRST and brings `AuthoredState::advance` with it, whose
/// three fallible sites have no counterpart in that old path.
///
/// So the property is a BICONDITIONAL, and it is asserted rather than argued:
/// `render_artifacts` accepts a stream exactly when `fold_to_field_defs` accepts it,
/// and when both refuse they refuse with the SAME message. A refusal `advance` adds
/// makes the left side `Err` while the right side is `Ok`, and this test goes red.
///
/// This closes a hole the equality gate CANNOT close by construction: that gate only
/// compares streams the fold accepted, so a stream the fold newly refuses simply
/// leaves its sample, and the count falls rather than a comparison failing. A check
/// whose failure direction is toward green is not a check.
#[test]
fn the_move_added_no_refusal_that_the_old_path_did_not_already_make() {
    let confined = support::confined_charter();
    let open = support::no_inject(SCHEMA);

    let mut cases: Vec<(String, Vec<Op>, &EffectivePolicy)> = Vec::new();
    for stem in STEMS {
        cases.push((format!("fixture:{stem}"), read_stem(stem), &confined));
    }
    for (name, source) in CARRIERS {
        cases.push((format!("carrier:{name}"), parse(source), &open));
    }
    for (name, source) in REFUSAL_PROBES {
        cases.push((format!("probe:{name}"), parse(source), &open));
    }

    let mut refused = 0_usize;
    let mut accepted = 0_usize;
    for (label, ops, policy) in &cases {
        for dialect in DIALECTS {
            // The RIGHT side is the whole of the old path's coherence gate, driven on
            // the same policy-resolved ops `render_artifacts` folds.
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
            let old_gate = zero_migrate::fold_to_field_defs(&resolved.ops, dialect, SCHEMA, policy);
            let now = render_artifacts(ops, dialect, SCHEMA, policy);

            match (&old_gate, &now) {
                (Ok(_), Ok(_)) => accepted += 1,
                (Err(old), Err(new)) => {
                    refused += 1;
                    assert!(
                        new.to_string().contains(&old.to_string()),
                        "{label}/{dialect:?}: both refuse, but with DIFFERENT messages. \
                         The old path's coherence gate said:\n  {old}\nand \
                         `render_artifacts` now says:\n  {new}"
                    );
                }
                (Ok(_), Err(new)) => panic!(
                    "{label}/{dialect:?}: OVER-REFUSAL. `fold_to_field_defs` - the whole \
                     of the coherence gate `render_artifacts` had before step 4 - accepts \
                     this stream, and `render_artifacts` now refuses it:\n  {new}\nThat is \
                     a stream that used to produce artifacts and now produces none."
                ),
                (Err(old), Ok(_)) => panic!(
                    "{label}/{dialect:?}: UNDER-REFUSAL. `fold_to_field_defs` refuses this \
                     stream and `render_artifacts` renders it anyway:\n  {old}"
                ),
            }
        }
    }

    // The control is only a control if it exercised BOTH outcomes. All-accept would
    // pass while proving nothing about refusals, and all-refuse would prove nothing
    // about the streams that still render.
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

/// The control above compares two Rust functions. This one checks that the probe
/// streams reach the arms they were written for, by asserting the exact refusals the
/// named-type arms produce - so a probe that silently stopped being a duplicate-enum
/// stream (a typo in the JSON, a schema change) fails here rather than passing the
/// biconditional trivially.
#[test]
fn the_refusal_probes_still_exercise_the_named_type_arms() {
    let open = support::no_inject(SCHEMA);
    let outcome = |name: &str| {
        let (_, source) = REFUSAL_PROBES
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("probe `{name}` exists"));
        render_artifacts(&parse(source), SqlDialect::Postgres, SCHEMA, &open)
            .map(|_| "rendered".to_string())
            .unwrap_or_else(|e| e.to_string())
    };
    assert!(
        outcome("duplicate_enum").contains("enum") && outcome("duplicate_enum").contains("tier"),
        "the duplicate-enum probe must be refused as a duplicate enum: {}",
        outcome("duplicate_enum")
    );
    assert!(
        outcome("duplicate_domain").contains("domain")
            && outcome("duplicate_domain").contains("positive_number"),
        "the duplicate-domain probe must be refused as a duplicate domain: {}",
        outcome("duplicate_domain")
    );
    assert_eq!(
        outcome("enum_recreated_after_drop"),
        "rendered",
        "a drop-and-recreate is NOT a duplicate, and a control that refused it would \
         make the two above pass for the wrong reason"
    );
}
