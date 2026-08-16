//! Two indexes sharing a name in one `createTable` are refused at the gate.
//!
//! This is a FAIL-OPEN, not a late verdict, which makes it worse than the
//! self-rename and duplicate-column findings that preceded it. Those reached the
//! server and were rejected loudly. This one SUCCEEDS.
//!
//! Measured. A `createTable` declaring two indexes both named `ix`, one on `(c)`
//! and one on `(d)`, lowers to
//!
//!     CREATE INDEX IF NOT EXISTS "ix" ON "prj_ir"."a" ("c")
//!     CREATE INDEX IF NOT EXISTS "ix" ON "prj_ir"."a" ("d")
//!
//! and PostgreSQL answers the second with
//!
//!     NOTICE:  relation "ix" already exists, skipping
//!
//! which is a NOTICE, not an error. The apply succeeds. The final schema carries
//! ONE index, on `(c)`. The index the author declared on `(d)` does not exist,
//! nothing failed, and nothing anywhere says so.
//!
//! `IF NOT EXISTS` is right for idempotent re-application — it is what makes a
//! re-run a no-op instead of an error — and it is exactly what turns this
//! authoring mistake into silence.
//!
//! It is decidable from the operation alone: two entries of one `indexes` list
//! share a name. One pass over one list, the same shape as the duplicate-column
//! check on UNIQUE constraints.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(op: &str, dialect: Dialect) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, dialect, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const TWO_INDEXES_ONE_NAME: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"ix","columns":[{"kind":"column","name":"c"}]},{"name":"ix","columns":[{"kind":"column","name":"d"}]}]}"#;

#[test]
fn two_indexes_sharing_a_name_are_refused_on_every_dialect() {
    for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
        let refusal = verdict(TWO_INDEXES_ONE_NAME, dialect).expect_err(&format!(
            "{dialect:?}: both indexes lower to CREATE INDEX IF NOT EXISTS under the \
             same name, so the second is SKIPPED with a notice and the apply \
             succeeds. The author declared an index that does not exist afterwards \
             and nothing reports it"
        ));
        assert!(
            refusal.to_lowercase().contains("index"),
            "{dialect:?}: the refusal must name the index as the problem: {refusal}"
        );
    }
}

#[test]
fn two_indexes_with_distinct_names_are_still_allowed() {
    // The control. Refusing every multi-index createTable would satisfy the test
    // above while breaking ordinary migrations.
    for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
        verdict(
            r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"ix_c","columns":[{"kind":"column","name":"c"}]},{"name":"ix_d","columns":[{"kind":"column","name":"d"}]}]}"#,
            dialect,
        )
        .unwrap_or_else(|e| panic!("{dialect:?}: two distinctly named indexes must pass: {e}"));
    }
}

#[test]
fn one_index_is_still_allowed() {
    // The narrower control: a single index must not be caught by an
    // over-eager duplicate scan.
    verdict(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false}],"primaryKey":["c"],"indexes":[{"name":"ix","columns":[{"kind":"column","name":"c"}]}]}"#,
        Dialect::Postgres,
    )
    .expect("a single index must pass");
}

// ---------------------------------------------------------------------------
// The same fail-open across OPS, which the first fix did not cover.
// ---------------------------------------------------------------------------

fn verdict_envelope(ops: &str, dialect: Dialect) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, dialect, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const TABLE: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"]}"#;

#[test]
fn two_create_index_ops_sharing_a_name_are_refused() {
    // Identical fail-open to the within-table case, reached the way authors are
    // more likely to write it: two separate operations. The first fix only
    // examined one `createTable`'s own `indexes` list, so this route stayed open.
    let ops = format!(
        r#"{TABLE},{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"c"}}]}},{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"d"}}]}}"#
    );
    let refusal = verdict_envelope(&ops, Dialect::Postgres).expect_err(
        "two createIndex ops under one name lower to two CREATE INDEX IF NOT EXISTS, \
         so the second is skipped with a notice and the apply succeeds without the \
         index the author declared",
    );
    assert!(
        refusal.to_lowercase().contains("index"),
        "the refusal must name the index as the problem: {refusal}"
    );
}

#[test]
fn reusing_an_index_name_after_dropping_it_is_still_allowed() {
    // THE CONTROL THAT SHAPES THE FIX. Create, drop, create again under the same
    // name is a legitimate migration - it is how an index definition is changed -
    // and a naive "no repeated name in this envelope" rule would refuse it. The
    // check therefore tracks which names are LIVE as the ops are walked, rather
    // than counting occurrences.
    let ops = format!(
        r#"{TABLE},{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"c"}}]}},{{"op":"dropIndex","name":"ix","table":"a"}},{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"d"}}]}}"#
    );
    verdict_envelope(&ops, Dialect::Postgres)
        .expect("recreating an index after dropping it must remain allowed");
}

#[test]
fn an_index_op_colliding_with_an_inline_index_is_refused() {
    // The two routes crossing: a name declared inline on createTable and again by
    // a standalone createIndex.
    let ops = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"ix","columns":[{"kind":"column","name":"c"}]}]},{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"d"}]}"#;
    let refusal = verdict_envelope(ops, Dialect::Postgres)
        .expect_err("an inline index and a later createIndex under one name collide the same way");
    assert!(
        refusal.contains(r#"index "ix" is created twice"#),
        "the refusal must be the duplicate-index-name rule naming this index: {refusal}"
    );
}

// ---------------------------------------------------------------------------
// The DERIVED-name route: a column's `unique` produces an index name of its own.
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_index_colliding_with_a_derived_unique_name_is_refused() {
    // A column marked `unique` does not render UNIQUE inline; it renders a
    // follow-on `CREATE UNIQUE INDEX "<table>_<column>_key"`. An explicit index
    // declared under that same derived name collides with it, and both carry
    // `IF NOT EXISTS`, so the second is skipped and the apply succeeds without it.
    //
    // Measured before the fix:
    //     CREATE UNIQUE INDEX IF NOT EXISTS "a_v_key" ON "prj_ir"."a" ("v")
    //     CREATE INDEX IF NOT EXISTS "a_v_key" ON "prj_ir"."a" ("w")
    //
    // This route was invisible to the first two fixes because one of the two
    // names never appears in the IR — it is derived during lowering.
    let ops = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"v","type":"int","nullable":true,"unique":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"a_v_key","columns":[{"kind":"column","name":"w"}]}]}"#;
    let refusal = verdict_envelope(ops, Dialect::Postgres).expect_err(
        "an explicit index named exactly what the unique column derives is silently \
         skipped at apply, leaving the declared index absent",
    );
    assert!(
        refusal.to_lowercase().contains("index"),
        "the refusal must name the index as the problem: {refusal}"
    );
}

#[test]
fn a_unique_column_alongside_a_differently_named_index_is_still_allowed() {
    // The control: the derived name and the explicit name do not collide, which
    // is the ordinary case and must not be swept up.
    let ops = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"v","type":"int","nullable":true,"unique":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"a_w_idx","columns":[{"kind":"column","name":"w"}]}]}"#;
    verdict_envelope(ops, Dialect::Postgres)
        .expect("a unique column and an unrelated index name must coexist");
}

#[test]
fn two_unique_columns_are_still_allowed() {
    // Each derives its own name, so they cannot collide with each other.
    let ops = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"v","type":"int","nullable":true,"unique":true},{"name":"w","type":"int","nullable":true,"unique":true}],"primaryKey":["c"]}"#;
    verdict_envelope(ops, Dialect::Postgres).expect("two unique columns derive distinct names");
}
