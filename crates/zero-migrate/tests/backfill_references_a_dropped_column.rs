//! A backfill reading or writing a column dropped earlier in the same migration
//! is refused.
//!
//! The one expression site `expr_references_a_dropped_column.rs` recorded as
//! still unreached: "a backfill `set` value is the one expression site still
//! unreached". Closing my own note rather than leaving it to age.
//!
//! MEASURED. `dropColumn v` followed by a backfill setting `w = v` produced
//! THREE plan steps - the two DDL statements and a `PlanStep::Backfill` - and the
//! update it performs was confirmed against live PostgreSQL:
//!
//!     ALTER TABLE "public"."a" DROP COLUMN "v"
//!     UPDATE bf.a SET "w" = "v"        ERROR: column "v" does not exist
//!
//! WHY THE EXISTING RESOLVER DOES NOT CATCH IT, which is the interesting part.
//! `validate_ir_resolved` DOES resolve a backfill's column references - a
//! reference to a wholly unknown column is refused at lower with "column
//! \"ghost\" does not resolve on the enclosing target table". But it resolves
//! against the table's DECLARED column set, which still contains a column this
//! migration dropped earlier. So the pass that looks at these references cannot
//! see the drop, and the pass that tracks drops did not look at these references.
//! Two checks, each correct alone, with the case falling between them.
//!
//! COVERAGE: the `set` values and the `filter` expression go through the
//! renderer's own AST walk; the `set` KEYS and `cursorColumns` are plain
//! identifiers and are checked as such.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

fn backfill(set_col: &str, read_col: &str, cursor: &str) -> String {
    format!(
        r#"{{"op":"backfill","table":"a","name":"bf","cursorColumns":["{cursor}"],"cursorStability":{{"mode":"guardUpdates"}},"batchSize":100,"set":{{"{set_col}":{{"node":"colRef","name":"{read_col}"}}}}}}"#
    )
}

#[test]
fn a_backfill_reading_a_dropped_column_is_refused() {
    let refusal = verdict(&format!(
        r#"{A},{{"op":"dropColumn","table":"a","column":"v"}},{}"#,
        backfill("w", "v", "c0")
    ))
    .expect_err("the backfill reads a column the drop removed");
    assert!(
        refusal.to_lowercase().contains("drop"),
        "the refusal must point at the drop that removed it: {refusal}"
    );
}

/// Assert the op and the column. The three sites below are all `backfill` on the
/// same table, so the column name is the only thing separating the write case
/// from the cursor and filter cases - and the op name, added when this rule was
/// taught to identify itself, is what separates them from every other rule.
///
/// The cursor and filter cases share a message legitimately: same op, same
/// column, different position in the statement. That is as far as the text goes.
fn expect_backfill_refusal(ops: &str, column: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    let expected = format!("this backfill names column {column:?}");
    assert!(
        refusal.contains(&expected),
        "{expected:?} is missing, so another rule or another column is satisfying \
         this test: {refusal}"
    );
}

#[test]
fn a_backfill_writing_a_dropped_column_is_refused() {
    expect_backfill_refusal(
        &format!(
            r#"{A},{{"op":"dropColumn","table":"a","column":"w"}},{}"#,
            backfill("w", "v", "c0")
        ),
        "w",
        "the backfill writes a column the drop removed",
    );
}

#[test]
fn a_backfill_cursoring_on_a_dropped_column_is_refused() {
    expect_backfill_refusal(
        &format!(
            r#"{A},{{"op":"dropColumn","table":"a","column":"v"}},{}"#,
            backfill("w", "c0", "v")
        ),
        "v",
        "the cursor column was dropped",
    );
}

#[test]
fn a_backfill_filtering_on_a_dropped_column_is_refused() {
    expect_backfill_refusal(
        &format!(
            r#"{A},{{"op":"dropColumn","table":"a","column":"v"}},{{"op":"backfill","table":"a","name":"bf","cursorColumns":["c0"],"cursorStability":{{"mode":"guardUpdates"}},"batchSize":100,"set":{{"w":{{"node":"literal","value":1}}}},"filter":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}"#
        ),
        "v",
        "the filter reads a column the drop removed",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn an_ordinary_backfill_is_still_allowed() {
    verdict(&format!(r#"{A},{}"#, backfill("w", "v", "c0")))
        .expect("a backfill over live columns is the ordinary case");
}

#[test]
fn a_backfill_after_dropping_an_unrelated_column_is_still_allowed() {
    verdict(&format!(
        r#"{A},{{"op":"dropColumn","table":"a","column":"v"}},{}"#,
        backfill("w", "c0", "c0")
    ))
    .expect("dropping one column must not block a backfill over others");
}

#[test]
fn a_backfill_after_the_column_is_re_added_is_still_allowed() {
    // The restoring move, which every member of this family has needed.
    verdict(&format!(
        r#"{A},{{"op":"dropColumn","table":"a","column":"v"}},{{"op":"addColumn","table":"a","column":"v","type":"int","nullable":true}},{}"#,
        backfill("w", "v", "c0")
    ))
    .expect("drop, re-add, then backfill is a real migration pattern");
}

#[test]
fn a_backfill_on_a_different_table_is_unaffected() {
    verdict(&format!(
        r#"{A},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}},{{"name":"w","type":"int","nullable":true}}],"primaryKey":["c0"]}},{{"op":"dropColumn","table":"a","column":"v"}},{{"op":"backfill","table":"b","name":"bf","cursorColumns":["c0"],"cursorStability":{{"mode":"guardUpdates"}},"batchSize":100,"set":{{"w":{{"node":"colRef","name":"v"}}}}}}"#
    ))
    .expect("the vacated set is per table; dropping a.v says nothing about b.v");
}
